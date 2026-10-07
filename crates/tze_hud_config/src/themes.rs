//! Named themes: swappable, token-only layers over the canonical defaults.
//!
//! A theme is a flat TOML map of canonical design-token keys and the existing
//! portal color keys to values (no layout logic). Built-in themes live in
//! `assets/themes/<name>.toml` and are embedded in the binary. Config selects
//! one with the reserved key
//! `[design_tokens] theme = "<name>"`; unset selects [`DEFAULT_THEME`].
//!
//! Resolution (lowest to highest): canonical defaults → selected theme →
//! the remaining `[design_tokens]` entries. See [`resolve_config_tokens`].
//!
//! ## Error codes produced
//!
//! | Error code | Condition |
//! |---|---|
//! | `CONFIG_UNKNOWN_THEME` | `[design_tokens] theme` names no built-in theme |

use tze_hud_scene::config::{ConfigError, ConfigErrorCode};

use crate::portal_tokens;
use crate::raw::RawConfig;
use crate::tokens::{DesignTokenMap, parse_color_hex, resolve_tokens, validate_canonical_value};

/// Reserved `[design_tokens]` key that selects a theme. Never a token itself:
/// it is stripped before resolution.
pub const THEME_KEY: &str = "theme";

/// Theme used when `[design_tokens] theme` is unset.
pub const DEFAULT_THEME: &str = "tonal-glass";

/// Built-in themes: `(name, TOML source)`.
const BUILTIN_THEMES: &[(&str, &str)] = &[
    (
        "tonal-glass",
        include_str!("../../../assets/themes/tonal-glass.toml"),
    ),
    (
        "classic",
        include_str!("../../../assets/themes/classic.toml"),
    ),
    (
        "blueprint",
        include_str!("../../../assets/themes/blueprint.toml"),
    ),
];

// Existing documented portal colors are valid theme overrides without adding
// canonical defaults. Numeric/family portal keys and unknown names stay outside
// this exception. Use exact constants rather than a portal.* prefix rule.
const PORTAL_COLOR_KEYS: &[&str] = &[
    portal_tokens::PORTAL_TOKEN_FRAME_BACKGROUND,
    portal_tokens::PORTAL_TOKEN_FRAME_BORDER_COLOR,
    portal_tokens::PORTAL_TOKEN_HEADER_TEXT_COLOR,
    portal_tokens::PORTAL_TOKEN_COMPOSER_BACKGROUND,
    portal_tokens::PORTAL_TOKEN_COMPOSER_TEXT_COLOR,
    portal_tokens::PORTAL_TOKEN_COMPOSER_AT_CAPACITY_COLOR,
    portal_tokens::PORTAL_TOKEN_TRANSCRIPT_BACKGROUND,
    portal_tokens::PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR,
    portal_tokens::PORTAL_TOKEN_TRANSCRIPT_SYSTEM_COLOR,
    portal_tokens::PORTAL_TOKEN_TRANSCRIPT_CODE_BACKGROUND,
    portal_tokens::PORTAL_TOKEN_TRANSCRIPT_CODE_TEXT,
    portal_tokens::PORTAL_TOKEN_TRANSCRIPT_LINK_COLOR,
    portal_tokens::PORTAL_TOKEN_TRANSCRIPT_DIM_TEXT_COLOR,
    portal_tokens::PORTAL_TOKEN_TRANSCRIPT_DIM_BACKGROUND,
    portal_tokens::PORTAL_TOKEN_STALE_MARKER_COLOR,
    portal_tokens::PORTAL_TOKEN_DISCONNECT_BADGE_COLOR,
    portal_tokens::PORTAL_TOKEN_UNREAD_INDICATOR_COLOR,
    portal_tokens::PORTAL_TOKEN_AWAITING_REPLY_COLOR,
    portal_tokens::PORTAL_TOKEN_EMPTY_STATE_COLOR,
    portal_tokens::PORTAL_TOKEN_CONNECTING_MARKER_COLOR,
    portal_tokens::PORTAL_TOKEN_ACTIVITY_CUE_COLOR,
    portal_tokens::PORTAL_TOKEN_STREAMING_CURSOR_COLOR,
    portal_tokens::PORTAL_TOKEN_DELIVERY_INFLIGHT_COLOR,
    portal_tokens::PORTAL_TOKEN_DELIVERY_DELIVERED_COLOR,
    portal_tokens::PORTAL_TOKEN_DELIVERY_FAILED_COLOR,
    portal_tokens::PORTAL_TOKEN_TIMESTAMP_COLOR,
    portal_tokens::PORTAL_TOKEN_LIFECYCLE_ACTIVE_COLOR,
    portal_tokens::PORTAL_TOKEN_LIFECYCLE_ATTACHED_COLOR,
    portal_tokens::PORTAL_TOKEN_LIFECYCLE_ATTENTION_COLOR,
    portal_tokens::PORTAL_TOKEN_LIFECYCLE_INACTIVE_COLOR,
    portal_tokens::PORTAL_TOKEN_DIVIDER_COLOR,
    portal_tokens::PORTAL_TOKEN_UNREAD_DIVIDER_COLOR,
    portal_tokens::PORTAL_TOKEN_COLLAPSED_BACKGROUND,
    portal_tokens::PORTAL_TOKEN_COLLAPSED_TEXT_COLOR,
    portal_tokens::PORTAL_TOKEN_SCROLL_INDICATOR_COLOR,
    portal_tokens::PORTAL_TOKEN_COMPOSER_CARET_COLOR,
    portal_tokens::PORTAL_TOKEN_COMPOSER_SELECTION_COLOR,
    portal_tokens::PORTAL_TOKEN_COMPOSER_PLACEHOLDER_COLOR,
    portal_tokens::PORTAL_TOKEN_FOCUS_RING_COLOR,
    portal_tokens::PORTAL_TOKEN_WINDOW_RESIZE_GRIP_COLOR,
    portal_tokens::PORTAL_TOKEN_WINDOW_RESIZE_GRIP_HOVER_COLOR,
];

fn validate_theme_value(key: &str, value: &str) -> Result<(), String> {
    if PORTAL_COLOR_KEYS.contains(&key) {
        parse_color_hex(value)
            .map(|_| ())
            .ok_or_else(|| "a color #RRGGBB or #RRGGBBAA".into())
    } else {
        validate_canonical_value(key, value)
    }
}

/// Names of the built-in themes, in declaration order.
pub fn builtin_theme_names() -> impl Iterator<Item = &'static str> {
    BUILTIN_THEMES.iter().map(|(name, _)| *name)
}

/// Parse and validate a theme source: canonical tokens and existing documented
/// portal color keys, with string values valid for their kind. Errors list every
/// bad entry. Portal colors do not add defaults or allow arbitrary portal keys.
///
/// Other keys are checked against [`crate::tokens::CANONICAL_TOKENS`] rather than
/// the `[design_tokens]` key pattern, so a theme can also set the runtime-only
/// `system_card.*` / `safe_mode.*` tokens that config cannot spell.
pub fn parse_theme(src: &str) -> Result<DesignTokenMap, Vec<String>> {
    let map: DesignTokenMap = toml::from_str(src).map_err(|e| vec![e.to_string()])?;
    let mut errors: Vec<String> = map
        .iter()
        .filter_map(|(key, value)| {
            validate_theme_value(key, value)
                .err()
                .map(|expected| format!("{key:?} = {value:?}: expected {expected}"))
        })
        .collect();
    if errors.is_empty() {
        Ok(map)
    } else {
        errors.sort();
        Err(errors)
    }
}

/// Tokens of the built-in theme `name`, or `None` if there is no such theme.
///
/// # Panics
///
/// If a built-in theme file is invalid; the `builtin_themes_are_valid` test
/// guarantees they are not.
pub fn builtin_theme(name: &str) -> Option<DesignTokenMap> {
    let (_, src) = BUILTIN_THEMES.iter().find(|(n, _)| *n == name)?;
    Some(parse_theme(src).unwrap_or_else(|e| panic!("built-in theme {name:?} invalid: {e:?}")))
}

/// The theme a `[design_tokens]` table selects ([`DEFAULT_THEME`] if unset).
pub fn selected_theme_name(config_tokens: &DesignTokenMap) -> &str {
    config_tokens
        .get(THEME_KEY)
        .map(String::as_str)
        .unwrap_or(DEFAULT_THEME)
}

/// Resolve a raw `[design_tokens]` table: canonical defaults → selected theme
/// → the table's other entries. The `theme` selector is not a token and is
/// not in the result.
///
/// Pure, so a later live theme swap can re-run it. An unknown theme name is
/// rejected by config validation ([`validate_theme`]), which fails strict
/// startup. Only the dev insecure-startup override
/// (`TZE_HUD_DEV_ALLOW_INSECURE_STARTUP`) runs with invalid config; there it
/// falls back to [`DEFAULT_THEME`] with a warning, like that mode's other
/// permissive fallbacks.
pub fn resolve_config_tokens(config_tokens: &DesignTokenMap) -> DesignTokenMap {
    let name = selected_theme_name(config_tokens);
    let theme = builtin_theme(name).unwrap_or_else(|| {
        tracing::warn!(theme = name, fallback = DEFAULT_THEME, "unknown theme");
        builtin_theme(DEFAULT_THEME).expect("default theme is built in")
    });
    let mut overrides = config_tokens.clone();
    overrides.remove(THEME_KEY);
    resolve_tokens(&theme, &overrides)
}

/// Validate `[design_tokens] theme`: it must name a built-in theme.
pub fn validate_theme(raw: &RawConfig, errors: &mut Vec<ConfigError>) {
    let Some(name) = raw
        .design_tokens
        .as_ref()
        .and_then(|dt| dt.0.get(THEME_KEY))
    else {
        return;
    };
    if builtin_theme_names().any(|n| n == name) {
        return;
    }
    let available = builtin_theme_names().collect::<Vec<_>>().join(", ");
    errors.push(ConfigError {
        code: ConfigErrorCode::UnknownTheme,
        field_path: format!("design_tokens.{THEME_KEY}"),
        expected: format!("one of: {available}"),
        got: format!("{name:?}"),
        hint: format!("unknown theme {name:?}; available themes: {available}"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawDesignTokens;
    use crate::tokens::CANONICAL_TOKENS;

    fn tokens(pairs: &[(&str, &str)]) -> DesignTokenMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn builtin_themes_are_valid() {
        for (name, src) in BUILTIN_THEMES {
            let map = parse_theme(src)
                .unwrap_or_else(|e| panic!("built-in theme {name:?} invalid: {e:#?}"));
            assert!(!map.is_empty(), "built-in theme {name:?} is empty");
            assert!(!map.contains_key(THEME_KEY), "{name:?} sets the selector");
        }
        assert!(builtin_theme_names().any(|n| n == DEFAULT_THEME));

        // Enumerate the documented keys independently of the validator list:
        // every existing color must accept both formats and preserve the input.
        let documented_colors = [
            "portal.frame.background",
            "portal.frame.border_color",
            "portal.header.text_color",
            "portal.composer.background",
            "portal.composer.text_color",
            "portal.composer.at_capacity_color",
            "portal.transcript.background",
            "portal.transcript.text_color",
            "portal.transcript.system_color",
            "portal.transcript.code_background",
            "portal.transcript.code_text",
            "portal.transcript.link_color",
            "portal.transcript.dim_text_color",
            "portal.transcript.dim_background",
            "portal.stale_marker.color",
            "portal.disconnect_badge.color",
            "portal.unread_indicator.color",
            "portal.awaiting_reply.color",
            "portal.empty_state.color",
            "portal.connecting_marker.color",
            "portal.activity_cue.color",
            "portal.streaming_cursor.color",
            "portal.delivery.inflight_color",
            "portal.delivery.delivered_color",
            "portal.delivery.failed_color",
            "portal.timestamp.color",
            "portal.lifecycle.active_color",
            "portal.lifecycle.attached_color",
            "portal.lifecycle.attention_color",
            "portal.lifecycle.inactive_color",
            "portal.divider.color",
            "portal.unread_divider.color",
            "portal.collapsed_card.background",
            "portal.collapsed_card.text_color",
            "portal.scroll_indicator.color",
            "portal.composer.caret_color",
            "portal.composer.selection_color",
            "portal.composer.placeholder_color",
            "portal.focus_ring.color",
            "portal.window.resize_grip.color",
            "portal.window.resize_grip.hover_color",
        ];
        for value in ["#123456", "#abcdef80", "  #AaBbCcDd  "] {
            let src = documented_colors
                .iter()
                .map(|key| format!("{key:?} = {value:?}\n"))
                .collect::<String>();
            let theme = parse_theme(&src).unwrap();
            assert_eq!(theme.len(), documented_colors.len());
            for key in documented_colors {
                assert_eq!(theme[key], value, "{key} must retain {value:?}");
            }
        }
        // The exception must not seed portal defaults into every resolved map.
        let canonical: DesignTokenMap = CANONICAL_TOKENS
            .iter()
            .map(|token| (token.key.to_string(), token.default_value.to_string()))
            .collect();
        assert_eq!(
            resolve_tokens(&DesignTokenMap::new(), &DesignTokenMap::new()),
            canonical
        );
    }

    /// Every built-in theme declares a complete semantic palette. Shared
    /// values are intentional declarations, rather than canonical fallback.
    #[test]
    fn full_themes_set_every_semantic_token() {
        let semantic = [
            "color.surface",
            "color.on_",
            "color.outline",
            "color.primary",
            "color.success",
            "color.caution",
            "color.error",
            "opacity.surface",
            "opacity.scrim",
            "state.",
            "focus.",
            "motion.",
            "shape.",
            "space.",
            "font.",
            "type.",
        ];
        for name in builtin_theme_names() {
            let theme = builtin_theme(name).unwrap();
            let resolved = resolve_config_tokens(&tokens(&[(THEME_KEY, name)]));
            for t in CANONICAL_TOKENS {
                if t.key != "color.outline.default" && semantic.iter().any(|p| t.key.starts_with(p))
                {
                    assert!(theme.contains_key(t.key), "{name} misses {}", t.key);
                    assert_eq!(resolved[t.key], theme[t.key], "{name}: {} fell back", t.key);
                }
            }
        }

        let classic = builtin_theme("classic").unwrap();
        for (role, legacy) in [
            ("color.on_surface", "color.text.primary"),
            ("color.outline", "color.border.default"),
            (
                "color.caution.container",
                "color.notification.urgency.urgent",
            ),
            (
                "color.error.container",
                "color.notification.urgency.critical",
            ),
            ("type.body.m.size", "typography.body.size"),
            ("type.body.m.weight", "typography.body.weight"),
        ] {
            assert_eq!(
                classic[role], classic[legacy],
                "Classic {role} lost its legacy meaning"
            );
        }

        // Opaque declared text/fill pairs only: no wallpaper, state layer or
        // user override is certified by these unrounded contrast assertions.
        let luminance = |key: &str| {
            let color = crate::tokens::parse_color_hex(&classic[key]).unwrap();
            assert_eq!(color.a, 1.0, "{key} must be opaque for this text pair");
            let linear = |component: f32| {
                let c = f64::from(component);
                if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
        };
        for (foreground, background) in [
            ("color.on_surface", "color.surface"),
            ("color.on_surface", "color.surface.container.low"),
            ("color.on_surface", "color.surface.container"),
            ("color.on_surface", "color.surface.container.high"),
            ("color.on_surface", "color.surface.container.highest"),
            ("color.on_surface.variant", "color.surface"),
            ("color.on_surface.variant", "color.surface.container.low"),
            ("color.on_surface.variant", "color.surface.container"),
            ("color.on_surface.variant", "color.surface.container.high"),
            (
                "color.on_surface.variant",
                "color.surface.container.highest",
            ),
            ("color.on_primary", "color.primary"),
            ("color.on_primary.container", "color.primary.container"),
            ("color.on_caution.container", "color.caution.container"),
            ("color.on_error.container", "color.error.container"),
        ] {
            let a = luminance(foreground);
            let b = luminance(background);
            let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(
                ratio >= 4.5,
                "Classic {foreground} on {background}: {ratio}"
            );
        }
    }

    #[test]
    fn parse_theme_rejects_unknown_keys_and_bad_values() {
        let errors = parse_theme(
            r##""color.surface" = "teal"
"not.canonical" = "1"
"motion.enter.easing" = "bouncy"
"shape.m" = "12"
"##,
        )
        .unwrap_err();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(
            parse_theme("\"shape.m\" = 12").is_err(),
            "values are strings"
        );

        for value in ["", "teal", "#12345", "#1234567", "#GGGGGG", "#你好"] {
            let src = PORTAL_COLOR_KEYS
                .iter()
                .map(|key| format!("{key:?} = {value:?}\n"))
                .collect::<String>();
            let errors = parse_theme(&src).unwrap_err();
            let mut expected = PORTAL_COLOR_KEYS
                .iter()
                .map(|key| format!("{key:?} = {value:?}: expected a color #RRGGBB or #RRGGBBAA"))
                .collect::<Vec<_>>();
            expected.sort();
            assert_eq!(errors, expected, "invalid color {value:?}");
        }
        for key in PORTAL_COLOR_KEYS {
            assert!(
                parse_theme(&format!("{key:?} = 12")).is_err(),
                "{key} requires a string"
            );
        }
        for key in [
            "",
            "portal.fake.color",
            portal_tokens::PORTAL_TOKEN_FRAME_OPACITY,
            portal_tokens::PORTAL_TOKEN_TRANSCRIPT_CODE_FONT_FAMILY,
            portal_tokens::PORTAL_TOKEN_TIMESTAMP_GRANULARITY,
        ] {
            assert_eq!(
                parse_theme(&format!("{key:?} = \"#123456\"")).unwrap_err(),
                [format!(
                    "{key:?} = \"#123456\": expected a canonical token key"
                )]
            );
        }

        // Accepted theme entries traverse the real resolver. Explicit config
        // still wins, and an invalid direct config color keeps its fallback.
        let theme = parse_theme(
            r##""portal.frame.background" = "#12345680"
"portal.transcript.background" = "#234567"
"portal.composer.background" = "#345678"
"##,
        )
        .unwrap();
        let config = tokens(&[(portal_tokens::PORTAL_TOKEN_COMPOSER_BACKGROUND, "#abcdef40")]);
        let resolved = resolve_tokens(&theme, &config);
        let part = portal_tokens::resolve_portal_tokens(&resolved);
        assert_eq!(part.frame_background, parse_color_hex("#12345680").unwrap());
        assert_eq!(
            part.transcript_background,
            parse_color_hex("#234567").unwrap()
        );
        assert_eq!(
            part.composer_background,
            parse_color_hex("#abcdef40").unwrap()
        );
        let defaults = portal_tokens::PortalPartTokens::default();
        assert_eq!(part.header_text_color, defaults.header_text_color);
        let invalid_override = tokens(&[(portal_tokens::PORTAL_TOKEN_COMPOSER_BACKGROUND, "teal")]);
        let part = portal_tokens::resolve_portal_tokens(&resolve_tokens(&theme, &invalid_override));
        assert_eq!(part.composer_background, defaults.composer_background);
    }

    #[test]
    fn unset_theme_selects_default() {
        let resolved = resolve_config_tokens(&DesignTokenMap::new());
        let default = builtin_theme(DEFAULT_THEME).unwrap();
        for (k, v) in &default {
            assert_eq!(&resolved[k], v, "{k} must come from {DEFAULT_THEME}");
        }
        assert!(!resolved.contains_key(THEME_KEY));
    }

    #[test]
    fn selected_theme_layers_between_canonical_and_config() {
        let config = tokens(&[
            ("theme", "classic"),
            ("border.radius.medium", "3"),
            ("custom.key", "x"),
            ("color.on_surface", "#FFFFFF"),
            ("color.surface.container", "#102030"),
        ]);
        let resolved = resolve_config_tokens(&config);
        // classic sets these; config does not.
        assert_eq!(resolved["color.text.primary"], "#F5F7FA");
        // config beats the theme.
        assert_eq!(resolved["border.radius.medium"], "3");
        assert_eq!(resolved["custom.key"], "x");
        // canonical fills what classic leaves out.
        assert_eq!(
            resolved["color.text.accent"],
            crate::tokens::canonical_token("color.text.accent")
                .unwrap()
                .default_value
        );
        assert!(!resolved.contains_key(THEME_KEY));
        // Explicit semantic overrides also beat selected Classic declarations.
        let classic = builtin_theme("classic").unwrap();
        for key in ["color.on_surface", "color.surface.container"] {
            assert_ne!(classic[key], config[key]);
            assert_eq!(resolved[key], config[key]);
        }
    }

    #[test]
    fn unknown_theme_is_a_validation_error_listing_themes() {
        let raw = RawConfig {
            design_tokens: Some(RawDesignTokens(tokens(&[("theme", "neon")]))),
            ..RawConfig::default()
        };
        let mut errors = Vec::new();
        validate_theme(&raw, &mut errors);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ConfigErrorCode::UnknownTheme);
        assert_eq!(errors[0].field_path, "design_tokens.theme");
        for name in builtin_theme_names() {
            assert!(errors[0].hint.contains(name), "{}", errors[0].hint);
        }
        // Startup resolution still succeeds on the default.
        let resolved = resolve_config_tokens(&raw.design_tokens.unwrap().0);
        assert_eq!(
            resolved["color.surface"],
            builtin_theme(DEFAULT_THEME).unwrap()["color.surface"]
        );
    }

    #[test]
    fn known_or_absent_theme_validates() {
        let mut errors = Vec::new();
        validate_theme(&RawConfig::default(), &mut errors);
        for name in builtin_theme_names() {
            let raw = RawConfig {
                design_tokens: Some(RawDesignTokens(tokens(&[("theme", name)]))),
                ..RawConfig::default()
            };
            validate_theme(&raw, &mut errors);
        }
        assert!(errors.is_empty(), "{errors:?}");
    }
}
