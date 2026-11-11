//! Widget and drag-handle geometry methods for the compositor.
//!
//! Moved from `renderer/mod.rs` (the "Widget/drag geometry" cluster,
//! formerly ~L7084–7345 at plan date) by Step R-8 of the renderer module
//! split (hud-fgryk).  No logic was changed; only the `pub(super)` visibility
//! modifier was added to `collect_drag_handle_entries` and
//! `append_drag_handle_vertices` so that `hit_regions.rs` can call them.
//!
//! ## Methods in this file
//!
//! - `resolve_widget_geometry` — convert a `WidgetInstance`/`WidgetDefinition`
//!   pair to pixel bounds using the instance's geometry policy (or the
//!   definition's default).
//! - `scene_id_hex` — format a `SceneId` as a lowercase hex string.
//! - `drag_handle_bounds` — compute the drag-handle rect for an element given
//!   its bounds and the configured `DragHandleStyle`.
//! - `collect_drag_handle_entries` — build the full `Vec<DragHandleEntry>` for
//!   the current frame covering tiles and zones (widgets get none).
//! - `drag_highlight_cmds` — SDF border commands for the active-drag highlight.
//! - `append_drag_handle_vertices` — emit GPU vertices for all drag handles.

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;

use super::Compositor;
use super::draw_cmds::DragHandleEntry;
use crate::pipeline::{RectVertex, RoundedRectDrawCmd, rect_vertices};

impl Compositor {
    fn scene_id_hex(id: SceneId) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(32);
        for b in id.to_bytes_le() {
            let _ = write!(&mut out, "{b:02x}");
        }
        out
    }

    /// Clamp a display-space rect so it stays fully on screen without changing
    /// its top-left anchoring (used for the header-band drag handle, which is the
    /// frame's top strip — not centered like the grip). hud-643dv.
    fn clamp_rect_to_display(r: Rect, sw: f32, sh: f32) -> Rect {
        let x = r.x.clamp(0.0, sw.max(0.0));
        let y = r.y.clamp(0.0, sh.max(0.0));
        let w = r.width.max(1.0).min((sw - x).max(1.0));
        let h = r.height.max(1.0).min((sh - y).max(1.0));
        Rect::new(x, y, w, h)
    }

    fn drag_handle_bounds(
        element_bounds: Rect,
        style: DragHandleStyle,
        scale: f32,
        sw: f32,
        sh: f32,
    ) -> Rect {
        let w = (style.width_dp * scale).max(1.0).min(sw.max(1.0));
        let h = (style.height_dp * scale).max(1.0).min(sh.max(1.0));
        let x = (element_bounds.x + (element_bounds.width - w) * 0.5).clamp(0.0, (sw - w).max(0.0));
        let y = (element_bounds.y - h * 0.5).clamp(0.0, (sh - h).max(0.0));
        Rect::new(x, y, w, h)
    }

    /// The pip resize grip: a square the move handle's width (at most half
    /// the element), inside the element's `(left, top)` corner.
    fn resize_grip_bounds(
        element_bounds: Rect,
        (left, top): (bool, bool),
        style: DragHandleStyle,
        scale: f32,
    ) -> Rect {
        let side = (style.width_dp * scale)
            .min(element_bounds.width * 0.5)
            .min(element_bounds.height * 0.5)
            .max(1.0);
        let x = if left {
            element_bounds.x
        } else {
            element_bounds.x + element_bounds.width - side
        };
        let y = if top {
            element_bounds.y
        } else {
            element_bounds.y + element_bounds.height - side
        };
        Rect::new(x, y, side, side)
    }

    /// The resize grip's glyph: six dots on a 3×3 grid of the grip square,
    /// forming a staircase that hugs its `(left, top)` corner (the classic
    /// window resize grip), each `(x, y, side)`. It always fits the square.
    pub(super) fn resize_grip_glyph(
        grip: Rect,
        (left, top): (bool, bool),
    ) -> impl Iterator<Item = (f32, f32, f32)> {
        let cell = grip.width.min(grip.height) / 3.0;
        let dot = (cell * 0.5).max(1.0).min(cell);
        let inset = (cell - dot) * 0.5;
        (0..3_u8)
            .flat_map(|row| (0..3_u8).map(move |col| (row, col)))
            // Toward the bottom-right corner: col + row >= 2. Mirror each axis
            // for a left or top corner.
            .filter(|(row, col)| row + col >= 2)
            .map(move |(row, col)| {
                let c = if left { 2 - col } else { col };
                let r = if top { 2 - row } else { row };
                (
                    grip.x + f32::from(c) * cell + inset,
                    grip.y + f32::from(r) * cell + inset,
                    dot,
                )
            })
    }

    pub(super) fn collect_drag_handle_entries(
        &self,
        scene: &SceneGraph,
        sw: f32,
        sh: f32,
    ) -> Vec<DragHandleEntry> {
        let style = DragHandleStyle::default();
        let scale = tze_hud_config::tokens::px_scale(&self.token_map);
        let mut entries: Vec<DragHandleEntry> = Vec::new();

        // Portal frame tiles get a full-width HEADER BAND drag handle instead of
        // the small centered grip — the whole top strip drags like a Windows
        // titlebar (hud-643dv). The band height is token-driven; the anchor set
        // comes from the shared structural portal-group resolution so only the
        // frame/anchor tile (not the panes) gets the band.
        let band_h = self
            .token_map
            .get(tze_hud_scene::types::PORTAL_HEADER_DRAG_BAND_TOKEN)
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(tze_hud_scene::types::PORTAL_HEADER_DRAG_BAND_PX_DEFAULT)
            * scale;
        let band_rects: std::collections::HashMap<SceneId, Rect> = scene
            .portal_header_band_anchors(band_h)
            .into_iter()
            .collect();

        for tile in self.policy_visible_tiles(scene) {
            let interaction_id = format!("drag-handle:{}", Self::scene_id_hex(tile.id));
            if let Some(band_rect) = band_rects.get(&tile.id) {
                // Header-band handle: clamp the frame-relative band rect to the
                // display like the grip does, but keep it full-width at the top
                // of the frame (no centering / top-edge straddle).
                let bounds = Self::clamp_rect_to_display(*band_rect, sw, sh);
                entries.push(DragHandleEntry {
                    element_id: tile.id,
                    element_kind: DragHandleElementKind::Tile,
                    bounds,
                    element_bounds: tile.bounds,
                    interaction_id,
                    style,
                    is_header_band: true,
                    resize_grip: None,
                });
                continue;
            }
            let bounds = Self::drag_handle_bounds(tile.bounds, style, scale, sw, sh);
            entries.push(DragHandleEntry {
                element_id: tile.id,
                element_kind: DragHandleElementKind::Tile,
                bounds,
                element_bounds: tile.bounds,
                interaction_id,
                style,
                is_header_band: false,
                resize_grip: None,
            });
        }

        let mut zone_names: Vec<_> = scene.shown_zone_publishes().map(|(name, _)| name).collect();
        zone_names.sort_unstable();
        for zone_name in zone_names {
            if scene
                .zone_registry
                .active_publishes
                .get(zone_name)
                .is_none_or(|p| p.is_empty())
            {
                continue;
            }
            let zone_def = match scene.zone_registry.zones.get(zone_name) {
                Some(z) => z,
                None => continue,
            };
            // Drag handles clamp to the primary display; zones placed on
            // another display are not draggable.
            if self.display_layout.zone_display_rect(zone_name).is_some() {
                continue;
            }
            let (x, y, w, h) =
                Self::resolve_zone_geometry(&scene.effective_zone_geometry(zone_def), sw, sh);
            let element_bounds_zone = Rect::new(x, y, w, h);
            let bounds = Self::drag_handle_bounds(element_bounds_zone, style, scale, sw, sh);
            let id_hex = Self::scene_id_hex(zone_def.id);
            entries.push(DragHandleEntry {
                element_id: zone_def.id,
                element_kind: DragHandleElementKind::Zone,
                bounds,
                element_bounds: element_bounds_zone,
                interaction_id: format!("drag-handle:{id_hex}"),
                style,
                is_header_band: false,
                resize_grip: None,
            });
            if zone_name.as_str() == PIP_ZONE {
                // The grip faces the display centre, but keeps the corner it
                // was pressed in while a drag is active: a pip grown across
                // the centre must not move its grip out from under the pointer.
                let corner = scene
                    .is_drag_active(zone_def.id)
                    .then(|| {
                        scene
                            .overlay
                            .drag_handle_hit_regions
                            .iter()
                            .filter(|r| r.element_id == zone_def.id)
                            .find_map(|r| r.resize_grip)
                    })
                    .flatten()
                    .unwrap_or_else(|| rect_corner_toward(element_bounds_zone, sw * 0.5, sh * 0.5));
                entries.push(DragHandleEntry {
                    element_id: zone_def.id,
                    element_kind: DragHandleElementKind::Zone,
                    bounds: Self::resize_grip_bounds(element_bounds_zone, corner, style, scale),
                    element_bounds: element_bounds_zone,
                    interaction_id: format!("{RESIZE_GRIP_PREFIX}{id_hex}"),
                    style,
                    is_header_band: false,
                    resize_grip: Some(corner),
                });
            }
        }

        // Widgets get no handle: widget geometry is set from the control
        // panel's Layout card, and a drawn handle that cannot drag is a dead
        // affordance.
        entries
    }

    /// The active-drag highlight: a `DRAG_HIGHLIGHT_BORDER_PX` inside border
    /// on the dragged element (occupied slots for notification stacks), drawn just before
    /// the drag-handle pass (so above widgets). Per the drag-to-reposition
    /// spec: no drop shadows, scale pulses, or animated transitions.
    pub(super) fn drag_highlight_cmds(
        &self,
        scene: &SceneGraph,
        handles: &[DragHandleEntry],
    ) -> Vec<RoundedRectDrawCmd> {
        // White at 0.9 alpha: visible on light and dark backgrounds.
        const HIGHLIGHT: Rgba = Rgba {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 0.9,
        };
        handles
            .iter()
            // The pip's grip shares its element with the move handle: one
            // border per element.
            .filter(|entry| entry.resize_grip.is_none() && scene.is_drag_active(entry.element_id))
            .filter_map(|entry| {
                let mut bounds = entry.element_bounds;
                if entry.element_kind == DragHandleElementKind::Zone {
                    let zone = scene.zone_by_id(entry.element_id)?;
                    let publishes = scene.zone_registry.active_publishes.get(&zone.name)?;
                    if matches!(zone.contention_policy, ContentionPolicy::Stack { .. })
                        && publishes
                            .iter()
                            .any(|p| matches!(p.content, ZoneContent::Notification(_)))
                    {
                        // Draw the occupied envelope from the very same admitted
                        // slots as the card/text/control passes, including gaps.
                        let slots = self.zone_slot_layout(
                            &zone.name,
                            publishes,
                            &zone.rendering_policy,
                            bounds.height,
                            bounds.width,
                        );
                        let (_, y, h) = slots.iter_visible(bounds.y).last()?;
                        bounds.height = y + h - bounds.y;
                    }
                }
                Some(self.border_only_cmd(
                    bounds,
                    0.0,
                    tze_hud_input::DRAG_HIGHLIGHT_BORDER_PX,
                    HIGHLIGHT,
                ))
            })
            .collect()
    }

    pub(super) fn append_drag_handle_vertices(
        &self,
        scene: &SceneGraph,
        handles: &[DragHandleEntry],
        vertices: &mut Vec<RectVertex>,
        sw: f32,
        sh: f32,
    ) {
        for entry in handles {
            let local_state = scene
                .overlay
                .chrome_states
                .get(&entry.interaction_id)
                .copied()
                .unwrap_or_default();
            let is_active_drag = scene.is_drag_active(entry.element_id);
            let opacity = if local_state.hovered || local_state.pressed || is_active_drag {
                entry.style.opacity_active
            } else {
                entry.style.opacity_idle
            }
            .clamp(0.0, 1.0);

            // Header-band handles are invisible (hud-643dv): the client draws the
            // header chrome, so the band paints no grip fill/glyph of its own — it
            // only contributes the active-drag highlight border
            // (`drag_highlight_cmds`). Painting a
            // translucent slab over the whole header would double-draw the client
            // header and look wrong.
            if entry.is_header_band {
                continue;
            }

            let mut base = entry.style.color;
            base.a = (base.a * opacity).clamp(0.0, 1.0);
            vertices.extend_from_slice(&rect_vertices(
                entry.bounds.x,
                entry.bounds.y,
                entry.bounds.width,
                entry.bounds.height,
                sw,
                sh,
                self.gpu_color(base),
            ));

            if let Some(corner) = entry.resize_grip {
                let grip = Rgba::new(base.r, base.g, base.b, (base.a * 0.9).clamp(0.0, 1.0));
                for (x, y, side) in Self::resize_grip_glyph(entry.bounds, corner) {
                    vertices.extend_from_slice(&rect_vertices(
                        x,
                        y,
                        side,
                        side,
                        sw,
                        sh,
                        self.gpu_color(grip),
                    ));
                }
                continue;
            }

            match entry.style.grip_pattern {
                DragHandleGripPattern::Dots => {
                    let dot = (entry.bounds.height * 0.35).max(1.0);
                    let gap = (dot * 0.6).max(1.0);
                    let total_w = dot * 3.0 + gap * 2.0;
                    let start_x = entry.bounds.x + (entry.bounds.width - total_w) * 0.5;
                    let y = entry.bounds.y + (entry.bounds.height - dot) * 0.5;
                    let grip = Rgba::new(base.r, base.g, base.b, (base.a * 0.9).clamp(0.0, 1.0));
                    for idx in 0..3 {
                        vertices.extend_from_slice(&rect_vertices(
                            start_x + idx as f32 * (dot + gap),
                            y,
                            dot,
                            dot,
                            sw,
                            sh,
                            self.gpu_color(grip),
                        ));
                    }
                }
                DragHandleGripPattern::Bar => {
                    let bar_w = (entry.bounds.width * 0.5).max(2.0);
                    let bar_h = (entry.bounds.height * 0.2).max(1.0);
                    let x = entry.bounds.x + (entry.bounds.width - bar_w) * 0.5;
                    let y = entry.bounds.y + (entry.bounds.height - bar_h) * 0.5;
                    let grip = Rgba::new(base.r, base.g, base.b, (base.a * 0.9).clamp(0.0, 1.0));
                    vertices.extend_from_slice(&rect_vertices(
                        x,
                        y,
                        bar_w,
                        bar_h,
                        sw,
                        sh,
                        self.gpu_color(grip),
                    ));
                }
                DragHandleGripPattern::None => {}
            }
        }
    }
}
