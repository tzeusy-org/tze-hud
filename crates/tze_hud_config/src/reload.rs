//! Hot-reload support — rig-mop4.
//!
//! Implements spec `configuration/spec.md` requirements:
//!
//! - **Configuration Reload** (lines 263-274, v1-mandatory)
//!   SIGHUP and `RuntimeService.ReloadConfig` gRPC trigger a live reload.
//!   No section is hot-reloadable today; a reload re-validates the whole
//!   config and warns about frozen sections that changed.
//!   Frozen fields (require restart): `[runtime]`, `[[tabs]]`, `[agents]`.
//!   On reload: entire config re-validated; validation errors returned without
//!   applying new config.
//!
//! - `[design_tokens]` is **frozen** — tokens are resolved at startup and baked
//!   into RenderingPolicy fields, SVG templates, and zone/widget registries.
//!   A SIGHUP that detects changes in a frozen section logs a WARN and applies
//!   nothing from it.
//!
//! ## Field Classification
//!
//! | Section                         | Reload behaviour |
//! |---------------------------------|-----------------|
//! | `[runtime]`                     | Frozen (restart required) |
//! | `[[tabs]]`                      | Frozen (restart required) |
//! | `[agents]`                      | Frozen (restart required) |
//! | `[design_tokens]`               | Frozen (restart required) |
//! | `[widget_runtime_assets]`       | Frozen (restart required) |
//!
//! ## Design Note
//!
//! This module provides:
//! 1. `FieldClassification` enum — documents which sections are frozen/hot.
//! 2. `HotReloadableConfig` — the subset of config that survives a SIGHUP.
//! 3. `reload_config` — parses, validates, and returns a new `HotReloadableConfig`
//!    or the validation errors. Never mutates state; the caller applies the result.
//! 4. `SighupHandler` — a thin wrapper around the UNIX `SIGHUP` signal with a
//!    callback mechanism. On non-Unix targets this is a no-op stub.
//!
//! The actual state storage and application live outside this module (typically
//! in the runtime or a `Config` actor), because those involve async coordination
//! and are out of scope for the pure validation layer.

use tze_hud_scene::config::{ConfigError, ConfigErrorCode};

use crate::loader::TzeHudConfig;

// ─── Field classification ──────────────────────────────────────────────────────

/// Whether a configuration section can be reloaded without a restart.
///
/// From spec §Configuration Reload (lines 263-264).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldClassification {
    /// Field is frozen at startup; changes require a full restart.
    Frozen,
}

/// Returns the reload classification for a top-level configuration section.
///
/// `section_path` is the dotted section name (e.g., `"runtime"`, `"agents"`).
/// Every section is frozen at startup today.
pub fn section_classification(_section_path: &str) -> FieldClassification {
    FieldClassification::Frozen
}

/// The names of config sections that are frozen for startup (require restart).
///
/// Includes the component-shape-language sections added by hud-sc0a.7.
pub const FROZEN_SECTIONS: &[&str] = &[
    "runtime",
    "tabs",
    "agents",
    "display_profile",
    "includes",
    "design_tokens",
    "widget_runtime_assets",
];

/// Check whether the new TOML introduces changes to frozen sections and emit
/// `tracing::warn!` for each frozen section that has changed.
///
/// This function compares the serialized form of each frozen section between
/// the currently-running config and the new parsed config. When a frozen section
/// differs, a WARN is emitted stating that a restart is required.
///
/// # Arguments
///
/// * `current_raw` — the raw config currently in use (from startup).
/// * `new_raw` — the newly parsed raw config (from the reload TOML).
///
/// Returns `true` if any frozen section changed (informational; callers may use
/// this to decide whether to log a summary).
pub fn check_frozen_section_changes(
    current_raw: &crate::raw::RawConfig,
    new_raw: &crate::raw::RawConfig,
) -> bool {
    // Compare serialized representations of the frozen sections.
    // We use serde_json::Value for order-independent structural comparison so
    // that HashMap fields (design_tokens, agents)
    // do not produce false-positive warnings due to non-deterministic map
    // iteration order in Rust's HashMap.
    let mut any_changed = false;

    macro_rules! check_frozen_field {
        ($field:ident, $section_name:expr) => {
            let current_val = serde_json::to_value(&current_raw.$field).ok();
            let new_val = serde_json::to_value(&new_raw.$field).ok();
            if current_val != new_val {
                tracing::warn!(
                    section = $section_name,
                    "SIGHUP detected change in frozen config section '{}'; \
                     a restart is required for this change to take effect",
                    $section_name
                );
                any_changed = true;
            }
        };
    }

    check_frozen_field!(runtime, "runtime");
    check_frozen_field!(tabs, "tabs");
    check_frozen_field!(display_profile, "display_profile");
    check_frozen_field!(design_tokens, "design_tokens");
    check_frozen_field!(widget_runtime_assets, "widget_runtime_assets");
    check_frozen_field!(agents, "agents");

    any_changed
}

// ─── Hot-reloadable config subset ─────────────────────────────────────────────

/// The subset of configuration that can be updated via a live reload.
///
/// Produced by `reload_config` when the new TOML is valid.
/// The caller is responsible for atomically applying this to the running state.
///
/// The `Default` implementation produces an all-`None` config (no policy overrides),
/// suitable as the initial state before the first SIGHUP or `ReloadConfig` call.
///
/// Empty today: every section is frozen. A successful reload still proves the
/// new file validates.
#[derive(Clone, Debug, Default)]
pub struct HotReloadableConfig {}

// ─── Reload entry point ────────────────────────────────────────────────────────

/// Parse and validate a new TOML configuration string for a live reload.
///
/// On success, returns the `HotReloadableConfig` extracted from the new TOML.
/// On failure (parse error or validation errors), returns `Err` with the errors.
///
/// The running configuration is NEVER modified by this function — the caller
/// applies the result only if `Ok(...)` is returned.
///
/// Per spec §Configuration Reload:
/// > On reload, the runtime MUST re-validate the entire config; validation errors
/// > MUST be returned without applying the new config.
pub fn reload_config(new_toml: &str) -> Result<HotReloadableConfig, Vec<ConfigError>> {
    use tze_hud_scene::config::ConfigLoader;

    // Step 1: parse TOML.
    let loader = TzeHudConfig::parse(new_toml).map_err(|parse_err| {
        vec![ConfigError {
            code: ConfigErrorCode::ParseError,
            field_path: String::new(),
            expected: "valid TOML".into(),
            got: parse_err.message.clone(),
            hint: format!(
                "fix the TOML syntax error at line {}, column {}",
                parse_err.line, parse_err.column
            ),
        }]
    })?;

    // Step 2: validate the entire config.
    let errors = loader.validate();
    if !errors.is_empty() {
        return Err(errors);
    }

    // Step 3: extract the hot-reloadable subset.
    Ok(HotReloadableConfig {})
}

// ─── SIGHUP handler ───────────────────────────────────────────────────────────

/// A callback invoked when SIGHUP is received.
///
/// The callback receives the path to the config file to reload.
/// It is responsible for calling `reload_config` and applying the result.
pub type SighupCallback = Box<dyn Fn(&str) + Send + Sync>;

/// SIGHUP reload coordinator.
///
/// This struct **does not** install a real OS signal handler. It stores the
/// config file path and exposes `trigger_reload()` for programmatic invocation.
///
/// Production runtimes MUST integrate with `tokio::signal::unix::signal(SignalKind::hangup())`
/// to receive OS SIGHUP signals asynchronously. When the signal fires, call
/// `trigger_reload()` on this struct from the signal handling task.
///
/// The separation keeps signal delivery (async runtime concern) separate from
/// config parsing/validation (pure logic concern), making both independently
/// testable.
pub struct SighupHandler {
    config_path: String,
}

impl SighupHandler {
    /// Create a new SIGHUP handler for the given config file path.
    pub fn new(config_path: impl Into<String>) -> Self {
        SighupHandler {
            config_path: config_path.into(),
        }
    }

    /// Returns the config path registered with this handler.
    pub fn config_path(&self) -> &str {
        &self.config_path
    }

    /// Simulate a SIGHUP reload by loading the config from `config_path` and
    /// calling `callback` with the new `HotReloadableConfig`.
    ///
    /// Returns `Ok(())` if reload succeeded, `Err(errors)` if validation failed.
    ///
    /// In tests, call this directly instead of sending a real SIGHUP.
    pub fn trigger_reload(
        &self,
        on_success: impl FnOnce(HotReloadableConfig),
    ) -> Result<(), Vec<ConfigError>> {
        let toml_src = std::fs::read_to_string(&self.config_path).map_err(|io_err| {
            vec![ConfigError {
                code: ConfigErrorCode::Other("CONFIG_RELOAD_IO_ERROR".into()),
                field_path: "config_path".into(),
                expected: "readable config file".into(),
                got: format!("{io_err}"),
                hint: format!("ensure {:?} exists and is readable", self.config_path),
            }]
        })?;

        let hot = reload_config(&toml_src)?;
        on_success(hot);
        Ok(())
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    trait IntoRaw {
        fn into_raw(self) -> crate::raw::RawConfig;
    }

    impl IntoRaw for TzeHudConfig {
        fn into_raw(self) -> crate::raw::RawConfig {
            self.raw
        }
    }

    use super::*;
    use tze_hud_scene::config::ConfigLoader;

    fn minimal_valid_toml() -> &'static str {
        r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#
    }

    // ── Field classification ──────────────────────────────────────────────────

    #[test]
    fn test_frozen_sections_classified_correctly() {
        assert_eq!(
            section_classification("runtime"),
            FieldClassification::Frozen
        );
        assert_eq!(section_classification("tabs"), FieldClassification::Frozen);
        assert_eq!(
            section_classification("agents.registered"),
            FieldClassification::Frozen
        );
        assert_eq!(
            section_classification("display_profile"),
            FieldClassification::Frozen
        );
        assert_eq!(
            section_classification("includes"),
            FieldClassification::Frozen
        );
        assert_eq!(
            section_classification("design_tokens"),
            FieldClassification::Frozen,
            "[design_tokens] must be frozen (requires restart)"
        );
        assert_eq!(
            section_classification("widget_runtime_assets"),
            FieldClassification::Frozen,
            "[widget_runtime_assets] must be frozen (requires restart)"
        );
    }

    // ── reload_config ─────────────────────────────────────────────────────────

    #[test]
    fn test_reload_config_valid_toml_returns_hot_config() {
        let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.claude]
allow = ["*"]
"#;
        let result = reload_config(toml);
        assert!(
            result.is_ok(),
            "valid config should reload successfully, got: {result:?}"
        );
    }

    #[test]
    fn test_reload_config_invalid_toml_returns_errors() {
        let bad_toml = "this is not valid toml [\n";
        let result = reload_config(bad_toml);
        assert!(result.is_err(), "invalid TOML should return errors");
        let errors = result.unwrap_err();
        assert!(!errors.is_empty());
        assert!(matches!(errors[0].code, ConfigErrorCode::ParseError));
    }

    #[test]
    fn test_reload_config_validation_error_returns_errors_without_applying() {
        // Spec scenario: SIGHUP with validation errors → errors returned, running
        // config unchanged.
        let bad_config = r#"
[runtime]
profile = "custom"

[display_profile]
extends = "full-display"
target_fps = 15
min_fps = 30

[[tabs]]
name = "Tab1"
"#;
        let result = reload_config(bad_config);
        assert!(
            result.is_err(),
            "validation error should be returned on reload"
        );
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::InvalidFpsRange)),
            "should return CONFIG_INVALID_FPS_RANGE, got: {errors:?}"
        );
    }

    // ── SighupHandler ─────────────────────────────────────────────────────────

    #[test]
    fn test_sighup_handler_config_path() {
        let handler = SighupHandler::new("/etc/tze_hud/config.toml");
        assert_eq!(handler.config_path(), "/etc/tze_hud/config.toml");
    }

    #[test]
    fn test_sighup_handler_trigger_reload_missing_file_returns_error() {
        let handler = SighupHandler::new("/tmp/tze_hud_no_such_file_reload_mop4.toml");
        let result = handler.trigger_reload(|_hot| {
            panic!("callback should not be called on missing file");
        });
        assert!(result.is_err(), "missing file should produce IO error");
        let errors = result.unwrap_err();
        assert!(!errors.is_empty());
        assert!(
            matches!(errors[0].code, ConfigErrorCode::Other(_)),
            "expected IO error code, got: {:?}",
            errors[0].code
        );
    }

    #[test]
    fn test_into_raw_exposes_raw_config() {
        let toml = minimal_valid_toml();
        let loader = TzeHudConfig::parse(toml).unwrap();
        let raw = loader.into_raw();
        assert!(raw.runtime.is_some(), "runtime should be present");
        assert!(!raw.tabs.is_empty(), "tabs should be present");
    }

    // ── FROZEN_SECTIONS list ──────────────────────────────────────────────────

    /// Verify the FROZEN_SECTIONS list includes the three component-shape-language sections.
    #[test]
    fn test_frozen_sections_list_includes_token_and_asset_sections() {
        assert!(
            FROZEN_SECTIONS.contains(&"design_tokens"),
            "design_tokens must be in FROZEN_SECTIONS"
        );
        assert!(
            FROZEN_SECTIONS.contains(&"widget_runtime_assets"),
            "widget_runtime_assets must be in FROZEN_SECTIONS"
        );
    }

    // ── check_frozen_section_changes ──────────────────────────────────────────

    /// When no changes to frozen sections, returns false.
    #[test]
    fn test_check_frozen_section_changes_no_changes_returns_false() {
        let raw = TzeHudConfig::parse(minimal_valid_toml())
            .unwrap()
            .into_raw();
        let changed = check_frozen_section_changes(&raw, &raw.clone());
        assert!(!changed, "no changes should return false");
    }

    /// When design_tokens section changes, returns true.
    #[test]
    fn test_check_frozen_section_changes_design_tokens_changed() {
        use crate::raw::RawDesignTokens;
        let current = TzeHudConfig::parse(minimal_valid_toml())
            .unwrap()
            .into_raw();
        let mut new_raw = current.clone();
        let mut tokens = std::collections::HashMap::new();
        tokens.insert("color.text.primary".to_string(), "#FF0000".to_string());
        new_raw.design_tokens = Some(RawDesignTokens(tokens));
        let changed = check_frozen_section_changes(&current, &new_raw);
        assert!(
            changed,
            "changed design_tokens should return true and emit WARN"
        );
    }
}
