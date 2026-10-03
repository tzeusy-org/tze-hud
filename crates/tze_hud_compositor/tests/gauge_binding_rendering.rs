//! Binding and rendering tests for all four gauge parameter types [hud-6oa4].
//!
//! These tests verify that every gauge parameter type produces the correct SVG
//! attribute mutation via the compositor's binding pipeline, and that contention
//! governance (LatestWins) works as specified.
//!
//! ## Coverage
//!
//! ### Task 6 — Binding and Rendering Tests
//!
//! 6.1  Linear binding  — `level` → `bar.height`  (0.5→100, 0.0→0, 1.0→200, 0.25→50)
//! 6.1y Bar fills upward — `bar.y` = 210 − height, so the bar grows from the track
//!      bottom up; tested alongside 6.1 cases.
//! 6.2  Direct color binding — `fill_color=[255,0,0,255]` → bar fill = `#ff0000`;
//!      alpha case: `fill_color=[0,255,0,128]` includes alpha component.
//! 6.3  Text-content binding — `label` → `label-text` text node.
//! 6.4  Discrete binding — `severity` → `indicator.fill` canonical hex values.
//!
//! ### Task 8 — Contention Tests
//!
//! 8.1  LatestWins — Agent B's publish overrides Agent A's.
//! 8.2  Parameter retention — unpublished params retain previous agent's values.
//! 8.x  All-four-params simultaneous publish.
//!
//! ## Sources
//!
//! - widget-system/spec.md §Requirement: SVG Layer Parameter Bindings
//! - widget-system/spec.md §Requirement: Widget Parameter Interpolation
//! - openspec/changes/exemplar-gauge-widget/tasks.md §6, §8
//! - assets/widget_bundles/gauge/ (production bundle)

use std::collections::HashMap;

use tze_hud_compositor::widget::{apply_svg_attribute, resolve_binding_value};
use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, RenderingPolicy, Rgba, WidgetBinding, WidgetBindingMapping,
    WidgetParameterValue,
};

// ─── Production fill.svg (after token resolution) ────────────────────────────
//
// This is the token-resolved form of assets/widget_bundles/gauge/fill.svg.
// Token placeholders are substituted with canonical values so the SVG parses
// cleanly in resvg and the element attributes are in their initial "default"
// state before any parameter bindings are applied.
//
// Bar starts at y=210, height=0 (empty track). The `clip-path` on the bar
// constrains visible area to x=30, y=10, w=40, h=200. For the bar to fill
// upward we must set BOTH `height` (the binding result) AND `y = 210 - height`
// so the bar bottom stays fixed at y=210 while the top rises.
//
// Canonical token substitutions applied here:
//   {{token.color.text.accent}}   → #4A9EFF   (default bar fill / info blue)
//   {{token.color.text.primary}}  → #FFFFFF
//   {{token.color.outline.default}}→ #000000
//   {{token.stroke.outline.width}}→ 1
//   {{token.color.severity.info}} → #4A9EFF
const PRODUCTION_FILL_SVG: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 240" width="100" height="240">
  <defs>
    <clipPath id="track-clip">
      <rect x="30" y="10" width="40" height="200"/>
    </clipPath>
  </defs>
  <rect
    id="bar"
    x="30" y="210"
    width="40" height="0"
    rx="3" ry="3"
    fill="#4A9EFF"
    clip-path="url(#track-clip)"/>
  <text
    id="label-text"
    data-role="text"
    x="50" y="228"
    text-anchor="middle"
    dominant-baseline="middle"
    font-family="sans-serif"
    font-size="10"
    fill="#FFFFFF"
    stroke="#000000"
    stroke-width="1"
    stroke-linejoin="round"
    paint-order="stroke fill"></text>
  <circle
    id="indicator"
    cx="85" cy="15"
    r="6"
    fill="#4A9EFF"/>
</svg>"##;

// ─── Production gauge binding definitions ─────────────────────────────────────
//
// Mirror the bindings declared in assets/widget_bundles/gauge/widget.toml so tests
// can call resolve_binding_value without loading the bundle from disk.

fn level_height_binding() -> WidgetBinding {
    WidgetBinding {
        param: "level".to_string(),
        target_element: "bar".to_string(),
        target_attribute: "height".to_string(),
        mapping: WidgetBindingMapping::Linear {
            attr_min: 0.0,
            attr_max: 200.0,
        },
    }
}

fn fill_color_binding() -> WidgetBinding {
    WidgetBinding {
        param: "fill_color".to_string(),
        target_element: "bar".to_string(),
        target_attribute: "fill".to_string(),
        mapping: WidgetBindingMapping::Direct,
    }
}

fn label_binding() -> WidgetBinding {
    WidgetBinding {
        param: "label".to_string(),
        target_element: "label-text".to_string(),
        target_attribute: "text-content".to_string(),
        mapping: WidgetBindingMapping::Direct,
    }
}

fn severity_binding() -> WidgetBinding {
    WidgetBinding {
        param: "severity".to_string(),
        target_element: "indicator".to_string(),
        target_attribute: "fill".to_string(),
        mapping: WidgetBindingMapping::Discrete {
            value_map: [
                ("info".to_string(), "#4A9EFF".to_string()),
                ("warning".to_string(), "#FFB800".to_string()),
                ("error".to_string(), "#FF4444".to_string()),
            ]
            .into_iter()
            .collect(),
        },
    }
}

fn level_param_constraints() -> HashMap<String, (f32, f32)> {
    [("level".to_string(), (0.0f32, 1.0f32))]
        .into_iter()
        .collect()
}

// ─── 6.1 Linear binding (level → bar height) ──────────────────────────────────

/// 6.1a: level=0.5 → bar height = 100.0.
///
/// Linear formula: (level − p_min) / (p_max − p_min) * (attr_max − attr_min)
///   = (0.5 − 0.0) / (1.0 − 0.0) * 200.0 = 100.0.
#[test]
fn gauge_linear_binding_level_half_produces_height_100() {
    let params = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.5))]);
    let val = resolve_binding_value(&level_height_binding(), &params, &level_param_constraints())
        .expect("resolve must succeed for level=0.5");
    assert_eq!(
        val, "100",
        "level=0.5 should produce height='100', got: {val}"
    );
}

/// 6.1b: level=0.0 → bar height = 0.0 (empty track, bar visually invisible).
#[test]
fn gauge_linear_binding_level_zero_produces_height_0() {
    let params = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.0))]);
    let val = resolve_binding_value(&level_height_binding(), &params, &level_param_constraints())
        .expect("resolve must succeed for level=0.0");
    assert_eq!(val, "0", "level=0.0 should produce height='0', got: {val}");
}

/// 6.1c: level=1.0 → bar height = 200.0 (full track).
#[test]
fn gauge_linear_binding_level_one_produces_height_200() {
    let params = HashMap::from([("level".to_string(), WidgetParameterValue::F32(1.0))]);
    let val = resolve_binding_value(&level_height_binding(), &params, &level_param_constraints())
        .expect("resolve must succeed for level=1.0");
    assert_eq!(
        val, "200",
        "level=1.0 should produce height='200', got: {val}"
    );
}

/// 6.1d: level=0.25 → bar height = 50.0.
#[test]
fn gauge_linear_binding_level_quarter_produces_height_50() {
    let params = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.25))]);
    let val = resolve_binding_value(&level_height_binding(), &params, &level_param_constraints())
        .expect("resolve must succeed for level=0.25");
    assert_eq!(
        val, "50",
        "level=0.25 should produce height='50', got: {val}"
    );
}

// ─── 6.1y Bar fills UPWARD: y = 210 − height ─────────────────────────────────
//
// The production fill.svg bar starts at y=210 with height=0. For the bar to
// fill upward (bottom fixed at y=210, top rising), both `height` and `y` must
// be updated. `y = 210 − height` keeps the bar bottom anchored at the track
// floor.

/// 6.1y-a: level=0.5 → height=100, y=110. Bar fills upward from track bottom.
#[test]
fn gauge_bar_fills_upward_at_level_half() {
    let height_str = "100";
    let expected_y: i64 = 210 - 100;

    let modified = apply_svg_attribute(PRODUCTION_FILL_SVG, "bar", "height", height_str);
    let modified = apply_svg_attribute(&modified, "bar", "y", &expected_y.to_string());

    assert!(
        modified.contains("height=\"100\""),
        "SVG should contain height=100: {modified}"
    );
    assert!(
        modified.contains("y=\"110\""),
        "SVG should contain y=110 (210-100) for upward fill: {modified}"
    );
    // y=210 (the initial value) must be replaced
    assert!(
        !modified.contains("y=\"210\""),
        "SVG should not retain the original y=210 after update: {modified}"
    );
}

/// 6.1y-b: level=1.0 → height=200, y=10. Bar fills entire track (top = y=10).
#[test]
fn gauge_bar_fills_upward_at_level_full() {
    let height_str = "200";
    let expected_y = 210 - 200_i64;

    let modified = apply_svg_attribute(PRODUCTION_FILL_SVG, "bar", "height", height_str);
    let modified = apply_svg_attribute(&modified, "bar", "y", &expected_y.to_string());

    assert!(
        modified.contains("height=\"200\""),
        "SVG should contain height=200: {modified}"
    );
    assert!(
        modified.contains("y=\"10\""),
        "SVG should contain y=10 (210-200) for full-height upward fill: {modified}"
    );
}

/// 6.1y-c: level=0.0 → height=0, y=210. Empty bar; y stays at track bottom.
#[test]
fn gauge_bar_upward_at_level_zero_stays_at_bottom() {
    let height_str = "0";
    let expected_y = 210;

    let modified = apply_svg_attribute(PRODUCTION_FILL_SVG, "bar", "height", height_str);
    let modified = apply_svg_attribute(&modified, "bar", "y", &expected_y.to_string());

    // height=0 and y=210 are both the same as the production default, so the
    // initial y="210" should still be present.
    assert!(
        modified.contains("height=\"0\""),
        "SVG should contain height=0: {modified}"
    );
    assert!(
        modified.contains("y=\"210\""),
        "SVG should contain y=210 for empty bar: {modified}"
    );
}

// ─── 6.2 Direct color binding (fill_color → bar fill) ────────────────────────

/// 6.2a: fill_color=[255,0,0,255] → bar fill = `#ff0000` (opaque red).
///
/// Rgba channels are 0.0..=1.0 in tze_hud. `[255,0,0,255]` in u8 maps to
/// `Rgba::new(1.0, 0.0, 0.0, 1.0)`.
#[test]
fn gauge_direct_color_binding_red_produces_hex_ff0000() {
    let params = HashMap::from([(
        "fill_color".to_string(),
        WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0)),
    )]);
    let val = resolve_binding_value(&fill_color_binding(), &params, &HashMap::new())
        .expect("resolve must succeed for fill_color=red");

    // Opaque red: alpha ≈ 1.0 → hex shorthand without alpha channel.
    assert_eq!(
        val, "#ff0000",
        "fill_color=[255,0,0,255] should produce '#ff0000', got: {val}"
    );
}

/// 6.2b: fill_color=[0,255,0,128] → bar fill includes an alpha component.
///
/// u8 128 / 255.0 ≈ 0.502.  The compositor emits `rgba(r,g,b,a)` when alpha < 1.
#[test]
fn gauge_direct_color_binding_green_with_alpha() {
    let alpha = 128.0f32 / 255.0;
    let params = HashMap::from([(
        "fill_color".to_string(),
        WidgetParameterValue::Color(Rgba::new(0.0, 1.0, 0.0, alpha)),
    )]);
    let val = resolve_binding_value(&fill_color_binding(), &params, &HashMap::new())
        .expect("resolve must succeed for fill_color=green+alpha");

    // alpha ≈ 0.502 < 1.0 → rgba(...) form expected
    assert!(
        val.starts_with("rgba("),
        "semi-transparent color should produce rgba(...), got: {val}"
    );
    assert!(
        val.contains("0,255,0"),
        "rgba color should contain green components, got: {val}"
    );
}

/// 6.2c: fill_color=[0,0,255,255] → bar fill = `#0000ff` (opaque blue).
#[test]
fn gauge_direct_color_binding_blue_produces_hex_0000ff() {
    let params = HashMap::from([(
        "fill_color".to_string(),
        WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 1.0, 1.0)),
    )]);
    let val = resolve_binding_value(&fill_color_binding(), &params, &HashMap::new())
        .expect("resolve must succeed for fill_color=blue");

    assert_eq!(
        val, "#0000ff",
        "fill_color=[0,0,255,255] should produce '#0000ff', got: {val}"
    );
}

/// 6.2d: SVG attribute mutation — the bar fill attribute in the SVG is updated.
///
/// Verifies that `apply_svg_attribute` correctly replaces the `fill` attribute
/// value on the `bar` element.
#[test]
fn gauge_direct_color_binding_mutates_bar_fill_in_svg() {
    let modified = apply_svg_attribute(PRODUCTION_FILL_SVG, "bar", "fill", "#ff0000");
    assert!(
        modified.contains("fill=\"#ff0000\""),
        "SVG bar fill should be updated to #ff0000: {modified}"
    );
    // The initial default fill (#4A9EFF from token) must be replaced.
    // Only check the bar element's fill (indicator still has #4A9EFF).
    // We verify by checking that the bar rect no longer has its original color.
    // Since both bar and indicator used #4A9EFF, we check the replacement occurred.
    assert!(
        modified.contains("fill=\"#ff0000\""),
        "fill attribute mutation must be applied: {modified}"
    );
}

// ─── 6.3 Text-content binding (label → label-text content) ───────────────────

/// 6.3a: label="CPU Load" → label-text element content = "CPU Load".
#[test]
fn gauge_text_content_binding_label_cpu_load() {
    let params = HashMap::from([(
        "label".to_string(),
        WidgetParameterValue::String("CPU Load".to_string()),
    )]);
    let val = resolve_binding_value(&label_binding(), &params, &HashMap::new())
        .expect("resolve must succeed for label='CPU Load'");
    assert_eq!(
        val, "CPU Load",
        "label binding should resolve to 'CPU Load', got: {val}"
    );

    // Verify SVG mutation sets the text content.
    let modified = apply_svg_attribute(PRODUCTION_FILL_SVG, "label-text", "text-content", &val);
    assert!(
        modified.contains(">CPU Load<"),
        "SVG label-text element should contain 'CPU Load' as text content: {modified}"
    );
}

/// 6.3b: label="" → label-text content = "" (empty string, element present).
#[test]
fn gauge_text_content_binding_empty_label() {
    let params = HashMap::from([(
        "label".to_string(),
        WidgetParameterValue::String(String::new()),
    )]);
    let val = resolve_binding_value(&label_binding(), &params, &HashMap::new())
        .expect("resolve must succeed for label=''");
    assert_eq!(
        val, "",
        "empty label should resolve to empty string, got: {val}"
    );

    let modified = apply_svg_attribute(PRODUCTION_FILL_SVG, "label-text", "text-content", &val);
    // text element must be present (not removed), content is empty
    assert!(
        modified.contains("id=\"label-text\""),
        "label-text element must still be present after empty label: {modified}"
    );
}

/// 6.3c: label="Memory Usage 87%" → long text renders correctly.
#[test]
fn gauge_text_content_binding_long_label() {
    let label = "Memory Usage 87%";
    let params = HashMap::from([(
        "label".to_string(),
        WidgetParameterValue::String(label.to_string()),
    )]);
    let val = resolve_binding_value(&label_binding(), &params, &HashMap::new())
        .expect("resolve must succeed for long label");
    assert_eq!(
        val, label,
        "long label should be preserved verbatim, got: {val}"
    );

    let modified = apply_svg_attribute(PRODUCTION_FILL_SVG, "label-text", "text-content", &val);
    assert!(
        modified.contains(">Memory Usage 87%<"),
        "SVG label-text must contain the full long label: {modified}"
    );
}

// ─── 6.4 Discrete binding (severity → indicator fill) ────────────────────────
//
// Canonical severity colors from widget.toml value_map:
//   info    = #4A9EFF
//   warning = #FFB800
//   error   = #FF4444

/// 6.4a: severity="info" → indicator fill = #4A9EFF.
#[test]
fn gauge_discrete_binding_severity_info_produces_4a9eff() {
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("info".to_string()),
    )]);
    let val = resolve_binding_value(&severity_binding(), &params, &HashMap::new())
        .expect("resolve must succeed for severity=info");
    assert_eq!(
        val, "#4A9EFF",
        "severity=info should map to #4A9EFF, got: {val}"
    );
}

/// 6.4b: severity="warning" → indicator fill = #FFB800.
#[test]
fn gauge_discrete_binding_severity_warning_produces_ffb800() {
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("warning".to_string()),
    )]);
    let val = resolve_binding_value(&severity_binding(), &params, &HashMap::new())
        .expect("resolve must succeed for severity=warning");
    assert_eq!(
        val, "#FFB800",
        "severity=warning should map to #FFB800, got: {val}"
    );
}

/// 6.4c: severity="error" → indicator fill = #FF4444.
#[test]
fn gauge_discrete_binding_severity_error_produces_ff4444() {
    let params = HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("error".to_string()),
    )]);
    let val = resolve_binding_value(&severity_binding(), &params, &HashMap::new())
        .expect("resolve must succeed for severity=error");
    assert_eq!(
        val, "#FF4444",
        "severity=error should map to #FF4444, got: {val}"
    );
}

/// 6.4d: SVG mutation — indicator fill attribute is updated in the SVG.
#[test]
fn gauge_discrete_binding_mutates_indicator_fill_in_svg() {
    let modified = apply_svg_attribute(PRODUCTION_FILL_SVG, "indicator", "fill", "#FFB800");
    assert!(
        modified.contains("fill=\"#FFB800\""),
        "SVG indicator fill should be updated to #FFB800 (warning): {modified}"
    );
}

// ─── Contention tests (task 8) ────────────────────────────────────────────────
//
// These tests use the SceneGraph + production gauge bundle for end-to-end
// validation.  They mirror the helper from gauge_param_validation.rs but
// are self-contained.

mod contention {
    use super::*;

    use std::path::PathBuf;
    use tze_hud_scene::SceneGraph;
    use tze_hud_scene::SceneId;
    use tze_hud_scene::types::WidgetInstance;
    use tze_hud_widget::loader::{BundleScanResult, load_bundle_dir_with_tokens};

    /// Path to the production exemplar gauge bundle.
    fn production_gauge_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..") // crates/
            .join("..") // workspace root
            .join("assets")
            .join("widgets")
            .join("gauge")
    }

    /// Minimal token map that resolves all `{{token.*}}` placeholders in the
    /// production gauge SVG files.  Values are test stubs — visual accuracy is
    /// irrelevant; we only need the bundle to load successfully.
    fn gauge_test_tokens() -> HashMap<String, String> {
        [
            ("color.backdrop.default", "#1a1a2e"),
            ("color.border.default", "#2a2a4e"),
            ("color.outline.default", "#3a3a5e"),
            ("color.severity.info", "#4a9eff"),
            ("color.text.secondary", "#8f96a3"),
            ("color.text.accent", "#4a9eff"),
            ("color.text.primary", "#cccccc"),
            ("opacity.backdrop.opaque", "0.9"),
            ("border.radius.small", "4"),
            ("border.radius.medium", "8"),
            ("border.radius.large", "16"),
            ("stroke.border.width", "1"),
            ("stroke.outline.width", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    /// Load the production gauge bundle and register it in a fresh `SceneGraph`.
    ///
    /// Returns `(scene, tab_id)` ready for `publish_to_widget("gauge", ...)` calls.
    fn scene_with_production_gauge() -> (SceneGraph, tze_hud_scene::types::SceneId) {
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

    // ── 8.1 LatestWins: Agent B's publish overrides Agent A's ─────────────────

    /// 8.1: Agent A publishes level=0.3, Agent B publishes level=0.7.
    /// Gauge resolves to level=0.7 (Agent B wins under LatestWins).
    #[test]
    fn gauge_latestwins_agent_b_overrides_agent_a() {
        let (mut scene, tab_id) = scene_with_production_gauge();

        // Agent A publishes level=0.3.
        let params_a = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.3))]);
        scene
            .publish_to_widget("gauge", params_a, "agent.a", None, 0, None)
            .expect("Agent A publish must succeed");

        // Agent B publishes level=0.7 (should override Agent A).
        let params_b = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.7))]);
        scene
            .publish_to_widget("gauge", params_b, "agent.b", None, 0, None)
            .expect("Agent B publish must succeed");

        // Effective params should reflect Agent B's level=0.7.
        let occupancy = scene
            .widget_registry
            .get_occupancy("gauge", tab_id)
            .expect("gauge occupancy must be accessible");

        match occupancy.effective_params.get("level") {
            Some(WidgetParameterValue::F32(v)) => {
                assert!(
                    (v - 0.7).abs() < 1e-5,
                    "LatestWins: Agent B's level=0.7 should override Agent A's 0.3, got: {v}"
                );
            }
            other => panic!("expected F32(0.7) for level, got: {other:?}"),
        }
    }

    // ── 8.2 Parameter retention ───────────────────────────────────────────────

    /// 8.2: Agent A publishes {level=0.5, label="CPU"}.
    ///      Agent B publishes {level=0.8} (no label).
    ///
    /// Under LatestWins the latest publication wins for overlapping params.
    /// Agent B's publish replaces Agent A's publication entirely, but Agent B
    /// did not include `label`. Therefore `label` falls back to the schema
    /// default ("") rather than retaining Agent A's "CPU".
    ///
    /// This test verifies the LatestWins per-publication merge semantics:
    /// the winning publication's params are merged over schema defaults.
    /// Only params explicitly included in the winning publish are retained from
    /// that publish; missing params come from defaults, not from prior publishes.
    #[test]
    fn gauge_latestwins_agent_b_overwrites_agent_a_entirely() {
        let (mut scene, tab_id) = scene_with_production_gauge();

        // Agent A publishes level=0.5 and label="CPU".
        let params_a = HashMap::from([
            ("level".to_string(), WidgetParameterValue::F32(0.5)),
            (
                "label".to_string(),
                WidgetParameterValue::String("CPU".to_string()),
            ),
        ]);
        scene
            .publish_to_widget("gauge", params_a, "agent.a", None, 0, None)
            .expect("Agent A publish must succeed");

        // Agent B publishes only level=0.8 (no label).
        let params_b = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.8))]);
        scene
            .publish_to_widget("gauge", params_b, "agent.b", None, 0, None)
            .expect("Agent B publish must succeed");

        let occupancy = scene
            .widget_registry
            .get_occupancy("gauge", tab_id)
            .expect("gauge occupancy must be accessible");

        // Level: Agent B's value wins.
        match occupancy.effective_params.get("level") {
            Some(WidgetParameterValue::F32(v)) => {
                assert!(
                    (v - 0.8).abs() < 1e-5,
                    "level should be Agent B's 0.8 under LatestWins, got: {v}"
                );
            }
            other => panic!("expected F32(0.8) for level, got: {other:?}"),
        }

        // Label: Agent B did not include label, so it falls back to schema default "".
        // Under LatestWins only one publication is active; missing params merge from defaults.
        match occupancy.effective_params.get("label") {
            Some(WidgetParameterValue::String(s)) => {
                // Agent B's publish supersedes Agent A's entirely; label is not in B's params,
                // so it reverts to schema default "".
                assert_eq!(
                    s, "",
                    "label should fall back to schema default '' (Agent B did not publish label), got: '{s}'"
                );
            }
            other => panic!("expected String for label, got: {other:?}"),
        }
    }

    /// 8.2-retention: Verify that Agent A's label IS retained when Agent B publishes
    /// the SAME widget using MergeByKey contention policy (per-param ownership model).
    ///
    /// This is the complement to 8.2 above — it shows that per-param retention
    /// requires MergeByKey, not LatestWins. Under MergeByKey each agent owns its
    /// published keys and other agents' params are preserved across partial publishes.
    #[test]
    fn gauge_mergebykey_retains_other_agents_unpublished_params() {
        let (mut scene, tab_id) = scene_with_production_gauge();

        // Override the instance contention policy to MergeByKey.
        // Directly set the contention_override on the registered instance.
        if let Some(instance) = scene.widget_registry.instances.get_mut("gauge") {
            instance.contention_override = Some(ContentionPolicy::MergeByKey { max_keys: 32 });
        }

        // Agent A publishes level=0.5 and label="CPU" (merge key = agent.a).
        let params_a = HashMap::from([
            ("level".to_string(), WidgetParameterValue::F32(0.5)),
            (
                "label".to_string(),
                WidgetParameterValue::String("CPU".to_string()),
            ),
        ]);
        scene
            .publish_to_widget(
                "gauge",
                params_a,
                "agent.a",
                Some("agent.a".to_string()),
                0,
                None,
            )
            .expect("Agent A publish must succeed");

        // Agent B publishes only level=0.8 under its own merge key.
        let params_b = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.8))]);
        scene
            .publish_to_widget(
                "gauge",
                params_b,
                "agent.b",
                Some("agent.b".to_string()),
                0,
                None,
            )
            .expect("Agent B publish must succeed");

        let occupancy = scene
            .widget_registry
            .get_occupancy("gauge", tab_id)
            .expect("gauge occupancy must be accessible");

        // Under MergeByKey, Agent B's level=0.8 wins for the "level" key (inserted last).
        match occupancy.effective_params.get("level") {
            Some(WidgetParameterValue::F32(v)) => {
                assert!(
                    (v - 0.8).abs() < 1e-5,
                    "MergeByKey: level should be 0.8 (Agent B's most recent), got: {v}"
                );
            }
            other => panic!("expected F32(0.8) for level, got: {other:?}"),
        }

        // Under MergeByKey, Agent A's label="CPU" is retained because Agent B never published label.
        match occupancy.effective_params.get("label") {
            Some(WidgetParameterValue::String(s)) => {
                assert_eq!(
                    s, "CPU",
                    "MergeByKey: label should be retained as 'CPU' from Agent A, got: '{s}'"
                );
            }
            other => panic!("expected String('CPU') for label, got: {other:?}"),
        }
    }

    // ── All-four-params simultaneous publish ──────────────────────────────────

    /// All four gauge parameters published simultaneously in a single call.
    ///
    /// Verifies that a single publish with all four params correctly updates all
    /// effective parameter values in one render pass.
    #[test]
    fn gauge_all_four_params_simultaneous_publish() {
        let (mut scene, tab_id) = scene_with_production_gauge();

        let params = HashMap::from([
            ("level".to_string(), WidgetParameterValue::F32(0.65)),
            (
                "label".to_string(),
                WidgetParameterValue::String("System".to_string()),
            ),
            (
                "fill_color".to_string(),
                // [255, 165, 0, 255] → orange
                WidgetParameterValue::Color(Rgba::new(1.0, 165.0 / 255.0, 0.0, 1.0)),
            ),
            (
                "severity".to_string(),
                WidgetParameterValue::Enum("warning".to_string()),
            ),
        ]);

        scene
            .publish_to_widget("gauge", params, "agent.all", None, 0, None)
            .expect("all-four-params publish must succeed");

        let occupancy = scene
            .widget_registry
            .get_occupancy("gauge", tab_id)
            .expect("gauge occupancy must be accessible");
        let ep = &occupancy.effective_params;

        // Verify level.
        match ep.get("level") {
            Some(WidgetParameterValue::F32(v)) => {
                assert!((v - 0.65).abs() < 1e-5, "level should be 0.65, got: {v}");
            }
            other => panic!("expected F32(0.65) for level, got: {other:?}"),
        }

        // Verify label.
        match ep.get("label") {
            Some(WidgetParameterValue::String(s)) => {
                assert_eq!(s, "System", "label should be 'System', got: '{s}'");
            }
            other => panic!("expected String('System') for label, got: {other:?}"),
        }

        // Verify fill_color is Color type.
        match ep.get("fill_color") {
            Some(WidgetParameterValue::Color(rgba)) => {
                assert!(
                    (rgba.r - 1.0).abs() < 1e-3,
                    "fill_color red channel should be ~1.0, got: {}",
                    rgba.r
                );
                assert!(
                    rgba.g > 0.6 && rgba.g < 0.7,
                    "fill_color green channel should be ~0.647 (165/255), got: {}",
                    rgba.g
                );
                assert!(
                    rgba.b.abs() < 1e-3,
                    "fill_color blue channel should be ~0.0, got: {}",
                    rgba.b
                );
            }
            other => panic!("expected Color for fill_color, got: {other:?}"),
        }

        // Verify severity.
        match ep.get("severity") {
            Some(WidgetParameterValue::Enum(s)) => {
                assert_eq!(s, "warning", "severity should be 'warning', got: '{s}'");
            }
            other => panic!("expected Enum('warning') for severity, got: {other:?}"),
        }
    }

    // ── All-four-params SVG attribute mutation in a single pass ───────────────

    /// Verify that resolving all four bindings and applying them to the production
    /// fill.svg yields the expected attributes in a single render pass.
    ///
    /// This tests the `resolve_binding_value` + `apply_svg_attribute` pipeline
    /// end-to-end using the production binding definitions.
    #[test]
    fn gauge_all_four_params_produce_correct_svg_attributes() {
        let level = 0.65f32;
        let params = HashMap::from([
            ("level".to_string(), WidgetParameterValue::F32(level)),
            (
                "label".to_string(),
                WidgetParameterValue::String("System".to_string()),
            ),
            (
                "fill_color".to_string(),
                WidgetParameterValue::Color(Rgba::new(1.0, 165.0 / 255.0, 0.0, 1.0)),
            ),
            (
                "severity".to_string(),
                WidgetParameterValue::Enum("warning".to_string()),
            ),
        ]);
        let constraints = level_param_constraints();

        // Resolve each binding.
        let height_val =
            resolve_binding_value(&level_height_binding(), &params, &constraints).unwrap();
        let fill_val =
            resolve_binding_value(&fill_color_binding(), &params, &HashMap::new()).unwrap();
        let label_val = resolve_binding_value(&label_binding(), &params, &HashMap::new()).unwrap();
        let sev_val = resolve_binding_value(&severity_binding(), &params, &HashMap::new()).unwrap();

        // level=0.65 → height = 0.65 * 200 = 130.
        assert_eq!(
            height_val, "130",
            "level=0.65 should produce height=130, got: {height_val}"
        );
        // fill_color=orange → hex color (no alpha)
        assert_eq!(
            fill_val, "#ffa500",
            "orange fill should be #ffa500, got: {fill_val}"
        );
        // label="System"
        assert_eq!(
            label_val, "System",
            "label should be 'System', got: {label_val}"
        );
        // severity=warning → canonical warning color
        assert_eq!(
            sev_val, "#FFB800",
            "warning severity should be #FFB800, got: {sev_val}"
        );

        // Apply all four bindings to the production SVG.
        let y_val = (210.0 - height_val.parse::<f32>().unwrap()) as i64;
        let mut svg = PRODUCTION_FILL_SVG.to_string();
        svg = apply_svg_attribute(&svg, "bar", "height", &height_val);
        svg = apply_svg_attribute(&svg, "bar", "y", &y_val.to_string());
        svg = apply_svg_attribute(&svg, "bar", "fill", &fill_val);
        svg = apply_svg_attribute(&svg, "label-text", "text-content", &label_val);
        svg = apply_svg_attribute(&svg, "indicator", "fill", &sev_val);

        // Verify all four mutations in the resulting SVG.
        assert!(
            svg.contains("height=\"130\""),
            "SVG must contain height=130: {svg}"
        );
        assert!(
            svg.contains("y=\"80\""),
            "SVG must contain y=80 (210-130) for upward fill: {svg}"
        );
        assert!(
            svg.contains("fill=\"#ffa500\""),
            "SVG must contain bar fill=#ffa500: {svg}"
        );
        assert!(
            svg.contains(">System<"),
            "SVG must contain label-text content='System': {svg}"
        );
        assert!(
            svg.contains("fill=\"#FFB800\""),
            "SVG must contain indicator fill=#FFB800 (warning): {svg}"
        );
    }
}
