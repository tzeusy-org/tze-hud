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

use std::borrow::Cow;
use std::collections::HashMap;

use tze_hud_scene::config::{ConfigError, ConfigErrorCode};
use tze_hud_scene::types::FontFamily;

use crate::raw::RawConfig;

// ─── DesignTokenMap ────────────────────────────────────────────────────────────

/// A flat, immutable (after startup) map of design tokens.
///
/// Keys follow the pattern `[a-z][a-z0-9]*(\.[a-z][a-z0-9_]*)*`.
/// Values are opaque strings until parsed by the consumer for their expected type.
pub type DesignTokenMap = HashMap<String, String>;

// ─── Token key validation ─────────────────────────────────────────────────────

/// Returns `true` if the key matches `^[a-z][a-z0-9]*(\.[a-z][a-z0-9_]*)*$`.
///
/// Segment rules:
/// - First segment: starts with `[a-z]`, followed by `[a-z0-9]*`
/// - Subsequent segments (after `.`): starts with `[a-z]`, followed by `[a-z0-9_]*`
fn is_valid_token_key(key: &str) -> bool {
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
/// Supported keywords:
/// - `"system-ui"` → `FontFamily::SystemSansSerif`
/// - `"sans-serif"` → `FontFamily::SystemSansSerif`
/// - `"monospace"` → `FontFamily::SystemMonospace`
/// - `"serif"` → `FontFamily::SystemSerif`
///
/// Any other value returns `None`. This is intentionally a free function
/// (not a method on `FontFamily`) because `FontFamily` is defined in
/// `tze_hud_scene::types` and we use it directly.
fn font_family_from_keyword(s: &str) -> Option<FontFamily> {
    match s {
        "system-ui" | "sans-serif" => Some(FontFamily::SystemSansSerif),
        "monospace" => Some(FontFamily::SystemMonospace),
        "serif" => Some(FontFamily::SystemSerif),
        _ => None,
    }
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
/// Leading/trailing whitespace is NOT permitted.
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
pub(crate) fn parse_font_family(s: &str) -> Option<FontFamily> {
    font_family_from_keyword(s)
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
/// does not specify them. This table defines each key's format and fallback.
///
/// Token groups: color, opacity, typography, spacing, stroke, border.radius,
/// portal divider, claimed-tile placement (`tile.*`) and the control panel
/// layout (`control_panel.*`).
pub static CANONICAL_TOKENS: &[CanonicalToken] = &[
    CanonicalToken {
        key: "tray.glyph.on_light",
        description: "Notification-area glyph on a light Windows taskbar",
        default_value: "#18202C",
    },
    CanonicalToken {
        key: "tray.glyph.on_dark",
        description: "Notification-area glyph on a dark Windows taskbar",
        default_value: "#E8EBF0",
    },
    // Color — text
    CanonicalToken {
        key: "color.text.primary",
        description: "Primary text color",
        default_value: "#FFFFFF",
    },
    CanonicalToken {
        key: "color.text.accent",
        description: "Accent/highlight text color",
        default_value: "#4A9EFF",
    },
    // Color — outline
    // Color — border
    CanonicalToken {
        key: "color.border.default",
        description: "Default border/frame color",
        default_value: "#333333",
    },
    CanonicalToken {
        key: "color.subtitle.backdrop",
        description: "Fill behind the whole subtitle zone, drawn when opacity.subtitle.backdrop > 0",
        default_value: "#000000",
    },
    // Color — severity: the alert banner's severity mark (a ringed glyph
    // before its text); a transparent value draws no mark. Defaults are
    // Tonal Glass's primary, caution and error roles.
    CanonicalToken {
        key: "color.severity.info",
        description: "Alert banner severity mark at urgency 0-1",
        default_value: "#8AB4FF",
    },
    CanonicalToken {
        key: "color.severity.warning",
        description: "Alert banner severity mark at urgency 2",
        default_value: "#F2C14E",
    },
    CanonicalToken {
        key: "color.severity.critical",
        description: "Alert banner severity mark at urgency 3",
        default_value: "#FF8A80",
    },
    // Opacity
    CanonicalToken {
        key: "opacity.backdrop.default",
        description: "Default backdrop opacity (0.0–1.0)",
        default_value: "0.6",
    },
    CanonicalToken {
        key: "opacity.subtitle.backdrop",
        description: "Opacity of the fill behind the whole subtitle zone (0.0–1.0); 0 draws none",
        default_value: "0",
    },
    CanonicalToken {
        key: "opacity.ambient.max",
        description: "Ceiling on ambient-background opacity (0.0–1.0), so the desktop always shows through the overlay",
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
    CanonicalToken {
        key: "space.subtitle.bottom",
        description: "Bottom inset of the default subtitle zone (dp)",
        default_value: "48",
    },
    // Subtitle pill roles; generic surface, scrim, corner, hairline and caption type are shared.
    CanonicalToken {
        key: "opacity.subtitle",
        description: "Visibility multiplier of the themed subtitle pill (Classic: 0); fill opacity is opacity.scrim",
        default_value: "1",
    },
    CanonicalToken {
        key: "space.subtitle.horizontal",
        description: "Subtitle pill horizontal padding (dp)",
        default_value: "20",
    },
    CanonicalToken {
        key: "space.subtitle.vertical",
        description: "Subtitle pill vertical padding (dp)",
        default_value: "10",
    },
    CanonicalToken {
        key: "size.subtitle.max_width",
        description: "Maximum subtitle pill outer width (dp), bounded by its zone; 0 uses zone width",
        default_value: "760",
    },
    CanonicalToken {
        key: "stroke.subtitle.text",
        description: "Subtitle text outline width (dp); Classic retains its legacy outline",
        default_value: "0",
    },
    CanonicalToken {
        key: "color.subtitle.outline",
        description: "Subtitle text outline color (Classic readability)",
        default_value: "#000000",
    },
    CanonicalToken {
        key: "space.status.margin",
        description: "Status pill outer bottom and right inset (dp)",
        default_value: "16",
    },
    CanonicalToken {
        key: "space.status.horizontal",
        description: "Status pill side padding (dp)",
        default_value: "14",
    },
    CanonicalToken {
        key: "space.status.key_value",
        description: "Status pill key/value and icon/text gap (dp)",
        default_value: "6",
    },
    CanonicalToken {
        key: "size.status.height",
        description: "Status pill height (dp)",
        default_value: "36",
    },
    CanonicalToken {
        key: "size.status.icon",
        description: "Explicit owner status icon size (dp)",
        default_value: "14",
    },
    CanonicalToken {
        key: "size.status.divider",
        description: "Status pill divider height (dp)",
        default_value: "14",
    },
    CanonicalToken {
        key: "size.status.entry_max",
        description: "Maximum status entry width before ellipsis (dp)",
        default_value: "240",
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
    CanonicalToken {
        key: "state.hidden.opacity",
        description: "Hidden widget state",
        default_value: "0",
    },
    CanonicalToken {
        key: "state.visible.opacity",
        description: "Visible widget state",
        default_value: "1",
    },
    // Approved gauge geometry, in logical SVG units. The viewBox and all layers
    // share these values; the widget instance supplies the final physical box.
    CanonicalToken {
        key: "gauge.width",
        description: "Gauge logical card width",
        default_value: "128",
    },
    CanonicalToken {
        key: "gauge.height",
        description: "Gauge logical card height",
        default_value: "132",
    },
    CanonicalToken {
        key: "gauge.edge.inset",
        description: "Gauge hairline inset",
        default_value: "0.5",
    },
    CanonicalToken {
        key: "gauge.edge.width",
        description: "Gauge inset card width",
        default_value: "127",
    },
    CanonicalToken {
        key: "gauge.edge.height",
        description: "Gauge inset card height",
        default_value: "131",
    },
    CanonicalToken {
        key: "gauge.ring.cx",
        description: "Gauge ring horizontal centre",
        default_value: "64",
    },
    CanonicalToken {
        key: "gauge.ring.cy",
        description: "Gauge ring vertical centre",
        default_value: "56",
    },
    CanonicalToken {
        key: "gauge.ring.radius",
        description: "Gauge ring centreline radius",
        default_value: "34",
    },
    CanonicalToken {
        key: "gauge.ring.stroke",
        description: "Gauge ring stroke width",
        default_value: "6",
    },
    CanonicalToken {
        key: "gauge.ring.circumference",
        description: "Gauge ring dash length; keep in sync with ring radius",
        default_value: "213.63",
    },
    CanonicalToken {
        key: "gauge.caption.y",
        description: "Gauge caption baseline",
        default_value: "112",
    },
    CanonicalToken {
        key: "gauge.hover.delay_ms",
        description: "Gauge hover dwell in milliseconds",
        default_value: "180",
    },
    // Approved progress card and status pill geometry, in logical SVG units.
    CanonicalToken {
        key: "progress.width",
        description: "Progress logical width",
        default_value: "320",
    },
    CanonicalToken {
        key: "progress.height",
        description: "Progress logical height",
        default_value: "50",
    },
    CanonicalToken {
        key: "progress.edge.inset",
        description: "Progress hairline inset",
        default_value: "0.5",
    },
    CanonicalToken {
        key: "progress.edge.width",
        description: "Progress inset card width",
        default_value: "319",
    },
    CanonicalToken {
        key: "progress.edge.height",
        description: "Progress inset card height",
        default_value: "49",
    },
    CanonicalToken {
        key: "progress.content.x",
        description: "Progress label and track left edge",
        default_value: "15",
    },
    CanonicalToken {
        key: "progress.text.y",
        description: "Progress label and readout baseline",
        default_value: "23",
    },
    CanonicalToken {
        key: "progress.label.width",
        description: "Progress label clip width, reserving space for the percentage",
        default_value: "240",
    },
    CanonicalToken {
        key: "progress.track.y",
        description: "Progress track top",
        default_value: "35",
    },
    CanonicalToken {
        key: "progress.track.width",
        description: "Progress track and full fill width",
        default_value: "290",
    },
    CanonicalToken {
        key: "progress.track.height",
        description: "Progress track thickness",
        default_value: "4",
    },
    CanonicalToken {
        key: "progress.track.radius",
        description: "Progress track corner radius",
        default_value: "2",
    },
    CanonicalToken {
        key: "progress.readout.x",
        description: "Progress percentage right edge",
        default_value: "305",
    },
    CanonicalToken {
        key: "progress.hover.delay_ms",
        description: "Progress hover dwell in milliseconds",
        default_value: "180",
    },
    CanonicalToken {
        key: "status.width",
        description: "Status logical canvas width",
        default_value: "320",
    },
    CanonicalToken {
        key: "status.height",
        description: "Status logical canvas height including tooltip",
        default_value: "124",
    },
    CanonicalToken {
        key: "status.edge.inset",
        description: "Status hairline inset",
        default_value: "0.5",
    },
    CanonicalToken {
        key: "status.edge.width",
        description: "Status inset pill and tooltip width",
        default_value: "319",
    },
    CanonicalToken {
        key: "status.edge.height",
        description: "Status inset pill height",
        default_value: "39",
    },
    CanonicalToken {
        key: "status.pill.radius",
        description: "Status pill circular end radius, half its inset height",
        default_value: "19.5",
    },
    CanonicalToken {
        key: "status.marker.x",
        description: "Status marker centre x",
        default_value: "17",
    },
    CanonicalToken {
        key: "status.marker.y",
        description: "Status marker centre y",
        default_value: "20",
    },
    CanonicalToken {
        key: "status.marker.radius",
        description: "Minimal status marker radius",
        default_value: "5",
    },
    CanonicalToken {
        key: "status.marker.disc.radius",
        description: "System and friendly marker disc radius",
        default_value: "9",
    },
    CanonicalToken {
        key: "status.marker.disc.opacity",
        description: "System and friendly tonal marker opacity",
        default_value: "0.22",
    },
    CanonicalToken {
        key: "status.marker.glyph.y",
        description: "Status marker glyph baseline",
        default_value: "24",
    },
    CanonicalToken {
        key: "status.marker.glyph.size",
        description: "Status marker glyph size",
        default_value: "12",
    },
    CanonicalToken {
        key: "status.name.x",
        description: "Status name left edge",
        default_value: "34",
    },
    CanonicalToken {
        key: "status.text.y",
        description: "Status name and reason baseline",
        default_value: "25",
    },
    CanonicalToken {
        key: "status.separator.x",
        description: "Status reason separator x",
        default_value: "93",
    },
    CanonicalToken {
        key: "status.separator.y",
        description: "Status separator top",
        default_value: "12",
    },
    CanonicalToken {
        key: "status.separator.height",
        description: "Status separator height",
        default_value: "16",
    },
    CanonicalToken {
        key: "status.reason.x",
        description: "Status inline reason left edge",
        default_value: "106",
    },
    CanonicalToken {
        key: "status.reason.width",
        description: "Status inline reason clip width",
        default_value: "200",
    },
    CanonicalToken {
        key: "status.text.clip.y",
        description: "Status inline text clip top",
        default_value: "8",
    },
    CanonicalToken {
        key: "status.text.clip.height",
        description: "Status inline text clip height",
        default_value: "24",
    },
    CanonicalToken {
        key: "status.tooltip.y",
        description: "Status tooltip top",
        default_value: "48.5",
    },
    CanonicalToken {
        key: "status.tooltip.height",
        description: "Status tooltip height",
        default_value: "75",
    },
    CanonicalToken {
        key: "status.tooltip.dot.x",
        description: "Status tooltip marker x",
        default_value: "20",
    },
    CanonicalToken {
        key: "status.tooltip.dot.y",
        description: "Status tooltip marker y",
        default_value: "71",
    },
    CanonicalToken {
        key: "status.tooltip.dot.radius",
        description: "Status tooltip marker radius",
        default_value: "4",
    },
    CanonicalToken {
        key: "status.tooltip.label.x",
        description: "Status tooltip label x",
        default_value: "32",
    },
    CanonicalToken {
        key: "status.tooltip.label.y",
        description: "Status tooltip label baseline",
        default_value: "77",
    },
    CanonicalToken {
        key: "status.tooltip.reason.x",
        description: "Status tooltip reason x",
        default_value: "16",
    },
    CanonicalToken {
        key: "status.tooltip.reason.y",
        description: "Status tooltip reason baseline",
        default_value: "104",
    },
    CanonicalToken {
        key: "status.hover.delay_ms",
        description: "Status hover dwell in milliseconds",
        default_value: "180",
    },
    CanonicalToken {
        key: "status.hover.height_pct",
        description: "Status pill fraction of its tooltip canvas",
        default_value: "0.32258065",
    },
    // Runtime drag affordances
    CanonicalToken {
        key: "size.drag.handle_width",
        description: "Move pill width (dp)",
        default_value: "32",
    },
    CanonicalToken {
        key: "size.drag.handle_height",
        description: "Move pill height (dp)",
        default_value: "4",
    },
    CanonicalToken {
        key: "size.drag.hit_height",
        description: "Move pill hit row height (dp)",
        default_value: "12",
    },
    CanonicalToken {
        key: "size.drag.resize_target",
        description: "Pip resize hit target (dp)",
        default_value: "24",
    },
    CanonicalToken {
        key: "opacity.drag.hover",
        description: "Move pill opacity while its element is hovered",
        default_value: "0.5",
    },
    CanonicalToken {
        key: "opacity.drag.active",
        description: "Move pill opacity while pressed or dragged",
        default_value: "1",
    },
    CanonicalToken {
        key: "opacity.drag.highlight",
        description: "Active element outline opacity",
        default_value: "0.9",
    },
    // Stroke
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
        key: "border.radius.large",
        description: "Large corner radius in pixels",
        default_value: "16",
    },
    // Portal transcript turn separator. Consumed by the compositor
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
    // Agent tile backdrop by root node kind (defaults match the compositor
    // fallbacks).
    CanonicalToken {
        key: "color.tile.background.text_markdown",
        description: "Agent tile backdrop behind a text root",
        default_value: "#6C6C88",
    },
    CanonicalToken {
        key: "color.tile.background.static_image",
        description: "Agent tile backdrop behind an image root",
        default_value: "#373737",
    },
    CanonicalToken {
        key: "color.tile.background.default",
        description: "Agent tile backdrop behind any other root",
        default_value: "#505073",
    },
    // Orphaned-tile disconnection badge (docs/invariants.md section 4).
    CanonicalToken {
        key: "tile.disconnect_badge.color",
        description: "Disconnection badge color on orphaned tiles",
        default_value: "#FFB800",
    },
    CanonicalToken {
        key: "tile.disconnect_badge.width_px",
        description: "Disconnection accent bar width along an orphaned tile's left edge (px)",
        default_value: "4",
    },
    CanonicalToken {
        key: "tile.disconnect_badge.gap_px",
        description: "Gap between an orphaned tile's left edge and its accent bar (px)",
        default_value: "3",
    },
    CanonicalToken {
        key: "tile.disconnect_badge.label_background",
        description: "Background of the 'Disconnected' strip along an orphaned tile's bottom",
        default_value: "#000000B3",
    },
    CanonicalToken {
        key: "tile.disconnect_badge.label_color",
        description: "Text color of the 'Disconnected' strip label",
        default_value: "#FFB800",
    },
    CanonicalToken {
        key: "tile.disconnect_badge.label_weight",
        description: "Font weight of the 'Disconnected' strip label (CSS numeric)",
        default_value: "600",
    },
    CanonicalToken {
        key: "tile.disconnect_badge.label_size_px",
        description: "Font size of the 'Disconnected' strip label (px)",
        default_value: "18",
    },
    CanonicalToken {
        key: "tile.disconnect_badge.label_padding_px",
        description: "Padding around the 'Disconnected' strip label (px)",
        default_value: "6",
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
        key: "notification.action.link_mark",
        description: "Trailing mark on a notification HTTPS link button",
        default_value: "↗",
    },
    CanonicalToken {
        key: "link.feedback.ms",
        description: "How long a pressed link shows Opened or Couldn't open (ms)",
        default_value: "1500",
    },
    CanonicalToken {
        key: "notification.goto.feedback.ms",
        description: "How long the result of Go to session stays visible (ms)",
        default_value: "1200",
    },
    CanonicalToken {
        key: "notification.copy.feedback.ms",
        description: "How long a notification button shows its copy result (ms)",
        default_value: "1200",
    },
    CanonicalToken {
        key: "notification.reveal.max_height",
        description: "Maximum height of the full notification text panel",
        default_value: "192",
    },
    CanonicalToken {
        key: NOTIFICATION_ACTION_HOLD_MS_KEY,
        description: "How long a destructive notification action must be held (ms, 0-3000; 0 = no hold)",
        default_value: "600",
    },
    CanonicalToken {
        key: NOTIFICATION_PRESS_GUARD_MS_KEY,
        description: "How long a card that arrives under a resting pointer ignores pointer presses (ms, 0-3000; 0 = no guard)",
        default_value: "400",
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
        key: "color.caution.outline",
        description: "Outline of a caution container (e.g. a paused control)",
        default_value: "#6B5518",
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
        key: "tile.hit_region.hover.color",
        description: "Hover tint for a tile hit region without a local override",
        default_value: "#FFFFFF",
    },
    CanonicalToken {
        key: "tile.hit_region.hover.opacity",
        description: "Opacity of the tile hit-region hover tint (0.0-1.0)",
        default_value: "0.1",
    },
    CanonicalToken {
        key: "tile.hit_region.pressed.color",
        description: "Pressed tint for a tile hit region without a local override",
        default_value: "#000000",
    },
    CanonicalToken {
        key: "tile.hit_region.pressed.opacity",
        description: "Opacity of the tile hit-region pressed tint (0.0-1.0)",
        default_value: "0.15",
    },
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
    // Control panel layout
    CanonicalToken {
        key: "control_panel.inset",
        description: "Gap between the control panel and each display edge (px)",
        default_value: "32",
    },
    CanonicalToken {
        key: "control_panel.column.min_width",
        description: "Narrowest control panel section column; as many as fit are used (px)",
        default_value: "360",
    },
    // Global UI scale
    CanonicalToken {
        key: UI_SCALE_KEY,
        description: "Global UI scale: multiplies every logical (dp) size on top of each \
                      monitor's Windows scale factor (0.5–3.0; config only, not themes)",
        default_value: "1",
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
    // Gauge ring readout: the approved ring centres an 18dp
    // percent, larger than the shared 14dp readout used by countdown/kpi.
    // Family and weight follow type.readout.
    CanonicalToken {
        key: "type.gauge.readout.size",
        description: "Gauge ring centred percent: font size (dp)",
        default_value: "18",
    },
    // Notification card roles: the card's own fill, hairline,
    // text, opacity, corner, padding and type, so a theme can style cards
    // without restyling every surface that shares the generic roles.
    // Defaults are Tonal Glass's generic values.
    CanonicalToken {
        key: "color.notification.container.low",
        description: "Notification card fill at urgency 0",
        default_value: "#222833",
    },
    CanonicalToken {
        key: "color.notification.container",
        description: "Notification card fill at urgency 1",
        default_value: "#222833",
    },
    CanonicalToken {
        key: "color.notification.outline",
        description: "Notification card hairline at urgency 0-1",
        default_value: "#343B47",
    },
    CanonicalToken {
        key: "color.notification.caution.outline",
        description: "Notification card hairline at urgency 2",
        default_value: "#6B5518",
    },
    CanonicalToken {
        key: "color.notification.error.outline",
        description: "Notification card hairline at urgency 3",
        default_value: "#FF8A80",
    },
    CanonicalToken {
        key: "color.on_notification.variant",
        description: "Notification card body at urgency 0-1",
        default_value: "#A9B1BE",
    },
    CanonicalToken {
        key: "color.on_notification.caution",
        description: "Notification card text at urgency 2",
        default_value: "#FFE7A8",
    },
    CanonicalToken {
        key: "color.on_notification.error",
        description: "Notification card text at urgency 3",
        default_value: "#FFDAD6",
    },
    CanonicalToken {
        key: "opacity.notification",
        description: "Notification card fill opacity (0.0-1.0)",
        default_value: "0.92",
    },
    CanonicalToken {
        key: "shape.notification",
        description: "Notification card corner radius (dp)",
        default_value: "16",
    },
    CanonicalToken {
        key: "space.notification",
        description: "Notification card padding (dp)",
        default_value: "16",
    },
    CanonicalToken {
        key: "type.notification.title.size",
        description: "Notification card title: font size (dp)",
        default_value: "16",
    },
    CanonicalToken {
        key: "type.notification.title.line_height",
        description: "Notification card title: line height (dp)",
        default_value: "24",
    },
    CanonicalToken {
        key: "type.notification.title.weight",
        description: "Notification card title: font weight (CSS numeric)",
        default_value: "600",
    },
    CanonicalToken {
        key: "type.notification.body.size",
        description: "Notification card body: font size (dp)",
        default_value: "14",
    },
    CanonicalToken {
        key: "type.notification.body.line_height",
        description: "Notification card body: line height (dp)",
        default_value: "20",
    },
    CanonicalToken {
        key: "type.notification.body.weight",
        description: "Notification card body: font weight (CSS numeric)",
        default_value: "400",
    },
    // Alert banner roles: the banner's own fill, hairline,
    // text, opacity, corner, margin, padding and type, so a theme can style
    // banners apart from notification cards and the generic roles. Defaults
    // are Tonal Glass's generic values.
    CanonicalToken {
        key: "color.alert.container",
        description: "Alert banner fill at urgency 0-1",
        default_value: "#222833",
    },
    CanonicalToken {
        key: "color.alert.caution.container",
        description: "Alert banner fill at urgency 2",
        default_value: "#222833",
    },
    CanonicalToken {
        key: "color.alert.error.container",
        description: "Alert banner fill at urgency 3",
        default_value: "#5C1A17",
    },
    CanonicalToken {
        key: "color.alert.outline",
        description: "Alert banner hairline (transparent: none)",
        default_value: "#343B47",
    },
    CanonicalToken {
        key: "color.on_alert",
        description: "Alert banner title (and a lone message line) at urgency 0-2",
        default_value: "#E8EBF0",
    },
    CanonicalToken {
        key: "color.on_alert.variant",
        description: "Alert banner body under a title at urgency 0-2",
        default_value: "#A9B1BE",
    },
    CanonicalToken {
        key: "color.on_alert.error",
        description: "Alert banner text at urgency 3",
        default_value: "#FFDAD6",
    },
    CanonicalToken {
        key: "opacity.alert",
        description: "Alert banner fill opacity (0.0-1.0)",
        default_value: "0.92",
    },
    CanonicalToken {
        key: "shape.alert",
        description: "Alert banner corner radius (dp)",
        default_value: "12",
    },
    CanonicalToken {
        key: "space.alert.margin",
        description: "Alert banner inset from the zone's sides and top, and between banners (dp)",
        default_value: "16",
    },
    CanonicalToken {
        key: "space.alert.horizontal",
        description: "Alert banner horizontal padding (dp)",
        default_value: "16",
    },
    CanonicalToken {
        key: "space.alert.vertical",
        description: "Alert banner vertical padding (dp)",
        default_value: "12",
    },
    CanonicalToken {
        key: "type.alert.title.size",
        description: "Alert banner title (and a lone message line): font size (dp)",
        default_value: "16",
    },
    CanonicalToken {
        key: "type.alert.title.line_height",
        description: "Alert banner title: line height (dp)",
        default_value: "24",
    },
    CanonicalToken {
        key: "type.alert.title.weight",
        description: "Alert banner title: font weight (CSS numeric)",
        default_value: "600",
    },
    CanonicalToken {
        key: "type.alert.body.size",
        description: "Alert banner body under a title: font size (dp)",
        default_value: "14",
    },
    CanonicalToken {
        key: "type.alert.body.line_height",
        description: "Alert banner body: line height (dp)",
        default_value: "20",
    },
    CanonicalToken {
        key: "type.alert.body.weight",
        description: "Alert banner body: font weight (CSS numeric)",
        default_value: "400",
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

/// Resolve one existing motion duration at profile construction, outside the
/// frame loop. Missing values use the canonical token; invalid direct maps
/// keep the same fallback without changing strict config validation.
pub fn resolve_motion_duration_ms(tokens: &DesignTokenMap, key: &str) -> u32 {
    let canonical = canonical_token(key)
        .filter(|_| {
            matches!(
                key,
                "motion.enter.ms" | "motion.exit.ms" | "motion.state.ms"
            )
        })
        .expect("motion duration key must name an existing canonical duration");
    let duration = |value: &str| {
        let value = value.trim();
        let value = value
            .strip_suffix("px")
            .unwrap_or(value)
            .trim()
            .parse::<f64>()
            .ok()?;
        let rounded = value.round();
        (value.is_finite()
            && value >= 0.0
            && value <= f64::from(u32::MAX)
            && rounded >= 0.0
            && rounded <= f64::from(u32::MAX))
        .then_some(rounded as u32)
    };
    if let Some(value) = tokens.get(key) {
        if let Some(ms) = duration(value) {
            return ms;
        }
        tracing::warn!(
            token_key = key,
            "invalid motion duration; using canonical default"
        );
    }
    duration(canonical.default_value).expect("canonical motion duration must be valid")
}

/// How long a destructive notification action must be held before it fires
/// (ms). `0` turns the hold off.
const NOTIFICATION_ACTION_HOLD_MS_KEY: &str = "notification.action.hold.ms";

/// The longest hold [`NOTIFICATION_ACTION_HOLD_MS_KEY`] may ask for.
const MAX_NOTIFICATION_ACTION_HOLD_MS: u32 = 3_000;

fn parse_hold_ms(value: &str) -> Option<u32> {
    value
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|ms| *ms <= MAX_NOTIFICATION_ACTION_HOLD_MS)
}

/// The destructive-action hold in ms from `notification.action.hold.ms`.
/// Uses the canonical default when unset or invalid.
pub fn notification_action_hold_ms(tokens: &DesignTokenMap) -> u32 {
    safety_interval_ms(tokens, NOTIFICATION_ACTION_HOLD_MS_KEY)
}

/// A card that an agent puts under a resting pointer ignores pointer presses
/// on its controls for this long (ms). `0` turns the guard off. A safety
/// interval, not motion: the OS animation preference leaves it alone.
const NOTIFICATION_PRESS_GUARD_MS_KEY: &str = "notification.press_guard.ms";

/// The fresh-card press guard in ms from `notification.press_guard.ms`.
/// Uses the canonical default when unset or invalid.
pub fn notification_press_guard_ms(tokens: &DesignTokenMap) -> u32 {
    safety_interval_ms(tokens, NOTIFICATION_PRESS_GUARD_MS_KEY)
}

fn safety_interval_ms(tokens: &DesignTokenMap, key: &str) -> u32 {
    tokens
        .get(key)
        .and_then(|v| parse_hold_ms(v))
        .unwrap_or_else(|| {
            canonical_token(key)
                .and_then(|token| parse_hold_ms(token.default_value))
                .expect("canonical safety interval must be valid")
        })
}

/// Easing curve names accepted by `motion.*.easing` tokens.
///
/// The compositor's curves (`renderer/easing.rs`) map as: `linear` ->
/// `Linear`, `standard` -> `EaseInOut`, `decelerate` -> `EaseOutQuad`,
/// `accelerate` -> `EaseInQuad`. Whole-zone transitions capture these curves.
const MOTION_EASINGS: &[&str] = &["linear", "standard", "decelerate", "accelerate"];

/// The canonical default of `key`: the one definition every fallback reads.
pub fn canonical_default(key: &str) -> Option<&'static str> {
    canonical_token(key).map(|token| token.default_value)
}

/// Look up a canonical token definition by key.
pub(crate) fn canonical_token(key: &str) -> Option<&'static CanonicalToken> {
    CANONICAL_TOKENS.iter().find(|t| t.key == key)
}

fn is_hit_region_opacity(key: &str) -> bool {
    matches!(
        key,
        "tile.hit_region.hover.opacity" | "tile.hit_region.pressed.opacity"
    )
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
pub(crate) fn validate_canonical_value(key: &str, value: &str) -> Result<(), String> {
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
    } else if key == NOTIFICATION_ACTION_HOLD_MS_KEY || key == NOTIFICATION_PRESS_GUARD_MS_KEY {
        (
            parse_hold_ms(value).is_some(),
            format!("a whole number of ms from 0 to {MAX_NOTIFICATION_ACTION_HOLD_MS}"),
        )
    } else if key == UI_SCALE_KEY {
        let (lo, hi) = UI_SCALE_RANGE;
        (
            parse_numeric(value).is_some_and(|n| (lo..=hi).contains(&n)),
            format!("a number from {lo} to {hi}"),
        )
    } else if is_hit_region_opacity(key) {
        (
            parse_numeric(value).is_some_and(|n| (0.0..=1.0).contains(&n)),
            "a number from 0 to 1".into(),
        )
    } else if parse_numeric(token.default_value).is_some() {
        (parse_numeric(value).is_some(), "a finite number".into())
    } else {
        (!value.is_empty(), "a non-empty value".into())
    };
    if ok { Ok(()) } else { Err(expected) }
}

// ─── Validation ───────────────────────────────────────────────────────────────

/// Token-map keys the runtime sets itself; they are not design values, so
/// `[design_tokens]` may not set them.
///
/// - `runtime.fonts_dir`: absolute `<config dir>/fonts` when it exists
///   (`tze_hud_compositor::fonts::RUNTIME_FONTS_DIR_KEY`).
/// - `runtime.px_scale` ([`PX_SCALE_KEY`]): physical px per logical dp.
/// - `runtime.reduce_motion` ([`OS_REDUCE_MOTION_KEY`]): the Windows
///   animation preference is off ([`apply_os_motion_layer`]).
/// - `runtime.text_scale` ([`TEXT_SCALE_KEY`]) and `runtime.caret_blink_ms`
///   ([`CARET_BLINK_MS_KEY`]): the Windows text size and caret blink time
///   ([`apply_os_text_and_caret`]). Unlike the motion layer, nothing in the
///   config overrides them.
///
/// Every `runtime.px_scale@<zone>` key is reserved too.
pub const RESERVED_RUNTIME_TOKEN_KEYS: &[&str] = &[
    "runtime.fonts_dir",
    PX_SCALE_KEY,
    OS_REDUCE_MOTION_KEY,
    TEXT_SCALE_KEY,
    CARET_BLINK_MS_KEY,
];

/// Whether the runtime, not config, sets `key`.
fn is_reserved_runtime_key(key: &str) -> bool {
    RESERVED_RUNTIME_TOKEN_KEYS.contains(&key) || key.starts_with(ZONE_PX_SCALE_PREFIX)
}

// ─── OS motion preference ─────────────────────────────────────────────────────

/// Runtime-set key, `"true"` while the OS asks for no animation (Windows
/// "Show animations" off, `SPI_GETCLIENTAREAANIMATION`). gRPC widget
/// publishes snap their explicit `transition_ms` to 0 while it is set.
pub const OS_REDUCE_MOTION_KEY: &str = "runtime.reduce_motion";

/// The duration tokens the OS motion layer sets to 0: exactly these, and
/// never one the owner sets in `[design_tokens]`.
pub const OS_MOTION_ZEROED_KEYS: [&str; 5] = [
    "motion.enter.ms",
    "motion.exit.ms",
    "motion.state.ms",
    crate::portal_tokens::PORTAL_TOKEN_TRANSITION_IN_MS,
    crate::portal_tokens::PORTAL_TOKEN_TRANSITION_OUT_MS,
];

/// Apply the OS animation preference as a token layer between the theme and
/// the config overrides: theme → OS preference → `[design_tokens]`.
///
/// `resolved` is a map from [`crate::themes::resolve_config_tokens`] and
/// `config_tokens` the `[design_tokens]` table it was resolved from. With
/// animations enabled this only clears [`OS_REDUCE_MOTION_KEY`]. With them
/// disabled every [`OS_MOTION_ZEROED_KEYS`] key the owner did not set becomes
/// `"0"` and [`OS_REDUCE_MOTION_KEY`] is set. The runtime reads the OS value
/// at startup and on each config reload; there is no live OS watch.
pub fn apply_os_motion_layer(
    resolved: &mut DesignTokenMap,
    config_tokens: &DesignTokenMap,
    animations_enabled: bool,
) {
    if animations_enabled {
        resolved.remove(OS_REDUCE_MOTION_KEY);
        return;
    }
    for key in OS_MOTION_ZEROED_KEYS {
        if !config_tokens.contains_key(key) {
            resolved.insert(key.to_owned(), "0".to_owned());
        }
    }
    resolved.insert(OS_REDUCE_MOTION_KEY.to_owned(), "true".to_owned());
}

// ─── OS text size and caret blink ─────────────────────────────────────────────

/// Runtime-set key: the Windows text size (Settings › Accessibility › Text
/// size) as a factor, 1–2.25; absent means 1. It multiplies `ui.scale`
/// ([`effective_scale`]).
pub const TEXT_SCALE_KEY: &str = "runtime.text_scale";

/// The values [`TEXT_SCALE_KEY`] may take: Windows' 100–225%.
pub const TEXT_SCALE_RANGE: (f32, f32) = (1.0, 2.25);

/// Runtime-set key: the Windows caret blink half-period in ms, 200–1200, or
/// `0` when Windows caret blinking is off (a steady caret); absent means
/// [`DEFAULT_CARET_BLINK_MS`].
pub const CARET_BLINK_MS_KEY: &str = "runtime.caret_blink_ms";

/// The caret blink half-period off Windows or when the setting cannot be
/// read: the conventional editor rate.
pub const DEFAULT_CARET_BLINK_MS: u32 = 530;

/// The blinking half-periods [`CARET_BLINK_MS_KEY`] may take; `0` (steady)
/// is outside it.
pub const CARET_BLINK_MS_RANGE: (u32, u32) = (200, 1200);

/// Record the Windows text size and caret blink time in `tokens`. At the
/// Windows defaults (text size 1, blink [`DEFAULT_CARET_BLINK_MS`]) both
/// keys are removed, so the map is the same as without this layer. The
/// runtime reads both at startup and on each config reload; there is no
/// live OS watch.
pub fn apply_os_text_and_caret(tokens: &mut DesignTokenMap, text_scale: f32, caret_blink_ms: u32) {
    if (text_scale - 1.0).abs() > f32::EPSILON {
        tokens.insert(TEXT_SCALE_KEY.to_owned(), text_scale.to_string());
    } else {
        tokens.remove(TEXT_SCALE_KEY);
    }
    if caret_blink_ms != DEFAULT_CARET_BLINK_MS {
        tokens.insert(CARET_BLINK_MS_KEY.to_owned(), caret_blink_ms.to_string());
    } else {
        tokens.remove(CARET_BLINK_MS_KEY);
    }
}

/// The Windows preferences the HUD follows as token layers, read by the
/// runtime at startup and on each config reload. The default is what the
/// HUD uses off Windows or when a setting cannot be read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OsPreferences {
    /// "Show animations" ([`apply_os_motion_layer`]).
    pub animations_enabled: bool,
    /// Text size as a factor, 1–2.25 ([`TEXT_SCALE_KEY`]).
    pub text_scale: f32,
    /// Caret blink half-period in ms, `0` for steady ([`CARET_BLINK_MS_KEY`]).
    pub caret_blink_ms: u32,
}

impl Default for OsPreferences {
    fn default() -> Self {
        Self {
            animations_enabled: true,
            text_scale: 1.0,
            caret_blink_ms: DEFAULT_CARET_BLINK_MS,
        }
    }
}

/// Apply every Windows preference layer to `resolved`, a map from
/// [`crate::themes::resolve_config_tokens`] of `config_tokens`: the motion
/// layer ([`apply_os_motion_layer`]) and the text size and caret blink
/// ([`apply_os_text_and_caret`]).
pub fn apply_os_preferences(
    resolved: &mut DesignTokenMap,
    config_tokens: &DesignTokenMap,
    os: &OsPreferences,
) {
    apply_os_motion_layer(resolved, config_tokens, os.animations_enabled);
    apply_os_text_and_caret(resolved, os.text_scale, os.caret_blink_ms);
}

/// The map's Windows text size ([`TEXT_SCALE_KEY`]), clamped to
/// [`TEXT_SCALE_RANGE`]; 1 when unset.
pub fn text_scale(tokens: &DesignTokenMap) -> f32 {
    let (lo, hi) = TEXT_SCALE_RANGE;
    tokens
        .get(TEXT_SCALE_KEY)
        .and_then(|v| parse_numeric(v))
        .map_or(1.0, |n| n.clamp(lo, hi))
}

/// The map's caret blink half-period in ms ([`CARET_BLINK_MS_KEY`]): `0`
/// for a steady caret, else clamped to [`CARET_BLINK_MS_RANGE`];
/// [`DEFAULT_CARET_BLINK_MS`] when unset.
pub fn caret_blink_ms(tokens: &DesignTokenMap) -> u32 {
    let (lo, hi) = CARET_BLINK_MS_RANGE;
    match tokens
        .get(CARET_BLINK_MS_KEY)
        .and_then(|v| v.parse::<u32>().ok())
    {
        Some(0) => 0,
        Some(ms) => ms.clamp(lo, hi),
        None => DEFAULT_CARET_BLINK_MS,
    }
}

/// The whole-HUD zoom: Windows text size × `ui.scale`, held to
/// [`UI_SCALE_RANGE`]. Every logical size and widget box uses it; the
/// panel's Zoom row edits `ui.scale` alone.
pub fn effective_scale(tokens: &DesignTokenMap) -> f32 {
    let (lo, hi) = UI_SCALE_RANGE;
    (text_scale(tokens) * ui_scale(tokens)).clamp(lo, hi)
}

/// Whether the OS motion layer is active in `tokens` ([`apply_os_motion_layer`]).
pub fn os_reduce_motion(tokens: &DesignTokenMap) -> bool {
    tokens
        .get(OS_REDUCE_MOTION_KEY)
        .is_some_and(|v| v == "true")
}

// ─── Logical sizes ────────────────────────────────────────────────────────────

/// The owner's global UI scale, a `[design_tokens]` key (themes may not set
/// it): every logical size is multiplied by it on top of the monitor's
/// Windows scale factor.
pub const UI_SCALE_KEY: &str = "ui.scale";

/// The values [`UI_SCALE_KEY`] may take.
pub const UI_SCALE_RANGE: (f32, f32) = (0.5, 3.0);

/// Physical px per logical dp for the display a token map draws on: the
/// monitor's scale factor × [`effective_scale`]. Set by the runtime
/// ([`set_px_scale`]); absent means 1.
pub const PX_SCALE_KEY: &str = "runtime.px_scale";

/// Prefix of the px per dp of a zone placed on a secondary monitor:
/// `runtime.px_scale@<zone>` overrides [`PX_SCALE_KEY`] for that zone
/// ([`set_zone_px_scale`], [`for_zone`]).
const ZONE_PX_SCALE_PREFIX: &str = "runtime.px_scale@";

/// The map's `ui.scale`, clamped to [`UI_SCALE_RANGE`]; 1 when unset.
pub fn ui_scale(tokens: &DesignTokenMap) -> f32 {
    let (lo, hi) = UI_SCALE_RANGE;
    tokens
        .get(UI_SCALE_KEY)
        .and_then(|v| parse_numeric(v))
        .map_or(1.0, |n| n.clamp(lo, hi))
}

/// Physical px per logical dp ([`PX_SCALE_KEY`]); 1 when unset.
pub fn px_scale(tokens: &DesignTokenMap) -> f32 {
    tokens
        .get(PX_SCALE_KEY)
        .and_then(|v| parse_numeric(v))
        .filter(|n| *n > 0.0)
        .unwrap_or(1.0)
}

/// Record that `tokens` draw on a monitor with Windows scale factor
/// `monitor_scale`: [`PX_SCALE_KEY`] becomes `monitor_scale ×`
/// [`effective_scale`].
pub fn set_px_scale(tokens: &mut DesignTokenMap, monitor_scale: f64) {
    let px = monitor_px_scale(tokens, monitor_scale);
    tokens.insert(PX_SCALE_KEY.to_owned(), px);
}

/// Record that `zone` is placed on a monitor with Windows scale factor
/// `monitor_scale`, other than the one [`PX_SCALE_KEY`] describes.
pub fn set_zone_px_scale(tokens: &mut DesignTokenMap, zone: &str, monitor_scale: f64) {
    let px = monitor_px_scale(tokens, monitor_scale);
    tokens.insert(format!("{ZONE_PX_SCALE_PREFIX}{zone}"), px);
}

/// `monitor_scale ×` [`effective_scale`], spelled for the token map; a bogus monitor
/// scale counts as 1.
fn monitor_px_scale(tokens: &DesignTokenMap, monitor_scale: f64) -> String {
    let monitor = if monitor_scale.is_finite() && monitor_scale > 0.0 {
        monitor_scale as f32
    } else {
        1.0
    };
    (monitor * effective_scale(tokens)).to_string()
}

/// [`for_zone`] for every zone placed on a secondary monitor at a scale
/// other than the primary's, keyed by zone name: built once per token
/// change so frames don't copy the map.
pub fn secondary_zone_tokens(tokens: &DesignTokenMap) -> HashMap<String, DesignTokenMap> {
    tokens
        .keys()
        .filter_map(|key| key.strip_prefix(ZONE_PX_SCALE_PREFIX))
        .filter_map(|zone| match for_zone(tokens, zone) {
            Cow::Owned(map) => Some((zone.to_owned(), map)),
            Cow::Borrowed(_) => None,
        })
        .collect()
}

/// The tokens `zone` draws with: `tokens` itself, or, for a zone on a
/// secondary monitor, a copy whose [`PX_SCALE_KEY`] is that monitor's.
/// Every size read for a zone goes through this, so its text, chrome and
/// hit regions share one scale.
pub fn for_zone<'a>(tokens: &'a DesignTokenMap, zone: &str) -> Cow<'a, DesignTokenMap> {
    match tokens.get(&format!("{ZONE_PX_SCALE_PREFIX}{zone}")) {
        Some(px) if tokens.get(PX_SCALE_KEY) != Some(px) => {
            let mut on_its_monitor = tokens.clone();
            on_its_monitor.insert(PX_SCALE_KEY.to_owned(), px.clone());
            Cow::Owned(on_its_monitor)
        }
        _ => Cow::Borrowed(tokens),
    }
}

/// Validate the `[design_tokens]` section of a `RawConfig`.
///
/// Produces `CONFIG_INVALID_TOKEN_KEY` for any key that does not match the
/// required pattern or is reserved for the runtime
/// ([`RESERVED_RUNTIME_TOKEN_KEYS`]). Non-canonical keys are accepted silently.
pub(crate) fn validate_design_tokens(raw: &RawConfig, errors: &mut Vec<ConfigError>) {
    let Some(tokens) = &raw.design_tokens else {
        return;
    };
    for key in tokens.0.keys() {
        if is_reserved_runtime_key(key) {
            errors.push(ConfigError {
                code: ConfigErrorCode::InvalidTokenKey,
                field_path: format!("design_tokens.{key}"),
                expected: "a design token, not a runtime-reserved key".into(),
                got: key.clone(),
                hint: if key.starts_with(PX_SCALE_KEY) || key == TEXT_SCALE_KEY {
                    format!("{key:?} is set by the runtime; set \"ui.scale\" to resize the HUD")
                } else if key == OS_REDUCE_MOTION_KEY || key == CARET_BLINK_MS_KEY {
                    format!("{key:?} is set by the runtime from the Windows setting")
                } else {
                    format!(
                        "{key:?} is set by the runtime; put font files in a `fonts` \
                         directory beside the config file instead"
                    )
                },
            });
            continue;
        }
        if (key == UI_SCALE_KEY
            || key == NOTIFICATION_ACTION_HOLD_MS_KEY
            || key == NOTIFICATION_PRESS_GUARD_MS_KEY
            || is_hit_region_opacity(key))
            && let Err(expected) = validate_canonical_value(key, &tokens.0[key])
        {
            errors.push(ConfigError {
                code: ConfigErrorCode::TokenValueParseError,
                field_path: format!("design_tokens.{key}"),
                expected,
                got: tokens.0[key].clone(),
                hint: if key == UI_SCALE_KEY {
                    "e.g. \"ui.scale\" = \"1.25\" makes everything a quarter bigger".into()
                } else if is_hit_region_opacity(key) {
                    format!("e.g. \"{key}\" = \"0.2\"")
                } else {
                    format!("e.g. \"{key}\" = \"800\"")
                },
            });
            continue;
        }
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
    fn test_parse_numeric_accepts_integer_and_decimal() {
        for (raw, want) in [("16", 16.0), ("1.5", 1.5)] {
            let n = parse_numeric(raw).unwrap();
            assert!((n - want).abs() < 1e-4, "{raw:?} parsed as {n}");
        }
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
        for (key, expected) in [
            ("motion.enter.ms", 180),
            ("motion.exit.ms", 120),
            ("motion.state.ms", 100),
        ] {
            assert_eq!(resolve_motion_duration_ms(&map, key), expected);
            for (value, expected_value) in [
                ("0", 0),
                ("12.4", 12),
                ("12.5", 13),
                ("-1", expected),
                ("NaN", expected),
                ("inf", expected),
                ("4294967294.6", u32::MAX),
                ("4294967295", u32::MAX),
                ("4294967295.4", expected),
                ("4294967296", expected),
            ] {
                let overridden = DesignTokenMap::from([(key.into(), value.into())]);
                assert_eq!(resolve_motion_duration_ms(&overridden, key), expected_value);
            }
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
        theme.insert("color.on_surface.variant".into(), "#00FF00".into());
        let mut config = DesignTokenMap::new();
        config.insert("color.text.primary".into(), "#FF0000".into());
        let map = resolve_tokens(&theme, &config);
        assert_eq!(map["color.text.primary"], "#FF0000", "config beats theme");
        assert_eq!(
            map["color.on_surface.variant"], "#00FF00",
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
    fn hit_region_opacity_accepts_only_unit_interval_values() {
        use crate::raw::RawDesignTokens;

        for key in [
            "tile.hit_region.hover.opacity",
            "tile.hit_region.pressed.opacity",
        ] {
            for (value, allowed) in [
                ("0", true),
                ("0.2", true),
                ("1", true),
                ("-0.1", false),
                ("1.1", false),
                ("NaN", false),
            ] {
                assert_eq!(validate_canonical_value(key, value).is_ok(), allowed);
                let raw = RawConfig {
                    design_tokens: Some(RawDesignTokens(
                        [(key.to_owned(), value.to_owned())].into(),
                    )),
                    ..Default::default()
                };
                let mut errors = Vec::new();
                validate_design_tokens(&raw, &mut errors);
                assert_eq!(errors.is_empty(), allowed, "{key} = {value}");
                if !allowed {
                    assert_eq!(errors[0].code, ConfigErrorCode::TokenValueParseError);
                }
            }
        }
    }

    #[test]
    fn ui_scale_is_config_only_and_ranged() {
        use crate::raw::RawDesignTokens;
        let check = |pairs: &[(&str, &str)]| {
            let raw = RawConfig {
                design_tokens: Some(RawDesignTokens(
                    pairs
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                )),
                ..Default::default()
            };
            let mut errors = Vec::new();
            validate_design_tokens(&raw, &mut errors);
            errors
        };
        assert!(check(&[(UI_SCALE_KEY, "1.25")]).is_empty());
        for bad in ["0.4", "3.5", "big"] {
            let errors = check(&[(UI_SCALE_KEY, bad)]);
            assert_eq!(errors.len(), 1, "{bad}");
            assert_eq!(errors[0].code, ConfigErrorCode::TokenValueParseError);
        }
        for good in ["0", "600", "3000"] {
            assert!(check(&[(NOTIFICATION_ACTION_HOLD_MS_KEY, good)]).is_empty());
            let tokens = DesignTokenMap::from([(
                NOTIFICATION_ACTION_HOLD_MS_KEY.to_owned(),
                good.to_owned(),
            )]);
            assert_eq!(
                notification_action_hold_ms(&tokens).to_string(),
                good,
                "hold {good}"
            );
        }
        for bad in ["-1", "3001", "0.5", "long"] {
            let errors = check(&[(NOTIFICATION_ACTION_HOLD_MS_KEY, bad)]);
            assert_eq!(errors.len(), 1, "{bad}");
            assert_eq!(errors[0].code, ConfigErrorCode::TokenValueParseError);
        }
        assert_eq!(notification_action_hold_ms(&DesignTokenMap::new()), 600);
        assert_eq!(notification_press_guard_ms(&DesignTokenMap::new()), 400);
        for (value, ms) in [("0", 0), ("250", 250), ("3000", 3000), ("3001", 400)] {
            let tokens = DesignTokenMap::from([(
                NOTIFICATION_PRESS_GUARD_MS_KEY.to_owned(),
                value.to_owned(),
            )]);
            assert_eq!(notification_press_guard_ms(&tokens), ms, "guard {value}");
            assert_eq!(
                check(&[(NOTIFICATION_PRESS_GUARD_MS_KEY, value)]).is_empty(),
                value != "3001"
            );
        }
        for key in [PX_SCALE_KEY, "runtime.px_scale@subtitle"] {
            let errors = check(&[(key, "2")]);
            assert_eq!(errors[0].code, ConfigErrorCode::InvalidTokenKey);
            assert!(errors[0].hint.contains("ui.scale"), "{}", errors[0].hint);
        }
        assert!(crate::themes::parse_theme("\"ui.scale\" = \"1.25\"").is_err());
    }

    /// The press guard is a safety interval: turning Windows animations off
    /// zeroes motion durations but leaves it unchanged.
    #[test]
    fn press_guard_is_not_zeroed_by_os_motion() {
        assert!(!OS_MOTION_ZEROED_KEYS.contains(&NOTIFICATION_PRESS_GUARD_MS_KEY));
        let mut resolved = crate::themes::resolve_config_tokens(&DesignTokenMap::new());
        apply_os_motion_layer(&mut resolved, &DesignTokenMap::new(), false);
        assert_eq!(resolved["motion.enter.ms"], "0");
        assert_eq!(notification_press_guard_ms(&resolved), 400);
    }

    #[test]
    fn px_scale_is_monitor_times_ui_scale() {
        let mut tokens = DesignTokenMap::from([(UI_SCALE_KEY.to_owned(), "1.25".to_owned())]);
        assert_eq!(px_scale(&tokens), 1.0, "unset means 1");
        set_px_scale(&mut tokens, 1.5);
        assert_eq!(px_scale(&tokens), 1.875);
        set_px_scale(&mut tokens, f64::NAN);
        assert_eq!(px_scale(&tokens), 1.25, "a bogus monitor scale counts as 1");
    }

    /// A zone on a secondary monitor reads that monitor's px per dp; every
    /// other zone shares the map itself; config may not set either key.
    #[test]
    fn zones_on_a_secondary_draw_at_its_scale() {
        let mut tokens = DesignTokenMap::from([(UI_SCALE_KEY.to_owned(), "1.25".to_owned())]);
        set_px_scale(&mut tokens, 1.5);
        set_zone_px_scale(&mut tokens, "subtitle", 1.0);
        let subtitle = for_zone(&tokens, "subtitle");
        assert!(matches!(subtitle, Cow::Owned(_)));
        assert_eq!(px_scale(&subtitle), 1.25);
        assert!(matches!(for_zone(&tokens, "status-bar"), Cow::Borrowed(_)));
        let cached = secondary_zone_tokens(&tokens);
        assert_eq!(cached.keys().collect::<Vec<_>>(), ["subtitle"]);
        assert_eq!(cached["subtitle"], *subtitle);
        assert_eq!(px_scale(&for_zone(&tokens, "status-bar")), 1.875);
    }

    #[test]
    fn test_validate_rejects_runtime_reserved_key() {
        use crate::raw::RawDesignTokens;
        let mut tokens = HashMap::new();
        tokens.insert("runtime.fonts_dir".to_string(), "C:/fonts".to_string());
        let raw = RawConfig {
            design_tokens: Some(RawDesignTokens(tokens)),
            ..Default::default()
        };
        let mut errors = Vec::new();
        validate_design_tokens(&raw, &mut errors);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ConfigErrorCode::InvalidTokenKey);
        assert_eq!(errors[0].field_path, "design_tokens.runtime.fonts_dir");
        assert!(errors[0].hint.contains("fonts"));
    }

    // hud-ljhku.13 R13.4 S1: theme -> OS preference -> config overrides.
    #[test]
    fn os_motion_layer_zeroes_exactly_the_five_durations() {
        let config = DesignTokenMap::new();
        let themed = crate::themes::resolve_config_tokens(&config);
        let mut resolved = themed.clone();
        apply_os_motion_layer(&mut resolved, &config, false);
        for key in OS_MOTION_ZEROED_KEYS {
            assert_eq!(resolved.get(key).map(String::as_str), Some("0"), "{key}");
        }
        assert!(os_reduce_motion(&resolved));
        // Nothing else moves: easing curves and every other token keep the
        // theme's value.
        for (key, value) in &themed {
            if !OS_MOTION_ZEROED_KEYS.contains(&key.as_str()) {
                assert_eq!(resolved.get(key), Some(value), "{key}");
            }
        }
        assert_eq!(resolved.len(), themed.len() + 3, "two portal keys + flag");
        assert_eq!(resolve_motion_duration_ms(&resolved, "motion.state.ms"), 0);
    }

    #[test]
    fn os_motion_layer_sits_below_config_overrides() {
        let config: DesignTokenMap = [
            ("motion.exit.ms".to_string(), "250".to_string()),
            ("portal.transition.in_ms".to_string(), "90".to_string()),
        ]
        .into();
        let mut resolved = crate::themes::resolve_config_tokens(&config);
        apply_os_motion_layer(&mut resolved, &config, false);
        assert_eq!(resolved["motion.exit.ms"], "250");
        assert_eq!(resolved["portal.transition.in_ms"], "90");
        assert_eq!(resolved["motion.enter.ms"], "0");
        assert_eq!(resolved["motion.state.ms"], "0");
        assert_eq!(resolved["portal.transition.out_ms"], "0");
    }

    #[test]
    fn os_motion_layer_with_animations_on_is_the_theme() {
        let config = DesignTokenMap::new();
        let themed = crate::themes::resolve_config_tokens(&config);
        let mut resolved = themed.clone();
        // A reload after the owner turns animations back on clears the flag.
        resolved.insert(OS_REDUCE_MOTION_KEY.into(), "true".into());
        apply_os_motion_layer(&mut resolved, &config, true);
        assert_eq!(resolved, themed);
        assert!(!os_reduce_motion(&resolved));
    }

    #[test]
    fn os_reduce_motion_key_is_runtime_reserved() {
        assert!(is_reserved_runtime_key(OS_REDUCE_MOTION_KEY));
    }

    /// `[design_tokens]` may set neither Windows key.
    #[test]
    fn os_text_and_caret_keys_are_runtime_reserved() {
        use crate::raw::RawDesignTokens;
        for key in [TEXT_SCALE_KEY, CARET_BLINK_MS_KEY] {
            assert!(is_reserved_runtime_key(key));
            let raw = RawConfig {
                design_tokens: Some(RawDesignTokens(HashMap::from([(
                    key.to_owned(),
                    "1".to_owned(),
                )]))),
                ..Default::default()
            };
            let mut errors = Vec::new();
            validate_design_tokens(&raw, &mut errors);
            assert_eq!(errors.len(), 1, "{key}");
            assert_eq!(errors[0].code, ConfigErrorCode::InvalidTokenKey);
            assert!(!errors[0].hint.contains("fonts"), "{}", errors[0].hint);
        }
    }

    /// At the Windows defaults the layer leaves the
    /// map as it was, and every reader is today's.
    #[test]
    fn os_text_and_caret_at_defaults_are_identity() {
        let themed = crate::themes::resolve_config_tokens(&DesignTokenMap::new());
        let mut tokens = themed.clone();
        tokens.insert(TEXT_SCALE_KEY.into(), "1.25".into());
        tokens.insert(CARET_BLINK_MS_KEY.into(), "0".into());
        apply_os_text_and_caret(&mut tokens, 1.0, DEFAULT_CARET_BLINK_MS);
        assert_eq!(tokens, themed, "a reload back to defaults clears both");
        assert_eq!(text_scale(&tokens), 1.0);
        assert_eq!(caret_blink_ms(&tokens), 530);
        assert_eq!(effective_scale(&tokens), ui_scale(&tokens));
    }

    /// Text size × `ui.scale`, held to 0.5–3.0;
    /// px per dp is the monitor's scale times that.
    #[test]
    fn effective_scale_is_text_size_times_ui_scale_held_to_range() {
        let at = |text: f32, ui: &str| {
            let mut tokens = DesignTokenMap::from([(UI_SCALE_KEY.to_owned(), ui.to_owned())]);
            apply_os_text_and_caret(&mut tokens, text, DEFAULT_CARET_BLINK_MS);
            tokens
        };
        assert_eq!(effective_scale(&at(1.25, "1")), 1.25);
        assert_eq!(effective_scale(&at(1.25, "0.8")), 1.0);
        assert_eq!(effective_scale(&at(2.25, "2")), 3.0, "held at 300%");
        assert_eq!(effective_scale(&at(2.25, "0.5")), 1.125, "floor 112.5%");
        let mut s1 = at(1.25, "1");
        set_px_scale(&mut s1, 1.5);
        assert_eq!(px_scale(&s1), 1.875);
        assert_eq!(ui_scale(&s1), 1.0, "the owner's Zoom is never rewritten");
    }

    /// Off-range values clamp; a steady caret stays 0.
    #[test]
    fn os_text_and_caret_readers_clamp() {
        let with = |key: &str, v: &str| DesignTokenMap::from([(key.to_owned(), v.to_owned())]);
        assert_eq!(text_scale(&with(TEXT_SCALE_KEY, "3")), 2.25);
        assert_eq!(text_scale(&with(TEXT_SCALE_KEY, "0.5")), 1.0);
        assert_eq!(caret_blink_ms(&with(CARET_BLINK_MS_KEY, "0")), 0);
        assert_eq!(caret_blink_ms(&with(CARET_BLINK_MS_KEY, "50")), 200);
        assert_eq!(caret_blink_ms(&with(CARET_BLINK_MS_KEY, "5000")), 1200);
        assert_eq!(caret_blink_ms(&with(CARET_BLINK_MS_KEY, "250")), 250);
        assert_eq!(caret_blink_ms(&with(CARET_BLINK_MS_KEY, "x")), 530);
    }

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
