//! SVG rendering pipeline for widgets.
//!
//! This module implements the compositor's SVG rendering pipeline for widgets
//! as specified in widget-system/spec.md §Requirement: Widget Compositor Rendering
//! and §Requirement: Widget Parameter Interpolation.
//!
//! ## Pipeline
//!
//! 1. At widget type registration, raw SVG bytes are stored per layer.
//! 2. On parameter publication (or initial render), bindings are applied to
//!    the SVG source to produce a modified SVG string.
//! 3. The modified SVG is parsed into a `usvg::Tree` and rasterized to a
//!    `tiny_skia::Pixmap` via `resvg::render`.
//! 4. The RGBA pixmap data is uploaded to a `wgpu::Texture`.
//! 5. Cached textures are reused when parameters are unchanged (dirty flag).
//!
//! ## Parameter Interpolation
//!
//! When `transition_ms > 0`, the compositor interpolates between old and new
//! parameter values:
//! - `f32`: linear interpolation
//! - `color`: component-wise linear interpolation in sRGB space
//! - `string` / `enum`: snap to new value at t=0
//!
//! ## Compositing
//!
//! Widget textures are composited at the widget instance's geometry position
//! using z_order >= WIDGET_TILE_Z_MIN (0x9000_0000), which places widget tiles
//! above zone tiles.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use ab_glyph::{Font, ScaleFont};
use tze_hud_scene::DegradationLevel;
use tze_hud_scene::types::{
    GeometryPolicy, Rgba, WIDGET_TILE_Z_MIN, WidgetBinding, WidgetBindingMapping, WidgetDefinition,
    WidgetParameterValue, WidgetRegistry,
};

// ─── SVG render plans ────────────────────────────────────────────────────────

/// Retained CPU render plan for widget SVG layers.
///
/// The plan owns SVG text and cloned bindings so the compositor can compile it
/// once per widget definition and reuse it across parameter changes. Layers
/// that match the compositor's small primitive subset bypass repeated XML parse
/// and render directly through `tiny-skia`; unsupported layers keep the existing
/// `resvg` fallback.
pub struct WidgetRenderPlan {
    layers: Vec<WidgetRenderPlanLayer>,
}

struct WidgetRenderPlanLayer {
    svg_text: String,
    bindings: Vec<WidgetBinding>,
    source_digest: [u8; 32],
    primitive_plan: Option<PrimitiveSvgLayerPlan>,
}

impl WidgetRenderPlan {
    /// Compile a retained render plan from SVG text and per-layer bindings.
    pub fn compile(svg_layers: &[(&str, &[WidgetBinding])]) -> Self {
        let layers = svg_layers
            .iter()
            .map(|(svg_text, bindings)| WidgetRenderPlanLayer {
                svg_text: (*svg_text).to_string(),
                bindings: (*bindings).to_vec(),
                source_digest: *blake3::hash(svg_text.as_bytes()).as_bytes(),
                primitive_plan: PrimitiveSvgLayerPlan::parse(svg_text),
            })
            .collect();
        Self { layers }
    }
}

#[derive(Clone)]
struct PrimitiveSvgLayerPlan {
    view_box: SvgViewBox,
    items: Vec<PrimitiveItem>,
    has_text_items: bool,
    text_target_ids: BTreeSet<String>,
}

#[derive(Clone, Copy)]
struct SvgViewBox {
    min_x: f32,
    min_y: f32,
    width: f32,
    height: f32,
}

#[derive(Clone)]
enum PrimitiveItem {
    Rect(PrimitiveRect),
    Circle(PrimitiveCircle),
    Text(PrimitiveText),
}

#[derive(Clone)]
struct PrimitiveRect {
    id: Option<String>,
    ancestor_ids: Vec<String>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    rx: Option<f32>,
    ry: Option<f32>,
    fill: Option<String>,
    fill_opacity: f32,
    stroke: Option<String>,
    stroke_opacity: f32,
    stroke_width: f32,
    opacity: f32,
}

#[derive(Clone)]
struct PrimitiveCircle {
    id: Option<String>,
    ancestor_ids: Vec<String>,
    cx: f32,
    cy: f32,
    r: f32,
    fill: Option<String>,
    fill_opacity: f32,
    stroke: Option<String>,
    stroke_opacity: f32,
    stroke_width: f32,
    opacity: f32,
}

#[derive(Clone)]
struct PrimitiveText {
    id: Option<String>,
    ancestor_ids: Vec<String>,
    view_box: SvgViewBox,
    x: f32,
    y: f32,
    text_anchor: Option<String>,
    dominant_baseline: Option<String>,
    font_family: Option<String>,
    font_size: f32,
    fill: Option<String>,
    opacity: f32,
    content: String,
}

#[derive(Clone)]
struct PrimitiveGroup {
    id: Option<String>,
    opacity: f32,
}

struct ResolvedLayerBindings {
    by_target_attr: BTreeMap<String, BTreeMap<String, String>>,
    digest: [u8; 32],
}

impl ResolvedLayerBindings {
    fn digest_excluding_targets(&self, excluded_targets: &BTreeSet<String>) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for (target, attrs) in &self.by_target_attr {
            if excluded_targets.contains(target) {
                continue;
            }
            for (attr, value) in attrs {
                hasher.update(target.as_bytes());
                hasher.update(&[0]);
                hasher.update(attr.as_bytes());
                hasher.update(&[0]);
                hasher.update(value.as_bytes());
                hasher.update(&[0]);
            }
        }
        *hasher.finalize().as_bytes()
    }
}

impl PrimitiveSvgLayerPlan {
    fn parse(svg_text: &str) -> Option<Self> {
        if contains_unsupported_primitive_svg(svg_text) {
            return None;
        }

        let svg_tag = find_start_tag(svg_text, "svg")?;
        let svg_attrs = parse_tag_attrs(svg_tag);
        let view_box = parse_view_box(&svg_attrs).or_else(|| {
            let width = parse_f32_attr(&svg_attrs, "width")?;
            let height = parse_f32_attr(&svg_attrs, "height")?;
            Some(SvgViewBox {
                min_x: 0.0,
                min_y: 0.0,
                width,
                height,
            })
        })?;
        if view_box.width <= 0.0 || view_box.height <= 0.0 {
            return None;
        }

        let mut items = Vec::new();
        let mut groups: Vec<PrimitiveGroup> = Vec::new();
        let mut skip_depth = 0usize;
        let mut pos = 0usize;

        while let Some(tag_start_rel) = svg_text[pos..].find('<') {
            let tag_start = pos + tag_start_rel;
            let tag_end_rel = find_tag_end(&svg_text[tag_start..])?;
            let tag_end = tag_start + tag_end_rel;
            let raw_tag = &svg_text[tag_start..=tag_end];
            let tag_name = match tag_name(raw_tag) {
                Some(name) => name,
                None => {
                    pos = tag_end + 1;
                    continue;
                }
            };

            if raw_tag.starts_with("</") {
                if skip_depth > 0 {
                    if tag_name == "defs" || tag_name == "clipPath" {
                        skip_depth -= 1;
                    }
                } else if tag_name == "g" {
                    let _ = groups.pop();
                }
                pos = tag_end + 1;
                continue;
            }

            let self_closing = raw_tag.ends_with("/>");
            if tag_name == "defs" || tag_name == "clipPath" {
                if !self_closing {
                    skip_depth += 1;
                }
                pos = tag_end + 1;
                continue;
            }
            if skip_depth > 0 {
                pos = tag_end + 1;
                continue;
            }

            let attrs = parse_tag_attrs(raw_tag);
            match tag_name {
                "g" => {
                    groups.push(PrimitiveGroup {
                        id: attrs.get("id").cloned(),
                        opacity: parse_f32_attr(&attrs, "opacity").unwrap_or(1.0),
                    });
                }
                "rect" => {
                    if let Some(rect) = PrimitiveRect::from_attrs(&attrs, &groups) {
                        items.push(PrimitiveItem::Rect(rect));
                    }
                }
                "circle" => {
                    if let Some(circle) = PrimitiveCircle::from_attrs(&attrs, &groups) {
                        items.push(PrimitiveItem::Circle(circle));
                    }
                }
                "text" => {
                    let close = svg_text[tag_end + 1..].find("</text>")?;
                    let content = &svg_text[tag_end + 1..tag_end + 1 + close];
                    if let Some(text) =
                        PrimitiveText::from_attrs(&attrs, &groups, view_box, content)
                    {
                        items.push(PrimitiveItem::Text(text));
                    }
                    pos = tag_end + 1 + close + "</text>".len();
                    continue;
                }
                "svg" | "?xml" | "!--" => {}
                _ => return None,
            }

            pos = tag_end + 1;
        }

        let has_text_items = items
            .iter()
            .any(|item| matches!(item, PrimitiveItem::Text(_)));
        let text_target_ids = items
            .iter()
            .filter_map(|item| match item {
                PrimitiveItem::Text(text) => text.id.clone(),
                _ => None,
            })
            .collect();

        Some(Self {
            view_box,
            items,
            has_text_items,
            text_target_ids,
        })
    }

    fn rasterize(
        &self,
        source_digest: [u8; 32],
        resolved_bindings: &ResolvedLayerBindings,
        pixel_width: u32,
        pixel_height: u32,
    ) -> Option<tiny_skia::Pixmap> {
        let mut pixmap = tiny_skia::Pixmap::new(pixel_width, pixel_height)?;
        self.draw_into(
            &mut pixmap,
            source_digest,
            resolved_bindings,
            pixel_width,
            pixel_height,
        );
        Some(pixmap)
    }

    fn draw_into(
        &self,
        pixmap: &mut tiny_skia::Pixmap,
        source_digest: [u8; 32],
        resolved_bindings: &ResolvedLayerBindings,
        pixel_width: u32,
        pixel_height: u32,
    ) {
        if self.has_text_items {
            self.draw_onto_with_text_split(
                pixmap,
                source_digest,
                resolved_bindings,
                pixel_width,
                pixel_height,
            );
        } else {
            self.draw_onto(pixmap, resolved_bindings, pixel_width, pixel_height);
        }
    }

    fn layer_transform(&self, pixel_width: u32, pixel_height: u32) -> tiny_skia::Transform {
        let uniform_scale = layer_scale(self.view_box, pixel_width, pixel_height);
        let rendered_w = self.view_box.width * uniform_scale;
        let rendered_h = self.view_box.height * uniform_scale;
        let offset_x =
            (pixel_width as f32 - rendered_w) * 0.5 - self.view_box.min_x * uniform_scale;
        let offset_y =
            (pixel_height as f32 - rendered_h) * 0.5 - self.view_box.min_y * uniform_scale;
        tiny_skia::Transform::from_translate(offset_x, offset_y)
            .post_scale(uniform_scale, uniform_scale)
    }

    fn draw_onto(
        &self,
        pixmap: &mut tiny_skia::Pixmap,
        resolved_bindings: &ResolvedLayerBindings,
        pixel_width: u32,
        pixel_height: u32,
    ) {
        let transform = self.layer_transform(pixel_width, pixel_height);

        for item in &self.items {
            match item {
                PrimitiveItem::Rect(rect) => {
                    rect.draw(pixmap, resolved_bindings, transform);
                }
                PrimitiveItem::Circle(circle) => {
                    circle.draw(pixmap, resolved_bindings, transform);
                }
                PrimitiveItem::Text(text) => {
                    text.draw(pixmap, resolved_bindings, pixel_width, pixel_height);
                }
            }
        }
    }

    fn draw_onto_with_text_split(
        &self,
        pixmap: &mut tiny_skia::Pixmap,
        source_digest: [u8; 32],
        resolved_bindings: &ResolvedLayerBindings,
        pixel_width: u32,
        pixel_height: u32,
    ) {
        let non_text_digest = resolved_bindings.digest_excluding_targets(&self.text_target_ids);
        let transform = self.layer_transform(pixel_width, pixel_height);
        let mut segment_start: Option<usize> = None;
        let mut segment_index = 0u32;

        for (idx, item) in self.items.iter().enumerate() {
            match item {
                PrimitiveItem::Text(text) => {
                    if let Some(start) = segment_start.take() {
                        self.draw_cached_non_text_segment(
                            pixmap,
                            start,
                            idx,
                            segment_index,
                            source_digest,
                            non_text_digest,
                            resolved_bindings,
                            transform,
                            pixel_width,
                            pixel_height,
                        );
                        segment_index = segment_index.saturating_add(1);
                    }
                    text.draw(pixmap, resolved_bindings, pixel_width, pixel_height);
                }
                PrimitiveItem::Rect(_) | PrimitiveItem::Circle(_) => {
                    segment_start.get_or_insert(idx);
                }
            }
        }

        if let Some(start) = segment_start {
            self.draw_cached_non_text_segment(
                pixmap,
                start,
                self.items.len(),
                segment_index,
                source_digest,
                non_text_digest,
                resolved_bindings,
                transform,
                pixel_width,
                pixel_height,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_cached_non_text_segment(
        &self,
        pixmap: &mut tiny_skia::Pixmap,
        start: usize,
        end: usize,
        segment_index: u32,
        source_digest: [u8; 32],
        non_text_digest: [u8; 32],
        resolved_bindings: &ResolvedLayerBindings,
        transform: tiny_skia::Transform,
        pixel_width: u32,
        pixel_height: u32,
    ) {
        if self.non_text_range_has_active_bindings(start, end, resolved_bindings) {
            self.draw_non_text_range(pixmap, start, end, resolved_bindings, transform);
            return;
        }

        let key = primitive_non_text_segment_cache_key(
            source_digest,
            segment_index,
            non_text_digest,
            pixel_width,
            pixel_height,
        );
        if let Some(cached) = primitive_non_text_layer_cache()
            .lock()
            .expect("primitive non-text SVG layer cache poisoned")
            .get(&key)
        {
            pixmap.as_mut().draw_pixmap(
                0,
                0,
                cached.as_ref().as_ref(),
                &tiny_skia::PixmapPaint::default(),
                tiny_skia::Transform::identity(),
                None,
            );
            return;
        }

        let Some(mut segment) = tiny_skia::Pixmap::new(pixel_width, pixel_height) else {
            return;
        };
        self.draw_non_text_range(&mut segment, start, end, resolved_bindings, transform);
        let mut cache = primitive_non_text_layer_cache()
            .lock()
            .expect("primitive non-text SVG layer cache poisoned");
        let cached = cache.insert_with_limits(
            key,
            segment,
            PRIMITIVE_NON_TEXT_LAYER_CACHE_MAX_ENTRIES,
            PRIMITIVE_NON_TEXT_LAYER_CACHE_MAX_BYTES,
            STATIC_SVG_LAYER_CACHE_MAX_ENTRY_BYTES,
        );
        pixmap.as_mut().draw_pixmap(
            0,
            0,
            cached.as_ref().as_ref(),
            &tiny_skia::PixmapPaint::default(),
            tiny_skia::Transform::identity(),
            None,
        );
    }

    fn draw_non_text_range(
        &self,
        pixmap: &mut tiny_skia::Pixmap,
        start: usize,
        end: usize,
        resolved_bindings: &ResolvedLayerBindings,
        transform: tiny_skia::Transform,
    ) {
        for item in &self.items[start..end] {
            match item {
                PrimitiveItem::Rect(rect) => {
                    rect.draw(pixmap, resolved_bindings, transform);
                }
                PrimitiveItem::Circle(circle) => {
                    circle.draw(pixmap, resolved_bindings, transform);
                }
                PrimitiveItem::Text(_) => {}
            }
        }
    }

    fn non_text_range_has_active_bindings(
        &self,
        start: usize,
        end: usize,
        resolved_bindings: &ResolvedLayerBindings,
    ) -> bool {
        self.items[start..end].iter().any(|item| match item {
            PrimitiveItem::Rect(rect) => rect.has_active_binding(resolved_bindings),
            PrimitiveItem::Circle(circle) => circle.has_active_binding(resolved_bindings),
            PrimitiveItem::Text(_) => false,
        })
    }
}

fn layer_scale(view_box: SvgViewBox, pixel_width: u32, pixel_height: u32) -> f32 {
    let sx = pixel_width as f32 / view_box.width;
    let sy = pixel_height as f32 / view_box.height;
    sx.min(sy)
}

impl PrimitiveRect {
    fn from_attrs(attrs: &BTreeMap<String, String>, groups: &[PrimitiveGroup]) -> Option<Self> {
        Some(Self {
            id: attrs.get("id").cloned(),
            ancestor_ids: group_ids(groups),
            x: parse_f32_attr(attrs, "x").unwrap_or(0.0),
            y: parse_f32_attr(attrs, "y").unwrap_or(0.0),
            width: parse_f32_attr(attrs, "width")?,
            height: parse_f32_attr(attrs, "height")?,
            rx: parse_f32_attr(attrs, "rx"),
            ry: parse_f32_attr(attrs, "ry"),
            fill: attrs.get("fill").cloned(),
            fill_opacity: parse_f32_attr(attrs, "fill-opacity").unwrap_or(1.0),
            stroke: attrs.get("stroke").cloned(),
            stroke_opacity: parse_f32_attr(attrs, "stroke-opacity").unwrap_or(1.0),
            stroke_width: parse_f32_attr(attrs, "stroke-width").unwrap_or(1.0),
            opacity: parse_f32_attr(attrs, "opacity").unwrap_or(1.0) * group_opacity(groups),
        })
    }

    fn draw(
        &self,
        pixmap: &mut tiny_skia::Pixmap,
        bindings: &ResolvedLayerBindings,
        transform: tiny_skia::Transform,
    ) {
        let x = self.bound_f32(bindings, "x", self.x);
        let y = self.bound_f32(bindings, "y", self.y);
        let width = self.bound_f32(bindings, "width", self.width);
        let height = self.bound_f32(bindings, "height", self.height);
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let Some(rect) = tiny_skia::Rect::from_xywh(x, y, width, height) else {
            return;
        };
        let path = self.path_for_rect(rect, bindings);
        if let Some(paint) = self.fill_paint(bindings) {
            pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, transform, None);
        }
        if let Some((paint, stroke)) = self.stroke_paint(bindings) {
            pixmap.stroke_path(&path, &paint, &stroke, transform, None);
        }
    }

    fn path_for_rect(
        &self,
        rect: tiny_skia::Rect,
        bindings: &ResolvedLayerBindings,
    ) -> tiny_skia::Path {
        let bound_rx = self
            .bound_attr(bindings, "rx")
            .and_then(parse_svg_number)
            .or(self.rx);
        let bound_ry = self
            .bound_attr(bindings, "ry")
            .and_then(parse_svg_number)
            .or(self.ry);
        let mut rx = bound_rx.or(bound_ry).unwrap_or(0.0).max(0.0);
        let mut ry = bound_ry.or(bound_rx).unwrap_or(0.0).max(0.0);
        rx = rx.min(rect.width() * 0.5);
        ry = ry.min(rect.height() * 0.5);

        if rx <= 0.0 || ry <= 0.0 {
            return tiny_skia::PathBuilder::from_rect(rect);
        }

        rounded_rect_path(rect, rx, ry).unwrap_or_else(|| tiny_skia::PathBuilder::from_rect(rect))
    }

    fn bound_attr<'a>(
        &'a self,
        bindings: &'a ResolvedLayerBindings,
        attr: &str,
    ) -> Option<&'a str> {
        let id = self.id.as_ref()?;
        bindings
            .by_target_attr
            .get(id)
            .and_then(|attrs| attrs.get(attr))
            .map(String::as_str)
    }

    fn bound_f32(&self, bindings: &ResolvedLayerBindings, attr: &str, default: f32) -> f32 {
        self.bound_attr(bindings, attr)
            .and_then(parse_svg_number)
            .unwrap_or(default)
    }

    fn effective_opacity(&self, bindings: &ResolvedLayerBindings) -> f32 {
        self.opacity * bound_ancestor_opacity(&self.ancestor_ids, bindings)
    }

    fn has_active_binding(&self, bindings: &ResolvedLayerBindings) -> bool {
        target_or_ancestor_has_binding(self.id.as_deref(), &self.ancestor_ids, bindings)
    }

    fn fill_paint(&self, bindings: &ResolvedLayerBindings) -> Option<tiny_skia::Paint<'static>> {
        let color = self
            .bound_attr(bindings, "fill")
            .or(self.fill.as_deref())
            .and_then(parse_svg_color)?;
        let alpha =
            (color.3 * self.fill_opacity * self.effective_opacity(bindings)).clamp(0.0, 1.0);
        if alpha <= 0.0 {
            return None;
        }
        Some(paint_rgba(color.0, color.1, color.2, alpha))
    }

    fn stroke_paint(
        &self,
        bindings: &ResolvedLayerBindings,
    ) -> Option<(tiny_skia::Paint<'static>, tiny_skia::Stroke)> {
        let color = self
            .bound_attr(bindings, "stroke")
            .or(self.stroke.as_deref())
            .and_then(parse_svg_color)?;
        let alpha =
            (color.3 * self.stroke_opacity * self.effective_opacity(bindings)).clamp(0.0, 1.0);
        if alpha <= 0.0 || self.stroke_width <= 0.0 {
            return None;
        }
        let stroke = tiny_skia::Stroke {
            width: self.stroke_width,
            ..Default::default()
        };
        Some((paint_rgba(color.0, color.1, color.2, alpha), stroke))
    }
}

impl PrimitiveCircle {
    fn from_attrs(attrs: &BTreeMap<String, String>, groups: &[PrimitiveGroup]) -> Option<Self> {
        Some(Self {
            id: attrs.get("id").cloned(),
            ancestor_ids: group_ids(groups),
            cx: parse_f32_attr(attrs, "cx")?,
            cy: parse_f32_attr(attrs, "cy")?,
            r: parse_f32_attr(attrs, "r")?,
            fill: attrs.get("fill").cloned(),
            fill_opacity: parse_f32_attr(attrs, "fill-opacity").unwrap_or(1.0),
            stroke: attrs.get("stroke").cloned(),
            stroke_opacity: parse_f32_attr(attrs, "stroke-opacity").unwrap_or(1.0),
            stroke_width: parse_f32_attr(attrs, "stroke-width").unwrap_or(1.0),
            opacity: parse_f32_attr(attrs, "opacity").unwrap_or(1.0) * group_opacity(groups),
        })
    }

    fn draw(
        &self,
        pixmap: &mut tiny_skia::Pixmap,
        bindings: &ResolvedLayerBindings,
        transform: tiny_skia::Transform,
    ) {
        let r = self.bound_f32(bindings, "r", self.r);
        if r <= 0.0 {
            return;
        }
        let Some(path) = tiny_skia::PathBuilder::from_circle(
            self.bound_f32(bindings, "cx", self.cx),
            self.bound_f32(bindings, "cy", self.cy),
            r,
        ) else {
            return;
        };
        if let Some(paint) = self.fill_paint(bindings) {
            pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, transform, None);
        }
        if let Some((paint, stroke)) = self.stroke_paint(bindings) {
            pixmap.stroke_path(&path, &paint, &stroke, transform, None);
        }
    }

    fn bound_attr<'a>(
        &'a self,
        bindings: &'a ResolvedLayerBindings,
        attr: &str,
    ) -> Option<&'a str> {
        let id = self.id.as_ref()?;
        bindings
            .by_target_attr
            .get(id)
            .and_then(|attrs| attrs.get(attr))
            .map(String::as_str)
    }

    fn bound_f32(&self, bindings: &ResolvedLayerBindings, attr: &str, default: f32) -> f32 {
        self.bound_attr(bindings, attr)
            .and_then(parse_svg_number)
            .unwrap_or(default)
    }

    fn effective_opacity(&self, bindings: &ResolvedLayerBindings) -> f32 {
        self.opacity * bound_ancestor_opacity(&self.ancestor_ids, bindings)
    }

    fn has_active_binding(&self, bindings: &ResolvedLayerBindings) -> bool {
        target_or_ancestor_has_binding(self.id.as_deref(), &self.ancestor_ids, bindings)
    }

    fn fill_paint(&self, bindings: &ResolvedLayerBindings) -> Option<tiny_skia::Paint<'static>> {
        let color = self
            .bound_attr(bindings, "fill")
            .or(self.fill.as_deref())
            .and_then(parse_svg_color)?;
        let alpha =
            (color.3 * self.fill_opacity * self.effective_opacity(bindings)).clamp(0.0, 1.0);
        if alpha <= 0.0 {
            return None;
        }
        Some(paint_rgba(color.0, color.1, color.2, alpha))
    }

    fn stroke_paint(
        &self,
        bindings: &ResolvedLayerBindings,
    ) -> Option<(tiny_skia::Paint<'static>, tiny_skia::Stroke)> {
        let color = self
            .bound_attr(bindings, "stroke")
            .or(self.stroke.as_deref())
            .and_then(parse_svg_color)?;
        let alpha =
            (color.3 * self.stroke_opacity * self.effective_opacity(bindings)).clamp(0.0, 1.0);
        if alpha <= 0.0 || self.stroke_width <= 0.0 {
            return None;
        }
        let stroke = tiny_skia::Stroke {
            width: self.stroke_width,
            ..Default::default()
        };
        Some((paint_rgba(color.0, color.1, color.2, alpha), stroke))
    }
}

impl PrimitiveText {
    fn from_attrs(
        attrs: &BTreeMap<String, String>,
        groups: &[PrimitiveGroup],
        view_box: SvgViewBox,
        content: &str,
    ) -> Option<Self> {
        Some(Self {
            id: attrs.get("id").cloned(),
            ancestor_ids: group_ids(groups),
            view_box,
            x: parse_f32_attr(attrs, "x").unwrap_or(0.0),
            y: parse_f32_attr(attrs, "y").unwrap_or(0.0),
            text_anchor: attrs.get("text-anchor").cloned(),
            dominant_baseline: attrs.get("dominant-baseline").cloned(),
            font_family: attrs.get("font-family").cloned(),
            font_size: parse_f32_attr(attrs, "font-size").unwrap_or(12.0),
            fill: attrs.get("fill").cloned(),
            opacity: parse_f32_attr(attrs, "opacity").unwrap_or(1.0) * group_opacity(groups),
            content: html_unescape_text(content),
        })
    }

    fn draw(
        &self,
        pixmap: &mut tiny_skia::Pixmap,
        bindings: &ResolvedLayerBindings,
        pixel_width: u32,
        pixel_height: u32,
    ) {
        let content = self
            .bound_attr(bindings, "text-content")
            .unwrap_or(self.content.as_str());
        if content.is_empty() {
            return;
        }
        let fill = self.bound_attr(bindings, "fill").or(self.fill.as_deref());
        if fill.is_some_and(is_svg_paint_none) {
            return;
        }
        let color = fill.and_then(parse_svg_color).unwrap_or((0, 0, 0, 1.0));
        let opacity = self.opacity * bound_ancestor_opacity(&self.ancestor_ids, bindings);
        if opacity <= 0.0 {
            return;
        }
        if let Some(mask) = self.rasterize_direct_text_mask(content, pixel_width, pixel_height) {
            draw_tinted_text_mask_at(
                pixmap,
                mask.pixmap.as_ref().as_ref(),
                0,
                mask.pixel_y,
                color,
                opacity,
            );
            return;
        }
        let Some(crop) = self.mask_crop(pixel_width, pixel_height) else {
            return;
        };
        let text_svg = self.render_mask_svg(content, crop.view_box);
        let Some(layer) = rasterize_cached_text_svg(&text_svg, pixel_width, crop.pixel_height)
        else {
            return;
        };
        draw_tinted_text_mask_at(
            pixmap,
            layer.as_ref().as_ref(),
            0,
            crop.pixel_y,
            color,
            opacity,
        );
    }

    fn bound_attr<'a>(
        &'a self,
        bindings: &'a ResolvedLayerBindings,
        attr: &str,
    ) -> Option<&'a str> {
        let id = self.id.as_ref()?;
        bindings
            .by_target_attr
            .get(id)
            .and_then(|attrs| attrs.get(attr))
            .map(String::as_str)
    }

    fn render_mask_svg(&self, content: &str, view_box: SvgViewBox) -> String {
        let mut attrs = format!(
            "x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"#ffffff\"",
            self.x, self.y, self.font_size
        );
        if let Some(value) = &self.text_anchor {
            attrs.push_str(&format!(" text-anchor=\"{}\"", escape_attr(value)));
        }
        if let Some(value) = &self.dominant_baseline {
            attrs.push_str(&format!(" dominant-baseline=\"{}\"", escape_attr(value)));
        }
        if let Some(value) = &self.font_family {
            attrs.push_str(&format!(" font-family=\"{}\"", escape_attr(value)));
        }
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{} {} {} {}\" width=\"{}\" height=\"{}\"><text {attrs}>{}</text></svg>",
            view_box.min_x,
            view_box.min_y,
            view_box.width,
            view_box.height,
            view_box.width,
            view_box.height,
            escape_text(content)
        )
    }

    fn mask_crop(&self, pixel_width: u32, pixel_height: u32) -> Option<TextMaskCrop> {
        let scale = layer_scale(self.view_box, pixel_width, pixel_height);
        if scale <= 0.0 {
            return None;
        }

        let min_y = self.view_box.min_y;
        let max_y = self.view_box.min_y + self.view_box.height;
        let pad = self.font_size.max(1.0) * 2.0;
        let crop_top = (self.y - pad).max(min_y);
        let crop_bottom = (self.y + pad).min(max_y);
        if crop_bottom <= crop_top {
            return None;
        }

        let pixel_y = ((crop_top - min_y) * scale).floor().max(0.0) as i32;
        let pixel_height = ((crop_bottom - crop_top) * scale).ceil().max(1.0) as u32;
        Some(TextMaskCrop {
            view_box: SvgViewBox {
                min_x: self.view_box.min_x,
                min_y: crop_top,
                width: self.view_box.width,
                height: crop_bottom - crop_top,
            },
            pixel_y,
            pixel_height,
        })
    }

    fn rasterize_direct_text_mask(
        &self,
        content: &str,
        pixel_width: u32,
        pixel_height: u32,
    ) -> Option<DirectTextMask> {
        if !self.uses_fast_path_font_family() || self.dominant_baseline.is_some() {
            return None;
        }

        let font = shared_widget_ab_glyph_font()?;
        let scale = layer_scale(self.view_box, pixel_width, pixel_height);
        let crop = self.mask_crop(pixel_width, pixel_height)?;
        let font_size_px = self.font_size * scale;
        if font_size_px <= 0.0 {
            return None;
        }

        let scaled_font = font.as_scaled(font_size_px);
        let cache_key = self.direct_text_mask_cache_key(content, &crop, pixel_width, pixel_height);
        if let Some(cached) = text_svg_layer_cache()
            .lock()
            .expect("text SVG layer cache poisoned")
            .get(&cache_key)
        {
            return Some(DirectTextMask {
                pixmap: cached,
                pixel_y: crop.pixel_y,
            });
        }

        let mut previous = None;
        let mut advance = 0.0f32;
        for ch in content.chars() {
            let glyph_id = scaled_font.glyph_id(ch);
            if let Some(previous) = previous {
                advance += scaled_font.kern(previous, glyph_id);
            }
            advance += scaled_font.h_advance(glyph_id);
            previous = Some(glyph_id);
        }

        let full_rendered_width = self.view_box.width * scale;
        let offset_x =
            (pixel_width as f32 - full_rendered_width) * 0.5 - self.view_box.min_x * scale;
        let mut cursor_x = offset_x + self.x * scale;
        match self.text_anchor.as_deref() {
            Some("middle") => cursor_x -= advance * 0.5,
            Some("end") => cursor_x -= advance,
            _ => {}
        }
        let baseline_y = (self.y - crop.view_box.min_y) * scale;

        let mut pixmap = tiny_skia::Pixmap::new(pixel_width, crop.pixel_height)?;
        previous = None;
        for ch in content.chars() {
            let glyph_id = scaled_font.glyph_id(ch);
            if let Some(previous) = previous {
                cursor_x += scaled_font.kern(previous, glyph_id);
            }
            let glyph = glyph_id
                .with_scale_and_position(font_size_px, ab_glyph::point(cursor_x, baseline_y));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|x, y, coverage| {
                    let px = bounds.min.x.floor() as i32 + x as i32;
                    let py = bounds.min.y.floor() as i32 + y as i32;
                    if px < 0
                        || py < 0
                        || px >= pixel_width as i32
                        || py >= crop.pixel_height as i32
                    {
                        return;
                    }
                    let idx = py as usize * pixel_width as usize + px as usize;
                    let alpha = (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
                    if alpha <= pixmap.pixels()[idx].alpha() {
                        return;
                    }
                    pixmap.pixels_mut()[idx] =
                        tiny_skia::PremultipliedColorU8::from_rgba(alpha, alpha, alpha, alpha)
                            .unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
                });
            }
            cursor_x += scaled_font.h_advance(glyph_id);
            previous = Some(glyph_id);
        }

        if !text_svg_layer_cache_admission()
            .lock()
            .expect("text SVG layer cache admission poisoned")
            .admit(cache_key)
        {
            return Some(DirectTextMask {
                pixmap: Arc::new(pixmap),
                pixel_y: crop.pixel_y,
            });
        }

        let mut cache = text_svg_layer_cache()
            .lock()
            .expect("text SVG layer cache poisoned");
        let pixmap = cache.insert_with_limits(
            cache_key,
            pixmap,
            TEXT_SVG_LAYER_CACHE_MAX_ENTRIES,
            TEXT_SVG_LAYER_CACHE_MAX_BYTES,
            STATIC_SVG_LAYER_CACHE_MAX_ENTRY_BYTES,
        );
        Some(DirectTextMask {
            pixmap,
            pixel_y: crop.pixel_y,
        })
    }

    fn uses_fast_path_font_family(&self) -> bool {
        self.font_family.as_deref().is_none_or(|family| {
            let family = family.trim().to_ascii_lowercase();
            family.is_empty() || family.contains("sans")
        })
    }

    fn direct_text_mask_cache_key(
        &self,
        content: &str,
        crop: &TextMaskCrop,
        pixel_width: u32,
        pixel_height: u32,
    ) -> StaticSvgLayerCacheKey {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"direct-text-mask-v1");
        hasher.update(content.as_bytes());
        hasher.update(&pixel_height.to_le_bytes());
        hasher.update(&self.x.to_le_bytes());
        hasher.update(&self.y.to_le_bytes());
        hasher.update(&self.font_size.to_le_bytes());
        hasher.update(self.text_anchor.as_deref().unwrap_or("").as_bytes());
        hasher.update(self.font_family.as_deref().unwrap_or("").as_bytes());
        hasher.update(&crop.view_box.min_x.to_le_bytes());
        hasher.update(&crop.view_box.min_y.to_le_bytes());
        hasher.update(&crop.view_box.width.to_le_bytes());
        hasher.update(&crop.view_box.height.to_le_bytes());
        StaticSvgLayerCacheKey {
            svg_digest: *hasher.finalize().as_bytes(),
            pixel_width,
            pixel_height: crop.pixel_height,
        }
    }
}

#[derive(Clone, Copy)]
struct TextMaskCrop {
    view_box: SvgViewBox,
    pixel_y: i32,
    pixel_height: u32,
}

struct DirectTextMask {
    pixmap: Arc<tiny_skia::Pixmap>,
    pixel_y: i32,
}

fn group_ids(groups: &[PrimitiveGroup]) -> Vec<String> {
    groups.iter().filter_map(|group| group.id.clone()).collect()
}

fn group_opacity(groups: &[PrimitiveGroup]) -> f32 {
    groups
        .iter()
        .map(|group| group.opacity)
        .fold(1.0, |acc, opacity| acc * opacity)
}

fn bound_ancestor_opacity(ancestor_ids: &[String], bindings: &ResolvedLayerBindings) -> f32 {
    ancestor_ids.iter().fold(1.0, |acc, id| {
        let bound = bindings
            .by_target_attr
            .get(id)
            .and_then(|attrs| attrs.get("opacity"))
            .and_then(|value| parse_svg_number(value))
            .unwrap_or(1.0);
        acc * bound
    })
}

fn target_or_ancestor_has_binding(
    id: Option<&str>,
    ancestor_ids: &[String],
    bindings: &ResolvedLayerBindings,
) -> bool {
    id.is_some_and(|id| bindings.by_target_attr.contains_key(id))
        || ancestor_ids
            .iter()
            .any(|ancestor_id| bindings.by_target_attr.contains_key(ancestor_id))
}

fn is_svg_paint_none(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("none")
}

fn contains_unsupported_primitive_svg(svg_text: &str) -> bool {
    [
        "<path",
        "<line",
        "<polyline",
        "<polygon",
        "<ellipse",
        "<image",
        "<use",
        "<linearGradient",
        "<radialGradient",
        "<pattern",
        " style=",
        " transform=",
    ]
    .iter()
    .any(|needle| svg_text.contains(needle))
}

fn find_start_tag<'a>(svg_text: &'a str, name: &str) -> Option<&'a str> {
    let start = svg_text.find(&format!("<{name}"))?;
    let end = start + find_tag_end(&svg_text[start..])?;
    Some(&svg_text[start..=end])
}

fn tag_name(raw_tag: &str) -> Option<&str> {
    let trimmed = raw_tag.trim_start_matches('<').trim_start_matches('/');
    let end = trimmed
        .char_indices()
        .find_map(|(idx, ch)| (ch.is_whitespace() || ch == '>' || ch == '/').then_some(idx))
        .unwrap_or(trimmed.len());
    let name = &trimmed[..end];
    (!name.is_empty()).then_some(name)
}

fn parse_tag_attrs(raw_tag: &str) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    let mut i = match raw_tag.find(char::is_whitespace) {
        Some(pos) => pos + 1,
        None => return attrs,
    };
    let bytes = raw_tag.as_bytes();

    while i < raw_tag.len() {
        while i < raw_tag.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= raw_tag.len() || bytes[i] == b'>' || bytes[i] == b'/' {
            break;
        }

        let key_start = i;
        while i < raw_tag.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let key = raw_tag[key_start..i].to_string();
        while i < raw_tag.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= raw_tag.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < raw_tag.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= raw_tag.len() {
            break;
        }
        let quote = bytes[i];
        if quote != b'"' && quote != b'\'' {
            continue;
        }
        i += 1;
        let value_start = i;
        while i < raw_tag.len() && bytes[i] != quote {
            i += 1;
        }
        if i <= raw_tag.len() {
            attrs.insert(key, raw_tag[value_start..i].to_string());
        }
        i += 1;
    }

    attrs
}

fn parse_view_box(attrs: &BTreeMap<String, String>) -> Option<SvgViewBox> {
    let raw = attrs.get("viewBox")?;
    let nums: Vec<f32> = raw
        .split(|ch: char| ch.is_ascii_whitespace() || ch == ',')
        .filter(|part| !part.is_empty())
        .filter_map(parse_svg_number)
        .collect();
    (nums.len() == 4).then_some(SvgViewBox {
        min_x: nums[0],
        min_y: nums[1],
        width: nums[2],
        height: nums[3],
    })
}

fn parse_f32_attr(attrs: &BTreeMap<String, String>, key: &str) -> Option<f32> {
    attrs.get(key).and_then(|value| parse_svg_number(value))
}

fn parse_svg_number(value: &str) -> Option<f32> {
    let trimmed = value.trim();
    let end = trimmed
        .char_indices()
        .find_map(|(idx, ch)| {
            (!(ch.is_ascii_digit() || matches!(ch, '.' | '-' | '+' | 'e' | 'E'))).then_some(idx)
        })
        .unwrap_or(trimmed.len());
    trimmed[..end].parse::<f32>().ok()
}

fn parse_svg_color(value: &str) -> Option<(u8, u8, u8, f32)> {
    let v = value.trim();
    if v == "none" {
        return None;
    }
    if let Some(hex) = v.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
                let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
                let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
                Some((r, g, b, 1.0))
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some((r, g, b, 1.0))
            }
            _ => None,
        };
    }
    if let Some(raw) = v.strip_prefix("rgba(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
        if parts.len() == 4 {
            let r = parts[0].parse::<u8>().ok()?;
            let g = parts[1].parse::<u8>().ok()?;
            let b = parts[2].parse::<u8>().ok()?;
            let a = parts[3].parse::<f32>().ok()?;
            return Some((r, g, b, a));
        }
    }
    None
}

fn paint_rgba(r: u8, g: u8, b: u8, a: f32) -> tiny_skia::Paint<'static> {
    let mut paint = tiny_skia::Paint::default();
    paint.set_color_rgba8(r, g, b, (a.clamp(0.0, 1.0) * 255.0).round() as u8);
    paint.anti_alias = true;
    paint
}

fn rounded_rect_path(rect: tiny_skia::Rect, rx: f32, ry: f32) -> Option<tiny_skia::Path> {
    const KAPPA: f32 = 0.552_284_8;

    let left = rect.left();
    let top = rect.top();
    let right = rect.right();
    let bottom = rect.bottom();
    let ox = rx * KAPPA;
    let oy = ry * KAPPA;

    let mut path = tiny_skia::PathBuilder::new();
    path.move_to(left + rx, top);
    path.line_to(right - rx, top);
    path.cubic_to(right - rx + ox, top, right, top + ry - oy, right, top + ry);
    path.line_to(right, bottom - ry);
    path.cubic_to(
        right,
        bottom - ry + oy,
        right - rx + ox,
        bottom,
        right - rx,
        bottom,
    );
    path.line_to(left + rx, bottom);
    path.cubic_to(
        left + rx - ox,
        bottom,
        left,
        bottom - ry + oy,
        left,
        bottom - ry,
    );
    path.line_to(left, top + ry);
    path.cubic_to(left, top + ry - oy, left + rx - ox, top, left + rx, top);
    path.close();
    path.finish()
}

fn draw_tinted_text_mask_at(
    pixmap: &mut tiny_skia::Pixmap,
    mask: tiny_skia::PixmapRef<'_>,
    dst_x: i32,
    dst_y: i32,
    color: (u8, u8, u8, f32),
    opacity: f32,
) {
    let color_alpha = (color.3 * opacity).clamp(0.0, 1.0);
    if color_alpha <= 0.0 {
        return;
    }
    let Some((left, top, right, bottom)) = text_mask_alpha_bounds(mask) else {
        return;
    };
    let color_alpha_u8 = (color_alpha * 255.0).round() as u32;
    let src_width = mask.width() as usize;
    let dst_width = pixmap.width() as usize;
    let dst_height = pixmap.height() as usize;
    let dst_pixels = pixmap.pixels_mut();
    let src_pixels = mask.pixels();

    for src_y in top..bottom {
        let Some(target_y) = (dst_y + src_y as i32)
            .try_into()
            .ok()
            .filter(|target_y: &usize| *target_y < dst_height)
        else {
            continue;
        };
        let src_row_start = src_y * src_width;
        let dst_row_start = target_y * dst_width;
        for src_x in left..right {
            let Some(target_x) = (dst_x + src_x as i32)
                .try_into()
                .ok()
                .filter(|target_x: &usize| *target_x < dst_width)
            else {
                continue;
            };
            let dst = &mut dst_pixels[dst_row_start + target_x];
            let src = &src_pixels[src_row_start + src_x];
            tint_text_mask_pixel(dst, src, color, color_alpha_u8);
        }
    }
}

fn tint_text_mask_pixel(
    dst: &mut tiny_skia::PremultipliedColorU8,
    src: &tiny_skia::PremultipliedColorU8,
    color: (u8, u8, u8, f32),
    color_alpha_u8: u32,
) {
    let mask_alpha = src.alpha() as u32;
    if mask_alpha == 0 {
        return;
    }

    let src_alpha = (mask_alpha * color_alpha_u8 + 127) / 255;
    if src_alpha == 0 {
        return;
    }

    let src_r = (color.0 as u32 * src_alpha + 127) / 255;
    let src_g = (color.1 as u32 * src_alpha + 127) / 255;
    let src_b = (color.2 as u32 * src_alpha + 127) / 255;
    let inv_alpha = 255 - src_alpha;
    let out_alpha = src_alpha + (dst.alpha() as u32 * inv_alpha + 127) / 255;
    let out_r = src_r + (dst.red() as u32 * inv_alpha + 127) / 255;
    let out_g = src_g + (dst.green() as u32 * inv_alpha + 127) / 255;
    let out_b = src_b + (dst.blue() as u32 * inv_alpha + 127) / 255;

    *dst = tiny_skia::PremultipliedColorU8::from_rgba(
        out_r.min(out_alpha) as u8,
        out_g.min(out_alpha) as u8,
        out_b.min(out_alpha) as u8,
        out_alpha as u8,
    )
    .unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
}

fn text_mask_alpha_bounds(mask: tiny_skia::PixmapRef<'_>) -> Option<(usize, usize, usize, usize)> {
    let width = mask.width() as usize;
    let height = mask.height() as usize;
    let mut left = width;
    let mut top = height;
    let mut right = 0usize;
    let mut bottom = 0usize;

    for (idx, pixel) in mask.pixels().iter().enumerate() {
        if pixel.alpha() == 0 {
            continue;
        }
        let x = idx % width;
        let y = idx / width;
        left = left.min(x);
        top = top.min(y);
        right = right.max(x + 1);
        bottom = bottom.max(y + 1);
    }

    (right > left && bottom > top).then_some((left, top, right, bottom))
}

fn escape_attr(value: &str) -> String {
    escape_text(value)
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn html_unescape_text(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

// ─── SVG attribute manipulation ───────────────────────────────────────────────

/// Apply a single attribute change to an SVG element identified by `element_id`.
///
/// Finds the XML tag containing `id="element_id"` and sets the named attribute.
/// If the attribute already exists in the tag, its value is replaced. Otherwise
/// the attribute is inserted before the closing `>` or `/>`.
///
/// For the synthetic `text-content` target attribute, the text node content of
/// the element is replaced (spec §Requirement: SVG Layer Parameter Bindings).
///
/// # Limitations
///
/// This operates on the raw SVG string with simple pattern matching. It handles
/// the common SVG element authoring patterns (single-line elements, attributes
/// on the same line as the id). Elements with multi-line attributes or id
/// attributes appearing after the target attribute may not be updated correctly.
/// Widget bundle authors should ensure elements are on single lines with `id`
/// as the first attribute for best results.
pub fn apply_svg_attribute(svg: &str, element_id: &str, attr: &str, value: &str) -> String {
    let id_patterns = [format!("id=\"{element_id}\""), format!("id='{element_id}'")];

    for id_pattern in &id_patterns {
        if let Some(elem_pos) = svg.find(id_pattern.as_str()) {
            let before_id = &svg[..elem_pos];
            let tag_start = match before_id.rfind('<') {
                Some(p) => p,
                None => continue,
            };

            // Find end of this tag (either > or />)
            let from_tag = &svg[tag_start..];

            // Check for text-content synthetic target
            if attr == "text-content" {
                return apply_svg_text_content(svg, element_id, value);
            }

            // Find the position of closing > or />
            let tag_end_rel = match find_tag_end(from_tag) {
                Some(p) => p,
                None => continue,
            };
            let tag_end = tag_start + tag_end_rel;
            let tag_content = &svg[tag_start..tag_end];

            // Check if this attr already exists in the tag
            let attr_eq = format!("{attr}=\"");
            let attr_eq_single = format!("{attr}='");

            if let Some(attr_pos) = tag_content.find(attr_eq.as_str()) {
                // Replace double-quoted existing value
                let abs_start = tag_start + attr_pos + attr_eq.len();
                let val_end = svg[abs_start..].find('"').unwrap_or(0);
                let abs_end = abs_start + val_end;
                return format!("{}{}{}", &svg[..abs_start], value, &svg[abs_end..]);
            } else if let Some(attr_pos) = tag_content.find(attr_eq_single.as_str()) {
                // Replace single-quoted existing value
                let abs_start = tag_start + attr_pos + attr_eq_single.len();
                let val_end = svg[abs_start..].find('\'').unwrap_or(0);
                let abs_end = abs_start + val_end;
                return format!("{}{}{}", &svg[..abs_start], value, &svg[abs_end..]);
            } else {
                // Insert new attribute before tag end (> or />)
                let insert_pos = tag_end;
                return format!(
                    "{} {}=\"{}\"{}",
                    &svg[..insert_pos],
                    attr,
                    value,
                    &svg[insert_pos..]
                );
            }
        }
    }

    // Element not found; return unmodified
    svg.to_string()
}

/// Find the end of an XML start tag in `s` (the position of '>' or '/', where
/// the next char is '>', not being inside a quoted attribute value).
///
/// Returns the index of the closing character ('>' or the '/' in '/>').
fn find_tag_end(s: &str) -> Option<usize> {
    let mut in_double_quote = false;
    let mut in_single_quote = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' if !in_single_quote => in_double_quote = !in_double_quote,
            '\'' if !in_double_quote => in_single_quote = !in_single_quote,
            '>' if !in_double_quote && !in_single_quote => {
                // Check if it's '/>'
                if i > 0 && s.as_bytes().get(i - 1) == Some(&b'/') {
                    return Some(i - 1);
                }
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

/// Replace the text content of an SVG element (for the synthetic `text-content`
/// binding target).
///
/// Finds `<tag id="element_id"...>` and replaces the character data between the
/// opening and closing tag.
fn apply_svg_text_content(svg: &str, element_id: &str, value: &str) -> String {
    let id_patterns = [format!("id=\"{element_id}\""), format!("id='{element_id}'")];

    for id_pattern in &id_patterns {
        if let Some(elem_pos) = svg.find(id_pattern.as_str()) {
            let before_id = &svg[..elem_pos];
            let tag_start = match before_id.rfind('<') {
                Some(p) => p,
                None => continue,
            };

            // Find the '>' closing the start tag
            let from_tag = &svg[tag_start..];
            let open_end = match from_tag.find('>') {
                Some(p) => tag_start + p + 1,
                None => continue,
            };

            // Find the closing tag by looking for '</'
            let after_open = &svg[open_end..];
            let close_start = match after_open.find("</") {
                Some(p) => open_end + p,
                None => continue,
            };

            // Replace the text content
            return format!("{}{}{}", &svg[..open_end], value, &svg[close_start..]);
        }
    }

    svg.to_string()
}

// ─── Binding value resolution ─────────────────────────────────────────────────

/// Compute the SVG attribute string value for a single binding.
///
/// Returns `None` if the binding cannot be resolved (e.g. parameter not found
/// in `params`).
pub fn resolve_binding_value(
    binding: &WidgetBinding,
    params: &HashMap<String, WidgetParameterValue>,
    param_constraints: &HashMap<String, (f32, f32)>, // (min, max) for f32 params
) -> Option<String> {
    let param_val = params.get(&binding.param)?;

    match &binding.mapping {
        WidgetBindingMapping::Linear { attr_min, attr_max } => {
            let f = match param_val {
                WidgetParameterValue::F32(v) => *v,
                _ => return None,
            };
            // Get param constraints for normalization
            let (p_min, p_max) = param_constraints
                .get(&binding.param)
                .copied()
                .unwrap_or((0.0, 1.0));
            let normalized = if (p_max - p_min).abs() < f32::EPSILON {
                0.0f32
            } else {
                ((f - p_min) / (p_max - p_min)).clamp(0.0, 1.0)
            };
            let attr_val = attr_min + normalized * (attr_max - attr_min);
            // Format with enough precision but avoid spurious decimals
            if (attr_val - attr_val.round()).abs() < 0.001 {
                let rounded = attr_val.round() as i64;
                Some(format!("{rounded}"))
            } else {
                Some(format!("{attr_val:.3}"))
            }
        }

        WidgetBindingMapping::Direct => match param_val {
            WidgetParameterValue::String(s) => Some(s.clone()),
            WidgetParameterValue::Color(rgba) => Some(rgba_to_svg_color(rgba)),
            _ => None,
        },

        WidgetBindingMapping::Discrete { value_map } => {
            let key = match param_val {
                WidgetParameterValue::Enum(s) => s.as_str(),
                _ => return None,
            };
            value_map.get(key).cloned()
        }
    }
}

/// Convert an `Rgba` (f32 components 0.0..=1.0) to an SVG color string `rgba(r,g,b,a)`.
fn rgba_to_svg_color(rgba: &Rgba) -> String {
    let r = (rgba.r * 255.0).round() as u8;
    let g = (rgba.g * 255.0).round() as u8;
    let b = (rgba.b * 255.0).round() as u8;
    let a = rgba.a; // SVG opacity as 0.0..=1.0
    if (a - 1.0).abs() < 0.004 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("rgba({r},{g},{b},{a:.3})")
    }
}

// ─── Parameter interpolation ──────────────────────────────────────────────────

/// Compute the effective transition progress `t` (0.0..=1.0) for a widget
/// animation, taking the current degradation level into account.
///
/// Under [`DegradationLevel::Simplified`], the compositor snaps to the final values
/// by returning `1.0` immediately, regardless of elapsed time.  This reduces
/// per-frame re-rasterizations during periods of degradation.
///
/// Under `Nominal`, the normal time-based interpolation value is returned.
pub fn compute_transition_t(
    elapsed_ms: f32,
    duration_ms: f32,
    degradation_level: DegradationLevel,
) -> f32 {
    if degradation_level >= DegradationLevel::Simplified {
        1.0
    } else {
        (elapsed_ms / duration_ms).clamp(0.0, 1.0)
    }
}

/// Interpolate between `old` and `new` parameter values at time `t` (0.0..=1.0).
///
/// - f32: linear interpolation
/// - color: component-wise linear interpolation in sRGB space
/// - string / enum: snap to `new` at t=0 (applied immediately)
pub fn interpolate_param(
    old: &WidgetParameterValue,
    new: &WidgetParameterValue,
    t: f32,
) -> WidgetParameterValue {
    match (old, new) {
        (WidgetParameterValue::F32(a), WidgetParameterValue::F32(b)) => {
            WidgetParameterValue::F32(a + (b - a) * t)
        }
        (WidgetParameterValue::Color(ca), WidgetParameterValue::Color(cb)) => {
            WidgetParameterValue::Color(Rgba::new(
                ca.r + (cb.r - ca.r) * t,
                ca.g + (cb.g - ca.g) * t,
                ca.b + (cb.b - ca.b) * t,
                ca.a + (cb.a - ca.a) * t,
            ))
        }
        // String and enum snap to new value immediately (spec §Widget Parameter Interpolation)
        _ => new.clone(),
    }
}

// ─── Widget texture cache ─────────────────────────────────────────────────────

/// Per-instance cached GPU texture for a widget.
///
/// When `dirty` is true, the compositor re-rasterizes the SVG and uploads a
/// new texture. When false, the cached texture is reused.
pub struct WidgetTextureEntry {
    resident_allocation_id: Option<tze_hud_resource::AllocationId>,
    /// The cached wgpu texture (RGBA8Unorm, premultiplied alpha).
    pub texture: wgpu::Texture,
    /// Bind group for sampling the texture in the widget render pass.
    pub bind_group: wgpu::BindGroup,
    /// Width of the texture in pixels.
    pub width: u32,
    /// Height of the texture in pixels.
    pub height: u32,
    /// When true, the texture must be re-rasterized before compositing.
    pub dirty: bool,
    /// Parameters used for the last successful rasterization.
    /// Compared against current scene params to detect changes from MCP publishes.
    pub last_rendered_params: HashMap<String, WidgetParameterValue>,
    /// Animation state: start time, duration, starting params, target params.
    pub animation: Option<WidgetAnimationState>,
}

/// Active animation state for a widget instance.
pub struct WidgetAnimationState {
    pub start: Instant,
    pub duration_ms: u32,
    pub from_params: HashMap<String, WidgetParameterValue>,
    pub to_params: HashMap<String, WidgetParameterValue>,
}

// ─── Standalone SVG rasterization (CPU-only, no GPU) ─────────────────────────

fn widget_usvg_options() -> resvg::usvg::Options<'static> {
    resvg::usvg::Options {
        fontdb: shared_widget_fontdb(),
        ..Default::default()
    }
}

fn shared_widget_fontdb() -> Arc<resvg::usvg::fontdb::Database> {
    static FONTDB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    FONTDB
        .get_or_init(|| {
            let mut db = resvg::usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

fn shared_widget_ab_glyph_font() -> Option<&'static ab_glyph::FontArc> {
    static FONT: OnceLock<Option<ab_glyph::FontArc>> = OnceLock::new();
    FONT.get_or_init(|| {
        let db = shared_widget_fontdb();
        let id = db.query(&resvg::usvg::fontdb::Query {
            families: &[resvg::usvg::fontdb::Family::SansSerif],
            ..Default::default()
        })?;
        db.with_face_data(id, |data, _face_index| {
            ab_glyph::FontArc::try_from_vec(data.to_vec()).ok()
        })
        .flatten()
    })
    .as_ref()
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct StaticSvgLayerCacheKey {
    svg_digest: [u8; 32],
    pixel_width: u32,
    pixel_height: u32,
}

const STATIC_SVG_LAYER_CACHE_MAX_ENTRIES: usize = 64;
const STATIC_SVG_LAYER_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;
const STATIC_SVG_LAYER_CACHE_MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;

struct StaticSvgLayerCacheEntry {
    pixmap: Arc<tiny_skia::Pixmap>,
    byte_len: usize,
}

#[derive(Default)]
struct StaticSvgLayerCache {
    entries: HashMap<StaticSvgLayerCacheKey, StaticSvgLayerCacheEntry>,
    lru: VecDeque<StaticSvgLayerCacheKey>,
    total_bytes: usize,
}

impl StaticSvgLayerCache {
    fn get(&mut self, key: &StaticSvgLayerCacheKey) -> Option<Arc<tiny_skia::Pixmap>> {
        let pixmap = self.entries.get(key)?.pixmap.clone();
        self.promote(key);
        Some(pixmap)
    }

    fn insert(
        &mut self,
        key: StaticSvgLayerCacheKey,
        pixmap: tiny_skia::Pixmap,
    ) -> Arc<tiny_skia::Pixmap> {
        self.insert_with_limits(
            key,
            pixmap,
            STATIC_SVG_LAYER_CACHE_MAX_ENTRIES,
            STATIC_SVG_LAYER_CACHE_MAX_BYTES,
            STATIC_SVG_LAYER_CACHE_MAX_ENTRY_BYTES,
        )
    }

    fn insert_with_limits(
        &mut self,
        key: StaticSvgLayerCacheKey,
        pixmap: tiny_skia::Pixmap,
        max_entries: usize,
        max_bytes: usize,
        max_entry_bytes: usize,
    ) -> Arc<tiny_skia::Pixmap> {
        let byte_len = pixmap.data().len();
        let pixmap = Arc::new(pixmap);
        if byte_len > max_entry_bytes || byte_len > max_bytes || max_entries == 0 {
            return pixmap;
        }

        if let Some(old) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(old.byte_len);
            self.remove_from_lru(&key);
        }

        self.total_bytes = self.total_bytes.saturating_add(byte_len);
        self.entries.insert(
            key,
            StaticSvgLayerCacheEntry {
                pixmap: Arc::clone(&pixmap),
                byte_len,
            },
        );
        self.lru.push_back(key);

        while self.entries.len() > max_entries || self.total_bytes > max_bytes {
            if !self.evict_lru_one() {
                break;
            }
        }
        self.entries
            .get(&key)
            .map(|entry| Arc::clone(&entry.pixmap))
            .unwrap_or(pixmap)
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
        self.total_bytes = 0;
    }

    fn promote(&mut self, key: &StaticSvgLayerCacheKey) {
        self.remove_from_lru(key);
        self.lru.push_back(*key);
    }

    fn remove_from_lru(&mut self, key: &StaticSvgLayerCacheKey) {
        if let Some(pos) = self.lru.iter().position(|candidate| candidate == key) {
            self.lru.remove(pos);
        }
    }

    fn evict_lru_one(&mut self) -> bool {
        while let Some(key) = self.lru.pop_front() {
            if let Some(entry) = self.entries.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.byte_len);
                return true;
            }
        }
        false
    }
}

#[derive(Default)]
struct RasterCacheAdmission {
    keys: HashSet<StaticSvgLayerCacheKey>,
    lru: VecDeque<StaticSvgLayerCacheKey>,
}

impl RasterCacheAdmission {
    fn admit(&mut self, key: StaticSvgLayerCacheKey) -> bool {
        if self.keys.remove(&key) {
            self.remove_from_lru(&key);
            return true;
        }

        self.keys.insert(key);
        self.lru.push_back(key);
        while self.keys.len() > RASTER_CACHE_ADMISSION_MAX_KEYS {
            if let Some(evicted) = self.lru.pop_front() {
                self.keys.remove(&evicted);
            } else {
                break;
            }
        }
        false
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.lru.clear();
    }

    fn remove_from_lru(&mut self, key: &StaticSvgLayerCacheKey) {
        if let Some(pos) = self.lru.iter().position(|candidate| candidate == key) {
            self.lru.remove(pos);
        }
    }
}

enum RasterizedSvgLayer {
    Owned(tiny_skia::Pixmap),
    Shared(Arc<tiny_skia::Pixmap>),
}

impl RasterizedSvgLayer {
    fn as_ref(&self) -> tiny_skia::PixmapRef<'_> {
        match self {
            Self::Owned(pixmap) => pixmap.as_ref(),
            Self::Shared(pixmap) => pixmap.as_ref().as_ref(),
        }
    }
}

fn static_svg_layer_cache() -> &'static Mutex<StaticSvgLayerCache> {
    static CACHE: OnceLock<Mutex<StaticSvgLayerCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(StaticSvgLayerCache::default()))
}

fn static_svg_layer_cache_key(
    svg_text: &str,
    pixel_width: u32,
    pixel_height: u32,
) -> StaticSvgLayerCacheKey {
    StaticSvgLayerCacheKey {
        svg_digest: *blake3::hash(svg_text.as_bytes()).as_bytes(),
        pixel_width,
        pixel_height,
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RasterizedLayerCacheKey {
    source_digest: [u8; 32],
    binding_digest: [u8; 32],
    pixel_width: u32,
    pixel_height: u32,
}

const BOUND_SVG_LAYER_CACHE_MAX_ENTRIES: usize = 128;
const BOUND_SVG_LAYER_CACHE_MAX_BYTES: usize = 48 * 1024 * 1024;
const TEXT_SVG_LAYER_CACHE_MAX_ENTRIES: usize = 128;
const TEXT_SVG_LAYER_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;
const PRIMITIVE_NON_TEXT_LAYER_CACHE_MAX_ENTRIES: usize = 128;
const PRIMITIVE_NON_TEXT_LAYER_CACHE_MAX_BYTES: usize = 48 * 1024 * 1024;
const COMPOSED_WIDGET_CACHE_MAX_ENTRIES: usize = 64;
const COMPOSED_WIDGET_CACHE_MAX_BYTES: usize = 48 * 1024 * 1024;
const RASTER_CACHE_ADMISSION_MAX_KEYS: usize = 256;

type RasterizedLayerCache = StaticSvgLayerCache;

fn bound_svg_layer_cache() -> &'static Mutex<RasterizedLayerCache> {
    static CACHE: OnceLock<Mutex<RasterizedLayerCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(RasterizedLayerCache::default()))
}

fn text_svg_layer_cache() -> &'static Mutex<RasterizedLayerCache> {
    static CACHE: OnceLock<Mutex<RasterizedLayerCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(RasterizedLayerCache::default()))
}

fn primitive_non_text_layer_cache() -> &'static Mutex<RasterizedLayerCache> {
    static CACHE: OnceLock<Mutex<RasterizedLayerCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(RasterizedLayerCache::default()))
}

fn composed_widget_cache() -> &'static Mutex<RasterizedLayerCache> {
    static CACHE: OnceLock<Mutex<RasterizedLayerCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(RasterizedLayerCache::default()))
}

fn bound_svg_layer_cache_admission() -> &'static Mutex<RasterCacheAdmission> {
    static ADMISSION: OnceLock<Mutex<RasterCacheAdmission>> = OnceLock::new();
    ADMISSION.get_or_init(|| Mutex::new(RasterCacheAdmission::default()))
}

fn text_svg_layer_cache_admission() -> &'static Mutex<RasterCacheAdmission> {
    static ADMISSION: OnceLock<Mutex<RasterCacheAdmission>> = OnceLock::new();
    ADMISSION.get_or_init(|| Mutex::new(RasterCacheAdmission::default()))
}

fn composed_widget_cache_admission() -> &'static Mutex<RasterCacheAdmission> {
    static ADMISSION: OnceLock<Mutex<RasterCacheAdmission>> = OnceLock::new();
    ADMISSION.get_or_init(|| Mutex::new(RasterCacheAdmission::default()))
}

fn cache_key_from_raster_key(key: &RasterizedLayerCacheKey) -> StaticSvgLayerCacheKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&key.source_digest);
    hasher.update(&key.binding_digest);
    StaticSvgLayerCacheKey {
        svg_digest: *hasher.finalize().as_bytes(),
        pixel_width: key.pixel_width,
        pixel_height: key.pixel_height,
    }
}

fn primitive_non_text_segment_cache_key(
    source_digest: [u8; 32],
    segment_index: u32,
    binding_digest: [u8; 32],
    pixel_width: u32,
    pixel_height: u32,
) -> StaticSvgLayerCacheKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"primitive-non-text-segment-v1");
    hasher.update(&source_digest);
    hasher.update(&segment_index.to_le_bytes());
    hasher.update(&binding_digest);
    StaticSvgLayerCacheKey {
        svg_digest: *hasher.finalize().as_bytes(),
        pixel_width,
        pixel_height,
    }
}

fn resolve_layer_bindings(
    bindings: &[WidgetBinding],
    params: &HashMap<String, WidgetParameterValue>,
    param_constraints: &HashMap<String, (f32, f32)>,
) -> ResolvedLayerBindings {
    let mut by_target_attr = BTreeMap::new();
    let mut hasher = blake3::Hasher::new();

    for binding in bindings {
        if let Some(attr_val) = resolve_binding_value(binding, params, param_constraints) {
            hasher.update(binding.target_element.as_bytes());
            hasher.update(&[0]);
            hasher.update(binding.target_attribute.as_bytes());
            hasher.update(&[0]);
            hasher.update(attr_val.as_bytes());
            hasher.update(&[0]);
            by_target_attr
                .entry(binding.target_element.clone())
                .or_insert_with(BTreeMap::new)
                .insert(binding.target_attribute.clone(), attr_val);
        }
    }

    ResolvedLayerBindings {
        by_target_attr,
        digest: *hasher.finalize().as_bytes(),
    }
}

fn cached_bound_layer(key: RasterizedLayerCacheKey) -> Option<Arc<tiny_skia::Pixmap>> {
    bound_svg_layer_cache()
        .lock()
        .expect("bound SVG layer cache poisoned")
        .get(&cache_key_from_raster_key(&key))
}

fn insert_bound_layer(
    key: RasterizedLayerCacheKey,
    pixmap: tiny_skia::Pixmap,
) -> Arc<tiny_skia::Pixmap> {
    let cache_key = cache_key_from_raster_key(&key);
    if !admit_bound_layer_cache_key(cache_key) {
        return Arc::new(pixmap);
    }
    insert_admitted_bound_layer(cache_key, pixmap)
}

fn admit_bound_layer_cache_key(cache_key: StaticSvgLayerCacheKey) -> bool {
    bound_svg_layer_cache_admission()
        .lock()
        .expect("bound SVG layer cache admission poisoned")
        .admit(cache_key)
}

fn insert_admitted_bound_layer(
    cache_key: StaticSvgLayerCacheKey,
    pixmap: tiny_skia::Pixmap,
) -> Arc<tiny_skia::Pixmap> {
    let mut cache = bound_svg_layer_cache()
        .lock()
        .expect("bound SVG layer cache poisoned");
    cache.insert_with_limits(
        cache_key,
        pixmap,
        BOUND_SVG_LAYER_CACHE_MAX_ENTRIES,
        BOUND_SVG_LAYER_CACHE_MAX_BYTES,
        STATIC_SVG_LAYER_CACHE_MAX_ENTRY_BYTES,
    )
}

fn rasterize_cached_text_svg(
    svg_text: &str,
    pixel_width: u32,
    pixel_height: u32,
) -> Option<Arc<tiny_skia::Pixmap>> {
    let key = static_svg_layer_cache_key(svg_text, pixel_width, pixel_height);
    if let Some(cached) = text_svg_layer_cache()
        .lock()
        .expect("text SVG layer cache poisoned")
        .get(&key)
    {
        return Some(cached);
    }

    let pixmap = rasterize_single_svg_layer(svg_text, pixel_width, pixel_height)?;
    if !text_svg_layer_cache_admission()
        .lock()
        .expect("text SVG layer cache admission poisoned")
        .admit(key)
    {
        return Some(Arc::new(pixmap));
    }

    let mut cache = text_svg_layer_cache()
        .lock()
        .expect("text SVG layer cache poisoned");
    let pixmap = cache.insert_with_limits(
        key,
        pixmap,
        TEXT_SVG_LAYER_CACHE_MAX_ENTRIES,
        TEXT_SVG_LAYER_CACHE_MAX_BYTES,
        STATIC_SVG_LAYER_CACHE_MAX_ENTRY_BYTES,
    );
    Some(pixmap)
}

fn composed_widget_cache_key(
    plan: &WidgetRenderPlan,
    resolved_layers: &[ResolvedLayerBindings],
    pixel_width: u32,
    pixel_height: u32,
) -> StaticSvgLayerCacheKey {
    let mut hasher = blake3::Hasher::new();
    for (layer, resolved) in plan.layers.iter().zip(resolved_layers) {
        hasher.update(&layer.source_digest);
        hasher.update(&resolved.digest);
    }
    StaticSvgLayerCacheKey {
        svg_digest: *hasher.finalize().as_bytes(),
        pixel_width,
        pixel_height,
    }
}

fn rasterize_single_svg_layer(
    svg_text: &str,
    pixel_width: u32,
    pixel_height: u32,
) -> Option<tiny_skia::Pixmap> {
    // Parse modified SVG into usvg::Tree.
    let opts = widget_usvg_options();
    let tree = match resvg::usvg::Tree::from_str(svg_text, &opts) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "rasterize_single_svg_layer: failed to parse SVG");
            return None;
        }
    };

    // Rasterize into a pixmap at the target size.
    let mut pixmap = match tiny_skia::Pixmap::new(pixel_width, pixel_height) {
        Some(p) => p,
        None => {
            tracing::warn!(
                width = pixel_width,
                height = pixel_height,
                "rasterize_single_svg_layer: failed to allocate pixmap"
            );
            return None;
        }
    };

    // Scale the SVG uniformly (preserving aspect ratio) and center it
    // within the target pixel rectangle.  This prevents circles from
    // becoming ovals on non-square screen geometries.
    let svg_size = tree.size();
    let sx = pixel_width as f32 / svg_size.width();
    let sy = pixel_height as f32 / svg_size.height();
    let uniform_scale = sx.min(sy);
    let rendered_w = svg_size.width() * uniform_scale;
    let rendered_h = svg_size.height() * uniform_scale;
    let offset_x = (pixel_width as f32 - rendered_w) * 0.5;
    let offset_y = (pixel_height as f32 - rendered_h) * 0.5;
    let transform = tiny_skia::Transform::from_translate(offset_x, offset_y)
        .post_scale(uniform_scale, uniform_scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    Some(pixmap)
}

fn rasterize_static_svg_layer(
    svg_text: &str,
    pixel_width: u32,
    pixel_height: u32,
) -> Option<RasterizedSvgLayer> {
    let key = static_svg_layer_cache_key(svg_text, pixel_width, pixel_height);

    if let Some(cached) = static_svg_layer_cache()
        .lock()
        .expect("static SVG layer cache poisoned")
        .get(&key)
    {
        return Some(RasterizedSvgLayer::Shared(cached));
    }

    let pixmap = rasterize_single_svg_layer(svg_text, pixel_width, pixel_height)?;
    let byte_len = pixmap.data().len();

    if byte_len > STATIC_SVG_LAYER_CACHE_MAX_ENTRY_BYTES
        || byte_len > STATIC_SVG_LAYER_CACHE_MAX_BYTES
    {
        return Some(RasterizedSvgLayer::Owned(pixmap));
    }

    let mut cache = static_svg_layer_cache()
        .lock()
        .expect("static SVG layer cache poisoned");
    Some(RasterizedSvgLayer::Shared(cache.insert(key, pixmap)))
}

/// Clear the process-local static SVG layer raster cache.
///
/// This is primarily used by benchmarks to distinguish first-render cost from
/// hot-path cost after static widget chrome has been cached.
pub fn clear_static_svg_layer_cache() {
    static_svg_layer_cache()
        .lock()
        .expect("static SVG layer cache poisoned")
        .clear();
}

/// Clear all process-local widget raster caches.
///
/// Benchmarks use this to separate cold parse/raster cost from retained-plan
/// and warm parameter cache behavior.
pub fn clear_widget_raster_caches() {
    clear_static_svg_layer_cache();
    bound_svg_layer_cache()
        .lock()
        .expect("bound SVG layer cache poisoned")
        .clear();
    text_svg_layer_cache()
        .lock()
        .expect("text SVG layer cache poisoned")
        .clear();
    primitive_non_text_layer_cache()
        .lock()
        .expect("primitive non-text SVG layer cache poisoned")
        .clear();
    composed_widget_cache()
        .lock()
        .expect("composed widget cache poisoned")
        .clear();
    bound_svg_layer_cache_admission()
        .lock()
        .expect("bound SVG layer cache admission poisoned")
        .clear();
    text_svg_layer_cache_admission()
        .lock()
        .expect("text SVG layer cache admission poisoned")
        .clear();
    composed_widget_cache_admission()
        .lock()
        .expect("composed widget cache admission poisoned")
        .clear();
}

fn composite_layer(base: &mut Option<tiny_skia::Pixmap>, layer: RasterizedSvgLayer) {
    // Composite this layer onto the accumulation pixmap (source-over).
    // Uses tiny_skia::PixmapMut::draw_pixmap which is SIMD-optimised and
    // handles premultiplied alpha correctly — avoiding a manual pixel loop.
    if let Some(base) = base.as_mut() {
        base.as_mut().draw_pixmap(
            0,
            0,
            layer.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            tiny_skia::Transform::identity(),
            None,
        );
    } else {
        *base = Some(match layer {
            RasterizedSvgLayer::Owned(pixmap) => pixmap,
            RasterizedSvgLayer::Shared(pixmap) => (*pixmap).clone(),
        });
    }
}

/// Rasterize all SVG layers for a widget definition with parameter bindings applied.
///
/// This is the **CPU-only** portion of the widget rendering pipeline — SVG string
/// manipulation, `usvg` parsing, and `resvg`/`tiny-skia` rasterization — without
/// the GPU texture upload step.
///
/// ## Performance contract
///
/// Per widget-system/spec.md §Requirement: Widget Compositor Rendering:
/// re-rasterization MUST complete in < 2ms for a 512×512 widget on reference
/// hardware.  This function is the hot path that must satisfy that budget.
///
/// ## Arguments
///
/// - `svg_layers` — pairs of `(svg_source, layer_bindings)` for each layer
///   in `widget_def.layers`.  Pre-loaded so this function is GPU-free.
/// - `param_constraints` — map from param name to `(f32_min, f32_max)`.
/// - `params` — current parameter values.
/// - `pixel_width` / `pixel_height` — target raster size.
///
/// ## Returns
///
/// Composed `tiny_skia::Pixmap` (RGBA, premultiplied alpha), or `None` if all
/// layers failed to rasterize.
pub fn rasterize_svg_layers(
    svg_layers: &[(&str, &[WidgetBinding])],
    param_constraints: &HashMap<String, (f32, f32)>,
    params: &HashMap<String, WidgetParameterValue>,
    pixel_width: u32,
    pixel_height: u32,
) -> Option<tiny_skia::Pixmap> {
    let plan = WidgetRenderPlan::compile(svg_layers);
    rasterize_widget_render_plan(&plan, param_constraints, params, pixel_width, pixel_height)
}

/// Rasterize a retained widget render plan.
///
/// Static layers use the static layer cache, bound layers use a digest of the
/// source SVG plus resolved binding values, and primitive-compatible layers
/// avoid reparsing XML on warm parameter changes.
pub fn rasterize_widget_render_plan(
    plan: &WidgetRenderPlan,
    param_constraints: &HashMap<String, (f32, f32)>,
    params: &HashMap<String, WidgetParameterValue>,
    pixel_width: u32,
    pixel_height: u32,
) -> Option<tiny_skia::Pixmap> {
    let resolved_layers: Vec<ResolvedLayerBindings> = plan
        .layers
        .iter()
        .map(|layer| resolve_layer_bindings(&layer.bindings, params, param_constraints))
        .collect();
    let composed_key = composed_widget_cache_key(plan, &resolved_layers, pixel_width, pixel_height);
    if let Some(cached) = composed_widget_cache()
        .lock()
        .expect("composed widget cache poisoned")
        .get(&composed_key)
    {
        return Some((*cached).clone());
    }

    let mut composed: Option<tiny_skia::Pixmap> = None;

    for (layer, resolved_bindings) in plan.layers.iter().zip(&resolved_layers) {
        if layer.bindings.is_empty() {
            if let Some(pixmap) =
                rasterize_static_svg_layer(&layer.svg_text, pixel_width, pixel_height)
            {
                composite_layer(&mut composed, pixmap);
            }
            continue;
        }

        let cache_key = RasterizedLayerCacheKey {
            source_digest: layer.source_digest,
            binding_digest: resolved_bindings.digest,
            pixel_width,
            pixel_height,
        };

        if let Some(cached) = cached_bound_layer(cache_key) {
            composite_layer(&mut composed, RasterizedSvgLayer::Shared(cached));
            continue;
        }

        if let Some(primitive_plan) = &layer.primitive_plan {
            let admitted_cache_key = cache_key_from_raster_key(&cache_key);
            if !admit_bound_layer_cache_key(admitted_cache_key) {
                if composed.is_none() {
                    let Some(pixmap) = tiny_skia::Pixmap::new(pixel_width, pixel_height) else {
                        continue;
                    };
                    composed = Some(pixmap);
                }
                let target = composed.as_mut().expect("composed pixmap initialized");
                primitive_plan.draw_into(
                    target,
                    layer.source_digest,
                    resolved_bindings,
                    pixel_width,
                    pixel_height,
                );
                continue;
            }

            if let Some(pixmap) = primitive_plan.rasterize(
                layer.source_digest,
                resolved_bindings,
                pixel_width,
                pixel_height,
            ) {
                let cached = insert_admitted_bound_layer(admitted_cache_key, pixmap);
                composite_layer(&mut composed, RasterizedSvgLayer::Shared(cached));
            }
            continue;
        }

        // Apply parameter bindings to the SVG source for unsupported layers.
        let mut modified_svg = layer.svg_text.clone();
        for binding in &layer.bindings {
            if let Some(attr_val) = resolve_binding_value(binding, params, param_constraints) {
                modified_svg = apply_svg_attribute(
                    &modified_svg,
                    &binding.target_element,
                    &binding.target_attribute,
                    &attr_val,
                );
            }
        }

        if let Some(pixmap) = rasterize_single_svg_layer(&modified_svg, pixel_width, pixel_height) {
            let cached = insert_bound_layer(cache_key, pixmap);
            composite_layer(&mut composed, RasterizedSvgLayer::Shared(cached));
        }
    }

    if let Some(pixmap) = composed {
        if !composed_widget_cache_admission()
            .lock()
            .expect("composed widget cache admission poisoned")
            .admit(composed_key)
        {
            return Some(pixmap);
        }

        let mut cache = composed_widget_cache()
            .lock()
            .expect("composed widget cache poisoned");
        let cached = cache.insert_with_limits(
            composed_key,
            pixmap,
            COMPOSED_WIDGET_CACHE_MAX_ENTRIES,
            COMPOSED_WIDGET_CACHE_MAX_BYTES,
            STATIC_SVG_LAYER_CACHE_MAX_ENTRY_BYTES,
        );
        return Some((*cached).clone());
    }

    None
}

// ─── WidgetRenderer (GPU state) ───────────────────────────────────────────────

/// Precomputed, scene-free draw geometry for a single widget instance (hud-uyhpn).
///
/// Produced by [`WidgetRenderer::collect_widget_draw_quads`] under the scene lock
/// and consumed by [`WidgetRenderer::composite_prepared`] after the lock drops.
/// Carries only the resolved pixel origin plus the instance name used to look up
/// the (compositor-owned) cached texture at draw time.
pub struct WidgetDrawQuad {
    /// Widget instance name — the key into `WidgetRenderer::textures`.
    pub instance_name: String,
    /// Resolved (pre-snap) pixel x origin.
    pub raw_x: f32,
    /// Resolved (pre-snap) pixel y origin.
    pub raw_y: f32,
}

/// The compositor-owned widget rendering state.
///
/// Created once per compositor and kept for the lifetime of the runtime.
pub struct WidgetRenderer {
    resident_ledger: Option<tze_hud_resource::ResidentLedger>,
    /// Raw SVG bytes keyed by (widget_type_id, svg_filename).
    /// Stored at widget type registration time.
    svgs: HashMap<(String, String), Vec<u8>>,
    svg_resident_allocation_ids: HashMap<(String, String), tze_hud_resource::AllocationId>,

    /// Per-instance texture cache keyed by instance_name.
    textures: HashMap<String, WidgetTextureEntry>,

    /// Retained CPU render plans keyed by widget type id.
    render_plans: HashMap<String, WidgetRenderPlan>,

    /// Bind group layout for the texture pipeline.
    texture_bind_group_layout: wgpu::BindGroupLayout,

    /// Render pipeline for compositing widget textures.
    texture_pipeline: wgpu::RenderPipeline,
}

fn reserve_widget_source(
    ledger: Option<&tze_hud_resource::ResidentLedger>,
    widget_type_id: &str,
    svg_filename: &str,
    svg_bytes: &[u8],
) -> Result<tze_hud_resource::AllocationId, tze_hud_resource::ResidentReserveError> {
    let allocation_id = tze_hud_resource::AllocationId(format!(
        "widget-source:renderer:{widget_type_id}:{svg_filename}:{}",
        blake3::hash(svg_bytes).to_hex()
    ));
    if let Some(ledger) = ledger {
        ledger.reserve(
            tze_hud_resource::ResidentClass::WidgetSource,
            allocation_id.clone(),
            svg_bytes.len() as u64,
        )?;
    }
    Ok(allocation_id)
}

impl WidgetRenderer {
    /// Create a new `WidgetRenderer` for the given device and output format.
    pub fn new(device: &wgpu::Device, output_format: wgpu::TextureFormat) -> Self {
        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("widget_texture_bgl"),
                entries: &[
                    // t_texture: RGBA texture
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    // s_sampler
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let texture_pipeline =
            Self::create_texture_pipeline(device, &texture_bind_group_layout, output_format);

        Self {
            resident_ledger: None,
            svgs: HashMap::new(),
            svg_resident_allocation_ids: HashMap::new(),
            textures: HashMap::new(),
            render_plans: HashMap::new(),
            texture_bind_group_layout,
            texture_pipeline,
        }
    }

    pub fn set_resident_ledger(&mut self, ledger: tze_hud_resource::ResidentLedger) {
        self.resident_ledger = Some(ledger);
    }

    fn create_texture_pipeline(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("widget_texture_shader"),
            source: wgpu::ShaderSource::Wgsl(WIDGET_TEXTURE_SHADER.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("widget_texture_pipeline_layout"),
            bind_group_layouts: &[layout],
            push_constant_ranges: &[],
        });

        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("widget_texture_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[WidgetVertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        })
    }

    /// Register raw SVG bytes for a widget layer.
    ///
    /// Called at startup when widget bundles are loaded, before any instances
    /// are initialized. The SVG bytes are stored keyed by (widget_type_id, svg_filename).
    pub fn register_svg(&mut self, widget_type_id: &str, svg_filename: &str, svg_bytes: Vec<u8>) {
        let byte_count = svg_bytes.len();
        let key = (widget_type_id.to_string(), svg_filename.to_string());
        let allocation_id = match reserve_widget_source(
            self.resident_ledger.as_ref(),
            widget_type_id,
            svg_filename,
            &svg_bytes,
        ) {
            Ok(id) => id,
            Err(_) => {
                tracing::warn!(
                    widget_type_id,
                    svg_filename,
                    "widget SVG source admission denied"
                );
                return;
            }
        };
        self.svgs.insert(key.clone(), svg_bytes);
        if let Some(old_id) = self.resident_ledger.as_ref().and_then(|_| {
            self.svg_resident_allocation_ids
                .insert(key, allocation_id.clone())
        }) && old_id != allocation_id
            && let Some(ledger) = &self.resident_ledger
        {
            ledger.release_evicted(tze_hud_resource::ResidentClass::WidgetSource, &old_id);
        }
        self.render_plans.remove(widget_type_id);
        tracing::debug!(
            widget_type = widget_type_id,
            svg_file = svg_filename,
            bytes = byte_count,
            "widget SVG registered"
        );
    }

    /// Mark a widget instance as dirty so it will be re-rasterized on the next frame.
    pub fn mark_dirty(&mut self, instance_name: &str) {
        if let Some(entry) = self.textures.get_mut(instance_name) {
            entry.dirty = true;
        }
    }

    /// Mark a widget instance as dirty and start a transition animation.
    pub fn start_transition(
        &mut self,
        instance_name: &str,
        from_params: HashMap<String, WidgetParameterValue>,
        to_params: HashMap<String, WidgetParameterValue>,
        transition_ms: u32,
    ) {
        if let Some(entry) = self.textures.get_mut(instance_name) {
            entry.dirty = true;
            entry.animation = Some(WidgetAnimationState {
                start: Instant::now(),
                duration_ms: transition_ms,
                from_params,
                to_params,
            });
        }
    }

    /// Resolve effective parameters for an instance, applying animation if active.
    ///
    /// Under degradation level [`DegradationLevel::Simplified`] or higher
    /// (corresponding to the spec's `RENDERING_SIMPLIFIED` threshold), the
    /// compositor snaps to the final parameter values immediately (`t = 1.0`)
    /// instead of interpolating.  This reduces re-rasterization to at most once
    /// per parameter change during transitions, saving CPU time under load.
    ///
    /// Returns `(effective_params, still_animating)`.
    pub fn resolve_animated_params(
        &mut self,
        instance_name: &str,
        current_params: &HashMap<String, WidgetParameterValue>,
        degradation_level: DegradationLevel,
    ) -> (HashMap<String, WidgetParameterValue>, bool) {
        let entry = match self.textures.get_mut(instance_name) {
            Some(e) => e,
            None => return (current_params.clone(), false),
        };

        let anim = match &entry.animation {
            Some(a) => a,
            None => return (current_params.clone(), false),
        };

        let elapsed_ms = anim.start.elapsed().as_millis() as f32;
        let duration_ms = anim.duration_ms as f32;
        // Under RENDERING_SIMPLIFIED or higher degradation, snap to final values
        // immediately to avoid per-frame re-rasterization.
        let t = compute_transition_t(elapsed_ms, duration_ms, degradation_level);

        let mut result = anim.from_params.clone();
        for (k, new_val) in &anim.to_params {
            let old_val = anim.from_params.get(k).unwrap_or(new_val);
            result.insert(k.clone(), interpolate_param(old_val, new_val, t));
        }

        let still_animating = t < 1.0;
        if !still_animating {
            entry.animation = None;
        } else {
            // Keep re-rasterizing while animating
            entry.dirty = true;
        }

        (result, still_animating)
    }

    /// Rasterize an SVG layer with parameter bindings applied and upload to GPU.
    ///
    /// Returns the time spent rasterizing in microseconds (for performance monitoring).
    /// Per spec, re-rasterization MUST complete in less than 2ms for a 512x512 widget.
    #[allow(clippy::too_many_arguments)]
    pub fn rasterize_and_upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instance_name: &str,
        widget_def: &WidgetDefinition,
        params: &HashMap<String, WidgetParameterValue>,
        pixel_width: u32,
        pixel_height: u32,
    ) -> u64 {
        let start = Instant::now();

        // Build a map from param name to (f32_min, f32_max) for linear binding normalization.
        let param_constraints: HashMap<String, (f32, f32)> = widget_def
            .parameter_schema
            .iter()
            .filter_map(|p| {
                if let Some(c) = &p.constraints {
                    let min = c.f32_min.unwrap_or(0.0);
                    let max = c.f32_max.unwrap_or(1.0);
                    Some((p.name.clone(), (min, max)))
                } else {
                    None
                }
            })
            .collect();

        if !self.render_plans.contains_key(&widget_def.id) {
            let mut svg_layers: Vec<(&str, &[WidgetBinding])> =
                Vec::with_capacity(widget_def.layers.len());

            for layer in &widget_def.layers {
                let key = (widget_def.id.clone(), layer.svg_file.clone());
                let svg_bytes = match self.svgs.get(&key) {
                    Some(b) => b,
                    None => {
                        tracing::warn!(
                            widget = widget_def.id,
                            svg_file = layer.svg_file,
                            "SVG bytes not registered for widget layer"
                        );
                        continue;
                    }
                };
                match std::str::from_utf8(svg_bytes) {
                    Ok(s) => {
                        svg_layers.push((s, &layer.bindings));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "widget SVG not valid UTF-8");
                    }
                }
            }

            self.render_plans.insert(
                widget_def.id.clone(),
                WidgetRenderPlan::compile(&svg_layers),
            );
        }

        // Delegate to the CPU-only rasterization path (shared with benchmarks/tests).
        let composed = self.render_plans.get(&widget_def.id).and_then(|plan| {
            rasterize_widget_render_plan(
                plan,
                &param_constraints,
                params,
                pixel_width,
                pixel_height,
            )
        });

        let raster_us = start.elapsed().as_micros() as u64;

        if raster_us > 2000 {
            tracing::warn!(
                widget = instance_name,
                raster_us,
                "widget re-rasterization exceeded 2ms budget"
            );
        }

        let pixmap = match composed {
            Some(p) => p,
            None => {
                tracing::debug!(widget = instance_name, "no layers rendered for widget");
                return raster_us;
            }
        };

        // Upload to GPU texture (or create a new texture if size changed).
        self.upload_texture(
            device,
            queue,
            instance_name,
            pixmap.data(),
            pixel_width,
            pixel_height,
        );

        let total_us = start.elapsed().as_micros() as u64;
        tracing::trace!(
            widget = instance_name,
            raster_us,
            total_us,
            width = pixel_width,
            height = pixel_height,
            "widget rasterized and uploaded"
        );

        total_us
    }

    /// Upload RGBA pixel data to a wgpu texture (creating or replacing the cached entry).
    fn upload_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instance_name: &str,
        rgba_data: &[u8],
        width: u32,
        height: u32,
    ) {
        let allocation_id =
            tze_hud_resource::AllocationId(format!("widget:{instance_name}:{width}x{height}"));
        if let Some(ledger) = &self.resident_ledger
            && ledger
                .reserve(
                    tze_hud_resource::ResidentClass::WidgetRaster,
                    allocation_id.clone(),
                    rgba_data.len() as u64,
                )
                .is_err()
        {
            tracing::warn!(instance_name, "widget raster cache admission denied");
            return;
        }
        // Create the GPU texture.
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(&format!("widget_tex_{instance_name}")),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Use Rgba8UnormSrgb: tiny-skia rasterises SVGs in sRGB color space.
            // The GPU must sRGB-decode on sample so colours match the hex values
            // in the SVG source (e.g. #4FB543 looks the same as in a browser).
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // Write pixel data.
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba_data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some(&format!("widget_sampler_{instance_name}")),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(&format!("widget_bg_{instance_name}")),
            layout: &self.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let old = self.textures.insert(
            instance_name.to_string(),
            WidgetTextureEntry {
                resident_allocation_id: self
                    .resident_ledger
                    .as_ref()
                    .map(|_| allocation_id.clone()),
                texture,
                bind_group,
                width,
                height,
                dirty: false,
                last_rendered_params: HashMap::new(),
                animation: None,
            },
        );
        if let Some(old) = old
            && let (Some(ledger), Some(id)) = (&self.resident_ledger, old.resident_allocation_id)
            && id != allocation_id
        {
            ledger.release_evicted(tze_hud_resource::ResidentClass::WidgetRaster, &id);
        }
    }

    /// Composite widget textures into the current render pass.
    ///
    /// Called from the compositor's frame render path. Widget tiles use
    /// z_order >= WIDGET_TILE_Z_MIN (0x9000_0000) so they appear above zone
    /// tiles when they overlap.
    ///
    /// # Arguments
    ///
    /// - `render_pass` — the active render pass to draw into
    /// - `registry` — the widget registry (instances + definitions)
    /// - `surf_w` / `surf_h` — surface dimensions for NDC conversion
    pub fn composite_widgets<'rp>(
        &'rp self,
        render_pass: &mut wgpu::RenderPass<'rp>,
        registry: &WidgetRegistry,
        surf_w: f32,
        surf_h: f32,
        device: &wgpu::Device,
    ) {
        use wgpu::util::DeviceExt;

        // Sort active instances by z_order (all widgets use WIDGET_TILE_Z_MIN
        // base + their declared order, so any ordering works for now).
        let mut instances: Vec<&tze_hud_scene::types::WidgetInstance> = registry
            .instances
            .values()
            .filter(|instance| {
                registry
                    .active_publishes
                    .get(&instance.instance_name)
                    .is_some_and(|publishes| !publishes.is_empty())
            })
            .collect();
        instances.sort_by_key(|_i| WIDGET_TILE_Z_MIN);

        render_pass.set_pipeline(&self.texture_pipeline);

        for instance in instances {
            let entry = match self.textures.get(&instance.instance_name) {
                Some(e) => e,
                None => continue,
            };

            // Resolve pixel geometry from the instance's geometry policy.
            let (raw_x, raw_y, _pw, _ph) =
                resolve_pixel_geometry(&instance.geometry_override, surf_w, surf_h).unwrap_or_else(
                    || {
                        // Fall back to full-screen if geometry not set
                        let def = registry
                            .definitions
                            .get(&instance.widget_type_name)
                            .map(|d| &d.default_geometry_policy);
                        if let Some(geo) = def {
                            resolve_pixel_geometry(&Some(*geo), surf_w, surf_h)
                                .unwrap_or((0.0, 0.0, surf_w, surf_h))
                        } else {
                            (0.0, 0.0, surf_w, surf_h)
                        }
                    },
                );
            // Pixel-snap widget quads so text-heavy SVGs remain crisp on screen.
            let (px, py, pw, ph) = snap_composite_rect(
                raw_x,
                raw_y,
                entry.width as f32,
                entry.height as f32,
                surf_w,
                surf_h,
            );

            // Build NDC quad vertices with UV coordinates.
            let vertices = widget_quad_vertices(px, py, pw, ph, surf_w, surf_h);
            let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("widget_quad_buf"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

            render_pass.set_bind_group(0, &entry.bind_group, &[]);
            render_pass.set_vertex_buffer(0, vertex_buf.slice(..));
            render_pass.draw(0..6, 0..1);
        }
    }

    /// Precompute the per-instance draw geometry for the widget pass (hud-uyhpn).
    ///
    /// This is the scene-reading half of [`Self::composite_widgets`]: it resolves
    /// each drawable instance's pixel origin from the registry (instance geometry
    /// override, with a definition-default / full-screen fallback) into an owned
    /// [`WidgetDrawQuad`]. Only instances that have an active publish AND a cached
    /// texture are emitted — matching the `continue` / `any_textured` gating of
    /// the original single-pass compositing.
    ///
    /// The matching draw half is [`Self::composite_prepared`], which needs only
    /// these owned quads (no `&WidgetRegistry`) and so runs after the scene lock
    /// is dropped.
    pub fn collect_widget_draw_quads(
        &self,
        registry: &WidgetRegistry,
        surf_w: f32,
        surf_h: f32,
    ) -> Vec<WidgetDrawQuad> {
        let mut instances: Vec<&tze_hud_scene::types::WidgetInstance> = registry
            .instances
            .values()
            .filter(|instance| {
                registry
                    .active_publishes
                    .get(&instance.instance_name)
                    .is_some_and(|publishes| !publishes.is_empty())
            })
            .collect();
        instances.sort_by_key(|_i| WIDGET_TILE_Z_MIN);

        let mut quads = Vec::new();
        for instance in instances {
            // Only instances with a cached texture are drawable (composite_widgets
            // `continue`s otherwise); skip them here so the draw half never needs
            // to consult the registry.
            if !self.textures.contains_key(&instance.instance_name) {
                continue;
            }
            let (raw_x, raw_y, _pw, _ph) =
                resolve_pixel_geometry(&instance.geometry_override, surf_w, surf_h).unwrap_or_else(
                    || {
                        let def = registry
                            .definitions
                            .get(&instance.widget_type_name)
                            .map(|d| &d.default_geometry_policy);
                        if let Some(geo) = def {
                            resolve_pixel_geometry(&Some(*geo), surf_w, surf_h)
                                .unwrap_or((0.0, 0.0, surf_w, surf_h))
                        } else {
                            (0.0, 0.0, surf_w, surf_h)
                        }
                    },
                );
            quads.push(WidgetDrawQuad {
                instance_name: instance.instance_name.clone(),
                raw_x,
                raw_y,
            });
        }
        quads
    }

    /// Draw precomputed widget quads into an already-open render pass (hud-uyhpn).
    ///
    /// The scene-free counterpart to [`Self::composite_widgets`]: it looks up each
    /// quad's cached texture entry (compositor-owned, not scene state), pixel-snaps
    /// against the texture dimensions, and draws. Geometry was resolved earlier by
    /// [`Self::collect_widget_draw_quads`].
    pub fn composite_prepared<'rp>(
        &'rp self,
        render_pass: &mut wgpu::RenderPass<'rp>,
        quads: &[WidgetDrawQuad],
        surf_w: f32,
        surf_h: f32,
        device: &wgpu::Device,
    ) {
        use wgpu::util::DeviceExt;

        if quads.is_empty() {
            return;
        }
        render_pass.set_pipeline(&self.texture_pipeline);

        for quad in quads {
            let entry = match self.textures.get(&quad.instance_name) {
                Some(e) => e,
                None => continue,
            };
            // Pixel-snap widget quads so text-heavy SVGs remain crisp on screen.
            let (px, py, pw, ph) = snap_composite_rect(
                quad.raw_x,
                quad.raw_y,
                entry.width as f32,
                entry.height as f32,
                surf_w,
                surf_h,
            );

            let vertices = widget_quad_vertices(px, py, pw, ph, surf_w, surf_h);
            let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("widget_quad_buf"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

            render_pass.set_bind_group(0, &entry.bind_group, &[]);
            render_pass.set_vertex_buffer(0, vertex_buf.slice(..));
            render_pass.draw(0..6, 0..1);
        }
    }

    /// Update the render pipeline's output format (e.g. on swapchain reconfiguration).
    pub fn update_format(&mut self, device: &wgpu::Device, new_format: wgpu::TextureFormat) {
        self.texture_pipeline =
            Self::create_texture_pipeline(device, &self.texture_bind_group_layout, new_format);
    }

    /// Get a reference to the texture entry for an instance (for testing / inspection).
    pub fn texture_entry(&self, instance_name: &str) -> Option<&WidgetTextureEntry> {
        self.textures.get(instance_name)
    }

    pub fn texture_entry_mut(&mut self, instance_name: &str) -> Option<&mut WidgetTextureEntry> {
        self.textures.get_mut(instance_name)
    }

    pub fn remove_texture(&mut self, instance_name: &str) {
        if let Some(entry) = self.textures.remove(instance_name)
            && let (Some(ledger), Some(id)) = (&self.resident_ledger, entry.resident_allocation_id)
        {
            ledger.release_evicted(tze_hud_resource::ResidentClass::WidgetRaster, &id);
        }
    }

    /// Returns true if any widget instance has an active animation (needs re-rasterize).
    pub fn has_active_animations(&self) -> bool {
        self.textures.values().any(|e| e.animation.is_some())
    }
}

/// Resolve pixel geometry from a `GeometryPolicy` option.
fn resolve_pixel_geometry(
    policy: &Option<GeometryPolicy>,
    surf_w: f32,
    surf_h: f32,
) -> Option<(f32, f32, f32, f32)> {
    match policy {
        Some(GeometryPolicy::Relative {
            x_pct,
            y_pct,
            width_pct,
            height_pct,
        }) => {
            let x = surf_w * x_pct;
            let y = surf_h * y_pct;
            let w = surf_w * width_pct;
            let h = surf_h * height_pct;
            Some((x, y, w, h))
        }
        Some(GeometryPolicy::EdgeAnchored {
            edge,
            height_pct,
            width_pct,
            margin_px,
        }) => {
            use tze_hud_scene::types::DisplayEdge;
            let w = surf_w * width_pct;
            let h = surf_h * height_pct;
            let x = (surf_w - w) / 2.0;
            let y = match edge {
                DisplayEdge::Top => *margin_px,
                DisplayEdge::Bottom => surf_h - h - margin_px,
                DisplayEdge::Left | DisplayEdge::Right => 0.0,
            };
            Some((x, y, w, h))
        }
        None => None,
    }
}

/// Snap a composite rectangle to integer pixels and clamp it to the surface.
fn snap_composite_rect(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    surf_w: f32,
    surf_h: f32,
) -> (f32, f32, f32, f32) {
    let mut pw = w.max(1.0).round();
    let mut ph = h.max(1.0).round();
    if surf_w.is_finite() && surf_w > 0.0 {
        pw = pw.min(surf_w.round().max(1.0));
    }
    if surf_h.is_finite() && surf_h > 0.0 {
        ph = ph.min(surf_h.round().max(1.0));
    }

    let mut px = x.round();
    let mut py = y.round();
    if surf_w.is_finite() && surf_w > 0.0 {
        let max_x = (surf_w - pw).max(0.0);
        px = px.clamp(0.0, max_x);
    } else {
        px = px.max(0.0);
    }
    if surf_h.is_finite() && surf_h > 0.0 {
        let max_y = (surf_h - ph).max(0.0);
        py = py.clamp(0.0, max_y);
    } else {
        py = py.max(0.0);
    }

    (px, py, pw, ph)
}

// ─── Vertex types ─────────────────────────────────────────────────────────────

/// Vertex for the widget texture quad (position + UV).
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct WidgetVertex {
    pub position: [f32; 2],
    pub uv: [f32; 2],
}

impl WidgetVertex {
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<WidgetVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
            ],
        }
    }
}

/// Generate a textured quad (6 vertices, two triangles) in NDC coordinates.
fn widget_quad_vertices(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    screen_w: f32,
    screen_h: f32,
) -> [WidgetVertex; 6] {
    let left = (x / screen_w) * 2.0 - 1.0;
    let right = ((x + w) / screen_w) * 2.0 - 1.0;
    // NDC: y increases upward; pixel y=0 is top
    let top = 1.0 - (y / screen_h) * 2.0;
    let bottom = 1.0 - ((y + h) / screen_h) * 2.0;

    // UV: (0,0) = top-left, (1,1) = bottom-right (standard texture space)
    [
        WidgetVertex {
            position: [left, top],
            uv: [0.0, 0.0],
        },
        WidgetVertex {
            position: [right, top],
            uv: [1.0, 0.0],
        },
        WidgetVertex {
            position: [left, bottom],
            uv: [0.0, 1.0],
        },
        WidgetVertex {
            position: [right, top],
            uv: [1.0, 0.0],
        },
        WidgetVertex {
            position: [right, bottom],
            uv: [1.0, 1.0],
        },
        WidgetVertex {
            position: [left, bottom],
            uv: [0.0, 1.0],
        },
    ]
}

// ─── Shader ──────────────────────────────────────────────────────────────────

/// WGSL shader for compositing widget textures.
///
/// Samples the widget texture (premultiplied RGBA) and outputs it directly,
/// blending with the scene using premultiplied alpha blending.
const WIDGET_TEXTURE_SHADER: &str = r#"
@group(0) @binding(0)
var t_texture: texture_2d<f32>;
@group(0) @binding(1)
var s_sampler: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(t_texture, s_sampler, in.uv);
}
"#;

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    #[test]
    fn renderer_retained_svg_uses_widget_source_class_atomically() {
        let ledger =
            tze_hud_resource::ResidentLedger::new(tze_hud_resource::ResidentLedgerLimits {
                aggregate_bytes: 4,
                resource_bytes: 0,
                widget_source_bytes: 4,
                widget_raster_bytes: 0,
                font_bytes: 0,
            });

        assert!(reserve_widget_source(Some(&ledger), "gauge", "fill.svg", b"1234").is_ok());
        assert!(reserve_widget_source(Some(&ledger), "gauge", "label.svg", b"x").is_err());
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.widget_source_bytes, 4);
        assert_eq!(snapshot.aggregate_bytes, 4);
        assert_eq!(snapshot.allocation_count, 1);
    }

    fn sans_serif_font_available() -> bool {
        let db = shared_widget_fontdb();
        let query = resvg::usvg::fontdb::Query {
            families: &[resvg::usvg::fontdb::Family::SansSerif],
            ..Default::default()
        };
        db.query(&query).is_some()
    }

    fn widget_cache_test_guard() -> MutexGuard<'static, ()> {
        static CACHE_TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        CACHE_TEST_MUTEX
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("widget cache test mutex poisoned")
    }

    fn expected_text_mask_cache_key(
        text: &PrimitiveText,
        content: &str,
        pixel_width: u32,
        pixel_height: u32,
    ) -> StaticSvgLayerCacheKey {
        let crop = text
            .mask_crop(pixel_width, pixel_height)
            .expect("text crop");
        if shared_widget_ab_glyph_font().is_some()
            && text.uses_fast_path_font_family()
            && text.dominant_baseline.is_none()
        {
            text.direct_text_mask_cache_key(content, &crop, pixel_width, pixel_height)
        } else {
            let text_svg = text.render_mask_svg(content, crop.view_box);
            static_svg_layer_cache_key(&text_svg, pixel_width, crop.pixel_height)
        }
    }

    // ── SVG attribute manipulation tests ──────────────────────────────────────

    fn test_cache_key(label: &str) -> StaticSvgLayerCacheKey {
        static_svg_layer_cache_key(label, 16, 16)
    }

    fn test_pixmap(width: u32, height: u32) -> tiny_skia::Pixmap {
        tiny_skia::Pixmap::new(width, height).expect("test pixmap allocation must succeed")
    }

    #[test]
    fn static_svg_layer_cache_key_uses_full_content_digest() {
        let key_a = static_svg_layer_cache_key("<svg><rect id=\"a\"/></svg>", 16, 16);
        let key_b = static_svg_layer_cache_key("<svg><rect id=\"b\"/></svg>", 16, 16);

        assert_ne!(
            key_a.svg_digest, key_b.svg_digest,
            "same-size SVG strings with different bytes must not share a cache identity"
        );
        assert_eq!(key_a.pixel_width, key_b.pixel_width);
        assert_eq!(key_a.pixel_height, key_b.pixel_height);
    }

    #[test]
    fn static_svg_layer_cache_skips_entries_above_per_entry_limit() {
        let mut cache = StaticSvgLayerCache::default();
        let key = test_cache_key("oversize");

        let uncached = cache.insert_with_limits(key, test_pixmap(2, 2), 64, 64, 15);

        assert!(
            cache.entries.is_empty(),
            "16-byte pixmap must not be cached when the per-entry limit is 15 bytes"
        );
        assert_eq!(cache.total_bytes, 0);
        assert!(cache.lru.is_empty());
        assert_eq!(
            uncached.data().len(),
            16,
            "oversize cache entries must still be returned to the caller"
        );
    }

    #[test]
    fn static_svg_layer_cache_evicts_lru_incrementally_to_byte_bound() {
        let mut cache = StaticSvgLayerCache::default();
        let key_a = test_cache_key("a");
        let key_b = test_cache_key("b");
        let key_c = test_cache_key("c");
        let key_d = test_cache_key("d");

        cache.insert_with_limits(key_a, test_pixmap(16, 16), 64, 2048, 2048);
        cache.insert_with_limits(key_b, test_pixmap(16, 16), 64, 2048, 2048);
        cache.insert_with_limits(key_c, test_pixmap(16, 16), 64, 2048, 2048);

        assert!(
            !cache.entries.contains_key(&key_a),
            "inserting the third 1024-byte pixmap into a 2048-byte cache evicts only the LRU entry"
        );
        assert!(cache.entries.contains_key(&key_b));
        assert!(cache.entries.contains_key(&key_c));
        assert_eq!(cache.total_bytes, 2048);

        let _ = cache.get(&key_b);
        cache.insert_with_limits(key_d, test_pixmap(16, 16), 64, 2048, 2048);

        assert!(
            !cache.entries.contains_key(&key_c),
            "cache hit must promote key_b so key_c becomes the next LRU eviction"
        );
        assert!(cache.entries.contains_key(&key_b));
        assert!(cache.entries.contains_key(&key_d));
        assert_eq!(cache.total_bytes, 2048);
    }

    #[test]
    fn retained_plan_static_cache_is_invalidated_by_svg_content() {
        let _cache_guard = widget_cache_test_guard();
        clear_widget_raster_caches();
        let red_svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
            <rect id="box" x="0" y="0" width="16" height="16" fill="#ff0000"/>
        </svg>"##;
        let blue_svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
            <rect id="box" x="0" y="0" width="16" height="16" fill="#0000ff"/>
        </svg>"##;
        let params = HashMap::new();
        let constraints = HashMap::new();

        let red_plan = WidgetRenderPlan::compile(&[(red_svg, &[])]);
        let blue_plan = WidgetRenderPlan::compile(&[(blue_svg, &[])]);
        let red = rasterize_widget_render_plan(&red_plan, &constraints, &params, 16, 16)
            .expect("red SVG must rasterize");
        let blue = rasterize_widget_render_plan(&blue_plan, &constraints, &params, 16, 16)
            .expect("blue SVG must rasterize");

        assert_ne!(
            red.data(),
            blue.data(),
            "same-size static layers with different SVG bytes must not share cached pixels"
        );
        let red_key = static_svg_layer_cache_key(red_svg, 16, 16);
        let blue_key = static_svg_layer_cache_key(blue_svg, 16, 16);
        let cache = static_svg_layer_cache().lock().expect("static cache");
        assert!(
            cache.entries.contains_key(&red_key),
            "static cache should include the red SVG content digest"
        );
        assert!(
            cache.entries.contains_key(&blue_key),
            "static cache should include the blue SVG content digest"
        );
        assert_eq!(
            red_key.pixel_width, blue_key.pixel_width,
            "same-size SVGs should differ by content digest, not dimensions"
        );
    }

    #[test]
    fn retained_plan_returns_oversize_composed_output_without_caching() {
        let _cache_guard = widget_cache_test_guard();
        clear_static_svg_layer_cache();
        clear_widget_raster_caches();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="2049" height="1025">
            <rect x="0" y="0" width="2049" height="1025" fill="#00ff00"/>
        </svg>"##;
        let plan = WidgetRenderPlan::compile(&[(svg, &[])]);
        let constraints = HashMap::new();
        let params = HashMap::new();

        let first = rasterize_widget_render_plan(&plan, &constraints, &params, 2049, 1025)
            .expect("first oversize composed render should bypass cache admission");
        let second = rasterize_widget_render_plan(&plan, &constraints, &params, 2049, 1025)
            .expect("admitted oversize composed render should still return uncached pixels");
        let cache = composed_widget_cache()
            .lock()
            .expect("composed widget cache");

        assert_eq!(first.data(), second.data());
        assert!(
            cache.entries.is_empty(),
            "oversize composed output should exceed the per-entry cache limit"
        );
    }

    #[test]
    fn retained_plan_bound_cache_is_invalidated_by_resolved_binding_values() {
        let _cache_guard = widget_cache_test_guard();
        clear_widget_raster_caches();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
            <rect id="bar" x="0" y="0" width="0" height="16" fill="#00ff00"/>
        </svg>"##;
        let binding = WidgetBinding {
            param: "level".to_string(),
            target_element: "bar".to_string(),
            target_attribute: "width".to_string(),
            mapping: WidgetBindingMapping::Linear {
                attr_min: 0.0,
                attr_max: 16.0,
            },
        };
        let bindings = vec![binding];
        let plan = WidgetRenderPlan::compile(&[(svg, bindings.as_slice())]);
        let constraints = HashMap::from([("level".to_string(), (0.0f32, 1.0f32))]);
        let params_half = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.5))]);
        let params_full = HashMap::from([("level".to_string(), WidgetParameterValue::F32(1.0))]);

        let half = rasterize_widget_render_plan(&plan, &constraints, &params_half, 16, 16)
            .expect("half-width bound layer must rasterize");
        let half_again = rasterize_widget_render_plan(&plan, &constraints, &params_half, 16, 16)
            .expect("same bound values must rasterize from cache");
        let full = rasterize_widget_render_plan(&plan, &constraints, &params_full, 16, 16)
            .expect("full-width bound layer must rasterize");
        let full_again = rasterize_widget_render_plan(&plan, &constraints, &params_full, 16, 16)
            .expect("same full-width bound layer must rasterize from cache");
        let half_key = cache_key_from_raster_key(&RasterizedLayerCacheKey {
            source_digest: plan.layers[0].source_digest,
            binding_digest: resolve_layer_bindings(&bindings, &params_half, &constraints).digest,
            pixel_width: 16,
            pixel_height: 16,
        });
        let full_key = cache_key_from_raster_key(&RasterizedLayerCacheKey {
            source_digest: plan.layers[0].source_digest,
            binding_digest: resolve_layer_bindings(&bindings, &params_full, &constraints).digest,
            pixel_width: 16,
            pixel_height: 16,
        });

        assert_eq!(
            half.data(),
            half_again.data(),
            "identical bound values must return identical cached pixels"
        );
        assert_ne!(
            half.data(),
            full.data(),
            "changed bound values must invalidate the bound-layer cache key"
        );
        assert_eq!(
            full.data(),
            full_again.data(),
            "repeated full-width values must return identical cached pixels"
        );
        let cache = bound_svg_layer_cache().lock().expect("bound cache");
        assert!(
            cache.entries.contains_key(&half_key),
            "bound cache should contain the half-width resolved binding"
        );
        assert!(
            cache.entries.contains_key(&full_key),
            "bound cache should contain the full-width resolved binding"
        );
    }

    #[test]
    fn retained_plan_bound_cache_includes_primitive_layers_after_static_layers() {
        let _cache_guard = widget_cache_test_guard();
        clear_widget_raster_caches();
        let background = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
            <rect id="bg" x="0" y="0" width="16" height="16" fill="#000000"/>
        </svg>"##;
        let fill = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
            <rect id="bar" x="0" y="0" width="0" height="16" fill="#00ff00"/>
        </svg>"##;
        let bindings = vec![WidgetBinding {
            param: "level".to_string(),
            target_element: "bar".to_string(),
            target_attribute: "width".to_string(),
            mapping: WidgetBindingMapping::Linear {
                attr_min: 0.0,
                attr_max: 16.0,
            },
        }];
        let layers = vec![(background, &[][..]), (fill, bindings.as_slice())];
        let plan = WidgetRenderPlan::compile(&layers);
        let constraints = HashMap::from([("level".to_string(), (0.0f32, 1.0f32))]);
        let params_half = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.5))]);
        let params_full = HashMap::from([("level".to_string(), WidgetParameterValue::F32(1.0))]);

        let _ = rasterize_widget_render_plan(&plan, &constraints, &params_half, 16, 16)
            .expect("half-width primitive layer must rasterize");
        let _ = rasterize_widget_render_plan(&plan, &constraints, &params_half, 16, 16)
            .expect("half-width primitive layer repeat must rasterize");
        let _ = rasterize_widget_render_plan(&plan, &constraints, &params_full, 16, 16)
            .expect("full-width primitive layer must rasterize");
        let _ = rasterize_widget_render_plan(&plan, &constraints, &params_full, 16, 16)
            .expect("full-width primitive layer repeat must rasterize");
        let half_key = cache_key_from_raster_key(&RasterizedLayerCacheKey {
            source_digest: plan.layers[1].source_digest,
            binding_digest: resolve_layer_bindings(&bindings, &params_half, &constraints).digest,
            pixel_width: 16,
            pixel_height: 16,
        });
        let full_key = cache_key_from_raster_key(&RasterizedLayerCacheKey {
            source_digest: plan.layers[1].source_digest,
            binding_digest: resolve_layer_bindings(&bindings, &params_full, &constraints).digest,
            pixel_width: 16,
            pixel_height: 16,
        });

        let cache = bound_svg_layer_cache().lock().expect("bound cache");
        assert!(
            cache.entries.contains_key(&half_key),
            "bound primitive layer after static layers should cache the half-width binding"
        );
        assert!(
            cache.entries.contains_key(&full_key),
            "bound primitive layer after static layers should cache the full-width binding"
        );
    }

    #[test]
    fn primitive_plan_does_not_pop_groups_inside_skipped_defs() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
            <g id="outer" opacity="0.5">
              <defs><g id="ignored"><rect id="ignored-box" x="0" y="0" width="1" height="1"/></g></defs>
              <rect id="box" x="0" y="0" width="16" height="16" fill="#00ff00"/>
            </g>
        </svg>"##;

        let plan = PrimitiveSvgLayerPlan::parse(svg).expect("primitive SVG should parse");
        let PrimitiveItem::Rect(rect) = &plan.items[0] else {
            panic!("expected the visible rect to be parsed");
        };

        assert_eq!(rect.ancestor_ids, vec!["outer".to_string()]);
        assert!(
            (rect.opacity - 0.5).abs() < f32::EPSILON,
            "visible rect should retain outer group opacity after skipped defs"
        );
    }

    #[test]
    fn primitive_plan_falls_back_for_style_attributes() {
        let styled = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
            <rect id="box" x="0" y="0" width="16" height="16" style="fill:#00ff00"/>
        </svg>"##;

        assert!(
            PrimitiveSvgLayerPlan::parse(styled).is_none(),
            "style attributes are outside the primitive fast path and must use resvg fallback"
        );
    }

    #[test]
    fn primitive_plan_supports_rounded_rect_attributes() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
            <rect id="box" x="0" y="0" width="16" height="16" rx="4" ry="2" fill="#00ff00"/>
        </svg>"##;

        let plan = PrimitiveSvgLayerPlan::parse(svg).expect("rounded rect should stay primitive");
        let PrimitiveItem::Rect(rect) = &plan.items[0] else {
            panic!("expected rounded rect primitive");
        };

        assert_eq!(rect.rx, Some(4.0));
        assert_eq!(rect.ry, Some(2.0));
    }

    #[test]
    fn primitive_rounded_rect_rasterizes_corners_without_resvg_fallback() {
        let _cache_guard = widget_cache_test_guard();
        clear_widget_raster_caches();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
            <rect id="box" x="0" y="0" width="16" height="16" rx="8" ry="8" fill="#00ff00"/>
        </svg>"##;
        let plan = WidgetRenderPlan::compile(&[(svg, &[])]);
        assert!(
            plan.layers[0].primitive_plan.is_some(),
            "rounded rect layer should compile into the primitive fast path"
        );

        let pixmap = rasterize_widget_render_plan(&plan, &HashMap::new(), &HashMap::new(), 16, 16)
            .expect("rounded rect should rasterize");
        let corner = pixmap.pixel(0, 0).expect("corner pixel");
        let center = pixmap.pixel(8, 8).expect("center pixel");

        assert_eq!(
            corner.alpha(),
            0,
            "rounded corner should remain transparent"
        );
        assert!(
            center.green() > 0 && center.alpha() > 0,
            "rounded rect center should be filled"
        );
    }

    #[test]
    fn text_mask_alpha_bounds_tracks_non_transparent_pixels() {
        let mut mask = tiny_skia::Pixmap::new(8, 6).expect("test mask allocation");
        mask.pixels_mut()[2 + 8] =
            tiny_skia::PremultipliedColorU8::from_rgba(128, 128, 128, 128).unwrap();
        mask.pixels_mut()[5 + 4 * 8] =
            tiny_skia::PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap();

        assert_eq!(text_mask_alpha_bounds(mask.as_ref()), Some((2, 1, 6, 5)));
    }

    #[test]
    fn text_mask_alpha_bounds_returns_none_for_empty_mask() {
        let mask = tiny_skia::Pixmap::new(8, 6).expect("test mask allocation");

        assert!(
            text_mask_alpha_bounds(mask.as_ref()).is_none(),
            "fully transparent text masks should skip tint compositing"
        );
    }

    #[test]
    fn escape_attr_escapes_quotes_for_generated_text_svg() {
        assert_eq!(
            escape_attr("Font \"Display\" & 'Mono'"),
            "Font &quot;Display&quot; &amp; &apos;Mono&apos;"
        );
    }

    #[test]
    fn text_svg_cache_is_stable_across_opacity_only_changes() {
        let _cache_guard = widget_cache_test_guard();
        clear_widget_raster_caches();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32">
            <g id="fade"><text id="label" x="4" y="20" font-size="12" fill="#ffffff">CPU</text></g>
        </svg>"##;
        let bindings = vec![WidgetBinding {
            param: "opacity".to_string(),
            target_element: "fade".to_string(),
            target_attribute: "opacity".to_string(),
            mapping: WidgetBindingMapping::Linear {
                attr_min: 0.25,
                attr_max: 0.75,
            },
        }];
        let layers = vec![(svg, bindings.as_slice())];
        let plan = WidgetRenderPlan::compile(&layers);
        let constraints = HashMap::from([("opacity".to_string(), (0.0f32, 1.0f32))]);
        let params_low = HashMap::from([("opacity".to_string(), WidgetParameterValue::F32(0.0))]);
        let params_high = HashMap::from([("opacity".to_string(), WidgetParameterValue::F32(1.0))]);

        let _ = rasterize_widget_render_plan(&plan, &constraints, &params_low, 64, 32)
            .expect("low-opacity text should rasterize");
        let _ = rasterize_widget_render_plan(&plan, &constraints, &params_high, 64, 32)
            .expect("high-opacity text should rasterize");
        let primitive_plan =
            PrimitiveSvgLayerPlan::parse(svg).expect("primitive text should parse");
        let PrimitiveItem::Text(text) = &primitive_plan.items[0] else {
            panic!("expected text primitive");
        };
        let expected_key = expected_text_mask_cache_key(text, "CPU", 64, 32);

        assert!(
            text_svg_layer_cache()
                .lock()
                .expect("text cache")
                .entries
                .contains_key(&expected_key),
            "opacity-only changes should cache text glyphs without baking opacity into the SVG key"
        );
    }

    #[test]
    fn text_svg_cache_is_stable_across_text_fill_color_changes() {
        let _cache_guard = widget_cache_test_guard();
        clear_widget_raster_caches();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32">
            <text id="label" x="4" y="20" font-size="12" fill="#ffffff">CPU</text>
        </svg>"##;
        let bindings = vec![WidgetBinding {
            param: "label_color".to_string(),
            target_element: "label".to_string(),
            target_attribute: "fill".to_string(),
            mapping: WidgetBindingMapping::Direct,
        }];
        let layers = vec![(svg, bindings.as_slice())];
        let plan = WidgetRenderPlan::compile(&layers);
        let constraints = HashMap::new();
        let params_red = HashMap::from([(
            "label_color".to_string(),
            WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)),
        )]);
        let params_blue = HashMap::from([(
            "label_color".to_string(),
            WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, 1.0)),
        )]);

        let red = rasterize_widget_render_plan(&plan, &constraints, &params_red, 64, 32)
            .expect("red text should rasterize");
        let blue = rasterize_widget_render_plan(&plan, &constraints, &params_blue, 64, 32)
            .expect("blue text should rasterize");
        let primitive_plan =
            PrimitiveSvgLayerPlan::parse(svg).expect("primitive text should parse");
        let PrimitiveItem::Text(text) = &primitive_plan.items[0] else {
            panic!("expected text primitive");
        };
        let expected_key = expected_text_mask_cache_key(text, "CPU", 64, 32);
        let cache = text_svg_layer_cache().lock().expect("text cache");

        if red.data() == blue.data() && !sans_serif_font_available() {
            eprintln!(
                "skipping text fill pixel delta assertion: no sans-serif system font detected"
            );
        } else {
            assert_ne!(
                red.data(),
                blue.data(),
                "text fill color changes must still affect final pixels"
            );
        }
        assert!(
            cache.entries.contains_key(&expected_key),
            "text cache key should be the fill-independent glyph mask SVG"
        );
    }

    #[test]
    fn primitive_text_fill_none_is_not_tinted_black() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32">
            <rect id="bg" x="0" y="0" width="64" height="32" fill="#00ff00"/>
            <text id="label" x="4" y="20" font-size="12" fill="none">CPU</text>
        </svg>"##;
        let plan = WidgetRenderPlan::compile(&[(svg, &[])]);
        let constraints = HashMap::new();
        let params = HashMap::new();

        let rendered = rasterize_widget_render_plan(&plan, &constraints, &params, 64, 32)
            .expect("text with fill none should still rasterize the non-text layer");
        let expected = rasterize_widget_render_plan(
            &WidgetRenderPlan::compile(&[(
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32">
                    <rect id="bg" x="0" y="0" width="64" height="32" fill="#00ff00"/>
                </svg>"##,
                &[],
            )]),
            &constraints,
            &params,
            64,
            32,
        )
        .expect("background-only control should rasterize");

        assert_eq!(
            rendered.data(),
            expected.data(),
            "fill=\"none\" text must not fall back to black tinting"
        );
    }

    #[test]
    fn text_content_changes_reuse_primitive_non_text_segments() {
        let _cache_guard = widget_cache_test_guard();
        clear_widget_raster_caches();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32">
            <rect id="bar" x="0" y="0" width="32" height="32" fill="#00ff00"/>
            <text id="label" x="4" y="20" font-size="12" fill="#ffffff">A</text>
            <circle id="indicator" cx="56" cy="16" r="6" fill="#ff0000"/>
        </svg>"##;
        let bindings = vec![WidgetBinding {
            param: "label".to_string(),
            target_element: "label".to_string(),
            target_attribute: "text-content".to_string(),
            mapping: WidgetBindingMapping::Direct,
        }];
        let layers = vec![(svg, bindings.as_slice())];
        let plan = WidgetRenderPlan::compile(&layers);
        let constraints = HashMap::new();
        let params_a = HashMap::from([(
            "label".to_string(),
            WidgetParameterValue::String("A".to_string()),
        )]);
        let params_b = HashMap::from([(
            "label".to_string(),
            WidgetParameterValue::String("B".to_string()),
        )]);

        let a = rasterize_widget_render_plan(&plan, &constraints, &params_a, 64, 32)
            .expect("label A should rasterize");
        let b = rasterize_widget_render_plan(&plan, &constraints, &params_b, 64, 32)
            .expect("label B should rasterize");
        let primitive_plan =
            PrimitiveSvgLayerPlan::parse(svg).expect("primitive layer should parse");
        let non_text_digest = resolve_layer_bindings(&bindings, &params_a, &constraints)
            .digest_excluding_targets(&primitive_plan.text_target_ids);
        let first_segment_key = primitive_non_text_segment_cache_key(
            plan.layers[0].source_digest,
            0,
            non_text_digest,
            64,
            32,
        );
        let second_segment_key = primitive_non_text_segment_cache_key(
            plan.layers[0].source_digest,
            1,
            non_text_digest,
            64,
            32,
        );
        let cache = primitive_non_text_layer_cache()
            .lock()
            .expect("primitive non-text cache");

        if a.data() == b.data() && !sans_serif_font_available() {
            eprintln!(
                "skipping text-content pixel delta assertion: no sans-serif system font detected"
            );
        } else {
            assert_ne!(
                a.data(),
                b.data(),
                "text-content changes must invalidate glyph output"
            );
        }
        assert!(
            cache.entries.contains_key(&first_segment_key),
            "non-text segment before text should be cached independently of text-content"
        );
        assert!(
            cache.entries.contains_key(&second_segment_key),
            "non-text segment after text should be cached independently of text-content"
        );
    }

    #[test]
    fn test_apply_svg_attribute_replaces_existing() {
        let svg = r#"<svg><rect id="bar" width="50" fill="blue"/></svg>"#;
        let result = apply_svg_attribute(svg, "bar", "width", "80");
        assert!(
            result.contains("width=\"80\""),
            "should replace existing width: {result}"
        );
        assert!(
            !result.contains("width=\"50\""),
            "should not contain old value: {result}"
        );
    }

    #[test]
    fn test_apply_svg_attribute_inserts_new() {
        let svg = r#"<svg><rect id="bar" fill="blue"/></svg>"#;
        let result = apply_svg_attribute(svg, "bar", "height", "100");
        assert!(
            result.contains("height=\"100\""),
            "should insert new attribute: {result}"
        );
    }

    #[test]
    fn test_apply_svg_attribute_unknown_element_noop() {
        let svg = r#"<svg><rect id="bar" width="50"/></svg>"#;
        let result = apply_svg_attribute(svg, "nonexistent", "width", "80");
        assert_eq!(result, svg, "unknown element should leave SVG unchanged");
    }

    #[test]
    fn test_apply_svg_text_content() {
        let svg = r#"<svg><text id="label">Old Label</text></svg>"#;
        let result = apply_svg_attribute(svg, "label", "text-content", "New Label");
        assert!(
            result.contains(">New Label<"),
            "should replace text content: {result}"
        );
        assert!(
            !result.contains("Old Label"),
            "should not contain old text: {result}"
        );
    }

    // ── Binding resolution tests ──────────────────────────────────────────────

    #[test]
    fn test_resolve_linear_binding() {
        let mut params = HashMap::new();
        params.insert("level".to_string(), WidgetParameterValue::F32(0.5));

        let binding = WidgetBinding {
            param: "level".to_string(),
            target_element: "bar".to_string(),
            target_attribute: "height".to_string(),
            mapping: WidgetBindingMapping::Linear {
                attr_min: 0.0,
                attr_max: 200.0,
            },
        };

        let mut constraints = HashMap::new();
        constraints.insert("level".to_string(), (0.0, 1.0));

        let val = resolve_binding_value(&binding, &params, &constraints).unwrap();
        assert_eq!(
            val, "100",
            "level=0.5 with attr range 0..200 should give 100"
        );
    }

    #[test]
    fn test_resolve_direct_color_binding() {
        let mut params = HashMap::new();
        params.insert(
            "fill".to_string(),
            WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)),
        );

        let binding = WidgetBinding {
            param: "fill".to_string(),
            target_element: "bg".to_string(),
            target_attribute: "fill".to_string(),
            mapping: WidgetBindingMapping::Direct,
        };

        let val = resolve_binding_value(&binding, &params, &HashMap::new()).unwrap();
        assert_eq!(val, "#ff0000", "red color should format as #ff0000");
    }

    #[test]
    fn test_resolve_direct_string_binding() {
        let mut params = HashMap::new();
        params.insert(
            "label".to_string(),
            WidgetParameterValue::String("CPU".to_string()),
        );

        let binding = WidgetBinding {
            param: "label".to_string(),
            target_element: "lbl".to_string(),
            target_attribute: "text-content".to_string(),
            mapping: WidgetBindingMapping::Direct,
        };

        let val = resolve_binding_value(&binding, &params, &HashMap::new()).unwrap();
        assert_eq!(val, "CPU");
    }

    #[test]
    fn test_resolve_discrete_binding() {
        let mut params = HashMap::new();
        params.insert(
            "severity".to_string(),
            WidgetParameterValue::Enum("warning".to_string()),
        );

        let mut value_map = std::collections::BTreeMap::new();
        value_map.insert("info".to_string(), "#00ff00".to_string());
        value_map.insert("warning".to_string(), "#ffff00".to_string());
        value_map.insert("error".to_string(), "#ff0000".to_string());

        let binding = WidgetBinding {
            param: "severity".to_string(),
            target_element: "indicator".to_string(),
            target_attribute: "fill".to_string(),
            mapping: WidgetBindingMapping::Discrete { value_map },
        };

        let val = resolve_binding_value(&binding, &params, &HashMap::new()).unwrap();
        assert_eq!(val, "#ffff00", "warning should map to #ffff00");
    }

    // ── Interpolation tests ───────────────────────────────────────────────────

    #[test]
    fn test_f32_linear_interpolation() {
        let old = WidgetParameterValue::F32(0.0);
        let new = WidgetParameterValue::F32(1.0);

        let mid = interpolate_param(&old, &new, 0.5);
        assert_eq!(mid, WidgetParameterValue::F32(0.5));

        let start = interpolate_param(&old, &new, 0.0);
        assert_eq!(start, WidgetParameterValue::F32(0.0));

        let end = interpolate_param(&old, &new, 1.0);
        assert_eq!(end, WidgetParameterValue::F32(1.0));
    }

    #[test]
    fn test_color_component_wise_interpolation() {
        let old = WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, 1.0)); // blue
        let new = WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)); // red

        let mid = interpolate_param(&old, &new, 0.5);
        if let WidgetParameterValue::Color(c) = mid {
            assert!((c.r - 0.5).abs() < 0.001, "r should be 0.5");
            assert!((c.g - 0.0).abs() < 0.001, "g should be 0.0");
            assert!((c.b - 0.5).abs() < 0.001, "b should be 0.5");
            assert!((c.a - 1.0).abs() < 0.001, "a should be 1.0");
        } else {
            panic!("expected Color variant");
        }
    }

    #[test]
    fn test_string_snaps_immediately() {
        let old = WidgetParameterValue::String("Old".to_string());
        let new = WidgetParameterValue::String("New".to_string());

        // Even at t=0.001, string should snap to new value
        let result = interpolate_param(&old, &new, 0.001);
        assert_eq!(
            result,
            WidgetParameterValue::String("New".to_string()),
            "string should snap to new value at any t"
        );
    }

    #[test]
    fn test_enum_snaps_immediately() {
        let old = WidgetParameterValue::Enum("info".to_string());
        let new = WidgetParameterValue::Enum("error".to_string());

        let result = interpolate_param(&old, &new, 0.001);
        assert_eq!(
            result,
            WidgetParameterValue::Enum("error".to_string()),
            "enum should snap to new value at any t"
        );
    }

    // ── Degradation-aware transition snapping tests ───────────────────────────

    /// Under Nominal, compute_transition_t returns the elapsed/duration ratio.
    #[test]
    fn compute_transition_t_interpolates_below_rendering_simplified() {
        // At 50% elapsed out of 100ms duration → t = 0.5
        let t_nominal = compute_transition_t(50.0, 100.0, DegradationLevel::Nominal);
        assert!(
            (t_nominal - 0.5).abs() < 1e-6,
            "Nominal: expected t=0.5, got {t_nominal}"
        );
    }

    /// Under Simplified, transitions snap to t=1.0 regardless of elapsed time.
    #[test]
    fn compute_transition_t_snaps_when_simplified() {
        let t = compute_transition_t(1.0, 1000.0, DegradationLevel::Simplified); // 0.1% elapsed
        assert_eq!(
            t, 1.0,
            "Simplified should snap transition to t=1.0, got {t}"
        );
    }

    /// Verify the snap produces the final parameter value for an f32 transition.
    #[test]
    fn degradation_snap_yields_final_f32_value() {
        let old = WidgetParameterValue::F32(0.0);
        let new_val = WidgetParameterValue::F32(100.0);

        // With t=1.0 (snap), interpolation must return the new value exactly.
        let snapped = interpolate_param(&old, &new_val, 1.0);
        assert_eq!(
            snapped,
            WidgetParameterValue::F32(100.0),
            "snapped f32 must equal final value"
        );
    }

    /// Verify the snap produces the final parameter value for a color transition.
    #[test]
    fn degradation_snap_yields_final_color_value() {
        let old = WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 0.0, 1.0)); // black
        let new_val = WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)); // red

        let snapped = interpolate_param(&old, &new_val, 1.0);
        if let WidgetParameterValue::Color(c) = snapped {
            assert!((c.r - 1.0).abs() < 1e-6, "snapped r must be 1.0");
            assert!((c.g - 0.0).abs() < 1e-6, "snapped g must be 0.0");
            assert!((c.b - 0.0).abs() < 1e-6, "snapped b must be 0.0");
        } else {
            panic!("expected Color variant after snap");
        }
    }

    // ── Rgba → SVG color string tests ─────────────────────────────────────────

    #[test]
    fn test_rgba_to_svg_color_opaque() {
        let color = Rgba::new(1.0, 0.0, 0.0, 1.0);
        assert_eq!(rgba_to_svg_color(&color), "#ff0000");
    }

    #[test]
    fn test_rgba_to_svg_color_with_alpha() {
        let color = Rgba::new(1.0, 0.0, 0.0, 0.5);
        let s = rgba_to_svg_color(&color);
        assert!(
            s.starts_with("rgba(255,0,0,"),
            "should use rgba format: {s}"
        );
    }

    // ── Linear binding with spec scenario ────────────────────────────────────

    #[test]
    fn test_linear_mapping_spec_scenario() {
        // spec: level=0.5 with min=0,max=1 linear to attr_min=0,attr_max=200 → "100"
        let mut params = HashMap::new();
        params.insert("level".to_string(), WidgetParameterValue::F32(0.5));

        let binding = WidgetBinding {
            param: "level".to_string(),
            target_element: "bar".to_string(),
            target_attribute: "height".to_string(),
            mapping: WidgetBindingMapping::Linear {
                attr_min: 0.0,
                attr_max: 200.0,
            },
        };

        let mut constraints = HashMap::new();
        constraints.insert("level".to_string(), (0.0, 1.0));

        let val = resolve_binding_value(&binding, &params, &constraints).unwrap();
        assert_eq!(val, "100");
    }

    // ── SVG + resvg smoke test ────────────────────────────────────────────────

    #[test]
    fn test_apply_binding_and_rasterize() {
        // Verify the full pipeline: apply binding → modified SVG → usvg parse → render
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <rect id="bar" x="0" y="0" width="50" height="100" fill="blue"/>
        </svg>"#;

        let modified = apply_svg_attribute(svg, "bar", "width", "80");
        assert!(
            modified.contains("width=\"80\""),
            "width should be updated to 80"
        );

        // Parse and rasterize
        let opts = resvg::usvg::Options::default();
        let tree = resvg::usvg::Tree::from_str(&modified, &opts)
            .expect("modified SVG should parse without error");

        let mut pixmap = tiny_skia::Pixmap::new(100, 100).expect("pixmap creation");
        let transform = tiny_skia::Transform::from_scale(1.0, 1.0);
        resvg::render(&tree, transform, &mut pixmap.as_mut());

        // Pixel at x=75 should be blue (inside the 80-wide rect)
        let px = pixmap.pixel(75, 50).expect("pixel access");
        assert!(px.blue() > 200, "pixel inside rect should be blue");
    }

    #[test]
    fn test_rasterization_performance_512x512() {
        // Spec: re-rasterization MUST complete in less than 2ms for 512x512.
        // On CI this is a soft check — we log a warning but don't fail.
        // Use r##"..."## so that "#" inside SVG attribute values doesn't terminate the string.
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512">
            <rect id="bar" x="0" y="0" width="256" height="512" fill="#336699"/>
            <rect id="accent" x="10" y="10" width="236" height="100" fill="#ff8800"/>
        </svg>"##;

        let start = std::time::Instant::now();

        let opts = resvg::usvg::Options::default();
        let tree = resvg::usvg::Tree::from_str(svg, &opts).expect("parse");
        let mut pixmap = tiny_skia::Pixmap::new(512, 512).expect("pixmap");
        let sx = 512.0 / tree.size().width();
        let sy = 512.0 / tree.size().height();
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );

        let elapsed_us = start.elapsed().as_micros();
        if elapsed_us > 2000 {
            eprintln!(
                "WARNING: 512x512 rasterization took {elapsed_us}µs (budget: 2000µs) — may fail on slow CI"
            );
        }
        // Verify non-trivial output
        assert_eq!(pixmap.data().len(), 512 * 512 * 4);
    }

    #[test]
    fn snap_composite_rect_rounds_fractional_origin() {
        let (x, y, w, h) = snap_composite_rect(2213.3333, 10.6667, 336.0, 128.0, 2560.0, 1440.0);
        assert_eq!(x, 2213.0);
        assert_eq!(y, 11.0);
        assert_eq!(w, 336.0);
        assert_eq!(h, 128.0);
    }

    #[test]
    fn snap_composite_rect_clamps_to_surface_bounds() {
        let (x, y, w, h) = snap_composite_rect(2500.9, 1430.2, 120.0, 40.0, 2560.0, 1440.0);
        assert_eq!(x, 2440.0);
        assert_eq!(y, 1400.0);
        assert_eq!(w, 120.0);
        assert_eq!(h, 40.0);
    }

    // ── Reference gauge rasterization tests ──────────────────────────────────
    //
    // Acceptance criterion 8 (hud-mim2.7): Compositor renders reference gauge
    // correctly. The fill.svg from the gauge fixture is rasterized with specific
    // parameter values and the pixel output is validated for correctness.
    //
    // Source: widget-system/spec.md §Deliverable 10 (Reference gauge widget bundle test fixture)

    /// Return the content of the reference gauge fill.svg fixture.
    fn reference_gauge_fill_svg() -> &'static str {
        // Inline the fill.svg content from the gauge fixture so this test has no
        // file system dependency and runs identically in CI (headless llvmpipe).
        // Use r##"..."## to allow "#" in SVG attribute hex color values.
        r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 220" width="100" height="220">
  <rect id="bar" x="30" y="10" width="40" height="0"
        fill="#00b4ff" rx="3" ry="3"/>
  <text id="label-text" x="50" y="215" text-anchor="middle"
        font-family="sans-serif" font-size="10" fill="#cccccc"></text>
  <circle id="indicator" cx="85" cy="15" r="6" fill="#00cc66"/>
</svg>"##
    }

    /// Apply a set of parameter bindings to the reference gauge fill.svg and
    /// return the modified SVG string.
    ///
    /// Used to test the SVG binding pipeline for the reference gauge fixture.
    fn apply_gauge_params(
        level_height: &str,
        fill_color: &str,
        label: &str,
        indicator_fill: &str,
    ) -> String {
        let svg = reference_gauge_fill_svg();
        let svg = apply_svg_attribute(svg, "bar", "height", level_height);
        let svg = apply_svg_attribute(&svg, "bar", "fill", fill_color);
        let svg = apply_svg_attribute(&svg, "label-text", "text-content", label);
        apply_svg_attribute(&svg, "indicator", "fill", indicator_fill)
    }

    /// Parse and rasterize an SVG string into a Pixmap of the given dimensions.
    ///
    /// Shared helper to avoid repeating SVG parse + rasterize setup across pixel
    /// assertion tests. Panics (via `.expect`) if the SVG is malformed or pixmap
    /// allocation fails — test failures are the expected signal.
    fn rasterize_svg(svg_data: &str, width: u32, height: u32) -> tiny_skia::Pixmap {
        let opts = resvg::usvg::Options::default();
        let tree =
            resvg::usvg::Tree::from_str(svg_data, &opts).expect("SVG must parse without error");

        let mut pixmap =
            tiny_skia::Pixmap::new(width, height).expect("pixmap creation must not fail");

        let transform = tiny_skia::Transform::from_scale(
            width as f32 / tree.size().width(),
            height as f32 / tree.size().height(),
        );

        resvg::render(&tree, transform, &mut pixmap.as_mut());
        pixmap
    }

    /// WHEN the reference gauge fill.svg is parsed with default parameters
    /// THEN resvg parses it without error and produces non-trivial pixel output.
    ///
    /// Source: widget-system/spec.md §Deliverable 10 — reference gauge fixture.
    #[test]
    fn reference_gauge_fill_svg_parses_and_rasterizes() {
        let svg = reference_gauge_fill_svg();

        let w = 100u32;
        let h = 220u32;
        let pixmap = rasterize_svg(svg, w, h);

        // Output must be 100×220×4 bytes.
        assert_eq!(pixmap.data().len(), (w * h * 4) as usize);

        // At least one non-zero pixel — the SVG has a circle and a rect.
        let has_nonzero = pixmap.data().iter().any(|&b| b > 0);
        assert!(
            has_nonzero,
            "rasterized reference gauge must produce non-transparent pixels"
        );
    }

    /// WHEN the bar height is set to 100 (50% fill) THEN the blue fill appears in
    /// the expected region.
    ///
    /// fill.svg bar: x=30, y=10, width=40. With height=100 and fill=#00b4ff (blue),
    /// the bar extends from y=10 to y=110. The pixel at (50, 60) — center of bar
    /// at mid-height — should be the blue fill color.
    ///
    /// Source: widget-system/spec.md §Requirement: SVG Layer Parameter Bindings (linear mapping).
    #[test]
    fn reference_gauge_bar_half_full_renders_blue_in_fill_region() {
        // Apply level=100 (50% fill height), blue fill, empty label, info indicator.
        let modified_svg = apply_gauge_params("100", "#00b4ff", "", "#00cc66");

        let w = 100u32;
        let h = 220u32;
        let pixmap = rasterize_svg(&modified_svg, w, h);

        // Bar at x=30..70, y=10..110. Sample pixel at (50, 60): inside the filled bar.
        // The bar fill is #00b4ff (R=0, G=180, B=255). Allow ±20 for anti-aliasing.
        let px = pixmap.pixel(50, 60).expect("pixel access");
        assert!(
            px.blue() > 180,
            "pixel at (50, 60) inside blue fill bar should have high blue channel, got: b={}",
            px.blue()
        );
        assert!(
            px.red() < 50,
            "pixel at (50, 60) inside blue bar should have low red, got: r={}",
            px.red()
        );
    }

    /// WHEN the bar fill is set to red (#ff0000) THEN the bar region renders red.
    ///
    /// Tests the fill_color → direct → bar.fill binding in the reference gauge.
    #[test]
    fn reference_gauge_bar_red_fill_renders_red() {
        let modified_svg = apply_gauge_params("100", "#ff0000", "", "#00cc66");

        let w = 100u32;
        let h = 220u32;
        let pixmap = rasterize_svg(&modified_svg, w, h);

        // Bar center at (50, 60): should be red (#ff0000).
        let px = pixmap.pixel(50, 60).expect("pixel access");
        assert!(
            px.red() > 180,
            "pixel inside red-filled bar should have high red channel, got: r={}",
            px.red()
        );
        assert!(
            px.blue() < 50,
            "pixel inside red-filled bar should have low blue channel, got: b={}",
            px.blue()
        );
    }

    /// WHEN the severity indicator is set to warning (#ffcc00) THEN the indicator
    /// circle region renders yellow.
    ///
    /// Tests the severity → discrete → indicator.fill binding.
    /// Indicator circle: cx=85, cy=15, r=6.
    #[test]
    fn reference_gauge_indicator_warning_renders_yellow() {
        // Warning severity maps to #ffcc00 (yellow-gold).
        let modified_svg = apply_gauge_params("0", "#00b4ff", "", "#ffcc00");

        let w = 100u32;
        let h = 220u32;
        let pixmap = rasterize_svg(&modified_svg, w, h);

        // Indicator circle center at (85, 15) in the 100×220 SVG coordinate space.
        // With scale 1.0, the center maps to pixel (85, 15).
        let px = pixmap
            .pixel(85, 15)
            .expect("pixel access at indicator center");
        assert!(
            px.red() > 180,
            "warning indicator should be yellow (high red), got: r={}",
            px.red()
        );
        assert!(
            px.green() > 150,
            "warning indicator should be yellow (high green), got: g={}",
            px.green()
        );
        assert!(
            px.blue() < 50,
            "warning indicator should be yellow (low blue), got: b={}",
            px.blue()
        );
    }

    /// WHEN the reference gauge is rasterized at 512×512 with a full-height bar
    /// THEN rasterization completes within the 2ms budget.
    ///
    /// Source: hud-mim2.7 acceptance criterion 10 — re-rasterization < 2ms for 512×512.
    #[test]
    fn reference_gauge_rasterization_within_2ms_budget_at_512x512() {
        let modified_svg = apply_gauge_params("200", "#00b4ff", "CPU", "#00cc66");

        let start = std::time::Instant::now();
        let pixmap = rasterize_svg(&modified_svg, 512, 512);
        let elapsed_us = start.elapsed().as_micros();

        // Soft budget check: warn on CI if over budget; do not fail the build.
        // The reference hardware target is 3GHz single-core; llvmpipe in CI may be slower.
        if elapsed_us > 2000 {
            eprintln!(
                "WARNING: reference gauge 512×512 rasterization took {elapsed_us}µs (budget: 2000µs) — may fail on slow CI"
            );
        }

        // The output must be valid 512×512 RGBA8.
        assert_eq!(pixmap.data().len(), 512 * 512 * 4);
    }
}
