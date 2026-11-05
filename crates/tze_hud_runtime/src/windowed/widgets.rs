use std::time::Instant;

use tze_hud_compositor::LocalComposerState;
use tze_hud_scene::types::{DragHandleContextMenuState, WidgetParameterValue};

use super::hittest::combined_overlay_hit_regions;
use super::input_dispatch::nanoseconds_since_start;
use super::lifecycle::record_pending_input_latency;
use super::{HitRegion, WinitApp};
use crate::widget_hover::{
    WidgetHoverTracker, build_hover_trackers, hidden_mutations_for_removed, tick_hover_trackers,
};

/// Default empty-draft placeholder hint used when a composer's owning
/// `HitRegionNode` has no `composer_placeholder` override configured
/// (hud-evk0j).
///
/// The portal composer is a chat-message composer (`portal-bottom-chat-composer`),
/// so this matches the existing UX. Rendered — dimmed, from
/// `portal.composer.placeholder_color` — only while the draft is empty; it is
/// never part of the draft buffer and is never submitted.
///
/// Per-composer override (hud-se6hs, follow-up to hud-evk0j): a
/// `HitRegionNode`'s `composer_placeholder` config is threaded through
/// `composer_draft_snapshot` and resolved below — `None` inherits this
/// default, `Some("")` opts the composer out of any placeholder, and
/// `Some(text)` supplies non-chat composers with their own hint copy.
const COMPOSER_DEFAULT_PLACEHOLDER: &str = "Type a message…";

/// Window-space anchor rect for the OS IME candidate window, in physical pixels
/// (hud-hxhnt finding 3). `x`/`y` is the caret's top-left; `height` is the caret
/// line height. Width is a fixed 1px (the caret is a thin vertical bar).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ImeCaretAnchor {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) height: f32,
}

/// Compute the IME candidate-window anchor for a composer caret (hud-hxhnt
/// finding 3), given the node's window-space region and the compositor-published
/// shaped layout. Pure geometry so it is unit-testable without a live window.
///
/// - `region_{x,y,w,h}` — the composer node's window-space bounds (tile origin +
///   tile-local HitRegion bounds).
/// - `content_inset` — the composer's interior text inset (`portal.spacing.content_inset_px`).
/// - `layout` — the FRESH published `ComposerVisualLayout` (caller filters on
///   `text_len`); `None` when none is available yet.
/// - `cursor_byte` — the caret's raw-draft byte offset.
///
/// With a fresh layout the caret x is the shaped `x_at_cursor` (offset by the
/// inset), corrected by `l.h_scroll_px` for the single-line profile's
/// caret-follow horizontal scroll — `x_at_cursor` is measured in unscrolled
/// draft space, so this recovers the SAME rendered screen x the chrome-layer
/// caret quad draws at (`layout.caret_x - layout.h_scroll_px`,
/// `append_composer_caret_vertices`); `h_scroll_px` is `0.0` for the
/// multi-line profile (wraps instead of scrolling), so this is a no-op there.
/// When the layout carries input-box geometry (multi-line profile), the
/// vertical position is the caret's visual row within that box. Without a
/// layout (first post-focus frame, or a single-line profile before its layout is
/// published) the anchor falls back to the node interior — still near the caret,
/// never the window origin. The x is clamped into the node interior so a long
/// draft cannot push the anchor outside the composer region.
pub(super) fn composer_ime_caret_anchor(
    region_x: f32,
    region_y: f32,
    region_w: f32,
    region_h: f32,
    content_inset: f32,
    layout: Option<&tze_hud_input::ComposerVisualLayout>,
    cursor_byte: usize,
) -> ImeCaretAnchor {
    let (caret_x, caret_top, caret_h) = match layout {
        Some(l) => {
            let x = content_inset + l.x_at_cursor(cursor_byte) - l.h_scroll_px;
            let (top, h) = match l.input_box {
                Some(bx) => {
                    let row = l.line_of(cursor_byte).unwrap_or(0);
                    (bx.row0_top + row as f32 * bx.line_height, bx.line_height)
                }
                // Single-line / no box geometry: caret spans the interior.
                None => (content_inset, (region_h - content_inset * 2.0).max(1.0)),
            };
            (x, top, h)
        }
        None => (
            content_inset,
            (region_h - content_inset).max(0.0),
            content_inset.max(1.0),
        ),
    };

    let caret_x = caret_x.clamp(content_inset, (region_w - content_inset).max(content_inset));
    ImeCaretAnchor {
        x: region_x + caret_x,
        y: region_y + caret_top,
        height: caret_h.max(1.0),
    }
}

/// Resolve a composer's placeholder hint from its `HitRegionNode` override
/// (as reported by `composer_draft_snapshot`), falling back to
/// [`COMPOSER_DEFAULT_PLACEHOLDER`] when unset.
///
/// Mirrors `HitRegionNode::composer_placeholder`'s three-state convention:
/// `None` (unset) → the global default; `Some("")` (explicit opt-out) →
/// `None` (no placeholder rendered); `Some(text)` (custom) → `Some(text)`.
fn resolve_composer_placeholder(override_config: Option<String>) -> Option<String> {
    match override_config {
        None => Some(COMPOSER_DEFAULT_PLACEHOLDER.to_string()),
        Some(text) if text.is_empty() => None,
        Some(text) => Some(text),
    }
}

/// Runtime-only press identity. It survives menu dismissal until the matching
/// release so cancelled/stale edges can never reach agent content underneath.
#[derive(Clone, Copy)]
pub(super) struct ContextMenuPress {
    menu: Option<(tze_hud_scene::SceneId, u64)>,
    window: winit::window::WindowId,
    pub(super) armed: bool,
}

fn menu_target_live(scene: &tze_hud_scene::SceneGraph, id: tze_hud_scene::SceneId) -> bool {
    scene.tile_id_shown(id)
        || (scene.zones_shown()
            && scene.zone_by_id(id).is_some_and(|zone| {
                scene
                    .zone_registry
                    .active_publishes
                    .get(&zone.name)
                    .is_some_and(|p| !p.is_empty())
            }))
}

impl WinitApp {
    /// Push the current composer draft snapshot to the compositor thread for
    /// local echo rendering (hud-r3ax6).
    ///
    /// Called immediately after every keystroke that mutates the draft buffer.
    /// The snapshot is written to the shared `local_composer_state` slot;
    /// the compositor thread drains it once per frame (no round-trip).
    ///
    /// Also records an input-to-local-ack latency sample so the p99 measurement
    /// required by hud-o9ybl can be produced for the composer path.
    ///
    /// No-ops if the draft manager reports no active draft (safety guard).
    pub(super) fn push_local_composer_echo(&mut self, input_started_at: std::time::Instant) {
        if let Some((text, cursor_byte, selection_anchor, at_capacity, node_id, placeholder_cfg)) =
            self.state.input_processor.composer_draft_snapshot()
        {
            let state = LocalComposerState {
                text,
                cursor_byte,
                selection_anchor,
                at_capacity,
                node_id,
                placeholder: resolve_composer_placeholder(placeholder_cfg),
            };
            if let Ok(mut guard) = self.state.local_composer_state.lock() {
                *guard = Some(Some(state));
            }
            // Measure input-to-local-ack: the "local ack" for a composer
            // keystroke is the moment the snapshot is handed to the compositor.
            // This is the equivalent of `result.local_ack_us` for pointer events.
            let local_ack_us = input_started_at.elapsed().as_micros() as u64;
            record_pending_input_latency(
                &self.state.pending_input_latency,
                input_started_at,
                local_ack_us,
            );
            // Request a redraw so the compositor picks up the new state promptly.
            if let Some(window) = &self.state.window {
                window.request_redraw();
            }
            // The caret may have moved; re-anchor the OS IME candidate window to
            // it so CJK/other input methods track the caret (hud-hxhnt finding 3).
            self.update_composer_ime_cursor_area();
        }
    }

    /// Anchor the OS IME candidate window to the shaped composer caret position
    /// (hud-hxhnt finding 3).
    ///
    /// `set_ime_allowed(true)` is set at composer focus (see
    /// [`super::lifecycle::focus_window_for_text_input`]) but `set_ime_cursor_area`
    /// was never called, so CJK/accented candidate windows floated at the window
    /// origin instead of following the caret. This re-anchors the IME area on
    /// every composer caret move. Position-only: `Ime::Preedit` composition stays
    /// v1-reserved per the input-model spec — this only tells the OS *where* to
    /// place the candidate window, not how to render preedit.
    ///
    /// Geometry is single-authority: the caret x, visual row, and input-box come
    /// from the compositor-published `composer_visual_layout` (the same slot the
    /// pointer hit-test and vertical caret movement read), never re-derived here.
    /// When no fresh layout is published yet (e.g. the first frame after focus, or
    /// a single-line profile before its layout is published) the area is anchored
    /// to the composer node's interior as a safe fallback — still near the caret,
    /// never the window origin. Best-effort: a momentarily-contended scene lock
    /// skips this update and the next caret move refreshes it.
    pub(super) fn update_composer_ime_cursor_area(&self) {
        let Some(window) = self.state.window.as_ref() else {
            return;
        };
        let Some((text, cursor_byte, _, _, node_id, _)) =
            self.state.input_processor.composer_draft_snapshot()
        else {
            return;
        };
        let Some(tile_id) = self.composer_focused_tile_id() else {
            return;
        };

        // Resolve the composer node's window-space region: tile origin + the
        // node's tile-local HitRegion bounds (the inverse of the pointer path's
        // `tile_local_pointer_xy`). Scene lock is best-effort — skip on contention.
        let Ok(state) = self.state.shared_state.try_lock() else {
            return;
        };
        let Ok(scene) = state.scene.try_lock() else {
            return;
        };
        let Some(tile) = scene.tiles.get(&tile_id) else {
            return;
        };
        let Some(node) = scene.nodes.get(&node_id) else {
            return;
        };
        let tze_hud_scene::NodeData::HitRegion(hr) = &node.data else {
            return;
        };
        let region_x = tile.bounds.x + hr.bounds.x;
        let region_y = tile.bounds.y + hr.bounds.y;
        let region_w = hr.bounds.width.max(1.0);
        let region_h = hr.bounds.height.max(1.0);
        drop(scene);
        drop(state);

        let content_inset =
            tze_hud_config::resolve_portal_tokens(&self.state.global_tokens).content_inset_px;

        // Prefer the shaped caret geometry from the published layout; fall back to
        // the node interior when no fresh layout exists.
        let layout = self
            .state
            .composer_visual_layout
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .filter(|l| l.text_len == text.len() && !l.lines.is_empty());

        let anchor = composer_ime_caret_anchor(
            region_x,
            region_y,
            region_w,
            region_h,
            content_inset,
            layout.as_ref(),
            cursor_byte,
        );
        window.set_ime_cursor_area(
            winit::dpi::PhysicalPosition::new(anchor.x, anchor.y),
            winit::dpi::PhysicalSize::new(1.0_f32, anchor.height),
        );
    }

    /// Clear the local composer echo (called on blur, submit, cancel).
    ///
    /// Stores `Some(None)` (explicit deactivation) in the shared slot so the
    /// compositor clears the overlay on the next frame.
    pub(super) fn clear_local_composer_echo(&mut self) {
        if let Ok(mut guard) = self.state.local_composer_state.lock() {
            *guard = Some(None);
        }
        if let Some(window) = &self.state.window {
            window.request_redraw();
        }
    }

    /// Rebuild runtime-managed widget hover trackers and refresh overlay hit-regions.
    pub(super) fn refresh_widget_hover_tracking(&mut self) {
        let (surf_w, surf_h) = if let Some(window) = &self.state.window {
            let size = window.inner_size();
            (size.width as f32, size.height as f32)
        } else {
            (
                self.state.config.window.width as f32,
                self.state.config.window.height as f32,
            )
        };

        let mut next_trackers: Option<std::collections::HashMap<String, WidgetHoverTracker>> = None;
        let mut dynamic_hit_regions: Option<Vec<HitRegion>> = None;
        let mut removed_mutations = Vec::new();
        if let Ok(state) = self.state.shared_state.try_lock() {
            if let Ok(scene) = state.scene.try_lock() {
                let next =
                    build_hover_trackers(&scene, surf_w, surf_h, &self.state.widget_hover_trackers);
                removed_mutations =
                    hidden_mutations_for_removed(&self.state.widget_hover_trackers, &next);
                dynamic_hit_regions = Some(combined_overlay_hit_regions(
                    &self.state.static_hit_regions,
                    &scene,
                ));
                next_trackers = Some(next);
            }
        }
        if let Some(next) = next_trackers {
            self.state.widget_hover_trackers = next;
        }
        self.apply_widget_hover_mutations(removed_mutations);

        // Pointer capture includes explicit static regions plus compositor-managed
        // zone interaction regions (notification dismiss/action affordances).
        //
        // If the scene lock is briefly unavailable, keep the last known dynamic
        // regions. Dropping to the usually-empty static set makes overlay
        // hit-testing flicker to passthrough during mutation bursts.
        if let Some(dynamic_hit_regions) = dynamic_hit_regions {
            self.state.hit_regions = dynamic_hit_regions;
        } else if self.state.hit_regions.is_empty() {
            self.state.hit_regions = self.state.static_hit_regions.clone();
        }
    }

    /// Tick widget hover trackers and apply local parameter mutations.
    pub(super) fn tick_widget_hover_tracking(&mut self) {
        if self.state.widget_hover_trackers.is_empty() {
            return;
        }
        let mutations = tick_hover_trackers(
            &mut self.state.widget_hover_trackers,
            self.state.cursor_x,
            self.state.cursor_y,
            Instant::now(),
        );
        self.apply_widget_hover_mutations(mutations);
    }

    /// Apply runtime-local hover mutations to widget instance params.
    fn apply_widget_hover_mutations(
        &mut self,
        mutations: Vec<crate::widget_hover::WidgetHoverMutation>,
    ) {
        if mutations.is_empty() {
            return;
        }

        if let Ok(state) = self.state.shared_state.try_lock() {
            if let Ok(mut scene) = state.scene.try_lock() {
                for mutation in mutations {
                    if let Err(e) = scene.set_widget_param_local(
                        &mutation.instance_name,
                        &mutation.param_name,
                        WidgetParameterValue::F32(mutation.value),
                    ) {
                        tracing::debug!(
                            error = %e,
                            widget = %mutation.instance_name,
                            param = %mutation.param_name,
                            "widget hover: failed to apply local hover mutation"
                        );
                    }
                }
            }
        }
    }

    // ── Chrome context menu (drag handle reset gesture) ────────────────────

    /// Show the chrome context menu anchored at the cursor position if the
    /// cursor is currently over a drag handle.
    ///
    /// Called on right-click (desktop).  No-op if the cursor is not on a
    /// drag handle.
    pub(super) fn handle_right_click_on_drag_handle(&mut self, secondary: Option<usize>) {
        let (cx, cy) = (self.state.cursor_x, self.state.cursor_y);
        let opened = {
            let Ok(state) = self.state.shared_state.try_lock() else {
                return;
            };
            let Ok(mut scene) = state.scene.try_lock() else {
                return;
            };
            let Some(element_id) = scene
                .overlay
                .drag_handle_hit_regions
                .iter()
                .find(|r| r.bounds.contains_point(cx, cy) && menu_target_live(&scene, r.element_id))
                .map(|r| r.element_id)
            else {
                return;
            };
            let monitor = secondary
                .and_then(|i| self.state.secondaries.get(i).map(|s| &s.spec))
                .or(self.state.primary_monitor.as_ref());
            let viewport = monitor
                .zip(self.state.primary_monitor.as_ref())
                .map(|(monitor, primary)| monitor.scene_rect(primary));
            let px_scale = monitor.map(|m| {
                m.scale as f32 * tze_hud_config::tokens::ui_scale(&self.state.global_tokens)
            });
            scene.overlay.drag_handle_context_menu = Some(DragHandleContextMenuState {
                element_id,
                anchor_x: cx,
                anchor_y: cy,
                shown_at_ns: nanoseconds_since_start(),
                reset_button_rect: None,
                viewport,
                px_scale,
            });
            scene.bump_geometry_epoch();
            true
        };
        if opened {
            self.state.context_menu_open = true;
            self.state.context_menu_dismiss_pending = false;
            if let Some(press) = self.state.context_menu_press.as_mut() {
                press.armed = false;
            }
        }
    }

    /// Own both delivered edges before generic pointer dispatch. Reset requires
    /// the same painted menu, item and window on Down and Up. Busy/stale input
    /// is consumed and dismissed, never replayed against a newer menu or agent.
    pub(super) fn handle_context_menu_edge(
        &mut self,
        pressed: bool,
        window: winit::window::WindowId,
    ) -> bool {
        let menu = {
            self.state.shared_state.try_lock().ok().and_then(|shared| {
                shared
                    .scene
                    .try_lock()
                    .ok()
                    .map(|scene| scene.overlay.drag_handle_context_menu.clone())
            })
        };
        let (x, y) = (self.state.cursor_x, self.state.cursor_y);
        if pressed {
            let snapshot_has_menu = self
                .state
                .pipeline
                .hit_test_snapshot
                .load()
                .context_menu
                .is_some();
            if !self.state.context_menu_open
                && !snapshot_has_menu
                && menu.as_ref().is_none_or(|m| m.is_none())
            {
                return false;
            }
            // Cancellation is authoritative immediately, even before its
            // deferred scene teardown can acquire the lock.
            let current = menu
                .as_ref()
                .and_then(|m| m.as_ref())
                .filter(|_| !self.state.context_menu_dismiss_pending);
            self.state.context_menu_press = Some(ContextMenuPress {
                menu: current.map(|m| (m.element_id, m.shown_at_ns)),
                window,
                armed: current
                    .is_some_and(|m| m.reset_button_rect.is_some_and(|r| r.contains_point(x, y))),
            });
            if menu.is_none() {
                self.cancel_context_menu();
            }
            return true;
        }
        let Some(press) = self.state.context_menu_press.take() else {
            return false;
        };
        let reset = {
            let mut reset = None;
            if let Ok(shared) = self.state.shared_state.try_lock()
                && let Ok(mut scene) = shared.scene.try_lock()
            {
                if let Some(menu) = scene.overlay.drag_handle_context_menu.as_ref()
                    && press.armed
                    && press.window == window
                    && press.menu == Some((menu.element_id, menu.shown_at_ns))
                    && nanoseconds_since_start().saturating_sub(menu.shown_at_ns) < 3_000_000_000
                    && menu
                        .reset_button_rect
                        .is_some_and(|r| r.contains_point(x, y))
                    && menu_target_live(&scene, menu.element_id)
                {
                    reset = Some(menu.element_id);
                }
                if scene.overlay.drag_handle_context_menu.take().is_some() {
                    scene.bump_geometry_epoch();
                }
                self.state.context_menu_dismiss_pending = false;
            } else {
                self.state.context_menu_dismiss_pending = true;
            }
            reset
        };
        self.state.context_menu_open = false;
        if self.state.context_menu_dismiss_pending {
            self.schedule_shared_scene_availability_wake();
        }
        if let Some(id) = reset {
            self.perform_reset_element_geometry(id);
        }
        true
    }

    pub(super) fn cancel_context_menu(&mut self) {
        if let Some(press) = self.state.context_menu_press.as_mut() {
            press.armed = false;
        }
        if self.state.context_menu_open {
            self.state.context_menu_dismiss_pending = true;
            self.state.context_menu_open = false;
        }
    }

    /// Auto-dismiss the context menu after 3 seconds.
    ///
    /// Called each frame from `RedrawRequested`.  No-op when no menu is showing.
    pub(super) fn tick_context_menu_auto_dismiss(&mut self) {
        let settled = {
            if let Ok(shared) = self.state.shared_state.try_lock()
                && let Ok(mut scene) = shared.scene.try_lock()
            {
                let expired = scene
                    .overlay
                    .drag_handle_context_menu
                    .as_ref()
                    .is_some_and(|m| {
                        nanoseconds_since_start().saturating_sub(m.shown_at_ns) >= 3_000_000_000
                            || !menu_target_live(&scene, m.element_id)
                    });
                if (expired || self.state.context_menu_dismiss_pending)
                    && scene.overlay.drag_handle_context_menu.take().is_some()
                {
                    scene.bump_geometry_epoch();
                }
                self.state.context_menu_open = scene.overlay.drag_handle_context_menu.is_some();
                self.state.context_menu_dismiss_pending = false;
                true
            } else {
                false
            }
        };
        if !settled && self.state.context_menu_dismiss_pending {
            self.schedule_shared_scene_availability_wake();
        }
    }

    /// Synchronously reset the geometry override for `element_id` and broadcast
    /// an `ElementRepositionedEvent` (hud-zc7f).
    ///
    /// This is the sync chrome-layer path for the "Reset to default" context
    /// menu action.  It mirrors the logic in `HudSessionImpl::reset_element_geometry`
    /// but runs on the main thread without async, using the stored broadcast sender.
    ///
    /// No-op if the element has no user override.
    fn perform_reset_element_geometry(&mut self, element_id: tze_hud_scene::SceneId) {
        // Collect previous override, fallback geometry, and optional persist path.
        let (previous_override, fallback_geometry, store_snapshot, persist_path) = {
            let Ok(mut state) = self.state.shared_state.try_lock() else {
                tracing::warn!("perform_reset_element_geometry: could not acquire shared state");
                return;
            };
            // Hand geometry authority back to the adapter FIRST (hud-lyqun),
            // before the override no-op short-circuit below: a whole-portal
            // move/resize takes the viewer lock but does not always write an
            // element-store override, so the lock must be released here even when
            // there is no override to clear. A portal member's reset releases the
            // WHOLE portal group so every constituent surface becomes
            // adapter-controlled again — not just the one the reset menu was
            // opened on.
            {
                // Classify under the scene lock before touching saved overrides.
                // A busy or removed Zone menu must never fall into Tile persistence.
                let Ok(mut scene) = state.scene.try_lock() else {
                    return;
                };
                let stored_zone = state
                    .element_store
                    .entries
                    .get(&element_id)
                    .is_some_and(|entry| entry.element_type == tze_hud_scene::ElementType::Zone);
                if scene.zone_by_id(element_id).is_some() || stored_zone {
                    scene.reset_zone_placement(element_id);
                    return;
                }
                match super::portal::resolve_portal_group(&scene, element_id) {
                    Some(group) => {
                        for member_id in group.member_ids {
                            scene.unlock_viewer_geometry(member_id);
                        }
                    }
                    None => scene.unlock_viewer_geometry(element_id),
                }
            }

            // Clear the override.
            let previous = state.element_store.reset_geometry_override(element_id);
            let Some(previous) = previous else {
                tracing::debug!(
                    element_id = %element_id,
                    "perform_reset_element_geometry: no override — geometry lock released, no override to clear"
                );
                return;
            };
            // Resolve fallback geometry (agent bounds → config → default policy).
            let fallback = {
                let Ok(scene) = state.scene.try_lock() else {
                    return;
                };
                state
                    .element_store
                    .entries
                    .get(&element_id)
                    .map(|entry| {
                        tze_hud_scene::element_store::fallback_geometry_for_element(
                            element_id, entry, &scene,
                        )
                    })
                    .unwrap_or(tze_hud_scene::ZERO_GEOMETRY_POLICY)
            };
            let store_snapshot = state.element_store.clone();
            let persist_path = state.element_store_path.clone();
            (previous, fallback, store_snapshot, persist_path)
        };

        // Persist the updated store on a background thread to avoid blocking the
        // Winit event loop with sync disk I/O (atomic write + fsync).
        if let Some(path) = persist_path {
            std::thread::spawn(move || {
                if let Err(e) =
                    crate::element_store::persist_element_store_to_path(&store_snapshot, &path)
                {
                    tracing::warn!(error = %e, "perform_reset_element_geometry: persist failed");
                }
            });
        }

        // Broadcast ElementRepositionedEvent.
        if let Some(ref tx) = self.state.element_repositioned_tx {
            let event = tze_hud_protocol::proto::ElementRepositionedEvent {
                // Use big-endian UUID bytes to match scene_id_to_bytes wire contract.
                element_id: element_id.as_uuid().as_bytes().to_vec(),
                new_geometry: Some(tze_hud_protocol::convert::geometry_policy_to_proto(
                    &fallback_geometry,
                )),
                previous_geometry: Some(tze_hud_protocol::convert::geometry_policy_to_proto(
                    &previous_override,
                )),
            };
            // Errors (no receivers, channel lagged) are silently ignored —
            // ElementRepositionedEvent is an ephemeral-realtime notification;
            // subscribers that are not present at the moment of a geometry reset
            // will learn the restored geometry on next subscription refresh.
            tx.send(event).unwrap_or_default();
            tracing::debug!(
                element_id = %element_id,
                "ElementRepositionedEvent broadcast after reset-to-default"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::graph::SceneGraph;

    // ─── resolve_composer_placeholder (hud-se6hs) ─────────────────────────

    /// Unset override (`None`, i.e. no `composer_placeholder` configured on
    /// the owning `HitRegionNode`) falls back to the global default —
    /// backward-compatible with every existing chat-composer caller.
    #[test]
    fn resolve_composer_placeholder_falls_back_to_default_when_unset() {
        assert_eq!(
            resolve_composer_placeholder(None),
            Some(COMPOSER_DEFAULT_PLACEHOLDER.to_string())
        );
    }

    /// A custom per-composer hint overrides the global default verbatim.
    #[test]
    fn resolve_composer_placeholder_uses_custom_override() {
        assert_eq!(
            resolve_composer_placeholder(Some("Search…".to_string())),
            Some("Search…".to_string())
        );
    }

    /// An explicit opt-out (`Some("")`) suppresses the placeholder entirely —
    /// distinct from the unset case above, which still shows the default.
    #[test]
    fn resolve_composer_placeholder_empty_string_opts_out() {
        assert_eq!(resolve_composer_placeholder(Some(String::new())), None);
    }

    // ─── composer_ime_caret_anchor (hud-hxhnt finding 3) ──────────────────

    /// With NO published layout the IME anchor falls back to the node interior
    /// (region origin + inset), NOT the window origin — the whole point of the
    /// fix (a candidate window must never float at (0,0)).
    #[test]
    fn ime_anchor_falls_back_to_node_interior_without_layout() {
        let a = composer_ime_caret_anchor(100.0, 200.0, 400.0, 40.0, 8.0, None, 3);
        assert_eq!(a.x, 108.0, "x = region_x + inset");
        assert_eq!(a.y, 232.0, "y = region_y + (region_h - inset)");
        assert!(a.height >= 1.0);
    }

    /// A single-line-style layout (glyph_x, no input_box) anchors the caret at
    /// the SHAPED x for the cursor byte, offset by the content inset.
    #[test]
    fn ime_anchor_uses_shaped_caret_x_single_line() {
        let layout = tze_hud_input::ComposerVisualLayout {
            lines: vec![tze_hud_input::ComposerVisualLine {
                start_byte: 0,
                end_byte: 3,
                // 'a','b','c' glyph boundaries + trailing width sentinel.
                glyph_x: vec![(0, 0.0), (1, 10.0), (2, 20.0), (3, 30.0)],
            }],
            text_len: 3,
            input_box: None,
            h_scroll_px: 0.0,
        };
        // Caret before byte 2 → shaped x 20.0; + inset 8 → 28; + region_x 100 → 128.
        let a = composer_ime_caret_anchor(100.0, 200.0, 400.0, 40.0, 8.0, Some(&layout), 2);
        assert_eq!(a.x, 128.0);
        assert_eq!(a.y, 208.0, "single-line caret top = region_y + inset");
    }

    /// A single-line layout with an active caret-follow horizontal scroll
    /// (`h_scroll_px`) must anchor the IME candidate window at the caret's
    /// ACTUAL rendered screen x — `x_at_cursor − h_scroll_px` — the same
    /// correction the chrome-layer caret quad applies
    /// (`append_composer_caret_vertices`'s `caret_x - h_scroll_px`). Before this
    /// fix the anchor used the raw unscrolled `x_at_cursor`, misplacing the
    /// candidate window for any horizontally-scrolled single-line composer.
    #[test]
    fn ime_anchor_corrects_for_single_line_h_scroll() {
        let layout = tze_hud_input::ComposerVisualLayout {
            lines: vec![tze_hud_input::ComposerVisualLine {
                start_byte: 0,
                end_byte: 3,
                glyph_x: vec![(0, 0.0), (1, 10.0), (2, 20.0), (3, 30.0)],
            }],
            text_len: 3,
            input_box: None,
            h_scroll_px: 15.0,
        };
        // Caret before byte 2 → shaped x 20.0; − scroll 15 → 5; + inset 8 → 13;
        // + region_x 100 → 113 (vs. 128 with no scroll correction).
        let a = composer_ime_caret_anchor(100.0, 200.0, 400.0, 40.0, 8.0, Some(&layout), 2);
        assert_eq!(a.x, 113.0);
    }

    /// A multi-line layout with input-box geometry anchors the caret at its
    /// VISUAL ROW (row0_top + row*line_height) so the candidate window tracks the
    /// correct wrapped line.
    #[test]
    fn ime_anchor_uses_visual_row_multi_line() {
        let line0 = tze_hud_input::ComposerVisualLine {
            start_byte: 0,
            end_byte: 3,
            glyph_x: vec![(0, 0.0), (1, 10.0), (2, 20.0), (3, 30.0)],
        };
        let line1 = tze_hud_input::ComposerVisualLine {
            start_byte: 3,
            end_byte: 6,
            glyph_x: vec![(3, 0.0), (4, 10.0), (5, 20.0), (6, 30.0)],
        };
        let layout = tze_hud_input::ComposerVisualLayout {
            lines: vec![line0, line1],
            text_len: 6,
            input_box: Some(tze_hud_input::ComposerInputBoxGeometry {
                box_top: 0.0,
                box_height: 40.0,
                row0_top: 4.0,
                line_height: 18.0,
                first_visible_row: 0,
                visible_rows: 2,
            }),
            h_scroll_px: 0.0,
        };
        // Cursor at byte 4 → row 1; y = region_y + row0_top + 1*line_height.
        let a = composer_ime_caret_anchor(100.0, 200.0, 400.0, 60.0, 8.0, Some(&layout), 4);
        assert_eq!(a.y, 200.0 + 4.0 + 18.0);
        assert_eq!(a.height, 18.0, "caret height = row line_height");
    }

    /// The caret x is clamped into the node interior so a long draft (shaped x
    /// past the region) cannot push the IME anchor outside the composer.
    #[test]
    fn ime_anchor_clamps_x_into_region() {
        let layout = tze_hud_input::ComposerVisualLayout {
            lines: vec![tze_hud_input::ComposerVisualLine {
                start_byte: 0,
                end_byte: 1,
                glyph_x: vec![(0, 0.0), (1, 9999.0)],
            }],
            text_len: 1,
            input_box: None,
            h_scroll_px: 0.0,
        };
        let a = composer_ime_caret_anchor(100.0, 200.0, 400.0, 40.0, 8.0, Some(&layout), 1);
        // region_w 400, inset 8 → max x = region_x + (400 - 8) = 492.
        assert_eq!(a.x, 492.0);
    }

    #[test]
    fn hover_tracker_region_resolves_from_widget_geometry() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).expect("create tab");

        scene
            .widget_registry
            .register_definition(tze_hud_scene::types::WidgetDefinition {
                id: "status-indicator".to_string(),
                name: "status-indicator".to_string(),
                description: "test".to_string(),
                parameter_schema: Vec::new(),
                layers: Vec::new(),
                default_geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 1.0,
                    height_pct: 1.0,
                },
                default_rendering_policy: tze_hud_scene::types::RenderingPolicy::default(),
                default_contention_policy: tze_hud_scene::types::ContentionPolicy::LatestWins,
                max_publishers: tze_hud_scene::types::WidgetDefinition::default_max_publishers(),
                ephemeral: false,
                hover_behavior: Some(tze_hud_scene::types::WidgetHoverBehavior {
                    trigger_rect: tze_hud_scene::types::WidgetNormalizedRect {
                        x_pct: 0.88,
                        y_pct: 0.06,
                        width_pct: 0.08,
                        height_pct: 0.22,
                    },
                    delay_ms: 3_000,
                    visibility_param: "tooltip_visible".to_string(),
                    hidden_value: 0.0,
                    visible_value: 1.0,
                }),
            });
        scene
            .widget_registry
            .register_instance(tze_hud_scene::types::WidgetInstance {
                id: tze_hud_scene::SceneId::new(),
                widget_type_name: "status-indicator".to_string(),
                tab_id,
                geometry_override: Some(tze_hud_scene::types::GeometryPolicy::Relative {
                    x_pct: 1660.0 / 1920.0,
                    y_pct: 8.0 / 1080.0,
                    width_pct: 252.0 / 1920.0,
                    height_pct: 96.0 / 1080.0,
                }),
                contention_override: None,
                instance_name: "main-status".to_string(),
                current_params: std::collections::HashMap::new(),
                auto_clear_ms: None,
            });

        let trackers =
            build_hover_trackers(&scene, 1920.0, 1080.0, &std::collections::HashMap::new());
        let region = trackers
            .get("main-status")
            .map(|t| t.region.clone())
            .expect("main-status hover tracker must resolve");
        assert!(
            region.x >= 1880.0 && region.x <= 1905.0,
            "x should sit near the icon trigger region, got {}",
            region.x
        );
        assert!(
            region.y >= 12.0 && region.y <= 18.0,
            "y should sit near the top trigger margin, got {}",
            region.y
        );
        assert!(
            region.width >= 19.0 && region.width <= 21.0,
            "width should match normalized trigger width"
        );
    }
}
