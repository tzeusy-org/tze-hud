use tze_hud_compositor::{Compositor, CompositorSurface};
use tze_hud_input::{PortalCursor, PortalRect, ResizeEdge, hit_affordance, portal_hover_cursor};
use tze_hud_scene::SceneId;
use tze_hud_scene::graph::SceneGraph;
use winit::window::{CursorIcon, WindowId};

use super::WinitApp;
use super::wake::{self, CursorEntrySignature, CursorEntryTurn};
use crate::idle_efficiency::RuntimeWakeupSource;
use crate::window::{HitRegion, WindowMode, should_capture_pointer_event};

/// Map a backend-agnostic [`PortalCursor`] onto a concrete winit cursor icon.
///
/// Kept as a total `match` (no wildcard arm) so adding a `PortalCursor` variant
/// is a compile error here until the winit mapping is supplied.
fn portal_cursor_to_winit(cursor: PortalCursor) -> CursorIcon {
    match cursor {
        PortalCursor::Default => CursorIcon::Default,
        PortalCursor::EwResize => CursorIcon::EwResize,
        PortalCursor::NsResize => CursorIcon::NsResize,
        PortalCursor::NeswResize => CursorIcon::NeswResize,
        PortalCursor::NwseResize => CursorIcon::NwseResize,
        PortalCursor::Grab => CursorIcon::Grab,
        PortalCursor::Grabbing => CursorIcon::Grabbing,
        PortalCursor::Text => CursorIcon::Text,
        PortalCursor::Pointer => CursorIcon::Pointer,
    }
}

/// Decide whether the pointer is over a portal-move drag-handle or the focused
/// portal's resize-affordance bands, for the overlay capture decision. Pure so
/// it is unit testable; the
/// [`WinitApp::cursor_over_focused_portal_affordance`] method wires the live
/// focus/snapshot/token inputs.
///
/// Two distinct cases capture:
/// - `over_drag_handle` is the caller's snapshot-wide drag-handle hit (any
///   element's move handle, not just the focused portal). It captures
///   regardless of focus or geometry, mirroring [`portal_hover_cursor`], which
///   shows the `Grab` hand over any drag-handle. Scoping this to the focused
///   portal would desync cursor-shape from capture (a `Grab` cursor over a
///   non-focused handle with no capture is the original passthrough bug) and
///   would block click-to-focus/drag of a non-focused portal.
/// - The resize bands are scoped to `focused_portal`: a `None` (or non-portal)
///   focus never captures via the resize path, so resize capture never widens
///   passthrough beyond the focused portal's own bands.
fn pointer_over_portal_affordance(
    focused_portal: Option<PortalRect>,
    x: f32,
    y: f32,
    affordance_px: f32,
    over_drag_handle: bool,
) -> bool {
    if over_drag_handle {
        return true;
    }
    matches!(
        focused_portal.map(|rect| hit_affordance(x, y, &rect, affordance_px)),
        Some(Some(_))
    )
}

pub(super) fn sync_scene_display_area(scene: &mut SceneGraph, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    scene.display_area = tze_hud_scene::Rect::new(0.0, 0.0, width as f32, height as f32);
}

fn zone_hit_regions_to_overlay_regions(scene: &SceneGraph) -> Vec<HitRegion> {
    scene
        .overlay
        .zone_hit_regions
        .iter()
        .map(|region| {
            HitRegion::new(
                region.bounds.x,
                region.bounds.y,
                region.bounds.width,
                region.bounds.height,
            )
        })
        .collect()
}

/// Collect overlay capture regions from content-layer tiles that contain at
/// least one `HitRegionNode` with `accepts_pointer = true`.
///
/// In overlay mode, the OS routes all pointer events to the desktop unless the
/// window has set `cursor_hittest(true)`. We flip to capture only when the
/// cursor is inside a known interactive region. Zone-owned affordances are
/// covered by `zone_hit_regions_to_overlay_regions`; this function handles
/// agent-owned content tiles that carry a `HitRegionNode`.
///
/// **Granularity**: we use the tile's display-space bounding box as the OS
/// capture region (not individual node bounds). The OS capture decision must be
/// made before the event is dispatched, so a coarser region is correct here.
/// Precise hit-testing against individual `HitRegionNode` bounds still happens
/// in Stage 2 (`crates/tze_hud_input/src/hit_test.rs`) after the event arrives.
///
/// Tiles with `InputMode::Passthrough` are excluded: they are transparent by
/// design and must not block underlying desktop events.
fn content_tile_hit_regions_from_scene(scene: &SceneGraph) -> Vec<HitRegion> {
    let mut regions = Vec::new();
    for tile in scene.tiles.values() {
        if tile.input_mode == tze_hud_scene::InputMode::Passthrough || !scene.tile_shown(tile) {
            continue;
        }
        if let Some(root_id) = tile.root_node {
            if tile_has_pointer_hit_region(scene, root_id) {
                regions.push(HitRegion::new(
                    tile.bounds.x,
                    tile.bounds.y,
                    tile.bounds.width,
                    tile.bounds.height,
                ));
            }
        }
    }
    regions
}

/// Returns `true` if the node subtree rooted at `node_id` contains at least
/// one `HitRegionNode` with `accepts_pointer = true`.
fn tile_has_pointer_hit_region(scene: &SceneGraph, node_id: tze_hud_scene::SceneId) -> bool {
    let Some(node) = scene.nodes.get(&node_id) else {
        return false;
    };
    if let tze_hud_scene::NodeData::HitRegion(hr) = &node.data {
        if hr.accepts_pointer {
            return true;
        }
    }
    node.children
        .iter()
        .any(|&child_id| tile_has_pointer_hit_region(scene, child_id))
}

pub(super) fn combined_overlay_hit_regions(
    static_regions: &[HitRegion],
    scene: &SceneGraph,
) -> Vec<HitRegion> {
    let mut regions = static_regions.to_vec();
    regions.extend(zone_hit_regions_to_overlay_regions(scene));
    regions.extend(content_tile_hit_regions_from_scene(scene));
    if let Some(rect) = scene
        .overlay
        .drag_handle_context_menu
        .as_ref()
        .and_then(|m| m.reset_button_rect)
    {
        regions.push(HitRegion::new(rect.x, rect.y, rect.width, rect.height));
    }
    regions
}

/// Bounds of the HUD controls a primary press would act on: card actions and
/// ×, unless the card is answered, press-guarded or safe mode is on, and each
/// portal's × and unread badge. Drag handles are not presses.
pub(super) fn pressable_control_regions(scene: &SceneGraph, safe_mode: bool) -> Vec<HitRegion> {
    use tze_hud_scene::types::ZoneInteractionKind as Kind;
    scene
        .overlay
        .zone_hit_regions
        .iter()
        .filter(|r| match r.kind {
            Kind::Dismiss | Kind::GoTo | Kind::Action { .. } => {
                !safe_mode
                    && scene.overlay.notification_press_guard != Some(r.publication_id)
                    && !scene
                        .overlay
                        .notification_choices
                        .contains_key(&r.publication_id)
            }
            Kind::JumpToLatest { .. } | Kind::DismissTile { .. } => true,
            Kind::DragHandle { .. } => false,
        })
        .map(|r| HitRegion::new(r.bounds.x, r.bounds.y, r.bounds.width, r.bounds.height))
        .collect()
}

pub(super) fn refresh_interaction_hit_regions_after_render(
    compositor: &Compositor,
    scene: &mut SceneGraph,
    surface: &dyn CompositorSurface,
) {
    let (surf_w, surf_h) = surface.size();
    compositor.populate_zone_hit_regions(scene, surf_w as f32, surf_h as f32);
}

#[cfg(any(test, target_os = "windows"))]
fn cursor_refresh_window_origin(
    outer_position: Option<winit::dpi::PhysicalPosition<i32>>,
    monitor_position: impl FnOnce() -> Option<winit::dpi::PhysicalPosition<i32>>,
) -> Option<winit::dpi::PhysicalPosition<i32>> {
    match outer_position {
        Some(position) => Some(position),
        None => monitor_position(),
    }
}

#[cfg(any(test, target_os = "windows"))]
fn screen_cursor_to_window_cursor(
    screen_cursor: winit::dpi::PhysicalPosition<i32>,
    window_origin: Option<winit::dpi::PhysicalPosition<i32>>,
) -> Option<(f32, f32)> {
    let window_origin = window_origin?;
    Some((
        (screen_cursor.x - window_origin.x) as f32,
        (screen_cursor.y - window_origin.y) as f32,
    ))
}

impl WinitApp {
    /// Refresh cursor position from OS state when passthrough is active.
    ///
    /// In overlay mode on Windows, `set_cursor_hittest(false)` prevents
    /// `CursorMoved` delivery to winit. Raw mouse input still arrives (see
    /// [`WinitApp::on_raw_mouse_motion`]); reading the global cursor position
    /// on it lets hit-testing flip back to capture when the cursor enters an
    /// active region.
    pub(super) fn refresh_cursor_position_from_os(&mut self) {
        if self.state.effective_mode == WindowMode::Overlay {
            #[cfg(target_os = "windows")]
            {
                use windows::Win32::Foundation::POINT;
                use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

                let Some(window) = &self.state.window else {
                    return;
                };
                let window_pos = cursor_refresh_window_origin(window.outer_position().ok(), || {
                    window.current_monitor().map(|monitor| monitor.position())
                });

                let mut pt = POINT { x: 0, y: 0 };
                // SAFETY: GetCursorPos writes to the provided POINT and has no
                // additional safety preconditions.
                let ok = unsafe { GetCursorPos(&mut pt).is_ok() };
                if ok {
                    if let Some((cursor_x, cursor_y)) = screen_cursor_to_window_cursor(
                        winit::dpi::PhysicalPosition::new(pt.x, pt.y),
                        window_pos,
                    ) {
                        self.apply_os_cursor_position(cursor_x, cursor_y);
                    } else {
                        tracing::trace!(
                            "overlay: skipped OS cursor refresh because window origin is unavailable"
                        );
                    }
                }
            }
        }
    }

    /// Take the cursor position read from the OS.
    ///
    /// The OS position is authoritative, so it also clears a `CursorLeft`. That
    /// event fires whenever the overlay flips from capturing back to
    /// click-through (the cursor crossed a drag handle or button), and in
    /// click-through no `CursorMoved` follows to clear it, so the hover close
    /// button stayed hidden until the next capture.
    #[cfg(any(target_os = "windows", test))]
    pub(super) fn apply_os_cursor_position(&mut self, x: f32, y: f32) {
        self.state.cursor_x = x;
        self.state.cursor_y = y;
        self.state.cursor_left_window = false;
    }

    /// True while raw mouse input must wake the loop, because passthrough could
    /// hide a click from an interactive region (see [`wake::cursor_entry_armed`]).
    pub(super) fn cursor_entry_armed(&self) -> bool {
        let snapshot = self.state.pipeline.hit_test_snapshot.load();
        let interactive = !self.state.hit_regions.is_empty()
            || !snapshot.drag_handles.is_empty()
            || snapshot.context_menu.is_some()
            || snapshot
                .tiles
                .iter()
                .any(|tile| tile.has_scroll_config || tile.dismissible);
        let panel_open = self
            .state
            .control_panel
            .as_ref()
            .is_some_and(|panel| panel.is_open());
        wake::cursor_entry_armed(
            self.state.effective_mode == WindowMode::Overlay,
            interactive || panel_open,
            !self.state.overlay_capturing,
        )
    }

    /// Route a winit device event: only raw mouse motion matters.
    pub(super) fn on_device_event(&mut self, event: &winit::event::DeviceEvent) {
        if matches!(event, winit::event::DeviceEvent::MouseMotion { .. }) {
            self.on_raw_mouse_motion();
        }
    }

    /// Recompute whether raw mouse input is needed, reseed the quiet-check
    /// signature, and return the winit listen mode to apply if it changed.
    ///
    /// Known limitation (accepted): winit emits `MouseMotion` only
    /// for relative mouse reports (`MOUSE_MOVE_RELATIVE`), so absolute-pointer
    /// devices (RDP sessions, VM tablets) never trigger entry detection; the
    /// old 30 Hz poll covered them. The target is a physical Windows desktop;
    /// no fallback poll. While armed, a mouse reporting at up to 1 kHz wakes
    /// the loop per report (cheap quiet check; zero wakes when still).
    pub(super) fn sync_cursor_entry_listening(
        &mut self,
    ) -> Option<winit::event_loop::DeviceEvents> {
        let armed = self.cursor_entry_armed();
        self.state.cursor_entry.signature = armed.then(|| self.cursor_entry_signature());
        let was = std::mem::replace(&mut self.state.cursor_entry.listening, armed);
        wake::device_events_transition(was, armed)
    }

    /// A raw mouse-motion event arrived (`RIDEV_INPUTSINK` on Windows, so it
    /// is delivered even while the overlay is click-through). Queue a cursor
    /// check for this turn when armed; a no-op otherwise.
    pub(super) fn on_raw_mouse_motion(&mut self) {
        if self.state.cursor_entry.listening {
            self.state.cursor_entry.tick = true;
        }
    }

    /// The dismissible tile whose viewer close button the cursor is over, from
    /// the lock-free snapshot; none once the cursor has left the window.
    pub(super) fn close_hover_target(&self) -> Option<SceneId> {
        (!self.state.cursor_left_window)
            .then(|| {
                self.state
                    .pipeline
                    .hit_test_snapshot
                    .load()
                    .close_hover_target(self.state.cursor_x, self.state.cursor_y)
            })
            .flatten()
    }

    /// Local move-pill target, even when the element body is click-through.
    pub(super) fn drag_handle_hover_target(&self) -> Option<SceneId> {
        (!self.state.cursor_left_window)
            .then(|| {
                self.state
                    .pipeline
                    .hit_test_snapshot
                    .load()
                    .drag_handle_hover_target(self.state.cursor_x, self.state.cursor_y)
            })
            .flatten()
    }

    /// Lock-free view of what the cursor currently hovers or would capture.
    pub(super) fn cursor_entry_signature(&self) -> CursorEntrySignature {
        let (x, y) = (self.state.cursor_x, self.state.cursor_y);
        CursorEntrySignature {
            capture: should_capture_pointer_event(
                WindowMode::Overlay,
                x,
                y,
                &self.state.hit_regions,
            ) || self.cursor_over_focused_portal_affordance()
                || self.panel_captures_cursor()
                || self
                    .state
                    .pipeline
                    .hit_test_snapshot
                    .load()
                    .context_menu
                    .is_some_and(|r| r.contains_point(x, y)),
            grip: self.resize_grip_hover_target(),
            close_target: self.close_hover_target(),
            drag_handle: self.drag_handle_hover_target(),
            widget_regions: self
                .state
                .widget_hover_trackers
                .values()
                .filter(|tracker| tracker.region.contains(x, y))
                .count() as u32,
        }
    }

    /// Service a raw mouse-motion check on the main thread alone.
    ///
    /// Costs a cursor read plus lock-free snapshot reads: no scene lock, no
    /// compositor wake, no main-work debt. Only a hover/capture change (or any
    /// other pending work) returns [`CursorEntryTurn::FullTurn`], after marking
    /// main work pending so the full turn publishes exactly one compositor
    /// wake. The capture flip itself still happens in that turn on the main
    /// thread, before anything waits on the compositor.
    pub(super) fn cursor_entry_turn(&mut self) -> CursorEntryTurn {
        self.refresh_cursor_position_from_os();
        let signature = self.cursor_entry_signature();
        let previous = self.state.cursor_entry.signature.replace(signature);
        let has_other_work = self.state.shutdown.is_triggered()
            || self.state.left_button_down
            || self.state.quiescent_efficiency.is_some()
            || !self.state.pending_keyboard_events.is_empty()
            || self.state.wake.has_unfinished_main_work();
        if has_other_work || previous != Some(signature) {
            self.state
                .wake
                .mark_main_work_pending(RuntimeWakeupSource::AnimationDeadline);
            return CursorEntryTurn::FullTurn;
        }
        // A FrameReady user event may be followed by raw mouse motion before
        // about_to_wait. Service its image even with unchanged hover, without
        // turning presentation-only debt into another scene/compositor wake.
        if self.state.deferred_present || self.state.frame_ready_rx.has_changed().unwrap_or(false) {
            return CursorEntryTurn::FullTurn;
        }
        CursorEntryTurn::Quiet {
            next: self.state.scheduled_main_deadline,
        }
    }

    /// Update overlay passthrough/capture state from current cursor+regions.
    pub(super) fn update_overlay_cursor_hittest(&mut self) {
        if self.state.effective_mode != WindowMode::Overlay {
            return;
        }
        let should_capture = should_capture_pointer_event(
            WindowMode::Overlay,
            self.state.cursor_x,
            self.state.cursor_y,
            &self.state.hit_regions,
        ) || self.state.left_button_down
            || self.cursor_over_focused_portal_affordance()
            || self.panel_captures_cursor()
            || self
                .state
                .pipeline
                .hit_test_snapshot
                .load()
                .context_menu
                .is_some_and(|r| r.contains_point(self.state.cursor_x, self.state.cursor_y));
        self.state.overlay_capturing = should_capture;
        self.apply_overlay_hittest(should_capture);
    }

    /// Resolve the focused portal tile's display-space rect from the lock-free
    /// hit-test snapshot, if a scrollable (portal) tile is focused on the active
    /// tab.
    ///
    /// Returns `None` when no tab is active, no tile is focused, or the focused
    /// tile is not a portal (so non-portal focus never shows resize cursors).
    fn focused_portal_rect(&self) -> Option<PortalRect> {
        self.focused_portal_tile_rect().map(|(_, rect)| rect)
    }

    /// Resolve both the focused portal tile's id and its display-space rect from
    /// the lock-free hit-test snapshot, if a scrollable (portal) tile is focused
    /// on the active tab.
    ///
    /// Backs [`WinitApp::focused_portal_rect`] (which drops the id) and
    /// [`WinitApp::resize_grip_hover_target`] (which needs the id to name the
    /// tile whose grip should light). Returns `None` when no tab is active, no
    /// tile is focused, or the focused tile is not a portal.
    fn focused_portal_tile_rect(&self) -> Option<(SceneId, PortalRect)> {
        let active_tab = self.state.active_tab_mirror.lock().ok().and_then(|g| *g)?;
        let focused_tile = self
            .state
            .focus_manager
            .current_owner(active_tab)
            .tile_id()?;
        let target = focused_tile.to_bytes_le();
        let snapshot = self.state.pipeline.hit_test_snapshot.load();
        let entry = snapshot
            .tiles
            .iter()
            .find(|t| t.has_scroll_config && t.tile_id_bytes == target)?;
        Some((
            focused_tile,
            PortalRect {
                x: entry.bounds.x,
                y: entry.bounds.y,
                width: entry.bounds.width,
                height: entry.bounds.height,
            },
        ))
    }

    /// The focused portal tile whose bottom-right resize corner the pointer is
    /// currently over, if any.
    ///
    /// Drives the compositor's resize-grip hover swap: the grip glyph is anchored
    /// at the portal's bottom-right corner and reads as the `╲` resize handle, so
    /// it lights only when the pointer is over that corner's resize band —
    /// [`ResizeEdge::BottomRight`]. Reuses the same focused-portal rect and
    /// affordance width as the resize cursor / affordance-capture paths, so the
    /// grip highlight, the resize cursor, and the capture region all agree on
    /// where the corner band is. Returns `None` for any other edge, a non-portal
    /// focus, or no focus.
    pub(super) fn resize_grip_hover_target(&self) -> Option<SceneId> {
        let (focused_tile, rect) = self.focused_portal_tile_rect()?;
        let affordance_px = self.resize_affordance_px();
        matches!(
            hit_affordance(
                self.state.cursor_x,
                self.state.cursor_y,
                &rect,
                affordance_px
            ),
            Some(ResizeEdge::BottomRight)
        )
        .then_some(focused_tile)
    }

    /// True when the pointer is over the focused portal's resize-affordance
    /// bands or drag-handle.
    ///
    /// In overlay passthrough mode `combined_overlay_hit_regions` only covers
    /// static + zone + content-tile regions, so a focused portal's resize bands
    /// and drag-handle are NOT capture regions. Without this, the window stays
    /// click-through over those affordances: the cursor shape set by
    /// [`WinitApp::update_portal_cursor_icon`] has no visible effect (the
    /// desktop owns the cursor) and resize/drag gestures never reach the
    /// runtime. ORing this into the overlay capture decision makes a focused
    /// portal's affordances behave like every other interactive region in
    /// overlay mode.
    ///
    /// Note: the resize bands are scoped to the focused portal, but the
    /// drag-handle term is the snapshot-wide
    /// [`HitTestSnapshot::hit_test_drag_handle`] (any element's move handle).
    /// This deliberately matches the snapshot-wide drag-handle input that
    /// [`portal_hover_cursor`] uses for the `Grab` cursor, so cursor-shape and
    /// capture stay in lockstep over every handle.
    fn cursor_over_focused_portal_affordance(&self) -> bool {
        let focused_portal = self.focused_portal_rect();
        let x = self.state.cursor_x;
        let y = self.state.cursor_y;
        let affordance_px = self.resize_affordance_px();
        let over_drag_handle = self
            .state
            .pipeline
            .hit_test_snapshot
            .load()
            .hit_test_drag_handle(x, y);
        pointer_over_portal_affordance(focused_portal, x, y, affordance_px, over_drag_handle)
    }

    /// A primary press at `(x, y)` would act on a HUD control: the open panel
    /// answers for its own area, the last frame's controls elsewhere.
    fn pointer_over_pressable(&self, x: f32, y: f32) -> bool {
        self.panel_pressable_at_cursor().unwrap_or_else(|| {
            self.state
                .pressable_regions
                .iter()
                .any(|r| r.contains(x, y))
        })
    }

    /// Update the OS cursor shape to reflect the portal resize/move affordance
    /// under the pointer.
    ///
    /// Called on pointer events. Computes the desired [`PortalCursor`] from the
    /// focused portal's affordance hit-test, any active resize/drag gesture, and
    /// the drag-handle hover state, then issues `Window::set_cursor` only when
    /// the shape actually changes (via [`CursorIconTracker`]). Restores the
    /// arrow when the pointer leaves every affordance.
    ///
    /// [`CursorIconTracker`]: tze_hud_input::CursorIconTracker
    pub(super) fn update_portal_cursor_icon(&mut self) {
        let x = self.state.cursor_x;
        let y = self.state.cursor_y;

        let focused_portal = self.focused_portal_rect();

        let snapshot = self.state.pipeline.hit_test_snapshot.load();
        let over_drag_handle = snapshot.hit_test_drag_handle(x, y);

        // An active resize gesture pins its edge cursor; an active drag-to-move
        // shows the closed hand. These outrank hover hit-testing so the cursor
        // does not flicker when the pointer leaves the affordance mid-gesture.
        let active_edge = self
            .state
            .portal_resize_states
            .values()
            .find_map(|s| s.active_edge());
        let drag_active = self
            .state
            .input_processor
            .drag_states
            .values()
            .any(|s| s.phase == tze_hud_input::DragPhase::Activated);

        let affordance_px = self.resize_affordance_px();

        let mut desired = portal_hover_cursor(
            x,
            y,
            focused_portal,
            affordance_px,
            over_drag_handle,
            active_edge,
            drag_active,
        );
        // The pip resize grip shows its diagonal cursor instead of the move
        // hand, from hover through the whole resize (its corner is frozen on
        // the drag state, so a press without a prior hover still shows it).
        let resizing = self
            .state
            .input_processor
            .drag_states
            .values()
            .find(|s| s.is_active_resize())
            .and_then(|s| s.resize_grip);
        if let Some(corner) = resizing {
            desired = PortalCursor::for_grip_corner(corner);
        } else if matches!(desired, PortalCursor::Grab | PortalCursor::Default) {
            // Below resizing: the grip, then a pressable control, which beats
            // the drag handle (the portal × sits in the header band).
            if let Some(cursor) = snapshot.resize_grip_cursor(x, y) {
                desired = cursor;
            } else if self.pointer_over_pressable(x, y) {
                desired = PortalCursor::Pointer;
            }
        }

        // Text, links and copy buttons: I-beam / hand (hud-ljhku.2).
        if desired == PortalCursor::Default
            && let Some(reading) = self.state.reading_cursor
        {
            desired = reading;
        }
        let shown = self.state.cursor_tracker.current();
        let changed = self.state.cursor_tracker.update(desired).is_some();
        let target = self
            .state
            .cursor_window
            .or_else(|| self.state.window.as_ref().map(|w| w.id()));
        for (id, cursor) in cursor_shape_calls(
            shown,
            desired,
            changed,
            self.state.cursor_shape_window,
            target,
        ) {
            if let Some(window) = self.hud_window(id) {
                window.set_cursor(portal_cursor_to_winit(cursor));
            }
        }
        self.state.cursor_shape_window = target;
    }

    /// The HUD window `id`: the primary or a secondary display's overlay.
    fn hud_window(&self, id: WindowId) -> Option<&winit::window::Window> {
        let primary = self.state.window.as_deref().filter(|w| w.id() == id);
        primary.or_else(|| {
            self.state
                .secondaries
                .iter()
                .find(|s| s.window.id() == id)
                .map(|s| &*s.window)
        })
    }

    /// `portal.window.resize_affordance_px`, resolved once per token change.
    fn resize_affordance_px(&self) -> f32 {
        if let Some(px) = self.state.resize_affordance_px.get() {
            return px;
        }
        let px = tze_hud_config::resolve_portal_tokens(&self.state.global_tokens)
            .window_resize_affordance_px;
        self.state.resize_affordance_px.set(Some(px));
        px
    }
}

/// The OS cursor calls that show `desired` on the pointer's window `target`.
/// Only that window shows a shape: the window that showed one before
/// returns to the arrow when the pointer leaves it, so a hand cannot linger
/// on another display. An unchanged shape on the same window needs no call.
fn cursor_shape_calls(
    shown: PortalCursor,
    desired: PortalCursor,
    changed: bool,
    shown_on: Option<WindowId>,
    target: Option<WindowId>,
) -> Vec<(WindowId, PortalCursor)> {
    let mut calls = Vec::new();
    if shown_on == target {
        if let Some(id) = target.filter(|_| changed) {
            calls.push((id, desired));
        }
        return calls;
    }
    if let Some(old) = shown_on.filter(|_| shown != PortalCursor::Default) {
        calls.push((old, PortalCursor::Default));
    }
    if let Some(id) = target.filter(|_| desired != PortalCursor::Default) {
        calls.push((id, desired));
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{scene_with_capture_tile, scene_with_drag_handle_tile};
    use super::*;
    use std::sync::Arc;
    use tze_hud_compositor::HeadlessSurface;
    use tze_hud_scene::Rect;

    #[test]
    fn combined_overlay_hit_regions_includes_zone_interaction_regions() {
        let static_regions = vec![HitRegion::new(10.0, 20.0, 30.0, 40.0)];
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        scene
            .overlay
            .zone_hit_regions
            .push(tze_hud_scene::types::ZoneHitRegion {
                publication_id: tze_hud_scene::SceneId::null(),
                zone_name: "notification-area".to_string(),
                published_at_wall_us: 123,
                publisher_namespace: "test-agent".to_string(),
                bounds: tze_hud_scene::types::Rect::new(100.0, 200.0, 20.0, 20.0),
                kind: tze_hud_scene::types::ZoneInteractionKind::Dismiss,
                interaction_id: "zone:notification-area:dismiss:123:test-agent".to_string(),
                tab_order: 0,
            });

        let combined = combined_overlay_hit_regions(&static_regions, &scene);

        assert_eq!(combined.len(), 2, "static + zone capture regions expected");
        assert_eq!(
            combined[0], static_regions[0],
            "static region must be preserved"
        );
        assert_eq!(
            combined[1],
            HitRegion::new(100.0, 200.0, 20.0, 20.0),
            "zone hit region must be exposed to overlay click-capture"
        );

        let mut menu: tze_hud_scene::DragHandleContextMenuState = serde_json::from_value(serde_json::json!({
            "element_id": tze_hud_scene::SceneId::new(), "anchor_x": 400.0, "anchor_y": 300.0, "shown_at_ns": 1
        })).unwrap();
        menu.reset_button_rect = Some(tze_hud_scene::Rect::new(404.0, 304.0, 152.0, 24.0));
        scene.overlay.drag_handle_context_menu = Some(menu);
        let combined = combined_overlay_hit_regions(&static_regions, &scene);
        assert!(
            combined.iter().any(|r| r.contains(480.0, 316.0)),
            "a painted menu action must capture independently of underlying content"
        );
        assert!(
            !combined.iter().any(|r| r.contains(600.0, 316.0)),
            "a menu must not turn adjacent desktop into a capture region"
        );
        scene.overlay.drag_handle_context_menu = None;
        assert_eq!(
            combined_overlay_hit_regions(&static_regions, &scene).len(),
            2
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windowed_post_render_refresh_updates_drag_handles_before_snapshot() {
        let _runtime_guard = crate::test_support::lock_headless_runtime().await;
        let mut compositor = match Compositor::new_headless(1920, 1080).await {
            Ok(compositor) => compositor,
            Err(e) => {
                eprintln!("skipping GPU-dependent drag-handle refresh test: {e}");
                return;
            }
        };
        compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
        let surface = HeadlessSurface::new(&compositor.device, 1920, 1080);
        let (mut scene, tile_id, _element_id, _interaction_id) =
            scene_with_drag_handle_tile(400.0, 300.0, 600.0, 200.0);

        let tile = scene
            .tiles
            .get_mut(&tile_id)
            .expect("drag-handle test tile must exist");
        tile.bounds.x = 500.0;
        tile.bounds.y = 330.0;

        let stale_snapshot = crate::pipeline::HitTestSnapshot::from_scene(&scene);

        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        compositor.render_frame(&mut scene, &surface);
        refresh_interaction_hit_regions_after_render(&compositor, &mut scene, &surface);
        let refreshed_snapshot = crate::pipeline::HitTestSnapshot::from_scene(&scene);
        let grip = scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .find(|handle| handle.element_id == tile_id)
            .unwrap()
            .bounds;
        let grip_center = (grip.x + grip.width / 2.0, grip.y + grip.height / 2.0);
        assert!(grip.y + grip.height < scene.tiles[&tile_id].bounds.y);
        assert!(
            !stale_snapshot.hit_test_drag_handle(grip_center.0, grip_center.1),
            "pre-refresh snapshot still carries the previous frame's drag-handle bounds"
        );
        assert!(
            refreshed_snapshot.hit_test_drag_handle(grip_center.0, grip_center.1),
            "snapshot built after the windowed post-render refresh must match the newly displayed drag-handle geometry"
        );

        // Body hover follows the front tile, independently of grip capture.
        let rear = scene.tiles[&tile_id].clone();
        let front_lease = scene.grant_lease("front-agent", 60_000);
        let front_bounds = tze_hud_scene::Rect::new(
            rear.bounds.x + 100.0,
            rear.bounds.y + 40.0,
            rear.bounds.width - 100.0,
            rear.bounds.height - 40.0,
        );
        let front_id = scene
            .create_tile(
                rear.tab_id,
                "front-agent",
                front_lease,
                front_bounds,
                rear.z_order + 1,
            )
            .unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        compositor.render_frame(&mut scene, &surface);
        refresh_interaction_hit_regions_after_render(&compositor, &mut scene, &surface);
        let overlap = crate::pipeline::HitTestSnapshot::from_scene(&scene);
        let (x, y) = (front_bounds.x + 20.0, front_bounds.y + 80.0);
        assert_eq!(
            overlap.hit_test(x, y).unwrap().tile_id_bytes,
            front_id.to_bytes_le()
        );
        assert!(!overlap.hit_test_drag_handle(x, y));
        assert_eq!(
            overlap.drag_handle_hover_target(x, y),
            Some(front_id),
            "overlapping body hover must reveal the front tile's pill"
        );
        assert_eq!(
            overlap.drag_handle_hover_target(rear.bounds.x + 20.0, y),
            Some(tile_id),
            "the uncovered rear body still reveals its own pill"
        );

        // Use the actual renderer's Zone handle, not a tile-shaped synthetic hit.
        use super::super::test_support::{make_test_zone, make_windowed_keyboard_test_app};
        use tze_hud_input::{FocusManager, InputProcessor, PointerEventKind};
        use tze_hud_scene::{GeometryPolicy, NotificationAction, NotificationPayload, ZoneContent};
        let mut zone = make_test_zone("notifications");
        zone.geometry_policy = GeometryPolicy::Relative {
            x_pct: 0.6,
            y_pct: 0.1,
            width_pct: 0.35,
            height_pct: 0.4,
        };
        let zone_id = zone.id;
        scene.register_zone(zone);
        scene
            .publish_to_zone(
                "notifications",
                ZoneContent::Notification(NotificationPayload {
                    progress: None,
                    text: "Move the stack".into(),
                    title: "Placement".into(),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    actions: vec![NotificationAction {
                        copy: None,
                        label: "Open".into(),
                        callback_id: Some("open".into()),
                        url: None,
                        role: None,
                    }],
                }),
                "zone-owner",
                None,
                None,
                None,
            )
            .unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        compositor.render_frame(&mut scene, &surface);
        refresh_interaction_hit_regions_after_render(&compositor, &mut scene, &surface);
        let handle = scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .find(|h| h.element_id == zone_id)
            .unwrap()
            .clone();
        assert_eq!(
            handle.element_kind,
            tze_hud_scene::DragHandleElementKind::Zone
        );
        assert!(!scene.tiles.contains_key(&zone_id));
        let snapshot = crate::pipeline::HitTestSnapshot::from_scene(&scene);
        let body_x = handle.element_bounds.unwrap().x + 40.0;
        let body_y = handle.element_bounds.unwrap().y + 40.0;
        assert_eq!(
            snapshot.drag_handle_hover_target(body_x, body_y),
            Some(zone_id)
        );
        let card = scene
            .overlay
            .notification_geometry
            .iter()
            .find(|card| card.zone_name == "notifications")
            .unwrap()
            .bounds;
        let gap_x = handle.bounds.x + handle.bounds.width / 2.0;
        let gap_y = (handle.bounds.y + handle.bounds.height + card.y) / 2.0;
        assert_eq!(
            snapshot.drag_handle_hover_target(gap_x, gap_y),
            Some(zone_id),
            "moving across the gap from card to grip keeps the pill visible"
        );
        assert!(
            !snapshot.hit_test_drag_handle(gap_x, gap_y),
            "the hover corridor adds no pointer capture"
        );
        assert_eq!(
            snapshot
                .drag_handle_hover_target(card.x + card.width / 2.0, card.y + card.height + 20.0),
            None,
            "empty logical zone below the painted stack has no hover pill"
        );
        assert!(
            !snapshot.hit_test_drag_handle(body_x, body_y),
            "body hover adds no capture"
        );
        let controls = scene.overlay.zone_hit_regions.clone();
        assert_eq!(controls.len(), 2, "dismiss and action were generated");
        let tile_bounds = scene.tiles[&tile_id].bounds;
        let (mut app, mut rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());
        app.state.cursor_x = handle.bounds.x + handle.bounds.width / 2.0;
        app.state.cursor_y = handle.bounds.y + handle.bounds.height / 2.0;
        let (x, y) = (app.state.cursor_x, app.state.cursor_y);
        app.enqueue_pointer_event(PointerEventKind::Down);
        assert!(
            app.state.input_processor.drag_states.contains_key(&0),
            "the emitted Zone grip must admit a real runtime drag despite having no tile"
        );
        app.state.cursor_x += 1.0;
        app.enqueue_pointer_event(PointerEventKind::Move);
        app.state.cursor_x = x - 76.8;
        app.state.cursor_y = y + 43.2;
        app.enqueue_pointer_event(PointerEventKind::Move);
        app.enqueue_pointer_event(PointerEventKind::Up);
        assert!(app.state.input_processor.drag_states.is_empty());
        assert!(app.state.primary_drag_gesture.is_none());
        let shared = app.state.shared_state.try_lock().unwrap();
        assert!(
            shared
                .element_store
                .entries
                .values()
                .all(|e| e.geometry_override.is_none())
        );
        let mut scene = shared.scene.try_lock().unwrap();
        assert_eq!(scene.tiles[&tile_id].bounds, tile_bounds);
        assert!(scene.overlay.drag_active_elements.is_empty());
        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        compositor.render_frame(&mut scene, &surface);
        refresh_interaction_hit_regions_after_render(&compositor, &mut scene, &surface);
        let moved = scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .find(|h| h.element_id == zone_id)
            .unwrap();
        let delta = (
            moved.bounds.x - handle.bounds.x,
            moved.bounds.y - handle.bounds.y,
        );
        assert!((delta.0 + 76.8).abs() < 0.1 && (delta.1 - 43.2).abs() < 0.1);
        assert_eq!(scene.overlay.zone_hit_regions.len(), controls.len());
        for before in controls {
            let after = scene
                .overlay
                .zone_hit_regions
                .iter()
                .find(|h| h.interaction_id == before.interaction_id)
                .unwrap();
            assert!((after.bounds.x - before.bounds.x - delta.0).abs() < 0.1);
            assert!((after.bounds.y - before.bounds.y - delta.1).abs() < 0.1);
            let snapshot = crate::pipeline::HitTestSnapshot::from_scene(&scene);
            assert!(snapshot.hit_test_drag_handle(
                moved.bounds.x + moved.bounds.width / 2.0,
                moved.bounds.y + moved.bounds.height / 2.0
            ));
        }

        let moved = moved.clone();
        drop(scene);
        drop(shared);
        app.state.cursor_x = moved.bounds.x + moved.bounds.width / 2.0;
        app.state.cursor_y = moved.bounds.y + moved.bounds.height / 2.0;
        let window = winit::window::WindowId::dummy();
        let native = super::super::lifecycle::ChromeCaptureTestScope::new(window);
        app.handle_mouse_button(winit::event::MouseButton::Right, false, window, None);
        {
            let shared = app.state.shared_state.try_lock().unwrap();
            let mut scene = shared.scene.try_lock().unwrap();
            compositor.render_frame(&mut scene, &surface);
            refresh_interaction_hit_regions_after_render(&compositor, &mut scene, &surface);
            let menu = scene.overlay.drag_handle_context_menu.as_ref().unwrap();
            let button = menu.reset_button_rect.unwrap();
            let capture = combined_overlay_hit_regions(&[], &scene);
            assert!(
                capture.iter().any(|r| r.contains(
                    button.x + button.width / 2.0,
                    button.y + button.height / 2.0
                )),
                "the actual opened and rendered menu must admit its reset click"
            );
            app.state.cursor_x = button.x + button.width / 2.0;
            app.state.cursor_y = button.y + button.height / 2.0;
        }
        let receipt = native.begin(true);
        app.handle_mouse_button(winit::event::MouseButton::Left, true, window, None);
        native.complete(receipt);
        assert!(
            app.state.input_processor.drag_states.is_empty(),
            "menu Down never starts an underlying drag"
        );
        let receipt = native.begin(false);
        app.handle_mouse_button(winit::event::MouseButton::Left, false, window, None);
        native.complete(receipt);
        assert!(
            native.healthy(),
            "matching Reset edges have native receipts"
        );
        app.handle_mouse_button(winit::event::MouseButton::Left, false, window, None);
        drop(native);
        let shared = app.state.shared_state.try_lock().unwrap();
        let mut scene = shared.scene.try_lock().unwrap();
        assert!(scene.overlay.drag_handle_context_menu.is_none());
        assert!(
            shared
                .element_store
                .entries
                .values()
                .all(|e| e.geometry_override.is_none())
        );
        compositor.render_frame(&mut scene, &surface);
        let reset = scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .find(|h| h.element_id == zone_id)
            .unwrap();
        assert_eq!(
            reset.bounds, handle.bounds,
            "matching menu Down/Up restores default zone placement"
        );
        assert_eq!(scene.tiles[&tile_id].bounds, tile_bounds);
        drop(scene);
        drop(shared);
        while rx.try_recv().is_ok() {}
        // Exercise delivered edges at the same real menu seam. Teardown rows
        // represent the existing timeout/clear lifecycle, not a replacement timer.
        for case in [
            "outside",
            "release-outside",
            "cancel-reentry",
            "focus",
            "replaced",
            "expired",
            "hidden",
            "removed",
            "wrong-window",
            "old-tile-press",
            "native-missing-up",
            "native-mismatched-up",
            "native-cancel",
            "native-allowed",
            "busy",
            "overlap",
            "no-op",
            "release-only",
        ] {
            // Every delivered edge traverses the native ledger on Windows;
            // ordinary positive rows need the same receipt setup as controls.
            let native = super::super::lifecycle::ChromeCaptureTestScope::new(window);
            let old_grip;
            let definition;
            {
                let shared = app.state.shared_state.try_lock().unwrap();
                let mut scene = shared.scene.try_lock().unwrap();
                definition = scene.zone_by_id(zone_id).unwrap().clone();
                if case != "no-op" {
                    scene.set_zone_placement(
                        zone_id,
                        GeometryPolicy::Relative {
                            x_pct: 0.4,
                            y_pct: 0.3,
                            width_pct: 0.35,
                            height_pct: 0.4,
                        },
                    );
                }
                compositor.prime_markdown_cache(&scene);
                compositor.prime_truncation_cache(&scene);
                compositor.render_frame(&mut scene, &surface);
                refresh_interaction_hit_regions_after_render(&compositor, &mut scene, &surface);
                old_grip = scene
                    .overlay
                    .drag_handle_hit_regions
                    .iter()
                    .find(|h| h.element_id == tile_id)
                    .unwrap()
                    .bounds;
                let grip = scene
                    .overlay
                    .drag_handle_hit_regions
                    .iter()
                    .find(|h| h.element_id == zone_id)
                    .unwrap();
                app.state.cursor_x = grip.bounds.x + grip.bounds.width / 2.0;
                app.state.cursor_y = grip.bounds.y + grip.bounds.height / 2.0;
            }
            if case == "old-tile-press" {
                let zone_cursor = (app.state.cursor_x, app.state.cursor_y);
                app.state.cursor_x = old_grip.x + old_grip.width / 2.0;
                app.state.cursor_y = old_grip.y + old_grip.height / 2.0;
                let receipt = native.begin(true);
                app.handle_mouse_button(winit::event::MouseButton::Left, true, window, None);
                native.complete(receipt);
                assert_eq!(
                    app.state.input_processor.drag_states[&0].element_id, tile_id,
                    "the old press really belongs to the unrelated rendered Tile grip"
                );
                // Deliberately omit its delivered Up. No about-to-wait or
                // cursor-motion fallback intervenes before the fresh menu Down.
                (app.state.cursor_x, app.state.cursor_y) = zone_cursor;
            }
            app.handle_mouse_button(winit::event::MouseButton::Right, false, window, None);
            let action;
            let before;
            {
                let shared = app.state.shared_state.try_lock().unwrap();
                let mut scene = shared.scene.try_lock().unwrap();
                before = scene.effective_zone_geometry(scene.zone_by_id(zone_id).unwrap());
                if case == "overlap" {
                    // Put the actual menu request over an actual rendered action:
                    // a leaked Down/Up would dispatch its real callback.
                    let action = scene
                        .overlay
                        .zone_hit_regions
                        .iter()
                        .find(|r| {
                            matches!(r.kind, tze_hud_scene::ZoneInteractionKind::Action { .. })
                        })
                        .unwrap()
                        .bounds;
                    let menu = scene.overlay.drag_handle_context_menu.as_mut().unwrap();
                    menu.anchor_x = action.x;
                    menu.anchor_y = action.y;
                }
                compositor.render_frame(&mut scene, &surface);
                refresh_interaction_hit_regions_after_render(&compositor, &mut scene, &surface);
                action = scene
                    .overlay
                    .drag_handle_context_menu
                    .as_ref()
                    .unwrap()
                    .reset_button_rect
                    .unwrap();
            }
            app.state.cursor_x = action.x + 8.0;
            app.state.cursor_y = action.y + 8.0;
            if case == "outside" {
                app.state.cursor_y = action.y + action.height + 8.0;
            }
            let shared_state = std::sync::Arc::clone(&app.state.shared_state);
            let availability = app.state.wake.main_work_generation();
            let held = (case == "busy").then(|| shared_state.try_lock().unwrap());
            if case != "release-only" {
                let receipt = native.begin(true);
                app.handle_mouse_button(winit::event::MouseButton::Left, true, window, None);
                native.complete(receipt);
                app.enqueue_pointer_event(PointerEventKind::Move);
            }
            if case == "native-cancel" {
                native.lose();
                app.drain_chrome_capture_cancellation();
            }
            if ["focus", "cancel-reentry"].contains(&case) {
                app.handle_window_focus(None, false);
                if case == "cancel-reentry" {
                    // No settle/timer turn intervenes: the cancelled painted
                    // request must not be re-armed by a newly delivered press.
                    let receipt = native.begin(true);
                    app.handle_mouse_button(winit::event::MouseButton::Left, true, window, None);
                    native.complete(receipt);
                }
            }
            if case == "replaced" {
                let shared = app.state.shared_state.try_lock().unwrap();
                let mut scene = shared.scene.try_lock().unwrap();
                scene
                    .overlay
                    .drag_handle_context_menu
                    .as_mut()
                    .unwrap()
                    .shown_at_ns += 1;
            }
            if ["expired", "hidden", "removed"].contains(&case) {
                let shared = app.state.shared_state.try_lock().unwrap();
                let mut scene = shared.scene.try_lock().unwrap();
                match case {
                    "expired" => scene.overlay.drag_handle_context_menu = None,
                    "hidden" => {
                        scene.set_owner_hidden(tze_hud_scene::graph::SurfaceClasses {
                            zones: true,
                            ..Default::default()
                        });
                    }
                    "removed" => {
                        scene.unregister_zone("notifications");
                    }
                    _ => unreachable!(),
                }
            }
            if case == "release-outside" {
                app.state.cursor_y = action.y + action.height + 8.0;
            }
            let release_window = if case == "wrong-window" {
                winit::window::WindowId::from(77)
            } else {
                window
            };
            let receipt = if case == "native-missing-up" {
                None
            } else {
                native.begin(case == "native-mismatched-up")
            };
            app.handle_mouse_button(winit::event::MouseButton::Left, false, release_window, None);
            native.complete(receipt);
            drop(native);
            drop(held);
            if case == "busy" {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                while app.state.wake.main_work_generation() == availability
                    && std::time::Instant::now() < deadline
                {
                    tokio::task::yield_now().await;
                }
                assert!(
                    app.state.wake.main_work_generation() > availability,
                    "lock release delivers the existing completion wake before settlement"
                );
            }
            app.tick_context_menu_auto_dismiss();
            assert!(
                !app.state.left_button_down && app.state.press_window.is_none(),
                "{case}: capture ends"
            );
            assert!(
                app.state.input_processor.drag_states.is_empty(),
                "{case}: no underlying drag"
            );
            assert!(
                rx.try_recv().is_err(),
                "{case}: menu edges cannot dispatch an agent callback"
            );
            {
                let shared = app.state.shared_state.try_lock().unwrap();
                let mut scene = shared.scene.try_lock().unwrap();
                if case == "removed" {
                    scene.register_zone(definition.clone());
                    scene.set_zone_placement(zone_id, before);
                }
                if case == "hidden" {
                    scene.set_owner_hidden(Default::default());
                }
                let expected = if ["overlap", "no-op", "native-allowed"].contains(&case) {
                    definition.geometry_policy
                } else {
                    before
                };
                assert_eq!(
                    scene.effective_zone_geometry(scene.zone_by_id(zone_id).unwrap()),
                    expected,
                    "{case}: only a matching actionable press resets geometry"
                );
                if case != "release-only" {
                    assert!(
                        scene.overlay.drag_handle_context_menu.is_none(),
                        "{case}: menu closes"
                    );
                }
                assert!(
                    shared
                        .element_store
                        .entries
                        .values()
                        .all(|e| e.geometry_override.is_none())
                );
                scene.overlay.drag_handle_context_menu = None;
                scene.reset_zone_placement(zone_id);
                if case == "removed" {
                    scene
                        .publish_to_zone(
                            "notifications",
                            ZoneContent::Notification(NotificationPayload {
                                progress: None,
                                text: "Move the stack".into(),
                                title: "Placement".into(),
                                icon: String::new(),
                                urgency: 1,
                                ttl_ms: None,
                                actions: vec![NotificationAction {
                                    copy: None,
                                    label: "Open".into(),
                                    callback_id: Some("open".into()),
                                    url: None,
                                    role: None,
                                }],
                            }),
                            "zone-owner",
                            None,
                            None,
                            None,
                        )
                        .unwrap();
                }
            }
            app.tick_context_menu_auto_dismiss();
        }
    }

    #[test]
    fn screen_cursor_without_window_origin_does_not_guess_local_coordinates() {
        let cursor = winit::dpi::PhysicalPosition::new(2600, 120);

        let local = screen_cursor_to_window_cursor(cursor, None);

        assert_eq!(
            local, None,
            "screen-relative cursor coordinates must not be treated as window-local when the \
             overlay window origin is unavailable"
        );
    }

    #[test]
    fn screen_cursor_with_nonzero_window_origin_becomes_window_relative() {
        let cursor = winit::dpi::PhysicalPosition::new(2600, 120);
        let window_origin = Some(winit::dpi::PhysicalPosition::new(2560, 40));

        let local = screen_cursor_to_window_cursor(cursor, window_origin);

        assert_eq!(
            local,
            Some((40.0, 80.0)),
            "cursor refresh must convert desktop coordinates into overlay-window coordinates"
        );
    }

    #[test]
    fn cursor_refresh_origin_falls_back_to_monitor_position() {
        let monitor_origin = winit::dpi::PhysicalPosition::new(2560, 0);

        let origin = cursor_refresh_window_origin(None, || Some(monitor_origin));

        assert_eq!(
            origin,
            Some(monitor_origin),
            "Windows overlay cursor refresh should use the monitor origin if the window origin \
             query fails"
        );
    }

    #[test]
    fn cursor_refresh_origin_does_not_query_monitor_when_window_position_exists() {
        let window_origin = winit::dpi::PhysicalPosition::new(64, 32);
        let mut monitor_lookup_count = 0;

        let origin = cursor_refresh_window_origin(Some(window_origin), || {
            monitor_lookup_count += 1;
            Some(winit::dpi::PhysicalPosition::new(2560, 0))
        });

        assert_eq!(origin, Some(window_origin));
        assert_eq!(
            monitor_lookup_count, 0,
            "monitor origin fallback must stay lazy on the hot cursor refresh path"
        );
    }

    #[test]
    fn content_tile_with_hit_region_node_registers_capture_region() {
        let (scene, tile_id) = scene_with_capture_tile();
        let tile_bounds = scene.tiles[&tile_id].bounds;

        let regions = content_tile_hit_regions_from_scene(&scene);

        assert_eq!(
            regions.len(),
            1,
            "exactly one capture region for one capture-mode tile with HitRegionNode"
        );
        assert_eq!(
            regions[0],
            HitRegion::new(
                tile_bounds.x,
                tile_bounds.y,
                tile_bounds.width,
                tile_bounds.height
            ),
            "capture region must match the tile's display-space bounds"
        );
    }

    #[test]
    fn passthrough_tile_with_hit_region_does_not_register_capture_region() {
        use tze_hud_scene::{HitRegionNode, InputMode, Node, NodeData, Rect, SceneId};
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("overlay", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "overlay",
                lease_id,
                Rect::new(0.0, 0.0, 300.0, 300.0),
                1,
            )
            .unwrap();
        scene.tiles.get_mut(&tile_id).unwrap().input_mode = InputMode::Passthrough;
        let node_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: node_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 100.0, 50.0),
                        interaction_id: "passthrough-btn".to_string(),
                        accepts_pointer: true,
                        accepts_focus: false,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        let regions = content_tile_hit_regions_from_scene(&scene);
        assert!(
            regions.is_empty(),
            "passthrough tile must not register a capture region"
        );
    }

    /// A tile the owner hid from the control panel lets the desktop have the
    /// pointer: the overlay stops capturing over it until it is shown again.
    #[test]
    fn hidden_tile_does_not_register_capture_region() {
        use tze_hud_scene::graph::SurfaceClasses;
        use tze_hud_scene::{HitRegionNode, Node, NodeData, Rect, SceneId};
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 200.0, 100.0),
                1,
            )
            .unwrap();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                        interaction_id: "btn".to_string(),
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();
        assert_eq!(content_tile_hit_regions_from_scene(&scene).len(), 1);

        scene.set_owner_hidden(SurfaceClasses {
            tiles: true,
            ..SurfaceClasses::NONE
        });
        assert!(content_tile_hit_regions_from_scene(&scene).is_empty());

        scene.set_owner_hidden(SurfaceClasses::NONE);
        assert_eq!(content_tile_hit_regions_from_scene(&scene).len(), 1);
    }

    #[test]
    fn tile_with_accepts_pointer_false_does_not_register_capture_region() {
        use tze_hud_scene::{HitRegionNode, Node, NodeData, Rect, SceneId};
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 200.0, 100.0),
                1,
            )
            .unwrap();
        let node_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: node_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                        interaction_id: "focus-only".to_string(),
                        accepts_pointer: false,
                        accepts_focus: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        let regions = content_tile_hit_regions_from_scene(&scene);
        assert!(
            regions.is_empty(),
            "tile with accepts_pointer=false node must not register a capture region"
        );
    }

    #[test]
    fn combined_overlay_hit_regions_includes_content_tile_regions() {
        let (scene, tile_id) = scene_with_capture_tile();
        let tile_bounds = scene.tiles[&tile_id].bounds;

        let static_regions = vec![HitRegion::new(10.0, 10.0, 50.0, 50.0)];
        let combined = combined_overlay_hit_regions(&static_regions, &scene);

        assert_eq!(
            combined.len(),
            2,
            "static + content-tile capture regions expected"
        );
        assert_eq!(combined[0], static_regions[0], "static region preserved");
        assert_eq!(
            combined[1],
            HitRegion::new(
                tile_bounds.x,
                tile_bounds.y,
                tile_bounds.width,
                tile_bounds.height
            ),
            "content-tile capture region must cover tile display-space bounds"
        );
    }

    #[test]
    fn fullscreen_captures_pointer_outside_any_hit_region() {
        let capture = should_capture_pointer_event(WindowMode::Fullscreen, 9000.0, 9000.0, &[]);
        assert!(capture, "fullscreen must capture all pointer events");
    }

    #[test]
    fn fullscreen_captures_pointer_even_with_regions_present() {
        let regions = vec![HitRegion::new(0.0, 0.0, 100.0, 100.0)];
        let capture =
            should_capture_pointer_event(WindowMode::Fullscreen, 9000.0, 9000.0, &regions);
        assert!(capture);
    }

    #[test]
    fn overlay_no_hit_regions_passes_through_all_events() {
        let capture = should_capture_pointer_event(WindowMode::Overlay, 500.0, 500.0, &[]);
        assert!(
            !capture,
            "overlay with no hit-regions must pass all events through"
        );
    }

    #[test]
    fn overlay_cursor_inside_hit_region_is_captured() {
        let regions = vec![HitRegion::new(100.0, 100.0, 200.0, 150.0)];
        let capture = should_capture_pointer_event(WindowMode::Overlay, 150.0, 150.0, &regions);
        assert!(capture, "cursor inside hit-region must be captured");
    }

    #[test]
    fn overlay_cursor_outside_all_hit_regions_passes_through() {
        let regions = vec![HitRegion::new(100.0, 100.0, 200.0, 150.0)];
        let capture = should_capture_pointer_event(WindowMode::Overlay, 50.0, 50.0, &regions);
        assert!(!capture, "cursor outside all hit-regions must pass through");
    }

    #[test]
    fn overlay_multiple_hit_regions_union_semantics() {
        let regions = vec![
            HitRegion::new(0.0, 0.0, 100.0, 100.0),
            HitRegion::new(500.0, 500.0, 100.0, 100.0),
        ];
        assert!(should_capture_pointer_event(
            WindowMode::Overlay,
            50.0,
            50.0,
            &regions
        ));
        assert!(should_capture_pointer_event(
            WindowMode::Overlay,
            550.0,
            550.0,
            &regions
        ));
        assert!(!should_capture_pointer_event(
            WindowMode::Overlay,
            300.0,
            300.0,
            &regions
        ));
    }

    #[test]
    fn overlay_capture_includes_focused_portal_affordances() {
        // in overlay passthrough mode the focused portal's resize
        // bands + drag-handle must still capture, or the cursor shape never
        // shows and gestures never reach.
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 150.0,
        };
        let aff = 10.0;

        // Over the right resize band (mid-height) -> captured.
        assert!(pointer_over_portal_affordance(
            Some(rect),
            299.0,
            175.0,
            aff,
            false
        ));
        // Over the top-left corner -> captured.
        assert!(pointer_over_portal_affordance(
            Some(rect),
            101.0,
            101.0,
            aff,
            false
        ));
        // Interior (no band, no handle) -> NOT captured: passthrough preserved.
        assert!(!pointer_over_portal_affordance(
            Some(rect),
            200.0,
            175.0,
            aff,
            false
        ));
        // Over the drag-handle -> captured even off the resize bands.
        assert!(pointer_over_portal_affordance(
            Some(rect),
            200.0,
            175.0,
            aff,
            true
        ));
        // No focused portal, no drag handle -> never widens passthrough.
        assert!(!pointer_over_portal_affordance(
            None, 101.0, 101.0, aff, false
        ));
        // No focused portal but over a drag handle -> still captured: the
        // snapshot-wide drag-handle hit lets the user grab/focus any portal,
        // matching the `Grab` cursor `portal_hover_cursor` shows there.
        assert!(pointer_over_portal_affordance(
            None, 101.0, 101.0, aff, true
        ));
    }

    #[test]
    fn scene_display_area_sync_uses_actual_surface_size() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);

        sync_scene_display_area(&mut scene, 2560, 1440);

        assert_eq!(scene.display_area.width, 2560.0);
        assert_eq!(scene.display_area.height, 1440.0);
    }

    #[test]
    fn scene_display_area_sync_ignores_zero_dimensions() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);

        sync_scene_display_area(&mut scene, 0, 1440);

        assert_eq!(scene.display_area.width, 1920.0);
        assert_eq!(scene.display_area.height, 1080.0);
    }

    fn control(
        kind: tze_hud_scene::types::ZoneInteractionKind,
        bounds: Rect,
        publication: SceneId,
    ) -> tze_hud_scene::types::ZoneHitRegion {
        tze_hud_scene::types::ZoneHitRegion {
            publication_id: publication,
            zone_name: "notification-area".to_string(),
            published_at_wall_us: 1,
            publisher_namespace: "agent".to_string(),
            bounds,
            kind,
            interaction_id: format!("control:{}:{}", bounds.x, bounds.y),
            tab_order: 0,
        }
    }

    /// The cursor the app resolves at `(x, y)` from the last frame's controls.
    fn cursor_at(app: &mut WinitApp, x: f32, y: f32) -> PortalCursor {
        app.refresh_widget_hover_tracking();
        app.state.cursor_x = x;
        app.state.cursor_y = y;
        app.update_portal_cursor_icon();
        app.state.cursor_tracker.current()
    }

    /// R3.1 S1-S3: card actions and ×, a portal × and its unread badge are
    /// pressable; an answered card's controls, card controls in safe mode and
    /// drag handles are not.
    #[test]
    fn pressable_regions_follow_what_a_press_would_do() {
        use tze_hud_scene::types::ZoneInteractionKind as Kind;
        let (card, answered, tile) = (SceneId::new(), SceneId::new(), SceneId::new());
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let at = |x: f32| Rect::new(x, 10.0, 20.0, 20.0);
        scene.overlay.zone_hit_regions = vec![
            control(Kind::Action { index: 0 }, at(0.0), card),
            control(Kind::Dismiss, at(100.0), card),
            control(Kind::JumpToLatest { tile_id: tile }, at(200.0), tile),
            control(Kind::DismissTile { tile_id: tile }, at(300.0), tile),
            control(Kind::Action { index: 1 }, at(400.0), answered),
            control(
                Kind::DragHandle {
                    element_id: tile,
                    element_kind: tze_hud_scene::DragHandleElementKind::Tile,
                    is_header_band: true,
                },
                at(500.0),
                tile,
            ),
        ];
        scene
            .overlay
            .notification_choices
            .insert(answered, "chosen".into());
        let xs = |safe_mode| -> Vec<f32> {
            pressable_control_regions(&scene, safe_mode)
                .iter()
                .map(|r| r.x)
                .collect()
        };
        assert_eq!(xs(false), [0.0, 100.0, 200.0, 300.0]);
        assert_eq!(xs(true), [200.0, 300.0], "safe mode refuses card controls");
    }

    /// R3.1 S7: a control inside a drag-handle band shows the hand and the band
    /// beside it the open hand; S2: off every control it is the arrow.
    #[test]
    fn pressable_control_beats_drag_handle_cursor() {
        use super::super::test_support::make_windowed_keyboard_test_app;
        use tze_hud_input::{FocusManager, InputProcessor};
        // Handle band: x 580..620, y 290..310.
        let (mut scene, tile, _, _) = scene_with_drag_handle_tile(400.0, 300.0, 400.0, 200.0);
        scene.overlay.zone_hit_regions.push(control(
            tze_hud_scene::types::ZoneInteractionKind::DismissTile { tile_id: tile },
            Rect::new(604.0, 292.0, 14.0, 14.0),
            tile,
        ));
        let snapshot = crate::pipeline::HitTestSnapshot::from_scene(&scene);
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());
        app.state
            .pipeline
            .hit_test_snapshot
            .store(Arc::new(snapshot));
        assert_eq!(cursor_at(&mut app, 610.0, 298.0), PortalCursor::Pointer);
        assert_eq!(cursor_at(&mut app, 585.0, 298.0), PortalCursor::Grab);
        assert_eq!(cursor_at(&mut app, 1000.0, 900.0), PortalCursor::Default);
    }

    /// R11.1 S2: a click-through overlay reports no cursor whatever shape the
    /// tracker last held; a capturing overlay and other modes report it.
    #[test]
    fn admin_cursor_is_null_while_the_app_beneath_owns_the_pointer() {
        use super::super::test_support::make_windowed_keyboard_test_app;
        use tze_hud_input::{FocusManager, InputProcessor};
        let (mut app, _rx) = make_windowed_keyboard_test_app(
            SceneGraph::new(1920.0, 1080.0),
            FocusManager::new(),
            InputProcessor::new(),
        );
        app.state.cursor_tracker.update(PortalCursor::Pointer);
        app.state.effective_mode = WindowMode::Overlay;
        app.state.overlay_capturing = false;
        assert_eq!(app.applied_cursor_name(), None);
        app.state.overlay_capturing = true;
        assert_eq!(app.applied_cursor_name(), Some("hand"));
        app.state.effective_mode = WindowMode::Fullscreen;
        app.state.overlay_capturing = false;
        assert_eq!(app.applied_cursor_name(), Some("hand"));
    }

    /// A control on a secondary display shows the hand on that display's
    /// window, not the primary's; leaving it returns that window to the arrow.
    #[test]
    fn pressable_cursor_lands_on_the_pointers_display_window() {
        use super::super::test_support::make_windowed_keyboard_test_app;
        use tze_hud_input::{FocusManager, InputProcessor};
        use tze_hud_scene::types::ZoneInteractionKind as Kind;
        // A second 1920-wide display to the right of the primary.
        let (primary, secondary) = (WindowId::from(1), WindowId::from(2));
        let mut scene = SceneGraph::new(3840.0, 1080.0);
        scene.overlay.zone_hit_regions.push(control(
            Kind::Action { index: 0 },
            Rect::new(2000.0, 100.0, 80.0, 24.0),
            SceneId::new(),
        ));
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());
        app.state.cursor_window = Some(secondary);
        assert_eq!(cursor_at(&mut app, 2020.0, 110.0), PortalCursor::Pointer);
        assert_eq!(app.state.cursor_shape_window, Some(secondary));
        app.state.cursor_window = Some(primary);
        assert_eq!(cursor_at(&mut app, 100.0, 110.0), PortalCursor::Default);
        assert_eq!(app.state.cursor_shape_window, Some(primary));
    }

    /// Only the pointer's window shows a shape: crossing displays resets the
    /// old window, and an unchanged shape on the same window costs no call.
    #[test]
    fn cursor_shape_moves_with_the_pointers_window() {
        use PortalCursor::{Default as Arrow, Pointer as Hand};
        let (a, b) = (Some(WindowId::from(1)), Some(WindowId::from(2)));
        let (ida, idb) = (a.unwrap(), b.unwrap());
        assert_eq!(cursor_shape_calls(Arrow, Hand, true, a, a), [(ida, Hand)]);
        assert!(cursor_shape_calls(Hand, Hand, false, a, a).is_empty());
        assert_eq!(
            cursor_shape_calls(Hand, Hand, false, a, b),
            [(ida, Arrow), (idb, Hand)],
            "no stale hand on the display the pointer left"
        );
        assert_eq!(cursor_shape_calls(Hand, Arrow, true, a, b), [(ida, Arrow)]);
        assert_eq!(cursor_shape_calls(Arrow, Hand, true, a, b), [(idb, Hand)]);
        assert!(cursor_shape_calls(Arrow, Arrow, false, a, b).is_empty());
        assert_eq!(
            cursor_shape_calls(Arrow, Hand, true, None, a),
            [(ida, Hand)]
        );
    }

    /// R3.1 S3-S5: answering a card or safe mode turns its controls back to
    /// the arrow under a resting pointer, a control that appears or leaves
    /// changes the shape without pointer motion, and an active drag keeps its
    /// closed hand over a control.
    #[test]
    fn pressable_cursor_follows_refusal_geometry_and_active_drag() {
        use super::super::test_support::make_windowed_keyboard_test_app;
        use tze_hud_input::{FocusManager, InputProcessor};
        use tze_hud_scene::types::ZoneInteractionKind as Kind;
        let card = SceneId::new();
        let button = control(
            Kind::Action { index: 0 },
            Rect::new(100.0, 100.0, 80.0, 24.0),
            card,
        );
        let (mut app, _rx) = make_windowed_keyboard_test_app(
            SceneGraph::new(1920.0, 1080.0),
            FocusManager::new(),
            InputProcessor::new(),
        );
        let with_scene = |app: &mut WinitApp, f: &dyn Fn(&mut SceneGraph)| {
            let shared = app.state.shared_state.try_lock().unwrap();
            f(&mut shared.scene.try_lock().unwrap());
        };
        assert_eq!(cursor_at(&mut app, 120.0, 110.0), PortalCursor::Default);
        // A card appears under the resting pointer.
        with_scene(&mut app, &|scene| {
            scene.overlay.zone_hit_regions.push(button.clone())
        });
        assert_eq!(cursor_at(&mut app, 120.0, 110.0), PortalCursor::Pointer);
        app.state
            .safe_mode_atomic
            .store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            cursor_at(&mut app, 120.0, 110.0),
            PortalCursor::Default,
            "safe mode"
        );
        app.state
            .safe_mode_atomic
            .store(false, std::sync::atomic::Ordering::Release);
        with_scene(&mut app, &|scene| {
            scene.overlay.notification_choices.insert(card, "x".into());
        });
        assert_eq!(
            cursor_at(&mut app, 120.0, 110.0),
            PortalCursor::Default,
            "answered"
        );
        with_scene(&mut app, &|scene| {
            scene.overlay.notification_choices.clear();
        });
        assert_eq!(cursor_at(&mut app, 120.0, 110.0), PortalCursor::Pointer);
        // The card leaves from under the resting pointer.
        with_scene(&mut app, &|scene| scene.overlay.zone_hit_regions.clear());
        assert_eq!(cursor_at(&mut app, 120.0, 110.0), PortalCursor::Default);
        // An active drag keeps its closed hand whatever lies beneath.
        with_scene(&mut app, &|scene| {
            scene.overlay.zone_hit_regions.push(button.clone())
        });
        let mut drag = tze_hud_input::DeviceDragState::new(
            "drag".into(),
            card,
            tze_hud_scene::DragHandleElementKind::Zone,
            120.0,
            110.0,
            250,
            true,
        );
        drag.phase = tze_hud_input::DragPhase::Activated;
        app.state.input_processor.drag_states.insert(0, drag);
        assert_eq!(cursor_at(&mut app, 120.0, 110.0), PortalCursor::Grabbing);
    }
}
