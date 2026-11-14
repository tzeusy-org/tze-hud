//! One measured horizontal status pill for text, dividers, icons and drag bounds.
use crate::text::TextItem;
use std::collections::HashMap;
use tze_hud_config::tokens::{CANONICAL_TOKENS, DesignTokenMap, parse_numeric, px_scale};
use tze_hud_scene::types::*;

fn canonical(key: &str) -> &'static str {
    CANONICAL_TOKENS
        .iter()
        .find(|t| t.key == key)
        .unwrap_or_else(|| panic!("status token {key} must be canonical"))
        .default_value
}
pub(super) fn number(tokens: &DesignTokenMap, key: &str) -> f32 {
    tokens
        .get(key)
        .and_then(|v| parse_numeric(v))
        .filter(|v| *v >= 0.0)
        .or_else(|| parse_numeric(canonical(key)))
        .expect("canonical status number")
}
fn color(tokens: &DesignTokenMap, key: &str) -> Rgba {
    tokens
        .get(key)
        .and_then(|v| super::token_colors::parse_hex_color(v))
        .or_else(|| super::token_colors::parse_hex_color(canonical(key)))
        .expect("canonical status color")
}
#[derive(Clone, Debug, PartialEq)]
pub(super) struct StatusStyle {
    pub fill: Rgba,
    pub outline: Rgba,
    pub radius: f32,
    pub hairline: f32,
    pub icon: f32,
    pub gap: f32,
    margin: f32,
    padding: f32,
    height: f32,
    line: f32,
    divider_height: f32,
    entry_gap: f32,
    entry_max: f32,
    font: FontFamily,
    size: f32,
    weight: u16,
    key_color: Rgba,
    value_color: Rgba,
}
impl StatusStyle {
    pub(super) fn resolve(tokens: &DesignTokenMap, policy: &RenderingPolicy) -> Self {
        let scale = px_scale(tokens);
        let dp = |key| number(tokens, key) * scale;
        let mut fill = policy
            .backdrop
            .unwrap_or_else(|| color(tokens, "color.surface.container.low"));
        fill.a *= policy
            .backdrop_opacity
            .unwrap_or_else(|| number(tokens, "opacity.surface"))
            .clamp(0.0, 1.0);
        let size = policy
            .font_size_px
            .unwrap_or_else(|| dp("type.label.m.size"))
            .max(f32::EPSILON);
        let line =
            dp("type.label.m.line_height") * size / dp("type.label.m.size").max(f32::EPSILON);
        Self {
            fill,
            outline: color(tokens, "color.outline.variant"),
            radius: policy.backdrop_radius.unwrap_or_else(|| dp("shape.full")),
            hairline: dp("stroke.border.width"),
            icon: dp("size.status.icon"),
            gap: dp("space.status.key_value"),
            margin: dp("space.status.margin"),
            padding: policy
                .margin_horizontal
                .or(policy.margin_px)
                .unwrap_or_else(|| dp("space.status.horizontal")),
            height: dp("size.status.height")
                .max(line + 2.0 * policy.margin_vertical.or(policy.margin_px).unwrap_or(0.0)),
            line,
            divider_height: dp("size.status.divider"),
            entry_gap: dp("space.m"),
            entry_max: dp("size.status.entry_max"),
            font: policy.font_family.unwrap_or(FontFamily::SystemSansSerif),
            size,
            weight: policy
                .font_weight
                .unwrap_or_else(|| number(tokens, "type.label.m.weight") as u16),
            key_color: color(tokens, "color.on_surface.variant"),
            value_color: policy
                .text_color
                .unwrap_or_else(|| color(tokens, "color.on_surface")),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(super) struct StatusEntry {
    pub key: String,
    pub value: String,
    pub icon: Option<String>,
    pub x: f32,
    pub width: f32,
    key_width: f32,
    value_width: f32,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StatusBarCard {
    pub rect: Rect,
    pub(super) style: StatusStyle,
    pub(super) entries: Vec<StatusEntry>,
}
impl StatusBarCard {
    pub(super) fn text_items(&self, opacity: f32) -> Vec<TextItem> {
        let s = &self.style;
        let y = self.rect.y + (self.rect.height - s.line) * 0.5;
        let mut items = Vec::with_capacity(self.entries.len() * 2);
        for entry in &self.entries {
            let x = self.rect.x
                + entry.x
                + if entry.icon.is_some() {
                    s.icon + s.gap
                } else {
                    0.0
                };
            for (text, x, width, color) in [
                (&entry.key, x, entry.key_width, s.key_color),
                (
                    &entry.value,
                    x + entry.key_width + s.gap,
                    entry.value_width,
                    s.value_color,
                ),
            ] {
                if width <= 0.0 {
                    continue;
                }
                let mut item = TextItem::from_zone_policy(
                    text,
                    x,
                    y,
                    width,
                    s.line,
                    &RenderingPolicy {
                        font_family: Some(s.font),
                        font_size_px: Some(s.size),
                        font_weight: Some(s.weight),
                        text_color: Some(color),
                        margin_px: Some(0.0),
                        ..Default::default()
                    },
                    opacity,
                    1.0,
                );
                item.clip_pixel_y = self.rect.y.max(item.pixel_y);
                item.clip_bounds_height = (self.rect.y + self.rect.height - item.clip_pixel_y)
                    .max(0.0)
                    .min(item.bounds_height);
                item.overflow = TextOverflow::Ellipsis;
                item.line_height_multiplier = s.line / s.size;
                items.push(item);
            }
        }
        items
    }
    pub(super) fn dividers(&self) -> impl Iterator<Item = Rect> + '_ {
        let height = self.style.divider_height.min(self.rect.height);
        self.entries.iter().skip(1).map(move |entry| {
            Rect::new(
                self.rect.x + entry.x - self.style.entry_gap - self.style.hairline,
                self.rect.y + (self.rect.height - height) * 0.5,
                self.style.hairline,
                height,
            )
        })
    }
}
fn entries(publishes: &[ZonePublishRecord], contention: ContentionPolicy) -> Vec<(String, String)> {
    let mut merged = HashMap::new();
    match contention {
        ContentionPolicy::MergeByKey { .. } => {
            for record in publishes {
                if let ZoneContent::StatusBar(payload) = &record.content {
                    merged.extend(payload.entries.clone());
                }
            }
        }
        _ => {
            if let Some(ZoneContent::StatusBar(payload)) = publishes.last().map(|r| &r.content) {
                merged.extend(payload.entries.clone());
            }
        }
    }
    let mut entries: Vec<_> = merged.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    if let ContentionPolicy::MergeByKey { max_keys } = contention {
        entries.truncate(max_keys as usize);
    }
    entries
}
#[derive(Clone, PartialEq)]
struct Inputs {
    entries: Vec<(String, String)>,
    icons: HashMap<String, String>,
    style: StatusStyle,
    zone: Rect,
    display: Rect,
}
pub(super) struct StatusBarLayoutCache {
    entries: HashMap<String, (Inputs, StatusBarCard, bool)>,
    pub(super) shapes: u64,
    max_input_bytes: usize,
}
impl Default for StatusBarLayoutCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            shapes: 0,
            max_input_bytes: crate::overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
        }
    }
}
impl StatusBarLayoutCache {
    pub(super) fn set_max_input_bytes(&mut self, bytes: usize) {
        if self.max_input_bytes != bytes {
            self.max_input_bytes = bytes;
            self.invalidate();
        }
    }
    pub(super) fn invalidate(&mut self) {
        self.entries.clear();
    }
    fn resolve(
        &mut self,
        name: &str,
        input: Inputs,
        fonts: &mut glyphon::FontSystem,
        actual: bool,
    ) -> StatusBarCard {
        if let Some((old, card, real_fonts)) = self.entries.get(name)
            && old == &input
            && (!actual || *real_fonts)
        {
            return card.clone();
        }
        let s = &input.style;
        let margin = s.margin.min(input.zone.width * 0.5);
        let target_height = s.height.max(s.line).max(s.icon);
        let margin_y = s
            .margin
            .min((input.zone.height - target_height).max(0.0) * 0.5);
        // Owner drag translates the maximum region, which may extend beyond
        // the display. Preserve that capacity instead of intersecting it with
        // the display; constrain the measured pill and its final placement.
        let width = (input.zone.width - margin * 2.0)
            .max(0.0)
            .min(input.display.width);
        let padding = s.padding.min(width * 0.5);
        let cap = (width - padding * 2.0).max(0.0);
        let mut shown = Vec::new();
        let mut used = 0.0;
        for (key, value) in &input.entries {
            let icon = input.icons.get(key).cloned();
            let inset = if icon.is_some() { s.icon + s.gap } else { 0.0 };
            let gap = if shown.is_empty() {
                0.0
            } else {
                s.entry_gap * 2.0 + s.hairline
            };
            let available = cap - used - gap;
            if available <= inset + s.gap {
                break;
            }
            let bounded = |text: &str| {
                let mut end = text.len().min(self.max_input_bytes);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                let mut shown = text[..end].replace(['\n', '\r'], " ");
                if end < text.len() {
                    shown.push_str(crate::overflow::ELLIPSIS);
                }
                shown
            };
            let shown_key = bounded(key);
            let shown_value = bounded(value);
            let measure = |fonts: &mut glyphon::FontSystem, text: &str| {
                crate::text::single_line_width(fonts, text, s.size, s.font, s.weight).ceil()
            };
            let key_natural = measure(fonts, &shown_key);
            let value_natural = measure(fonts, &shown_value);
            self.shapes += 2;
            let natural = inset + key_natural + s.gap + value_natural;
            let entry_width = natural.min(s.entry_max).min(available);
            // Keep both parts readable when an entry is constrained. Every
            // text item has its own one-line ellipsis and exact measured box.
            let text_space = (entry_width - inset - s.gap).max(0.0);
            let key_width = key_natural.min(if natural > entry_width {
                text_space * 0.5
            } else {
                text_space
            });
            let value_width = value_natural.min((text_space - key_width).max(0.0));
            shown.push(StatusEntry {
                key: shown_key,
                value: shown_value,
                icon,
                x: padding + used + gap,
                width: entry_width,
                key_width,
                value_width,
            });
            used += gap + entry_width;
            if natural > available {
                break;
            }
        }
        let height = s
            .height
            .max(s.line)
            .max(s.icon)
            .min((input.zone.height - margin_y * 2.0).max(0.0))
            .min(input.display.height);
        let outer = (used + padding * 2.0).min(width);
        let card = StatusBarCard {
            rect: Rect::new(
                (input.zone.x + input.zone.width - margin - outer).clamp(
                    input.display.x,
                    input.display.x + input.display.width - outer,
                ),
                (input.zone.y + input.zone.height - margin_y - height).clamp(
                    input.display.y,
                    input.display.y + input.display.height - height,
                ),
                outer,
                height,
            ),
            style: s.clone(),
            entries: shown,
        };
        self.entries
            .insert(name.to_owned(), (input, card.clone(), actual));
        card
    }
}
impl super::Compositor {
    fn status_bar_display_rect(&self, name: &str) -> Rect {
        self.display_layout
            .zone_display_rect(name)
            .unwrap_or_else(|| Rect::new(0.0, 0.0, self.width as f32, self.height as f32))
    }
    pub(crate) fn status_bar_card(
        &self,
        name: &str,
        publishes: &[ZonePublishRecord],
        contention: ContentionPolicy,
        policy: &RenderingPolicy,
        zone: Rect,
    ) -> Option<StatusBarCard> {
        let values = entries(publishes, contention);
        if values.is_empty() || zone.width <= 0.0 || zone.height <= 0.0 {
            return None;
        }
        let tokens = self.zone_tokens(name);
        let input = Inputs {
            entries: values,
            icons: policy.key_icon_map.clone(),
            style: StatusStyle::resolve(tokens, policy),
            zone,
            display: self.status_bar_display_rect(name),
        };
        if let Some((old, card, _)) = self.status_bar_layouts.borrow().entries.get(name)
            && old == &input
        {
            return Some(card.clone());
        }
        let mut fallback = self.notification_layouts.borrow_mut();
        Some(self.status_bar_layouts.borrow_mut().resolve(
            name,
            input,
            fallback.fallback_fonts(tokens),
            false,
        ))
    }
    pub(super) fn prime_status_bar_layouts(
        &mut self,
        scene: &tze_hud_scene::graph::SceneGraph,
        sw: f32,
        sh: f32,
    ) {
        let mut cache = self.status_bar_layouts.borrow_mut();
        cache
            .entries
            .retain(|name, _| scene.shown_zone_publishes().any(|(shown, _)| shown == name));
        for (name, records) in scene.shown_zone_publishes() {
            let Some(zone) = scene.zone_registry.zones.get(name) else {
                continue;
            };
            let values = entries(records, zone.contention_policy);
            if values.is_empty() {
                cache.entries.remove(name);
                continue;
            }
            let (x, y, w, h) =
                self.zone_geometry(name, &scene.effective_zone_geometry(zone), sw, sh);
            let tokens = tze_hud_config::tokens::for_zone(&self.token_map, name);
            let input = Inputs {
                entries: values,
                icons: zone.rendering_policy.key_icon_map.clone(),
                style: StatusStyle::resolve(&tokens, &zone.rendering_policy),
                zone: Rect::new(x, y, w, h),
                display: self.status_bar_display_rect(name),
            };
            if let Some(rasterizer) = self.text_rasterizer.as_mut() {
                cache.resolve(name, input, rasterizer.font_system_mut(), true);
            }
        }
    }
}
