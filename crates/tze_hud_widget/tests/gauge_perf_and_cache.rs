//! Performance budget and texture-cache validation tests for the production
//! gauge widget bundle.
//!
//! Acceptance criteria (hud-cxgy):
//!
//!   1. Single-param re-rasterization at 512×512: CI budget (500ms); spec target 2ms.
//!   2. Cache hit: `current_params` equality signals zero re-rasterization.
//!   3. Multi-param change (all four params): CI budget (500ms).
//!   4. Same-value publish: `current_params` remains equal → treated as cache hit.
//!   5. Interpolation frames: every frame within CI budget during a 300ms transition.
//!
//! Strategy
//! ─────────
//! Tests 1, 3, and 5 time `rasterize_svg_layers` — the pure CPU re-rasterization
//! path — with production SVGs loaded from the bundle.  No GPU device is required.
//!
//! Tests 2 and 4 drive `SceneGraph::publish_to_widget` and inspect
//! `WidgetInstance::current_params` before/after identical publishes.  The
//! compositor's frame loop gates re-rasterization on
//! `entry.last_rendered_params != effective_params`; verifying params equality
//! proves the cache-hit path would be taken.
//!
//! Thresholds
//! ──────────
//! | Context            | Threshold | Rationale                                  |
//! |--------------------|-----------|---------------------------------------------|
//! | Reference hardware | < 2ms     | Spec target (exemplar-gauge-widget/spec.md §Performance Budget) |
//! | CI (any renderer)  | < 500ms   | Headroom for debug builds + llvmpipe VMs    |
//!
//! Source: exemplar-gauge-widget/spec.md §Requirement: Gauge Widget Performance Budget.
//! [hud-cxgy]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use tze_hud_compositor::widget::{compute_transition_t, interpolate_param, rasterize_svg_layers};
use tze_hud_scene::DegradationLevel;
use tze_hud_scene::SceneGraph;
use tze_hud_scene::SceneId;
use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, RenderingPolicy, Rgba, WidgetBinding, WidgetInstance,
    WidgetParameterValue,
};
use tze_hud_widget::loader::{BundleScanResult, load_bundle_dir_with_tokens};

// ─── CI threshold ─────────────────────────────────────────────────────────────

/// Lenient CI threshold: 500ms allows ample headroom for debug builds and
/// software renderers (llvmpipe).
///
/// The strict 2ms spec requirement is enforced by the Criterion benchmark in
/// the compositor crate:
///   `cargo bench -p tze_hud_compositor --bench widget_rasterize`
const CI_THRESHOLD_MS: u64 = 500;

/// Spec target for documentation purposes.  Not enforced in CI.
#[allow(dead_code)]
const SPEC_TARGET_MS: u64 = 2;

// ─── Fixture helpers ──────────────────────────────────────────────────────────

/// Minimal token map resolving all `{{token.*}}` placeholders in the production
/// gauge SVG files.  Values are chosen for test isolation only.
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
fn production_gauge_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("assets")
        .join("widget_bundles")
        .join("gauge")
}

/// Load the production gauge bundle.  Panics on failure.
fn load_gauge_bundle() -> tze_hud_widget::loader::LoadedBundle {
    let path = production_gauge_path();
    let tokens = gauge_test_tokens();
    match load_bundle_dir_with_tokens(&path, &tokens) {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("production gauge bundle failed to load: {e}"),
    }
}

/// Build `(svg_texts, layer_specs)` from the loaded bundle so that
/// `rasterize_svg_layers` can be called without a GPU device.
///
/// Returns `(background_svg, fill_svg)` as resolved UTF-8 strings.
fn gauge_svg_texts(bundle: &tze_hud_widget::loader::LoadedBundle) -> (String, String) {
    let bg = bundle
        .svg_contents
        .get("background.svg")
        .expect("background.svg must be present in production gauge bundle");
    let fill = bundle
        .svg_contents
        .get("fill.svg")
        .expect("fill.svg must be present in production gauge bundle");

    let bg_str = std::str::from_utf8(bg)
        .expect("background.svg must be valid UTF-8")
        .to_string();
    let fill_str = std::str::from_utf8(fill)
        .expect("fill.svg must be valid UTF-8")
        .to_string();

    (bg_str, fill_str)
}

/// Extract bindings for the fill layer from the loaded bundle.
fn fill_bindings(bundle: &tze_hud_widget::loader::LoadedBundle) -> Vec<WidgetBinding> {
    bundle
        .definition
        .layers
        .iter()
        .find(|l| l.svg_file == "fill.svg")
        .expect("fill.svg layer should be present in gauge bundle")
        .bindings
        .clone()
}

/// Build f32 constraint map for the gauge `level` parameter.
fn level_constraints() -> HashMap<String, (f32, f32)> {
    HashMap::from([("level".to_string(), (0.0f32, 1.0f32))])
}

/// Build params representing a `level` change.
fn params_level(level: f32) -> HashMap<String, WidgetParameterValue> {
    HashMap::from([
        ("level".to_string(), WidgetParameterValue::F32(level)),
        (
            "fill_color".to_string(),
            WidgetParameterValue::Color(Rgba {
                r: 0.0,
                g: 0.706,
                b: 1.0,
                a: 1.0,
            }),
        ),
        (
            "label".to_string(),
            WidgetParameterValue::String("Test".to_string()),
        ),
        (
            "severity".to_string(),
            WidgetParameterValue::Enum("info".to_string()),
        ),
    ])
}

/// Build params exercising all four gauge parameters.
fn params_all_changed() -> HashMap<String, WidgetParameterValue> {
    HashMap::from([
        ("level".to_string(), WidgetParameterValue::F32(0.65)),
        (
            "fill_color".to_string(),
            WidgetParameterValue::Color(Rgba {
                r: 1.0,
                g: 0.647,
                b: 0.0,
                a: 1.0,
            }),
        ),
        (
            "label".to_string(),
            WidgetParameterValue::String("CPU".to_string()),
        ),
        (
            "severity".to_string(),
            WidgetParameterValue::Enum("warning".to_string()),
        ),
    ])
}

/// Build a `SceneGraph` with the production gauge definition registered and one
/// "gauge" instance.  Returns `(scene, tab_id)`.
fn scene_with_production_gauge() -> (SceneGraph, tze_hud_scene::types::SceneId) {
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

// ═══════════════════════════════════════════════════════════════════════════════
// TEST 1: Single-param re-rasterization at 512×512 within CI budget
// ═══════════════════════════════════════════════════════════════════════════════

/// WHEN the 512×512 gauge widget is re-rasterized after a `level` parameter
/// change THEN rasterization completes within the lenient CI threshold of 500ms.
///
/// Verifies the §Performance Budget requirement for single-param change.
/// The strict 2ms spec target is measured by the Criterion benchmark.
///
/// Source: exemplar-gauge-widget/spec.md §Scenario: Re-rasterization within budget
/// [hud-cxgy]
#[test]
fn single_param_level_change_rasterize_within_ci_budget() {
    let bundle = load_gauge_bundle();
    let (bg_svg, fill_svg) = gauge_svg_texts(&bundle);
    let bindings = fill_bindings(&bundle);
    let constraints = level_constraints();
    let params = params_level(0.75);

    let layers: Vec<(&str, &[WidgetBinding])> =
        vec![(bg_svg.as_str(), &[]), (fill_svg.as_str(), &bindings)];

    // Warmup: one iteration to allow any lazy initialization.
    let _ = rasterize_svg_layers(&layers, &constraints, &params, 512, 512);

    // Timed iteration — single level param change.
    let params_changed = params_level(0.42);
    let start = Instant::now();
    let pixmap = rasterize_svg_layers(&layers, &constraints, &params_changed, 512, 512);
    let elapsed_us = start.elapsed().as_micros() as u64;
    let elapsed_ms = elapsed_us / 1000;

    // Correctness: must produce a valid 512×512 pixmap.
    let pixmap = pixmap.expect("rasterize_svg_layers must produce a pixmap for the gauge");
    assert_eq!(pixmap.width(), 512, "pixmap width must be 512");
    assert_eq!(pixmap.height(), 512, "pixmap height must be 512");

    eprintln!(
        "[gauge_perf] single-param (level) re-rasterize 512×512: {elapsed_us}µs ({elapsed_ms} ms) \
         — CI threshold: {CI_THRESHOLD_MS}ms, spec target (ref hw): {SPEC_TARGET_MS}ms",
    );

    assert!(
        elapsed_ms < CI_THRESHOLD_MS,
        "gauge 512×512 single-param re-rasterization took {elapsed_ms}ms, exceeds CI threshold of {CI_THRESHOLD_MS}ms \
         (spec target for reference hardware is {SPEC_TARGET_MS}ms). \
         Likely a catastrophic regression. \
         Run `cargo bench -p tze_hud_compositor --bench widget_rasterize` on optimised \
         reference hardware to verify the 2ms spec requirement.",
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// TEST 2: Cache hit — unchanged params produce equal current_params
// ═══════════════════════════════════════════════════════════════════════════════

/// WHEN a frame is rendered and no gauge parameters have changed since the last
/// rasterization THEN the compositor reuses the cached GPU texture without
/// re-rasterizing.
///
/// Verified by confirming that `current_params` is identical before and after a
/// second publish with the same values.  The compositor's frame loop gates
/// re-rasterization on `entry.last_rendered_params != effective_params`; equal
/// params → cache hit (no re-rasterization call).
///
/// Source: exemplar-gauge-widget/spec.md §Scenario: Unchanged parameters reuse cache
/// [hud-cxgy]
#[test]
fn unchanged_params_signal_cache_hit() {
    let (mut scene, _tab) = scene_with_production_gauge();

    // Initial publish — set the gauge to a known state.
    let params_initial = HashMap::from([
        ("level".to_string(), WidgetParameterValue::F32(0.5)),
        (
            "fill_color".to_string(),
            WidgetParameterValue::Color(Rgba {
                r: 0.0,
                g: 0.706,
                b: 1.0,
                a: 1.0,
            }),
        ),
        (
            "label".to_string(),
            WidgetParameterValue::String("System".to_string()),
        ),
        (
            "severity".to_string(),
            WidgetParameterValue::Enum("info".to_string()),
        ),
    ]);

    scene
        .publish_to_widget("gauge", params_initial.clone(), "agent.test", None, 0, None)
        .expect("initial publish should succeed");

    // Capture current_params after the initial publish.
    let params_after_first = scene
        .widget_registry
        .instances
        .get("gauge")
        .expect("gauge instance must exist")
        .current_params
        .clone();

    // Publish exactly the same values again (same-value publish).
    scene
        .publish_to_widget("gauge", params_initial.clone(), "agent.test", None, 0, None)
        .expect("same-value re-publish should succeed");

    // current_params must be identical — the compositor would detect
    // `last_rendered_params == effective_params` and skip re-rasterization.
    let params_after_second = scene
        .widget_registry
        .instances
        .get("gauge")
        .expect("gauge instance must exist")
        .current_params
        .clone();

    assert_eq!(
        params_after_first, params_after_second,
        "current_params must be identical after a same-value re-publish; \
         params mismatch would cause unnecessary re-rasterization"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// TEST 3: Multi-param change within CI budget
// ═══════════════════════════════════════════════════════════════════════════════

/// WHEN the 512×512 gauge widget is re-rasterized after changing level,
/// fill_color, and severity simultaneously THEN rasterization still completes
/// within the lenient CI threshold of 500ms.
///
/// Multi-param changes are no more expensive than single-param changes because
/// all binding resolution and SVG manipulation happens inline before the single
/// usvg parse + render call.
///
/// Source: exemplar-gauge-widget/spec.md §Requirement: Gauge Widget Performance Budget
/// [hud-cxgy]
#[test]
fn multi_param_change_rasterize_within_ci_budget() {
    let bundle = load_gauge_bundle();
    let (bg_svg, fill_svg) = gauge_svg_texts(&bundle);
    let bindings = fill_bindings(&bundle);
    let constraints = level_constraints();

    let layers: Vec<(&str, &[WidgetBinding])> =
        vec![(bg_svg.as_str(), &[]), (fill_svg.as_str(), &bindings)];

    // Warmup with a baseline param set.
    let params_baseline = params_level(0.0);
    let _ = rasterize_svg_layers(&layers, &constraints, &params_baseline, 512, 512);

    // Timed iteration — all four params changed simultaneously.
    let params_all = params_all_changed();
    let start = Instant::now();
    let pixmap = rasterize_svg_layers(&layers, &constraints, &params_all, 512, 512);
    let elapsed_us = start.elapsed().as_micros() as u64;
    let elapsed_ms = elapsed_us / 1000;

    let pixmap = pixmap.expect("rasterize_svg_layers must produce a pixmap for multi-param change");
    assert_eq!(pixmap.width(), 512, "pixmap width must be 512");
    assert_eq!(pixmap.height(), 512, "pixmap height must be 512");

    eprintln!(
        "[gauge_perf] multi-param (level+fill_color+label+severity) re-rasterize 512×512: \
         {elapsed_us}µs ({elapsed_ms} ms) — CI threshold: {CI_THRESHOLD_MS}ms, spec target (ref hw): {SPEC_TARGET_MS}ms",
    );

    assert!(
        elapsed_ms < CI_THRESHOLD_MS,
        "gauge 512×512 multi-param re-rasterization took {elapsed_ms}ms, exceeds CI threshold of {CI_THRESHOLD_MS}ms \
         (spec target for reference hardware is {SPEC_TARGET_MS}ms). \
         Run `cargo bench -p tze_hud_compositor --bench widget_rasterize` on optimised \
         reference hardware to verify the 2ms spec requirement.",
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// TEST 4: Same-value publish treated as cache hit
// ═══════════════════════════════════════════════════════════════════════════════

/// WHEN an agent publishes parameter values that are identical to the currently
/// rendered values THEN `current_params` remains effectively equal after the
/// second publish, causing the compositor to treat it as a cache hit and skip
/// re-rasterization.
///
/// Note: `publish_to_widget` always inserts validated values into `current_params`
/// (even when the incoming value is identical).  The invariant being tested is
/// that the resulting stored value compares equal, so the compositor's
/// `last_rendered_params != effective_params` check would return false and
/// skip re-rasterization.
///
/// Exercises the case where `level=0.75` is published, then `level=0.75` is
/// published again.  The effective level in `current_params` must compare equal.
///
/// Source: exemplar-gauge-widget/spec.md §Scenario: Unchanged parameters reuse cache
/// [hud-cxgy]
#[test]
fn same_value_publish_is_cache_hit() {
    let (mut scene, _tab) = scene_with_production_gauge();

    // First publish: set level to 0.75.
    let level_params = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.75))]);

    scene
        .publish_to_widget("gauge", level_params.clone(), "agent.test", None, 0, None)
        .expect("first publish should succeed");

    let params_after_first = {
        let inst = scene
            .widget_registry
            .instances
            .get("gauge")
            .expect("gauge instance must exist");
        inst.current_params
            .get("level")
            .cloned()
            .expect("level must be present in current_params")
    };

    // Second publish: same level=0.75.
    scene
        .publish_to_widget("gauge", level_params, "agent.test", None, 0, None)
        .expect("same-value second publish should succeed");

    let params_after_second = {
        let inst = scene
            .widget_registry
            .instances
            .get("gauge")
            .expect("gauge instance must exist");
        inst.current_params
            .get("level")
            .cloned()
            .expect("level must be present in current_params after second publish")
    };

    // The level value in current_params must be identical.
    // This means `last_rendered_params == effective_params` in the compositor frame loop,
    // which causes it to skip re-rasterization (cache hit).
    assert_eq!(
        params_after_first, params_after_second,
        "same-value publish must leave current_params equal for 'level'; \
         a differing stored value would cause a spurious cache miss in the compositor"
    );

    // Verify the value is the clamped 0.75 we published.
    match params_after_second {
        WidgetParameterValue::F32(v) => {
            assert!(
                (v - 0.75).abs() < 1e-6,
                "level in current_params must be 0.75 (published value), got {v}"
            );
        }
        other => panic!("expected F32 for level, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TEST 5: Interpolation frames — every frame within CI budget over 300ms
// ═══════════════════════════════════════════════════════════════════════════════

/// WHEN the gauge animates from level=0.2 to level=0.8 over 300ms THEN every
/// interpolated frame must rasterize within the lenient CI threshold.
///
/// Simulates 10 evenly-spaced interpolation steps (0ms … 300ms) and times each
/// `rasterize_svg_layers` call.  All must complete within 500ms (CI threshold).
///
/// This validates that the interpolation path does not introduce overhead beyond
/// normal single-param rasterization — each frame differs only in the effective
/// `level` value passed to `rasterize_svg_layers`.
///
/// Source: exemplar-gauge-widget/spec.md §Requirement: Gauge Widget Interpolation Behavior,
///         §Scenario: Level animates smoothly over 300ms
/// [hud-cxgy]
#[test]
fn interpolation_frames_all_within_ci_budget() {
    let bundle = load_gauge_bundle();
    let (bg_svg, fill_svg) = gauge_svg_texts(&bundle);
    let bindings = fill_bindings(&bundle);
    let constraints = level_constraints();

    let layers: Vec<(&str, &[WidgetBinding])> =
        vec![(bg_svg.as_str(), &[]), (fill_svg.as_str(), &bindings)];

    let old_level = WidgetParameterValue::F32(0.2);
    let new_level = WidgetParameterValue::F32(0.8);
    let transition_ms = 300.0f32;

    // Warmup at t=0.
    let t_warmup = compute_transition_t(0.0, transition_ms, DegradationLevel::Nominal);
    let effective_warmup = interpolate_param(&old_level, &new_level, t_warmup);
    let mut warmup_params = params_level(0.2);
    warmup_params.insert("level".to_string(), effective_warmup);
    let _ = rasterize_svg_layers(&layers, &constraints, &warmup_params, 512, 512);

    // 10 evenly-spaced frames from 0ms to 300ms (inclusive).
    let step_count = 10usize;
    let mut max_elapsed_ms: u64 = 0;
    let mut frame_timings: Vec<(f32, u64)> = Vec::with_capacity(step_count + 1);

    for i in 0..=step_count {
        let elapsed_ms = (i as f32 / step_count as f32) * transition_ms;
        let t = compute_transition_t(elapsed_ms, transition_ms, DegradationLevel::Nominal);
        let effective_level = interpolate_param(&old_level, &new_level, t);

        // Build params with the interpolated level.
        let mut params = params_level(0.2); // use baseline fill_color/label/severity
        params.insert("level".to_string(), effective_level);

        let start = Instant::now();
        let pixmap = rasterize_svg_layers(&layers, &constraints, &params, 512, 512);
        let frame_us = start.elapsed().as_micros() as u64;
        let frame_ms = frame_us / 1000;

        // Each frame must produce a valid pixmap.
        let pixmap = pixmap.expect("rasterize_svg_layers must produce a pixmap during transition");
        assert_eq!(pixmap.width(), 512, "frame {i}: pixmap width must be 512");
        assert_eq!(pixmap.height(), 512, "frame {i}: pixmap height must be 512");

        frame_timings.push((elapsed_ms, frame_ms));

        if frame_ms > max_elapsed_ms {
            max_elapsed_ms = frame_ms;
        }

        assert!(
            frame_ms < CI_THRESHOLD_MS,
            "interpolation frame at t={elapsed_ms:.0}ms (elapsed {frame_ms}ms) exceeds CI \
             threshold of {CI_THRESHOLD_MS}ms; spec target for reference hardware is {SPEC_TARGET_MS}ms"
        );
    }

    // Log all frame timings for visibility.
    for (elapsed, frame_ms) in &frame_timings {
        eprintln!("[gauge_perf] interpolation frame at t={elapsed:.0}ms: {frame_ms}ms");
    }
    eprintln!(
        "[gauge_perf] interpolation frames (10 steps, 300ms transition): \
         max={max_elapsed_ms}ms — CI threshold: {CI_THRESHOLD_MS}ms, spec target (ref hw): {SPEC_TARGET_MS}ms",
    );
}
