//! Widget and drag-handle geometry methods for the compositor.
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
//! - `drag_pill_cmds` — hover-only SDF move pills.
//! - `append_drag_handle_vertices` — pip resize dots.

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;

use super::Compositor;
use super::draw_cmds::DragHandleEntry;
use super::token_colors::{parse_hex_color, resolve_resize_grip_tokens};
use crate::pipeline::{RectVertex, RoundedRectDrawCmd, rect_vertices};
use tze_hud_config::tokens::{CANONICAL_TOKENS, DesignTokenMap, parse_numeric, px_scale};

/// Resolved component roles. Defaults live only in the canonical token table.
struct DragHandleStyle {
    width: f32,
    height: f32,
    hit_height: f32,
    gap: f32,
    resize_target: f32,
    radius: f32,
    color: Rgba,
    hover_opacity: f32,
    active_opacity: f32,
    highlight: Rgba,
    highlight_width: f32,
    highlight_radius: f32,
}

impl DragHandleStyle {
    fn resolve(tokens: &DesignTokenMap) -> Self {
        let canonical = |key| {
            CANONICAL_TOKENS
                .iter()
                .find(|t| t.key == key)
                .expect("drag role must be canonical")
                .default_value
        };
        let number = |key| {
            tokens
                .get(key)
                .and_then(|v| parse_numeric(v))
                .filter(|v| *v >= 0.0)
                .or_else(|| parse_numeric(canonical(key)))
                .expect("canonical drag number")
        };
        let color = |key| {
            tokens
                .get(key)
                .and_then(|v| parse_hex_color(v))
                .or_else(|| parse_hex_color(canonical(key)))
                .expect("canonical drag color")
        };
        let dp = |key| number(key) * px_scale(tokens);
        let mut highlight = color("color.primary");
        highlight.a *= number("opacity.drag.highlight").clamp(0.0, 1.0);
        Self {
            width: dp("size.drag.handle_width"),
            height: dp("size.drag.handle_height"),
            hit_height: dp("size.drag.hit_height"),
            gap: dp("space.s"),
            resize_target: dp("size.drag.resize_target"),
            radius: dp("shape.full"),
            color: color("color.on_surface.variant"),
            hover_opacity: number("opacity.drag.hover").clamp(0.0, 1.0),
            active_opacity: number("opacity.drag.active").clamp(0.0, 1.0),
            highlight,
            highlight_width: dp("stroke.border.width"),
            highlight_radius: dp("shape.l"),
        }
    }
}

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
    /// frame's top strip — not centered like the grip)..
    fn clamp_rect_to_display(r: Rect, sw: f32, sh: f32) -> Rect {
        let x = r.x.clamp(0.0, sw.max(0.0));
        let y = r.y.clamp(0.0, sh.max(0.0));
        let w = r.width.max(1.0).min((sw - x).max(1.0));
        let h = r.height.max(1.0).min((sh - y).max(1.0));
        Rect::new(x, y, w, h)
    }

    fn drag_handle_bounds(element_bounds: Rect, style: &DragHandleStyle, sw: f32, sh: f32) -> Rect {
        let w = style.width.max(1.0).min(sw.max(1.0));
        let h = style.hit_height.max(1.0).min(sh.max(1.0));
        let x = (element_bounds.x + (element_bounds.width - w) * 0.5).clamp(0.0, (sw - w).max(0.0));
        let y = (element_bounds.y - h - style.gap).clamp(0.0, (sh - h).max(0.0));
        Rect::new(x, y, w, h)
    }

    /// The pip resize grip: a token-sized square (at most half
    /// the element), inside the element's `(left, top)` corner.
    fn resize_grip_bounds(
        element_bounds: Rect,
        (left, top): (bool, bool),
        style: &DragHandleStyle,
    ) -> Rect {
        let side = style
            .resize_target
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

    pub(super) fn collect_drag_handle_entries(
        &self,
        scene: &SceneGraph,
        sw: f32,
        sh: f32,
    ) -> Vec<DragHandleEntry> {
        let style = DragHandleStyle::resolve(&self.token_map);
        let scale = tze_hud_config::tokens::px_scale(&self.token_map);
        let mut entries: Vec<DragHandleEntry> = Vec::new();

        // Portal frame tiles get a full-width HEADER BAND drag handle instead of
        // the small centered grip — the whole top strip drags like a Windows
        // titlebar. The band height is token-driven; the anchor set
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
                    is_header_band: true,
                    resize_grip: None,
                });
                continue;
            }
            let bounds = Self::drag_handle_bounds(tile.bounds, &style, sw, sh);
            entries.push(DragHandleEntry {
                element_id: tile.id,
                element_kind: DragHandleElementKind::Tile,
                bounds,
                element_bounds: tile.bounds,
                interaction_id,
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
            // A status-bar card's handle sits on the card it draws, not on the
            // maximum region the card may grow to.
            let zone_rect = Rect::new(x, y, w, h);
            let records = scene.zone_registry.active_publishes.get(zone_name);
            let card = records.and_then(|publishes| {
                self.status_bar_card(
                    zone_name,
                    publishes,
                    zone_def.contention_policy,
                    &zone_def.rendering_policy,
                    zone_rect,
                )
            });
            if card.is_none()
                && records
                    .and_then(|p| p.last())
                    .is_some_and(|r| matches!(r.content, ZoneContent::StatusBar(_)))
            {
                continue;
            }
            let element_bounds_zone = card.map_or(zone_rect, |c| c.rect);
            let element_bounds_zone = if zone_name == "subtitle" {
                let Some(ZoneContent::StreamText(text)) = scene
                    .zone_registry
                    .active_publishes
                    .get(zone_name)
                    .and_then(|records| records.last())
                    .map(|r| &r.content)
                else {
                    continue;
                };
                let Some(pill) =
                    self.subtitle_layout(zone_name, text, &zone_def.rendering_policy, zone_rect)
                else {
                    continue;
                };
                pill.rect
            } else {
                element_bounds_zone
            };
            let bounds = Self::drag_handle_bounds(element_bounds_zone, &style, sw, sh);
            let id_hex = Self::scene_id_hex(zone_def.id);
            entries.push(DragHandleEntry {
                element_id: zone_def.id,
                element_kind: DragHandleElementKind::Zone,
                bounds,
                element_bounds: element_bounds_zone,
                interaction_id: format!("drag-handle:{id_hex}"),
                is_header_band: false,
                resize_grip: None,
            });
            if zone_name.as_str() == PIP_ZONE {
                // The grip faces the display centre, but keeps the corner it
                // was pressed in while a drag is active: a pip grown across
                // the centre must not move its grip out from under the pointer.
                // A plain move freezes it too; intended, as the runtime pins
                // the grip's own corner from the press.
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
                    bounds: Self::resize_grip_bounds(element_bounds_zone, corner, &style),
                    element_bounds: element_bounds_zone,
                    interaction_id: format!("{RESIZE_GRIP_PREFIX}{id_hex}"),
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

    /// The active-drag highlight: a token-driven inside border
    /// on the dragged element (occupied slots for notification stacks), drawn just before
    /// the drag-handle pass (so above widgets). Per the drag-to-reposition
    /// spec: no drop shadows, scale pulses, or animated transitions.
    pub(super) fn drag_highlight_cmds(
        &self,
        scene: &SceneGraph,
        handles: &[DragHandleEntry],
    ) -> Vec<RoundedRectDrawCmd> {
        let style = DragHandleStyle::resolve(&self.token_map);
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
                            scene,
                            &zone.name,
                            publishes,
                            &zone.rendering_policy,
                            bounds.height,
                            bounds.width,
                        );
                        let (_, card) = slots.iter_visible(bounds).last()?;
                        bounds.height = card.y + card.height - bounds.y;
                    }
                }
                Some(
                    self.border_only_cmd(
                        bounds,
                        style
                            .highlight_radius
                            .min(bounds.width * 0.5)
                            .min(bounds.height * 0.5),
                        style.highlight_width,
                        style.highlight,
                    ),
                )
            })
            .collect()
    }

    /// A single SDF pill, shown by element hover or an active press/drag. The
    /// full portal band retains its input precedence but never paints a slab.
    pub(super) fn drag_pill_cmds(
        &self,
        scene: &SceneGraph,
        handles: &[DragHandleEntry],
    ) -> Vec<RoundedRectDrawCmd> {
        let style = DragHandleStyle::resolve(&self.token_map);
        handles
            .iter()
            .filter_map(|entry| {
                if entry.resize_grip.is_some() {
                    return None;
                }
                let local = scene
                    .overlay
                    .chrome_states
                    .get(&entry.interaction_id)
                    .copied()
                    .unwrap_or_default();
                let active = local.pressed || scene.is_drag_active(entry.element_id);
                if !active && !local.hovered && self.drag_handle_hover != Some(entry.element_id) {
                    return None;
                }
                let width = style.width.min(entry.bounds.width);
                let height = style.height.min(entry.bounds.height);
                let row = if entry.is_header_band {
                    style.hit_height.min(entry.bounds.height)
                } else {
                    entry.bounds.height
                };
                let mut color = style.color;
                color.a *= if active {
                    style.active_opacity
                } else {
                    style.hover_opacity
                };
                Some(RoundedRectDrawCmd {
                    x: entry.bounds.x + (entry.bounds.width - width) * 0.5,
                    y: entry.bounds.y + (row - height) * 0.5,
                    width,
                    height,
                    radius: style.radius.min(width * 0.5).min(height * 0.5),
                    color: self.gpu_color(color),
                    border: None,
                    clip: None,
                })
            })
            .collect()
    }

    /// The pip's existing resize dot mark is independent of the move pill.
    pub(super) fn append_drag_handle_vertices(
        &self,
        scene: &SceneGraph,
        handles: &[DragHandleEntry],
        vertices: &mut Vec<RectVertex>,
        sw: f32,
        sh: f32,
    ) {
        for entry in handles {
            let Some(corner) = entry.resize_grip else {
                continue;
            };
            let local = scene
                .overlay
                .chrome_states
                .get(&entry.interaction_id)
                .copied()
                .unwrap_or_default();
            let tokens = resolve_resize_grip_tokens(&self.token_map);
            let side = tokens
                .size_px
                .min(entry.bounds.width)
                .min(entry.bounds.height);
            let color = self.gpu_color_raw(tokens.mark_color(
                local.hovered || local.pressed || scene.is_drag_active(entry.element_id),
            ));
            for dot in Self::resize_grip_dot_rects(entry.bounds, side, corner) {
                if dot.width > 0.0 {
                    vertices.extend_from_slice(&rect_vertices(
                        dot.x, dot.y, dot.width, dot.height, sw, sh, color,
                    ));
                }
            }
        }
    }
}
