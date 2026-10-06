//! Zone registry configuration validation — rig-mop4.
//!
//! Implements spec `configuration/spec.md` requirements:
//!
//! - **Zone Registry Configuration** (lines 123-134, v1-mandatory)
//!   Built-in zone types: `subtitle`, `notification`, `status_bar`, `pip`,
//!   `ambient_background`, `alert_banner`. Custom zone types definable via
//!   `[zones]`. Unknown zone types produce `CONFIG_UNKNOWN_ZONE_TYPE`.
//!
//! ## Built-in Zone Types
//!
//! These zone types are always available without an explicit `[zones]` definition:
//! - `subtitle` — bottom-strip subtitle overlay
//! - `notification` — notification tray
//! - `status_bar` — top or bottom status bar
//! - `pip` — picture-in-picture floating overlay
//! - `ambient_background` — full-screen background ambient display
//! - `alert_banner` — urgent alert banner

use tze_hud_scene::config::{ConfigError, ConfigErrorCode};

use crate::raw::{RawConfig, RawZones};

// ─── Built-in zone types ──────────────────────────────────────────────────────

/// Built-in zone types that are always available without `[zones]` definition.
///
/// From spec §Zone Registry Configuration (lines 124-125).
pub const BUILTIN_ZONE_TYPES: &[&str] = &[
    "subtitle",
    "notification",
    "status_bar",
    "pip",
    "ambient_background",
    "alert_banner",
];

// ─── Validation ───────────────────────────────────────────────────────────────

/// Returns `true` if the given zone type name is known (built-in or custom).
///
/// `custom_zone_types` is the set of zone type names defined in `[zones]`.
#[cfg(test)]
fn is_known_zone_type(zone_type: &str, custom_zone_types: &[&str]) -> bool {
    BUILTIN_ZONE_TYPES.contains(&zone_type) || custom_zone_types.contains(&zone_type)
}

/// Returns `true` if `name` is a known v1 built-in zone type.
fn is_builtin_zone_type(name: &str) -> bool {
    BUILTIN_ZONE_TYPES.contains(&name)
}

/// Validate a zone type reference, appending an error if it is unknown.
///
/// `field_path` is the dotted config path to the zone reference (for error reporting).
#[cfg(test)]
pub(crate) fn validate_zone_type_ref(
    zone_type: &str,
    field_path: &str,
    custom_zone_types: &[&str],
    errors: &mut Vec<ConfigError>,
) {
    if !is_known_zone_type(zone_type, custom_zone_types) {
        errors.push(ConfigError {
            code: ConfigErrorCode::UnknownZoneType,
            field_path: field_path.into(),
            expected: format!(
                "a built-in zone type ({}) or a custom zone type defined in [zones]",
                BUILTIN_ZONE_TYPES.join(", ")
            ),
            got: format!("{zone_type:?}"),
            hint: format!(
                "unknown zone type {:?}; add it to [zones] or use a built-in: {}",
                zone_type,
                BUILTIN_ZONE_TYPES.join(", ")
            ),
        });
    }
}

/// Validate the `[zones]` section.
///
/// The zone registry itself (custom type definitions) is always valid as long as
/// zone type names are non-empty strings.  Unknown zone type *references* (from
/// tab zone config) are caught by `validate_tab_zone_references`.
///
/// This function validates the custom zone type definitions have valid keys.
pub fn validate_zones(zones: &RawZones, errors: &mut Vec<ConfigError>) {
    for zone_name in zones.0.keys() {
        if zone_name.is_empty() {
            errors.push(ConfigError {
                code: ConfigErrorCode::UnknownZoneType,
                field_path: "zones".into(),
                expected: "non-empty zone type name".into(),
                got: "empty string".into(),
                hint: "zone type names in [zones] must be non-empty strings".into(),
            });
        }
    }
}

/// Validate per-tab zone-type references in `[[tabs]]`.
///
/// For each tab that lists zone types in its `zones` field:
/// - Built-in zone types are always accepted.
/// - Custom zone types are accepted if they are defined in the `[zones]`
///   section of the config.
/// - Any other name produces a `CONFIG_UNKNOWN_ZONE_TYPE` error.
pub fn validate_tab_zone_references(raw: &RawConfig, errors: &mut Vec<ConfigError>) {
    // Collect custom zone type names defined in [zones].
    let custom_zone_names: std::collections::HashSet<&str> = raw
        .zones
        .as_ref()
        .map(|z| z.0.keys().map(|k| k.as_str()).collect())
        .unwrap_or_default();

    for (i, tab) in raw.tabs.iter().enumerate() {
        for zone_type in &tab.zones {
            if is_builtin_zone_type(zone_type) {
                // Built-in: always valid.
                continue;
            }
            if custom_zone_names.contains(zone_type.as_str()) {
                // Defined in [zones]: valid.
                continue;
            }
            // Unknown zone type.
            errors.push(ConfigError {
                code: ConfigErrorCode::UnknownZoneType,
                field_path: format!("tabs[{i}].zones"),
                expected: format!(
                    "a built-in zone type ({}) or a type defined in [zones]",
                    BUILTIN_ZONE_TYPES.join(", ")
                ),
                got: format!("{zone_type:?}"),
                hint: format!(
                    "unknown zone type {:?}; add it to [zones] or use a built-in: {}",
                    zone_type,
                    BUILTIN_ZONE_TYPES.join(", ")
                ),
            });
        }
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_zone_types_are_known() {
        for zone_type in BUILTIN_ZONE_TYPES {
            assert!(
                is_known_zone_type(zone_type, &[]),
                "built-in zone type {zone_type:?} should be known"
            );
        }
    }

    #[test]
    fn test_unknown_zone_type_produces_error() {
        // Spec scenario: zone type "news_ticker" not in [zones] and not built-in
        // → CONFIG_UNKNOWN_ZONE_TYPE.
        let mut errors = Vec::new();
        validate_zone_type_ref("news_ticker", "tabs[0].zones.news_ticker", &[], &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::UnknownZoneType)),
            "unknown zone type 'news_ticker' should produce CONFIG_UNKNOWN_ZONE_TYPE"
        );
    }

    #[test]
    fn test_custom_zone_type_is_known() {
        let mut errors = Vec::new();
        validate_zone_type_ref(
            "news_ticker",
            "tabs[0].zones.news_ticker",
            &["news_ticker"],
            &mut errors,
        );
        assert!(
            errors.is_empty(),
            "custom zone type should be accepted when defined in [zones]"
        );
    }

    #[test]
    fn test_builtin_subtitle_zone_no_custom_def_needed() {
        // Spec scenario: tab defines subtitle = { policy = "bottom_strip", layer = "content" }
        // without a custom [zones.subtitle] entry → built-in subtitle zone type used.
        let mut errors = Vec::new();
        validate_zone_type_ref("subtitle", "tabs[0].zones.subtitle", &[], &mut errors);
        assert!(
            errors.is_empty(),
            "built-in subtitle zone type should be accepted without custom definition"
        );
    }

    #[test]
    fn test_all_builtin_zone_types_available() {
        // Verify the complete list of built-in zone types per spec.
        let expected = &[
            "subtitle",
            "notification",
            "status_bar",
            "pip",
            "ambient_background",
            "alert_banner",
        ];
        for zt in expected.iter() {
            assert!(
                BUILTIN_ZONE_TYPES.contains(zt),
                "expected built-in zone type {zt:?} to be in BUILTIN_ZONE_TYPES"
            );
        }
        assert_eq!(
            BUILTIN_ZONE_TYPES.len(),
            expected.len(),
            "BUILTIN_ZONE_TYPES count mismatch"
        );
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::raw::{RawConfig, RawTab, RawZoneType, RawZones};
    use std::collections::HashMap;

    fn make_config_with_tab_zones(zone_names: Vec<String>) -> RawConfig {
        let mut raw = RawConfig::default();
        raw.tabs.push(RawTab {
            name: Some("Main".into()),
            zones: zone_names,
            ..Default::default()
        });
        raw
    }

    fn make_config_with_custom_zone(
        tab_zone_names: Vec<String>,
        custom_zones: Vec<&str>,
    ) -> RawConfig {
        let mut raw = make_config_with_tab_zones(tab_zone_names);
        let mut map = HashMap::new();
        for name in custom_zones {
            map.insert(name.to_string(), RawZoneType::default());
        }
        raw.zones = Some(RawZones(map));
        raw
    }

    #[test]
    fn builtin_zone_types_are_accepted() {
        for builtin in BUILTIN_ZONE_TYPES {
            let raw = make_config_with_tab_zones(vec![builtin.to_string()]);
            let mut errors = Vec::new();
            validate_tab_zone_references(&raw, &mut errors);
            assert!(
                errors.is_empty(),
                "built-in zone type {builtin:?} should be accepted, got errors: {errors:?}"
            );
        }
    }

    #[test]
    fn custom_zone_type_defined_in_zones_section_accepted() {
        let raw = make_config_with_custom_zone(vec!["news_ticker".into()], vec!["news_ticker"]);
        let mut errors = Vec::new();
        validate_tab_zone_references(&raw, &mut errors);
        assert!(
            errors.is_empty(),
            "custom zone type defined in [zones] should be accepted, got errors: {errors:?}"
        );
    }

    #[test]
    fn unknown_zone_type_produces_error() {
        let raw = make_config_with_tab_zones(vec!["news_ticker".into()]);
        let mut errors = Vec::new();
        validate_tab_zone_references(&raw, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::UnknownZoneType)),
            "unknown zone type should produce CONFIG_UNKNOWN_ZONE_TYPE, got: {errors:?}"
        );
    }

    #[test]
    fn no_zones_on_tab_produces_no_error() {
        let raw = make_config_with_tab_zones(vec![]);
        let mut errors = Vec::new();
        validate_tab_zone_references(&raw, &mut errors);
        assert!(
            errors.is_empty(),
            "tab with no zones should produce no errors"
        );
    }

    #[test]
    fn mixed_builtin_and_custom_zones_accepted() {
        let raw = make_config_with_custom_zone(
            vec!["subtitle".into(), "news_ticker".into()],
            vec!["news_ticker"],
        );
        let mut errors = Vec::new();
        validate_tab_zone_references(&raw, &mut errors);
        assert!(
            errors.is_empty(),
            "mix of builtin and defined custom zones should be accepted, got errors: {errors:?}"
        );
    }

    #[test]
    fn is_builtin_zone_type_returns_true_for_all_builtins() {
        for builtin in BUILTIN_ZONE_TYPES {
            assert!(
                is_builtin_zone_type(builtin),
                "is_builtin_zone_type({builtin:?}) should return true"
            );
        }
    }

    #[test]
    fn is_builtin_zone_type_returns_false_for_unknown() {
        assert!(!is_builtin_zone_type("news_ticker"));
        assert!(!is_builtin_zone_type("SUBTITLE")); // Case-sensitive.
        assert!(!is_builtin_zone_type(""));
    }
}
