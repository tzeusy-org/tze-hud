//! Hit-region population methods for the compositor.
//!
//! Moved from `renderer/mod.rs` (the "Hit regions" cluster,
//! formerly ~L7346–7899 at plan date) by Step R-8 of the renderer module
//! split (hud-fgryk).  No logic was changed; only the `pub(super)` visibility
//! modifier was added to `populate_drag_handle_hit_regions_from` and
//! `collect_context_menu_vertices` so that callers in sibling modules can
//! access them.
//!
//! ## Methods in this file
//!
//! - `populate_drag_handle_hit_regions_from` — populate drag-handle hit regions
//!   from the `collect_drag_handle_entries` result a frame already holds.
//! - `collect_context_menu_vertices` — build vertices for the drag-handle reset
//!   context menu popup when one is showing.
//! - `populate_zone_hit_regions` — recompute zone interaction hit regions
//!   (dismiss and action buttons) for the current frame. Also recomputes the
//!   runtime "jump to latest" pill hit region for scrolled-back portal tiles
//!   (hud-9ci61) via `populate_jump_to_latest_hit_regions`.

use std::collections::HashMap;

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;

use super::Compositor;
use super::draw_cmds::DragHandleEntry;
use super::token_colors::{
    notification_action_button_bounds, notification_dismiss_bounds, resolve_jump_to_latest_tokens,
    resolve_scroll_indicator_tokens, resolve_tile_close_tokens, tile_close_button_bounds,
};
use crate::pipeline::{RectVertex, rect_vertices};

impl Compositor {
    /// Populate drag-handle hit regions from a `collect_drag_handle_entries`
    /// result (called by `render_frame_headless` and the windowed frame path).
    pub(super) fn populate_drag_handle_hit_regions_from(
        &self,
        scene: &mut SceneGraph,
        handles: Vec<DragHandleEntry>,
    ) {
        scene.overlay.drag_handle_hit_regions.clear();
        for (tab_order, entry) in (0_u32..).zip(handles.into_iter()) {
            let hit_region = HitRegionNode {
                bounds: entry.bounds,
                interaction_id: entry.interaction_id.clone(),
                accepts_focus: false,
                accepts_pointer: true,
                auto_capture: false,
                ..Default::default()
            };
            scene
                .overlay
                .drag_handle_hit_regions
                .push(DragHandleHitRegion {
                    element_id: entry.element_id,
                    element_kind: entry.element_kind,
                    bounds: entry.bounds,
                    interaction_id: entry.interaction_id,
                    hit_region,
                    tab_order,
                    is_header_band: entry.is_header_band,
                });
        }

        scene.overlay.drag_handle_states.retain(|k, _| {
            scene
                .overlay
                .drag_handle_hit_regions
                .iter()
                .any(|r| &r.interaction_id == k)
        });
    }

    // ── Chrome context menu rendering (hud-zc7f) ─────────────────────────────

    /// Dimensions for the "Reset to default" context menu popup.
    const CONTEXT_MENU_W: f32 = 160.0;
    const CONTEXT_MENU_H: f32 = 32.0;
    const CONTEXT_MENU_PADDING: f32 = 4.0;

    /// Build vertices for the drag-handle reset context menu, if one is showing.
    ///
    /// Returns an empty `Vec` when `scene.overlay.drag_handle_context_menu` is `None`.
    /// The menu is rendered as two rects:
    /// - A semi-transparent dark background (the popup container).
    /// - A lighter "Reset to default" button inside it.
    pub(super) fn collect_context_menu_vertices(
        &self,
        scene: &SceneGraph,
        sw: f32,
        sh: f32,
    ) -> Vec<RectVertex> {
        let Some(ref menu) = scene.overlay.drag_handle_context_menu else {
            return Vec::new();
        };

        let mut vertices = Vec::with_capacity(12); // 2 quads × 6 vertices

        // Menu background.
        let bg_rgba = [0.12_f32, 0.12, 0.12, 0.92];
        let bg_color = self.gpu_color_raw(bg_rgba);
        let bg_verts = rect_vertices(
            menu.anchor_x,
            menu.anchor_y,
            Self::CONTEXT_MENU_W,
            Self::CONTEXT_MENU_H,
            sw,
            sh,
            bg_color,
        );
        vertices.extend_from_slice(&bg_verts);

        // "Reset to default" button.
        let btn_rgba = [0.22_f32, 0.22, 0.22, 0.95];
        let btn_color = self.gpu_color_raw(btn_rgba);
        let p = Self::CONTEXT_MENU_PADDING;
        let btn_verts = rect_vertices(
            menu.anchor_x + p,
            menu.anchor_y + p,
            Self::CONTEXT_MENU_W - p * 2.0,
            Self::CONTEXT_MENU_H - p * 2.0,
            sw,
            sh,
            btn_color,
        );
        vertices.extend_from_slice(&btn_verts);

        vertices
    }

    /// Recompute the zone interaction hit regions for the current frame:
    /// notification dismiss and action buttons (see
    /// [`populate_notification_hit_regions`]), then the runtime "jump to latest"
    /// pill for scrolled-back portal tiles.
    ///
    /// # Called by
    ///
    /// Called after a frame render completes to refresh `scene.overlay.zone_hit_regions`
    /// for the next frame's hit-testing.  This prepares `SceneGraph::hit_test` to
    /// return `ZoneInteraction` for zone affordances based on the most recently
    /// rendered layout.
    pub fn populate_zone_hit_regions(&self, scene: &mut SceneGraph, sw: f32, sh: f32) {
        let mut tab_order = populate_notification_hit_regions_in(
            scene,
            &self.display_layout,
            sw,
            sh,
            &self.token_map,
        );

        // ── Jump-to-latest pill hit region (hud-9ci61) ───────────────────────
        // Recomputed here (not only at render time) so windowed hit-testing,
        // which calls this method directly and independent of the render
        // cadence, always has a fresh region to test against. Geometry
        // mirrors `renderer/frame.rs`'s pill render block exactly — both call
        // the same pure `compute_jump_to_latest_pill` function — so a click
        // always lands where the pill is drawn.
        //
        // `zone_name` / `publisher_namespace` are not meaningful for a
        // tile-scoped runtime affordance; this reuses the sentinel-zone
        // convention already established for chrome drag handles
        // (`__chrome_drag_handle__` in `SceneGraph::hit_test`).
        let scroll_indicator_tokens = resolve_scroll_indicator_tokens(&self.token_map);
        let jump_to_latest_tokens = resolve_jump_to_latest_tokens(&self.token_map);
        // Order tiles FRONT-to-back by the same effective z-order the renderer
        // draws with — `sort_tiles_with_drag_boost` returns back-to-front (drag
        // boost included), so reversing yields top-most first. This matters
        // when two scrolled-back tiles overlap: `SceneGraph::hit_test` returns
        // the FIRST matching `zone_hit_regions` entry, and the renderer draws
        // the top-most tile's pill last (on top). Pushing top-most first makes
        // hit-test resolve to the pill actually drawn on top; the raw
        // `visible_tiles()` (back-to-front) order would instead let a lower-z
        // pill — possibly occluded by a higher-z tile body — steal the click
        // (hud-9ci61).
        //
        // Collect owned tile geometry first: the `&Tile`s borrow `scene`
        // immutably for their lifetime, which would otherwise conflict with the
        // `scene.overlay` mutation below.
        let mut ordered_tiles =
            Self::sort_tiles_with_drag_boost(self.policy_visible_tiles(scene), scene);
        ordered_tiles.reverse();
        let tile_geoms: Vec<(SceneId, Rect)> = ordered_tiles
            .into_iter()
            .map(|tile| (tile.id, tile.bounds))
            .collect();
        for (tile_id, bounds) in tile_geoms {
            let Some(scroll_cfg) = scene.tile_scroll_config(tile_id) else {
                continue;
            };
            let Some(content_height) = scroll_cfg.content_height else {
                continue;
            };
            let viewport_px = bounds.height;
            let (_, scroll_offset_y) = self.display_tile_scroll_offset(scene, tile_id);
            if tze_hud_input::compute_scroll_indicator(
                viewport_px,
                content_height,
                scroll_offset_y,
                &scroll_indicator_tokens,
            )
            .is_none()
            {
                continue; // No overflow — nothing to jump back from.
            }
            let scrolled_back = !scene.tile_follow_tail_at_tail(tile_id);
            let Some(pill) = tze_hud_input::compute_jump_to_latest_pill(
                bounds.width,
                viewport_px,
                scrolled_back,
                &jump_to_latest_tokens,
            ) else {
                continue;
            };
            let pill_bounds = Rect::new(
                bounds.x + pill.x_px,
                bounds.y + pill.y_px,
                pill.width_px,
                pill.height_px,
            );
            scene.overlay.zone_hit_regions.push(ZoneHitRegion {
                zone_name: "__chrome_jump_to_latest__".to_string(),
                published_at_wall_us: 0,
                publisher_namespace: "runtime".to_string(),
                bounds: pill_bounds,
                kind: ZoneInteractionKind::JumpToLatest { tile_id },
                interaction_id: format!("jump-to-latest:{tile_id}"),
                tab_order,
            });
            tab_order += 1;
        }

        // ── Viewer close button hit region (hud-jm8nq.11) ────────────────────
        // Only the hovered tile has a button, at the exact bounds the renderer
        // draws (`tile_close_button_bounds`). Pointer-up on it dismisses the
        // tile locally; the agent never sees the press.
        if let Some(tile) = self.tile_close_target(scene) {
            let tile_id = tile.id;
            let bounds =
                tile_close_button_bounds(tile.bounds, &resolve_tile_close_tokens(&self.token_map));
            scene.overlay.zone_hit_regions.insert(
                0,
                ZoneHitRegion {
                    zone_name: "__chrome_tile_close__".to_string(),
                    published_at_wall_us: 0,
                    publisher_namespace: "runtime".to_string(),
                    bounds,
                    kind: ZoneInteractionKind::DismissTile { tile_id },
                    interaction_id: format!("tile-close:{tile_id}"),
                    tab_order,
                },
            );
        }
    }
}

/// Recompute the notification hit regions for the current frame.
///
/// Clears `scene.overlay.zone_hit_regions` then repopulates it with dismiss (×)
/// buttons and action buttons for every visible notification slot in every
/// Stack zone that contains `ZoneContent::Notification` publications.
///
/// # Layout
///
/// For each notification slot (heights from `Compositor::zone_slot_layout_with_tokens`, the same layout the renderer uses):
///
/// - **Dismiss button**: a square in the top-right corner of the slot.
///   See `notification_dismiss_bounds`.
///
/// - **Action buttons**: a horizontal row at the bottom of the slot.
///   See `notification_action_button_bounds` (shared with the renderer);
///   `n_actions` is capped at `MAX_NOTIFICATION_ACTIONS`.
///
/// Tab order within a slot: dismiss button first, then actions left-to-right.
/// Slots are ordered top-to-bottom (slot 0 = newest, matching rendering order).
///
/// # Called by
///
/// [`Compositor::populate_zone_hit_regions`] after each rendered frame. The
/// geometry needs no GPU state, so GPU-free harnesses call it directly to make
/// notification buttons clickable through `SceneGraph::hit_test`.
///
/// Returns the next free tab-order index.
pub fn populate_notification_hit_regions(
    scene: &mut SceneGraph,
    sw: f32,
    sh: f32,
    token_map: &HashMap<String, String>,
) -> u32 {
    populate_notification_hit_regions_in(
        scene,
        &crate::display::DisplayLayout::default(),
        sw,
        sh,
        token_map,
    )
}

/// [`populate_notification_hit_regions`] with zones resolved through a
/// multi-display `layout` (regions land in scene coordinates).
pub fn populate_notification_hit_regions_in(
    scene: &mut SceneGraph,
    layout: &crate::display::DisplayLayout,
    sw: f32,
    sh: f32,
    token_map: &HashMap<String, String>,
) -> u32 {
    scene.overlay.zone_hit_regions.clear();
    let mut tab_order: u32 = 0;

    // Sort zone names for deterministic tab-order assignment across frames.
    // HashMap iteration order is nondeterministic; sorting ensures keyboard
    // focus cycling is stable when multiple interactive zones are present.
    let mut zone_names: Vec<_> = scene.zone_registry.active_publishes.keys().collect();
    zone_names.sort_unstable();

    for zone_name in zone_names {
        let publishes = match scene.zone_registry.active_publishes.get(zone_name) {
            Some(p) => p,
            None => continue,
        };
        if publishes.is_empty() {
            continue;
        }
        let zone_def = match scene.zone_registry.zones.get(zone_name) {
            Some(z) => z,
            None => continue,
        };
        // Only Stack zones with Notification content get interactive regions.
        if !matches!(zone_def.contention_policy, ContentionPolicy::Stack { .. }) {
            continue;
        }

        let policy = &zone_def.rendering_policy;
        let (zx, zy, zw, zh) =
            super::zone_geometry_in(layout, zone_name, &zone_def.geometry_policy, sw, sh);
        let layout =
            Compositor::zone_slot_layout_with_tokens(token_map, zone_name, publishes, policy, zh);

        for (pub_idx, slot_y, effective_slot_h) in layout.iter_visible(zy) {
            let record = &publishes[pub_idx];

            let n_payload = match &record.content {
                ZoneContent::Notification(n) => n,
                _ => continue, // Only notifications get interactive affordances.
            };

            // ── Dismiss (×) button ────────────────────────────────────────
            // Top-right corner of the slot; same bounds the renderer draws.
            let dismiss_bounds = notification_dismiss_bounds(zx, slot_y, zw, effective_slot_h);
            let dismiss_id = format!(
                "zone:{}:dismiss:{}:{}",
                zone_name, record.published_at_wall_us, record.publisher_namespace,
            );
            scene.overlay.zone_hit_regions.push(ZoneHitRegion {
                zone_name: zone_name.clone(),
                published_at_wall_us: record.published_at_wall_us,
                publisher_namespace: record.publisher_namespace.clone(),
                bounds: dismiss_bounds,
                kind: ZoneInteractionKind::Dismiss,
                interaction_id: dismiss_id,
                tab_order,
            });
            tab_order += 1;

            // ── Action buttons ────────────────────────────────────────────
            let n_actions = n_payload.actions.len().min(MAX_NOTIFICATION_ACTIONS);
            if n_actions > 0 {
                let action_rects =
                    notification_action_button_bounds(zx, slot_y, zw, effective_slot_h, n_actions);

                for (action, action_bounds) in n_payload.actions.iter().zip(action_rects) {
                    let action_id = format!(
                        "zone:{}:action:{}:{}:{}",
                        zone_name,
                        record.published_at_wall_us,
                        record.publisher_namespace,
                        action.callback_id,
                    );
                    scene.overlay.zone_hit_regions.push(ZoneHitRegion {
                        zone_name: zone_name.clone(),
                        published_at_wall_us: record.published_at_wall_us,
                        publisher_namespace: record.publisher_namespace.clone(),
                        bounds: action_bounds,
                        kind: ZoneInteractionKind::Action {
                            callback_id: action.callback_id.clone(),
                        },
                        interaction_id: action_id,
                        tab_order,
                    });
                    tab_order += 1;
                }
            }
        }
    }
    tab_order
}
