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
//! - `synthetic_widget_element_id` — derive a stable `SceneId` for a widget
//!   instance from its name.
//! - `scene_id_hex` — format a `SceneId` as a lowercase hex string.
//! - `drag_handle_bounds` — compute the drag-handle rect for an element given
//!   its bounds and the configured `DragHandleStyle`.
//! - `collect_drag_handle_entries` — build the full `Vec<DragHandleEntry>` for
//!   the current frame covering tiles, zones, and widgets.
//! - `drag_highlight_cmds` — SDF border commands for the active-drag highlight.
//! - `append_drag_handle_vertices` — emit GPU vertices for all drag handles.

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;

use super::Compositor;
use super::draw_cmds::DragHandleEntry;
use crate::pipeline::{RectVertex, RoundedRectDrawCmd, rect_vertices};

impl Compositor {
    fn resolve_widget_geometry(
        instance: &WidgetInstance,
        definition: &WidgetDefinition,
        sw: f32,
        sh: f32,
    ) -> (f32, f32, f32, f32) {
        let policy = instance
            .geometry_override
            .as_ref()
            .unwrap_or(&definition.default_geometry_policy);
        Self::resolve_zone_geometry(policy, sw, sh)
    }

    fn synthetic_widget_element_id(instance_name: &str) -> SceneId {
        let key = format!("widget:{instance_name}");
        let rid = ResourceId::of(key.as_bytes());
        SceneId::from_bytes_le(&rid.as_bytes()[..16]).unwrap_or(SceneId::null())
    }

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

    fn drag_handle_bounds(element_bounds: Rect, style: DragHandleStyle, sw: f32, sh: f32) -> Rect {
        let w = style.width_dp.max(1.0).min(sw.max(1.0));
        let h = style.height_dp.max(1.0).min(sh.max(1.0));
        let x = (element_bounds.x + (element_bounds.width - w) * 0.5).clamp(0.0, (sw - w).max(0.0));
        let y = (element_bounds.y - h * 0.5).clamp(0.0, (sh - h).max(0.0));
        Rect::new(x, y, w, h)
    }

    pub(super) fn collect_drag_handle_entries(
        &self,
        scene: &SceneGraph,
        sw: f32,
        sh: f32,
    ) -> Vec<DragHandleEntry> {
        let style = DragHandleStyle::default();
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
            .unwrap_or(tze_hud_scene::types::PORTAL_HEADER_DRAG_BAND_PX_DEFAULT);
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
                });
                continue;
            }
            let bounds = Self::drag_handle_bounds(tile.bounds, style, sw, sh);
            entries.push(DragHandleEntry {
                element_id: tile.id,
                element_kind: DragHandleElementKind::Tile,
                bounds,
                element_bounds: tile.bounds,
                interaction_id,
                style,
                is_header_band: false,
            });
        }

        let mut zone_names: Vec<_> = scene.zone_registry.active_publishes.keys().collect();
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
            let (x, y, w, h) = Self::resolve_zone_geometry(&zone_def.geometry_policy, sw, sh);
            let element_bounds_zone = Rect::new(x, y, w, h);
            let bounds = Self::drag_handle_bounds(element_bounds_zone, style, sw, sh);
            let interaction_id = format!("drag-handle:{}", Self::scene_id_hex(zone_def.id));
            entries.push(DragHandleEntry {
                element_id: zone_def.id,
                element_kind: DragHandleElementKind::Zone,
                bounds,
                element_bounds: element_bounds_zone,
                interaction_id,
                style,
                is_header_band: false,
            });
        }

        let active_tab = scene.active_tab;
        let mut widget_names: Vec<_> = scene.widget_registry.instances.keys().collect();
        widget_names.sort_unstable();
        for instance_name in widget_names {
            if scene
                .widget_registry
                .active_publishes
                .get(instance_name)
                .is_none_or(|p| p.is_empty())
            {
                continue;
            }
            let instance = match scene.widget_registry.instances.get(instance_name) {
                Some(i) => i,
                None => continue,
            };
            if let Some(tab_id) = active_tab
                && instance.tab_id != tab_id
            {
                continue;
            }
            let definition = match scene
                .widget_registry
                .definitions
                .get(&instance.widget_type_name)
            {
                Some(d) => d,
                None => continue,
            };
            let (x, y, w, h) = Self::resolve_widget_geometry(instance, definition, sw, sh);
            let element_bounds_widget = Rect::new(x, y, w, h);
            let bounds = Self::drag_handle_bounds(element_bounds_widget, style, sw, sh);
            let element_id = Self::synthetic_widget_element_id(instance_name);
            let interaction_id = format!("drag-handle:{}", Self::scene_id_hex(element_id));
            entries.push(DragHandleEntry {
                element_id,
                element_kind: DragHandleElementKind::Widget,
                bounds,
                element_bounds: element_bounds_widget,
                interaction_id,
                style,
                is_header_band: false,
            });
        }

        entries
    }

    /// The active-drag highlight: a `DRAG_HIGHLIGHT_BORDER_PX` inside border
    /// on the dragged element's bounds, drawn by the SDF pipeline just before
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
            .filter(|entry| scene.is_drag_active(entry.element_id))
            .map(|entry| {
                self.border_only_cmd(
                    entry.element_bounds,
                    0.0,
                    tze_hud_input::DRAG_HIGHLIGHT_BORDER_PX,
                    HIGHLIGHT,
                )
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
                .drag_handle_states
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
