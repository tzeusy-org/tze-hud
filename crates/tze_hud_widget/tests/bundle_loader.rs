//! Integration tests for the widget asset bundle loader.
//!
//! These tests cover:
//! - Valid bundle loading (acceptance criterion 1)
//! - All structured error codes (acceptance criterion 2)
//! - Binding validation (acceptance criterion 3)
//! - SVG ID resolution
//! - Reference gauge fixture
//!
//! Source: widget-system/spec.md §Requirement: Widget Asset Bundle Format,
//!         §Requirement: SVG Layer Parameter Bindings.

use std::collections::HashMap;
use std::path::PathBuf;

use tze_hud_scene::types::ContentionPolicy;
use tze_hud_widget::error::BundleError;
use tze_hud_widget::loader::{
    BundleScanResult, load_bundle_dir, load_bundle_dir_with_tokens, scan_bundle_dirs,
};

// ─── Test helper: path to the gauge fixture ───────────────────────────────────

fn gauge_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("gauge")
}

// ─── Reference gauge fixture ──────────────────────────────────────────────────

/// Acceptance criterion 1: valid widget bundles load and produce WidgetDefinitions.
/// Acceptance criterion 6: Reference gauge bundle included in test fixtures.
#[test]
fn gauge_fixture_loads_successfully() {
    let dir = gauge_fixture_path();
    let result = load_bundle_dir(&dir);
    match result {
        BundleScanResult::Ok(bundle) => {
            let def = &bundle.definition;
            assert_eq!(def.id, "gauge", "widget id should be 'gauge'");
            assert_eq!(def.name, "gauge");
            assert!(
                !def.description.is_empty(),
                "description should not be empty"
            );

            // Parameter schema: level, label, fill_color, severity.
            assert_eq!(def.parameter_schema.len(), 4);
            let param_names: Vec<&str> = def
                .parameter_schema
                .iter()
                .map(|p| p.name.as_str())
                .collect();
            assert!(param_names.contains(&"level"));
            assert!(param_names.contains(&"label"));
            assert!(param_names.contains(&"fill_color"));
            assert!(param_names.contains(&"severity"));

            // Layers: background and fill.
            assert_eq!(def.layers.len(), 2);
            assert_eq!(def.layers[0].svg_file, "background.svg");
            assert_eq!(def.layers[1].svg_file, "fill.svg");

            // background.svg has no bindings.
            assert!(def.layers[0].bindings.is_empty());

            // fill.svg has 4 bindings: level→bar/height (linear),
            // fill_color→bar/fill (direct), label→label-text/text-content (direct),
            // severity→indicator/fill (discrete).
            let fill_bindings = &def.layers[1].bindings;
            assert_eq!(fill_bindings.len(), 4);

            // Check linear binding.
            let level_binding = fill_bindings.iter().find(|b| b.param == "level").unwrap();
            assert_eq!(level_binding.target_element, "bar");
            assert_eq!(level_binding.target_attribute, "height");
            assert!(
                matches!(
                    level_binding.mapping,
                    tze_hud_scene::types::WidgetBindingMapping::Linear {
                        attr_min,
                        attr_max
                    } if (attr_min - 0.0).abs() < 1e-6 && (attr_max - 200.0).abs() < 1e-6
                ),
                "expected linear mapping with attr_min=0, attr_max=200"
            );

            // Check direct binding for color.
            let color_binding = fill_bindings
                .iter()
                .find(|b| b.param == "fill_color")
                .unwrap();
            assert_eq!(color_binding.target_element, "bar");
            assert_eq!(color_binding.target_attribute, "fill");
            assert!(matches!(
                color_binding.mapping,
                tze_hud_scene::types::WidgetBindingMapping::Direct
            ));

            // Check text-content synthetic binding.
            let label_binding = fill_bindings.iter().find(|b| b.param == "label").unwrap();
            assert_eq!(label_binding.target_element, "label-text");
            assert_eq!(label_binding.target_attribute, "text-content");
            assert!(matches!(
                label_binding.mapping,
                tze_hud_scene::types::WidgetBindingMapping::Direct
            ));

            // Check discrete binding.
            let sev_binding = fill_bindings
                .iter()
                .find(|b| b.param == "severity")
                .unwrap();
            assert_eq!(sev_binding.target_element, "indicator");
            assert_eq!(sev_binding.target_attribute, "fill");
            if let tze_hud_scene::types::WidgetBindingMapping::Discrete { value_map } =
                &sev_binding.mapping
            {
                assert_eq!(value_map.get("info").map(String::as_str), Some("#00cc66"));
                assert_eq!(
                    value_map.get("warning").map(String::as_str),
                    Some("#ffcc00")
                );
                assert_eq!(value_map.get("error").map(String::as_str), Some("#ff3300"));
            } else {
                panic!("expected discrete mapping for severity binding");
            }

            // SVG content is captured.
            assert!(bundle.svg_contents.contains_key("background.svg"));
            assert!(bundle.svg_contents.contains_key("fill.svg"));
        }
        BundleScanResult::Err(e) => {
            panic!("gauge fixture failed to load: {e}");
        }
    }
}

// ─── Status-indicator fixture [hud-tjq8.1] ───────────────────────────────────

fn status_indicator_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("status-indicator")
}

/// Returns the canonical token map for the status-indicator fixture.
/// Keys match every {{token.key}} placeholder in indicator.svg.
fn status_indicator_tokens() -> HashMap<String, String> {
    let mut tokens = HashMap::new();
    tokens.insert("color.border.default".to_string(), "#333333".to_string());
    tokens.insert("color.text.secondary".to_string(), "#aaaaaa".to_string());
    tokens
}

/// Validates the status-indicator fixture bundle:
/// - widget type is "status-indicator"
/// - exactly 5 parameters (status, theme, label, reason, tooltip_visible)
/// - exactly 1 layer (indicator.svg)
/// - includes theme-selective visibility/styling bindings
///
/// Source: hud-tjq8.1 §Create status-indicator asset bundle
#[test]
fn status_indicator_fixture_loads_successfully() {
    let dir = status_indicator_fixture_path();
    let tokens = status_indicator_tokens();
    let result = load_bundle_dir_with_tokens(&dir, &tokens);
    match result {
        BundleScanResult::Ok(bundle) => {
            let def = &bundle.definition;
            assert_eq!(
                def.id, "status-indicator",
                "widget id should be 'status-indicator'"
            );
            assert_eq!(def.name, "status-indicator");
            assert!(
                !def.description.is_empty(),
                "description should not be empty"
            );

            // Contention policy must be LatestWins as declared in widget.toml.
            assert!(
                matches!(def.default_contention_policy, ContentionPolicy::LatestWins),
                "default_contention_policy must be ContentionPolicy::LatestWins"
            );

            // Parameter schema: status/theme (enum), label/reason (string), tooltip_visible (f32).
            assert_eq!(
                def.parameter_schema.len(),
                5,
                "must have exactly 5 parameters"
            );
            let param_names: Vec<&str> = def
                .parameter_schema
                .iter()
                .map(|p| p.name.as_str())
                .collect();
            assert!(
                param_names.contains(&"status"),
                "must declare 'status' param"
            );
            assert!(param_names.contains(&"theme"), "must declare 'theme' param");
            assert!(param_names.contains(&"label"), "must declare 'label' param");
            assert!(
                param_names.contains(&"reason"),
                "must declare 'reason' param"
            );
            assert!(
                param_names.contains(&"tooltip_visible"),
                "must declare 'tooltip_visible' param"
            );

            let hover = def
                .hover_behavior
                .as_ref()
                .expect("status-indicator fixture must declare hover_behavior");
            assert_eq!(
                hover.visibility_param, "tooltip_visible",
                "hover behavior must drive tooltip_visible"
            );
            assert_eq!(hover.delay_ms, 1_500, "hover delay should be 1500ms");
            assert!((hover.hidden_value - 0.0).abs() < f32::EPSILON);
            assert!((hover.visible_value - 1.0).abs() < f32::EPSILON);
            assert!((hover.trigger_rect.x_pct - 0.88).abs() < 1e-6);
            assert!((hover.trigger_rect.y_pct - 0.06).abs() < 1e-6);
            assert!((hover.trigger_rect.width_pct - 0.08).abs() < 1e-6);
            assert!((hover.trigger_rect.height_pct - 0.22).abs() < 1e-6);

            // Verify status param is an enum with the expected allowed values.
            let status_param = def
                .parameter_schema
                .iter()
                .find(|p| p.name == "status")
                .unwrap();
            let status_constraints = status_param
                .constraints
                .as_ref()
                .expect("status parameter should have enum constraints");
            assert!(
                status_constraints
                    .enum_allowed_values
                    .contains(&"online".to_string()),
                "status constraints must include 'online'"
            );
            assert!(
                status_constraints
                    .enum_allowed_values
                    .contains(&"away".to_string()),
                "status constraints must include 'away'"
            );
            assert!(
                status_constraints
                    .enum_allowed_values
                    .contains(&"busy".to_string()),
                "status constraints must include 'busy'"
            );
            assert!(
                status_constraints
                    .enum_allowed_values
                    .contains(&"offline".to_string()),
                "status constraints must include 'offline'"
            );

            // Verify theme param is an enum with the expected allowed values.
            let theme_param = def
                .parameter_schema
                .iter()
                .find(|p| p.name == "theme")
                .unwrap();
            let theme_constraints = theme_param
                .constraints
                .as_ref()
                .expect("theme parameter should have enum constraints");
            assert!(
                theme_constraints
                    .enum_allowed_values
                    .contains(&"minimal".to_string()),
                "theme constraints must include 'minimal'"
            );
            assert!(
                theme_constraints
                    .enum_allowed_values
                    .contains(&"system".to_string()),
                "theme constraints must include 'system'"
            );
            assert!(
                theme_constraints
                    .enum_allowed_values
                    .contains(&"friendly".to_string()),
                "theme constraints must include 'friendly'"
            );

            // Layers: exactly 1 layer (indicator.svg).
            assert_eq!(def.layers.len(), 1, "must have exactly 1 layer");
            assert_eq!(def.layers[0].svg_file, "indicator.svg");

            // indicator.svg has status, theme, tooltip visibility and text bindings.
            let bindings = &def.layers[0].bindings;
            assert_eq!(
                bindings.len(),
                19,
                "indicator.svg must have exactly 19 bindings"
            );

            // Check canonical status color binding: status → system-fill / fill.
            let status_binding = bindings
                .iter()
                .find(|b| b.param == "status" && b.target_element == "system-fill")
                .unwrap();
            assert_eq!(status_binding.target_attribute, "fill");
            if let tze_hud_scene::types::WidgetBindingMapping::Discrete { value_map } =
                &status_binding.mapping
            {
                assert_eq!(value_map.get("online").map(String::as_str), Some("#4FB543"));
                assert_eq!(value_map.get("away").map(String::as_str), Some("#D97706"));
                assert_eq!(value_map.get("busy").map(String::as_str), Some("#DC2626"));
                assert_eq!(
                    value_map.get("offline").map(String::as_str),
                    Some("#6B7280")
                );
            } else {
                panic!("expected discrete mapping for status binding");
            }

            // Check canonical status glyph binding: status → system-glyph / text-content.
            let status_glyph_binding = bindings
                .iter()
                .find(|b| b.param == "status" && b.target_element == "system-glyph")
                .unwrap();
            assert_eq!(status_glyph_binding.target_attribute, "text-content");
            if let tze_hud_scene::types::WidgetBindingMapping::Discrete { value_map } =
                &status_glyph_binding.mapping
            {
                assert_eq!(value_map.get("online").map(String::as_str), Some("✓"));
                assert_eq!(value_map.get("away").map(String::as_str), Some("…"));
                assert_eq!(value_map.get("busy").map(String::as_str), Some("!"));
                assert_eq!(value_map.get("offline").map(String::as_str), Some("×"));
            } else {
                panic!("expected discrete mapping for status glyph binding");
            }

            // Theme binding: system-group opacity should map theme enum values.
            let system_theme_binding = bindings
                .iter()
                .find(|b| b.param == "theme" && b.target_element == "system-group")
                .unwrap();
            assert_eq!(system_theme_binding.target_attribute, "opacity");
            if let tze_hud_scene::types::WidgetBindingMapping::Discrete { value_map } =
                &system_theme_binding.mapping
            {
                assert_eq!(value_map.get("minimal").map(String::as_str), Some("0"));
                assert_eq!(value_map.get("system").map(String::as_str), Some("0.82"));
                assert_eq!(value_map.get("friendly").map(String::as_str), Some("0"));
            } else {
                panic!("expected discrete mapping for theme/system-group binding");
            }

            // Check linear opacity binding: tooltip_visible -> tooltip-group/opacity.
            let tooltip_opacity_binding = bindings
                .iter()
                .find(|b| b.param == "tooltip_visible")
                .unwrap();
            assert_eq!(tooltip_opacity_binding.target_element, "tooltip-group");
            assert_eq!(tooltip_opacity_binding.target_attribute, "opacity");
            assert!(
                matches!(
                    tooltip_opacity_binding.mapping,
                    tze_hud_scene::types::WidgetBindingMapping::Linear {
                        attr_min,
                        attr_max
                    } if (attr_min - 0.0).abs() < 1e-6 && (attr_max - 1.0).abs() < 1e-6
                ),
                "tooltip_visible must linearly map to opacity 0..1"
            );

            // Check direct text-content binding: label → tooltip-label.
            let label_binding = bindings
                .iter()
                .find(|b| b.param == "label" && b.target_element == "tooltip-label")
                .unwrap();
            assert_eq!(label_binding.target_attribute, "text-content");
            assert!(matches!(
                label_binding.mapping,
                tze_hud_scene::types::WidgetBindingMapping::Direct
            ));

            // Check direct text-content binding: reason → tooltip-reason.
            let reason_binding = bindings.iter().find(|b| b.param == "reason").unwrap();
            assert_eq!(reason_binding.target_element, "tooltip-reason");
            assert_eq!(reason_binding.target_attribute, "text-content");
            assert!(matches!(
                reason_binding.mapping,
                tze_hud_scene::types::WidgetBindingMapping::Direct
            ));

            // SVG content is captured.
            assert!(bundle.svg_contents.contains_key("indicator.svg"));
        }
        BundleScanResult::Err(e) => {
            panic!("status-indicator fixture failed to load: {e}");
        }
    }
}

// ─── Error: WIDGET_BUNDLE_NO_MANIFEST ─────────────────────────────────────────

/// Acceptance criterion 2: WIDGET_BUNDLE_NO_MANIFEST.
/// Source: widget-system/spec.md §Scenario: Missing manifest rejected.
#[test]
fn no_manifest_error() {
    let dir = tempfile::tempdir().unwrap();
    // Create an SVG file but no widget.toml.
    std::fs::write(dir.path().join("fill.svg"), b"<svg></svg>").unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::NoManifest { path }) => {
            assert!(path.contains(dir.path().to_str().unwrap()));
        }
        other => panic!("expected NoManifest, got {other:?}"),
    }
}

#[test]
fn no_manifest_wire_code() {
    let err = BundleError::NoManifest {
        path: "/tmp/test".to_string(),
    };
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_NO_MANIFEST");
}

// ─── Error: WIDGET_BUNDLE_INVALID_MANIFEST ────────────────────────────────────

/// Acceptance criterion 2: WIDGET_BUNDLE_INVALID_MANIFEST.
/// Source: widget-system/spec.md §Scenario: Invalid manifest rejected.
#[test]
fn invalid_manifest_toml_syntax_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        b"this is not [ valid toml >>>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::InvalidManifest { path, detail }) => {
            assert!(path.contains(dir.path().to_str().unwrap()));
            assert!(!detail.is_empty());
        }
        other => panic!("expected InvalidManifest, got {other:?}"),
    }
}

#[test]
fn invalid_manifest_missing_name() {
    let dir = tempfile::tempdir().unwrap();
    // Valid TOML but missing required 'name' field.
    std::fs::write(
        dir.path().join("widget.toml"),
        b"version = \"1.0.0\"\ndescription = \"test\"\n",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::InvalidManifest { detail, .. }) => {
            assert!(
                detail.contains("name"),
                "error should mention 'name', got: {detail}"
            );
        }
        other => panic!("expected InvalidManifest, got {other:?}"),
    }
}

#[test]
fn invalid_manifest_wire_code() {
    let err = BundleError::InvalidManifest {
        path: "/tmp".to_string(),
        detail: "missing name".to_string(),
    };
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_INVALID_MANIFEST");
}

// ─── Error: WIDGET_BUNDLE_MISSING_SVG ────────────────────────────────────────

/// Acceptance criterion 2: WIDGET_BUNDLE_MISSING_SVG.
/// Source: widget-system/spec.md §Scenario: Missing SVG file rejected.
#[test]
fn missing_svg_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "test"
version = "1.0.0"
description = "test widget"

[[layers]]
svg_file = "nonexistent.svg"
"#,
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::MissingSvg { svg_file, .. }) => {
            assert_eq!(svg_file, "nonexistent.svg");
        }
        other => panic!("expected MissingSvg, got {other:?}"),
    }
}

#[test]
fn missing_svg_wire_code() {
    let err = BundleError::MissingSvg {
        path: "/tmp".to_string(),
        svg_file: "fill.svg".to_string(),
    };
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_MISSING_SVG");
}

// ─── Error: WIDGET_BUNDLE_SVG_PARSE_ERROR ────────────────────────────────────

/// Acceptance criterion 2: WIDGET_BUNDLE_SVG_PARSE_ERROR.
/// Source: widget-system/spec.md §Scenario: SVG parse failure rejected.
#[test]
fn svg_parse_error_invalid_xml() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "test"
version = "1.0.0"
description = "test"

[[layers]]
svg_file = "bad.svg"
"#,
    )
    .unwrap();
    // Write a file that is not valid SVG.
    std::fs::write(dir.path().join("bad.svg"), b"not xml at all ><<>").unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::SvgParseError { svg_file, .. }) => {
            assert_eq!(svg_file, "bad.svg");
        }
        other => panic!("expected SvgParseError, got {other:?}"),
    }
}

#[test]
fn svg_parse_error_non_svg_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "test"
version = "1.0.0"
description = "test"

[[layers]]
svg_file = "html.svg"
"#,
    )
    .unwrap();
    // Write well-formed XML but with <html> root, not <svg>.
    std::fs::write(
        dir.path().join("html.svg"),
        b"<html><body>not an svg</body></html>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::SvgParseError { svg_file, .. }) => {
            assert_eq!(svg_file, "html.svg");
        }
        other => panic!("expected SvgParseError, got {other:?}"),
    }
}

#[test]
fn svg_parse_error_wire_code() {
    let err = BundleError::SvgParseError {
        path: "/tmp".to_string(),
        svg_file: "fill.svg".to_string(),
        detail: "bad XML".to_string(),
    };
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_SVG_PARSE_ERROR");
}

// ─── Error: WIDGET_BUNDLE_DUPLICATE_TYPE ─────────────────────────────────────

/// Acceptance criterion 2: WIDGET_BUNDLE_DUPLICATE_TYPE.
/// Source: widget-system/spec.md §Scenario: Duplicate type name rejected.
#[test]
fn duplicate_type_error_via_scan() {
    // Create two bundle directories with the same widget name.
    let root = tempfile::tempdir().unwrap();
    let bundle_a = root.path().join("bundle_a");
    let bundle_b = root.path().join("bundle_b");
    std::fs::create_dir_all(&bundle_a).unwrap();
    std::fs::create_dir_all(&bundle_b).unwrap();

    let manifest = br#"name = "gauge"
version = "1.0.0"
description = "a gauge"

[[layers]]
svg_file = "fill.svg"
"#;
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"bar\"/></svg>";

    std::fs::write(bundle_a.join("widget.toml"), manifest).unwrap();
    std::fs::write(bundle_a.join("fill.svg"), svg).unwrap();
    std::fs::write(bundle_b.join("widget.toml"), manifest).unwrap();
    std::fs::write(bundle_b.join("fill.svg"), svg).unwrap();

    let results = scan_bundle_dirs(
        &[root.path().to_path_buf()],
        &std::collections::HashMap::new(),
    );

    // Exactly one should succeed and one should fail with DuplicateType.
    let ok_count = results
        .iter()
        .filter(|r| matches!(r, BundleScanResult::Ok(_)))
        .count();
    let dup_count = results
        .iter()
        .filter(|r| matches!(r, BundleScanResult::Err(BundleError::DuplicateType { .. })))
        .count();
    assert_eq!(ok_count, 1, "exactly one bundle should succeed");
    assert_eq!(dup_count, 1, "exactly one duplicate error expected");
}

#[test]
fn duplicate_type_wire_code() {
    let err = BundleError::DuplicateType {
        name: "gauge".to_string(),
        existing_path: "/a".to_string(),
        new_path: "/b".to_string(),
    };
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_DUPLICATE_TYPE");
}

// ─── Error: WIDGET_BINDING_UNRESOLVABLE ───────────────────────────────────────

/// Acceptance criterion 2 + 3: WIDGET_BINDING_UNRESOLVABLE.
/// Bindings referencing nonexistent params/elements rejected at load time.
/// Source: widget-system/spec.md §Scenario: Unresolvable binding rejected.
#[test]
fn binding_nonexistent_param_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "test"
version = "1.0.0"
description = "test"

# No parameters declared!

[[layers]]
svg_file = "fill.svg"

[[layers.bindings]]
param = "nonexistent"
target_element = "bar"
target_attribute = "height"
mapping = "linear"
attr_min = 0.0
attr_max = 200.0
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"bar\"/></svg>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::BindingUnresolvable { detail, .. }) => {
            assert!(
                detail.contains("nonexistent"),
                "error should mention the bad param name, got: {detail}"
            );
        }
        other => panic!("expected BindingUnresolvable, got {other:?}"),
    }
}

#[test]
fn binding_nonexistent_element_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "test"
version = "1.0.0"
description = "test"

[[parameter_schema]]
name = "level"
type = "f32"
default = 0.0

[[layers]]
svg_file = "fill.svg"

[[layers.bindings]]
param = "level"
target_element = "no-such-id"
target_attribute = "height"
mapping = "linear"
attr_min = 0.0
attr_max = 100.0
"#,
    )
    .unwrap();
    // SVG does not have id="no-such-id".
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"bar\"/></svg>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::BindingUnresolvable { detail, .. }) => {
            assert!(
                detail.contains("no-such-id"),
                "error should mention the missing element id, got: {detail}"
            );
        }
        other => panic!("expected BindingUnresolvable, got {other:?}"),
    }
}

#[test]
fn binding_incompatible_mapping_type_rejected() {
    // 'linear' mapping is only valid for f32 parameters; applying it to a
    // 'string' parameter must be rejected.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "test"
version = "1.0.0"
description = "test"

[[parameter_schema]]
name = "label"
type = "string"
default = ""

[[layers]]
svg_file = "fill.svg"

[[layers.bindings]]
param = "label"
target_element = "bar"
target_attribute = "height"
mapping = "linear"
attr_min = 0.0
attr_max = 100.0
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"bar\"/></svg>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::BindingUnresolvable { detail, .. }) => {
            assert!(
                detail.contains("linear"),
                "error should mention 'linear', got: {detail}"
            );
        }
        other => panic!("expected BindingUnresolvable, got {other:?}"),
    }
}

#[test]
fn binding_unresolvable_wire_code() {
    let err = BundleError::BindingUnresolvable {
        path: "/tmp".to_string(),
        detail: "param not found".to_string(),
    };
    assert_eq!(err.wire_code(), "WIDGET_BINDING_UNRESOLVABLE");
}

// ─── Three mapping types ──────────────────────────────────────────────────────

/// Acceptance criterion: all three mapping types load correctly.
#[test]
fn linear_mapping_loads() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "linear-test"
version = "1.0.0"
description = "linear"

[[parameter_schema]]
name = "level"
type = "f32"
default = 0.0

[parameter_schema.constraints]
f32_min = 0.0
f32_max = 1.0

[[layers]]
svg_file = "fill.svg"

[[layers.bindings]]
param = "level"
target_element = "bar"
target_attribute = "height"
mapping = "linear"
attr_min = 10.0
attr_max = 150.0
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"bar\"/></svg>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Ok(bundle) => {
            let binding = &bundle.definition.layers[0].bindings[0];
            assert!(matches!(
                binding.mapping,
                tze_hud_scene::types::WidgetBindingMapping::Linear {
                    attr_min,
                    attr_max
                } if (attr_min - 10.0).abs() < 1e-5 && (attr_max - 150.0).abs() < 1e-5
            ));
        }
        BundleScanResult::Err(e) => panic!("unexpected error: {e}"),
    }
}

#[test]
fn direct_string_mapping_loads() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "direct-test"
version = "1.0.0"
description = "direct"

[[parameter_schema]]
name = "label"
type = "string"
default = ""

[[layers]]
svg_file = "fill.svg"

[[layers.bindings]]
param = "label"
target_element = "label-el"
target_attribute = "text-content"
mapping = "direct"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><text id=\"label-el\"/></svg>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Ok(bundle) => {
            let binding = &bundle.definition.layers[0].bindings[0];
            assert_eq!(binding.target_attribute, "text-content");
            assert!(matches!(
                binding.mapping,
                tze_hud_scene::types::WidgetBindingMapping::Direct
            ));
        }
        BundleScanResult::Err(e) => panic!("unexpected error: {e}"),
    }
}

#[test]
fn discrete_enum_mapping_loads() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "discrete-test"
version = "1.0.0"
description = "discrete"

[[parameter_schema]]
name = "severity"
type = "enum"
default = "info"

[parameter_schema.constraints]
enum_allowed_values = ["info", "warning", "error"]

[[layers]]
svg_file = "fill.svg"

[[layers.bindings]]
param = "severity"
target_element = "indicator"
target_attribute = "fill"
mapping = "discrete"

[layers.bindings.value_map]
info = "green"
warning = "yellow"
error = "red"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><circle id=\"indicator\"/></svg>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Ok(bundle) => {
            let binding = &bundle.definition.layers[0].bindings[0];
            if let tze_hud_scene::types::WidgetBindingMapping::Discrete { value_map } =
                &binding.mapping
            {
                assert_eq!(value_map.get("info").map(String::as_str), Some("green"));
                assert_eq!(value_map.get("warning").map(String::as_str), Some("yellow"));
                assert_eq!(value_map.get("error").map(String::as_str), Some("red"));
            } else {
                panic!("expected discrete mapping");
            }
        }
        BundleScanResult::Err(e) => panic!("unexpected error: {e}"),
    }
}

// ─── Discrete binding value_map coverage validation ──────────────────────────

/// A discrete binding whose value_map exactly covers all enum_allowed_values passes.
#[test]
fn discrete_value_map_complete_coverage_passes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "coverage-ok"
version = "1.0.0"
description = "complete coverage"

[[parameter_schema]]
name = "status"
type = "enum"
default = "on"

[parameter_schema.constraints]
enum_allowed_values = ["on", "off"]

[[layers]]
svg_file = "fill.svg"

[[layers.bindings]]
param = "status"
target_element = "indicator"
target_attribute = "fill"
mapping = "discrete"

[layers.bindings.value_map]
on = "green"
off = "red"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><circle id=\"indicator\"/></svg>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    assert!(
        matches!(result, BundleScanResult::Ok(_)),
        "expected Ok, got: {result:?}"
    );
}

/// Empty enum_allowed_values with empty value_map passes validation.
#[test]
fn discrete_empty_enum_and_empty_value_map_passes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "empty-enum"
version = "1.0.0"
description = "empty enum"

[[parameter_schema]]
name = "status"
type = "enum"
default = ""

[parameter_schema.constraints]
enum_allowed_values = []

[[layers]]
svg_file = "fill.svg"

[[layers.bindings]]
param = "status"
target_element = "indicator"
target_attribute = "fill"
mapping = "discrete"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><circle id=\"indicator\"/></svg>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    assert!(
        matches!(result, BundleScanResult::Ok(_)),
        "expected Ok, got: {result:?}"
    );
}

/// A discrete binding whose value_map is missing an enum value fails with BindingUnresolvable.
#[test]
fn discrete_value_map_missing_enum_value_fails() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "missing-value"
version = "1.0.0"
description = "missing enum value"

[[parameter_schema]]
name = "severity"
type = "enum"
default = "info"

[parameter_schema.constraints]
enum_allowed_values = ["info", "warning", "error"]

[[layers]]
svg_file = "fill.svg"

[[layers.bindings]]
param = "severity"
target_element = "indicator"
target_attribute = "fill"
mapping = "discrete"

[layers.bindings.value_map]
info = "green"
warning = "yellow"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><circle id=\"indicator\"/></svg>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::BindingUnresolvable { detail, .. }) => {
            assert!(
                detail.contains("error"),
                "error message should mention the missing enum value 'error', got: {detail}"
            );
        }
        other => panic!("expected BindingUnresolvable error, got: {other:?}"),
    }
}

/// A discrete binding whose value_map has extra entries beyond enum_allowed_values fails.
#[test]
fn discrete_value_map_extra_entries_fails() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "extra-entries"
version = "1.0.0"
description = "extra value_map entries"

[[parameter_schema]]
name = "severity"
type = "enum"
default = "info"

[parameter_schema.constraints]
enum_allowed_values = ["info", "warning"]

[[layers]]
svg_file = "fill.svg"

[[layers.bindings]]
param = "severity"
target_element = "indicator"
target_attribute = "fill"
mapping = "discrete"

[layers.bindings.value_map]
info = "green"
warning = "yellow"
critical = "purple"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><circle id=\"indicator\"/></svg>",
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::BindingUnresolvable { detail, .. }) => {
            assert!(
                detail.contains("critical"),
                "error message should mention the extra key 'critical', got: {detail}"
            );
        }
        other => panic!("expected BindingUnresolvable error, got: {other:?}"),
    }
}

// ─── Rejected bundle does not prevent others from loading ─────────────────────

/// Spec: A rejected bundle MUST NOT prevent other valid bundles from loading.
/// Source: widget-system/spec.md §Requirement: Widget Asset Bundle Format.
#[test]
fn invalid_bundle_does_not_prevent_valid_bundles() {
    let root = tempfile::tempdir().unwrap();

    // Bundle 1: valid.
    let valid_dir = root.path().join("valid-gauge");
    std::fs::create_dir_all(&valid_dir).unwrap();
    std::fs::write(
        valid_dir.join("widget.toml"),
        br#"name = "progress"
version = "1.0.0"
description = "progress bar"

[[layers]]
svg_file = "fill.svg"
"#,
    )
    .unwrap();
    std::fs::write(
        valid_dir.join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
    )
    .unwrap();

    // Bundle 2: invalid (no manifest).
    let bad_dir = root.path().join("no-manifest-bundle");
    std::fs::create_dir_all(&bad_dir).unwrap();
    std::fs::write(bad_dir.join("fill.svg"), b"<svg></svg>").unwrap();

    let results = scan_bundle_dirs(
        &[root.path().to_path_buf()],
        &std::collections::HashMap::new(),
    );

    let ok_count = results
        .iter()
        .filter(|r| matches!(r, BundleScanResult::Ok(_)))
        .count();
    assert_eq!(ok_count, 1, "valid bundle should still load");

    let err_count = results
        .iter()
        .filter(|r| matches!(r, BundleScanResult::Err(_)))
        .count();
    assert_eq!(err_count, 1, "invalid bundle should produce one error");
}

// ─── Empty bundle directory ──────────────────────────────────────────────────

#[test]
fn empty_bundle_root_returns_no_results() {
    let root = tempfile::tempdir().unwrap();
    let results = scan_bundle_dirs(
        &[root.path().to_path_buf()],
        &std::collections::HashMap::new(),
    );
    assert!(
        results.is_empty(),
        "empty root should produce no results, got {results:?}"
    );
}

// ─── Error: WIDGET_BUNDLE_INVALID_NAME ───────────────────────────────────────

/// Acceptance criterion: widget type name must match [a-z][a-z0-9-]*.
/// Names with uppercase letters, spaces, or special characters are rejected.
/// Source: scene-graph/spec.md §Widget Type Identifier.
#[test]
fn invalid_name_uppercase_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "Gauge"
version = "1.0.0"
description = "uppercase name not allowed"
"#,
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::InvalidName { name, .. }) => {
            assert_eq!(name, "Gauge");
        }
        other => panic!("expected InvalidName, got {other:?}"),
    }
}

#[test]
fn invalid_name_starts_with_digit_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "1gauge"
version = "1.0.0"
description = "digit-leading name not allowed"
"#,
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::InvalidName { name, .. }) => {
            assert_eq!(name, "1gauge");
        }
        other => panic!("expected InvalidName, got {other:?}"),
    }
}

#[test]
fn invalid_name_underscore_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "my_gauge"
version = "1.0.0"
description = "underscore not allowed"
"#,
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::InvalidName { name, .. }) => {
            assert_eq!(name, "my_gauge");
        }
        other => panic!("expected InvalidName, got {other:?}"),
    }
}

#[test]
fn invalid_name_space_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "my gauge"
version = "1.0.0"
description = "space not allowed"
"#,
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::InvalidName { name, .. }) => {
            assert_eq!(name, "my gauge");
        }
        other => panic!("expected InvalidName, got {other:?}"),
    }
}

#[test]
fn valid_name_with_hyphens_and_digits_accepted() {
    // Names like "my-widget2" must be accepted.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "my-widget2"
version = "1.0.0"
description = "hyphen and digit are valid"
"#,
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    // No layers in this minimal bundle, but the name is valid so we should NOT get InvalidName.
    // We expect Ok (no layers = valid but empty layer list).
    match result {
        BundleScanResult::Ok(bundle) => {
            assert_eq!(bundle.definition.id, "my-widget2");
        }
        BundleScanResult::Err(BundleError::InvalidName { name, .. }) => {
            panic!("valid name '{name}' was incorrectly rejected");
        }
        BundleScanResult::Err(e) => {
            // Any other error (e.g. MissingSvg) is acceptable — the name was valid.
            let _ = e;
        }
    }
}

#[test]
fn invalid_name_exclamation_mark_rejected() {
    // Regression: 'My Gauge!' must be rejected — uppercase, space, and '!' are all
    // non-conforming characters. Source: hud-qdmf (GAP-3).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "My Gauge!"
version = "1.0.0"
description = "special chars not allowed"
"#,
    )
    .unwrap();

    let result = load_bundle_dir(dir.path());
    match result {
        BundleScanResult::Err(BundleError::InvalidName { name, .. }) => {
            assert_eq!(name, "My Gauge!");
        }
        other => panic!("expected InvalidName, got {other:?}"),
    }
}

#[test]
fn invalid_name_wire_code() {
    let err = BundleError::InvalidName {
        path: "/tmp/test".to_string(),
        name: "Gauge".to_string(),
    };
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_INVALID_NAME");
}

// ─── Token placeholder resolution (bundle-level integration) ─────────────────

/// SVG containing a resolved token placeholder loads successfully; resolved
/// text is stored in svg_contents.
#[test]
fn bundle_with_resolved_token_loads() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "token-test"
version = "1.0.0"
description = "token substitution test"

[[layers]]
svg_file = "fill.svg"
"#,
    )
    .unwrap();
    // SVG uses a token placeholder for the fill colour.
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"bar\" fill=\"{{token.color.primary}}\"/></svg>",
    )
    .unwrap();

    let mut tokens = HashMap::new();
    tokens.insert("color.primary".to_string(), "red".to_string());

    let result = load_bundle_dir_with_tokens(dir.path(), &tokens);
    match result {
        BundleScanResult::Ok(bundle) => {
            // The stored SVG bytes should contain the resolved value, not the placeholder.
            let svg_bytes = bundle.svg_contents.get("fill.svg").unwrap();
            let svg_str = std::str::from_utf8(svg_bytes).unwrap();
            assert!(
                svg_str.contains("red"),
                "resolved token value should be present: {svg_str}"
            );
            assert!(
                !svg_str.contains("{{token.color.primary}}"),
                "placeholder should have been replaced: {svg_str}"
            );
        }
        BundleScanResult::Err(e) => panic!("unexpected error: {e}"),
    }
}

/// SVG with an unresolved token produces `BundleError::UnresolvedToken`.
#[test]
fn bundle_with_unresolved_token_fails() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "unresolved-test"
version = "1.0.0"
description = "unresolved token test"

[[layers]]
svg_file = "fill.svg"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect fill=\"{{token.missing.key}}\"/></svg>",
    )
    .unwrap();

    // Empty token map — placeholder cannot be resolved.
    let result = load_bundle_dir_with_tokens(dir.path(), &HashMap::new());
    match result {
        BundleScanResult::Err(BundleError::UnresolvedToken {
            svg_file,
            token_key,
            ..
        }) => {
            assert_eq!(svg_file, "fill.svg");
            assert_eq!(token_key, "missing.key");
        }
        other => panic!("expected UnresolvedToken, got {other:?}"),
    }
}

/// Wire code for `UnresolvedToken` is `WIDGET_BUNDLE_UNRESOLVED_TOKEN`.
#[test]
fn unresolved_token_wire_code() {
    let err = BundleError::UnresolvedToken {
        path: "/tmp".to_string(),
        svg_file: "fill.svg".to_string(),
        token_key: "color.primary".to_string(),
    };
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_UNRESOLVED_TOKEN");
}

/// `load_bundle_dir` (no-token variant) still works when SVG contains no placeholders.
#[test]
fn load_bundle_dir_no_tokens_no_placeholders() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("widget.toml"),
        br#"name = "plain-test"
version = "1.0.0"
description = "no placeholders"

[[layers]]
svg_file = "fill.svg"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fill.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"bar\"/></svg>",
    )
    .unwrap();

    // load_bundle_dir uses an empty token map internally.
    let result = load_bundle_dir(dir.path());
    assert!(
        matches!(result, BundleScanResult::Ok(_)),
        "plain SVG without placeholders should load fine: {result:?}"
    );
}

// ─── Exemplar gauge bundle (assets/widgets/gauge/) [hud-x5cm] ────────────────
//
// These tests validate the production-quality exemplar gauge bundle that lives
// at assets/widgets/gauge/ (distinct from the unit-test fixture at
// crates/tze_hud_widget/tests/fixtures/gauge/).
//
// The exemplar uses {{token.key}} placeholders in all SVG color/style
// attributes — zero hardcoded hex values.  A canonical token map is required
// to load it successfully.

/// Returns the path to the production exemplar gauge bundle.
/// Located at <workspace>/assets/widgets/gauge/ relative to CARGO_MANIFEST_DIR.
fn exemplar_gauge_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..") // crates/
        .join("..") // workspace root
        .join("assets")
        .join("widgets")
        .join("gauge")
}

/// Returns the canonical token map for the production gauge exemplar.
/// Keys match every {{token.key}} placeholder in background.svg and fill.svg.
fn canonical_gauge_tokens() -> HashMap<String, String> {
    let mut tokens = HashMap::new();
    tokens.insert("color.backdrop.default".to_string(), "#000000".to_string());
    tokens.insert("color.border.default".to_string(), "#333333".to_string());
    tokens.insert("color.text.accent".to_string(), "#4A9EFF".to_string());
    tokens.insert("color.text.primary".to_string(), "#FFFFFF".to_string());
    tokens.insert("color.text.secondary".to_string(), "#8F96A3".to_string());
    tokens.insert("color.outline.default".to_string(), "#000000".to_string());
    tokens.insert("border.radius.medium".to_string(), "8".to_string());
    tokens.insert("border.radius.large".to_string(), "16".to_string());
    tokens.insert("stroke.border.width".to_string(), "1".to_string());
    tokens.insert("stroke.outline.width".to_string(), "1".to_string());
    tokens.insert("color.severity.info".to_string(), "#4A9EFF".to_string());
    tokens
}

/// AC-1: The exemplar gauge bundle loads without errors via `load_bundle_dir_with_tokens`.
/// Validates that the production bundle is structurally valid and all token
/// placeholders are resolvable with the canonical token map.
///
/// Source: hud-x5cm §Acceptance criterion 1.
#[test]
fn exemplar_gauge_loads_without_errors() {
    let dir = exemplar_gauge_path();
    let tokens = canonical_gauge_tokens();
    let result = load_bundle_dir_with_tokens(&dir, &tokens);
    match &result {
        BundleScanResult::Ok(_) => {}
        BundleScanResult::Err(e) => {
            panic!("exemplar gauge bundle failed to load: {e}");
        }
    }
}

/// AC-2: Token resolution produces correct values in the stored SVG bytes.
/// After loading with the canonical token map, the resolved SVG contents must
/// contain the expected colour values and must not contain any {{token.*}} placeholders.
///
/// Source: hud-x5cm §Acceptance criterion 2.
#[test]
fn exemplar_gauge_token_resolution_correct() {
    let dir = exemplar_gauge_path();
    let tokens = canonical_gauge_tokens();
    let result = load_bundle_dir_with_tokens(&dir, &tokens);
    let bundle = match result {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("exemplar gauge failed to load: {e}"),
    };

    // background.svg: tokens color.backdrop.default and color.border.default must be resolved.
    let bg = bundle.svg_contents.get("background.svg").unwrap();
    let bg_str = std::str::from_utf8(bg).unwrap();
    assert!(
        bg_str.contains("#000000"),
        "background.svg must contain resolved backdrop color #000000: {bg_str}"
    );
    assert!(
        bg_str.contains("#333333"),
        "background.svg must contain resolved border color #333333: {bg_str}"
    );
    assert!(
        !bg_str.contains("{{token."),
        "background.svg must not contain unresolved token placeholders after substitution: {bg_str}"
    );

    // fill.svg: accent, primary, outline, stroke-width, severity tokens must be resolved.
    let fill = bundle.svg_contents.get("fill.svg").unwrap();
    let fill_str = std::str::from_utf8(fill).unwrap();
    assert!(
        fill_str.contains("#4A9EFF"),
        "fill.svg must contain resolved accent color #4A9EFF: {fill_str}"
    );
    assert!(
        fill_str.contains("#FFFFFF"),
        "fill.svg must contain resolved text.primary color #FFFFFF: {fill_str}"
    );
    assert!(
        !fill_str.contains("{{token."),
        "fill.svg must not contain unresolved token placeholders after substitution: {fill_str}"
    );
}

/// AC-3: The exemplar gauge registers as "gauge" with 6 params, 2 layers, and
/// correct default values for each parameter.
///
/// Source: hud-x5cm §Acceptance criterion 3.
#[test]
fn exemplar_gauge_registration_structure() {
    let dir = exemplar_gauge_path();
    let tokens = canonical_gauge_tokens();
    let result = load_bundle_dir_with_tokens(&dir, &tokens);
    let bundle = match result {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("exemplar gauge failed to load: {e}"),
    };

    let def = &bundle.definition;

    // Widget identity.
    assert_eq!(def.id, "gauge", "widget id must be 'gauge'");
    assert_eq!(def.name, "gauge", "widget name must be 'gauge'");
    assert!(
        !def.description.is_empty(),
        "widget description must not be empty"
    );

    // 6 parameters: level, label, fill_color, severity, tooltip_visible, readout.
    assert_eq!(
        def.parameter_schema.len(),
        6,
        "must have exactly 6 parameters"
    );
    let param_names: Vec<&str> = def
        .parameter_schema
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert!(param_names.contains(&"level"), "must declare 'level' param");
    assert!(param_names.contains(&"label"), "must declare 'label' param");
    assert!(
        param_names.contains(&"fill_color"),
        "must declare 'fill_color' param"
    );
    assert!(
        param_names.contains(&"severity"),
        "must declare 'severity' param"
    );
    assert!(
        param_names.contains(&"tooltip_visible"),
        "must declare 'tooltip_visible' param"
    );
    assert!(
        param_names.contains(&"readout"),
        "must declare 'readout' param"
    );

    // Defaults: level=0.0, label="", fill_color=[74,158,255,255], severity="info".
    let level_param = def
        .parameter_schema
        .iter()
        .find(|p| p.name == "level")
        .unwrap();
    assert!(
        matches!(
            level_param.default_value,
            tze_hud_scene::types::WidgetParameterValue::F32(v) if (v - 0.0_f32).abs() < 1e-6
        ),
        "level default must be 0.0, got {:?}",
        level_param.default_value
    );

    let label_param = def
        .parameter_schema
        .iter()
        .find(|p| p.name == "label")
        .unwrap();
    assert!(
        matches!(
            &label_param.default_value,
            tze_hud_scene::types::WidgetParameterValue::String(s) if s.is_empty()
        ),
        "label default must be empty string, got {:?}",
        label_param.default_value
    );

    let fill_color_param = def
        .parameter_schema
        .iter()
        .find(|p| p.name == "fill_color")
        .unwrap();
    // Default declared as [74, 158, 255, 255] → normalized to [~0.29, ~0.62, ~1.0, 1.0].
    if let tze_hud_scene::types::WidgetParameterValue::Color(c) = fill_color_param.default_value {
        assert!(
            (c.r - 74.0_f32 / 255.0).abs() < 0.005,
            "fill_color default r must be ~{}, got {}",
            74.0_f32 / 255.0,
            c.r
        );
        assert!(
            (c.g - 158.0_f32 / 255.0).abs() < 0.005,
            "fill_color default g must be ~{}, got {}",
            158.0_f32 / 255.0,
            c.g
        );
        assert!(
            (c.b - 255.0_f32 / 255.0).abs() < 0.005,
            "fill_color default b must be ~1.0, got {}",
            c.b
        );
        assert!(
            (c.a - 1.0_f32).abs() < 0.005,
            "fill_color default a must be 1.0, got {}",
            c.a
        );
    } else {
        panic!(
            "fill_color default must be Color, got {:?}",
            fill_color_param.default_value
        );
    }

    let severity_param = def
        .parameter_schema
        .iter()
        .find(|p| p.name == "severity")
        .unwrap();
    assert!(
        matches!(
            &severity_param.default_value,
            tze_hud_scene::types::WidgetParameterValue::Enum(s) if s == "info"
        ),
        "severity default must be 'info', got {:?}",
        severity_param.default_value
    );

    let tooltip_visible_param = def
        .parameter_schema
        .iter()
        .find(|p| p.name == "tooltip_visible")
        .unwrap();
    assert!(
        matches!(
            tooltip_visible_param.default_value,
            tze_hud_scene::types::WidgetParameterValue::F32(v) if (v - 0.0_f32).abs() < 1e-6
        ),
        "tooltip_visible default must be 0.0, got {:?}",
        tooltip_visible_param.default_value
    );

    let readout_param = def
        .parameter_schema
        .iter()
        .find(|p| p.name == "readout")
        .unwrap();
    assert!(
        matches!(
            &readout_param.default_value,
            tze_hud_scene::types::WidgetParameterValue::String(s) if s == "0/100 (0%)"
        ),
        "readout default must be '0/100 (0%)', got {:?}",
        readout_param.default_value
    );

    // 2 layers: background.svg (no bindings) and fill.svg (7 bindings).
    assert_eq!(def.layers.len(), 2, "must have exactly 2 layers");
    assert_eq!(
        def.layers[0].svg_file, "background.svg",
        "first layer must be background.svg"
    );
    assert_eq!(
        def.layers[1].svg_file, "fill.svg",
        "second layer must be fill.svg"
    );
    assert!(
        def.layers[0].bindings.is_empty(),
        "background.svg must have no bindings"
    );
    assert_eq!(
        def.layers[1].bindings.len(),
        7,
        "fill.svg must have exactly 7 bindings"
    );
}

/// AC-4: Operator token override — supplying color.text.primary=#00FF00 causes
/// the label element's fill in the resolved SVG to be green (#00FF00) instead
/// of the canonical white (#FFFFFF).
///
/// Source: hud-x5cm §Acceptance criterion 4.
#[test]
fn exemplar_gauge_operator_token_override_label_green() {
    let dir = exemplar_gauge_path();

    // Start with the canonical token map, then override color.text.primary.
    let mut tokens = canonical_gauge_tokens();
    tokens.insert("color.text.primary".to_string(), "#00FF00".to_string());

    let result = load_bundle_dir_with_tokens(&dir, &tokens);
    let bundle = match result {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("exemplar gauge failed to load with override: {e}"),
    };

    let fill = bundle.svg_contents.get("fill.svg").unwrap();
    let fill_str = std::str::from_utf8(fill).unwrap();

    assert!(
        fill_str.contains("#00FF00"),
        "fill.svg must contain overridden label color #00FF00: {fill_str}"
    );
    assert!(
        !fill_str.contains("#FFFFFF"),
        "fill.svg must NOT contain the default text.primary #FFFFFF after override: {fill_str}"
    );
    assert!(
        !fill_str.contains("{{token."),
        "fill.svg must have no unresolved token placeholders after override: {fill_str}"
    );
}

/// AC-5: The raw SVG files in the exemplar bundle contain zero hardcoded hex
/// colour values.  All colours must use {{token.key}} placeholder form.
///
/// This test reads the SVG files directly from disk (before token resolution)
/// and checks that no bare #RRGGBB or #RGB literals appear in attribute values.
///
/// Source: hud-x5cm §Acceptance criterion 5.
#[test]
fn exemplar_gauge_svgs_have_no_hardcoded_hex_colors() {
    let dir = exemplar_gauge_path();
    // Regex-free check: scan for patterns matching #[0-9a-fA-F]{3,6} in SVG attribute context.
    // We read the raw bytes so no token substitution occurs.
    for svg_name in &["background.svg", "fill.svg"] {
        let path = dir.join(svg_name);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {svg_name}: {e}"));

        // Scan for any # followed by 3-8 hex digits (covers #RGB, #RRGGBB, #RRGGBBAA).
        // We walk the string looking for '#' outside of comments.
        for (i, ch) in content.char_indices() {
            if ch != '#' {
                continue;
            }
            // Count consecutive hex digits following the '#'.
            let hex_run: String = content[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            // A hex color is 3, 4, 6, or 8 hex digits after '#'.
            if matches!(hex_run.len(), 3 | 4 | 6 | 8) {
                // Allow '#' inside XML comments <!-- ... --> and inside CDATA.
                // Simple heuristic: check the preceding context for "<!--".
                let before = &content[..i];
                let in_comment = before.rfind("<!--").is_some()
                    && before
                        .rfind("-->")
                        .is_none_or(|end| end < before.rfind("<!--").unwrap());
                if !in_comment {
                    panic!(
                        "{svg_name} contains a hardcoded hex color '#{hex_run}' at byte offset \
                         {i}. All colors must use {{{{token.key}}}} placeholders."
                    );
                }
            }
        }
    }
}

/// AC-1 (failure path): Loading the exemplar gauge without any token map must
/// fail with `BundleError::UnresolvedToken` because the SVGs contain
/// {{token.key}} placeholders that cannot be resolved.
///
/// Source: hud-x5cm §Acceptance criterion 1 (negative case).
#[test]
fn exemplar_gauge_fails_without_token_map() {
    let dir = exemplar_gauge_path();
    // Pass an empty token map — placeholders cannot be resolved.
    let result = load_bundle_dir_with_tokens(&dir, &HashMap::new());
    match result {
        BundleScanResult::Err(BundleError::UnresolvedToken { .. }) => {
            // Expected: at least one token placeholder was unresolvable.
        }
        other => panic!(
            "expected UnresolvedToken when loading exemplar gauge without tokens, got {other:?}"
        ),
    }
}

// ─── Production status-indicator bundle (assets/widget_bundles/status-indicator/) ──

/// Returns the path to the production status-indicator bundle.
/// Located at <workspace>/assets/widget_bundles/status-indicator/ relative to
/// CARGO_MANIFEST_DIR.
fn production_status_indicator_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..") // crates/
        .join("..") // workspace root
        .join("assets")
        .join("widget_bundles")
        .join("status-indicator")
}

/// Returns a minimal token map covering every {{token.key}} placeholder in
/// the production status-indicator SVG.
fn production_status_indicator_tokens() -> HashMap<String, String> {
    let mut tokens = HashMap::new();
    tokens.insert("color.border.default".to_string(), "#333333".to_string());
    tokens.insert("color.text.secondary".to_string(), "#B0B0B0".to_string());
    tokens.insert("color.outline.default".to_string(), "#000000".to_string());
    tokens
}

/// The production status-indicator bundle loads without errors using the
/// canonical token map.  Validates structural integrity and token resolution.
#[test]
fn production_status_indicator_loads_without_errors() {
    let dir = production_status_indicator_path();
    let tokens = production_status_indicator_tokens();
    let result = load_bundle_dir_with_tokens(&dir, &tokens);
    match &result {
        BundleScanResult::Ok(_) => {}
        BundleScanResult::Err(e) => {
            panic!("production status-indicator bundle failed to load: {e}");
        }
    }
}

/// After loading with the canonical token map, all {{token.*}} placeholders
/// must be replaced and the expected resolved values must appear in the SVG.
#[test]
fn production_status_indicator_token_resolution_correct() {
    let dir = production_status_indicator_path();
    let tokens = production_status_indicator_tokens();
    let bundle = match load_bundle_dir_with_tokens(&dir, &tokens) {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("production status-indicator failed to load: {e}"),
    };

    let svg_bytes = bundle
        .svg_contents
        .get("indicator.svg")
        .expect("indicator.svg must be in svg_contents");
    let svg_text = std::str::from_utf8(svg_bytes).expect("SVG must be valid UTF-8");

    // No unresolved placeholders in non-comment SVG content.
    // Strip XML comments first: <!-- ... --> before checking for {{token.
    let svg_no_comments = {
        let mut out = String::new();
        let mut rest = svg_text;
        while let Some(start) = rest.find("<!--") {
            out.push_str(&rest[..start]);
            if let Some(end) = rest[start..].find("-->") {
                rest = &rest[start + end + 3..];
            } else {
                break;
            }
        }
        out.push_str(rest);
        out
    };
    assert!(
        !svg_no_comments.contains("{{token."),
        "indicator.svg must not contain unresolved token placeholders outside comments: {svg_text}"
    );

    // Resolved border color must appear (stroke attribute on the circle).
    assert!(
        svg_text.contains("#333333"),
        "indicator.svg must contain resolved border color #333333: {svg_text}"
    );
}

/// The production status-indicator bundle registers as "status-indicator" with
/// 5 parameters (status, theme, label, reason, tooltip_visible), 1 layer
/// (indicator.svg), and 19 bindings.
#[test]
fn production_status_indicator_registration_structure() {
    let dir = production_status_indicator_path();
    let tokens = production_status_indicator_tokens();
    let bundle = match load_bundle_dir_with_tokens(&dir, &tokens) {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("production status-indicator failed to load: {e}"),
    };

    let def = &bundle.definition;

    assert_eq!(
        def.name, "status-indicator",
        "widget name must be 'status-indicator'"
    );

    // 5 parameters: status/theme (enum), label/reason (string), tooltip_visible (f32).
    assert_eq!(
        def.parameter_schema.len(),
        5,
        "must have exactly 5 parameters"
    );
    let param_names: Vec<&str> = def
        .parameter_schema
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert!(
        param_names.contains(&"status"),
        "must declare 'status' param"
    );
    assert!(param_names.contains(&"theme"), "must declare 'theme' param");
    assert!(param_names.contains(&"label"), "must declare 'label' param");
    assert!(
        param_names.contains(&"reason"),
        "must declare 'reason' param"
    );
    assert!(
        param_names.contains(&"tooltip_visible"),
        "must declare 'tooltip_visible' param"
    );

    // 1 layer: indicator.svg with 19 bindings.
    assert_eq!(def.layers.len(), 1, "must have exactly 1 layer");
    assert_eq!(
        def.layers[0].svg_file, "indicator.svg",
        "layer must be indicator.svg"
    );
    assert_eq!(
        def.layers[0].bindings.len(),
        19,
        "indicator.svg must have exactly 19 bindings"
    );
}

/// The discrete value_map for the "status" binding must contain all four
/// canonical state colors (online, away, busy, offline).
#[test]
fn production_status_indicator_discrete_value_map_complete() {
    use tze_hud_scene::types::WidgetBindingMapping;

    let dir = production_status_indicator_path();
    let tokens = production_status_indicator_tokens();
    let bundle = match load_bundle_dir_with_tokens(&dir, &tokens) {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("production status-indicator failed to load: {e}"),
    };

    let layer = &bundle.definition.layers[0];
    let status_binding = layer
        .bindings
        .iter()
        .find(|b| b.param == "status" && b.target_element == "system-fill")
        .expect("status/system-fill binding must exist in indicator.svg layer");

    let value_map = match &status_binding.mapping {
        WidgetBindingMapping::Discrete { value_map } => value_map,
        other => panic!("status binding must be Discrete, got {other:?}"),
    };

    assert_eq!(
        value_map.get("online").map(|s| s.as_str()),
        Some("#4FB543"),
        "online → #4FB543"
    );
    assert_eq!(
        value_map.get("away").map(|s| s.as_str()),
        Some("#D97706"),
        "away → #D97706"
    );
    assert_eq!(
        value_map.get("busy").map(|s| s.as_str()),
        Some("#DC2626"),
        "busy → #DC2626"
    );
    assert_eq!(
        value_map.get("offline").map(|s| s.as_str()),
        Some("#6B7280"),
        "offline → #6B7280"
    );
}
