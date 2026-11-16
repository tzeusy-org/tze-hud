//! The notification card's resolved Tonal Glass style.
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
//! Alert banners (`alert-banner`) are the same card drawn
//! from their own roles (`color.alert.*`, `color.on_alert.*`,
//! `opacity.alert`, `space.alert.*`, `type.alert.*`, corners from
//! `shape.alert` via the policy) and add an outer inset
//! (`space.alert.margin`) and a severity mark: a ring with an `i` or `!`
//! glyph tinted `color.severity.*` (a transparent tint draws none). A lone
//! message line takes the title role in a banner, the body role in a card.

use std::collections::HashMap;

use tze_hud_config::tokens::{CANONICAL_TOKENS, px_scale};
use tze_hud_scene::types::{ChromeLocalState, RenderingPolicy, Rgba};

use super::token_colors::parse_hex_color;

/// The × glyph, drawn as text so its diagonals are antialiased.
pub(super) const DISMISS_GLYPH: &str = "\u{d7}";

/// The alert banner severity mark's glyph: `i` for info, `!` for caution
/// and critical.
pub(super) fn severity_glyph(urgency: u32) -> &'static str {
    if urgency >= 2 { "!" } else { "i" }
}

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
    /// Card fill, `opacity.notification` already applied.
    pub(super) fill: Rgba,
    /// The 1 dp hairline.
    pub(super) outline: Rgba,
    pub(super) title: Rgba,
    pub(super) body: Rgba,
    /// The severity mark's tint (alert banners); `None` draws no mark.
    pub(super) mark: Option<Rgba>,
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
const ALERT_TITLE: RoleKeys = [
    "type.alert.title.size",
    "type.alert.title.line_height",
    "type.alert.title.weight",
];
const ALERT_BODY: RoleKeys = [
    "type.alert.body.size",
    "type.alert.body.line_height",
    "type.alert.body.weight",
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

/// The token keys one kind of notification surface reads; each array is
/// indexed by urgency 0-3.
struct Roles {
    fills: [&'static str; 4],
    outlines: [&'static str; 4],
    titles: [&'static str; 4],
    bodies: [&'static str; 4],
    /// Severity mark tints (alert banners only).
    marks: Option<[&'static str; 4]>,
    opacity: &'static str,
    padding_h: &'static str,
    padding_v: &'static str,
    /// Outer inset from the zone's sides and top, and between stacked
    /// surfaces; `None`: flush, stacked `space.s` apart.
    margin: Option<&'static str>,
    title: RoleKeys,
    body: RoleKeys,
    icon: &'static str,
    /// A lone message line takes the title role (else the body role).
    lone_title: bool,
}

const CARD: Roles = Roles {
    fills: [
        "color.notification.container.low",
        "color.notification.container",
        "color.caution.container",
        "color.error.container",
    ],
    outlines: [
        "color.notification.outline",
        "color.notification.outline",
        "color.notification.caution.outline",
        "color.notification.error.outline",
    ],
    titles: [
        "color.on_surface",
        "color.on_surface",
        "color.on_notification.caution",
        "color.on_notification.error",
    ],
    bodies: [
        "color.on_notification.variant",
        "color.on_notification.variant",
        "color.on_notification.caution",
        "color.on_notification.error",
    ],
    marks: None,
    opacity: "opacity.notification",
    padding_h: "space.notification",
    padding_v: "space.notification",
    margin: None,
    title: TITLE,
    body: BODY,
    icon: "space.xxl",
    lone_title: false,
};

const ALERT: Roles = Roles {
    fills: [
        "color.alert.container",
        "color.alert.container",
        "color.alert.caution.container",
        "color.alert.error.container",
    ],
    outlines: ["color.alert.outline"; 4],
    titles: [
        "color.on_alert",
        "color.on_alert",
        "color.on_alert",
        "color.on_alert.error",
    ],
    bodies: [
        "color.on_alert.variant",
        "color.on_alert.variant",
        "color.on_alert.variant",
        "color.on_alert.error",
    ],
    marks: Some([
        "color.severity.info",
        "color.severity.info",
        "color.severity.warning",
        "color.severity.critical",
    ]),
    opacity: "opacity.alert",
    padding_h: "space.alert.horizontal",
    padding_v: "space.alert.vertical",
    margin: Some("space.alert.margin"),
    title: ALERT_TITLE,
    body: ALERT_BODY,
    icon: "space.xl",
    lone_title: true,
};

/// Resolved notification-card style, all sizes in physical px.
#[derive(Clone, Debug)]
pub(super) struct NotificationStyle {
    /// Inset from the zone's sides and top (alert banners; cards are flush).
    pub(super) margin: f32,
    pub(super) padding_h: f32,
    pub(super) padding_v: f32,
    pub(super) hairline: f32,
    /// Between stacked cards.
    pub(super) stack_gap: f32,
    pub(super) icon_size: f32,
    pub(super) icon_gap: f32,
    pub(super) title: TextRole,
    pub(super) body: TextRole,
    /// A lone message line (no title): the body role on a card, the title
    /// role on a banner.
    pub(super) lone: TextRole,
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
    /// The notification-card style (`notification-area` and any other zone
    /// but `alert-banner`).
    pub(super) fn resolve(tokens: &HashMap<String, String>, policy: &RenderingPolicy) -> Self {
        Self::resolve_roles(&CARD, tokens, policy)
    }

    /// The style of `zone_name`'s notifications: alert banners read the
    /// alert roles, every other zone the card roles.
    pub(super) fn for_zone(
        zone_name: &str,
        tokens: &HashMap<String, String>,
        policy: &RenderingPolicy,
    ) -> Self {
        if super::token_colors::is_alert_banner_zone(zone_name) {
            Self::resolve_roles(&ALERT, tokens, policy)
        } else {
            Self::resolve(tokens, policy)
        }
    }

    /// Resolve from the zone's (monitor-scaled) token map. Zone policy
    /// sizes keep precedence (margins, radius, title size and weight, fill
    /// opacity); colours are token roles, restyled through
    /// `[design_tokens]`. Whether a card has a backdrop at all stays the
    /// policy's `backdrop` switch, checked by the callers.
    fn resolve_roles(
        roles: &Roles,
        tokens: &HashMap<String, String>,
        policy: &RenderingPolicy,
    ) -> Self {
        let title_token = dp(tokens, roles.title[0]);
        // An explicit title size scales the whole type ramp with it.
        let factor = policy
            .font_size_px
            .filter(|s| s.is_finite() && *s > 0.0 && title_token > 0.0)
            .map_or(1.0, |s| s / title_token);
        let mut title = TextRole::resolve(tokens, roles.title, factor);
        if let Some(weight) = policy.font_weight {
            title.weight = weight;
        }
        let body = TextRole::resolve(tokens, roles.body, factor);
        let action_label = TextRole::resolve(tokens, ACTION, factor);
        let opacity = policy
            .backdrop_opacity
            .unwrap_or_else(|| number(tokens, roles.opacity))
            .clamp(0.0, 1.0);
        let tones = std::array::from_fn(|i| {
            let mut fill = color(tokens, roles.fills[i]);
            fill.a *= opacity;
            NotificationTone {
                fill,
                outline: color(tokens, roles.outlines[i]),
                title: color(tokens, roles.titles[i]),
                body: color(tokens, roles.bodies[i]),
                mark: roles
                    .marks
                    .map(|marks| color(tokens, marks[i]))
                    .filter(|c| c.a > 0.0),
            }
        });
        let margin = roles.margin.map_or(0.0, |key| dp(tokens, key));
        Self {
            margin,
            padding_h: policy
                .margin_horizontal
                .or(policy.margin_px)
                .unwrap_or_else(|| dp(tokens, roles.padding_h)),
            padding_v: policy
                .margin_vertical
                .or(policy.margin_px)
                .unwrap_or_else(|| dp(tokens, roles.padding_v)),
            hairline: dp(tokens, "stroke.border.width"),
            stack_gap: if roles.margin.is_some() {
                margin
            } else {
                dp(tokens, "space.s")
            },
            icon_size: dp(tokens, roles.icon),
            icon_gap: dp(tokens, "space.m"),
            dismiss_size: title.line_height,
            dismiss_gap: dp(tokens, "space.s"),
            // The × glyph fills the title line box (its cross spans about
            // half the em), like the mock's 16 dp icon in a 28 dp button.
            dismiss_glyph: TextRole {
                size: title.line_height,
                ..title
            },
            lone: if roles.lone_title { title } else { body },
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
            + if has_title {
                self.title.line_height + self.body.line_height
            } else {
                self.lone.line_height
            }
    }

    /// The severity mark of a publication at `urgency` (alert banners), when
    /// no agent icon takes the icon column.
    pub(super) fn mark(&self, urgency: u32, agent_icon: bool) -> Option<Rgba> {
        (!agent_icon).then(|| self.tone(urgency).mark).flatten()
    }

    /// Bounds of the icon column's square (agent icon or severity mark) in a
    /// card at (`x`, `slot_y`): at the padding, centred on the text block.
    pub(super) fn icon_bounds(
        &self,
        x: f32,
        slot_y: f32,
        has_title: bool,
    ) -> tze_hud_scene::types::Rect {
        let text_h = self.text_block_height(has_title);
        tze_hud_scene::types::Rect::new(
            x + self.padding_h,
            slot_y + (text_h - self.icon_size).max(0.0) * 0.5,
            self.icon_size,
            self.icon_size,
        )
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
/// drawing and the hit region. It sits in the title row at the padding.
pub(super) fn dismiss_bounds(
    zone_name: &str,
    tokens: &HashMap<String, String>,
    policy: &RenderingPolicy,
    card: tze_hud_scene::types::Rect,
) -> tze_hud_scene::types::Rect {
    let style = NotificationStyle::for_zone(zone_name, tokens, policy);
    let size = style.dismiss_size;
    let top = style.padding_v.min((card.height - 1.0).max(0.0));
    tze_hud_scene::types::Rect::new(
        card.x + card.width - style.padding_h - size,
        card.y + top,
        size,
        size.min(card.height - top).max(1.0),
    )
}
