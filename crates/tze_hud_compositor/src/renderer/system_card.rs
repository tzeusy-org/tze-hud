//! Owner-only pairing cards and toasts, above scene content and safe mode.
//!
//! Semantic surface/type roles and logical-dp geometry come from canonical
//! tokens, the selected theme and owner overrides. The runtime owns content,
//! expiry and firewall verdicts; none of this model enters an agent surface.
//! The retained layout measures with the renderer's actual fonts only when
//! content, tokens, fonts or surface dimensions change.

use std::collections::HashMap;

use glyphon::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Weight};
use tze_hud_config::tokens::{CANONICAL_TOKENS, parse_numeric, px_scale};
use tze_hud_scene::types::{FontFamily, RenderingPolicy, TextAlign};

use super::control_panel::{PanelRect, PanelShape};
use super::token_colors::parse_hex_color;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemCardKind {
    /// Lines are producer-owned slots: code, pairing URL, validity, then
    /// optional warnings emitted only for an actual blocked firewall verdict.
    Pairing,
    /// Bottom-centre toast; every line uses the body role.
    Toast,
}

/// Platform-neutral, owner-only content. Expiry remains the runtime's concern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemCardModel {
    pub kind: SystemCardKind,
    pub title: String,
    pub lines: Vec<String>,
    /// Optional local host identity supplied by the pairing producer.
    pub host: Option<String>,
}

pub(super) struct SystemCardLayout {
    pub(super) shapes: Vec<PanelShape>,
    pub(super) texts: Vec<TextItem>,
}

fn canonical(key: &str) -> &'static str {
    CANONICAL_TOKENS
        .iter()
        .find(|t| t.key == key)
        .unwrap_or_else(|| panic!("system card token {key} must be canonical"))
        .default_value
}
fn number(tokens: &HashMap<String, String>, key: &str) -> f32 {
    tokens
        .get(key)
        .and_then(|v| parse_numeric(v))
        .filter(|v| *v >= 0.0)
        .or_else(|| parse_numeric(canonical(key)))
        .expect("canonical system card number")
}
fn color(tokens: &HashMap<String, String>, key: &str) -> Rgba {
    tokens
        .get(key)
        .and_then(|v| parse_hex_color(v))
        .or_else(|| parse_hex_color(canonical(key)))
        .expect("canonical system card color")
}

#[derive(Clone, Copy)]
struct TypeRole {
    size: f32,
    line: f32,
    weight: u16,
    family: FontFamily,
}
impl TypeRole {
    fn resolve(tokens: &HashMap<String, String>, role: &str, mono: bool) -> Self {
        let scale = px_scale(tokens);
        Self {
            size: (number(tokens, &format!("type.{role}.size")) * scale).max(f32::EPSILON),
            line: (number(tokens, &format!("type.{role}.line_height")) * scale).max(f32::EPSILON),
            weight: number(tokens, &format!("type.{role}.weight")) as u16,
            family: if mono {
                FontFamily::SystemMonospace
            } else {
                FontFamily::SystemSansSerif
            },
        }
    }
    fn item(self, text: &str, rect: PanelRect, tint: Rgba, align: TextAlign) -> TextItem {
        let policy = RenderingPolicy {
            font_size_px: Some(self.size),
            font_weight: Some(self.weight),
            font_family: Some(self.family),
            text_color: Some(tint),
            text_align: Some(align),
            margin_px: Some(0.0),
            ..Default::default()
        };
        let mut item =
            TextItem::from_zone_policy(text, rect.x, rect.y, rect.w, rect.h, &policy, 1.0, 1.0);
        item.font_size_px = self.size;
        item.line_height_multiplier = self.line / self.size;
        item
    }
    fn height(self, fonts: &mut FontSystem, text: &str, width: f32) -> f32 {
        let mut buffer = Buffer::new(fonts, Metrics::new(self.size, self.line));
        buffer.set_size(fonts, Some(width.max(f32::EPSILON)), None);
        buffer.set_wrap(fonts, crate::text::WRAPPED_TEXT_WRAP);
        let family = match self.family {
            FontFamily::SystemMonospace => Family::Monospace,
            FontFamily::SystemSerif => Family::Serif,
            FontFamily::SystemSansSerif => Family::SansSerif,
        };
        buffer.set_text(
            fonts,
            text,
            Attrs::new()
                .family(family)
                .weight(Weight(self.weight.clamp(100, 900))),
            Shaping::Advanced,
        );
        buffer.shape_until_scroll(fonts, false);
        buffer.layout_runs().count().max(1) as f32 * self.line
    }
}

pub(super) fn system_card_layout(
    tokens: &HashMap<String, String>,
    card: &SystemCardModel,
    w: f32,
    h: f32,
    fonts: &mut FontSystem,
) -> SystemCardLayout {
    let dp = |key| number(tokens, key) * px_scale(tokens);
    let margin = dp("space.l").min(w / 2.0).min(h / 2.0);
    let width = dp("size.system_card.width").min((w - margin * 2.0).max(f32::EPSILON));
    let padding = dp("space.system_card.padding").min(width / 2.0);
    let inner = (width - padding * 2.0).max(f32::EPSILON);
    let gap = dp("space.system_card.group_gap");
    let title = TypeRole::resolve(tokens, "headline.s", false);
    let body = TypeRole::resolve(tokens, "body.m", false);
    let meta = TypeRole::resolve(tokens, "system_card.metadata", false);
    let value = TypeRole {
        size: TypeRole::resolve(tokens, "label.m", true).size,
        family: FontFamily::SystemMonospace,
        ..meta
    };
    let mut code = TypeRole::resolve(tokens, "code.display", true);
    let primary = color(tokens, "color.on_surface");
    let muted = color(tokens, "color.on_surface.variant");
    let mut texts = Vec::new();
    let mut shapes = Vec::new();
    let mut y = padding;
    let mut push =
        |text: &str, role: TypeRole, tint: Rgba, x: f32, at: f32, available: f32, align| {
            let height = role.height(fonts, text, available);
            texts.push(role.item(
                text,
                PanelRect {
                    x,
                    y: at,
                    w: available,
                    h: height,
                },
                tint,
                align,
            ));
            height
        };
    y += push(
        &card.title,
        title,
        primary,
        padding,
        y,
        inner,
        TextAlign::Start,
    );
    match card.kind {
        SystemCardKind::Pairing => {
            y += dp("space.xs");
            let intro = match card.lines.get(2) {
                Some(validity) => {
                    format!("Give this code to the agent. It works once and is {validity}.")
                }
                None => "Give this code to the agent. It works once.".to_owned(),
            };
            y += push(&intro, body, muted, padding, y, inner, TextAlign::Start);
            y += gap;
            if let Some(digits) = card.lines.first() {
                let inset = dp("space.l").min(inner / 2.0);
                let measure = (inner - inset * 2.0).max(f32::EPSILON);
                // The code must remain on one line, even on a narrow display.
                let natural = crate::text::single_line_width(
                    fonts,
                    digits,
                    code.size,
                    code.family,
                    code.weight,
                );
                let fit = (measure / natural.max(f32::EPSILON)).min(1.0);
                code.size *= fit;
                code.line *= fit;
                let well_height = code.line + inset * 2.0;
                shapes.push(PanelShape {
                    rect: PanelRect {
                        x: padding,
                        y,
                        w: inner,
                        h: well_height,
                    },
                    radius: dp("shape.l"),
                    fill: color(tokens, "color.surface.container"),
                    border: None,
                    clip: None,
                });
                texts.push(code.item(
                    digits,
                    PanelRect {
                        x: padding + inset,
                        y: y + inset,
                        w: measure,
                        h: code.line,
                    },
                    primary,
                    TextAlign::Center,
                ));
                y += well_height + gap;
            }
            let key_width = dp("size.system_card.metadata_key").min(inner / 2.0);
            let value_width = (inner - key_width).max(f32::EPSILON);
            let mut first_row = true;
            for (label, text) in [
                ("MCP", card.lines.get(1).map(String::as_str)),
                ("Host", card.host.as_deref()),
            ] {
                if let Some(text) = text.filter(|t| !t.is_empty()) {
                    if !first_row {
                        y += dp("space.system_card.row_gap");
                    }
                    first_row = false;
                    let row_h = meta.line.max(value.height(fonts, text, value_width));
                    texts.push(meta.item(
                        label,
                        PanelRect {
                            x: padding,
                            y,
                            w: key_width,
                            h: row_h,
                        },
                        muted,
                        TextAlign::Start,
                    ));
                    texts.push(value.item(
                        text,
                        PanelRect {
                            x: padding + key_width,
                            y,
                            w: value_width,
                            h: row_h,
                        },
                        primary,
                        TextAlign::Start,
                    ));
                    y += row_h;
                }
            }
            for warning in card.lines.iter().skip(3) {
                y += gap;
                let height = body.height(fonts, warning, inner);
                texts.push(body.item(
                    warning,
                    PanelRect {
                        x: padding,
                        y,
                        w: inner,
                        h: height,
                    },
                    color(tokens, "color.caution"),
                    TextAlign::Start,
                ));
                y += height;
            }
        }
        SystemCardKind::Toast => {
            for line in &card.lines {
                y += dp("space.xs");
                y += push(line, body, muted, padding, y, inner, TextAlign::Start);
            }
        }
    }
    let height = y + padding;
    let available_height = (h - margin * 2.0).max(f32::EPSILON);
    // Fit the complete owner-only message, including warnings, to a constrained
    // viewport. Scale all geometry and type together; never clip the code/fix.
    let fit = (available_height / height.max(f32::EPSILON)).min(1.0);
    let x = (w - width * fit) / 2.0;
    let top = match card.kind {
        SystemCardKind::Pairing => (h - height * fit) / 2.0,
        SystemCardKind::Toast => (h - height * fit - dp("space.xxl").min(margin * 2.0)).max(0.0),
    };
    let mut fill = color(tokens, "color.surface.container.highest");
    fill.a *= number(tokens, "opacity.system_card").clamp(0.0, 1.0);
    shapes.insert(
        0,
        PanelShape {
            rect: PanelRect {
                x: 0.0,
                y: 0.0,
                w: width,
                h: height,
            },
            radius: dp("shape.xl"),
            fill,
            border: Some((
                dp("stroke.border.width"),
                color(tokens, "color.outline.variant"),
            )),
            clip: None,
        },
    );
    for shape in &mut shapes {
        shape.rect.x = x + shape.rect.x * fit;
        shape.rect.y = top + shape.rect.y * fit;
        shape.rect.w *= fit;
        shape.rect.h *= fit;
        shape.radius *= fit;
        if let Some((stroke, _)) = &mut shape.border {
            *stroke *= fit;
        }
    }
    for item in &mut texts {
        item.pixel_x = x + item.pixel_x * fit;
        item.pixel_y = top + item.pixel_y * fit;
        item.bounds_width *= fit;
        item.bounds_height *= fit;
        item.font_size_px *= fit;
        item.clip_pixel_x = item.pixel_x;
        item.clip_pixel_y = item.pixel_y;
        item.clip_bounds_width = item.bounds_width;
        item.clip_bounds_height = item.bounds_height;
    }
    SystemCardLayout { shapes, texts }
}

impl Compositor {
    /// Mirror the runtime's current card and invalidate only on a real change.
    pub fn set_system_card(&mut self, card: Option<SystemCardModel>) -> bool {
        if self.system_card == card {
            return false;
        }
        self.system_card = card;
        self.system_card_layout = None;
        true
    }
    pub fn set_debug_zone_tints(&mut self, on: bool) -> bool {
        std::mem::replace(&mut self.debug_zone_tints, on) != on
    }
    pub(super) fn ensure_system_card_layout(
        &mut self,
        sw: f32,
        sh: f32,
    ) -> Option<&SystemCardLayout> {
        let card = self.system_card.as_ref()?;
        let key = (sw.to_bits(), sh.to_bits());
        if self
            .system_card_layout
            .as_ref()
            .is_none_or(|(old, _)| *old != key)
        {
            let fonts = self.text_rasterizer.as_mut()?.font_system_mut();
            let layout = system_card_layout(&self.token_map, card, sw, sh, fonts);
            self.system_card_layout = Some((key, layout));
        }
        self.system_card_layout.as_ref().map(|(_, layout)| layout)
    }
    pub(super) fn system_card_shapes(&mut self, sw: f32, sh: f32) -> Vec<PanelShape> {
        self.ensure_system_card_layout(sw, sh)
            .map(|layout| layout.shapes.clone())
            .unwrap_or_default()
    }
    pub(super) fn system_card_text_items(&mut self, sw: f32, sh: f32) -> Vec<TextItem> {
        self.ensure_system_card_layout(sw, sh)
            .map(|layout| layout.texts.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pairing() -> SystemCardModel {
        SystemCardModel {
            kind: SystemCardKind::Pairing,
            title: "Pair an agent".into(),
            lines: vec![
                "482913".into(),
                "http://192.0.2.10:9090/pair".into(),
                "valid for 5 minutes".into(),
            ],
            host: Some("hud-example".into()),
        }
    }
    fn layout(
        tokens: &HashMap<String, String>,
        card: &SystemCardModel,
        w: f32,
        h: f32,
    ) -> SystemCardLayout {
        system_card_layout(tokens, card, w, h, &mut crate::fonts::bundled_font_system())
    }
    #[test]
    fn system_card_pairing_layout_is_centred_with_code_line() {
        let layout = layout(&HashMap::new(), &pairing(), 1920.0, 1080.0);
        let bg = layout.shapes[0].rect;
        assert_eq!(bg.w, 440.0);
        assert_eq!(bg.x + bg.w / 2.0, 960.0);
        assert!((bg.y + bg.h / 2.0 - 540.0).abs() < 0.01);
        assert_eq!(
            layout.shapes.len(),
            2,
            "rounded outer card and code well, no accent bar"
        );
        assert_eq!(layout.shapes[0].radius, 28.0);
        assert_eq!(layout.shapes[0].border.unwrap().0, 1.0);
        let code = layout.texts.iter().find(|t| &*t.text == "482913").unwrap();
        assert_eq!(code.font_size_px, 40.0);
        assert_eq!(code.font_family, FontFamily::SystemMonospace);
        assert_eq!(code.line_height_multiplier, 48.0 / 40.0);
        assert_eq!(code.font_weight, 500);
        for t in &layout.texts {
            assert!(t.pixel_x >= bg.x && t.pixel_x + t.bounds_width <= bg.x + bg.w + 0.01);
            assert!(t.pixel_y >= bg.y && t.pixel_y + t.bounds_height <= bg.y + bg.h + 0.01);
        }
    }
    #[test]
    fn system_card_toast_anchors_bottom_centre() {
        let toast = SystemCardModel {
            kind: SystemCardKind::Toast,
            title: "Updated to dev-abc1234".into(),
            lines: vec!["Restart complete".into()],
            host: None,
        };
        let layout = layout(&HashMap::new(), &toast, 1920.0, 1080.0);
        let bg = layout.shapes[0].rect;
        assert_eq!(bg.x + bg.w / 2.0, 960.0);
        assert_eq!(bg.y + bg.h, 1080.0 - 32.0);
        assert_eq!(layout.shapes.len(), 1);
        assert_eq!(layout.texts.len(), 2);
        assert!(layout.shapes[0].border.is_some());
    }
    #[test]
    fn system_card_text_items_follow_tokens() {
        for theme in ["tonal-glass", "classic", "blueprint"] {
            for (monitor, ui, scale) in [(1.0, 1.0, 1.0), (1.5, 1.0, 1.5), (1.5, 1.25, 1.875)] {
                let mut tokens = tze_hud_config::themes::resolve_config_tokens(&HashMap::from([(
                    "theme".into(),
                    theme.into(),
                )]));
                tokens.insert("ui.scale".into(), ui.to_string());
                tze_hud_config::tokens::set_px_scale(&mut tokens, monitor);
                let card = pairing();
                let result = layout(&tokens, &card, 3840.0, 2160.0);
                let bg = &result.shapes[0];
                assert!((bg.rect.w - 440.0 * scale).abs() < 0.01, "{theme}/{scale}");
                assert_eq!(bg.fill.a, number(&tokens, "opacity.system_card"));
                let code = result.texts.iter().find(|t| &*t.text == "482913").unwrap();
                assert!(
                    (code.font_size_px - number(&tokens, "type.code.display.size") * scale).abs()
                        < 0.01
                );
                let mut warned = card.clone();
                warned.lines[1] = "http://example.test/".repeat(30);
                warned.lines.push(
                    "Windows Firewall blocks remote agents: run the local allow-remote command. "
                        .repeat(8),
                );
                let small = layout(&tokens, &warned, 320.0, 240.0);
                let bounds = small.shapes[0].rect;
                assert!(
                    bounds.x >= 0.0
                        && bounds.y >= 0.0
                        && bounds.x + bounds.w <= 320.01
                        && bounds.y + bounds.h <= 240.01
                );
                for text in &small.texts {
                    assert!(text.pixel_x >= bounds.x && text.pixel_y >= bounds.y);
                    assert!(text.pixel_x + text.bounds_width <= bounds.x + bounds.w + 0.01);
                    assert!(text.pixel_y + text.bounds_height <= bounds.y + bounds.h + 0.01);
                }
                let warning = small.texts.last().unwrap();
                assert_eq!(
                    warning.color,
                    crate::text::rgba_to_srgb_u8(color(&tokens, "color.caution"))
                );
            }
        }
        let tokens = HashMap::from([
            ("size.system_card.width".into(), "600".into()),
            ("type.code.display.size".into(), "64".into()),
            ("color.surface.container.highest".into(), "#FF0000".into()),
            ("color.on_surface".into(), "#00FF00".into()),
        ]);
        let result = layout(&tokens, &pairing(), 1920.0, 1080.0);
        assert_eq!(result.shapes[0].rect.w, 600.0);
        assert_eq!(result.shapes[0].fill.r, 1.0);
        assert!(result.texts.iter().any(|t| &*t.text == "482913"
            && t.font_size_px == 64.0
            && t.color[..3] == [0, 255, 0]));
    }
}
