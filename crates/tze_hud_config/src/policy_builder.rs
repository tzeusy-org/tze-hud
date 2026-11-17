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

/// Convert a parsed token colour to the scene-types `Rgba` the compositor
/// draws. Token hex is sRGB; scene colours are linear (the renderer encodes
/// back to sRGB on output), so the colour channels are linearised here, once.
/// Alpha is coverage and stays as declared.
fn tokens_color_to_scene(c: crate::tokens::Rgba) -> Rgba {
    fn linear(c: f32) -> f32 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    Rgba {
        r: linear(c.r),
        g: linear(c.g),
        b: linear(c.b),
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

/// Caption typography and text padding for the subtitle pill. Explicit owner
/// policy values win. The optional whole-zone contrast backdrop is separate
/// from the compositor's content-hugging themed pill.
pub fn apply_subtitle_token_defaults(policy: &mut RenderingPolicy, tokens: &DesignTokenMap) {
    if policy.text_color.is_none() {
        policy.text_color = token_color(tokens, "color.on_surface");
    }
    if policy.font_family.is_none() {
        // Named caption families use the configured sans role; generic family
        // keywords can select mono/serif without changing other zone roles.
        policy.font_family = token_font_family(tokens, "type.caption.display.family")
            .or(Some(FontFamily::SystemSansSerif));
    }
    if policy.font_size_px.is_none() {
        policy.font_size_px = token_f32(tokens, "type.caption.display.size");
    }
    if policy.font_weight.is_none() {
        policy.font_weight = token_u16(tokens, "type.caption.display.weight");
    }
    // Preserve Appearance → Subtitle backdrop: an optional whole-zone fill,
    // independent of the content-hugging themed pill.
    if policy.backdrop.is_none()
        && let Some(opacity) = token_f32(tokens, "opacity.subtitle.backdrop").filter(|o| *o > 0.0)
    {
        policy.backdrop = token_color(tokens, "color.subtitle.backdrop");
        policy.backdrop_opacity = Some(opacity.min(1.0));
    }
    if policy.outline_color.is_none() {
        policy.outline_color = token_color(tokens, "color.subtitle.outline");
    }
    if policy.outline_width.is_none() {
        policy.outline_width = token_f32(tokens, "stroke.subtitle.text");
    }
    // text_align: hardcoded default (Center), not token-driven
    if policy.text_align.is_none() {
        policy.text_align = Some(TextAlign::Center);
    }
    if policy.margin_horizontal.is_none() {
        policy.margin_horizontal = policy
            .margin_px
            .or_else(|| token_f32(tokens, "space.subtitle.horizontal"));
    }
    if policy.margin_vertical.is_none() {
        policy.margin_vertical = policy
            .margin_px
            .or_else(|| token_f32(tokens, "space.subtitle.vertical"));
    }
    // overflow: Ellipsis (hardcoded, not token-driven)
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
/// Token mappings (the notification card roles,; the
/// compositor's `NotificationStyle` reads every other card metric and colour
/// from tokens directly):
/// - `text_color` ← `color.on_surface`
/// - `font_family` ← `typography.body.family`
/// - `font_size_px` / `font_weight` ← `type.notification.title.size` /
///   `.weight` (the title; the body and action roles scale with an explicit
///   size)
/// - `backdrop` ← `color.notification.container` (presence enables the card;
///   its fill is the urgency tone)
/// - `backdrop_opacity` ← `opacity.notification`
/// - `outline_color` ← `None` (the card hairline is an SDF border)
/// - `margin_horizontal`, `margin_vertical` ← `space.notification`
/// - `backdrop_radius` ← `shape.notification`
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
        policy.font_size_px = token_f32(tokens, "type.notification.title.size");
    }
    if policy.font_weight.is_none() {
        policy.font_weight = token_u16(tokens, "type.notification.title.weight");
    }
    if policy.backdrop.is_none() {
        policy.backdrop = token_color(tokens, "color.notification.container");
    }
    if policy.backdrop_opacity.is_none() {
        policy.backdrop_opacity = token_f32(tokens, "opacity.notification");
    }
    // The card padding: text, action row and × all sit this far from the edge.
    let padding = token_f32(tokens, "space.notification");
    if policy.margin_horizontal.is_none() {
        policy.margin_horizontal = padding;
    }
    if policy.margin_vertical.is_none() {
        policy.margin_vertical = padding;
    }
    if policy.backdrop_radius.is_none() {
        policy.backdrop_radius = token_f32(tokens, "shape.notification");
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
/// The status bar is a content-sized horizontal pill: the
/// theme's tonal container surface, rounded, inset like a card. Values read in
/// `text_color`; the compositor dims each entry's key with `color.on_surface.variant`.
///
/// Token mappings:
/// - `text_color` ← `color.on_surface`
/// - `font_family` ← `type.label.m.family`
/// - `font_size_px` ← `type.label.m.size`
/// - `backdrop` ← `color.surface.container.low`
/// - `backdrop_opacity` ← `opacity.surface`
/// - `backdrop_radius` ← `shape.full`
/// - `margin_horizontal` ← `space.status.horizontal`
/// - `text_align` ← `Start`; `overflow` ← `Ellipsis` (one horizontal entry)
pub fn apply_status_bar_token_defaults(policy: &mut RenderingPolicy, tokens: &DesignTokenMap) {
    if policy.text_color.is_none() {
        policy.text_color = token_color(tokens, "color.on_surface");
    }
    if policy.font_family.is_none() {
        // Named label faces use the existing shared sans role (`font.sans`).
        policy.font_family =
            token_font_family(tokens, "type.label.m.family").or(Some(FontFamily::SystemSansSerif));
    }
    if policy.font_size_px.is_none() {
        policy.font_size_px = token_f32(tokens, "type.label.m.size");
    }
    if policy.font_weight.is_none() {
        policy.font_weight = token_u16(tokens, "type.label.m.weight");
    }
    if policy.backdrop.is_none() {
        policy.backdrop = token_color(tokens, "color.surface.container.low");
    }
    if policy.backdrop_opacity.is_none() {
        policy.backdrop_opacity = token_f32(tokens, "opacity.surface");
    }
    if policy.backdrop_radius.is_none() {
        policy.backdrop_radius = token_f32(tokens, "shape.full");
    }
    let inset = token_f32(tokens, "space.status.horizontal");
    if policy.margin_horizontal.is_none() {
        policy.margin_horizontal = inset;
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
/// Token mappings (the alert banner roles,; the compositor's
/// `NotificationStyle` reads every other banner metric and colour from
/// tokens directly):
/// - `text_color` ← `color.on_alert` (non-notification stream text)
/// - `font_family` ← `typography.body.family`
/// - `font_size_px` / `font_weight` ← `type.alert.title.size` / `.weight`
/// - `backdrop` ← `color.alert.container` (presence enables the banner; its
///   fill is the urgency tone)
/// - `backdrop_opacity` ← `opacity.alert`
/// - `margin_horizontal` / `margin_vertical` ← `space.alert.horizontal` /
///   `.vertical`
/// - `backdrop_radius` ← `shape.alert`
/// - `text_align` ← `Start`. No text outline: banner text is drawn in a role
///   that contrasts with its fill.
pub fn apply_alert_banner_token_defaults(policy: &mut RenderingPolicy, tokens: &DesignTokenMap) {
    if policy.text_color.is_none() {
        policy.text_color = token_color(tokens, "color.on_alert");
    }
    if policy.font_family.is_none() {
        policy.font_family = token_font_family(tokens, "typography.body.family");
    }
    if policy.font_size_px.is_none() {
        policy.font_size_px = token_f32(tokens, "type.alert.title.size");
    }
    if policy.font_weight.is_none() {
        policy.font_weight = token_u16(tokens, "type.alert.title.weight");
    }
    if policy.backdrop.is_none() {
        policy.backdrop = token_color(tokens, "color.alert.container");
    }
    if policy.backdrop_opacity.is_none() {
        policy.backdrop_opacity = token_f32(tokens, "opacity.alert");
    }
    if policy.margin_horizontal.is_none() {
        policy.margin_horizontal = token_f32(tokens, "space.alert.horizontal");
    }
    if policy.margin_vertical.is_none() {
        policy.margin_vertical = token_f32(tokens, "space.alert.vertical");
    }
    if policy.backdrop_radius.is_none() {
        policy.backdrop_radius = token_f32(tokens, "shape.alert");
    }
    if policy.text_align.is_none() {
        policy.text_align = Some(TextAlign::Start);
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
        let tokens = default_tokens();
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

        assert_eq!(policy.text_color, token_color(&tokens, "color.on_surface"));
        assert_eq!(
            policy.font_size_px,
            token_f32(&tokens, "type.caption.display.size")
        );
        assert_eq!(
            policy.font_weight,
            token_u16(&tokens, "type.caption.display.weight")
        );
        assert_eq!(
            policy.margin_horizontal,
            token_f32(&tokens, "space.subtitle.horizontal")
        );
        assert_eq!(
            policy.margin_vertical,
            token_f32(&tokens, "space.subtitle.vertical")
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

        // overflow should be Ellipsis
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
        // Explicit owner values take precedence over semantic token defaults.
        let mut policy = RenderingPolicy {
            font_size_px: Some(32.0),
            margin_px: Some(5.0),
            transition_in_ms: Some(0),
            transition_out_ms: Some(275),
            ..RenderingPolicy::default()
        };

        apply_token_defaults_for_zone("subtitle", &mut policy, &tokens);

        // Keep explicit typography, unified padding and motion values.
        assert_eq!(
            policy.font_size_px,
            Some(32.0),
            "explicit value must not be overwritten by token default"
        );
        assert_eq!(
            (policy.margin_horizontal, policy.margin_vertical),
            (Some(5.0), Some(5.0)),
            "explicit unified padding wins over axis token defaults"
        );
        let mut axis_policy = RenderingPolicy {
            margin_px: Some(5.0),
            margin_horizontal: Some(7.0),
            margin_vertical: Some(9.0),
            ..RenderingPolicy::default()
        };
        apply_token_defaults_for_zone("subtitle", &mut axis_policy, &tokens);
        assert_eq!(
            (axis_policy.margin_horizontal, axis_policy.margin_vertical),
            (Some(7.0), Some(9.0)),
            "explicit axis padding wins over unified padding"
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
        // spec: WHEN color.on_surface = "#00FF00" THEN text_color = Rgba(0,1,0,1)
        let mut config_tokens = DesignTokenMap::new();
        config_tokens.insert("color.on_surface".to_string(), "#00FF00".to_string());
        let tokens = resolve_tokens(&DesignTokenMap::new(), &config_tokens);

        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("subtitle", &mut policy, &tokens);

        let tc = policy.text_color.expect("text_color should be set");
        assert!((tc.r).abs() < 1e-4, "r should be 0 for #00FF00");
        assert!((tc.g - 1.0).abs() < 1e-4, "g should be 1.0 for #00FF00");
        assert!((tc.b).abs() < 1e-4, "b should be 0 for #00FF00");

        // Mid-range channels are linearised: sRGB 0x80 is linear ~0.2159.
        config_tokens.insert("color.on_surface".to_string(), "#80808080".to_string());
        let tokens = resolve_tokens(&DesignTokenMap::new(), &config_tokens);
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("subtitle", &mut policy, &tokens);
        let tc = policy.text_color.expect("text_color should be set");
        assert!(
            (tc.r - 0.2159).abs() < 1e-3,
            "sRGB 0x80 is linear 0.2159, got {}",
            tc.r
        );
        assert!(
            (tc.a - 128.0 / 255.0).abs() < 1e-4,
            "alpha is not gamma-encoded"
        );
    }

    #[test]
    fn test_notification_area_token_defaults() {
        let tokens = default_tokens();
        let mut policy = RenderingPolicy::default();
        apply_token_defaults_for_zone("notification-area", &mut policy, &tokens);

        // The card reads its own notification roles.
        assert_eq!(policy.text_color, token_color(&tokens, "color.on_surface"));
        assert!(policy.font_family.is_some());
        assert_eq!(
            policy.font_size_px,
            token_f32(&tokens, "type.notification.title.size")
        );
        assert_eq!(
            policy.font_weight,
            token_u16(&tokens, "type.notification.title.weight")
        );
        assert_eq!(
            policy.backdrop,
            token_color(&tokens, "color.notification.container")
        );
        assert_eq!(
            policy.backdrop_opacity,
            token_f32(&tokens, "opacity.notification")
        );
        // No text outline: the card hairline is an SDF border.
        assert!(
            policy.outline_color.is_none(),
            "notification-area must NOT have outline_color set from tokens"
        );
        assert_eq!(
            policy.margin_horizontal,
            token_f32(&tokens, "space.notification")
        );
        assert_eq!(
            policy.margin_vertical,
            token_f32(&tokens, "space.notification")
        );
        assert_eq!(
            policy.backdrop_radius,
            token_f32(&tokens, "shape.notification")
        );
        assert_eq!(
            policy.backdrop_radius,
            Some(16.0),
            "canonical shape.notification"
        );
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
        assert_eq!(policy.text_color, token_color(&tokens, "color.on_surface"));
        assert!(policy.font_family.is_some());
        assert!(policy.font_size_px.is_some());
        assert_eq!(
            policy.backdrop,
            token_color(&tokens, "color.surface.container.low")
        );
        assert_eq!(
            policy.backdrop_opacity,
            token_f32(&tokens, "opacity.surface")
        );
        assert_eq!(policy.backdrop_radius, token_f32(&tokens, "shape.full"));
        assert_eq!(
            policy.margin_horizontal,
            token_f32(&tokens, "space.status.horizontal")
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
        // Seeded from the alert banner roles.
        assert_eq!(
            policy.font_size_px,
            token_f32(&tokens, "type.alert.title.size")
        );
        assert_eq!(
            policy.backdrop,
            token_color(&tokens, "color.alert.container")
        );
        assert_eq!(policy.backdrop_opacity, token_f32(&tokens, "opacity.alert"));
        assert_eq!(policy.backdrop_radius, token_f32(&tokens, "shape.alert"));
        assert_eq!(
            (policy.margin_horizontal, policy.margin_vertical),
            (
                token_f32(&tokens, "space.alert.horizontal"),
                token_f32(&tokens, "space.alert.vertical")
            )
        );
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
