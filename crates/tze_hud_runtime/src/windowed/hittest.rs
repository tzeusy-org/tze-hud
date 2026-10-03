use tze_hud_compositor::{Compositor, CompositorSurface};
use tze_hud_input::{PortalCursor, PortalRect, ResizeEdge, hit_affordance, portal_hover_cursor};
use tze_hud_scene::SceneId;
use tze_hud_scene::graph::SceneGraph;
use winit::window::CursorIcon;

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
        if tile.input_mode == tze_hud_scene::InputMode::Passthrough {
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
    regions
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
                        self.state.cursor_x = cursor_x;
                        self.state.cursor_y = cursor_y;
                    } else {
                        tracing::trace!(
                            "overlay: skipped OS cursor refresh because window origin is unavailable"
                        );
                    }
                }
            }
        }
    }

    /// True while raw mouse input must wake the loop, because passthrough could
    /// hide a click from an interactive region (see [`wake::cursor_entry_armed`]).
    pub(super) fn cursor_entry_armed(&self) -> bool {
        let snapshot = self.state.pipeline.hit_test_snapshot.load();
        let interactive = !self.state.hit_regions.is_empty()
            || !snapshot.drag_handles.is_empty()
            || snapshot
                .tiles
                .iter()
                .any(|tile| tile.has_scroll_config || tile.dismissible);
        wake::cursor_entry_armed(
            self.state.effective_mode == WindowMode::Overlay,
            interactive,
            !self.state.overlay_capturing,
        )
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

    /// Lock-free view of what the cursor currently hovers or would capture.
    pub(super) fn cursor_entry_signature(&self) -> CursorEntrySignature {
        let (x, y) = (self.state.cursor_x, self.state.cursor_y);
        CursorEntrySignature {
            capture: should_capture_pointer_event(
                WindowMode::Overlay,
                x,
                y,
                &self.state.hit_regions,
            ) || self.cursor_over_focused_portal_affordance(),
            grip: self.resize_grip_hover_target(),
            close_target: self.close_hover_target(),
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
            || self.state.pending_mode_switch.is_some()
            || self.state.left_button_down
            || self.state.quiescent_efficiency.is_some()
            || !self.state.pending_input_capture_commands.is_empty()
            || !self.state.pending_keyboard_events.is_empty()
            || self.state.wake.has_unfinished_main_work();
        if has_other_work || previous != Some(signature) {
            self.state
                .wake
                .mark_main_work_pending(RuntimeWakeupSource::AnimationDeadline);
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
            || self.cursor_over_focused_portal_affordance();
        self.state.overlay_capturing = should_capture;
        if let Some(window) = &self.state.window {
            if let Err(e) = window.set_cursor_hittest(should_capture) {
                tracing::trace!(
                    error = %e,
                    capture = should_capture,
                    "overlay: set_cursor_hittest failed"
                );
            }
        }
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
    /// currently over, if any (hud-wgiys).
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
        let affordance_px = tze_hud_config::resolve_portal_tokens(&self.state.global_tokens)
            .window_resize_affordance_px;
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
    /// overlay mode (hud-adh61, follow-up to hud-g5yu1).
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
        let affordance_px = tze_hud_config::resolve_portal_tokens(&self.state.global_tokens)
            .window_resize_affordance_px;
        let over_drag_handle = self
            .state
            .pipeline
            .hit_test_snapshot
            .load()
            .hit_test_drag_handle(x, y);
        pointer_over_portal_affordance(focused_portal, x, y, affordance_px, over_drag_handle)
    }

    /// Update the OS cursor shape to reflect the portal resize/move affordance
    /// under the pointer (hud-g5yu1).
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

        let portal_part = tze_hud_config::resolve_portal_tokens(&self.state.global_tokens);
        let affordance_px = portal_part.window_resize_affordance_px;

        let desired = portal_hover_cursor(
            x,
            y,
            focused_portal,
            affordance_px,
            over_drag_handle,
            active_edge,
            drag_active,
        );

        if let Some(next) = self.state.cursor_tracker.update(desired) {
            if let Some(window) = &self.state.window {
                window.set_cursor(portal_cursor_to_winit(next));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{scene_with_capture_tile, scene_with_drag_handle_tile};
    use super::*;
    use tze_hud_compositor::HeadlessSurface;

    #[test]
    fn combined_overlay_hit_regions_includes_zone_interaction_regions() {
        let static_regions = vec![HitRegion::new(10.0, 20.0, 30.0, 40.0)];
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        scene
            .overlay
            .zone_hit_regions
            .push(tze_hud_scene::types::ZoneHitRegion {
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
        assert!(
            !stale_snapshot.hit_test_drag_handle(800.0, 330.0),
            "pre-refresh snapshot still carries the previous frame's drag-handle bounds"
        );

        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        compositor.render_frame(&mut scene, &surface);
        refresh_interaction_hit_regions_after_render(&compositor, &mut scene, &surface);
        let refreshed_snapshot = crate::pipeline::HitTestSnapshot::from_scene(&scene);

        assert!(
            refreshed_snapshot.hit_test_drag_handle(800.0, 330.0),
            "snapshot built after the windowed post-render refresh must match the newly displayed drag-handle geometry"
        );
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
            "cursor polling must convert desktop coordinates into overlay-window coordinates"
        );
    }

    #[test]
    fn cursor_refresh_origin_falls_back_to_monitor_position() {
        let monitor_origin = winit::dpi::PhysicalPosition::new(2560, 0);

        let origin = cursor_refresh_window_origin(None, || Some(monitor_origin));

        assert_eq!(
            origin,
            Some(monitor_origin),
            "Windows overlay cursor polling should use the monitor origin if the window origin \
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
        // hud-adh61: in overlay passthrough mode the focused portal's resize
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
}
