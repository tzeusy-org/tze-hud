//! Hit-region population methods for the compositor.
//!
//! Moved from `renderer/mod.rs` (the "Hit regions" cluster,
//! formerly ~L7346–7899 at plan date) by Step R-8 of the renderer module
//! split (hud-fgryk).  No logic was changed; only the `pub(super)` visibility
//! modifier was added to `populate_drag_handle_hit_regions_from` and
//! `collect_context_menu` so that callers in sibling modules can
//! access them.
//!
//! ## Methods in this file
//!
//! - `populate_drag_handle_hit_regions_from` — populate drag-handle hit regions
//!   from the `collect_drag_handle_entries` result a frame already holds.
//! - `collect_context_menu` — build vertices for the drag-handle reset
//!   context menu popup when one is showing.
//! - `populate_zone_hit_regions` — recompute zone interaction hit regions
//!   (dismiss and action buttons) for the current frame. Also recomputes the
//!   runtime "jump to latest" pill hit region for scrolled-back portal tiles
//!   (hud-9ci61) via `populate_jump_to_latest_hit_regions`.

use std::collections::HashMap;

use tze_hud_scene::DegradationLevel;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::graph::overlay::{NotificationControl, NotificationGeometry};
use tze_hud_scene::types::*;

use super::Compositor;
use super::draw_cmds::DragHandleEntry;
use super::token_colors::{
    notification_dismiss_bounds, notification_inset, resolve_jump_to_latest_tokens,
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
                    resize_grip: entry.resize_grip,
                });
        }

        prune_chrome_states(scene);
    }

    // ── Chrome context menu rendering (hud-zc7f) ─────────────────────────────

    /// Resolve the single-item menu once for this frame. Its one action rect is
    /// also its painted footprint and OS capture region; no invisible padding
    /// can activate a different surface. Text shares the compositor's font owner.
    pub(super) fn collect_context_menu(
        &mut self,
        scene: &mut SceneGraph,
        sw: f32,
        sh: f32,
    ) -> (Vec<RectVertex>, Vec<crate::text::TextItem>) {
        let Some(menu) = scene.overlay.drag_handle_context_menu.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        if !self
            .collect_drag_handle_entries(scene, sw, sh)
            .iter()
            .any(|h| h.element_id == menu.element_id)
        {
            scene.overlay.drag_handle_context_menu = None;
            scene.bump_geometry_epoch();
            return (Vec::new(), Vec::new());
        }
        let viewport = menu.viewport.unwrap_or(Rect::new(0.0, 0.0, sw, sh));
        let scale = menu
            .px_scale
            .unwrap_or_else(|| tze_hud_config::tokens::px_scale(&self.token_map));
        let font_px = self
            .token_map
            .get("system_card.body.font_size_px")
            .and_then(|s| s.trim_end_matches("px").parse::<f32>().ok())
            .filter(|n| n.is_finite() && *n > 0.0)
            .unwrap_or(14.0)
            * scale;
        let Some(tr) = self.text_rasterizer.as_mut() else {
            // A plate without its label is not an actionable menu.
            scene
                .overlay
                .drag_handle_context_menu
                .as_mut()
                .unwrap()
                .reset_button_rect = None;
            return (Vec::new(), Vec::new());
        };
        let label = "Reset to default";
        let label_w = tr.measure_single_line(label, font_px, FontFamily::SystemSansSerif, 400);
        let padding = 8.0 * scale;
        let w = (160.0 * scale)
            .max(label_w + padding * 2.0)
            .min(viewport.width.max(1.0));
        let h = (32.0 * scale)
            .max(font_px * crate::text::LINE_HEIGHT_MULTIPLIER + padding)
            .min(viewport.height.max(1.0));
        let rect = Rect::new(
            menu.anchor_x
                .clamp(viewport.x, viewport.x + (viewport.width - w).max(0.0)),
            menu.anchor_y
                .clamp(viewport.y, viewport.y + (viewport.height - h).max(0.0)),
            w,
            h,
        );
        let color = |key, fallback| {
            super::token_colors::resolve_token_color(&self.token_map, key).unwrap_or(fallback)
        };
        let background = color(
            "color.surface.container.highest",
            Rgba::new(0.165, 0.192, 0.239, 1.0),
        );
        let foreground = color("color.on_surface", Rgba::new(0.91, 0.922, 0.941, 1.0));
        let policy = RenderingPolicy {
            font_size_px: Some(font_px),
            font_weight: Some(400),
            text_color: Some(foreground),
            margin_px: Some(0.0),
            ..Default::default()
        };
        let text_h = font_px * crate::text::LINE_HEIGHT_MULTIPLIER;
        let mut text = crate::text::TextItem::from_zone_policy(
            label,
            rect.x + padding,
            rect.y + (rect.height - text_h) * 0.5,
            (w - padding * 2.0).max(1.0),
            text_h,
            &policy,
            1.0,
            1.0,
        );
        text.clip_pixel_x = rect.x;
        text.clip_pixel_y = rect.y;
        text.clip_bounds_width = rect.width;
        text.clip_bounds_height = rect.height;
        scene
            .overlay
            .drag_handle_context_menu
            .as_mut()
            .unwrap()
            .reset_button_rect = Some(rect);
        (
            rect_vertices(
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                sw,
                sh,
                self.gpu_color_raw(background.to_array()),
            )
            .to_vec(),
            vec![text],
        )
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
            &self.notification_layouts,
        );
        let now = self
            .last_animation_frame_at
            .unwrap_or_else(std::time::Instant::now);
        for card in &mut scene.overlay.notification_geometry {
            let zone = self.zone_animation_states.get(&card.zone_name);
            let publication = self
                .pub_animation_states
                .get(&card.zone_name)
                .and_then(|states| states.get(&card.publication_id));
            // The renderer bypasses both zone and publication fades at this
            // degradation level; retained animation state did not affect pixels.
            card.state = if self.degradation_policy.level >= DegradationLevel::Simplified {
                "shown"
            } else if publication.is_some_and(|s| s.fade_start.is_some())
                || zone.is_some_and(|s| s.target_opacity == 0.0)
            {
                "exiting"
            } else if zone.is_some_and(|s| !s.is_complete_at(now)) {
                "entering"
            } else {
                "shown"
            };
        }

        // ── Jump-to-latest pill hit region (hud-9ci61) ───────────────────────
        // Foreground drawing and hit targets share exposed pieces. A higher
        // card's body must cover both, including while drag changes its order.
        let indicator = resolve_scroll_indicator_tokens(&self.token_map);
        let tokens = resolve_jump_to_latest_tokens(&self.token_map);
        let regions: Vec<_> =
            Self::sort_tiles_with_drag_boost(self.policy_visible_tiles(scene), scene)
                .into_iter()
                .rev()
                .map(|tile| {
                    (
                        tile.id,
                        self.jump_to_latest_visible_regions(tile, scene, &tokens, &indicator),
                    )
                })
                .collect();
        for (tile_id, pieces) in regions {
            for bounds in &pieces {
                scene.overlay.zone_hit_regions.push(ZoneHitRegion {
                    publication_id: tze_hud_scene::SceneId::null(),
                    zone_name: "__chrome_jump_to_latest__".to_string(),
                    published_at_wall_us: 0,
                    publisher_namespace: "runtime".to_string(),
                    bounds: *bounds,
                    kind: ZoneInteractionKind::JumpToLatest { tile_id },
                    interaction_id: format!("jump-to-latest:{tile_id}"),
                    tab_order,
                });
            }
            if !pieces.is_empty() {
                tab_order += 1;
            }
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
                    publication_id: tze_hud_scene::SceneId::null(),
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
///   See `NotificationActionLayout::bounds` (shared with the renderer);
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
        &Default::default(),
    )
}

/// [`populate_notification_hit_regions`] with zones resolved through a
/// multi-display `layout` (regions land in scene coordinates).
fn populate_notification_hit_regions_in(
    scene: &mut SceneGraph,
    layout: &crate::display::DisplayLayout,
    sw: f32,
    sh: f32,
    token_map: &HashMap<String, String>,
    layouts: &std::cell::RefCell<super::notification_layout::NotificationLayoutCache>,
) -> u32 {
    layouts.borrow_mut().retain(scene);
    scene.prune_notification_feedback();
    scene.overlay.zone_hit_regions.clear();
    scene.overlay.notification_geometry.clear();
    let mut tab_order: u32 = 0;

    // Sort zone names for deterministic tab-order assignment across frames.
    // HashMap iteration order is nondeterministic; sorting ensures keyboard
    // focus cycling is stable when multiple interactive zones are present.
    let mut zone_names: Vec<String> = scene
        .shown_zone_publishes()
        .map(|(name, _)| name.clone())
        .collect();
    zone_names.sort_unstable();

    for zone_name in &zone_names {
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
        let zone_tokens = tze_hud_config::tokens::for_zone(token_map, zone_name);
        let scale = tze_hud_config::tokens::px_scale(&zone_tokens);
        let (zx, zy, zw, zh) = super::zone_geometry_in(
            layout,
            zone_name,
            &scene.effective_zone_geometry(zone_def),
            sw,
            sh,
        );
        let layout = Compositor::zone_slot_layout_with_tokens(
            &zone_tokens,
            zone_name,
            publishes,
            policy,
            zh,
            zw,
            layouts,
        );

        for (pub_idx, slot_y, effective_slot_h) in layout.iter_visible(zy) {
            let record = &publishes[pub_idx];
            let n_payload = match &record.content {
                ZoneContent::Notification(n) => n,
                _ => continue, // Only notifications get interactive affordances.
            };

            // ── Dismiss (×) button ────────────────────────────────────────
            // Top-right corner of the slot; same bounds the renderer draws.
            let dismiss_bounds = notification_dismiss_bounds(
                zx,
                slot_y,
                zw,
                effective_slot_h,
                scale,
                notification_inset(policy, scale),
            );
            let dismiss_id = notification_dismiss_interaction_id(zone_name, record);
            let action_rects = layouts
                .borrow_mut()
                .resolve(&zone_tokens, zone_name, record, policy, zw)
                .bounds(
                    zx,
                    slot_y,
                    zw,
                    effective_slot_h,
                    notification_inset(policy, scale),
                );
            let choice = scene
                .overlay
                .notification_choices
                .get(&record.publication_id);
            let chosen = (0..action_rects.len()).find(|&index| {
                choice
                    == Some(&notification_action_interaction_id(
                        zone_name, record, index,
                    ))
            });
            let focus = if scene.overlay.notification_focus.as_ref() == Some(&dismiss_id) {
                Some(NotificationControl::Close)
            } else {
                (0..action_rects.len())
                    .find(|&index| {
                        scene.overlay.notification_focus.as_ref()
                            == Some(&notification_action_interaction_id(
                                zone_name, record, index,
                            ))
                    })
                    .map(NotificationControl::Action)
            };
            scene
                .overlay
                .notification_geometry
                .push(NotificationGeometry {
                    zone_name: zone_name.clone(),
                    publication_id: record.publication_id,
                    bounds: Rect::new(zx, slot_y, zw, effective_slot_h),
                    actions: action_rects.clone(),
                    close: dismiss_bounds,
                    focus,
                    chosen,
                    state: "shown",
                });
            // Chosen cards still draw their finite exit, but take no input.
            if choice.is_some() {
                continue;
            }
            scene.overlay.zone_hit_regions.push(ZoneHitRegion {
                zone_name: zone_name.clone(),
                publication_id: record.publication_id,
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
                for (index, (_action, action_bounds)) in
                    n_payload.actions.iter().zip(action_rects).enumerate()
                {
                    let action_id = notification_action_interaction_id(zone_name, record, index);
                    scene.overlay.zone_hit_regions.push(ZoneHitRegion {
                        zone_name: zone_name.clone(),
                        publication_id: record.publication_id,
                        published_at_wall_us: record.published_at_wall_us,
                        publisher_namespace: record.publisher_namespace.clone(),
                        bounds: action_bounds,
                        kind: ZoneInteractionKind::Action { index },
                        interaction_id: action_id,
                        tab_order,
                    });
                    tab_order += 1;
                }
            }
        }
    }
    prune_chrome_states(scene);
    tab_order
}

/// Interaction id of a notification's dismiss (×) button; also the key of its
/// local hover/press state in `scene.overlay.chrome_states`.
pub(super) fn notification_dismiss_interaction_id(
    zone_name: &str,
    record: &ZonePublishRecord,
) -> String {
    format!(
        "zone:{}:dismiss:{}:{}",
        zone_name, record.publication_id, record.publisher_namespace,
    )
}

/// Interaction id of one notification action button; also the key of its
/// local hover/press state in `scene.overlay.chrome_states`.
pub(super) fn notification_action_interaction_id(
    zone_name: &str,
    record: &ZonePublishRecord,
    index: usize,
) -> String {
    format!(
        "zone:{}:action:{}:{}:{}",
        zone_name, record.publication_id, record.publisher_namespace, index,
    )
}

/// Drop local hover/press state for chrome controls whose hit region is gone
/// (a dismissed notification, a removed tile's drag handle), so stale
/// feedback never outlives its control.
fn prune_chrome_states(scene: &mut SceneGraph) {
    let overlay = &mut scene.overlay;
    overlay.chrome_states.retain(|k, _| {
        overlay
            .notification_choices
            .values()
            .any(|chosen| chosen == k)
            || overlay
                .drag_handle_hit_regions
                .iter()
                .any(|r| &r.interaction_id == k)
            || overlay
                .zone_hit_regions
                .iter()
                .any(|r| &r.interaction_id == k)
    });
}
