//! Parameter validation tests for the production exemplar gauge widget.
//!
//! These tests load the canonical production gauge bundle from
//! `assets/widget_bundles/gauge/` and exercise its parameter types through the
//! scene graph's `publish_to_widget` validation path.
//!
//! Acceptance criteria (hud-awpt):
//! 1. Level clamping: out-of-range f32 values are clamped to [0.0, 1.0] silently.
//! 2. Level NaN/infinity: rejected with `WidgetParameterInvalidValue`.
//! 3. Unknown parameter rejection: `WidgetUnknownParameter` error.
//! 4. Severity enum validation: case-sensitive, only "info"/"warning"/"error" accepted.
//! 5. Default values match widget.toml schema exactly.
//!
//! Source: widget-system/spec.md §Requirement: Widget Parameter Validation.

use std::collections::HashMap;
use std::path::PathBuf;

use tze_hud_scene::SceneGraph;
use tze_hud_scene::SceneId;
use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetInstance, WidgetParameterValue,
};
use tze_hud_scene::validation::ValidationError;
use tze_hud_widget::loader::{BundleScanResult, load_bundle_dir_with_tokens};

// ─── Fixture helpers ──────────────────────────────────────────────────────────

/// Minimal token map that resolves all `{{token.*}}` placeholders used in the
/// production gauge SVG files.
///
/// These placeholder values are for test isolation only — the actual visual
/// appearance is irrelevant; we just need the loader to succeed so that we can
/// exercise parameter validation in `publish_to_widget`.
fn gauge_test_tokens() -> HashMap<String, String> {
    HashMap::from([
        ("color.backdrop.default".to_string(), "#1a1a2e".to_string()),
        ("color.border.default".to_string(), "#2a2a4e".to_string()),
        ("color.outline.default".to_string(), "#3a3a5e".to_string()),
        ("color.severity.info".to_string(), "#4a9eff".to_string()),
        ("color.text.secondary".to_string(), "#8f96a3".to_string()),
        ("color.text.accent".to_string(), "#ffffff".to_string()),
        ("color.text.primary".to_string(), "#cccccc".to_string()),
        ("opacity.backdrop.opaque".to_string(), "0.9".to_string()),
        ("border.radius.small".to_string(), "4".to_string()),
        ("border.radius.medium".to_string(), "8".to_string()),
        ("border.radius.large".to_string(), "16".to_string()),
        ("key".to_string(), "test-key".to_string()),
        ("stroke.border.width".to_string(), "1".to_string()),
        ("stroke.outline.width".to_string(), "1".to_string()),
    ])
}

// ─── Fixture paths ─────────────────────────────────────────────────────────────

/// Path to the production exemplar gauge bundle.
///
/// Located at `assets/widget_bundles/gauge/` relative to the workspace root.
/// `CARGO_MANIFEST_DIR` points to `crates/tze_hud_widget/`, so we go up two levels.
fn production_gauge_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..") // crates/
        .join("..") // workspace root
        .join("assets")
        .join("widget_bundles")
        .join("gauge")
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Load the production gauge bundle and register it in a fresh `SceneGraph`.
///
/// Returns `(scene, tab_id)` ready for `publish_to_widget("gauge", ...)` calls.
fn scene_with_production_gauge() -> (SceneGraph, tze_hud_scene::types::SceneId /* tab_id */) {
    let path = production_gauge_path();
    let tokens = gauge_test_tokens();
    let result = load_bundle_dir_with_tokens(&path, &tokens);

    let bundle = match result {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => {
            panic!("production gauge bundle failed to load: {e}");
        }
    };

    let mut definition = bundle.definition.clone();
    // Override policies for test isolation (production bundle uses defaults).
    definition.default_contention_policy = ContentionPolicy::LatestWins;
    definition.default_rendering_policy = RenderingPolicy::default();
    definition.default_geometry_policy = GeometryPolicy::Relative {
        x_pct: 0.0,
        y_pct: 0.0,
        width_pct: 0.25,
        height_pct: 0.25,
    };

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();

    scene.widget_registry.register_definition(definition);

    // Build default current_params from the loaded schema.
    let def = scene
        .widget_registry
        .get_definition("gauge")
        .expect("gauge definition should be registered");
    let current_params: HashMap<String, WidgetParameterValue> = def
        .parameter_schema
        .iter()
        .map(|p| (p.name.clone(), p.default_value.clone()))
        .collect();

    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "gauge".to_string(),
        current_params,
    });

    (scene, tab_id)
}

// ─── AC 1: Level clamping ──────────────────────────────────────────────────────

/// WHEN level is submitted above the max (1.0) THEN it is clamped to 1.0 silently.
///
/// Source: widget-system/spec.md — f32 out-of-range values are clamped, not rejected.
#[test]
fn gauge_level_above_max_is_clamped_to_one() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([("level".to_string(), WidgetParameterValue::F32(1.5))]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        result.is_ok(),
        "level > 1.0 should clamp silently, got error: {result:?}"
    );

    let pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(pubs.len(), 1, "one publication should be recorded");
    match pubs[0].params.get("level") {
        Some(WidgetParameterValue::F32(v)) => {
            assert!(
                (v - 1.0).abs() < 1e-6,
                "clamped level should be 1.0, got {v}"
            );
        }
        other => panic!("expected F32(1.0) for level, got {other:?}"),
    }
}

/// WHEN level is submitted below the min (0.0) THEN it is clamped to 0.0 silently.
#[test]
fn gauge_level_below_min_is_clamped_to_zero() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([("level".to_string(), WidgetParameterValue::F32(-0.5))]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        result.is_ok(),
        "level < 0.0 should clamp silently, got error: {result:?}"
    );

    let pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(pubs.len(), 1);
    match pubs[0].params.get("level") {
        Some(WidgetParameterValue::F32(v)) => {
            assert!(v.abs() < 1e-6, "clamped level should be 0.0, got {v}");
        }
        other => panic!("expected F32(0.0) for level, got {other:?}"),
    }
}

/// WHEN level is at the boundary (exactly 0.0 or 1.0) THEN it is accepted unchanged.
#[test]
fn gauge_level_at_boundaries_accepted_unchanged() {
    for boundary in [0.0f32, 1.0f32] {
        let (mut scene, _tab) = scene_with_production_gauge();
        let params = HashMap::from([("level".to_string(), WidgetParameterValue::F32(boundary))]);
        let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
        assert!(
            result.is_ok(),
            "boundary level {boundary} should be accepted, got: {result:?}"
        );
        let pubs = scene.widget_registry.active_for_widget("gauge");
        match pubs[0].params.get("level") {
            Some(WidgetParameterValue::F32(v)) => {
                assert!(
                    (v - boundary).abs() < 1e-6,
                    "boundary {boundary} should be preserved, got {v}"
                );
            }
            other => panic!("expected F32({boundary}) for level, got {other:?}"),
        }
    }
}

// ─── AC 2: Level NaN / infinity rejection ─────────────────────────────────────

/// WHEN level is NaN THEN publish_to_widget returns WidgetParameterInvalidValue.
///
/// Source: widget-system/spec.md §Requirement: Widget Parameter Validation (F32 invariant).
#[test]
fn gauge_level_nan_is_rejected() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([("level".to_string(), WidgetParameterValue::F32(f32::NAN))]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "NaN level should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

/// WHEN level is +Inf THEN publish_to_widget returns WidgetParameterInvalidValue.
#[test]
fn gauge_level_pos_inf_is_rejected() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "level".to_string(),
        WidgetParameterValue::F32(f32::INFINITY),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "+Inf level should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

/// WHEN level is -Inf THEN publish_to_widget returns WidgetParameterInvalidValue.
#[test]
fn gauge_level_neg_inf_is_rejected() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "level".to_string(),
        WidgetParameterValue::F32(f32::NEG_INFINITY),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "-Inf level should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

// ─── AC 3: Unknown parameter rejection ────────────────────────────────────────

/// WHEN a parameter name not in the schema is submitted THEN WidgetUnknownParameter.
///
/// Source: widget-system/spec.md §Requirement: Widget Parameter Validation.
#[test]
fn gauge_unknown_parameter_is_rejected() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "nonexistent_param".to_string(),
        WidgetParameterValue::F32(0.5),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(result, Err(ValidationError::WidgetUnknownParameter { .. })),
        "unknown param should produce WidgetUnknownParameter, got: {result:?}"
    );
}

/// WHEN a near-miss parameter name (e.g. "Level" with capital L) is submitted
/// THEN WidgetUnknownParameter (schema is case-sensitive).
#[test]
fn gauge_parameter_names_are_case_sensitive() {
    let (mut scene, _tab) = scene_with_production_gauge();
    // "Level" vs "level" — the schema only declares "level" (lowercase).
    let params = HashMap::from([("Level".to_string(), WidgetParameterValue::F32(0.5))]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(result, Err(ValidationError::WidgetUnknownParameter { .. })),
        "capitalized 'Level' should be rejected as unknown parameter, got: {result:?}"
    );
}

// ─── AC 4: Severity enum validation ──────────────────────────────────────────

/// WHEN severity is "info" THEN it is accepted.
#[test]
fn gauge_severity_info_accepted() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("info".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        result.is_ok(),
        "severity='info' should be accepted, got: {result:?}"
    );
}

/// WHEN severity is "warning" THEN it is accepted.
#[test]
fn gauge_severity_warning_accepted() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("warning".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        result.is_ok(),
        "severity='warning' should be accepted, got: {result:?}"
    );
}

/// WHEN severity is "error" THEN it is accepted.
#[test]
fn gauge_severity_error_accepted() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("error".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        result.is_ok(),
        "severity='error' should be accepted, got: {result:?}"
    );
}

/// WHEN severity is "Info" (capital I) THEN it is rejected (case-sensitive enum).
///
/// Source: widget-system/spec.md — enum allowed_values comparison is case-sensitive.
#[test]
fn gauge_severity_capital_info_is_rejected() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("Info".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "'Info' (capital I) should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

/// WHEN severity is "WARNING" (all caps) THEN it is rejected (case-sensitive enum).
#[test]
fn gauge_severity_all_caps_warning_is_rejected() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("WARNING".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "'WARNING' (all caps) should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

/// WHEN severity is "critical" (not in allowed_values) THEN it is rejected.
#[test]
fn gauge_severity_critical_is_rejected() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("critical".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "severity='critical' (unlisted) should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

/// WHEN severity is an empty string THEN it is rejected.
#[test]
fn gauge_severity_empty_string_is_rejected() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "empty severity should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

// ─── AC 5: Default values match widget.toml schema ───────────────────────────

/// WHEN the production gauge bundle is loaded THEN all parameter defaults
/// match the values declared in `assets/widget_bundles/gauge/widget.toml` exactly.
///
/// Expected from widget.toml:
///   level      = 0.0         (f32)
///   label      = ""          (string)
///   fill_color = [74,158,255,255] → Rgba { r: 74/255, g: 158/255, b: 1.0, a: 1.0 }
///   severity   = "info"      (enum)
///   tooltip_visible = 0.0    (f32)
///   readout    = "0/100 (0%)" (string)
#[test]
fn gauge_default_values_match_widget_toml_schema() {
    let path = production_gauge_path();
    let tokens = gauge_test_tokens();
    let bundle = match load_bundle_dir_with_tokens(&path, &tokens) {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("production gauge bundle failed to load: {e}"),
    };
    let schema = &bundle.definition.parameter_schema;
    assert_eq!(schema.len(), 6, "gauge schema should have 6 parameters");

    // Find each parameter by name and verify its default.
    let level_decl = schema
        .iter()
        .find(|p| p.name == "level")
        .expect("'level' parameter should be declared");
    assert!(
        matches!(level_decl.default_value, WidgetParameterValue::F32(v) if v.abs() < 1e-6),
        "level default should be F32(0.0), got: {:?}",
        level_decl.default_value
    );

    let label_decl = schema
        .iter()
        .find(|p| p.name == "label")
        .expect("'label' parameter should be declared");
    assert!(
        matches!(&label_decl.default_value, WidgetParameterValue::String(s) if s.is_empty()),
        "label default should be String(\"\"), got: {:?}",
        label_decl.default_value
    );

    let fill_color_decl = schema
        .iter()
        .find(|p| p.name == "fill_color")
        .expect("'fill_color' parameter should be declared");
    // widget.toml: default = [74, 158, 255, 255]
    // loader converts to Rgba with f32 [0.0, 1.0] components.
    match &fill_color_decl.default_value {
        WidgetParameterValue::Color(rgba) => {
            let expected_r = 74.0f32 / 255.0;
            let expected_g = 158.0f32 / 255.0;
            let expected_b = 255.0f32 / 255.0;
            let expected_a = 255.0f32 / 255.0;
            assert!(
                (rgba.r - expected_r).abs() < 1e-4,
                "fill_color r should be {expected_r:.4}, got {:.4}",
                rgba.r
            );
            assert!(
                (rgba.g - expected_g).abs() < 1e-4,
                "fill_color g should be {expected_g:.4}, got {:.4}",
                rgba.g
            );
            assert!(
                (rgba.b - expected_b).abs() < 1e-4,
                "fill_color b should be {expected_b:.4}, got {:.4}",
                rgba.b
            );
            assert!(
                (rgba.a - expected_a).abs() < 1e-4,
                "fill_color a should be {expected_a:.4}, got {:.4}",
                rgba.a
            );
        }
        other => panic!("fill_color default should be Color variant, got: {other:?}"),
    }

    let severity_decl = schema
        .iter()
        .find(|p| p.name == "severity")
        .expect("'severity' parameter should be declared");
    assert!(
        matches!(&severity_decl.default_value, WidgetParameterValue::Enum(s) if s == "info"),
        "severity default should be Enum(\"info\"), got: {:?}",
        severity_decl.default_value
    );

    let tooltip_visible_decl = schema
        .iter()
        .find(|p| p.name == "tooltip_visible")
        .expect("'tooltip_visible' parameter should be declared");
    assert!(
        matches!(
            tooltip_visible_decl.default_value,
            WidgetParameterValue::F32(v) if v.abs() < 1e-6
        ),
        "tooltip_visible default should be F32(0.0), got: {:?}",
        tooltip_visible_decl.default_value
    );

    let readout_decl = schema
        .iter()
        .find(|p| p.name == "readout")
        .expect("'readout' parameter should be declared");
    assert!(
        matches!(
            &readout_decl.default_value,
            WidgetParameterValue::String(s) if s == "0/100 (0%)"
        ),
        "readout default should be String(\"0/100 (0%)\"), got: {:?}",
        readout_decl.default_value
    );
}

/// WHEN the production gauge schema is loaded THEN severity constraints declare
/// exactly ["info", "warning", "error"] as allowed values.
#[test]
fn gauge_severity_constraints_match_widget_toml() {
    let path = production_gauge_path();
    let tokens = gauge_test_tokens();
    let bundle = match load_bundle_dir_with_tokens(&path, &tokens) {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("production gauge bundle failed to load: {e}"),
    };
    let schema = &bundle.definition.parameter_schema;
    let severity_decl = schema
        .iter()
        .find(|p| p.name == "severity")
        .expect("'severity' parameter should be declared");

    let constraints = severity_decl
        .constraints
        .as_ref()
        .expect("severity parameter should have constraints");
    let allowed: Vec<&str> = constraints
        .enum_allowed_values
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(
        allowed,
        vec!["info", "warning", "error"],
        "severity allowed_values should be [\"info\", \"warning\", \"error\"] in order"
    );
}

/// WHEN the production gauge schema is loaded THEN level constraints declare
/// f32_min=0.0 and f32_max=1.0.
#[test]
fn gauge_level_constraints_match_widget_toml() {
    let path = production_gauge_path();
    let tokens = gauge_test_tokens();
    let bundle = match load_bundle_dir_with_tokens(&path, &tokens) {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("production gauge bundle failed to load: {e}"),
    };
    let schema = &bundle.definition.parameter_schema;
    let level_decl = schema
        .iter()
        .find(|p| p.name == "level")
        .expect("'level' parameter should be declared");

    let constraints = level_decl
        .constraints
        .as_ref()
        .expect("level parameter should have constraints");
    assert!(
        matches!(constraints.f32_min, Some(v) if (v - 0.0).abs() < 1e-6),
        "level f32_min should be 0.0, got: {:?}",
        constraints.f32_min
    );
    assert!(
        matches!(constraints.f32_max, Some(v) if (v - 1.0).abs() < 1e-6),
        "level f32_max should be 1.0, got: {:?}",
        constraints.f32_max
    );
}

// ─── Type mismatch: all four parameter types ──────────────────────────────────

/// WHEN a string value is submitted for the f32 'level' parameter
/// THEN WidgetParameterTypeMismatch is returned.
#[test]
fn gauge_level_string_value_is_type_mismatch() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([(
        "level".to_string(),
        WidgetParameterValue::String("0.5".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterTypeMismatch { .. })
        ),
        "string for f32 param should produce WidgetParameterTypeMismatch, got: {result:?}"
    );
}

/// WHEN an f32 value is submitted for the color 'fill_color' parameter
/// THEN WidgetParameterTypeMismatch is returned.
#[test]
fn gauge_fill_color_f32_value_is_type_mismatch() {
    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([("fill_color".to_string(), WidgetParameterValue::F32(0.5))]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterTypeMismatch { .. })
        ),
        "f32 for color param should produce WidgetParameterTypeMismatch, got: {result:?}"
    );
}

/// WHEN a string value is submitted for the enum 'severity' parameter
/// THEN WidgetParameterTypeMismatch is returned (wrong variant, not just wrong value).
#[test]
fn gauge_severity_string_variant_is_type_mismatch() {
    let (mut scene, _tab) = scene_with_production_gauge();
    // Submit a String variant, not an Enum variant — even if the content is valid.
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::String("info".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterTypeMismatch { .. })
        ),
        "String variant for enum param should produce WidgetParameterTypeMismatch, got: {result:?}"
    );
}

// ─── Multi-param publish ──────────────────────────────────────────────────────

/// WHEN all four valid parameters are published together THEN publish succeeds.
#[test]
fn gauge_all_valid_params_published_together() {
    use tze_hud_scene::types::Rgba;

    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([
        ("level".to_string(), WidgetParameterValue::F32(0.75)),
        (
            "label".to_string(),
            WidgetParameterValue::String("CPU".to_string()),
        ),
        (
            "fill_color".to_string(),
            WidgetParameterValue::Color(Rgba::new(1.0, 0.5, 0.0, 1.0)),
        ),
        (
            "severity".to_string(),
            WidgetParameterValue::Enum("warning".to_string()),
        ),
    ]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        result.is_ok(),
        "all valid gauge params should be accepted, got: {result:?}"
    );

    let pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(pubs.len(), 1, "one publication should be recorded");
    assert_eq!(
        pubs[0].params.len(),
        4,
        "all 4 params should be in the record"
    );
}

/// WHEN one invalid parameter is mixed with valid parameters THEN the whole
/// publish is rejected (no partial apply).
#[test]
fn gauge_mixed_valid_and_invalid_params_rejects_all() {
    use tze_hud_scene::types::Rgba;

    let (mut scene, _tab) = scene_with_production_gauge();
    let params = HashMap::from([
        (
            "level".to_string(),
            WidgetParameterValue::F32(0.5), // valid
        ),
        (
            "fill_color".to_string(),
            WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)), // valid
        ),
        (
            "severity".to_string(),
            WidgetParameterValue::Enum("CRITICAL".to_string()), // invalid: not in allowed values
        ),
    ]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        result.is_err(),
        "invalid severity should reject the entire publish, got: {result:?}"
    );

    // No publish should have been recorded.
    let pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        pubs.len(),
        0,
        "no publication should be recorded when validation fails"
    );
}
