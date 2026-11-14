//! Token-to-RenderingPolicy mapper.
//!
//! At startup, the runtime builds each built-in zone's `RenderingPolicy` by
//! starting from the zone registry default and filling every `None` field from
//! the global design tokens (`[design_tokens]` merged over canonical fallbacks).
//! The result is immutable after startup.

use tze_hud_scene::types::{FontFamily, RenderingPolicy, Rgba, TextAlign, TextOverflow};

use crate::tokens::{
    DesignTokenMap, parse_color_hex, parse_font_family, parse_numeric, resolve_motion_duration_ms,
};

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
/// - `backdrop` ← `color.subtitle.backdrop`, `backdrop_opacity` ←
///   `opacity.subtitle.backdrop`, only when that opacity is above 0
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
    // Subtitle reads by its text outline alone over a transparent zone. The
    // owner can add a fill behind the whole zone (Appearance → Subtitle
    // backdrop), which makes streamed subtitles stand out: it is drawn only
    // while `opacity.subtitle.backdrop` > 0.
    if policy.backdrop.is_none()
        && let Some(opacity) = token_f32(tokens, "opacity.subtitle.backdrop").filter(|o| *o > 0.0)
    {
        policy.backdrop = token_color(tokens, "color.subtitle.backdrop");
        policy.backdrop_opacity = Some(opacity.min(1.0));
    }
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
        policy.transition_in_ms = Some(resolve_motion_duration_ms(tokens, "motion.enter.ms"));
    }
    if policy.transition_out_ms.is_none() {
        policy.transition_out_ms = Some(resolve_motion_duration_ms(tokens, "motion.exit.ms"));
    }
}

/// Apply token-derived defaults for the `notification-area` zone to `policy`.
///
/// Token mappings (Tonal Glass card, hud-h51u7.4.1; the compositor's
/// `NotificationStyle` reads every other card metric and colour from
/// semantic tokens directly):
/// - `text_color` ← `color.on_surface`
/// - `font_family` ← `typography.body.family`
/// - `font_size_px` / `font_weight` ← `type.title.m.size` / `.weight` (the
///   title; the body and action roles scale with an explicit size)
/// - `backdrop` ← `color.surface.container.high` (presence enables the card;
///   its fill is the urgency tone)
/// - `backdrop_opacity` ← `opacity.surface`
/// - `outline_color` ← `None` (the card hairline is an SDF border)
/// - `margin_horizontal`, `margin_vertical` ← `space.l`
/// - `backdrop_radius` ← `shape.l`
/// - `text_align` ← `Start`; transitions ← `motion.enter.ms` / `motion.exit.ms`
pub fn apply_notification_area_token_defaults(
    policy: &mut RenderingPolicy,
    tokens: &DesignTokenMap,
) {
    if policy.text_color.is_none() {
        policy.text_color = token_color(tokens, "color.on_surface");
    }
    if policy.font_family.is_none() {
        policy.font_family = token_font_family(tokens, "typography.body.family");
    }
    if policy.font_size_px.is_none() {
        policy.font_size_px = token_f32(tokens, "type.title.m.size");
    }
    if policy.font_weight.is_none() {
        policy.font_weight = token_u16(tokens, "type.title.m.weight");
    }
    if policy.backdrop.is_none() {
        policy.backdrop = token_color(tokens, "color.surface.container.high");
    }
    if policy.backdrop_opacity.is_none() {
        policy.backdrop_opacity = token_f32(tokens, "opacity.surface");
    }
    // The card padding: text, action row and × all sit this far from the edge.
    let padding = token_f32(tokens, "space.l");
    if policy.margin_horizontal.is_none() {
        policy.margin_horizontal = padding;
    }
    if policy.margin_vertical.is_none() {
        policy.margin_vertical = padding;
    }
    if policy.backdrop_radius.is_none() {
        policy.backdrop_radius = token_f32(tokens, "shape.l");
    }
    if policy.text_align.is_none() {
        policy.text_align = Some(TextAlign::Start);
    }
    if policy.transition_in_ms.is_none() {
        policy.transition_in_ms = Some(resolve_motion_duration_ms(tokens, "motion.enter.ms"));
    }
    if policy.transition_out_ms.is_none() {
        policy.transition_out_ms = Some(resolve_motion_duration_ms(tokens, "motion.exit.ms"));
    }
}

/// Apply token-derived defaults for the `status-bar` zone to `policy`.
///
/// The status bar is a compact card styled like a notification card: the
/// theme's tonal container surface, rounded, inset like a card. Values read in
/// `text_color`; the compositor dims each row's key with `color.text.secondary`.
///
/// Token mappings:
/// - `text_color` ← `color.text.primary`
/// - `font_family` ← `typography.body.family`
/// - `font_size_px` ← `typography.body.size`
/// - `backdrop` ← `color.surface.container`
/// - `backdrop_opacity` ← `opacity.surface`
/// - `backdrop_radius` ← `border.radius.medium`
/// - `margin_horizontal`, `margin_vertical` ← `spacing.notification.inset`
/// - `text_align` ← `Start`; `overflow` ← `Ellipsis` (one line per entry)
pub fn apply_status_bar_token_defaults(policy: &mut RenderingPolicy, tokens: &DesignTokenMap) {
    if policy.text_color.is_none() {
        policy.text_color = token_color(tokens, "color.text.primary");
    }
    if policy.font_family.is_none() {
        policy.font_family = token_font_family(tokens, "typography.body.family");
    }
    if policy.font_size_px.is_none() {
        policy.font_size_px = token_f32(tokens, "typography.body.size");
    }
    if policy.backdrop.is_none() {
        policy.backdrop = token_color(tokens, "color.surface.container");
    }
    if policy.backdrop_opacity.is_none() {
        policy.backdrop_opacity = token_f32(tokens, "opacity.surface");
    }
    if policy.backdrop_radius.is_none() {
        policy.backdrop_radius = token_f32(tokens, "border.radius.medium");
    }
    let inset = token_f32(tokens, "spacing.notification.inset");
    if policy.margin_horizontal.is_none() {
        policy.margin_horizontal = inset;
    }
    if policy.margin_vertical.is_none() {
        policy.margin_vertical = inset;
    }
    if policy.text_align.is_none() {
        policy.text_align = Some(TextAlign::Start);
    }
    if policy.overflow.is_none() {
        policy.overflow = Some(TextOverflow::Ellipsis);
    }
}

/// Apply token-derived defaults for the `alert-banner` zone to `policy`.
///
/// Token mappings:
/// - `text_color` ← `color.text.primary` (the compositor draws banner text in
///   `color.on_severity.*` for the publication's urgency instead)
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
    // No text outline: the compositor draws banner text in the
    // `color.on_severity.*` color that contrasts with its severity fill.
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
/// with token-derived defaults filling every `None` field, then every size
/// converted from logical dp to physical px at the map's
/// [`px_scale`](crate::tokens::px_scale) (monitor scale × `ui.scale`).
pub fn build_effective_policy(
    zone_name: &str,
    zone_default: &RenderingPolicy,
    tokens: &DesignTokenMap,
) -> RenderingPolicy {
    let mut policy = zone_default.clone();
    apply_token_defaults_for_zone(zone_name, &mut policy, tokens);
    scale_policy_sizes(&mut policy, crate::tokens::px_scale(tokens));
    policy
}

/// Multiply every size field of `policy` (dp) by `scale` (px per dp).
fn scale_policy_sizes(policy: &mut RenderingPolicy, scale: f32) {
    for size in [
        &mut policy.font_size_px,
        &mut policy.margin_px,
        &mut policy.outline_width,
        &mut policy.margin_horizontal,
        &mut policy.margin_vertical,
        &mut policy.backdrop_radius,
    ]
    .into_iter()
    .flatten()
    {
        *size *= scale;
    }
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
    fn subtitle_backdrop_is_drawn_only_while_its_opacity_is_above_zero() {
        let mut tokens = default_tokens();
        let mut off = RenderingPolicy::default();
        apply_token_defaults_for_zone("subtitle", &mut off, &tokens);
        assert!(off.backdrop.is_none() && off.backdrop_opacity.is_none());

        tokens.insert("opacity.subtitle.backdrop".into(), "0.85".into());
        tokens.insert("color.subtitle.backdrop".into(), "#102030".into());
        let mut on = RenderingPolicy::default();
        apply_token_defaults_for_zone("subtitle", &mut on, &tokens);
        assert_eq!(on.backdrop, token_color(&tokens, "color.subtitle.backdrop"));
        assert_eq!(on.backdrop_opacity, Some(0.85));
    }

    #[test]
    fn notification_margins_follow_the_notification_inset_token() {
        let mut tokens = default_tokens();
        tokens.insert("spacing.notification.inset".into(), "16".into());
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("notification-area", &mut policy, &tokens);
        assert_eq!(policy.margin_horizontal, Some(16.0));
        assert_eq!(policy.margin_vertical, Some(16.0));
    }

    #[test]
    fn alert_banner_draws_no_text_outline() {
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("alert-banner", &mut policy, &default_tokens());
        assert!(policy.outline_color.is_none() && policy.outline_width.is_none());
    }

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

        // Fade durations should follow the resolved canonical motion tokens.
        assert_eq!(policy.transition_in_ms, Some(180));
        assert_eq!(policy.transition_out_ms, Some(120));

        // outline_color should be set
        assert!(policy.outline_color.is_some());

        // overflow should be Ellipsis (subtitle exemplar spec)
        assert_eq!(
            policy.overflow,
            Some(TextOverflow::Ellipsis),
            "subtitle zone must default to TextOverflow::Ellipsis"
        );

        let overrides = DesignTokenMap::from([
            ("motion.enter.ms".into(), "275".into()),
            ("motion.exit.ms".into(), "85".into()),
        ]);
        let resolved = resolve_tokens(&DesignTokenMap::new(), &overrides);
        let overridden = build_effective_policy("subtitle", &RenderingPolicy::default(), &resolved);
        assert_eq!(overridden.transition_in_ms, Some(275));
        assert_eq!(overridden.transition_out_ms, Some(85));
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
            transition_in_ms: Some(0),
            transition_out_ms: Some(275),
            ..RenderingPolicy::default()
        };

        apply_token_defaults_for_zone("subtitle", &mut policy, &tokens);

        // font_size_px should remain 32.0, not the canonical default of 28
        assert_eq!(
            policy.font_size_px,
            Some(32.0),
            "explicit value must not be overwritten by token default"
        );
        assert_eq!(policy.transition_in_ms, Some(0), "explicit snap wins");
        assert_eq!(
            policy.transition_out_ms,
            Some(275),
            "explicit duration wins"
        );
    }

    #[test]
    fn test_custom_color_token_reflected_in_policy() {
        // spec: WHEN color.text.primary = "#00FF00" THEN text_color = Rgba(0,1,0,1)
        let mut config_tokens = DesignTokenMap::new();
        config_tokens.insert("color.text.primary".to_string(), "#00FF00".to_string());
        let tokens = resolve_tokens(&DesignTokenMap::new(), &config_tokens);

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

        // The Tonal Glass card reads semantic roles (hud-h51u7.4.1).
        assert_eq!(policy.text_color, token_color(&tokens, "color.on_surface"));
        assert!(policy.font_family.is_some());
        assert_eq!(policy.font_size_px, token_f32(&tokens, "type.title.m.size"));
        assert_eq!(
            policy.font_weight,
            token_u16(&tokens, "type.title.m.weight")
        );
        assert_eq!(
            policy.backdrop,
            token_color(&tokens, "color.surface.container.high")
        );
        assert_eq!(
            policy.backdrop_opacity,
            token_f32(&tokens, "opacity.surface")
        );
        // No text outline: the card hairline is an SDF border.
        assert!(
            policy.outline_color.is_none(),
            "notification-area must NOT have outline_color set from tokens"
        );
        assert_eq!(policy.margin_horizontal, token_f32(&tokens, "space.l"));
        assert_eq!(policy.margin_vertical, token_f32(&tokens, "space.l"));
        assert_eq!(policy.backdrop_radius, token_f32(&tokens, "shape.l"));
        assert_eq!(policy.backdrop_radius, Some(16.0), "canonical shape.l");
        assert_eq!(policy.transition_in_ms, Some(180));
        assert_eq!(policy.transition_out_ms, Some(120));
        let overridden = resolve_tokens(
            &DesignTokenMap::new(),
            &DesignTokenMap::from([
                ("motion.enter.ms".into(), "320".into()),
                ("motion.exit.ms".into(), "0".into()),
            ]),
        );
        let policy = build_effective_policy(
            "notification-area",
            &RenderingPolicy::default(),
            &overridden,
        );
        assert_eq!(policy.transition_in_ms, Some(320));
        assert_eq!(policy.transition_out_ms, Some(0));

        // Sizes are dp: at a 150% monitor they are 1.5x, and ui.scale 1.25
        // multiplies on top (1.875x). Opacity and timing do not scale.
        let at = |ui_scale: &str| {
            let mut tokens = resolve_tokens(
                &DesignTokenMap::new(),
                &DesignTokenMap::from([(crate::tokens::UI_SCALE_KEY.into(), ui_scale.into())]),
            );
            crate::tokens::set_px_scale(&mut tokens, 1.5);
            build_effective_policy("notification-area", &RenderingPolicy::default(), &tokens)
        };
        let base = build_effective_policy(
            "notification-area",
            &RenderingPolicy::default(),
            &default_tokens(),
        );
        for (ui_scale, factor) in [("1", 1.5), ("1.25", 1.875)] {
            let scaled = at(ui_scale);
            for (name, dp, px) in [
                ("font", base.font_size_px, scaled.font_size_px),
                ("h margin", base.margin_horizontal, scaled.margin_horizontal),
                ("v margin", base.margin_vertical, scaled.margin_vertical),
                ("radius", base.backdrop_radius, scaled.backdrop_radius),
            ] {
                let (dp, px) = (dp.unwrap(), px.unwrap());
                assert!(
                    (px - dp * factor).abs() < 1e-3,
                    "{name} at {ui_scale}: {dp} -> {px}"
                );
            }
            assert_eq!(scaled.backdrop_opacity, base.backdrop_opacity);
            assert_eq!(scaled.transition_in_ms, base.transition_in_ms);
        }
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

        // A card on the theme's tonal surface, values in primary text.
        assert_eq!(
            policy.text_color,
            token_color(&tokens, "color.text.primary")
        );
        assert!(policy.font_family.is_some());
        assert!(policy.font_size_px.is_some());
        assert_eq!(
            policy.backdrop,
            token_color(&tokens, "color.surface.container")
        );
        assert_eq!(
            policy.backdrop_opacity,
            token_f32(&tokens, "opacity.surface")
        );
        assert_eq!(
            policy.backdrop_radius,
            token_f32(&tokens, "border.radius.medium")
        );
        assert_eq!(
            policy.margin_horizontal,
            token_f32(&tokens, "spacing.notification.inset")
        );
        assert_eq!(policy.overflow, Some(TextOverflow::Ellipsis));
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
