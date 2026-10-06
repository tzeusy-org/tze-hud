//! Named themes: swappable, token-only layers over the canonical defaults.
//!
//! A theme is a flat TOML map of canonical design-token keys to values (no
//! layout logic). Built-in themes live in `assets/themes/<name>.toml` and are
//! embedded in the binary. Config selects one with the reserved key
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

use crate::raw::RawConfig;
use crate::tokens::{DesignTokenMap, resolve_tokens, validate_canonical_value};

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

/// Names of the built-in themes, in declaration order.
pub fn builtin_theme_names() -> impl Iterator<Item = &'static str> {
    BUILTIN_THEMES.iter().map(|(name, _)| *name)
}

/// Parse and validate a theme source: a flat map of canonical token keys to
/// string values, each valid for its token kind. Errors list every bad entry.
///
/// Keys are checked against [`crate::tokens::CANONICAL_TOKENS`] rather than
/// the `[design_tokens]` key pattern, so a theme can also set the runtime-only
/// `system_card.*` / `safe_mode.*` tokens that config cannot spell.
pub fn parse_theme(src: &str) -> Result<DesignTokenMap, Vec<String>> {
    let map: DesignTokenMap = toml::from_str(src).map_err(|e| vec![e.to_string()])?;
    let mut errors: Vec<String> = map
        .iter()
        .filter_map(|(key, value)| {
            validate_canonical_value(key, value)
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
            ("color.caution.container", "color.notification.urgency.urgent"),
            ("color.error.container", "color.notification.urgency.critical"),
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
            ("color.on_surface.variant", "color.surface.container.highest"),
            ("color.on_primary", "color.primary"),
            ("color.on_primary.container", "color.primary.container"),
            ("color.on_caution.container", "color.caution.container"),
            ("color.on_error.container", "color.error.container"),
        ] {
            let a = luminance(foreground);
            let b = luminance(background);
            let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(ratio >= 4.5, "Classic {foreground} on {background}: {ratio}");
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
