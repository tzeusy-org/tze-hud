//! The notification card's one resolved style (hud-h51u7.4.1, Tonal Glass).
//!
//! Every size, colour and opacity a `notification-area` card draws with comes
//! from here, read from design tokens: the card's own roles
//! (`color.notification.*`, `color.on_notification.*`, with titles in
//! `color.on_surface`,
//! `opacity.notification`, `shape.notification`, `space.notification`,
//! `type.notification.{title,body}.*`; Tonal Glass gives them its generic
//! surface, outline, text, corner, padding and type values, Classic its legacy
//! card look) and generic roles for the controls: `space.xxl` actions in
//! `shape.s` with `state.*` overlays. Urgency 2 and 3 fill with the caution
//! and error container roles. Backdrop, SDF border, icon,
//! text bands, dismiss and action geometry all read this one view, so drawing
//! and hit testing cannot drift. The only literals are the canonical defaults
//! in `tze_hud_config`, used when a token is absent.
//!
//! Alert banners (`alert-banner`) keep their own pre-restyle metrics until
//! hud-h51u7.4.2.

use std::collections::HashMap;

use tze_hud_config::tokens::{CANONICAL_TOKENS, px_scale};
use tze_hud_scene::types::{ChromeLocalState, RenderingPolicy, Rgba};

use super::token_colors::parse_hex_color;

/// The × glyph, drawn as text so its diagonals are antialiased.
pub(super) const DISMISS_GLYPH: &str = "\u{d7}";

fn canonical(key: &str) -> &'static str {
    CANONICAL_TOKENS
        .iter()
        .find(|t| t.key == key)
        .map(|t| t.default_value)
        .unwrap_or_else(|| panic!("notification style token {key} must be canonical"))
}

fn number(tokens: &HashMap<String, String>, key: &str) -> f32 {
    let parse = |v: &str| v.trim().trim_end_matches("px").parse::<f32>().ok();
    tokens
        .get(key)
        .and_then(|v| parse(v))
        .filter(|v| v.is_finite() && *v >= 0.0)
        .or_else(|| parse(canonical(key)))
        .expect("canonical notification number")
}

/// A logical-dp size token in physical px.
fn dp(tokens: &HashMap<String, String>, key: &str) -> f32 {
    number(tokens, key) * px_scale(tokens)
}

fn color(tokens: &HashMap<String, String>, key: &str) -> Rgba {
    tokens
        .get(key)
        .and_then(|v| parse_hex_color(v))
        .or_else(|| parse_hex_color(canonical(key)))
        .expect("canonical notification color")
}

/// `base` with `layer` composited over it at `opacity` (a state layer).
fn overlay(base: Option<Rgba>, layer: Rgba, opacity: f32) -> Rgba {
    match base {
        Some(b) => Rgba::new(
            b.r + (layer.r - b.r) * opacity,
            b.g + (layer.g - b.g) * opacity,
            b.b + (layer.b - b.b) * opacity,
            b.a + (1.0 - b.a) * opacity,
        ),
        None => Rgba::new(layer.r, layer.g, layer.b, opacity),
    }
}

/// Colours of one urgency level.
#[derive(Clone, Copy, Debug)]
pub(super) struct NotificationTone {
    /// Card fill, `opacity.surface` already applied.
    pub(super) fill: Rgba,
    /// The 1 dp hairline.
    pub(super) outline: Rgba,
    pub(super) title: Rgba,
    pub(super) body: Rgba,
}

/// One text line role (title, body or action label).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct TextRole {
    pub(super) size: f32,
    pub(super) line_height: f32,
    pub(super) weight: u16,
}

/// The `size`, `line_height` and `weight` keys of one type role.
type RoleKeys = [&'static str; 3];
const TITLE: RoleKeys = [
    "type.notification.title.size",
    "type.notification.title.line_height",
    "type.notification.title.weight",
];
const BODY: RoleKeys = [
    "type.notification.body.size",
    "type.notification.body.line_height",
    "type.notification.body.weight",
];
const ACTION: RoleKeys = [
    "type.body.m.size",
    "type.body.m.line_height",
    "type.label.m.weight",
];

impl TextRole {
    fn resolve(
        tokens: &HashMap<String, String>,
        [size, line, weight]: RoleKeys,
        factor: f32,
    ) -> Self {
        Self {
            size: dp(tokens, size) * factor,
            line_height: dp(tokens, line) * factor,
            weight: number(tokens, weight) as u16,
        }
    }
    /// `TextItem::line_height_multiplier` for this role.
    pub(super) fn multiplier(&self) -> f32 {
        if self.size > 0.0 {
            self.line_height / self.size
        } else {
            1.0
        }
    }
}

/// Resolved notification-card style, all sizes in physical px.
#[derive(Clone, Debug)]
pub(super) struct NotificationStyle {
    pub(super) padding_h: f32,
    pub(super) padding_v: f32,
    pub(super) hairline: f32,
    /// Between stacked cards.
    pub(super) stack_gap: f32,
    pub(super) icon_size: f32,
    pub(super) icon_gap: f32,
    pub(super) title: TextRole,
    pub(super) body: TextRole,
    /// The dismiss button: a square as tall as the title line.
    pub(super) dismiss_size: f32,
    pub(super) dismiss_gap: f32,
    pub(super) dismiss_glyph: TextRole,
    pub(super) action_height: f32,
    pub(super) action_padding: f32,
    pub(super) action_label: TextRole,
    /// Between side-by-side or stacked actions.
    pub(super) action_gap: f32,
    /// Between the text and the action row.
    pub(super) action_row_gap: f32,
    pub(super) control_radius: f32,
    pub(super) focus_width: f32,
    hover: f32,
    pressed: f32,
    on_surface: Rgba,
    primary: Rgba,
    primary_container: Rgba,
    on_primary_container: Rgba,
    focus: Rgba,
    tones: [NotificationTone; 4],
}

impl NotificationStyle {
    /// Resolve from the zone's (monitor-scaled) token map. Zone policy
    /// sizes keep precedence (margins, radius, title size and weight, fill
    /// opacity); colours are token roles, restyled through
    /// `[design_tokens]`. Whether a card has a backdrop at all stays the
    /// policy's `backdrop` switch, checked by the callers.
    pub(super) fn resolve(tokens: &HashMap<String, String>, policy: &RenderingPolicy) -> Self {
        let title_token = dp(tokens, TITLE[0]);
        // An explicit title size scales the whole type ramp with it.
        let factor = policy
            .font_size_px
            .filter(|s| s.is_finite() && *s > 0.0 && title_token > 0.0)
            .map_or(1.0, |s| s / title_token);
        let mut title = TextRole::resolve(tokens, TITLE, factor);
        if let Some(weight) = policy.font_weight {
            title.weight = weight;
        }
        let body = TextRole::resolve(tokens, BODY, factor);
        let action_label = TextRole::resolve(tokens, ACTION, factor);
        let padding = dp(tokens, "space.notification");
        let opacity = policy
            .backdrop_opacity
            .unwrap_or_else(|| number(tokens, "opacity.notification"))
            .clamp(0.0, 1.0);
        let with_opacity = |mut c: Rgba| {
            c.a *= opacity;
            c
        };
        let neutral = |fill: &str| NotificationTone {
            fill: with_opacity(color(tokens, fill)),
            outline: color(tokens, "color.notification.outline"),
            title: color(tokens, "color.on_surface"),
            body: color(tokens, "color.on_notification.variant"),
        };
        let caution_text = color(tokens, "color.on_notification.caution");
        let error_text = color(tokens, "color.on_notification.error");
        let tones = [
            neutral("color.notification.container.low"),
            neutral("color.notification.container"),
            NotificationTone {
                fill: with_opacity(color(tokens, "color.caution.container")),
                outline: color(tokens, "color.notification.caution.outline"),
                title: caution_text,
                body: caution_text,
            },
            NotificationTone {
                fill: with_opacity(color(tokens, "color.error.container")),
                outline: color(tokens, "color.notification.error.outline"),
                title: error_text,
                body: error_text,
            },
        ];
        Self {
            padding_h: policy
                .margin_horizontal
                .or(policy.margin_px)
                .unwrap_or(padding),
            padding_v: policy
                .margin_vertical
                .or(policy.margin_px)
                .unwrap_or(padding),
            hairline: dp(tokens, "stroke.border.width"),
            stack_gap: dp(tokens, "space.s"),
            icon_size: dp(tokens, "space.xxl"),
            icon_gap: dp(tokens, "space.m"),
            dismiss_size: title.line_height,
            dismiss_gap: dp(tokens, "space.s"),
            // The × glyph fills the title line box (its cross spans about
            // half the em), like the mock's 16 dp icon in a 28 dp button.
            dismiss_glyph: TextRole {
                size: title.line_height,
                ..title
            },
            title,
            body,
            action_height: dp(tokens, "space.xxl"),
            action_padding: dp(tokens, "space.m"),
            action_label,
            action_gap: dp(tokens, "space.xs"),
            action_row_gap: dp(tokens, "space.m"),
            control_radius: dp(tokens, "shape.s"),
            focus_width: dp(tokens, "focus.ring.width"),
            hover: number(tokens, "state.hover.opacity").clamp(0.0, 1.0),
            pressed: number(tokens, "state.pressed.opacity").clamp(0.0, 1.0),
            on_surface: color(tokens, "color.on_surface"),
            primary: color(tokens, "color.primary"),
            primary_container: color(tokens, "color.primary.container"),
            on_primary_container: color(tokens, "color.on_primary.container"),
            focus: color(tokens, "color.primary"),
            tones,
        }
    }

    /// Urgency 0 and 1 neutral (each its own fill), 2 caution, 3 (and
    /// above) error.
    pub(super) fn tone(&self, urgency: u32) -> NotificationTone {
        self.tones[urgency.min(3) as usize]
    }

    /// Text column inset from the card's left edge.
    pub(super) fn text_left(&self, icon_shown: bool) -> f32 {
        self.padding_h
            + if icon_shown {
                self.icon_size + self.icon_gap
            } else {
                0.0
            }
    }

    /// Height of the text block alone (padding, title and body lines).
    pub(super) fn text_block_height(&self, has_title: bool) -> f32 {
        2.0 * self.padding_v
            + self.body.line_height
            + if has_title {
                self.title.line_height
            } else {
                0.0
            }
    }

    /// Fill of a control in `state` over its idle fill: pressed wins over
    /// hovered (the `state.*` layers of `color.on_surface`).
    pub(super) fn control_fill(&self, state: ChromeLocalState, idle: Option<Rgba>) -> Option<Rgba> {
        if state.pressed {
            Some(overlay(idle, self.on_surface, self.pressed))
        } else if state.hovered {
            Some(overlay(idle, self.on_surface, self.hover))
        } else {
            idle
        }
    }

    /// Idle fill of action `index`: the first is the filled primary action.
    pub(super) fn action_fill(&self, index: usize) -> Option<Rgba> {
        (index == 0).then_some(self.primary_container)
    }

    /// Label colour of action `index` on a card of `urgency`: the filled
    /// primary action's on-container role; the transparent ones
    /// `color.primary` on a neutral card, else the tone's own text role
    /// (primary blue does not hold contrast on caution/error containers).
    pub(super) fn action_label_color(&self, index: usize, urgency: u32) -> Rgba {
        if index == 0 {
            self.on_primary_container
        } else if urgency <= 1 {
            self.primary
        } else {
            self.tone(urgency).title
        }
    }

    /// Focus ring colour of a control.
    pub(super) fn focus_color(&self) -> Rgba {
        self.focus
    }
}

/// Bounds of a notification's dismiss ×: the single geometry source for
/// drawing and the hit region. A card's sits in its title row at the
/// padding; an alert banner keeps its pre-restyle square (hud-h51u7.4.2).
#[allow(clippy::too_many_arguments)]
pub(super) fn dismiss_bounds(
    zone_name: &str,
    tokens: &HashMap<String, String>,
    policy: &RenderingPolicy,
    x: f32,
    slot_y: f32,
    w: f32,
    slot_h: f32,
) -> tze_hud_scene::types::Rect {
    let scale = px_scale(tokens);
    if super::token_colors::is_alert_banner_zone(zone_name) {
        return super::token_colors::notification_dismiss_bounds(
            x,
            slot_y,
            w,
            slot_h,
            scale,
            super::token_colors::notification_inset(policy, scale),
        );
    }
    let style = NotificationStyle::resolve(tokens, policy);
    let size = style.dismiss_size;
    let top = style.padding_v.min((slot_h - 1.0).max(0.0));
    tze_hud_scene::types::Rect::new(
        x + w - style.padding_h - size,
        slot_y + top,
        size,
        size.min(slot_h - top).max(1.0),
    )
}
