//! `[displays.<NAME>]`: place zones on a non-primary monitor.
//!
//! ```toml
//! [displays.DISPLAY6]
//! zones = ["notification-area"]
//! ```
//!
//! The HUD overlays every connected monitor. Zones render on the primary
//! unless a `[displays.<NAME>]` table lists them; `<NAME>` is the OS display
//! name (`DISPLAY6`, with or without the `\\.\` prefix, any case). A zone
//! assigned to a display that is not connected renders on the primary.

use std::collections::{HashMap, HashSet};

use tze_hud_scene::config::{ConfigError, ConfigErrorCode};
use tze_hud_scene::types::ZoneRegistry;

use crate::raw::RawConfig;

/// Zone name → configured display name, for every `[displays]` assignment.
pub fn zone_display_assignments(raw: &RawConfig) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (display, table) in raw.displays.iter().flatten() {
        for zone in &table.zones {
            out.insert(zone.clone(), display.clone());
        }
    }
    out
}

/// Validate `[displays]`: zones must be known zone instances and each zone may
/// be assigned to at most one display.
pub fn validate_displays(raw: &RawConfig, errors: &mut Vec<ConfigError>) {
    let Some(displays) = &raw.displays else {
        return;
    };
    let known = ZoneRegistry::with_defaults();
    let mut known_names: Vec<&str> = known.zones.keys().map(String::as_str).collect();
    known_names.sort_unstable();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut names: Vec<&String> = displays.keys().collect();
    names.sort();
    for display in names {
        for zone in &displays[display].zones {
            let field_path = format!("displays.{display}.zones");
            if !known.zones.contains_key(zone) {
                errors.push(ConfigError {
                    code: ConfigErrorCode::Other("CONFIG_UNKNOWN_DISPLAY_ZONE".into()),
                    field_path,
                    expected: format!("one of: {}", known_names.join(", ")),
                    got: format!("{zone:?}"),
                    hint: "list zone names as agents publish to them".into(),
                });
            } else if !seen.insert(zone.as_str()) {
                errors.push(ConfigError {
                    code: ConfigErrorCode::Other("CONFIG_DUPLICATE_DISPLAY_ZONE".into()),
                    field_path,
                    expected: "each zone under at most one [displays.<NAME>]".into(),
                    got: format!("{zone:?} listed twice"),
                    hint: format!("keep {zone:?} under a single display"),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors_for(src: &str) -> Vec<ConfigError> {
        let raw: RawConfig = toml::from_str(src).expect("parses");
        let mut errors = Vec::new();
        validate_displays(&raw, &mut errors);
        errors
    }

    #[test]
    fn assigns_known_zones_to_displays() {
        let raw: RawConfig = toml::from_str(
            "[displays.DISPLAY6]\nzones = [\"notification-area\", \"subtitle\"]\n",
        )
        .unwrap();
        let map = zone_display_assignments(&raw);
        assert_eq!(map.get("subtitle").map(String::as_str), Some("DISPLAY6"));
        assert_eq!(map.len(), 2);
        assert!(errors_for("[displays.DISPLAY6]\nzones = [\"subtitle\"]\n").is_empty());
    }

    #[test]
    fn rejects_unknown_and_doubly_assigned_zones() {
        let errors = errors_for(
            "[displays.DISPLAY6]\nzones = [\"nope\", \"pip\"]\n\
             [displays.DISPLAY8]\nzones = [\"pip\"]\n",
        );
        let codes: Vec<String> = errors.iter().map(|e| format!("{:?}", e.code)).collect();
        assert_eq!(errors.len(), 2, "{codes:?}");
        assert!(codes[0].contains("CONFIG_UNKNOWN_DISPLAY_ZONE"));
        assert!(codes[1].contains("CONFIG_DUPLICATE_DISPLAY_ZONE"));
    }

    #[test]
    fn absent_section_is_valid() {
        assert!(errors_for("").is_empty());
        assert!(zone_display_assignments(&RawConfig::default()).is_empty());
    }
}
