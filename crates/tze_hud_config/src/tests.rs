//! Integration tests for `TzeHudConfig`.
//!
//! Each test corresponds to a WHEN/THEN scenario from the issue acceptance
//! criteria (rig-j90m, rig-umgy) and `configuration/spec.md`.

use crate::loader::TzeHudConfig;
use tze_hud_scene::config::{ConfigErrorCode, ConfigLoader, ParseError};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn parse_ok(toml: &str) -> TzeHudConfig {
    TzeHudConfig::parse(toml).expect("parse should succeed for this TOML")
}

// ── Spec §TOML Configuration Format ──────────────────────────────────────────

/// WHEN valid TOML provided THEN parse succeeds.
#[test]
fn spec_valid_toml_accepted() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#;
    let result = TzeHudConfig::parse(toml);
    assert!(result.is_ok(), "valid TOML should be accepted");
}

/// WHEN invalid TOML THEN parse error includes line and column.
#[test]
fn spec_parse_error_includes_line_column() {
    let bad_toml = "this is not = valid toml [\n";
    let result = TzeHudConfig::parse(bad_toml);
    match result {
        Err(ParseError { line, column, .. }) => {
            assert!(line >= 1, "line should be >= 1, got {line}");
            assert!(column >= 1, "column should be >= 1, got {column}");
        }
        Ok(_) => panic!("invalid TOML should have failed to parse"),
    }
}

// ── Spec §Configuration File Resolution Order ─────────────────────────────────

/// WHEN no config file found at any location THEN Err lists searched paths.
#[test]
fn spec_no_config_found_lists_searched_paths() {
    let result =
        TzeHudConfig::resolve_config_path(Some("/tmp/tze_hud_no_such_file_j90m_test.toml"));
    match result {
        Err(paths) => {
            assert!(!paths.is_empty(), "searched paths must be listed");
        }
        Ok(_) => panic!("should not have found a non-existent file"),
    }
}

// ── Spec §Minimal Valid Configuration ─────────────────────────────────────────

/// WHEN minimal config (runtime + one tab) THEN freeze succeeds.
#[test]
fn spec_minimal_config_accepted() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Home"
"#;
    let loader = parse_ok(toml);
    let resolved = loader.freeze();
    assert!(
        resolved.is_ok(),
        "minimal config should freeze successfully"
    );
    let config = resolved.unwrap();
    assert_eq!(config.tab_names, vec!["Home".to_string()]);
}

/// WHEN config has [runtime] but no [[tabs]] THEN CONFIG_NO_TABS.
#[test]
fn spec_missing_tabs_rejected_with_config_no_tabs() {
    let toml = r#"
[runtime]
profile = "full-display"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let has_no_tabs = errors
        .iter()
        .any(|e| matches!(e.code, ConfigErrorCode::NoTabs));
    assert!(has_no_tabs, "no [[tabs]] should produce CONFIG_NO_TABS");
}

// ── Spec §Layered Config Composition (v1-reserved) ────────────────────────────

/// WHEN config has `includes` field THEN startup error (post-v1 reserved).
#[test]
fn spec_includes_field_rejected() {
    let toml = r#"
includes = "/etc/tze_hud/base.toml"

[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let has_includes_error = errors
        .iter()
        .any(|e| matches!(e.code, ConfigErrorCode::ConfigIncludesNotSupported));
    assert!(
        has_includes_error,
        "includes field should produce CONFIG_INCLUDES_NOT_SUPPORTED"
    );
}

// ── Spec §Structured Validation Error Collection ──────────────────────────────

/// WHEN multiple validation errors exist THEN all are reported together.
#[test]
fn spec_multiple_errors_collected() {
    let toml = r#"
[runtime]
profile = "totally_unknown_profile"

[[tabs]]
name = "Dup"

[[tabs]]
name = "Dup"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    // Should have at least: UNKNOWN_PROFILE and DUPLICATE_TAB_NAME.
    assert!(
        errors.len() >= 2,
        "should collect multiple errors, got: {:?}",
        errors.iter().map(|e| &e.code).collect::<Vec<_>>()
    );
}

// ── Spec §Tab Configuration Validation ───────────────────────────────────────

/// WHEN two tabs share name "Morning" THEN CONFIG_DUPLICATE_TAB_NAME.
#[test]
fn spec_duplicate_tab_name_rejected() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Morning"

[[tabs]]
name = "Morning"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::DuplicateTabName)),
        "should have CONFIG_DUPLICATE_TAB_NAME error"
    );
}

/// WHEN two tabs both set default_tab = true THEN CONFIG_MULTIPLE_DEFAULT_TABS.
#[test]
fn spec_multiple_default_tabs_rejected() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "A"
default_tab = true

[[tabs]]
name = "B"
default_tab = true
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::MultipleDefaultTabs)),
        "should have CONFIG_MULTIPLE_DEFAULT_TABS error"
    );
}

// ── Spec §Reserved Fraction Validation ───────────────────────────────────────

// ── Spec §FPS Range Validation ────────────────────────────────────────────────

// ── Spec §Scene Event Naming Convention ──────────────────────────────────────

// ── Spec §Widget Bundle Configuration (CONFIG_WIDGET_* error codes) ──────────
//
// These tests verify that the TOML-level config validator produces the correct
// CONFIG_WIDGET_* error codes for each error case when config is parsed via
// TzeHudConfig::parse/validate.
//
// Source: configuration/spec.md §Widget Bundle Configuration,
//         §Widget Instance Configuration (hud-mim2.7 acceptance criterion 11).

/// WHEN [widget_bundles].paths contains a non-existent directory THEN
/// CONFIG_WIDGET_BUNDLE_PATH_NOT_FOUND is produced.
#[test]
fn spec_widget_bundle_path_not_found_error() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[widget_bundles]
paths = ["/tmp/tze_hud_nonexistent_widget_bundle_dir_mim2_7_test"]
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::WidgetBundlePathNotFound)),
        "non-existent bundle path should produce CONFIG_WIDGET_BUNDLE_PATH_NOT_FOUND, got: {:?}",
        errors.iter().map(|e| &e.code).collect::<Vec<_>>()
    );
}

/// WHEN [widget_bundles] is absent THEN no CONFIG_WIDGET_* errors are produced.
/// An empty registry is a valid configuration.
#[test]
fn spec_absent_widget_bundles_no_error() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let widget_errors: Vec<_> = errors
        .iter()
        .filter(|e| {
            matches!(
                e.code,
                ConfigErrorCode::WidgetBundlePathNotFound
                    | ConfigErrorCode::UnknownWidgetType
                    | ConfigErrorCode::WidgetInvalidInitialParams
            )
        })
        .collect();
    assert!(
        widget_errors.is_empty(),
        "absent [widget_bundles] should produce no widget errors, got: {widget_errors:?}"
    );
}

/// WHEN max_agent_bytes > max_total_bytes in [widget_runtime_assets]
/// THEN CONFIG_WIDGET_ASSET_BUDGET_INVALID is produced.
#[test]
fn spec_widget_runtime_asset_budget_relationship_rejected() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[widget_runtime_assets]
max_total_bytes = 1024
max_agent_bytes = 2048
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors.iter().any(|e| matches!(
            e.code,
            ConfigErrorCode::Other(ref code) if code == "CONFIG_WIDGET_ASSET_BUDGET_INVALID"
        )),
        "expected CONFIG_WIDGET_ASSET_BUDGET_INVALID, got: {:?}",
        errors.iter().map(|e| &e.code).collect::<Vec<_>>()
    );
}

/// WHEN [[tabs.widgets]] widget_type is missing THEN CONFIG_UNKNOWN_WIDGET_TYPE.
///
/// During config-only validation (no loaded types), a missing/empty widget_type
/// field still produces an error because widget_type is required.
#[test]
fn spec_widget_instance_missing_type_produces_error() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[[tabs.widgets]]
# widget_type is intentionally omitted
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    // Missing widget_type produces CONFIG_UNKNOWN_WIDGET_TYPE (empty type name).
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::UnknownWidgetType)),
        "missing widget_type should produce CONFIG_UNKNOWN_WIDGET_TYPE, got: {:?}",
        errors.iter().map(|e| &e.code).collect::<Vec<_>>()
    );
}

// ── Spec §Schema Export ───────────────────────────────────────────────────────

// ── Spec §Display Profile headless - not extendable ──────────────────────────

// ── Spec §Profile Budget Escalation Prevention ────────────────────────────────

// ── Spec §Profile Extends Conflict Detection ─────────────────────────────────

// ── Spec §Display Profile full-display — freeze ───────────────────────────────

/// WHEN profile = "full-display" THEN resolved profile has correct budget values (spec lines 55-56).
#[test]
fn spec_full_display_profile_budget_values() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "T"
"#;
    let loader = parse_ok(toml);
    let resolved = loader.freeze().expect("freeze should succeed");
    assert_eq!(resolved.profile.max_tiles, 1024);
    assert_eq!(resolved.profile.max_texture_mb, 2048);
    assert_eq!(resolved.profile.max_agents, 16);
    assert_eq!(resolved.profile.target_fps, 60);
    assert_eq!(resolved.profile.min_fps, 30);
}

/// WHEN profile = "headless" THEN resolved profile has correct budget values (spec lines 63-65).
#[test]
fn spec_headless_profile_budget_values() {
    let toml = r#"
[runtime]
profile = "headless"

[[tabs]]
name = "T"
"#;
    let loader = parse_ok(toml);
    let resolved = loader.freeze().expect("freeze should succeed");
    assert_eq!(resolved.profile.max_tiles, 256);
    assert_eq!(resolved.profile.max_texture_mb, 512);
    assert_eq!(resolved.profile.max_agents, 8);
    assert_eq!(resolved.profile.max_agent_update_hz, 60);
    assert_eq!(resolved.profile.target_fps, 60);
    assert_eq!(resolved.profile.min_fps, 1);
    assert_eq!(resolved.profile.name, "headless");
}

// ── Spec §Headless Virtual Display ───────────────────────────────────────────

// ── [display_profile] is gone ─────────────────────────────────────────────────

/// WHEN a config still has `[display_profile]` THEN it is rejected with a hint
/// naming the two built-in profiles.
#[test]
fn display_profile_table_is_rejected_with_hint() {
    let toml = r#"
[runtime]
profile = "full-display"

[display_profile]
max_tiles = 512

[[tabs]]
name = "Main"
"#;
    let errors = TzeHudConfig::parse(toml).unwrap().validate();
    let err = errors
        .iter()
        .find(|e| matches!(e.code, ConfigErrorCode::DisplayProfileNotSupported))
        .expect("[display_profile] should be rejected");
    assert_eq!(err.field_path, "display_profile");
    assert!(err.hint.contains("headless"), "{}", err.hint);
}

// ── Agents live in agents.toml ────────────────────────────────────────────────

/// WHEN a config file still has `[agents]` THEN a config error points at pairing.
#[test]
fn agents_table_in_config_is_rejected_with_pairing_hint() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.agent_a]
psk_env = "AGENT_A_PSK"
allow = ["*"]
"#;
    let errors = TzeHudConfig::parse(toml).unwrap().validate();
    let err = errors
        .iter()
        .find(|e| matches!(e.code, ConfigErrorCode::AgentsInConfigFile))
        .expect("[agents] in config.toml should be rejected");
    assert_eq!(err.field_path, "agents");
    assert!(err.hint.contains("pairing"), "{}", err.hint);
}

// ── freeze / ResolvedConfig ───────────────────────────────────────────────────

/// WHEN minimal config frozen THEN tab_names contains the tab.
#[test]
fn spec_freeze_populates_tab_names() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Dashboard"
"#;
    let loader = parse_ok(toml);
    let resolved = loader.freeze().expect("freeze should succeed");
    assert_eq!(resolved.tab_names, vec!["Dashboard".to_string()]);
}

/// WHEN config has validation errors THEN freeze returns Err.
#[test]
fn spec_freeze_returns_err_on_validation_errors() {
    let toml = r#"
[runtime]
profile = "full-display"
"#;
    let loader = parse_ok(toml);
    let result = loader.freeze();
    assert!(result.is_err(), "freeze should fail when there are no tabs");
}

// ── Spec §Zone Registry — per-tab zone-type reference validation ──────────────

/// WHEN tab references a built-in zone type THEN no error.
#[test]
fn spec_builtin_zone_type_accepted() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
zones = ["subtitle", "notification", "status_bar", "pip", "ambient_background", "alert_banner"]
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let zone_errors: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.code, ConfigErrorCode::UnknownZoneType))
        .collect();
    assert!(
        zone_errors.is_empty(),
        "all built-in zone types should be accepted, got errors: {zone_errors:?}"
    );
}

/// WHEN tab references a custom zone type defined in [zones] THEN no error.
#[test]
fn spec_custom_zone_type_defined_in_zones_accepted() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
zones = ["news_ticker"]

[zones.news_ticker]
policy = "latest_wins"
layer = "content"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let zone_errors: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.code, ConfigErrorCode::UnknownZoneType))
        .collect();
    assert!(
        zone_errors.is_empty(),
        "custom zone type defined in [zones] should be accepted, got errors: {zone_errors:?}"
    );
}

/// WHEN tab references a zone type not in [zones] and not built-in THEN CONFIG_UNKNOWN_ZONE_TYPE.
#[test]
fn spec_unknown_zone_type_rejected() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
zones = ["news_ticker"]
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::UnknownZoneType)),
        "unknown zone type should produce CONFIG_UNKNOWN_ZONE_TYPE, got: {:?}",
        errors.iter().map(|e| &e.code).collect::<Vec<_>>()
    );
    // Error should reference the offending zone name.
    let zone_error = errors
        .iter()
        .find(|e| matches!(e.code, ConfigErrorCode::UnknownZoneType))
        .unwrap();
    assert!(
        zone_error.got.contains("news_ticker"),
        "error should identify the unknown zone type, got: {:?}",
        zone_error.got
    );
}

/// WHEN tab has no zones field THEN no zone validation errors.
#[test]
fn spec_tab_without_zones_field_no_error() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let zone_errors: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.code, ConfigErrorCode::UnknownZoneType))
        .collect();
    assert!(
        zone_errors.is_empty(),
        "tab with no zones field should produce no zone errors, got: {zone_errors:?}"
    );
}

// ── Spec §Zone Registry Configuration (rig-mop4) ─────────────────────────────

/// WHEN a tab references zone type "news_ticker" not defined in [zones] and not built-in
/// THEN CONFIG_UNKNOWN_ZONE_TYPE.
///
/// This is tested at the zones module level since tab zone references are
/// validated by the zones module directly.
#[test]
fn spec_unknown_zone_type_produces_error() {
    use crate::zones::validate_zone_type_ref;

    let mut errors = Vec::new();
    validate_zone_type_ref("news_ticker", "tabs[0].zones.news_ticker", &[], &mut errors);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::UnknownZoneType)),
        "unknown zone type should produce CONFIG_UNKNOWN_ZONE_TYPE"
    );
}

/// WHEN a tab defines subtitle = { ... } without custom [zones.subtitle] THEN built-in used.
#[test]
fn spec_builtin_zone_type_subtitle_accepted() {
    use crate::zones::validate_zone_type_ref;

    let mut errors = Vec::new();
    validate_zone_type_ref("subtitle", "tabs[0].zones.subtitle", &[], &mut errors);
    assert!(
        errors.is_empty(),
        "built-in subtitle zone type should be accepted without custom definition"
    );
}

// ── Spec §Configuration Reload (rig-mop4) ────────────────────────────────────

/// WHEN SIGHUP received with a valid config THEN the reload succeeds.
#[test]
fn spec_reload_hot_section_change() {
    use crate::reload::reload_config;

    let new_toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#;
    let result = reload_config(new_toml);
    assert!(result.is_ok(), "valid reload config should succeed");
}

/// WHEN SIGHUP received and updated config has validation errors THEN
/// errors returned and running config unchanged.
#[test]
fn spec_reload_validation_failure_leaves_config_unchanged() {
    use crate::reload::reload_config;

    let bad_toml = r#"
[runtime]
profile = "mobile"

[[tabs]]
name = "Main"
"#;
    let result = reload_config(bad_toml);
    assert!(
        result.is_err(),
        "reload with validation error should return Err"
    );
    let errors = result.unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::UnknownProfile)),
        "should return validation error from reload, got: {errors:?}"
    );
}

// ── Spec §Config Schema Version and Compatibility Policy ──────────────────────

fn has_schema_version_error(errors: &[tze_hud_scene::config::ConfigError]) -> bool {
    errors
        .iter()
        .any(|e| matches!(&e.code, ConfigErrorCode::Other(s) if s == "CONFIG_SCHEMA_VERSION_UNSUPPORTED"))
}

/// WHEN config omits schema_version THEN it is treated as current (no schema error).
#[test]
fn spec_absent_schema_version_defaults_to_current() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        !has_schema_version_error(&errors),
        "absent schema_version must not produce a schema-version error"
    );
}

/// WHEN schema_version exceeds the supported max THEN CONFIG_SCHEMA_VERSION_UNSUPPORTED
/// and the gate short-circuits the remaining field-level validation.
#[test]
fn spec_newer_schema_version_fails_closed() {
    let toml = r#"
schema_version = 999

[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let schema_err = errors
        .iter()
        .find(|e| matches!(&e.code, ConfigErrorCode::Other(s) if s == "CONFIG_SCHEMA_VERSION_UNSUPPORTED"))
        .expect("newer schema_version must fail closed");
    assert!(
        schema_err.expected.contains("supported range")
            || schema_err.hint.contains("supports config schema versions"),
        "schema-version error must name the supported range"
    );
    assert_eq!(
        errors.len(),
        1,
        "unsupported schema version should short-circuit other field validation"
    );
}

/// WHEN schema_version is within the supported range THEN no schema-version error.
#[test]
fn spec_supported_schema_version_proceeds() {
    let toml = r#"
schema_version = 1

[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        !has_schema_version_error(&errors),
        "in-range schema_version must not produce a schema-version error"
    );
}
