//! Content-hugging subtitle geometry, shared by text, backdrop and drag bounds.
//! Measure complete publications once with the rasterizer's fonts. Reveal frames
//! reuse those bounds, so the pill does not breathe as words appear.
use crate::text::{TextItem, apply_opacity_to_color, rgba_to_srgb_u8};
use std::sync::Arc;
use tze_hud_config::tokens::{CANONICAL_TOKENS, DesignTokenMap, parse_numeric, px_scale};
use tze_hud_scene::types::*;

fn canonical(key: &str) -> &'static str {
    CANONICAL_TOKENS
        .iter()
        .find(|t| t.key == key)
        .unwrap_or_else(|| panic!("subtitle token {key} must be canonical"))
        .default_value
}
fn number(tokens: &DesignTokenMap, key: &str) -> f32 {
    tokens
        .get(key)
        .and_then(|v| parse_numeric(v))
        .filter(|v| *v >= 0.0)
        .or_else(|| parse_numeric(canonical(key)))
        .expect("canonical subtitle number")
}
fn color(tokens: &DesignTokenMap, key: &str) -> Rgba {
    tokens
        .get(key)
        .and_then(|v| super::token_colors::parse_hex_color(v))
        .or_else(|| super::token_colors::parse_hex_color(canonical(key)))
        .expect("canonical subtitle color")
}
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SubtitleStyle {
    pub fill: Rgba,
    pub outline: Rgba,
    pub radius: f32,
    pub hairline: f32,
    padding_h: f32,
    padding_v: f32,
    max_width: f32,
    line_height: f32,
    policy: RenderingPolicy,
}
impl SubtitleStyle {
    fn resolve(tokens: &DesignTokenMap, policy: &RenderingPolicy) -> Self {
        let scale = px_scale(tokens);
        let mut fill = color(tokens, "color.surface");
        fill.a *= number(tokens, "opacity.scrim").clamp(0.0, 1.0)
            * number(tokens, "opacity.subtitle").clamp(0.0, 1.0);
        Self {
            fill,
            outline: color(tokens, "color.outline.variant"),
            radius: number(tokens, "shape.l") * scale,
            hairline: number(tokens, "stroke.border.width") * scale,
            padding_h: policy
                .margin_horizontal
                .or(policy.margin_px)
                .unwrap_or_else(|| number(tokens, "space.subtitle.horizontal") * scale),
            padding_v: policy
                .margin_vertical
                .or(policy.margin_px)
                .unwrap_or_else(|| number(tokens, "space.subtitle.vertical") * scale),
            max_width: number(tokens, "size.subtitle.max_width") * scale,
            line_height: number(tokens, "type.caption.display.line_height") * scale,
            policy: policy.clone(),
        }
    }
    /// Paint-only changes do not invalidate shaped extents.
    fn same_metrics(&self, other: &Self) -> bool {
        self.padding_h == other.padding_h
            && self.padding_v == other.padding_v
            && self.max_width == other.max_width
            && self.line_height == other.line_height
            && self.policy.font_size_px == other.policy.font_size_px
            && self.policy.font_family == other.policy.font_family
            && self.policy.font_weight == other.policy.font_weight
    }
    fn text_item(
        &self,
        text: &str,
        bounds: Rect,
        opacity: f32,
        tokens: &DesignTokenMap,
    ) -> TextItem {
        let scale = px_scale(tokens);
        let size = self
            .policy
            .font_size_px
            .unwrap_or_else(|| number(tokens, "type.caption.display.size") * scale)
            .max(f32::EPSILON);
        let outline_width = self
            .policy
            .outline_width
            .unwrap_or_else(|| number(tokens, "stroke.subtitle.text") * scale);
        let outline = self
            .policy
            .outline_color
            .unwrap_or_else(|| color(tokens, "color.subtitle.outline"));
        TextItem {
            text: Arc::from(text),
            pixel_x: bounds.x,
            pixel_y: bounds.y,
            bounds_width: bounds.width,
            bounds_height: bounds.height,
            clip_pixel_x: bounds.x,
            clip_pixel_y: bounds.y,
            clip_bounds_width: bounds.width,
            clip_bounds_height: bounds.height,
            font_size_px: size,
            font_family: self
                .policy
                .font_family
                .unwrap_or(FontFamily::SystemSansSerif),
            font_weight: self
                .policy
                .font_weight
                .unwrap_or_else(|| number(tokens, "type.caption.display.weight") as u16)
                .clamp(100, 900),
            color: apply_opacity_to_color(
                rgba_to_srgb_u8(
                    self.policy
                        .text_color
                        .unwrap_or_else(|| color(tokens, "color.on_surface")),
                ),
                opacity,
            ),
            alignment: self.policy.text_align.unwrap_or(TextAlign::Center),
            overflow: self.policy.overflow.unwrap_or(TextOverflow::Ellipsis),
            outline_color: (outline_width > 0.0)
                .then(|| apply_opacity_to_color(rgba_to_srgb_u8(outline), opacity)),
            outline_width: (outline_width > 0.0).then_some(outline_width),
            opacity,
            color_runs: Box::default(),
            styled_runs: Box::default(),
            line_height_multiplier: self.line_height.max(f32::EPSILON) / size,
            viewport: if self.policy.stream_tail_anchored == Some(true) {
                crate::overflow::TruncationViewport::TailAnchored
            } else {
                crate::overflow::TruncationViewport::HeadAnchored
            },
        }
    }
}
#[derive(Clone, Debug)]
pub(super) struct SubtitleLayout {
    pub rect: Rect,
    pub content: Rect,
    pub style: SubtitleStyle,
}
impl SubtitleLayout {
    pub(super) fn text_item(&self, text: &str, opacity: f32, tokens: &DesignTokenMap) -> TextItem {
        self.style.text_item(text, self.content, opacity, tokens)
    }
}
#[derive(PartialEq)]
struct Inputs {
    text: String,
    zone: Rect,
    style: SubtitleStyle,
}
#[derive(Default)]
pub(super) struct SubtitleLayoutCache {
    entry: Option<(Inputs, SubtitleLayout, bool)>,
    pub(super) shapes: u64,
}
impl SubtitleLayoutCache {
    pub(super) fn invalidate(&mut self) {
        self.entry = None;
    }
    fn resolve(
        &mut self,
        text: &str,
        zone: Rect,
        tokens: &DesignTokenMap,
        policy: &RenderingPolicy,
        fonts: &mut glyphon::FontSystem,
        actual: bool,
    ) -> SubtitleLayout {
        let style = SubtitleStyle::resolve(tokens, policy);
        if let Some((previous, layout, from_actual)) = &self.entry
            && previous.text == text
            && previous.zone == zone
            && previous.style.same_metrics(&style)
            && (!actual || *from_actual)
        {
            let mut layout = layout.clone();
            layout.style = style;
            return layout;
        }
        let outer_width = if style.max_width > 0.0 {
            zone.width.min(style.max_width)
        } else {
            zone.width
        };
        let pad_h = style.padding_h.min(outer_width.max(0.0) * 0.5);
        let pad_v = style.padding_v.min(zone.height.max(0.0) * 0.5);
        let cap = (outer_width - pad_h * 2.0).max(f32::EPSILON);
        let item = style.text_item(text, Rect::new(0.0, 0.0, cap, zone.height), 1.0, tokens);
        let natural = crate::text::single_line_width(
            fonts,
            text,
            item.font_size_px,
            item.font_family,
            item.font_weight,
        );
        let width = natural.ceil().min(cap).max(f32::EPSILON);
        let line = style.line_height.max(f32::EPSILON);
        let mut buffer =
            glyphon::Buffer::new(fonts, glyphon::Metrics::new(item.font_size_px, line));
        buffer.set_size(fonts, Some(width), None);
        buffer.set_wrap(fonts, crate::text::WRAPPED_TEXT_WRAP);
        let family = match item.font_family {
            FontFamily::SystemSansSerif => glyphon::Family::SansSerif,
            FontFamily::SystemMonospace => glyphon::Family::Monospace,
            FontFamily::SystemSerif => glyphon::Family::Serif,
        };
        buffer.set_text(
            fonts,
            text,
            glyphon::Attrs::new()
                .family(family)
                .weight(glyphon::Weight(item.font_weight)),
            glyphon::Shaping::Advanced,
        );
        buffer.shape_until_scroll(fonts, false);
        self.shapes += 2;
        let available = (zone.height - pad_v * 2.0).max(0.0);
        let height = (buffer.layout_runs().count() as f32 * line)
            .min((available / line).floor().max(1.0) * line)
            .min(available);
        let rect = Rect::new(
            zone.x + (zone.width - width - pad_h * 2.0) * 0.5,
            zone.y,
            width + pad_h * 2.0,
            height + pad_v * 2.0,
        );
        let layout = SubtitleLayout {
            rect,
            content: Rect::new(rect.x + pad_h, rect.y + pad_v, width, height),
            style,
        };
        self.entry = Some((
            Inputs {
                text: text.to_owned(),
                zone,
                style: layout.style.clone(),
            },
            layout.clone(),
            actual,
        ));
        layout
    }
}
impl super::Compositor {
    pub(super) fn subtitle_layout(
        &self,
        name: &str,
        text: &str,
        policy: &RenderingPolicy,
        zone: Rect,
    ) -> Option<SubtitleLayout> {
        if name != "subtitle" || text.is_empty() || zone.width <= 0.0 || zone.height <= 0.0 {
            return None;
        }
        let tokens = self.zone_tokens(name);
        let style = SubtitleStyle::resolve(tokens, policy);
        if let Some((previous, layout, _)) = &self.subtitle_layouts.borrow().entry
            && previous.text == text
            && previous.zone == zone
            && previous.style.same_metrics(&style)
        {
            let mut layout = layout.clone();
            layout.style = style;
            return Some(layout);
        }
        let mut fonts = self.notification_layouts.borrow_mut();
        Some(self.subtitle_layouts.borrow_mut().resolve(
            text,
            zone,
            tokens,
            policy,
            fonts.fallback_fonts(tokens),
            false,
        ))
    }
    pub(super) fn prime_subtitle_layout(
        &mut self,
        scene: &tze_hud_scene::graph::SceneGraph,
        sw: f32,
        sh: f32,
    ) {
        let Some(zone) = scene.zone_registry.zones.get("subtitle") else {
            self.subtitle_layouts.borrow_mut().invalidate();
            return;
        };
        let record = scene
            .shown_zone_publishes()
            .find(|(name, _)| name.as_str() == "subtitle")
            .and_then(|(_, records)| records.last());
        let Some(ZoneContent::StreamText(text)) = record.map(|r| &r.content) else {
            self.subtitle_layouts.borrow_mut().invalidate();
            return;
        };
        if text.is_empty() {
            self.subtitle_layouts.borrow_mut().invalidate();
            return;
        }
        let (x, y, w, h) =
            self.zone_geometry("subtitle", &scene.effective_zone_geometry(zone), sw, sh);
        let tokens = tze_hud_config::tokens::for_zone(&self.token_map, "subtitle");
        if let Some(rasterizer) = self.text_rasterizer.as_mut() {
            self.subtitle_layouts.borrow_mut().resolve(
                text,
                Rect::new(x, y, w, h),
                &tokens,
                &zone.rendering_policy,
                rasterizer.font_system_mut(),
                true,
            );
        }
    }
}

impl super::Compositor {
    pub(super) fn subtitle_backdrop_cmd(
        &self,
        pill: &SubtitleLayout,
        opacity: f32,
    ) -> crate::pipeline::RoundedRectDrawCmd {
        let mut fill = pill.style.fill;
        fill.a *= opacity;
        let mut outline = pill.style.outline;
        outline.a *= opacity;
        crate::pipeline::RoundedRectDrawCmd {
            x: pill.rect.x,
            y: pill.rect.y,
            width: pill.rect.width,
            height: pill.rect.height,
            radius: pill
                .style
                .radius
                .min(pill.rect.width * 0.5)
                .min(pill.rect.height * 0.5),
            color: self.gpu_color(fill),
            border: (pill.style.hairline > 0.0).then(|| crate::pipeline::RoundedRectBorder {
                width: pill.style.hairline,
                color: self.gpu_color(outline),
            }),
            clip: None,
        }
    }
}
