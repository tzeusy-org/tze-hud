//! `TzeHudConfig` — concrete implementation of `ConfigLoader`.
//!
//! Validation collects every error before reporting. Rejected-by-design keys
//! (`includes`, `[agents]`, `[display_profile]`) fail with a hint.

use std::collections::HashMap;

use tze_hud_scene::config::{
    ConfigError, ConfigErrorCode, ConfigLoader, DisplayProfile, ParseError, ResolvedConfig,
};

use crate::raw::RawConfig;
use crate::resolver;
use crate::runtime_widget_assets;
use crate::tokens;
use crate::widgets;
use crate::zones;

// ─── Config schema version ───────────────────────────────────────────────────

/// Current config schema version. An absent `schema_version` field is treated as
/// this value, so existing (unversioned) v1 configs load unchanged.
pub const CURRENT_CONFIG_SCHEMA_VERSION: u32 = 1;

/// Maximum config schema version this runtime can load. A config declaring a
/// `schema_version` greater than this fails closed with
/// `CONFIG_SCHEMA_VERSION_UNSUPPORTED` (configuration spec §Config Schema Version
/// and Compatibility Policy).
pub const MAX_SUPPORTED_CONFIG_SCHEMA_VERSION: u32 = 1;

// ─── TzeHudConfig ─────────────────────────────────────────────────────────────

/// Concrete implementation of `ConfigLoader` for tze_hud.
///
/// Created via `TzeHudConfig::parse(toml_src)`.
pub struct TzeHudConfig {
    pub(crate) raw: RawConfig,
}

impl ConfigLoader for TzeHudConfig {
    // ── parse ─────────────────────────────────────────────────────────────────

    fn parse(toml_src: &str) -> Result<Self, ParseError>
    where
        Self: Sized,
    {
        toml::from_str::<RawConfig>(toml_src)
            .map(|raw| TzeHudConfig { raw })
            .map_err(|e| {
                // toml 0.8 errors have a span that includes line/column.
                let message = e.to_string();

                // Extract line/column from the error message.
                // Format: "... at line N column M"
                let (line, column) = parse_toml_location(&message);
                ParseError {
                    message,
                    line,
                    column,
                }
            })
    }

    // ── normalize ─────────────────────────────────────────────────────────────

    fn normalize(&mut self) {
        // Ensure `runtime` is present (even if empty) so downstream code can
        // rely on `self.raw.runtime.as_ref()` without repeated Option handling.
        if self.raw.runtime.is_none() {
            self.raw.runtime = Some(Default::default());
        }
    }

    // ── validate ──────────────────────────────────────────────────────────────

    fn validate(&self) -> Vec<ConfigError> {
        let mut errors: Vec<ConfigError> = Vec::new();

        // ── (0) schema_version gate (precedes field-level validation) ─────────
        // Spec §Config Schema Version and Compatibility Policy: absent → current
        // (back-compatible default); within range → proceed; newer than the
        // runtime's max supported version → fail closed naming the range. An
        // unsupported schema means we cannot reliably interpret the rest of the
        // document, so this short-circuits the remaining field-level checks.
        let schema_version = self
            .raw
            .schema_version
            .unwrap_or(CURRENT_CONFIG_SCHEMA_VERSION);
        if schema_version > MAX_SUPPORTED_CONFIG_SCHEMA_VERSION {
            errors.push(ConfigError {
                code: ConfigErrorCode::Other("CONFIG_SCHEMA_VERSION_UNSUPPORTED".into()),
                field_path: "schema_version".into(),
                expected: format!(
                    "schema_version within supported range 0..={MAX_SUPPORTED_CONFIG_SCHEMA_VERSION}"
                ),
                got: schema_version.to_string(),
                hint: format!(
                    "this runtime supports config schema versions up to {MAX_SUPPORTED_CONFIG_SCHEMA_VERSION}; upgrade the runtime or lower schema_version"
                ),
            });
            return errors;
        }

        // ── (1) includes field (v1-reserved) ──────────────────────────────────
        if self.raw.includes.is_some() {
            // AnyValue wraps any TOML value; presence alone is the error.
            errors.push(ConfigError {
                code: ConfigErrorCode::ConfigIncludesNotSupported,
                field_path: "includes".into(),
                expected: "field must be absent (layered composition is post-v1)".into(),
                got: "includes field present".into(),
                hint: "remove the `includes` field; layered config is reserved for post-v1".into(),
            });
        }

        // ── (2) [runtime] present and profile set ─────────────────────────────
        // Spec §Minimal Valid Configuration: a minimal valid config MUST have
        // a [runtime] section with a `profile` field (RFC 0006 §2.1, §2.4).
        let profile_str = match self.raw.runtime.as_ref() {
            None => {
                errors.push(ConfigError {
                    code: ConfigErrorCode::Other("CONFIG_MISSING_RUNTIME_SECTION".into()),
                    field_path: "runtime".into(),
                    expected: "[runtime] table must be present".into(),
                    got: "runtime table missing".into(),
                    hint: "add a [runtime] table with a `profile` field, e.g.:\n[runtime]\nprofile = \"full-display\"".into(),
                });
                None
            }
            Some(runtime) => match runtime.profile.as_deref() {
                None => {
                    errors.push(ConfigError {
                        code: ConfigErrorCode::Other("CONFIG_MISSING_RUNTIME_PROFILE".into()),
                        field_path: "runtime.profile".into(),
                        expected: "non-empty profile name (e.g. \"full-display\")".into(),
                        got: "profile field missing".into(),
                        hint: "add `profile = \"full-display\"` (or another valid profile) under [runtime]".into(),
                    });
                    None
                }
                Some(p) => Some(p),
            },
        };

        // Validate profile value if present.
        if let Some(p) = profile_str {
            validate_profile(p, &mut errors);
        }

        // ── (3) [display_profile] is rejected; the built-in profiles are fixed ──
        if self.raw.display_profile.is_some() {
            errors.push(ConfigError {
                code: ConfigErrorCode::DisplayProfileNotSupported,
                field_path: "display_profile".into(),
                expected: "no [display_profile] table".into(),
                got: "[display_profile] present".into(),
                hint: "remove [display_profile]; choose [runtime].profile = \"full-display\" \
                       or \"headless\""
                    .into(),
            });
        }

        // ── (4) [[tabs]] — at least one, names unique, ≤1 default ────────────
        validate_tabs(&self.raw, &mut errors);

        // ── (4b) Per-tab zone-type reference validation ───────────────────────
        zones::validate_tab_zone_references(&self.raw, &mut errors);

        // ── (10) Zone registry ────────────────────────────────────────────────
        if let Some(zone_registry) = &self.raw.zones {
            zones::validate_zones(zone_registry, &mut errors);
        }

        // ── (11) Agents live in agents.toml, never in the config file ─────────
        if self.raw.agents.is_some() {
            errors.push(ConfigError {
                code: ConfigErrorCode::AgentsInConfigFile,
                field_path: "agents".into(),
                expected: "no [agents] table".into(),
                got: "[agents] present".into(),
                hint: "remove [agents]; agents are added by pairing and stored as PSK hashes \
                       in agents.toml next to this config"
                    .into(),
            });
        }

        // ── (13) Widget bundle path existence validation ───────────────────────
        // We validate bundle path existence and per-tab widget instance references
        // here during config validation. The actual bundle content is validated at
        // runtime startup by the tze_hud_widget crate.
        //
        // During config-only validation (no runtime loaded yet), we have no
        // LoadedWidgetType entries — known_types will be empty and type reference
        // checks are skipped (they will be enforced at runtime startup).
        widgets::validate_widget_bundles(
            &self.raw,
            /*config_parent=*/ None,
            /*loaded_types=*/ &[],
            &mut errors,
        );
        {
            let known = std::collections::HashSet::new();
            let type_map = std::collections::HashMap::new();
            widgets::validate_widget_instances(&self.raw, &known, &type_map, &mut errors);
        }

        // ── (13b) Runtime widget asset durable budget relationship ───────────
        runtime_widget_assets::validate_runtime_widget_asset_budgets(&self.raw, &mut errors);

        // ── (14) Design token key validation ──────────────────────────────────
        tokens::validate_design_tokens(&self.raw, &mut errors);

        errors
    }

    // ── freeze ────────────────────────────────────────────────────────────────

    fn freeze(mut self) -> Result<ResolvedConfig, Vec<ConfigError>> {
        self.normalize();
        let errors = self.validate();
        if !errors.is_empty() {
            return Err(errors);
        }

        // `validate()` already rejected unknown profile names.
        let profile = self
            .raw
            .runtime
            .as_ref()
            .and_then(|r| r.profile.as_deref())
            .and_then(DisplayProfile::builtin)
            .expect("validated config names a built-in profile");

        let tab_names = self
            .raw
            .tabs
            .iter()
            .filter_map(|t| t.name.clone())
            .collect();

        let source_path = None; // Set by caller after file load.

        Ok(ResolvedConfig {
            profile,
            tab_names,
            source_path,
        })
    }

    // ── resolve_config_path ───────────────────────────────────────────────────

    fn resolve_config_path(cli_path: Option<&str>) -> Result<String, Vec<String>>
    where
        Self: Sized,
    {
        resolver::resolve_config_path(cli_path)
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Extract line/column from a toml error message.
///
/// toml 0.8 formats errors like:
/// `TOML parse error at line 2, column 5`
fn parse_toml_location(msg: &str) -> (u32, u32) {
    // Try to parse "at line N, column M" or "at line N column M".
    let mut line = 1u32;
    let mut col = 1u32;

    // Find "line N"
    if let Some(idx) = msg.find("line ") {
        let rest = &msg[idx + 5..];
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = num.parse::<u32>() {
            line = n;
        }
    }

    // Find "column M"
    if let Some(idx) = msg.find("column ") {
        let rest = &msg[idx + 7..];
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = num.parse::<u32>() {
            col = n;
        }
    }

    (line, col)
}

/// Validate the profile string value and append any errors.
fn validate_profile(profile: &str, errors: &mut Vec<ConfigError>) {
    match profile {
        "full-display" | "headless" => {}
        other => {
            errors.push(ConfigError {
                code: ConfigErrorCode::UnknownProfile,
                field_path: "runtime.profile".into(),
                expected: "\"full-display\" or \"headless\"".into(),
                got: format!("{other:?}"),
                hint: format!("unknown profile {other:?}; valid values: full-display, headless"),
            });
        }
    }
}

/// Validate `[[tabs]]` entries.
fn validate_tabs(raw: &RawConfig, errors: &mut Vec<ConfigError>) {
    // Must have at least one tab.
    if raw.tabs.is_empty() {
        errors.push(ConfigError {
            code: ConfigErrorCode::NoTabs,
            field_path: "tabs".into(),
            expected: "at least one [[tabs]] entry".into(),
            got: "empty array".into(),
            hint: "add a [[tabs]] section with at least a `name` field".into(),
        });
        return;
    }

    // Collect names, check uniqueness.
    let mut seen_names: HashMap<String, usize> = HashMap::new();
    let mut default_count = 0usize;

    for (i, tab) in raw.tabs.iter().enumerate() {
        // Name must be present.
        let name = match &tab.name {
            Some(n) => n.clone(),
            None => {
                errors.push(ConfigError {
                    code: ConfigErrorCode::Other("CONFIG_TAB_MISSING_NAME".into()),
                    field_path: format!("tabs[{i}].name"),
                    expected: "non-empty string".into(),
                    got: "absent".into(),
                    hint: "every [[tabs]] entry must have a `name` field".into(),
                });
                continue;
            }
        };

        // Name uniqueness.
        if let Some(prev) = seen_names.get(&name) {
            errors.push(ConfigError {
                code: ConfigErrorCode::DuplicateTabName,
                field_path: format!("tabs[{i}].name"),
                expected: format!("unique name; \"{name}\" already used at tabs[{prev}]"),
                got: format!("{name:?}"),
                hint: format!("rename the second tab (tabs[{i}]) to a unique name"),
            });
        } else {
            seen_names.insert(name, i);
        }

        // Default tab count.
        if tab.default_tab {
            default_count += 1;
            if default_count > 1 {
                errors.push(ConfigError {
                    code: ConfigErrorCode::MultipleDefaultTabs,
                    field_path: format!("tabs[{i}].default_tab"),
                    expected: "at most one tab with default_tab = true".into(),
                    got: format!("tabs[{i}] is the second tab with default_tab = true"),
                    hint: "set default_tab = true on at most one tab".into(),
                });
            }
        }
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod unit_tests {
    use super::*;

    // ── parse ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_parse_error_location_extraction_with_line_col() {
        // A message containing "line 3, column 7"
        let msg = "TOML parse error at line 3, column 7\n  --> details";
        let (l, c) = parse_toml_location(msg);
        assert_eq!(l, 3);
        assert_eq!(c, 7);
    }

    #[test]
    fn test_parse_error_location_fallback_when_not_found() {
        let msg = "some error without location info";
        let (l, c) = parse_toml_location(msg);
        assert_eq!(l, 1, "line should default to 1");
        assert_eq!(c, 1, "column should default to 1");
    }

    // ── profile validation ────────────────────────────────────────────────────

    #[test]
    fn test_validate_profile_mobile_is_unknown() {
        let mut errors = Vec::new();
        validate_profile("mobile", &mut errors);
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0].code, ConfigErrorCode::UnknownProfile));
    }

    #[test]
    fn test_validate_profile_unknown_gives_unknown_error() {
        let mut errors = Vec::new();
        validate_profile("totally_unknown", &mut errors);
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0].code, ConfigErrorCode::UnknownProfile));
    }

    #[test]
    fn test_validate_profile_known_profiles_no_error() {
        for p in &["full-display", "headless"] {
            let mut errors = Vec::new();
            validate_profile(p, &mut errors);
            assert!(errors.is_empty(), "profile {p:?} should not produce errors");
        }
    }

    // ── tab validation ────────────────────────────────────────────────────────

    #[test]
    fn test_no_tabs_produces_no_tabs_error() {
        let raw = RawConfig::default();
        let mut errors = Vec::new();
        validate_tabs(&raw, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::NoTabs))
        );
    }

    #[test]
    fn test_duplicate_tab_name_produces_error() {
        let mut raw = RawConfig::default();
        raw.tabs.push(crate::raw::RawTab {
            name: Some("Home".into()),
            ..Default::default()
        });
        raw.tabs.push(crate::raw::RawTab {
            name: Some("Home".into()),
            ..Default::default()
        });
        let mut errors = Vec::new();
        validate_tabs(&raw, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::DuplicateTabName)),
            "duplicate tab name should produce error"
        );
    }

    #[test]
    fn test_multiple_default_tabs_produces_error() {
        let mut raw = RawConfig::default();
        raw.tabs.push(crate::raw::RawTab {
            name: Some("A".into()),
            default_tab: true,
            ..Default::default()
        });
        raw.tabs.push(crate::raw::RawTab {
            name: Some("B".into()),
            default_tab: true,
            ..Default::default()
        });
        let mut errors = Vec::new();
        validate_tabs(&raw, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::MultipleDefaultTabs)),
            "multiple default_tab=true should produce error"
        );
    }
}
