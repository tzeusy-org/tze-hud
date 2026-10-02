//! Token-to-RenderingPolicy mapper.
//!
//! At startup, the runtime builds each built-in zone's `RenderingPolicy` by
//! starting from the zone registry default and filling every `None` field from
//! the global design tokens (`[design_tokens]` merged over canonical fallbacks).
//! The result is immutable after startup.

use std::collections::HashMap;

use tze_hud_scene::types::{FontFamily, RenderingPolicy, Rgba, TextAlign, TextOverflow};

use crate::tokens::{DesignTokenMap, parse_color_hex, parse_font_family, parse_numeric};

// ─── Token lookup helpers ─────────────────────────────────────────────────────

/// Convert a tokens-module `Rgba` to the scene-types `Rgba`.
fn tokens_color_to_scene(c: crate::tokens::Rgba) -> Rgba {
    Rgba {
        r: c.r,
        g: c.g,
        b: c.b,
        a: c.a,
    }
}

/// Look up a token as `Rgba`. Returns `None` if the key is absent or unparseable.
fn token_color(tokens: &DesignTokenMap, key: &str) -> Option<Rgba> {
    tokens
        .get(key)
        .and_then(|v| parse_color_hex(v))
        .map(tokens_color_to_scene)
}

/// Look up a token as `f32`. Returns `None` if absent or unparseable.
fn token_f32(tokens: &DesignTokenMap, key: &str) -> Option<f32> {
    tokens.get(key).and_then(|v| parse_numeric(v))
}

/// Look up a token as `u16`. Returns `None` if absent or unparseable.
fn token_u16(tokens: &DesignTokenMap, key: &str) -> Option<u16> {
    tokens
        .get(key)
        .and_then(|v| parse_numeric(v))
        .map(|n| n as u16)
}

/// Look up a token as `FontFamily`. Returns `None` if absent or unparseable.
fn token_font_family(tokens: &DesignTokenMap, key: &str) -> Option<FontFamily> {
    tokens.get(key).and_then(|v| parse_font_family(v))
}

// ─── Token-to-RenderingPolicy mapper ──────────────────────────────────────────

/// Apply token-derived defaults for the `subtitle` zone to `policy`.
///
/// Populates `None` fields only — explicit (non-`None`) values are left untouched.
///
/// Token mappings (per spec §Requirement: Default Zone Rendering with Tokens):
/// - `text_color` ← `color.text.primary`
/// - `font_family` ← `typography.subtitle.family`
/// - `font_size_px` ← `typography.subtitle.size`
/// - `font_weight` ← `typography.subtitle.weight`
/// - `backdrop` ← `color.backdrop.default`
/// - `backdrop_opacity` ← `opacity.backdrop.default`
/// - `outline_color` ← `color.outline.default`
/// - `outline_width` ← `stroke.outline.width`
/// - `text_align` ← `Center` (hardcoded, not token-driven)
/// - `margin_vertical` ← `spacing.padding.medium`
/// - `overflow` ← `Ellipsis` (hardcoded per subtitle exemplar spec)
pub fn apply_subtitle_token_defaults(policy: &mut RenderingPolicy, tokens: &DesignTokenMap) {
    if policy.text_color.is_none() {
        policy.text_color = token_color(tokens, "color.text.primary");
    }
    if policy.font_family.is_none() {
        policy.font_family = token_font_family(tokens, "typography.subtitle.family");
    }
    if policy.font_size_px.is_none() {
        policy.font_size_px = token_f32(tokens, "typography.subtitle.size");
    }
    if policy.font_weight.is_none() {
        policy.font_weight = token_u16(tokens, "typography.subtitle.weight");
    }
    // Subtitle uses outline-only readability (no backdrop). The DualLayer
    // technique achieves legibility via text outline alone on a transparent
    // background, letting the underlying content show through.
    // backdrop and backdrop_opacity are intentionally NOT populated from tokens.
    if policy.outline_color.is_none() {
        policy.outline_color = token_color(tokens, "color.outline.default");
    }
    if policy.outline_width.is_none() {
        policy.outline_width = token_f32(tokens, "stroke.outline.width");
    }
    // text_align: hardcoded default (Center), not token-driven
    if policy.text_align.is_none() {
        policy.text_align = Some(TextAlign::Center);
    }
    if policy.margin_vertical.is_none() {
        policy.margin_vertical = token_f32(tokens, "spacing.padding.medium");
    }
    // overflow: Ellipsis per subtitle exemplar spec (hardcoded, not token-driven)
    if policy.overflow.is_none() {
        policy.overflow = Some(TextOverflow::Ellipsis);
    }
    // Fade transitions for subtitle publish/clear.
    if policy.transition_in_ms.is_none() {
        policy.transition_in_ms = Some(200);
    }
    if policy.transition_out_ms.is_none() {
        policy.transition_out_ms = Some(150);
    }
}

/// Apply token-derived defaults for the `notification-area` zone to `policy`.
///
/// Token mappings:
/// - `text_color` ← `color.text.primary`
/// - `font_family` ← `typography.body.family`
/// - `font_size_px` ← `typography.body.size`
/// - `font_weight` ← `typography.body.weight`
/// - `backdrop` ← `color.backdrop.default`
/// - `backdrop_opacity` ← `opacity.backdrop.opaque`
/// - `outline_color` ← `None` (no outline for notifications; spec says explicitly None)
/// - `margin_horizontal` ← `spacing.padding.medium`
/// - `margin_vertical` ← `spacing.padding.medium`
/// - `backdrop_radius` ← `border.radius.medium`
/// - `text_align` ← `Start`; transitions 120 ms in / 180 ms out (not token-driven)
pub fn apply_notification_area_token_defaults(
    policy: &mut RenderingPolicy,
    tokens: &DesignTokenMap,
) {
    if policy.text_color.is_none() {
        policy.text_color = token_color(tokens, "color.text.primary");
    }
    if policy.font_family.is_none() {
        policy.font_family = token_font_family(tokens, "typography.body.family");
    }
    if policy.font_size_px.is_none() {
        policy.font_size_px = token_f32(tokens, "typography.body.size");
    }
    if policy.font_weight.is_none() {
        policy.font_weight = token_u16(tokens, "typography.body.weight");
    }
    if policy.backdrop.is_none() {
        policy.backdrop = token_color(tokens, "color.backdrop.default");
    }
    if policy.backdrop_opacity.is_none() {
        policy.backdrop_opacity = token_f32(tokens, "opacity.backdrop.opaque");
    }
    // outline_color: explicitly None for notifications — not token-driven
    if policy.margin_horizontal.is_none() {
        policy.margin_horizontal = token_f32(tokens, "spacing.padding.medium");
    }
    if policy.margin_vertical.is_none() {
        policy.margin_vertical = token_f32(tokens, "spacing.padding.medium");
    }
    if policy.backdrop_radius.is_none() {
        policy.backdrop_radius = token_f32(tokens, "border.radius.medium");
    }
    if policy.text_align.is_none() {
        policy.text_align = Some(TextAlign::Start);
    }
    if policy.transition_in_ms.is_none() {
        policy.transition_in_ms = Some(120);
    }
    if policy.transition_out_ms.is_none() {
        policy.transition_out_ms = Some(180);
    }
}

/// Apply token-derived defaults for the `status-bar` zone to `policy`.
///
/// Token mappings:
/// - `text_color` ← `color.text.secondary`
/// - `font_family` ← `typography.body.family`
/// - `font_size_px` ← `typography.body.size`
/// - `backdrop` ← `color.backdrop.default`
/// - `backdrop_opacity` ← `opacity.backdrop.opaque`
pub fn apply_status_bar_token_defaults(policy: &mut RenderingPolicy, tokens: &DesignTokenMap) {
    if policy.text_color.is_none() {
        policy.text_color = token_color(tokens, "color.text.secondary");
    }
    if policy.font_family.is_none() {
        policy.font_family = token_font_family(tokens, "typography.body.family");
    }
    if policy.font_size_px.is_none() {
        policy.font_size_px = token_f32(tokens, "typography.body.size");
    }
    // OpaqueBackdrop readability required per component type contract
    // (heart-and-soul/presence.md line 281: opacity >= 0.8).
    if policy.backdrop.is_none() {
        policy.backdrop = token_color(tokens, "color.backdrop.default");
    }
    if policy.backdrop_opacity.is_none() {
        policy.backdrop_opacity = token_f32(tokens, "opacity.backdrop.opaque");
    }
    // Right-aligned, compact vertical layout.
    if policy.text_align.is_none() {
        policy.text_align = Some(TextAlign::End);
    }
}

/// Apply token-derived defaults for the `alert-banner` zone to `policy`.
///
/// Token mappings:
/// - `text_color` ← `color.text.primary`
/// - `font_family` ← `typography.heading.family`
/// - `font_size_px` ← `typography.heading.size`
/// - `font_weight` ← `typography.heading.weight`
/// - `backdrop` ← `color.backdrop.default`
/// - `backdrop_opacity` ← `opacity.backdrop.opaque`
pub fn apply_alert_banner_token_defaults(policy: &mut RenderingPolicy, tokens: &DesignTokenMap) {
    if policy.text_color.is_none() {
        policy.text_color = token_color(tokens, "color.text.primary");
    }
    if policy.font_family.is_none() {
        policy.font_family = token_font_family(tokens, "typography.heading.family");
    }
    if policy.font_size_px.is_none() {
        policy.font_size_px = token_f32(tokens, "typography.heading.size");
    }
    if policy.font_weight.is_none() {
        policy.font_weight = token_u16(tokens, "typography.heading.weight");
    }
    if policy.backdrop.is_none() {
        policy.backdrop = token_color(tokens, "color.backdrop.default");
    }
    if policy.backdrop_opacity.is_none() {
        policy.backdrop_opacity = token_f32(tokens, "opacity.backdrop.opaque");
    }
    // Black text outline for legibility on light backdrops (e.g. warning/amber).
    if policy.outline_color.is_none() {
        policy.outline_color = token_color(tokens, "color.outline.default");
    }
    if policy.outline_width.is_none() {
        policy.outline_width = Some(1.0);
    }
    // Generous vertical padding for banner readability.
    if policy.margin_vertical.is_none() {
        policy.margin_vertical = Some(12.0);
    }
    if policy.margin_horizontal.is_none() {
        policy.margin_horizontal = Some(12.0);
    }
}

/// Apply token-derived defaults to `policy` for the given zone type name.
///
/// For zone types that have no token-driven defaults (`ambient-background`, `pip`),
/// this is a no-op.
pub fn apply_token_defaults_for_zone(
    zone_name: &str,
    policy: &mut RenderingPolicy,
    tokens: &DesignTokenMap,
) {
    match zone_name {
        "subtitle" => apply_subtitle_token_defaults(policy, tokens),
        "notification-area" => apply_notification_area_token_defaults(policy, tokens),
        "status-bar" => apply_status_bar_token_defaults(policy, tokens),
        "alert-banner" => apply_alert_banner_token_defaults(policy, tokens),
        // ambient-background, pip: no token-driven rendering policy fields
        _ => {}
    }
}

// ─── Effective policy constructor ─────────────────────────────────────────────

/// Construct the effective `RenderingPolicy` for a zone type: the zone default
/// with token-derived defaults filling every `None` field.
pub fn build_effective_policy(
    zone_name: &str,
    zone_default: &RenderingPolicy,
    tokens: &DesignTokenMap,
) -> RenderingPolicy {
    let mut policy = zone_default.clone();
    apply_token_defaults_for_zone(zone_name, &mut policy, tokens);
    policy
}

/// Build effective rendering policies for all built-in zone types.
///
/// Returns a `HashMap<zone_name, effective_RenderingPolicy>` that can be used
/// to patch `ZoneRegistry::with_defaults()` after construction.
pub fn build_all_effective_policies(
    zone_defaults: &HashMap<String, RenderingPolicy>,
    tokens: &DesignTokenMap,
) -> HashMap<String, RenderingPolicy> {
    zone_defaults
        .iter()
        .map(|(zone_name, zone_default)| {
            (
                zone_name.clone(),
                build_effective_policy(zone_name, zone_default, tokens),
            )
        })
        .collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::resolve_tokens;

    fn default_tokens() -> DesignTokenMap {
        resolve_tokens(&DesignTokenMap::new(), &DesignTokenMap::new())
    }

    // ── apply_token_defaults_for_zone ─────────────────────────────────────────

    #[test]
    fn test_subtitle_token_defaults_populate_none_fields() {
        let tokens = default_tokens();
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("subtitle", &mut policy, &tokens);

        // text_color should be populated from color.text.primary = "#FFFFFF"
        assert!(
            policy.text_color.is_some(),
            "text_color should be set from tokens"
        );
        let tc = policy.text_color.unwrap();
        assert!(
            (tc.r - 1.0).abs() < 1e-4,
            "text_color.r should be 1.0 for #FFFFFF"
        );
        assert!(
            (tc.g - 1.0).abs() < 1e-4,
            "text_color.g should be 1.0 for #FFFFFF"
        );
        assert!(
            (tc.b - 1.0).abs() < 1e-4,
            "text_color.b should be 1.0 for #FFFFFF"
        );

        // font_family should be set
        assert!(policy.font_family.is_some());
        assert_eq!(policy.font_family.unwrap(), FontFamily::SystemSansSerif);

        // text_align should be Center (hardcoded)
        assert_eq!(policy.text_align, Some(TextAlign::Center));

        // backdrop should be None — subtitle uses outline-only readability (no backdrop)
        assert!(
            policy.backdrop.is_none(),
            "subtitle zone must not set backdrop (outline-only readability)"
        );

        // transition_in_ms and transition_out_ms should be set
        assert_eq!(policy.transition_in_ms, Some(200));
        assert_eq!(policy.transition_out_ms, Some(150));

        // outline_color should be set
        assert!(policy.outline_color.is_some());

        // overflow should be Ellipsis (subtitle exemplar spec)
        assert_eq!(
            policy.overflow,
            Some(TextOverflow::Ellipsis),
            "subtitle zone must default to TextOverflow::Ellipsis"
        );
    }

    #[test]
    fn test_subtitle_overflow_not_overwritten_when_explicit() {
        let tokens = default_tokens();
        // Pre-set overflow to Clip (explicit config override)
        let mut policy = RenderingPolicy {
            overflow: Some(TextOverflow::Clip),
            ..RenderingPolicy::default()
        };
        apply_token_defaults_for_zone("subtitle", &mut policy, &tokens);
        // overflow should remain Clip — token default must not overwrite explicit values
        assert_eq!(
            policy.overflow,
            Some(TextOverflow::Clip),
            "explicit overflow must not be overwritten by token default"
        );
    }

    #[test]
    fn test_non_subtitle_zones_do_not_set_overflow() {
        let tokens = default_tokens();
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("notification-area", &mut policy, &tokens);
        // notification-area has no overflow token default
        assert!(
            policy.overflow.is_none(),
            "notification-area must not set overflow"
        );
    }

    #[test]
    fn test_token_defaults_do_not_overwrite_existing_values() {
        let tokens = default_tokens();
        // Pre-set font_size_px to 32.0 (explicit config value)
        let mut policy = RenderingPolicy {
            font_size_px: Some(32.0),
            ..RenderingPolicy::default()
        };

        apply_token_defaults_for_zone("subtitle", &mut policy, &tokens);

        // font_size_px should remain 32.0, not the canonical default of 28
        assert_eq!(
            policy.font_size_px,
            Some(32.0),
            "explicit value must not be overwritten by token default"
        );
    }

    #[test]
    fn test_custom_color_token_reflected_in_policy() {
        // spec: WHEN color.text.primary = "#00FF00" THEN text_color = Rgba(0,1,0,1)
        let mut config_tokens = DesignTokenMap::new();
        config_tokens.insert("color.text.primary".to_string(), "#00FF00".to_string());
        let tokens = resolve_tokens(&config_tokens, &DesignTokenMap::new());

        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("subtitle", &mut policy, &tokens);

        let tc = policy.text_color.expect("text_color should be set");
        assert!((tc.r).abs() < 1e-4, "r should be 0 for #00FF00");
        assert!((tc.g - 1.0).abs() < 1e-4, "g should be 1.0 for #00FF00");
        assert!((tc.b).abs() < 1e-4, "b should be 0 for #00FF00");
    }

    #[test]
    fn test_notification_area_token_defaults() {
        let tokens = default_tokens();
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("notification-area", &mut policy, &tokens);

        assert!(policy.text_color.is_some());
        assert!(policy.font_family.is_some());
        assert!(policy.font_size_px.is_some());
        assert!(policy.backdrop.is_some());
        assert!(policy.backdrop_opacity.is_some());
        // No outline for notifications (spec: outline_color ← None)
        assert!(
            policy.outline_color.is_none(),
            "notification-area must NOT have outline_color set from tokens"
        );
        assert!(policy.margin_horizontal.is_some());
        assert!(policy.margin_vertical.is_some());
        // backdrop_radius should be set from border.radius.medium (canonical default: 8.0)
        assert!(
            policy.backdrop_radius.is_some(),
            "notification-area must have backdrop_radius set from border.radius.medium token"
        );
        let radius = policy.backdrop_radius.unwrap();
        assert!(
            (radius - 8.0).abs() < 1e-4,
            "border.radius.medium canonical default is 8.0, got {radius}"
        );
    }

    #[test]
    fn test_notification_area_backdrop_radius_not_overwritten_when_explicit() {
        let tokens = default_tokens();
        // Pre-set backdrop_radius to 4.0 (explicit override)
        let mut policy = RenderingPolicy {
            backdrop_radius: Some(4.0),
            ..RenderingPolicy::default()
        };
        apply_token_defaults_for_zone("notification-area", &mut policy, &tokens);
        // Must remain 4.0 — token default must not overwrite explicit values
        assert_eq!(
            policy.backdrop_radius,
            Some(4.0),
            "explicit backdrop_radius must not be overwritten by token default"
        );
    }

    #[test]
    fn test_status_bar_token_defaults() {
        let tokens = default_tokens();
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("status-bar", &mut policy, &tokens);

        assert!(policy.text_color.is_some());
        assert!(policy.font_family.is_some());
        assert!(policy.font_size_px.is_some());
        assert!(policy.backdrop.is_some());
        assert!(policy.backdrop_opacity.is_some());
    }

    #[test]
    fn test_alert_banner_token_defaults() {
        let tokens = default_tokens();
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("alert-banner", &mut policy, &tokens);

        assert!(policy.text_color.is_some());
        assert!(policy.font_family.is_some());
        assert!(policy.font_size_px.is_some());
        assert!(policy.font_weight.is_some());
        assert!(policy.backdrop.is_some());
        assert!(policy.backdrop_opacity.is_some());
    }

    #[test]
    fn test_ambient_background_no_token_defaults() {
        let tokens = default_tokens();
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("ambient-background", &mut policy, &tokens);

        // ambient-background has no token-driven rendering policy fields
        assert!(policy.text_color.is_none());
        assert!(policy.font_family.is_none());
        assert!(policy.backdrop.is_none());
    }

    #[test]
    fn test_pip_no_token_defaults() {
        let tokens = default_tokens();
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("pip", &mut policy, &tokens);

        // pip has no token-driven rendering policy fields
        assert!(policy.text_color.is_none());
        assert!(policy.font_family.is_none());
    }

    // ── build_effective_policy ────────────────────────────────────────────────

    #[test]
    fn test_build_effective_policy_fills_token_defaults() {
        let tokens = default_tokens();
        let zone_default = RenderingPolicy::default();
        let policy = build_effective_policy("subtitle", &zone_default, &tokens);

        // Should have token-derived defaults
        assert!(policy.text_color.is_some());
        assert!(policy.font_family.is_some());
        assert_eq!(policy.text_align, Some(TextAlign::Center));
    }

    #[test]
    fn test_build_effective_policy_absent_zone_type_is_noop() {
        let tokens = default_tokens();
        let zone_default = RenderingPolicy::default();
        let policy = build_effective_policy("ambient-background", &zone_default, &tokens);

        // ambient-background gets no token defaults
        assert_eq!(policy, RenderingPolicy::default());
    }
}

/// Claimed-tile placement sizes from the resolved `tile.*` tokens; missing or
/// unparseable entries keep the defaults.
pub fn tile_placement_from_tokens(
    tokens: &DesignTokenMap,
) -> tze_hud_scene::placement::TilePlacementTokens {
    let mut t = tze_hud_scene::placement::TilePlacementTokens::default();
    for (class, slot) in [
        ("small", &mut t.small),
        ("medium", &mut t.medium),
        ("large", &mut t.large),
        ("wide", &mut t.wide),
        ("tall", &mut t.tall),
    ] {
        if let Some(w) = token_f32(tokens, &format!("tile.{class}.width")).filter(|v| *v > 0.0) {
            slot.0 = w;
        }
        if let Some(h) = token_f32(tokens, &format!("tile.{class}.height")).filter(|v| *v > 0.0) {
            slot.1 = h;
        }
    }
    if let Some(m) = token_f32(tokens, "tile.margin").filter(|v| *v >= 0.0) {
        t.margin = m;
    }
    if let Some(g) = token_f32(tokens, "tile.gap").filter(|v| *v >= 0.0) {
        t.gap = g;
    }
    t
}

#[cfg(test)]
mod tile_placement_tests {
    use super::*;

    #[test]
    fn canonical_tile_tokens_match_placement_defaults() {
        let resolved =
            crate::tokens::resolve_tokens(&DesignTokenMap::new(), &DesignTokenMap::new());
        assert_eq!(
            tile_placement_from_tokens(&resolved),
            tze_hud_scene::placement::TilePlacementTokens::default()
        );
    }

    #[test]
    fn tile_tokens_override_sizes() {
        let mut tokens = DesignTokenMap::new();
        tokens.insert("tile.small.width".into(), "100".into());
        tokens.insert("tile.gap".into(), "4".into());
        let t = tile_placement_from_tokens(&tokens);
        assert_eq!(t.small.0, 100.0);
        assert_eq!(t.gap, 4.0);
    }
}
