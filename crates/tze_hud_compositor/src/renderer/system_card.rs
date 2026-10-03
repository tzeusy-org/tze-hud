//! Runtime system card and toast chrome (hud-i2e10.4).
//!
//! A centred card (pairing code + address) or a bottom-centre toast, drawn
//! above all other content, including the safe-mode overlay, from
//! `system_card.*` design tokens. It is drawn by the final
//! `encode_system_card_pass` (backdrop quads, then a dedicated overlay text
//! layer), never in the main passes. The model is pushed
//! by the runtime through [`Compositor::set_system_card`]; it never enters the
//! scene graph, so no agent API (SceneSnapshot, `hud_surfaces`, zone publish
//! results) can observe it. Fallbacks below MUST stay in sync with
//! `tze_hud_config`'s `CANONICAL_TOKENS` (the crates are intentionally
//! unlinked).

use std::collections::HashMap;

use tze_hud_scene::types::{RenderingPolicy, TextAlign};

use super::safe_mode_overlay::OverlayRect;
use super::token_colors::{parse_hex_color, resolve_token_color};
use super::*;

const BACKGROUND_DEFAULT_HEX: &str = "#0C1426F2";
const ACCENT_DEFAULT_HEX: &str = "#4A9EFF";
const TEXT_DEFAULT_HEX: &str = "#FFFFFF";
const WIDTH_DEFAULT_PX: f32 = 420.0;
const PADDING_DEFAULT_PX: f32 = 20.0;
const ACCENT_WIDTH_DEFAULT_PX: f32 = 4.0;
const TOAST_MARGIN_DEFAULT_PX: f32 = 32.0;
const TITLE_FONT_DEFAULT_PX: f32 = 18.0;
const BODY_FONT_DEFAULT_PX: f32 = 14.0;
const CODE_FONT_DEFAULT_PX: f32 = 40.0;
/// Line box height as a multiple of font size, matching the text pipeline.
const LINE_HEIGHT: f32 = 1.4;

/// What the runtime asked the compositor to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemCardKind {
    /// Centred card; `lines[0]` is the one-time code, set in the code font size.
    Pairing,
    /// Bottom-centre toast; lines use the body font size.
    Toast,
}

/// Platform-neutral card content. Expiry is the runtime's concern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemCardModel {
    pub kind: SystemCardKind,
    pub title: String,
    pub lines: Vec<String>,
}

/// Back-to-front draw commands for one card on a `w` x `h` surface.
pub(super) struct SystemCardLayout {
    pub(super) rects: Vec<OverlayRect>,
    pub(super) texts: Vec<TextItem>,
}

fn px_token(tokens: &HashMap<String, String>, key: &str, default: f32) -> f32 {
    tokens
        .get(key)
        .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(default)
}

pub(super) fn system_card_layout(
    tokens: &HashMap<String, String>,
    card: &SystemCardModel,
    w: f32,
    h: f32,
) -> SystemCardLayout {
    let color = |key: &str, fallback: &str| {
        resolve_token_color(tokens, key)
            .or_else(|| parse_hex_color(fallback))
            .unwrap_or(Rgba::WHITE)
    };
    let padding = px_token(tokens, "system_card.padding_px", PADDING_DEFAULT_PX);
    let accent_w = px_token(
        tokens,
        "system_card.accent.width_px",
        ACCENT_WIDTH_DEFAULT_PX,
    );
    let title_px = px_token(
        tokens,
        "system_card.title.font_size_px",
        TITLE_FONT_DEFAULT_PX,
    )
    .clamp(6.0, 200.0);
    let body_px = px_token(
        tokens,
        "system_card.body.font_size_px",
        BODY_FONT_DEFAULT_PX,
    )
    .clamp(6.0, 200.0);
    let code_px = px_token(
        tokens,
        "system_card.code.font_size_px",
        CODE_FONT_DEFAULT_PX,
    )
    .clamp(6.0, 200.0);
    let card_w = px_token(tokens, "system_card.width_px", WIDTH_DEFAULT_PX)
        .max(1.0)
        .min(w);

    let line_px = |i: usize| {
        if card.kind == SystemCardKind::Pairing && i == 0 {
            code_px
        } else {
            body_px
        }
    };
    let content_h = title_px * LINE_HEIGHT
        + (0..card.lines.len())
            .map(|i| line_px(i) * LINE_HEIGHT)
            .sum::<f32>();
    let card_h = (content_h + padding * 2.0).min(h);
    let x = (w - card_w) / 2.0;
    let y = match card.kind {
        SystemCardKind::Pairing => (h - card_h) / 2.0,
        SystemCardKind::Toast => {
            let margin = px_token(
                tokens,
                "system_card.toast.margin_px",
                TOAST_MARGIN_DEFAULT_PX,
            );
            (h - card_h - margin).max(0.0)
        }
    };

    let rects = vec![
        OverlayRect {
            x,
            y,
            w: card_w,
            h: card_h,
            color: color("system_card.background", BACKGROUND_DEFAULT_HEX).to_array(),
        },
        OverlayRect {
            x,
            y,
            w: accent_w.min(card_w),
            h: card_h,
            color: color("system_card.accent.color", ACCENT_DEFAULT_HEX).to_array(),
        },
    ];

    let text_color = color("color.text.primary", TEXT_DEFAULT_HEX);
    let text_x = x + accent_w + padding;
    let text_w = (card_w - accent_w - padding * 2.0).max(1.0);
    let mut texts = Vec::with_capacity(1 + card.lines.len());
    let mut line_y = y + padding;
    let mut push = |text: &str, font_px: f32, weight: u16| {
        let box_h = font_px * LINE_HEIGHT;
        let policy = RenderingPolicy {
            font_size_px: Some(font_px),
            font_weight: Some(weight),
            text_color: Some(text_color),
            text_align: Some(TextAlign::Start),
            margin_px: Some(0.0),
            ..Default::default()
        };
        texts.push(TextItem::from_zone_policy(
            text, text_x, line_y, text_w, box_h, &policy, 1.0,
        ));
        line_y += box_h;
    };
    push(&card.title, title_px, 700);
    for (i, line) in card.lines.iter().enumerate() {
        push(line, line_px(i), 400);
    }
    SystemCardLayout { rects, texts }
}

impl Compositor {
    /// Mirror the runtime's current card; returns true when it changed (the
    /// caller must then repaint once, since no scene change accompanies it).
    pub fn set_system_card(&mut self, card: Option<SystemCardModel>) -> bool {
        if self.system_card == card {
            return false;
        }
        self.system_card = card;
        true
    }

    /// Card backdrop quads for the final system-card pass; empty when no card is set.
    pub(super) fn system_card_vertices(&self, sw: f32, sh: f32) -> Vec<RectVertex> {
        let Some(card) = &self.system_card else {
            return Vec::new();
        };
        system_card_layout(&self.token_map, card, sw, sh)
            .rects
            .iter()
            .flat_map(|r| rect_vertices(r.x, r.y, r.w, r.h, sw, sh, self.gpu_color_raw(r.color)))
            .collect()
    }

    /// Card text for the overlay text layer; empty when no card is set.
    pub(super) fn system_card_text_items(&self, sw: f32, sh: f32) -> Vec<TextItem> {
        self.system_card
            .as_ref()
            .map(|card| system_card_layout(&self.token_map, card, sw, sh).texts)
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
            lines: vec!["482913".into(), "http://100.64.0.1:9090/pair".into()],
        }
    }

    /// Defaults centre a pairing card; the code line uses the code font size.
    #[test]
    fn system_card_pairing_layout_is_centred_with_code_line() {
        let layout = system_card_layout(&HashMap::new(), &pairing(), 1920.0, 1080.0);
        let bg = layout.rects[0];
        assert_eq!(bg.x + bg.w / 2.0, 960.0);
        assert!((bg.y + bg.h / 2.0 - 540.0).abs() < 0.01);
        assert_eq!(layout.texts.len(), 3, "title + two lines");
        assert_eq!(layout.texts[1].font_size_px, CODE_FONT_DEFAULT_PX);
        assert_eq!(layout.texts[2].font_size_px, BODY_FONT_DEFAULT_PX);
        for t in &layout.texts {
            assert!(t.pixel_x >= bg.x && t.pixel_x + t.bounds_width <= bg.x + bg.w);
            assert!(t.pixel_y >= bg.y && t.pixel_y + t.bounds_height <= bg.y + bg.h);
        }
    }

    /// A toast sits at the bottom centre, not over the middle of the display.
    #[test]
    fn system_card_toast_anchors_bottom_centre() {
        let toast = SystemCardModel {
            kind: SystemCardKind::Toast,
            title: "Updated to dev-abc1234".into(),
            lines: vec![],
        };
        let bg = system_card_layout(&HashMap::new(), &toast, 1920.0, 1080.0).rects[0];
        assert_eq!(bg.x + bg.w / 2.0, 960.0);
        assert_eq!(bg.y + bg.h, 1080.0 - TOAST_MARGIN_DEFAULT_PX);
    }

    /// Size, width and colors come from `system_card.*` tokens, not literals.
    #[test]
    fn system_card_text_items_follow_tokens() {
        let base = system_card_layout(&HashMap::new(), &pairing(), 1920.0, 1080.0);
        let tokens = HashMap::from([
            (
                "system_card.code.font_size_px".to_string(),
                "64".to_string(),
            ),
            ("system_card.width_px".to_string(), "600".to_string()),
            ("system_card.background".to_string(), "#FF0000".to_string()),
            ("color.text.primary".to_string(), "#00FF00".to_string()),
        ]);
        let themed = system_card_layout(&tokens, &pairing(), 1920.0, 1080.0);
        assert_eq!(themed.texts[1].font_size_px, 64.0);
        assert_ne!(base.texts[1].font_size_px, themed.texts[1].font_size_px);
        assert_eq!(themed.rects[0].w, 600.0);
        assert_eq!(themed.rects[0].color, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(themed.texts[1].color[..3], [0, 255, 0]);
    }
}
