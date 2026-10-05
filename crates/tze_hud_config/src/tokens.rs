//! Design token system for tze_hud configuration.
//!
//! Implements `[design_tokens]` TOML section handling:
//! - Key validation pattern `[a-z][a-z0-9]*(\.[a-z][a-z0-9_]*)*`
//! - Four token value parsers: color hex, numeric, font family, literal string
//! - Canonical token schema (~28 required keys with fallback defaults)
//! - Three-layer token resolution:
//!   canonical fallbacks → selected theme ([`crate::themes`]) → config overrides
//!
//! ## Error codes produced
//!
//! | Error code | Condition |
//! |---|---|
//! | `CONFIG_INVALID_TOKEN_KEY` | Key in `[design_tokens]` does not match the required pattern |
//! | `TOKEN_VALUE_PARSE_ERROR` | A token value string could not be parsed into the expected format |

use std::collections::HashMap;

use tze_hud_scene::config::{ConfigError, ConfigErrorCode};
use tze_hud_scene::types::FontFamily;

use crate::raw::RawConfig;

// ─── DesignTokenMap ────────────────────────────────────────────────────────────

/// A flat, immutable (after startup) map of design tokens.
///
/// Keys follow the pattern `[a-z][a-z0-9]*(\.[a-z][a-z0-9_]*)*`.
/// Values are opaque strings until explicitly parsed via `TokenValue`.
pub type DesignTokenMap = HashMap<String, String>;

// ─── Token key validation ─────────────────────────────────────────────────────

/// Returns `true` if the key matches `^[a-z][a-z0-9]*(\.[a-z][a-z0-9_]*)*$`.
///
/// Segment rules:
/// - First segment: starts with `[a-z]`, followed by `[a-z0-9]*`
/// - Subsequent segments (after `.`): starts with `[a-z]`, followed by `[a-z0-9_]*`
pub fn is_valid_token_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let mut segments = key.split('.');
    // First segment: [a-z][a-z0-9]*
    if let Some(first) = segments.next() {
        if !is_valid_first_segment(first) {
            return false;
        }
    } else {
        return false;
    }
    // Remaining segments: [a-z][a-z0-9_]*
    for seg in segments {
        if !is_valid_subsequent_segment(seg) {
            return false;
        }
    }
    true
}

fn is_valid_first_segment(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn is_valid_subsequent_segment(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

// ─── Token value types ────────────────────────────────────────────────────────

/// RGBA color value, components in `[0.0, 1.0]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// Parse a font family from a canonical v1 keyword string.
///
/// Supported keywords (per spec):
/// - `"system-ui"` → `FontFamily::SystemSansSerif`
/// - `"sans-serif"` → `FontFamily::SystemSansSerif`
/// - `"monospace"` → `FontFamily::SystemMonospace`
/// - `"serif"` → `FontFamily::SystemSerif`
///
/// Any other value returns `None`. This is intentionally a free function
/// (not a method on `FontFamily`) because `FontFamily` is defined in
/// `tze_hud_scene::types` and we use it directly.
pub fn font_family_from_keyword(s: &str) -> Option<FontFamily> {
    match s {
        "system-ui" | "sans-serif" => Some(FontFamily::SystemSansSerif),
        "monospace" => Some(FontFamily::SystemMonospace),
        "serif" => Some(FontFamily::SystemSerif),
        _ => None,
    }
}

/// A parsed token value.
///
/// Parsing is attempted in order:
/// 1. Color hex (`#RRGGBB` or `#RRGGBBAA`) → `Color(Rgba)`
/// 2. Numeric (decimal) → `Numeric(f32)`
/// 3. Font family keyword → `Font(FontFamily)`
/// 4. Everything else → `Literal(String)`
#[derive(Clone, Debug, PartialEq)]
pub enum TokenValue {
    Color(Rgba),
    Numeric(f32),
    Font(FontFamily),
    Literal(String),
}

// ─── Value parsers ────────────────────────────────────────────────────────────

/// Parse a `#RRGGBB` or `#RRGGBBAA` hex color string into `Rgba`.
///
/// Returns `None` if the string does not match either form.
/// Hex digits are case-insensitive. Non-ASCII input is always rejected.
pub fn parse_color_hex(s: &str) -> Option<Rgba> {
    let s = s.trim();
    if !s.starts_with('#') || !s.is_ascii() {
        return None;
    }
    let hex = &s[1..];
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Rgba {
                r: r as f32 / 255.0,
                g: g as f32 / 255.0,
                b: b as f32 / 255.0,
                a: 1.0,
            })
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(Rgba {
                r: r as f32 / 255.0,
                g: g as f32 / 255.0,
                b: b as f32 / 255.0,
                a: a as f32 / 255.0,
            })
        }
        _ => None,
    }
}

/// Parse a decimal numeric string into `f32`.
///
/// Leading/trailing whitespace is NOT permitted (per spec).
/// NaN and infinity strings (`"nan"`, `"inf"`, `"infinity"`, etc.) are rejected.
pub fn parse_numeric(s: &str) -> Option<f32> {
    let n = s.parse::<f32>().ok()?;
    if n.is_nan() || n.is_infinite() {
        return None;
    }
    Some(n)
}

/// Parse a font family keyword.
///
/// Whitespace trimming is not performed; the input must match exactly.
pub fn parse_font_family(s: &str) -> Option<FontFamily> {
    font_family_from_keyword(s)
}

/// Parse a token value string into a `TokenValue`.
///
/// Order of precedence:
/// 1. Color hex
/// 2. Numeric
/// 3. Font family
/// 4. Literal string (always succeeds)
pub fn parse_token_value(s: &str) -> TokenValue {
    if let Some(color) = parse_color_hex(s) {
        return TokenValue::Color(color);
    }
    if let Some(n) = parse_numeric(s) {
        return TokenValue::Numeric(n);
    }
    if let Some(f) = parse_font_family(s) {
        return TokenValue::Font(f);
    }
    TokenValue::Literal(s.to_string())
}

// ─── Canonical token schema ───────────────────────────────────────────────────

/// A canonical token definition: key, expected format, and fallback default.
pub struct CanonicalToken {
    pub key: &'static str,
    pub description: &'static str,
    pub default_value: &'static str,
}

/// The canonical token keys with their fallback defaults.
///
/// These are always present in the resolved token map even if the config
/// does not specify them. The schema is defined by the component-shape-language
/// specification (`openspec/changes/component-shape-language/specs/
/// component-shape-language/spec.md`, §Requirement: Canonical Token Schema).
///
/// Token groups: color, opacity, typography, spacing, stroke, border.radius,
/// portal divider, and claimed-tile placement (`tile.*`).
pub static CANONICAL_TOKENS: &[CanonicalToken] = &[
    // Color — text
    CanonicalToken {
        key: "color.text.primary",
        description: "Primary text color",
        default_value: "#FFFFFF",
    },
    CanonicalToken {
        key: "color.text.secondary",
        description: "Secondary/muted text color",
        default_value: "#B0B0B0",
    },
    CanonicalToken {
        key: "color.text.accent",
        description: "Accent/highlight text color",
        default_value: "#4A9EFF",
    },
    // Color — backdrop
    CanonicalToken {
        key: "color.backdrop.default",
        description: "Default backdrop fill color",
        default_value: "#000000",
    },
    // Color — outline
    CanonicalToken {
        key: "color.outline.default",
        description: "Default text outline/stroke color",
        default_value: "#000000",
    },
    // Color — border
    CanonicalToken {
        key: "color.border.default",
        description: "Default border/frame color",
        default_value: "#333333",
    },
    // Color — severity
    CanonicalToken {
        key: "color.severity.info",
        description: "Info severity indicator color",
        default_value: "#4A9EFF",
    },
    CanonicalToken {
        key: "color.severity.warning",
        description: "Warning severity indicator color",
        default_value: "#FFB800",
    },
    CanonicalToken {
        key: "color.severity.error",
        description: "Error severity indicator color",
        default_value: "#FF4444",
    },
    CanonicalToken {
        key: "color.severity.critical",
        description: "Critical severity indicator color",
        default_value: "#FF0000",
    },
    // Opacity
    CanonicalToken {
        key: "opacity.backdrop.default",
        description: "Default backdrop opacity (0.0–1.0)",
        default_value: "0.6",
    },
    CanonicalToken {
        key: "opacity.backdrop.opaque",
        description: "Opaque backdrop opacity threshold (0.0–1.0)",
        default_value: "0.9",
    },
    // Typography — body
    CanonicalToken {
        key: "typography.body.family",
        description: "Body text font family",
        default_value: "system-ui",
    },
    CanonicalToken {
        key: "typography.body.size",
        description: "Body text size in pixels",
        default_value: "16",
    },
    CanonicalToken {
        key: "typography.body.weight",
        description: "Body text weight (CSS numeric)",
        default_value: "400",
    },
    // Typography — heading
    CanonicalToken {
        key: "typography.heading.family",
        description: "Heading font family",
        default_value: "system-ui",
    },
    CanonicalToken {
        key: "typography.heading.size",
        description: "Heading text size in pixels",
        default_value: "24",
    },
    CanonicalToken {
        key: "typography.heading.weight",
        description: "Heading font weight (CSS numeric)",
        default_value: "700",
    },
    // Typography — subtitle
    CanonicalToken {
        key: "typography.subtitle.family",
        description: "Subtitle font family",
        default_value: "system-ui",
    },
    CanonicalToken {
        key: "typography.subtitle.size",
        description: "Subtitle text size in pixels",
        default_value: "28",
    },
    CanonicalToken {
        key: "typography.subtitle.weight",
        description: "Subtitle font weight (CSS numeric)",
        default_value: "600",
    },
    // Spacing
    CanonicalToken {
        key: "spacing.unit",
        description: "Base spacing unit in pixels",
        default_value: "8",
    },
    CanonicalToken {
        key: "spacing.padding.small",
        description: "Small internal padding in pixels",
        default_value: "4",
    },
    CanonicalToken {
        key: "spacing.padding.medium",
        description: "Medium internal padding in pixels",
        default_value: "8",
    },
    CanonicalToken {
        key: "spacing.padding.large",
        description: "Large internal padding in pixels",
        default_value: "16",
    },
    // Stroke
    CanonicalToken {
        key: "stroke.outline.width",
        description: "Text outline stroke width in pixels",
        default_value: "2",
    },
    CanonicalToken {
        key: "stroke.border.width",
        description: "Border/frame stroke width in pixels",
        default_value: "1",
    },
    // Border radius
    CanonicalToken {
        key: "border.radius.small",
        description: "Small corner radius in pixels",
        default_value: "4",
    },
    CanonicalToken {
        key: "border.radius.medium",
        description: "Medium corner radius in pixels",
        default_value: "8",
    },
    CanonicalToken {
        key: "border.radius.large",
        description: "Large corner radius in pixels",
        default_value: "16",
    },
    // Portal transcript turn separator (hud-nx7yq.4). Consumed by the compositor
    // markdown path to render a token-styled divider on thematic-break (`---`)
    // lines between transcript entries.
    CanonicalToken {
        key: "portal.divider.color",
        description: "Transcript turn separator (divider) color (RGBA hex)",
        default_value: "#46536E",
    },
    CanonicalToken {
        key: "portal.divider.thickness_px",
        description: "Transcript turn separator (divider) thickness in pixels",
        default_value: "2",
    },
    // Claimed-tile placement (ClaimTile anchor + size class; docs/api.md).
    CanonicalToken {
        key: "tile.small.width",
        description: "Claimed tile width, small size class (px)",
        default_value: "240",
    },
    CanonicalToken {
        key: "tile.small.height",
        description: "Claimed tile height, small size class (px)",
        default_value: "120",
    },
    CanonicalToken {
        key: "tile.medium.width",
        description: "Claimed tile width, medium size class (px)",
        default_value: "360",
    },
    CanonicalToken {
        key: "tile.medium.height",
        description: "Claimed tile height, medium size class (px)",
        default_value: "220",
    },
    CanonicalToken {
        key: "tile.large.width",
        description: "Claimed tile width, large size class (px)",
        default_value: "560",
    },
    CanonicalToken {
        key: "tile.large.height",
        description: "Claimed tile height, large size class (px)",
        default_value: "360",
    },
    CanonicalToken {
        key: "tile.wide.width",
        description: "Claimed tile width, wide size class (px)",
        default_value: "720",
    },
    CanonicalToken {
        key: "tile.wide.height",
        description: "Claimed tile height, wide size class (px)",
        default_value: "120",
    },
    CanonicalToken {
        key: "tile.tall.width",
        description: "Claimed tile width, tall size class (px)",
        default_value: "300",
    },
    CanonicalToken {
        key: "tile.tall.height",
        description: "Claimed tile height, tall size class (px)",
        default_value: "520",
    },
    // Orphaned-tile disconnection badge (docs/invariants.md section 4).
    CanonicalToken {
        key: "tile.disconnect_badge.color",
        description: "Disconnection badge color on orphaned tiles",
        default_value: "#FFB800",
    },
    CanonicalToken {
        key: "tile.disconnect_badge.size_px",
        description: "Disconnection badge square extent on orphaned tiles (px)",
        default_value: "16",
    },
    // Viewer close button shown on a hovered tile (docs/invariants.md section 3).
    CanonicalToken {
        key: "tile.close_button.size_px",
        description: "Hover close button square extent on tiles (px)",
        default_value: "22",
    },
    CanonicalToken {
        key: "tile.close_button.margin_px",
        description: "Inset of the hover close button from the tile's top-right corner (px)",
        default_value: "6",
    },
    CanonicalToken {
        key: "tile.close_button.background",
        description: "Fill of the hover close button (RGBA hex)",
        default_value: "#000000B3",
    },
    CanonicalToken {
        key: "tile.close_button.glyph_color",
        description: "Color of the hover close button's x mark (RGBA hex)",
        default_value: "#FFFFFF",
    },
    CanonicalToken {
        key: "tile.close_button.glyph_stroke_px",
        description: "Stroke thickness of the hover close button's x mark (px)",
        default_value: "2",
    },
    // Safe-mode overlay (docs/invariants.md section 5), drawn by the compositor.
    CanonicalToken {
        key: "safe_mode.overlay.color",
        description: "Full-surface dim while safe mode is active (RGBA hex)",
        default_value: "#000000B3",
    },
    CanonicalToken {
        key: "safe_mode.banner.color",
        description: "Top banner bar color while safe mode is active",
        default_value: "#FFB800",
    },
    CanonicalToken {
        key: "safe_mode.banner.height_px",
        description: "Top banner bar height while safe mode is active (px)",
        default_value: "8",
    },
    // Notification action buttons (docs/api.md notification `actions`).
    CanonicalToken {
        key: "notification.action.background",
        description: "Fill of notification action buttons (RGBA hex)",
        default_value: "#FFFFFF26",
    },
    CanonicalToken {
        key: "typography.notification.action.font_size_px",
        description: "Notification action button label font size (px)",
        default_value: "12",
    },
    CanonicalToken {
        key: "typography.notification.action.font_weight",
        description: "Notification action button label font weight",
        default_value: "600",
    },
    // Runtime system card / toast (pairing code, update notices); never agent-visible.
    CanonicalToken {
        key: "system_card.background",
        description: "System card and toast fill (RGBA hex)",
        default_value: "#0C1426F2",
    },
    CanonicalToken {
        key: "system_card.accent.color",
        description: "System card left accent bar color",
        default_value: "#4A9EFF",
    },
    CanonicalToken {
        key: "system_card.accent.width_px",
        description: "System card left accent bar width (px)",
        default_value: "4",
    },
    CanonicalToken {
        key: "system_card.width_px",
        description: "System card and toast width (px)",
        default_value: "420",
    },
    CanonicalToken {
        key: "system_card.padding_px",
        description: "System card inner padding (px)",
        default_value: "20",
    },
    CanonicalToken {
        key: "system_card.toast.margin_px",
        description: "Toast distance from the bottom display edge (px)",
        default_value: "32",
    },
    CanonicalToken {
        key: "system_card.title.font_size_px",
        description: "System card title font size (px)",
        default_value: "18",
    },
    CanonicalToken {
        key: "system_card.body.font_size_px",
        description: "System card body line font size (px)",
        default_value: "14",
    },
    CanonicalToken {
        key: "system_card.code.font_size_px",
        description: "Pairing code font size on the system card (px)",
        default_value: "40",
    },
    CanonicalToken {
        key: "tile.margin",
        description: "Inset of claimed tiles from the display edge (px)",
        default_value: "24",
    },
    CanonicalToken {
        key: "tile.gap",
        description: "Gap between claimed tiles stacked at one anchor (px)",
        default_value: "12",
    },
    // Semantic surfaces (tonal elevation, lowest -> highest; hud-h51u7)
    CanonicalToken {
        key: "color.surface",
        description: "Base surface color (lowest tonal level)",
        default_value: "#0F1216",
    },
    CanonicalToken {
        key: "color.surface.container.low",
        description: "Low-emphasis container surface",
        default_value: "#161A20",
    },
    CanonicalToken {
        key: "color.surface.container",
        description: "Default container surface (cards, panels)",
        default_value: "#1B2028",
    },
    CanonicalToken {
        key: "color.surface.container.high",
        description: "Raised container surface",
        default_value: "#222833",
    },
    CanonicalToken {
        key: "color.surface.container.highest",
        description: "Highest container surface (menus, focused cards)",
        default_value: "#2A313D",
    },
    CanonicalToken {
        key: "opacity.surface",
        description: "Opacity of container surfaces over the desktop (0.0-1.0)",
        default_value: "0.92",
    },
    CanonicalToken {
        key: "opacity.scrim",
        description: "Opacity of full-surface scrims behind modal content (0.0-1.0)",
        default_value: "0.72",
    },
    // Semantic roles
    CanonicalToken {
        key: "color.on_surface",
        description: "Primary content (text, icons) on any surface",
        default_value: "#E8EBF0",
    },
    CanonicalToken {
        key: "color.on_surface.variant",
        description: "Secondary/muted content on any surface",
        default_value: "#A9B1BE",
    },
    CanonicalToken {
        key: "color.outline",
        description: "Emphasized outline (focus-adjacent borders, dividers that must read)",
        default_value: "#5A6475",
    },
    CanonicalToken {
        key: "color.outline.variant",
        description: "Subtle outline (hairline card borders, separators)",
        default_value: "#343B47",
    },
    CanonicalToken {
        key: "color.primary",
        description: "Primary accent (active state, links, progress)",
        default_value: "#8AB4FF",
    },
    CanonicalToken {
        key: "color.on_primary",
        description: "Content on a primary fill",
        default_value: "#0B1B36",
    },
    CanonicalToken {
        key: "color.primary.container",
        description: "Low-emphasis primary fill",
        default_value: "#1E3A66",
    },
    CanonicalToken {
        key: "color.on_primary.container",
        description: "Content on a primary container",
        default_value: "#D6E3FF",
    },
    CanonicalToken {
        key: "color.success",
        description: "Success/healthy status",
        default_value: "#7BD88F",
    },
    CanonicalToken {
        key: "color.caution",
        description: "Caution/warning status",
        default_value: "#F2C14E",
    },
    CanonicalToken {
        key: "color.caution.container",
        description: "Caution container fill",
        default_value: "#4A3A10",
    },
    CanonicalToken {
        key: "color.on_caution.container",
        description: "Content on a caution container",
        default_value: "#FFE7A8",
    },
    CanonicalToken {
        key: "color.error",
        description: "Error/critical status",
        default_value: "#FF8A80",
    },
    CanonicalToken {
        key: "color.error.container",
        description: "Error container fill",
        default_value: "#5C1A17",
    },
    CanonicalToken {
        key: "color.on_error.container",
        description: "Content on an error container",
        default_value: "#FFDAD6",
    },
    // State layers and focus
    CanonicalToken {
        key: "state.hover.opacity",
        description: "Opacity of the on-surface state layer while hovered (0.0-1.0)",
        default_value: "0.08",
    },
    CanonicalToken {
        key: "state.pressed.opacity",
        description: "Opacity of the on-surface state layer while pressed (0.0-1.0)",
        default_value: "0.12",
    },
    CanonicalToken {
        key: "focus.ring.width",
        description: "Keyboard focus ring stroke width (logical px)",
        default_value: "2",
    },
    CanonicalToken {
        key: "focus.ring.offset",
        description: "Gap between an element and its focus ring (logical px)",
        default_value: "2",
    },
    // Motion (easing names: see MOTION_EASINGS)
    CanonicalToken {
        key: "motion.enter.ms",
        description: "Enter transition duration (ms)",
        default_value: "180",
    },
    CanonicalToken {
        key: "motion.exit.ms",
        description: "Exit transition duration (ms)",
        default_value: "120",
    },
    CanonicalToken {
        key: "motion.state.ms",
        description: "State-change (hover/press) transition duration (ms)",
        default_value: "100",
    },
    CanonicalToken {
        key: "motion.enter.easing",
        description: "Enter transition easing curve",
        default_value: "decelerate",
    },
    CanonicalToken {
        key: "motion.exit.easing",
        description: "Exit transition easing curve",
        default_value: "accelerate",
    },
    // Shape scale (corner radius, logical px)
    CanonicalToken {
        key: "shape.xs",
        description: "Extra-small corner radius",
        default_value: "4",
    },
    CanonicalToken {
        key: "shape.s",
        description: "Small corner radius",
        default_value: "8",
    },
    CanonicalToken {
        key: "shape.m",
        description: "Medium corner radius",
        default_value: "12",
    },
    CanonicalToken {
        key: "shape.l",
        description: "Large corner radius",
        default_value: "16",
    },
    CanonicalToken {
        key: "shape.xl",
        description: "Extra-large corner radius",
        default_value: "28",
    },
    CanonicalToken {
        key: "shape.full",
        description: "Fully rounded (pill) corner radius",
        default_value: "999",
    },
    // Spacing scale (4 px grid, logical px)
    CanonicalToken {
        key: "space.xs",
        description: "Extra-small spacing",
        default_value: "4",
    },
    CanonicalToken {
        key: "space.s",
        description: "Small spacing",
        default_value: "8",
    },
    CanonicalToken {
        key: "space.m",
        description: "Medium spacing",
        default_value: "12",
    },
    CanonicalToken {
        key: "space.l",
        description: "Large spacing",
        default_value: "16",
    },
    CanonicalToken {
        key: "space.xl",
        description: "Extra-large spacing",
        default_value: "24",
    },
    CanonicalToken {
        key: "space.xxl",
        description: "Double-extra-large spacing",
        default_value: "32",
    },
    // Type scale (family / size / line height / weight)
    CanonicalToken {
        key: "font.sans",
        description: "Sans-serif font family name (free-form; unloaded names fall back)",
        default_value: "IBM Plex Sans",
    },
    CanonicalToken {
        key: "font.mono",
        description: "Monospace font family name (free-form; unloaded names fall back)",
        default_value: "IBM Plex Mono",
    },
    CanonicalToken {
        key: "type.code.display.family",
        description: "Pairing-code display: font family name",
        default_value: "IBM Plex Mono",
    },
    CanonicalToken {
        key: "type.code.display.size",
        description: "Pairing-code display: font size (logical px)",
        default_value: "40",
    },
    CanonicalToken {
        key: "type.code.display.line_height",
        description: "Pairing-code display: line height (logical px)",
        default_value: "48",
    },
    CanonicalToken {
        key: "type.code.display.weight",
        description: "Pairing-code display: font weight (CSS numeric)",
        default_value: "500",
    },
    CanonicalToken {
        key: "type.caption.display.family",
        description: "Subtitle/caption display: font family name",
        default_value: "IBM Plex Sans",
    },
    CanonicalToken {
        key: "type.caption.display.size",
        description: "Subtitle/caption display: font size (logical px)",
        default_value: "26",
    },
    CanonicalToken {
        key: "type.caption.display.line_height",
        description: "Subtitle/caption display: line height (logical px)",
        default_value: "34",
    },
    CanonicalToken {
        key: "type.caption.display.weight",
        description: "Subtitle/caption display: font weight (CSS numeric)",
        default_value: "600",
    },
    CanonicalToken {
        key: "type.headline.s.family",
        description: "Small headline: font family name",
        default_value: "IBM Plex Sans",
    },
    CanonicalToken {
        key: "type.headline.s.size",
        description: "Small headline: font size (logical px)",
        default_value: "20",
    },
    CanonicalToken {
        key: "type.headline.s.line_height",
        description: "Small headline: line height (logical px)",
        default_value: "28",
    },
    CanonicalToken {
        key: "type.headline.s.weight",
        description: "Small headline: font weight (CSS numeric)",
        default_value: "600",
    },
    CanonicalToken {
        key: "type.title.m.family",
        description: "Medium title (card titles): font family name",
        default_value: "IBM Plex Sans",
    },
    CanonicalToken {
        key: "type.title.m.size",
        description: "Medium title (card titles): font size (logical px)",
        default_value: "16",
    },
    CanonicalToken {
        key: "type.title.m.line_height",
        description: "Medium title (card titles): line height (logical px)",
        default_value: "24",
    },
    CanonicalToken {
        key: "type.title.m.weight",
        description: "Medium title (card titles): font weight (CSS numeric)",
        default_value: "600",
    },
    CanonicalToken {
        key: "type.body.m.family",
        description: "Medium body text: font family name",
        default_value: "IBM Plex Sans",
    },
    CanonicalToken {
        key: "type.body.m.size",
        description: "Medium body text: font size (logical px)",
        default_value: "14",
    },
    CanonicalToken {
        key: "type.body.m.line_height",
        description: "Medium body text: line height (logical px)",
        default_value: "20",
    },
    CanonicalToken {
        key: "type.body.m.weight",
        description: "Medium body text: font weight (CSS numeric)",
        default_value: "400",
    },
    CanonicalToken {
        key: "type.label.m.family",
        description: "Medium label (buttons, chips): font family name",
        default_value: "IBM Plex Sans",
    },
    CanonicalToken {
        key: "type.label.m.size",
        description: "Medium label (buttons, chips): font size (logical px)",
        default_value: "12",
    },
    CanonicalToken {
        key: "type.label.m.line_height",
        description: "Medium label (buttons, chips): line height (logical px)",
        default_value: "16",
    },
    CanonicalToken {
        key: "type.label.m.weight",
        description: "Medium label (buttons, chips): font weight (CSS numeric)",
        default_value: "500",
    },
    CanonicalToken {
        key: "type.label.s.family",
        description: "Small label (metadata): font family name",
        default_value: "IBM Plex Sans",
    },
    CanonicalToken {
        key: "type.label.s.size",
        description: "Small label (metadata): font size (logical px)",
        default_value: "11",
    },
    CanonicalToken {
        key: "type.label.s.line_height",
        description: "Small label (metadata): line height (logical px)",
        default_value: "16",
    },
    CanonicalToken {
        key: "type.label.s.weight",
        description: "Small label (metadata): font weight (CSS numeric)",
        default_value: "500",
    },
    CanonicalToken {
        key: "type.readout.family",
        description: "Numeric readout (gauges, timers): font family name",
        default_value: "IBM Plex Mono",
    },
    CanonicalToken {
        key: "type.readout.size",
        description: "Numeric readout (gauges, timers): font size (logical px)",
        default_value: "14",
    },
    CanonicalToken {
        key: "type.readout.line_height",
        description: "Numeric readout (gauges, timers): line height (logical px)",
        default_value: "20",
    },
    CanonicalToken {
        key: "type.readout.weight",
        description: "Numeric readout (gauges, timers): font weight (CSS numeric)",
        default_value: "500",
    },
    // Notification card renderer keys (defaults match the compositor fallbacks)
    CanonicalToken {
        key: "color.notification.urgency.low",
        description: "Notification card backdrop, urgency low",
        default_value: "#000000",
    },
    CanonicalToken {
        key: "color.notification.urgency.normal",
        description: "Notification card backdrop, urgency normal",
        default_value: "#0C1426",
    },
    CanonicalToken {
        key: "color.notification.urgency.urgent",
        description: "Notification card backdrop, urgency urgent",
        default_value: "#2A1E08",
    },
    CanonicalToken {
        key: "color.notification.urgency.critical",
        description: "Notification card backdrop, urgency critical",
        default_value: "#450612",
    },
    CanonicalToken {
        key: "typography.notification.body.scale",
        description: "Notification body line size relative to the title (0.5-1.0)",
        default_value: "0.85",
    },
    CanonicalToken {
        key: "typography.notification.title.weight",
        description: "Notification title font weight (CSS numeric)",
        default_value: "700",
    },
];

// ─── Token resolution ─────────────────────────────────────────────────────────

/// Resolve the effective design token map using three-layer precedence
/// (lowest to highest):
/// 1. Canonical fallback defaults ([`CANONICAL_TOKENS`])
/// 2. Theme tokens (`theme_tokens`, see [`crate::themes`])
/// 3. Config `[design_tokens]` overrides (`config_tokens`)
///
/// Pure function of its inputs, so a later live theme swap can re-run it and
/// re-apply the result. The returned map contains ALL canonical tokens (via
/// fallbacks) plus any non-canonical tokens from the theme or config layers.
/// Callers holding a raw `[design_tokens]` table (which may carry the
/// reserved `theme` selector) use [`crate::themes::resolve_config_tokens`].
pub fn resolve_tokens(
    theme_tokens: &DesignTokenMap,
    config_tokens: &DesignTokenMap,
) -> DesignTokenMap {
    let mut resolved = DesignTokenMap::with_capacity(
        CANONICAL_TOKENS.len() + theme_tokens.len() + config_tokens.len(),
    );
    for token in CANONICAL_TOKENS {
        resolved.insert(token.key.to_string(), token.default_value.to_string());
    }
    for layer in [theme_tokens, config_tokens] {
        for (k, v) in layer {
            resolved.insert(k.clone(), v.clone());
        }
    }
    resolved
}

/// Easing curve names accepted by `motion.*.easing` tokens.
///
/// The compositor's curves (`renderer/easing.rs`) map as: `linear` ->
/// `Linear`, `standard` -> `EaseInOut`, `decelerate` -> `EaseOutQuad`.
/// `accelerate` (ease-in) has no compositor curve yet; it is added when a
/// consumer of `motion.exit.easing` lands (hud-h51u7.4).
pub const MOTION_EASINGS: &[&str] = &["linear", "standard", "decelerate", "accelerate"];

/// Look up a canonical token definition by key.
pub fn canonical_token(key: &str) -> Option<&'static CanonicalToken> {
    CANONICAL_TOKENS.iter().find(|t| t.key == key)
}

/// Check `value` against the kind of the canonical token `key`.
///
/// The kind is inferred from the key and its canonical default: font family
/// keys (`font.*`, `*.family`) take any non-empty family name (free-form, so
/// fonts stay user-configurable); `*.easing` keys take a [`MOTION_EASINGS`]
/// name; a color default requires `#RRGGBB`/`#RRGGBBAA`; a numeric default
/// requires a finite number; anything else must be non-empty.
///
/// Returns a human-readable description of the expected value on failure.
/// Non-canonical keys are rejected.
pub fn validate_canonical_value(key: &str, value: &str) -> Result<(), String> {
    let Some(token) = canonical_token(key) else {
        return Err("a canonical token key".into());
    };
    let (ok, expected) = if key.starts_with("font.") || key.ends_with(".family") {
        (
            !value.trim().is_empty(),
            "a non-empty font family name".into(),
        )
    } else if key.ends_with(".easing") {
        (
            MOTION_EASINGS.contains(&value),
            format!("one of {}", MOTION_EASINGS.join(", ")),
        )
    } else if parse_color_hex(token.default_value).is_some() {
        (
            parse_color_hex(value).is_some(),
            "a color #RRGGBB or #RRGGBBAA".into(),
        )
    } else if parse_numeric(token.default_value).is_some() {
        (parse_numeric(value).is_some(), "a finite number".into())
    } else {
        (!value.is_empty(), "a non-empty value".into())
    };
    if ok { Ok(()) } else { Err(expected) }
}

// ─── Validation ───────────────────────────────────────────────────────────────

/// Validate the `[design_tokens]` section of a `RawConfig`.
///
/// Produces `CONFIG_INVALID_TOKEN_KEY` for any key that does not match the
/// required pattern. Non-canonical keys are accepted silently.
pub fn validate_design_tokens(raw: &RawConfig, errors: &mut Vec<ConfigError>) {
    let Some(tokens) = &raw.design_tokens else {
        return;
    };
    for key in tokens.0.keys() {
        if !is_valid_token_key(key) {
            errors.push(ConfigError {
                code: ConfigErrorCode::InvalidTokenKey,
                field_path: format!("design_tokens.{key}"),
                expected: "key matching [a-z][a-z0-9]*(\\.[a-z][a-z0-9_]*)*".into(),
                got: key.clone(),
                hint: format!(
                    "use lowercase dot-separated segments, e.g. \"color.text.primary\"; \
                     got {key:?}"
                ),
            });
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Key validation ────────────────────────────────────────────────────────

    #[test]
    fn test_valid_token_keys() {
        assert!(is_valid_token_key("color.text.primary"));
        assert!(is_valid_token_key("typography.body.size"));
        assert!(is_valid_token_key("spacing.unit"));
        assert!(is_valid_token_key("stroke.outline.width"));
        assert!(is_valid_token_key("color.backdrop.default"));
        assert!(is_valid_token_key("a.b"));
        assert!(is_valid_token_key("a1.b2_c"));
        assert!(is_valid_token_key("abc123.def_ghi"));
    }

    #[test]
    fn test_invalid_token_keys() {
        // Empty string
        assert!(!is_valid_token_key(""));
        // Starts with digit
        assert!(!is_valid_token_key("1color.text"));
        // Uppercase in first segment
        assert!(!is_valid_token_key("Color.text.primary"));
        // Uppercase in subsequent segment
        assert!(!is_valid_token_key("color.Text.primary"));
        // First segment contains underscore (not allowed)
        assert!(!is_valid_token_key("color_bg.primary"));
        // Trailing dot
        assert!(!is_valid_token_key("color.text."));
        // Leading dot
        assert!(!is_valid_token_key(".color.text"));
        // Empty segment
        assert!(!is_valid_token_key("color..text"));
        // Kebab-case in first segment
        assert!(!is_valid_token_key("color-bg.primary"));
        // No dot (single segment only) — single segment is valid if it matches first-segment rules
        assert!(is_valid_token_key("spacing"));
        // Hyphen in subsequent segment
        assert!(!is_valid_token_key("color.text-primary"));
    }

    // ── Color hex parsing ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_color_hex_rrggbb() {
        let rgba = parse_color_hex("#FFFFFF").unwrap();
        assert!((rgba.r - 1.0).abs() < 1e-4);
        assert!((rgba.g - 1.0).abs() < 1e-4);
        assert!((rgba.b - 1.0).abs() < 1e-4);
        assert!((rgba.a - 1.0).abs() < 1e-4);
    }

    #[test]
    fn test_parse_color_hex_rrggbb_black() {
        let rgba = parse_color_hex("#000000").unwrap();
        assert!((rgba.r).abs() < 1e-4);
        assert!((rgba.g).abs() < 1e-4);
        assert!((rgba.b).abs() < 1e-4);
        assert!((rgba.a - 1.0).abs() < 1e-4);
    }

    #[test]
    fn test_parse_color_hex_rrggbbaa() {
        let rgba = parse_color_hex("#00000099").unwrap();
        assert!((rgba.r).abs() < 1e-4);
        assert!((rgba.g).abs() < 1e-4);
        assert!((rgba.b).abs() < 1e-4);
        // 0x99 = 153 → 153/255 ≈ 0.6
        assert!((rgba.a - 153.0 / 255.0).abs() < 1e-3);
    }

    #[test]
    fn test_parse_color_hex_invalid() {
        assert!(parse_color_hex("FFFFFF").is_none()); // no leading #
        assert!(parse_color_hex("#FFF").is_none()); // too short
        assert!(parse_color_hex("#GGGGGG").is_none()); // invalid hex
        assert!(parse_color_hex("").is_none());
        assert!(parse_color_hex("not-a-color").is_none());
    }

    #[test]
    fn test_parse_color_hex_rejects_non_ascii() {
        // Multi-byte UTF-8 inputs must not panic — they must return None.
        // A naïve byte-length check (e.g. `hex.len() == 6`) can produce a
        // valid length match with multi-byte chars while the byte offsets
        // don't align with character boundaries, causing a panic.
        assert!(parse_color_hex("#你好").is_none());
        assert!(parse_color_hex("#\u{1F600}\u{1F600}").is_none());
    }

    // ── Numeric parsing ───────────────────────────────────────────────────────

    #[test]
    fn test_parse_numeric_integer() {
        let n = parse_numeric("16").unwrap();
        assert!((n - 16.0).abs() < 1e-4);
    }

    #[test]
    fn test_parse_numeric_decimal() {
        let n = parse_numeric("1.5").unwrap();
        assert!((n - 1.5).abs() < 1e-4);
    }

    #[test]
    fn test_parse_numeric_invalid() {
        assert!(parse_numeric("abc").is_none());
        assert!(parse_numeric("").is_none());
        assert!(parse_numeric("1.2.3").is_none());
    }

    #[test]
    fn test_parse_numeric_rejects_nan_and_infinity() {
        // Spec: NaN and infinity strings MUST be rejected
        assert!(parse_numeric("nan").is_none());
        assert!(parse_numeric("NaN").is_none());
        assert!(parse_numeric("inf").is_none());
        assert!(parse_numeric("infinity").is_none());
        assert!(parse_numeric("-inf").is_none());
    }

    #[test]
    fn test_parse_numeric_rejects_whitespace() {
        // Spec: leading/trailing whitespace MUST NOT be permitted
        assert!(parse_numeric(" 16").is_none());
        assert!(parse_numeric("16 ").is_none());
        assert!(parse_numeric(" 1.5 ").is_none());
    }

    // ── Font family parsing ───────────────────────────────────────────────────

    #[test]
    fn test_parse_font_family_keywords() {
        use tze_hud_scene::types::FontFamily;
        // Both "system-ui" and "sans-serif" map to SystemSansSerif
        assert_eq!(
            parse_font_family("system-ui"),
            Some(FontFamily::SystemSansSerif)
        );
        assert_eq!(
            parse_font_family("sans-serif"),
            Some(FontFamily::SystemSansSerif)
        );
        assert_eq!(parse_font_family("serif"), Some(FontFamily::SystemSerif));
        assert_eq!(
            parse_font_family("monospace"),
            Some(FontFamily::SystemMonospace)
        );
        assert!(parse_font_family("Arial").is_none());
        assert!(parse_font_family("").is_none());
        // Whitespace is NOT trimmed — must match exactly
        assert!(parse_font_family(" sans-serif").is_none());
    }

    // ── parse_token_value dispatch ────────────────────────────────────────────

    #[test]
    fn test_parse_token_value_color() {
        let tv = parse_token_value("#FF0000");
        assert!(matches!(tv, TokenValue::Color(_)));
    }

    #[test]
    fn test_parse_token_value_numeric() {
        let tv = parse_token_value("16");
        assert!(matches!(tv, TokenValue::Numeric(n) if (n - 16.0).abs() < 1e-4));
    }

    #[test]
    fn test_parse_token_value_font() {
        use tze_hud_scene::types::FontFamily;
        let tv = parse_token_value("monospace");
        assert_eq!(tv, TokenValue::Font(FontFamily::SystemMonospace));
    }

    #[test]
    fn test_parse_token_value_literal() {
        let tv = parse_token_value("my-custom-value");
        assert_eq!(tv, TokenValue::Literal("my-custom-value".to_string()));
    }

    // ── Fallback resolution ───────────────────────────────────────────────────

    #[test]
    fn test_resolve_tokens_canonical_fallbacks_present() {
        let map = resolve_tokens(&DesignTokenMap::new(), &DesignTokenMap::new());
        // All canonical tokens must be present
        for token in CANONICAL_TOKENS {
            assert!(
                map.contains_key(token.key),
                "canonical token '{}' missing from resolved map",
                token.key
            );
            assert_eq!(
                map[token.key], token.default_value,
                "canonical token '{}' has wrong default",
                token.key
            );
        }
    }

    #[test]
    fn test_resolve_tokens_config_overrides_fallback() {
        let mut config_tokens = DesignTokenMap::new();
        config_tokens.insert("color.text.primary".to_string(), "#FF0000".to_string());
        let map = resolve_tokens(&DesignTokenMap::new(), &config_tokens);
        assert_eq!(map["color.text.primary"], "#FF0000");
    }

    /// Precedence is canonical < theme < config.
    #[test]
    fn test_resolve_tokens_layer_order() {
        let mut theme = DesignTokenMap::new();
        theme.insert("color.text.primary".into(), "#00FF00".into());
        theme.insert("color.text.secondary".into(), "#00FF00".into());
        let mut config = DesignTokenMap::new();
        config.insert("color.text.primary".into(), "#FF0000".into());
        let map = resolve_tokens(&theme, &config);
        assert_eq!(map["color.text.primary"], "#FF0000", "config beats theme");
        assert_eq!(
            map["color.text.secondary"], "#00FF00",
            "theme beats canonical"
        );
        assert_eq!(
            map["color.text.accent"],
            canonical_token("color.text.accent").unwrap().default_value,
            "canonical fills the rest"
        );
    }

    #[test]
    fn test_resolve_tokens_non_canonical_keys_accepted() {
        let mut config_tokens = DesignTokenMap::new();
        config_tokens.insert("custom.brand.color".to_string(), "#ABCDEF".to_string());
        let map = resolve_tokens(&DesignTokenMap::new(), &config_tokens);
        assert_eq!(map["custom.brand.color"], "#ABCDEF");
    }

    #[test]
    fn test_canonical_keys_unique_and_defaults_valid() {
        let mut seen = std::collections::HashSet::new();
        for t in CANONICAL_TOKENS {
            assert!(seen.insert(t.key), "duplicate canonical key {}", t.key);
            assert_eq!(
                validate_canonical_value(t.key, t.default_value),
                Ok(()),
                "canonical default of {} must be valid",
                t.key
            );
        }
    }

    #[test]
    fn test_validate_canonical_value_kinds() {
        assert!(validate_canonical_value("color.surface", "#123456").is_ok());
        assert!(validate_canonical_value("color.surface", "blue").is_err());
        assert!(validate_canonical_value("shape.m", "12").is_ok());
        assert!(validate_canonical_value("shape.m", "12px").is_err());
        assert!(validate_canonical_value("motion.enter.easing", "decelerate").is_ok());
        assert!(validate_canonical_value("motion.enter.easing", "bouncy").is_err());
        // Family names are free-form, not a closed keyword set.
        assert!(validate_canonical_value("font.sans", "Some Custom Face").is_ok());
        assert!(validate_canonical_value("type.body.m.family", "Inter").is_ok());
        assert!(validate_canonical_value("font.mono", "").is_err());
        assert!(validate_canonical_value("not.a.token", "1").is_err());
    }

    // ── validate_design_tokens ────────────────────────────────────────────────

    #[test]
    fn test_validate_no_design_tokens_section_ok() {
        let raw = RawConfig::default();
        let mut errors = Vec::new();
        validate_design_tokens(&raw, &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_validate_valid_keys_no_errors() {
        use crate::raw::RawDesignTokens;
        let mut tokens = HashMap::new();
        tokens.insert("color.text.primary".to_string(), "#FFFFFF".to_string());
        tokens.insert("spacing.unit".to_string(), "8".to_string());
        let raw = RawConfig {
            design_tokens: Some(RawDesignTokens(tokens)),
            ..RawConfig::default()
        };
        let mut errors = Vec::new();
        validate_design_tokens(&raw, &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_validate_invalid_key_produces_error() {
        use crate::raw::RawDesignTokens;
        let mut tokens = HashMap::new();
        tokens.insert("Color.Text.Primary".to_string(), "#FFFFFF".to_string()); // uppercase
        let raw = RawConfig {
            design_tokens: Some(RawDesignTokens(tokens)),
            ..RawConfig::default()
        };
        let mut errors = Vec::new();
        validate_design_tokens(&raw, &mut errors);
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0].code, ConfigErrorCode::InvalidTokenKey));
    }

    #[test]
    fn test_validate_multiple_invalid_keys_all_reported() {
        use crate::raw::RawDesignTokens;
        let mut tokens = HashMap::new();
        tokens.insert("1bad.key".to_string(), "value".to_string());
        tokens.insert("also-bad".to_string(), "value".to_string());
        tokens.insert("color.text.primary".to_string(), "#FFF".to_string()); // valid
        let raw = RawConfig {
            design_tokens: Some(RawDesignTokens(tokens)),
            ..RawConfig::default()
        };
        let mut errors = Vec::new();
        validate_design_tokens(&raw, &mut errors);
        // Exactly 2 errors (the 2 invalid keys)
        let invalid_token_errors: Vec<_> = errors
            .iter()
            .filter(|e| matches!(e.code, ConfigErrorCode::InvalidTokenKey))
            .collect();
        assert_eq!(invalid_token_errors.len(), 2);
    }

    // ── Parse error reporting ─────────────────────────────────────────────────

    #[test]
    fn test_parse_error_code_exists() {
        // Verify that TOKEN_VALUE_PARSE_ERROR code can be constructed
        let _err = ConfigError {
            code: ConfigErrorCode::TokenValueParseError,
            field_path: "design_tokens.color.text.primary".to_string(),
            expected: "color hex #RRGGBB or #RRGGBBAA".to_string(),
            got: "not-a-color".to_string(),
            hint: "use a hex color like #FF0000".to_string(),
        };
    }
}
