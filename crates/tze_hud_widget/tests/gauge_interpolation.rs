//! Interpolation behavior tests for the production gauge widget bundle.
//!
//! Verifies the compositor's "smooth at 60fps" promise: f32 and color parameters
//! interpolate smoothly over `transition_ms`, while string and enum parameters
//! snap immediately.
//!
//! This covers openspec task 7 (Interpolation Tests), acceptance criteria:
//!   7.1 — f32 linear interpolation (level): midpoint at t=0.5 = 0.5; bar height 100px
//!   7.2 — Color component-wise sRGB interpolation (fill_color): midpoint per-channel
//!   7.3 — String snap (label): immediate at t=0, transition_ms ignored
//!   7.4 — Enum snap (severity): immediate at t=0, transition_ms ignored
//!   7.5 — Zero transition: all params in one frame, final values immediately available
//!   7.6 — Transition interruption: new publish restarts from current effective value
//!         Mixed snap + interpolate in single publish
//!
//! Strategy: use `interpolate_param` (pure) at controlled `t` values to verify
//! interpolation semantics, then confirm with `resolve_binding_value` that SVG
//! attribute output matches the spec formulas.  Uses the production gauge bundle
//! with token stubs; no GPU required.
//!
//! Source: widget-system/spec.md §Requirement: Widget Parameter Interpolation.
//! [hud-w17j]

use std::collections::HashMap;
use std::path::PathBuf;

use tze_hud_compositor::widget::{compute_transition_t, interpolate_param, resolve_binding_value};
use tze_hud_scene::DegradationLevel;
use tze_hud_scene::SceneId;
use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, RenderingPolicy, Rgba, WidgetInstance, WidgetParameterValue,
};
use tze_hud_widget::loader::{BundleScanResult, load_bundle_dir_with_tokens};

// ─── Fixture helpers ──────────────────────────────────────────────────────────

/// Minimal token map resolving all `{{token.*}}` placeholders in the production
/// gauge SVG files.  Values are for test isolation only — visual fidelity is not
/// required; we need the loader to succeed so that parameter semantics can be
/// tested.
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

/// Path to the production gauge bundle (`assets/widget_bundles/gauge/`).
///
/// `CARGO_MANIFEST_DIR` is `crates/tze_hud_widget/`; go up two levels to the
/// workspace root, then descend into `assets/widget_bundles/gauge/`.
fn production_gauge_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("assets")
        .join("widget_bundles")
        .join("gauge")
}

/// Load the production gauge bundle.  Panics on load failure.
fn load_gauge_bundle() -> tze_hud_widget::loader::LoadedBundle {
    let path = production_gauge_path();
    let tokens = gauge_test_tokens();
    match load_bundle_dir_with_tokens(&path, &tokens) {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("production gauge bundle failed to load: {e}"),
    }
}

/// Build a `SceneGraph` with the production gauge definition and one "gauge"
/// instance registered.  Returns `(scene, tab_id)`.
fn scene_with_production_gauge() -> (tze_hud_scene::SceneGraph, tze_hud_scene::types::SceneId) {
    let bundle = load_gauge_bundle();
    let mut definition = bundle.definition.clone();
    definition.default_contention_policy = ContentionPolicy::LatestWins;
    definition.default_rendering_policy = RenderingPolicy::default();
    definition.default_geometry_policy = GeometryPolicy::Relative {
        x_pct: 0.0,
        y_pct: 0.0,
        width_pct: 0.25,
        height_pct: 0.25,
    };

    let mut scene = tze_hud_scene::SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.widget_registry.register_definition(definition);

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

/// Build the f32 constraint map for the level parameter (min=0.0, max=1.0).
fn level_constraints() -> HashMap<String, (f32, f32)> {
    HashMap::from([("level".to_string(), (0.0f32, 1.0f32))])
}

/// Look up the `bar` height binding from the production gauge fill layer.
fn bar_height_binding(
    bundle: &tze_hud_widget::loader::LoadedBundle,
) -> tze_hud_scene::types::WidgetBinding {
    let fill_layer = bundle
        .definition
        .layers
        .iter()
        .find(|l| l.svg_file == "fill.svg")
        .expect("fill.svg layer should exist");
    fill_layer
        .bindings
        .iter()
        .find(|b| b.param == "level" && b.target_attribute == "height")
        .cloned()
        .expect("level→height binding should exist in fill.svg layer")
}

/// Look up the `bar` fill binding (fill_color → fill).
fn bar_fill_binding(
    bundle: &tze_hud_widget::loader::LoadedBundle,
) -> tze_hud_scene::types::WidgetBinding {
    let fill_layer = bundle
        .definition
        .layers
        .iter()
        .find(|l| l.svg_file == "fill.svg")
        .expect("fill.svg layer should exist");
    fill_layer
        .bindings
        .iter()
        .find(|b| b.param == "fill_color")
        .cloned()
        .expect("fill_color binding should exist in fill.svg layer")
}

/// Look up the `label-text` text-content binding.
fn label_binding(
    bundle: &tze_hud_widget::loader::LoadedBundle,
) -> tze_hud_scene::types::WidgetBinding {
    let fill_layer = bundle
        .definition
        .layers
        .iter()
        .find(|l| l.svg_file == "fill.svg")
        .expect("fill.svg layer should exist");
    fill_layer
        .bindings
        .iter()
        .find(|b| b.param == "label")
        .cloned()
        .expect("label binding should exist in fill.svg layer")
}

/// Look up the `indicator` severity binding.
fn severity_binding(
    bundle: &tze_hud_widget::loader::LoadedBundle,
) -> tze_hud_scene::types::WidgetBinding {
    let fill_layer = bundle
        .definition
        .layers
        .iter()
        .find(|l| l.svg_file == "fill.svg")
        .expect("fill.svg layer should exist");
    fill_layer
        .bindings
        .iter()
        .find(|b| b.param == "severity")
        .cloned()
        .expect("severity binding should exist in fill.svg layer")
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7.1 — f32 LINEAR INTERPOLATION (level)
// ═══════════════════════════════════════════════════════════════════════════════

/// At the start of a level transition (t=0), the effective level equals the old
/// value (0.2).
///
/// Formula: value(t) = old + (new - old) * t
/// At t=0: 0.2 + (0.8 - 0.2) * 0.0 = 0.2
/// [hud-w17j]
#[test]
fn f32_level_interpolation_at_t0_equals_start() {
    let old_level = WidgetParameterValue::F32(0.2);
    let new_level = WidgetParameterValue::F32(0.8);

    let effective = interpolate_param(&old_level, &new_level, 0.0);

    match effective {
        WidgetParameterValue::F32(v) => {
            assert!(
                (v - 0.2).abs() < 1e-6,
                "at t=0 effective level should be 0.2 (start), got {v}"
            );
        }
        other => panic!("expected F32 at t=0, got {other:?}"),
    }
}

/// At the midpoint of a level transition (t=0.5), the effective level is 0.5.
///
/// Formula: value(t) = old + (new - old) * t
/// At t=0.5: 0.2 + (0.8 - 0.2) * 0.5 = 0.5
/// Bar height at midpoint: 0.5 * 200.0 = 100.0px
/// [hud-w17j]
#[test]
fn f32_level_interpolation_midpoint_is_05() {
    let old_level = WidgetParameterValue::F32(0.2);
    let new_level = WidgetParameterValue::F32(0.8);

    let t = compute_transition_t(150.0, 300.0, DegradationLevel::Nominal);
    assert!(
        (t - 0.5).abs() < 1e-6,
        "elapsed=150ms / duration=300ms should give t=0.5, got {t}"
    );

    let effective = interpolate_param(&old_level, &new_level, t);

    match effective {
        WidgetParameterValue::F32(v) => {
            assert!(
                (v - 0.5).abs() < 1e-4,
                "at t=0.5 effective level should be 0.5 (midpoint), got {v}"
            );
        }
        other => panic!("expected F32 at midpoint, got {other:?}"),
    }
}

/// At the midpoint, the bar height binding resolves to "100" (0.5 * 200.0 = 100px).
///
/// Source: widget-system/spec.md §Requirement: Widget Compositor Rendering —
/// bar height at midpoint = 0.5 * 200.0 = 100.0px.
/// [hud-w17j]
#[test]
fn f32_level_midpoint_bar_height_is_100px() {
    let bundle = load_gauge_bundle();
    let binding = bar_height_binding(&bundle);
    let constraints = level_constraints();

    let old_level = WidgetParameterValue::F32(0.2);
    let new_level = WidgetParameterValue::F32(0.8);

    // Compute effective level at t=0.5 (elapsed=150ms, duration=300ms)
    let t = compute_transition_t(150.0, 300.0, DegradationLevel::Nominal);
    let effective_level = interpolate_param(&old_level, &new_level, t);

    let mut params = HashMap::new();
    params.insert("level".to_string(), effective_level);

    let height_str = resolve_binding_value(&binding, &params, &constraints)
        .expect("bar height binding should resolve at midpoint");

    // The format rounds integer values: 100.0 → "100"
    assert_eq!(
        height_str, "100",
        "bar height at midpoint should be '100' (100.0px), got {height_str:?}"
    );
}

/// At the end of a level transition (t=1.0), the effective level is the target (0.8).
///
/// Formula: value(t) = old + (new - old) * t
/// At t=1.0: 0.2 + (0.8 - 0.2) * 1.0 = 0.8
/// [hud-w17j]
#[test]
fn f32_level_interpolation_at_t1_equals_target() {
    let old_level = WidgetParameterValue::F32(0.2);
    let new_level = WidgetParameterValue::F32(0.8);

    let t = compute_transition_t(300.0, 300.0, DegradationLevel::Nominal);
    assert!(
        (t - 1.0).abs() < 1e-6,
        "elapsed=300ms / duration=300ms should give t=1.0, got {t}"
    );

    let effective = interpolate_param(&old_level, &new_level, t);

    match effective {
        WidgetParameterValue::F32(v) => {
            assert!(
                (v - 0.8).abs() < 1e-6,
                "at t=1.0 effective level should be 0.8 (target), got {v}"
            );
        }
        other => panic!("expected F32 at t=1.0, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7.2 — COLOR COMPONENT-WISE sRGB INTERPOLATION (fill_color)
// ═══════════════════════════════════════════════════════════════════════════════

/// At the midpoint of a fill_color transition (blue→red, t=0.5), each sRGB
/// channel interpolates independently.
///
/// Blue [0,0,255,255] → Red [255,0,0,255] at t=0.5:
///   R: 0 + (255 - 0) * 0.5 = 127.5 → in f32 [0,1]: 0 + (1-0)*0.5 = 0.5
///   G: unchanged at 0.0
///   B: 1.0 + (0 - 1.0) * 0.5 = 0.5
///   A: 1.0 (unchanged)
/// [hud-w17j]
#[test]
fn color_fill_color_midpoint_is_component_wise() {
    let old_color = WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, 1.0)); // blue
    let new_color = WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)); // red

    let t = compute_transition_t(150.0, 300.0, DegradationLevel::Nominal);
    let effective = interpolate_param(&old_color, &new_color, t);

    match effective {
        WidgetParameterValue::Color(c) => {
            assert!(
                (c.r - 0.5).abs() < 1e-4,
                "R channel at midpoint should be 0.5 (≈128/255), got {:.4}",
                c.r
            );
            assert!(
                c.g.abs() < 1e-6,
                "G channel should be 0.0 throughout blue→red transition, got {}",
                c.g
            );
            assert!(
                (c.b - 0.5).abs() < 1e-4,
                "B channel at midpoint should be 0.5 (≈128/255), got {:.4}",
                c.b
            );
            assert!(
                (c.a - 1.0).abs() < 1e-6,
                "A channel should remain 1.0 (both endpoints opaque), got {}",
                c.a
            );
        }
        other => panic!("expected Color at midpoint, got {other:?}"),
    }
}

/// Alpha channel also interpolates component-wise.
///
/// Blue semi-transparent [0,0,255,128] → Red opaque [255,0,0,255] at t=0.5:
///   R: 0.0 + (1.0 - 0.0) * 0.5 = 0.5
///   A: (128/255) + (1.0 - 128/255) * 0.5 ≈ 0.75
/// [hud-w17j]
#[test]
fn color_alpha_channel_interpolates_independently() {
    let alpha_start = 128.0f32 / 255.0; // ≈ 0.502
    let old_color = WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, alpha_start));
    let new_color = WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0));

    let effective = interpolate_param(&old_color, &new_color, 0.5);

    match effective {
        WidgetParameterValue::Color(c) => {
            let expected_a = alpha_start + (1.0 - alpha_start) * 0.5;
            assert!(
                (c.a - expected_a).abs() < 1e-4,
                "A channel should interpolate: expected {expected_a:.4}, got {:.4}",
                c.a
            );
        }
        other => panic!("expected Color for alpha interpolation test, got {other:?}"),
    }
}

/// At t=0 the fill_color binding resolves to the old (start) color exactly.
/// [hud-w17j]
#[test]
fn color_fill_color_at_t0_equals_start_color() {
    let old_color = WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, 1.0)); // blue
    let new_color = WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)); // red

    let effective = interpolate_param(&old_color, &new_color, 0.0);

    match effective {
        WidgetParameterValue::Color(c) => {
            assert!(
                (c.r - 0.0).abs() < 1e-6,
                "R should be 0.0 at t=0, got {}",
                c.r
            );
            assert!(
                (c.b - 1.0).abs() < 1e-6,
                "B should be 1.0 at t=0, got {}",
                c.b
            );
        }
        other => panic!("expected Color at t=0, got {other:?}"),
    }
}

/// The fill layer bar-fill binding resolves to a midpoint color string for a
/// blue→red transition at t=0.5.
///
/// At midpoint, r=0.5 (≈128), g=0, b=0.5 (≈128), a=1.0 → "#800080" or similar.
/// [hud-w17j]
#[test]
fn color_fill_color_midpoint_bar_fill_svg_attribute() {
    let bundle = load_gauge_bundle();
    let binding = bar_fill_binding(&bundle);

    let old_color = WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, 1.0)); // blue
    let new_color = WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)); // red

    // Midpoint
    let effective = interpolate_param(&old_color, &new_color, 0.5);

    let mut params = HashMap::new();
    params.insert("fill_color".to_string(), effective.clone());

    let fill_str = resolve_binding_value(&binding, &params, &HashMap::new())
        .expect("fill_color binding should resolve at midpoint");

    // Midpoint color r≈128, g=0, b≈128 → "#800080" (purple)
    // We verify it's not the start (#0000ff) and not the end (#ff0000).
    assert_ne!(
        fill_str, "#0000ff",
        "midpoint fill should not equal start color #0000ff, got {fill_str:?}"
    );
    assert_ne!(
        fill_str, "#ff0000",
        "midpoint fill should not equal end color #ff0000, got {fill_str:?}"
    );

    // Verify it IS exactly the expected midpoint hex color "#800080".
    // At t=0.5: R channel = round(0.5 * 255) = 128 = 0x80; B = 0x80; G = 0x00.
    assert_eq!(
        fill_str, "#800080",
        "midpoint fill should be '#800080' (purple: r=0x80, g=0x00, b=0x80); got {fill_str:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7.3 — STRING SNAP (label)
// ═══════════════════════════════════════════════════════════════════════════════

/// When label changes from "CPU" to "Memory" with transition_ms=500, the label
/// snaps immediately at t=0 — no character interpolation.
/// [hud-w17j]
#[test]
fn string_label_snaps_immediately_at_t0() {
    let old_label = WidgetParameterValue::String("CPU".to_string());
    let new_label = WidgetParameterValue::String("Memory".to_string());

    // t=0 (transition just started)
    let effective = interpolate_param(&old_label, &new_label, 0.0);

    assert_eq!(
        effective,
        WidgetParameterValue::String("Memory".to_string()),
        "string label should snap to 'Memory' immediately at t=0 (not 'CPU')"
    );
}

/// String snap holds at any t value, including mid-transition (t=0.5) and
/// near-zero (t=0.001).  transition_ms is honored for f32/color co-publishes
/// but the string itself never interpolates.
/// [hud-w17j]
#[test]
fn string_label_snaps_at_any_t_value() {
    let old_label = WidgetParameterValue::String("CPU".to_string());
    let new_label = WidgetParameterValue::String("Memory".to_string());

    for t in [0.0f32, 0.001, 0.1, 0.5, 0.99, 1.0] {
        let effective = interpolate_param(&old_label, &new_label, t);
        assert_eq!(
            effective,
            WidgetParameterValue::String("Memory".to_string()),
            "string label should snap to 'Memory' at t={t}, got {effective:?}"
        );
    }
}

/// The label text-content SVG attribute reflects the snapped value immediately.
///
/// Even when transition_ms=500 is specified for a co-published f32 param, the
/// label binding resolves the final value "Memory" at t=0.
/// [hud-w17j]
#[test]
fn string_label_snap_reflected_in_svg_attribute() {
    let bundle = load_gauge_bundle();
    let binding = label_binding(&bundle);

    let old_label = WidgetParameterValue::String("CPU".to_string());
    let new_label = WidgetParameterValue::String("Memory".to_string());

    // Even at very small t (beginning of a 500ms transition), snap occurs.
    let t_near_start = compute_transition_t(1.0, 500.0, DegradationLevel::Nominal); // ~0.002
    let effective = interpolate_param(&old_label, &new_label, t_near_start);

    let mut params = HashMap::new();
    params.insert("label".to_string(), effective);

    let text_val = resolve_binding_value(&binding, &params, &HashMap::new())
        .expect("label text-content binding should resolve");

    assert_eq!(
        text_val, "Memory",
        "label text-content binding should resolve to 'Memory' immediately, got {text_val:?}"
    );
}

/// Publish label via SceneGraph and verify the stored value is the new label
/// immediately (no deferred application).
/// [hud-w17j]
#[test]
fn string_label_publish_stores_new_value_immediately() {
    let (mut scene, _tab) = scene_with_production_gauge();

    // First publish: set label="CPU"
    let params_cpu = HashMap::from([(
        "label".to_string(),
        WidgetParameterValue::String("CPU".to_string()),
    )]);
    scene
        .publish_to_widget("gauge", params_cpu, "agent.test", None, 500, None)
        .expect("label='CPU' should be accepted");

    // Second publish: label="Memory" with same transition_ms=500
    let params_mem = HashMap::from([(
        "label".to_string(),
        WidgetParameterValue::String("Memory".to_string()),
    )]);
    scene
        .publish_to_widget("gauge", params_mem, "agent.test", None, 500, None)
        .expect("label='Memory' should be accepted");

    // LatestWins: only one publication is active.
    let pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        pubs.len(),
        1,
        "LatestWins: exactly one publication expected"
    );

    match pubs[0].params.get("label") {
        Some(WidgetParameterValue::String(s)) => {
            assert_eq!(
                s, "Memory",
                "stored label should be 'Memory' immediately (snap), got {s:?}"
            );
        }
        other => panic!("expected String(\"Memory\") for label after snap, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7.4 — ENUM SNAP (severity)
// ═══════════════════════════════════════════════════════════════════════════════

/// When severity changes from "info" to "error" with transition_ms=500, the
/// indicator fill snaps to #FF4444 immediately at t=0.
///
/// Contrast: fill_color (direct color binding) interpolates; severity (discrete
/// enum binding) snaps — even when transition_ms > 0.
/// [hud-w17j]
#[test]
fn enum_severity_snaps_immediately_at_t0() {
    let old_severity = WidgetParameterValue::Enum("info".to_string());
    let new_severity = WidgetParameterValue::Enum("error".to_string());

    // t=0 (start of a 500ms transition)
    let effective = interpolate_param(&old_severity, &new_severity, 0.0);

    assert_eq!(
        effective,
        WidgetParameterValue::Enum("error".to_string()),
        "severity enum should snap to 'error' immediately at t=0 (not 'info')"
    );
}

/// Enum snap holds at all t values throughout a nominal 500ms transition.
/// [hud-w17j]
#[test]
fn enum_severity_snaps_at_any_t_value() {
    let old_severity = WidgetParameterValue::Enum("info".to_string());
    let new_severity = WidgetParameterValue::Enum("error".to_string());

    for t in [0.0f32, 0.001, 0.1, 0.5, 0.99, 1.0] {
        let effective = interpolate_param(&old_severity, &new_severity, t);
        assert_eq!(
            effective,
            WidgetParameterValue::Enum("error".to_string()),
            "severity enum should snap to 'error' at t={t}, got {effective:?}"
        );
    }
}

/// The severity discrete binding resolves to #FF4444 immediately, even when
/// called at a small t close to zero (representing the frame after publish).
///
/// Source: widget.toml: severity.error = "#FF4444"
/// [hud-w17j]
#[test]
fn enum_severity_snap_indicator_fill_is_ff4444() {
    let bundle = load_gauge_bundle();
    let binding = severity_binding(&bundle);

    let old_severity = WidgetParameterValue::Enum("info".to_string());
    let new_severity = WidgetParameterValue::Enum("error".to_string());

    // Near t=0 (first frame of a 500ms transition)
    let t_near_start = compute_transition_t(1.0, 500.0, DegradationLevel::Nominal);
    let effective = interpolate_param(&old_severity, &new_severity, t_near_start);

    let mut params = HashMap::new();
    params.insert("severity".to_string(), effective);

    let fill_str = resolve_binding_value(&binding, &params, &HashMap::new())
        .expect("severity discrete binding should resolve");

    assert_eq!(
        fill_str, "#FF4444",
        "indicator fill should be #FF4444 (error) immediately, even at t~0; got {fill_str:?}"
    );
}

/// Direct color fill_color DOES interpolate — contrast with severity enum snap.
///
/// Both transitions run concurrently with transition_ms=500.  At t≈0.5:
/// - severity indicator snaps to error (#FF4444) immediately at t=0
/// - fill_color bar still interpolating (not yet at final red)
///
/// [hud-w17j]
#[test]
fn direct_color_interpolates_while_enum_snaps_in_same_transition() {
    let bundle = load_gauge_bundle();
    let severity_b = severity_binding(&bundle);
    let fill_b = bar_fill_binding(&bundle);

    // Transition: severity info→error (discrete snap), fill_color blue→red (interpolate)
    let old_severity = WidgetParameterValue::Enum("info".to_string());
    let new_severity = WidgetParameterValue::Enum("error".to_string());
    let old_fill = WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, 1.0)); // blue
    let new_fill = WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)); // red

    let t_mid = 0.5f32;

    let effective_severity = interpolate_param(&old_severity, &new_severity, t_mid);
    let effective_fill = interpolate_param(&old_fill, &new_fill, t_mid);

    // Severity: already error at t=0.5 (snap behavior)
    let mut sev_params = HashMap::new();
    sev_params.insert("severity".to_string(), effective_severity);
    let sev_fill = resolve_binding_value(&severity_b, &sev_params, &HashMap::new())
        .expect("severity binding should resolve");
    assert_eq!(
        sev_fill, "#FF4444",
        "severity snap at t=0.5 should be #FF4444"
    );

    // fill_color: still mid-transition (not yet red)
    let mut fill_params = HashMap::new();
    fill_params.insert("fill_color".to_string(), effective_fill);
    let fill_color_str = resolve_binding_value(&fill_b, &fill_params, &HashMap::new())
        .expect("fill_color binding should resolve");
    // Mid-transition fill is NOT pure red
    assert_ne!(
        fill_color_str, "#ff0000",
        "fill_color at t=0.5 should not be pure red (still interpolating); got {fill_color_str:?}"
    );
    assert_ne!(
        fill_color_str, "#0000ff",
        "fill_color at t=0.5 should not be starting blue (already moved); got {fill_color_str:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7.5 — ZERO TRANSITION (all params in one frame)
// ═══════════════════════════════════════════════════════════════════════════════

/// At t=1.0, `interpolate_param` returns the target (final) value for all
/// continuous param types.  This models the fully-elapsed end-of-transition
/// state where the compositor settles params to their final values.
/// [hud-w17j]
#[test]
fn interpolation_at_t1_yields_final_value() {
    // f32: end of transition → final value
    let f32_old = WidgetParameterValue::F32(0.0);
    let f32_final = WidgetParameterValue::F32(0.9);
    let snapped = interpolate_param(&f32_old, &f32_final, 1.0);
    assert_eq!(
        snapped,
        WidgetParameterValue::F32(0.9),
        "at t=1.0, f32 level should equal final value 0.9"
    );

    // Color: end of transition → final color
    let color_old = WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, 1.0));
    let color_final = WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0));
    let color_snapped = interpolate_param(&color_old, &color_final, 1.0);
    assert_eq!(
        color_snapped,
        WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)),
        "at t=1.0, fill_color should equal final (red)"
    );
}

/// When transition_ms=0, all published params are available in their final form
/// from the first active publication.  This test publishes level, label, and
/// severity simultaneously with transition_ms=0 and verifies all three are
/// stored at their final values.
/// [hud-w17j]
#[test]
fn zero_transition_all_params_immediately_final() {
    let (mut scene, _tab) = scene_with_production_gauge();

    // Publish level=0.9, label="Disk", severity="warning" with transition_ms=0.
    let params = HashMap::from([
        ("level".to_string(), WidgetParameterValue::F32(0.9)),
        (
            "label".to_string(),
            WidgetParameterValue::String("Disk".to_string()),
        ),
        (
            "severity".to_string(),
            WidgetParameterValue::Enum("warning".to_string()),
        ),
    ]);
    scene
        .publish_to_widget("gauge", params, "agent.test", None, 0, None)
        .expect("zero-transition all-params publish should be accepted");

    let pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(pubs.len(), 1, "LatestWins: exactly one publication");
    assert_eq!(
        pubs[0].transition_ms, 0,
        "transition_ms should be 0 in the record"
    );

    match pubs[0].params.get("level") {
        Some(WidgetParameterValue::F32(v)) => {
            assert!(
                (v - 0.9).abs() < 1e-6,
                "level should be 0.9 immediately, got {v}"
            );
        }
        other => panic!("expected F32(0.9) for level, got {other:?}"),
    }

    match pubs[0].params.get("label") {
        Some(WidgetParameterValue::String(s)) => {
            assert_eq!(s, "Disk", "label should be 'Disk' immediately");
        }
        other => panic!("expected String('Disk') for label, got {other:?}"),
    }

    match pubs[0].params.get("severity") {
        Some(WidgetParameterValue::Enum(s)) => {
            assert_eq!(s, "warning", "severity should be 'warning' immediately");
        }
        other => panic!("expected Enum('warning') for severity, got {other:?}"),
    }
}

/// Zero-transition publish produces a single active publication record.
/// Verifies that three separate params published together with transition_ms=0
/// result in exactly one publication, not three.
///
/// The compositor should only need one rasterize pass for this publish.
/// [hud-w17j]
#[test]
fn zero_transition_produces_single_publication_record() {
    let (mut scene, _tab) = scene_with_production_gauge();

    let params = HashMap::from([
        ("level".to_string(), WidgetParameterValue::F32(0.9)),
        (
            "label".to_string(),
            WidgetParameterValue::String("Disk".to_string()),
        ),
        (
            "severity".to_string(),
            WidgetParameterValue::Enum("warning".to_string()),
        ),
    ]);
    scene
        .publish_to_widget("gauge", params, "agent.test", None, 0, None)
        .expect("zero-transition publish should succeed");

    let pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        pubs.len(),
        1,
        "zero-transition publish of 3 params should produce exactly one publication record (not 3)"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7.6 — TRANSITION INTERRUPTION
// ═══════════════════════════════════════════════════════════════════════════════

/// When a new publish interrupts an in-progress transition, interpolation
/// restarts from the current effective value toward the new target.
///
/// Scenario:
///   - Start: level=0.0 (default)
///   - Publish A: level=0.8 with transition_ms=300
///   - At t=100ms (effective level ≈ 0.267)
///   - Publish B: level=0.4 with transition_ms=300
///   - Interpolation restarts from 0.267 toward 0.4
///   - The restart should not overshoot or jump
///
/// [hud-w17j]
#[test]
fn transition_interruption_restarts_from_current_effective_value() {
    // Phase 1: Publish A is level 0.0 → 0.8 at t=100ms/300ms ≈ 0.333
    let level_before_a = WidgetParameterValue::F32(0.0);
    let level_target_a = WidgetParameterValue::F32(0.8);

    let t_a = compute_transition_t(100.0, 300.0, DegradationLevel::Nominal);
    assert!(
        (t_a - (100.0f32 / 300.0)).abs() < 1e-5,
        "t_a should be 100/300 ≈ 0.333, got {t_a}"
    );

    let effective_at_interrupt = interpolate_param(&level_before_a, &level_target_a, t_a);
    let level_at_interrupt = match effective_at_interrupt {
        WidgetParameterValue::F32(v) => v,
        other => panic!("expected F32 for effective level at interrupt, got {other:?}"),
    };

    // Expected: 0.0 + (0.8 - 0.0) * (100/300) ≈ 0.267
    let expected_at_interrupt = 0.0f32 + (0.8 - 0.0) * (100.0 / 300.0);
    assert!(
        (level_at_interrupt - expected_at_interrupt).abs() < 1e-4,
        "effective level at interruption should be ≈{expected_at_interrupt:.4}, got {level_at_interrupt:.4}"
    );

    // Phase 2: Publish B interrupts — new transition from current effective → 0.4
    let level_restart_from = WidgetParameterValue::F32(level_at_interrupt);
    let level_target_b = WidgetParameterValue::F32(0.4);

    // Immediately after restart (t=0): effective level = level_at_interrupt (no jump)
    let effective_at_restart = interpolate_param(&level_restart_from, &level_target_b, 0.0);
    match effective_at_restart {
        WidgetParameterValue::F32(v) => {
            assert!(
                (v - level_at_interrupt).abs() < 1e-6,
                "at t=0 of restarted transition, level should equal the interruption value {level_at_interrupt:.4}, got {v:.4}"
            );
        }
        other => panic!("expected F32 at restart t=0, got {other:?}"),
    }

    // Midpoint of new transition (t=0.5): interpolates from interrupt value toward 0.4
    let effective_mid_b = interpolate_param(&level_restart_from, &level_target_b, 0.5);
    let expected_mid_b = level_at_interrupt + (0.4 - level_at_interrupt) * 0.5;
    match effective_mid_b {
        WidgetParameterValue::F32(v) => {
            assert!(
                (v - expected_mid_b).abs() < 1e-4,
                "at midpoint of restarted transition, level should be ≈{expected_mid_b:.4}, got {v:.4}"
            );
            // Must not overshoot target (0.4) or undershoot interrupt value
            assert!(
                v >= level_at_interrupt - 1e-6,
                "level should not drop below interrupt value {level_at_interrupt:.4} (no overshoot), got {v:.4}"
            );
            assert!(
                v <= 0.4 + 1e-6,
                "level should not exceed target 0.4 (no overshoot), got {v:.4}"
            );
        }
        other => panic!("expected F32 at midpoint of restart, got {other:?}"),
    }

    // End of new transition (t=1.0): reaches target 0.4
    let effective_end_b = interpolate_param(&level_restart_from, &level_target_b, 1.0);
    match effective_end_b {
        WidgetParameterValue::F32(v) => {
            assert!(
                (v - 0.4).abs() < 1e-6,
                "at end of restarted transition, level should reach target 0.4, got {v:.4}"
            );
        }
        other => panic!("expected F32 at end of restart, got {other:?}"),
    }
}

/// The LatestWins contention policy ensures that interrupting publishes atomically
/// replace the previous publication.  The new transition_ms is stored in the new
/// record, not inherited from the interrupted one.
/// [hud-w17j]
#[test]
fn transition_interruption_new_record_has_new_transition_ms() {
    let (mut scene, _tab) = scene_with_production_gauge();

    // Publish A: level=0.8 with transition_ms=300
    let params_a = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.8))]);
    scene
        .publish_to_widget("gauge", params_a, "agent.test", None, 300, None)
        .expect("publish A should succeed");

    let pubs_after_a = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(pubs_after_a.len(), 1);
    assert_eq!(
        pubs_after_a[0].transition_ms, 300,
        "publish A should record transition_ms=300"
    );

    // Publish B interrupts: level=0.4 with a DIFFERENT transition_ms=500 to
    // verify the stored value comes from publish B, not inherited from publish A.
    let params_b = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.4))]);
    scene
        .publish_to_widget("gauge", params_b, "agent.test", None, 500, None)
        .expect("publish B (interruption) should succeed");

    let pubs_after_b = scene.widget_registry.active_for_widget("gauge");
    // LatestWins: only one record after the interruption
    assert_eq!(
        pubs_after_b.len(),
        1,
        "LatestWins: exactly one record after interruption"
    );
    assert_eq!(
        pubs_after_b[0].transition_ms, 500,
        "interrupting publish B should store its own transition_ms=500 (not inherited 300 from publish A)"
    );

    match pubs_after_b[0].params.get("level") {
        Some(WidgetParameterValue::F32(v)) => {
            assert!(
                (v - 0.4).abs() < 1e-6,
                "level after interruption should be target 0.4, got {v}"
            );
        }
        other => panic!("expected F32(0.4) after interruption, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MIXED SNAP + INTERPOLATE IN A SINGLE PUBLISH
// ═══════════════════════════════════════════════════════════════════════════════

/// When all four params are published together with transition_ms=300:
/// - label and severity snap at t=0 (immediate)
/// - level and fill_color interpolate over 300ms
///
/// At t=0 (start of transition):
/// - label = "Memory" (snapped, not "CPU")
/// - severity = "warning" (snapped, not "info")
/// - level = old_level (still at start)
/// - fill_color = old_fill_color (still at start)
///
/// [hud-w17j]
#[test]
fn mixed_snap_and_interpolate_label_severity_snap_at_t0() {
    let old_level = WidgetParameterValue::F32(0.0);
    let new_level = WidgetParameterValue::F32(0.8);
    let old_fill = WidgetParameterValue::Color(Rgba::new(0.0, 0.5, 1.0, 1.0)); // cyan-blue
    let new_fill = WidgetParameterValue::Color(Rgba::new(1.0, 0.647, 0.0, 1.0)); // orange
    let old_label = WidgetParameterValue::String("CPU".to_string());
    let new_label = WidgetParameterValue::String("Memory".to_string());
    let old_severity = WidgetParameterValue::Enum("info".to_string());
    let new_severity = WidgetParameterValue::Enum("warning".to_string());

    // t=0 (first frame of a 300ms transition)
    let t = 0.0f32;

    // Snap params: label and severity snap immediately
    let eff_label = interpolate_param(&old_label, &new_label, t);
    let eff_severity = interpolate_param(&old_severity, &new_severity, t);
    // Interpolated params: level and fill_color start at old values
    let eff_level = interpolate_param(&old_level, &new_level, t);
    let eff_fill = interpolate_param(&old_fill, &new_fill, t);

    assert_eq!(
        eff_label,
        WidgetParameterValue::String("Memory".to_string()),
        "label should snap to 'Memory' at t=0 in mixed publish"
    );
    assert_eq!(
        eff_severity,
        WidgetParameterValue::Enum("warning".to_string()),
        "severity should snap to 'warning' at t=0 in mixed publish"
    );
    assert_eq!(
        eff_level,
        WidgetParameterValue::F32(0.0),
        "level should still be at start (0.0) at t=0 in mixed publish"
    );

    assert_eq!(
        eff_fill, old_fill,
        "fill_color should still be at start (cyan-blue) at t=0 in mixed publish"
    );
}

/// At t=0.5 of the mixed publish:
/// - label = "Memory" (still snapped — snap is permanent, not time-dependent)
/// - severity = "warning" (still snapped)
/// - level = midpoint between 0.0 and 0.8 (≈0.4)
/// - fill_color = midpoint between cyan-blue and orange
///
/// [hud-w17j]
#[test]
fn mixed_snap_and_interpolate_at_t05_level_and_color_still_interpolating() {
    let old_level = WidgetParameterValue::F32(0.0);
    let new_level = WidgetParameterValue::F32(0.8);
    let old_fill = WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, 1.0)); // blue
    let new_fill = WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)); // red
    let old_label = WidgetParameterValue::String("CPU".to_string());
    let new_label = WidgetParameterValue::String("Memory".to_string());
    let old_severity = WidgetParameterValue::Enum("info".to_string());
    let new_severity = WidgetParameterValue::Enum("warning".to_string());

    let t = 0.5f32; // midpoint of 300ms transition

    let eff_label = interpolate_param(&old_label, &new_label, t);
    let eff_severity = interpolate_param(&old_severity, &new_severity, t);
    let eff_level = interpolate_param(&old_level, &new_level, t);
    let eff_fill = interpolate_param(&old_fill, &new_fill, t);

    // Snapped params remain at their new values at t=0.5
    assert_eq!(
        eff_label,
        WidgetParameterValue::String("Memory".to_string()),
        "label should remain 'Memory' at t=0.5 (snap persists)"
    );
    assert_eq!(
        eff_severity,
        WidgetParameterValue::Enum("warning".to_string()),
        "severity should remain 'warning' at t=0.5 (snap persists)"
    );

    // Interpolated params are at midpoint
    match eff_level {
        WidgetParameterValue::F32(v) => {
            // 0.0 + (0.8 - 0.0) * 0.5 = 0.4
            assert!(
                (v - 0.4).abs() < 1e-4,
                "level at t=0.5 should be ≈0.4, got {v}"
            );
        }
        other => panic!("expected F32 for level at t=0.5, got {other:?}"),
    }

    match eff_fill {
        WidgetParameterValue::Color(c) => {
            // R: 0.0 + (1.0 - 0.0) * 0.5 = 0.5
            assert!(
                (c.r - 0.5).abs() < 1e-4,
                "fill_color R at t=0.5 should be 0.5, got {}",
                c.r
            );
            // B: 1.0 + (0.0 - 1.0) * 0.5 = 0.5
            assert!(
                (c.b - 0.5).abs() < 1e-4,
                "fill_color B at t=0.5 should be 0.5, got {}",
                c.b
            );
        }
        other => panic!("expected Color for fill_color at t=0.5, got {other:?}"),
    }
}

/// Mixed publish: all four SVG attribute bindings resolve correctly at t=0.5.
///
/// Verifies the compositor correctly handles mixed interpolation semantics in
/// one publish by checking the actual SVG attribute output for each binding.
/// [hud-w17j]
#[test]
fn mixed_publish_svg_attributes_at_t05() {
    let bundle = load_gauge_bundle();
    let level_b = bar_height_binding(&bundle);
    let fill_b = bar_fill_binding(&bundle);
    let label_b = label_binding(&bundle);
    let severity_b = severity_binding(&bundle);

    let old_level = WidgetParameterValue::F32(0.0);
    let new_level = WidgetParameterValue::F32(0.8);
    let old_fill = WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, 1.0));
    let new_fill = WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0));

    let t = 0.5f32;
    let eff_level = interpolate_param(&old_level, &new_level, t);
    let eff_fill = interpolate_param(&old_fill, &new_fill, t);
    let eff_label = interpolate_param(
        &WidgetParameterValue::String("CPU".to_string()),
        &WidgetParameterValue::String("Memory".to_string()),
        t,
    );
    let eff_severity = interpolate_param(
        &WidgetParameterValue::Enum("info".to_string()),
        &WidgetParameterValue::Enum("warning".to_string()),
        t,
    );

    let constraints = level_constraints();
    let mut params = HashMap::new();
    params.insert("level".to_string(), eff_level);
    params.insert("fill_color".to_string(), eff_fill);
    params.insert("label".to_string(), eff_label);
    params.insert("severity".to_string(), eff_severity);

    // bar height: level=0.4 → 0.4 * 200 = 80px
    let height_str = resolve_binding_value(&level_b, &params, &constraints)
        .expect("level height binding should resolve");
    assert_eq!(
        height_str, "80",
        "bar height at t=0.5 (level=0.4) should be '80', got {height_str:?}"
    );

    // bar fill: midpoint blue→red at t=0.5 → neither blue nor red
    let fill_str = resolve_binding_value(&fill_b, &params, &HashMap::new())
        .expect("fill_color binding should resolve");
    assert_ne!(
        fill_str, "#0000ff",
        "fill at t=0.5 should not be starting blue"
    );
    assert_ne!(fill_str, "#ff0000", "fill at t=0.5 should not be final red");

    // label: "Memory" (snapped)
    let label_str = resolve_binding_value(&label_b, &params, &HashMap::new())
        .expect("label binding should resolve");
    assert_eq!(
        label_str, "Memory",
        "label at t=0.5 should be 'Memory' (snapped), got {label_str:?}"
    );

    // severity: "warning" → #FFB800 (snapped at t=0; still #FFB800 at t=0.5)
    let sev_str = resolve_binding_value(&severity_b, &params, &HashMap::new())
        .expect("severity binding should resolve");
    assert_eq!(
        sev_str, "#FFB800",
        "severity at t=0.5 should be #FFB800 (warning, snapped), got {sev_str:?}"
    );
}
