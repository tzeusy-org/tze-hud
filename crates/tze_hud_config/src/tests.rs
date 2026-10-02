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

/// WHEN reserved_top + reserved_bottom >= 1.0 THEN CONFIG_INVALID_RESERVED_FRACTION.
#[test]
fn spec_reserved_fractions_sum_to_one_rejected() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[tabs.layout]
reserved_top_fraction = 0.5
reserved_bottom_fraction = 0.5
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::InvalidReservedFraction)),
        "reserved_top + reserved_bottom = 1.0 should produce CONFIG_INVALID_RESERVED_FRACTION"
    );
}

// ── Spec §FPS Range Validation ────────────────────────────────────────────────

/// WHEN target_fps < min_fps THEN CONFIG_INVALID_FPS_RANGE.
#[test]
fn spec_fps_range_target_below_min_rejected() {
    let toml = r#"
[runtime]
profile = "custom"

[display_profile]
extends = "full-display"
target_fps = 15
min_fps = 30

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let has_fps_error = errors
        .iter()
        .any(|e| matches!(e.code, ConfigErrorCode::InvalidFpsRange));
    assert!(
        has_fps_error,
        "target_fps < min_fps should produce CONFIG_INVALID_FPS_RANGE"
    );
}

// ── Spec §Scene Event Naming Convention ──────────────────────────────────────

/// WHEN tab_switch_on_event = "doorbell.ring" THEN accepted.
#[test]
fn spec_valid_event_name_accepted() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
tab_switch_on_event = "doorbell.ring"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let event_errors: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.code, ConfigErrorCode::InvalidEventName))
        .collect();
    assert!(
        event_errors.is_empty(),
        "valid event name should not produce errors"
    );
}

/// WHEN tab_switch_on_event = "Doorbell-Ring" THEN CONFIG_INVALID_EVENT_NAME.
#[test]
fn spec_invalid_event_name_rejected() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
tab_switch_on_event = "Doorbell-Ring"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::InvalidEventName)),
        "invalid event name should produce CONFIG_INVALID_EVENT_NAME"
    );
}

/// WHEN tab_switch_on_event = "" THEN accepted with no warning.
#[test]
fn spec_empty_event_name_accepted_no_warning() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
tab_switch_on_event = ""
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let event_errors: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.code, ConfigErrorCode::InvalidEventName))
        .collect();
    assert!(
        event_errors.is_empty(),
        "empty event name must not produce an error"
    );
}

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

/// WHEN schema_value() is called THEN valid JSON Schema returned.
#[test]
fn spec_schema_export_produces_valid_json_schema() {
    let schema = crate::schema::schema_value();
    assert!(schema.is_object(), "schema must be a JSON object");
}

// ── Spec §Display Profile headless - not extendable ──────────────────────────

/// WHEN extends = "headless" THEN CONFIG_HEADLESS_NOT_EXTENDABLE.
#[test]
fn spec_headless_not_extendable() {
    let toml = r#"
[runtime]
profile = "custom"

[display_profile]
extends = "headless"

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::HeadlessNotExtendable)),
        "extends=headless must produce CONFIG_HEADLESS_NOT_EXTENDABLE, got: {:?}",
        errors.iter().map(|e| &e.code).collect::<Vec<_>>()
    );
}

// ── Spec §Profile Budget Escalation Prevention ────────────────────────────────

/// WHEN custom profile extends full-display and sets max_tiles = 2048 THEN
/// CONFIG_PROFILE_BUDGET_ESCALATION (spec lines 107-108).
#[test]
fn spec_budget_escalation_rejected() {
    let toml = r#"
[runtime]
profile = "custom"

[display_profile]
extends = "full-display"
max_tiles = 2048

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::ProfileBudgetEscalation)),
        "max_tiles=2048 exceeding base 1024 should produce BUDGET_ESCALATION, got: {:?}",
        errors.iter().map(|e| &e.code).collect::<Vec<_>>()
    );
    // Error should identify the offending field.
    let budget_error = errors
        .iter()
        .find(|e| matches!(e.code, ConfigErrorCode::ProfileBudgetEscalation))
        .unwrap();
    assert!(
        budget_error.field_path.contains("max_tiles"),
        "error should identify max_tiles field, got: {:?}",
        budget_error.field_path
    );
}

// ── Spec §Profile Extends Conflict Detection ─────────────────────────────────

/// WHEN profile = "full-display" and extends = "headless" THEN
/// CONFIG_PROFILE_EXTENDS_CONFLICTS_WITH_PROFILE (spec lines 120-121).
///
/// headless-not-extendable fires first (both errors are acceptable here per spec).
#[test]
fn spec_extends_conflicts_with_profile() {
    let toml = r#"
[runtime]
profile = "full-display"

[display_profile]
extends = "headless"

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    // Must produce at least one of the two relevant errors.
    let has_conflict = errors.iter().any(|e| {
        matches!(
            e.code,
            ConfigErrorCode::HeadlessNotExtendable
                | ConfigErrorCode::ProfileExtendsConflictsWithProfile
        )
    });
    assert!(
        has_conflict,
        "profile=full-display + extends=headless must produce a conflict/not-extendable error, got: {:?}",
        errors.iter().map(|e| &e.code).collect::<Vec<_>>()
    );
}

/// WHEN profile = "headless" and extends = "full-display" THEN
/// CONFIG_PROFILE_EXTENDS_CONFLICTS_WITH_PROFILE.
#[test]
fn spec_headless_extends_full_display_conflict() {
    let toml = r#"
[runtime]
profile = "headless"

[display_profile]
extends = "full-display"

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::ProfileExtendsConflictsWithProfile)),
        "profile=headless + extends=full-display must produce EXTENDS_CONFLICTS, got: {:?}",
        errors.iter().map(|e| &e.code).collect::<Vec<_>>()
    );
}

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

/// WHEN profile=headless and headless_width=1280, headless_height=720 THEN
/// zone geometry computes against 1280x720 virtual surface (spec lines 282-283).
///
/// The headless dimension values are exercised at the profile module level; this
/// test confirms the loader accepts the config and produces a headless profile.
#[test]
fn spec_headless_virtual_display_dimensions() {
    let toml = r#"
[runtime]
profile = "headless"
headless_width = 1280
headless_height = 720

[[tabs]]
name = "T"
"#;
    let loader = parse_ok(toml);
    let resolved = loader.freeze().expect("freeze should succeed");
    assert_eq!(resolved.profile.name, "headless");
    assert_eq!(
        resolved.profile.max_tiles, 256,
        "headless budget preserved with custom dimensions"
    );
}

// ── Spec §Capability Vocabulary ───────────────────────────────────────────────

/// WHEN non-canonical capability in agent config THEN CONFIG_UNKNOWN_CAPABILITY.
#[test]
fn spec_unknown_capability_rejected() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.agent_a]
capabilities = ["createTiles"]
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let has_cap_error = errors
        .iter()
        .any(|e| matches!(e.code, ConfigErrorCode::UnknownCapability));
    assert!(
        has_cap_error,
        "non-canonical capability 'createTiles' should produce UNKNOWN_CAPABILITY"
    );
}

/// WHEN canonical capabilities ["create_tiles", "publish_zone:subtitle", "lease:priority:1"]
/// THEN accepted (spec scenario lines 155-156).
#[test]
fn spec_valid_canonical_capability_list_accepted() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.agent_a]
capabilities = ["create_tiles", "publish_zone:subtitle", "lease:priority:1"]
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let cap_errors: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.code, ConfigErrorCode::UnknownCapability))
        .collect();
    assert!(
        cap_errors.is_empty(),
        "canonical capability list should produce no errors, got: {cap_errors:?}"
    );
}

/// WHEN createTiles (camelCase) used THEN error hint mentions create_tiles.
#[test]
fn spec_unknown_capability_hint_mentions_canonical_match() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.agent_a]
capabilities = ["createTiles"]
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let cap_error = errors
        .iter()
        .find(|e| matches!(e.code, ConfigErrorCode::UnknownCapability))
        .expect("should have UNKNOWN_CAPABILITY error");
    assert!(
        cap_error.hint.contains("create_tiles"),
        "hint should suggest create_tiles, got: {:?}",
        cap_error.hint
    );
}

/// WHEN legacy name read_scene used THEN CONFIG_UNKNOWN_CAPABILITY with hint for read_scene_topology.
#[test]
fn spec_legacy_read_scene_rejected_with_hint() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.agent_a]
capabilities = ["read_scene"]
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let cap_error = errors
        .iter()
        .find(|e| matches!(e.code, ConfigErrorCode::UnknownCapability))
        .expect("should have UNKNOWN_CAPABILITY error for legacy read_scene");
    assert!(
        cap_error.hint.contains("read_scene_topology"),
        "hint should point to canonical replacement, got: {:?}",
        cap_error.hint
    );
}

/// WHEN legacy name receive_input used THEN CONFIG_UNKNOWN_CAPABILITY with hint for access_input_events.
#[test]
fn spec_legacy_receive_input_rejected_with_hint() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.agent_a]
capabilities = ["receive_input"]
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let cap_error = errors
        .iter()
        .find(|e| matches!(e.code, ConfigErrorCode::UnknownCapability))
        .expect("should have UNKNOWN_CAPABILITY error for legacy receive_input");
    assert!(
        cap_error.hint.contains("access_input_events"),
        "hint should point to canonical replacement, got: {:?}",
        cap_error.hint
    );
}

/// WHEN legacy name zone_publish used THEN CONFIG_UNKNOWN_CAPABILITY with hint for publish_zone.
#[test]
fn spec_legacy_zone_publish_rejected_with_hint() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.agent_a]
capabilities = ["zone_publish"]
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let cap_error = errors
        .iter()
        .find(|e| matches!(e.code, ConfigErrorCode::UnknownCapability))
        .expect("should have UNKNOWN_CAPABILITY error for legacy zone_publish");
    assert!(
        cap_error.hint.contains("publish_zone"),
        "hint should point to canonical replacement, got: {:?}",
        cap_error.hint
    );
}

/// WHEN all flat canonical capabilities in agent config THEN no errors.
#[test]
fn spec_all_flat_canonical_capabilities_accepted() {
    let caps = [
        "create_tiles",
        "modify_own_tiles",
        "manage_tabs",
        "upload_resource",
        "register_widget_asset",
        "read_scene_topology",
        "access_input_events",
        "read_telemetry",
        "resident_mcp",
    ];
    let cap_list = caps
        .iter()
        .map(|c| format!("{c:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = format!(
        r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.agent_a]
capabilities = [{cap_list}]
"#
    );
    let loader = parse_ok(&toml);
    let errors = loader.validate();
    let cap_errors: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.code, ConfigErrorCode::UnknownCapability))
        .collect();
    assert!(
        cap_errors.is_empty(),
        "all flat canonical capabilities should be accepted, got errors: {cap_errors:?}"
    );
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

/// WHEN custom profile extends full-display with max_tiles=512 THEN resolved has 512.
#[test]
fn spec_custom_profile_override_applied() {
    let toml = r#"
[runtime]
profile = "custom"

[display_profile]
extends = "full-display"
max_tiles = 512

[[tabs]]
name = "T"
"#;
    let loader = parse_ok(toml);
    let resolved = loader.freeze().expect("freeze should succeed");
    assert_eq!(
        resolved.profile.max_tiles, 512,
        "override max_tiles should be applied"
    );
    assert_eq!(resolved.profile.name, "custom");
    // Other values fall back to base.
    assert_eq!(
        resolved.profile.max_texture_mb, 2048,
        "non-overridden fields use base values"
    );
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

// ── Spec §Agent Registration with Per-Agent Budget Overrides (rig-mop4) ───────

/// WHEN agent sets max_tiles = 4 and profile has max_tiles = 1024 THEN accepted.
#[test]
fn spec_agent_budget_within_ceiling_accepted() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.my_agent]
max_tiles = 4
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let budget_errors: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.code, ConfigErrorCode::AgentBudgetExceedsProfile))
        .collect();
    assert!(
        budget_errors.is_empty(),
        "max_tiles=4 within profile ceiling should be accepted, got: {budget_errors:?}"
    );
}

#[test]
fn freeze_retains_registered_agent_budget_overrides() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.my_agent]
max_tiles = 4
max_texture_mb = 32
max_update_hz = 10
"#;

    let resolved = parse_ok(toml).freeze().expect("valid config freezes");
    let overrides = resolved
        .agent_budget_overrides
        .get("my_agent")
        .expect("registered override retained");
    assert_eq!(overrides.max_tiles, Some(4));
    assert_eq!(overrides.max_texture_mb, Some(32));
    assert_eq!(overrides.max_update_hz, Some(10));
}

/// WHEN agent sets max_tiles = 2048 and profile has max_tiles = 1024 THEN
/// CONFIG_AGENT_BUDGET_EXCEEDS_PROFILE identifying agent, field, and ceiling.
#[test]
fn spec_agent_budget_exceeds_ceiling_rejected() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.big_agent]
max_tiles = 2048
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.code, ConfigErrorCode::AgentBudgetExceedsProfile)),
        "max_tiles=2048 exceeding profile ceiling 1024 should produce CONFIG_AGENT_BUDGET_EXCEEDS_PROFILE"
    );
    let err = errors
        .iter()
        .find(|e| matches!(e.code, ConfigErrorCode::AgentBudgetExceedsProfile))
        .unwrap();
    // Must identify the agent.
    assert!(
        err.hint.contains("big_agent"),
        "error hint should identify agent, got: {:?}",
        err.hint
    );
    // Must identify the field.
    assert!(
        err.field_path.contains("max_tiles"),
        "error path should identify field, got: {:?}",
        err.field_path
    );
    // Must identify the ceiling.
    assert!(
        err.expected.contains("1024"),
        "error should identify ceiling 1024, got: {:?}",
        err.expected
    );
}

// ── Spec §Dynamic Agent Policy (rig-mop4) ────────────────────────────────────

/// WHEN no [agents.dynamic_policy] section present THEN connections from
/// unregistered agents rejected (dynamic_agents_allowed = false).
#[test]
fn spec_no_dynamic_policy_dynamic_agents_disabled() {
    use crate::agents::dynamic_agents_allowed;
    use crate::raw::RawAgents;

    let agents = RawAgents::default();
    assert!(
        !dynamic_agents_allowed(&agents),
        "no [agents.dynamic_policy] should mean dynamic agents are disabled"
    );
}

// ── Spec §Authentication Secret Indirection (rig-mop4) ───────────────────────

/// WHEN agent sets auth_psk_env and env var is unset THEN warning logged.
#[test]
fn spec_auth_psk_unset_env_produces_warning() {
    use crate::agents::check_agent_auth_env_vars_with_lookup;
    use crate::raw::{RawAgents, RawRegisteredAgent};
    use std::collections::HashMap;

    let env_var = "SPEC_TEST_AGENT_KEY_UNSET_MOP4_ABC999";

    let mut registered = HashMap::new();
    registered.insert(
        "spec_agent".to_string(),
        RawRegisteredAgent {
            auth_psk_env: Some(env_var.into()),
            ..Default::default()
        },
    );
    let agents = RawAgents {
        registered: Some(registered),
        ..Default::default()
    };
    // Use mock env lookup to avoid unsafe env mutation.
    let mock_lookup = |_var_name: &str| -> Option<String> { None };
    let warnings = check_agent_auth_env_vars_with_lookup(&agents, mock_lookup);
    assert!(
        !warnings.is_empty(),
        "unset auth_psk_env should produce a warning"
    );
    assert_eq!(warnings[0].env_var_name, env_var);
}

// ── Spec §Configuration Reload (rig-mop4) ────────────────────────────────────

/// WHEN SIGHUP received and updated config changes a hot-reloadable section THEN
/// the new value takes effect without restart.
#[test]
fn spec_reload_hot_section_change() {
    use crate::reload::reload_config;

    let new_toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.dynamic_policy]
allow_dynamic_agents = true
"#;
    let result = reload_config(new_toml);
    assert!(result.is_ok(), "valid reload config should succeed");
    let hot = result.unwrap();
    assert_eq!(
        hot.dynamic_policy.as_ref().map(|p| p.allow_dynamic_agents),
        Some(true),
        "reload should apply new degradation threshold"
    );
}

/// WHEN SIGHUP received and updated config has validation errors THEN
/// errors returned and running config unchanged.
#[test]
fn spec_reload_validation_failure_leaves_config_unchanged() {
    use crate::reload::reload_config;

    let bad_toml = r#"
[runtime]
profile = "custom"

[display_profile]
extends = "full-display"
target_fps = 15
min_fps = 30

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
            .any(|e| matches!(e.code, ConfigErrorCode::InvalidFpsRange)),
        "should return validation error from reload, got: {errors:?}"
    );
}

// ── Spec §Agent Registration — max_update_hz ceiling (hud-7sku) ───────────────

/// WHEN agent max_update_hz is within profile ceiling THEN configuration accepted.
#[test]
fn spec_agent_max_update_hz_within_ceiling_accepted() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.agent_a]
max_update_hz = 30
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let has_budget_error = errors
        .iter()
        .any(|e| matches!(e.code, ConfigErrorCode::AgentBudgetExceedsProfile));
    assert!(
        !has_budget_error,
        "max_update_hz=30 within full-display ceiling of 60 should be accepted, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.code, &e.field_path))
            .collect::<Vec<_>>()
    );
}

/// WHEN agent max_update_hz exceeds profile ceiling THEN CONFIG_AGENT_BUDGET_EXCEEDS_PROFILE.
#[test]
fn spec_agent_max_update_hz_exceeds_ceiling_rejected() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.agent_a]
max_update_hz = 120
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let budget_error = errors.iter().find(|e| {
        matches!(e.code, ConfigErrorCode::AgentBudgetExceedsProfile)
            && e.field_path.contains("max_update_hz")
    });
    assert!(
        budget_error.is_some(),
        "max_update_hz=120 exceeding full-display ceiling of 60 should produce \
         CONFIG_AGENT_BUDGET_EXCEEDS_PROFILE, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.code, &e.field_path))
            .collect::<Vec<_>>()
    );
    let err = budget_error.unwrap();
    assert!(
        err.field_path.contains("agent_a"),
        "error field_path should identify the agent name, got: {:?}",
        err.field_path
    );
    assert!(
        err.field_path.contains("max_update_hz"),
        "error field_path should identify the field, got: {:?}",
        err.field_path
    );
}

/// WHEN agent max_update_hz equals profile ceiling THEN accepted (equality is within ceiling).
#[test]
fn spec_agent_max_update_hz_equal_to_ceiling_accepted() {
    let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.registered.agent_a]
max_update_hz = 60
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let has_budget_error = errors
        .iter()
        .any(|e| matches!(e.code, ConfigErrorCode::AgentBudgetExceedsProfile));
    assert!(
        !has_budget_error,
        "max_update_hz=60 equal to full-display ceiling of 60 should be accepted, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.code, &e.field_path))
            .collect::<Vec<_>>()
    );
}

/// WHEN agent max_update_hz equals the headless profile ceiling THEN accepted.
#[test]
fn spec_agent_max_update_hz_equal_to_headless_ceiling_accepted() {
    let toml = r#"
[runtime]
profile = "headless"

[[tabs]]
name = "Main"

[agents.registered.ci_agent]
max_update_hz = 60
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    assert!(
        errors.is_empty(),
        "max_update_hz=60 equal to headless ceiling should validate cleanly, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.code, &e.field_path))
            .collect::<Vec<_>>()
    );
}

/// WHEN agent max_update_hz exceeds headless profile ceiling THEN rejected.
#[test]
fn spec_agent_max_update_hz_exceeds_headless_ceiling_rejected() {
    let toml = r#"
[runtime]
profile = "headless"

[[tabs]]
name = "Main"

[agents.registered.ci_agent]
max_update_hz = 61
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let has_budget_error = errors.iter().any(|e| {
        matches!(e.code, ConfigErrorCode::AgentBudgetExceedsProfile)
            && e.field_path.contains("max_update_hz")
    });
    assert!(
        has_budget_error,
        "max_update_hz=61 exceeding headless ceiling of 60 should produce \
         CONFIG_AGENT_BUDGET_EXCEEDS_PROFILE"
    );
}

/// WHEN custom profile sets max_agent_update_hz above base THEN CONFIG_PROFILE_BUDGET_ESCALATION.
#[test]
fn spec_profile_budget_escalation_max_agent_update_hz_rejected() {
    let toml = r#"
[runtime]
profile = "custom"

[display_profile]
extends = "full-display"
max_agent_update_hz = 120

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let has_escalation = errors.iter().any(|e| {
        matches!(e.code, ConfigErrorCode::ProfileBudgetEscalation)
            && e.field_path.contains("max_agent_update_hz")
    });
    assert!(
        has_escalation,
        "max_agent_update_hz=120 exceeding full-display base of 60 should produce \
         CONFIG_PROFILE_BUDGET_ESCALATION, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.code, &e.field_path))
            .collect::<Vec<_>>()
    );
}

/// WHEN custom profile overrides max_agent_update_hz within ceiling THEN accepted and resolved.
#[test]
fn spec_custom_profile_max_agent_update_hz_override_applied() {
    let toml = r#"
[runtime]
profile = "custom"

[display_profile]
extends = "full-display"
max_agent_update_hz = 30

[[tabs]]
name = "Main"
"#;
    let loader = parse_ok(toml);
    let resolved = loader.freeze().expect("freeze should succeed");
    assert_eq!(
        resolved.profile.max_agent_update_hz, 30,
        "overridden max_agent_update_hz should be 30"
    );
    assert_eq!(
        resolved.profile.max_tiles,
        tze_hud_scene::config::DisplayProfile::full_display().max_tiles,
        "non-overridden fields use base values"
    );
}

/// WHEN custom profile lowers max_agent_update_hz to 30 and agent sets max_update_hz=45
/// THEN CONFIG_AGENT_BUDGET_EXCEEDS_PROFILE (agent exceeds the tightened custom ceiling).
///
/// Regression test: profile_ceiling_for_validation must apply custom overrides, not just
/// use the base profile ceiling.
#[test]
fn spec_agent_max_update_hz_exceeds_custom_tightened_ceiling_rejected() {
    let toml = r#"
[runtime]
profile = "custom"

[display_profile]
extends = "full-display"
max_agent_update_hz = 30

[[tabs]]
name = "Main"

[agents.registered.fast_agent]
max_update_hz = 45
"#;
    let loader = parse_ok(toml);
    let errors = loader.validate();
    let budget_error = errors.iter().find(|e| {
        matches!(e.code, ConfigErrorCode::AgentBudgetExceedsProfile)
            && e.field_path.contains("max_update_hz")
    });
    assert!(
        budget_error.is_some(),
        "max_update_hz=45 exceeding custom ceiling of 30 should produce \
         CONFIG_AGENT_BUDGET_EXCEEDS_PROFILE, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.code, &e.field_path))
            .collect::<Vec<_>>()
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
