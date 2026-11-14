//! `[displays."<key>"]`: place zones on a non-primary monitor.
//!
//! ```toml
//! [displays."DEL41A2-7XK2N13"]
//! zones = ["notification-area"]
//! ```
//!
//! The HUD overlays every connected monitor. Zones render on the primary
//! unless a `[displays."<key>"]` table lists them. `<key>` is the monitor's
//! EDID model+serial key that `/admin/status` and the log show, any case;
//! two monitors with the same model+serial carry an `@<connector>` suffix. A
//! zone whose key matches no connected monitor renders on the primary.

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

/// Canonical form of a configured or reported display key: trimmed, any
/// `\\.\` device-namespace prefix dropped, uppercase.
pub fn normalize_display_key(key: &str) -> String {
    key.trim().trim_start_matches(r"\\.\").to_ascii_uppercase()
}

/// Index of the connected display a configured key names. `connected` holds
/// each display's key and, when it has an EDID identity, its model+serial
/// base. The key matches case-insensitively; a key without an `@<connector>`
/// suffix also matches a base exactly one connected display has.
pub fn match_display_key(configured: &str, connected: &[(&str, Option<&str>)]) -> Option<usize> {
    let wanted = normalize_display_key(configured);
    if let Some(i) = connected
        .iter()
        .position(|(key, _)| normalize_display_key(key) == wanted)
    {
        return Some(i);
    }
    if wanted.contains('@') {
        return None;
    }
    let mut by_base = connected
        .iter()
        .enumerate()
        .filter(|(_, (_, base))| base.is_some_and(|b| b.eq_ignore_ascii_case(&wanted)))
        .map(|(i, _)| i);
    match (by_base.next(), by_base.next()) {
        (Some(i), None) => Some(i),
        _ => None,
    }
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
                    expected: "each zone under at most one [displays.<key>]".into(),
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
        let raw: RawConfig =
            toml::from_str("[displays.DISPLAY6]\nzones = [\"notification-area\", \"subtitle\"]\n")
                .unwrap();
        let map = zone_display_assignments(&raw);
        assert_eq!(map.get("subtitle").map(String::as_str), Some("DISPLAY6"));
        assert_eq!(map.len(), 2);
        assert!(errors_for("[displays.DISPLAY6]\nzones = [\"subtitle\"]\n").is_empty());

        // Quoted connector-suffixed keys parse verbatim.
        let key = "DEL41A2@DISPLAY/DEL41A2/5&1A2B3C&0&UID4352";
        let raw: RawConfig =
            toml::from_str(&format!("[displays.\"{key}\"]\nzones = [\"pip\"]\n")).unwrap();
        assert_eq!(
            zone_display_assignments(&raw)
                .get("pip")
                .map(String::as_str),
            Some(key)
        );

        // Matching: exact key (any case), or an unsuffixed base that exactly
        // one connected display has; otherwise unmatched.
        let connected = [
            ("DISPLAY1", None),
            ("DEL41A2-7XK2N13", Some("DEL41A2-7XK2N13")),
            ("DEL41A2@DISPLAY/DEL41A2/1&0&UID1", Some("DEL41A2")),
            ("DEL41A2@DISPLAY/DEL41A2/1&0&UID2", Some("DEL41A2")),
            ("GSM5B7F@DISPLAY/GSM5B7F/1&0&UID3", Some("GSM5B7F")),
        ];
        for (configured, want) in [
            ("del41a2-7xk2n13", Some(1)),
            (r"\\.\display1", Some(0)),
            ("DEL41A2@display/del41a2/1&0&uid2", Some(3)),
            ("GSM5B7F", Some(4)),
            ("DEL41A2", None),
            ("DEL41A2@DISPLAY/DEL41A2/1&0&UID9", None),
            ("ACI27A1-NOPE", None),
        ] {
            assert_eq!(
                match_display_key(configured, &connected),
                want,
                "{configured}"
            );
        }
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
