//! CPU scene-graph calibration for the benchmark's hardware-factor vector.
//!
//! Runs a fixed publish workload plus a synthetic hash loop and reports
//! how much slower this machine is than the reference. The Windows and
//! constrained-envelope budget gates divide raw observations by this factor.

// Only the headless benchmark runs the workload; the result type is always used.
#![cfg_attr(not(feature = "headless"), allow(dead_code))]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, NotificationPayload, RenderingPolicy, SceneId,
    StatusBarPayload, WidgetDefinition, WidgetInstance, WidgetParamConstraints, WidgetParamType,
    WidgetParameterDeclaration, WidgetParameterValue, ZoneContent, ZoneRegistry,
};

/// Result of the CPU calibration workload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CpuCalibrationResult {
    /// How many times slower this machine is than the reference (1.0 = reference).
    pub speed_factor: f64,
    /// Scene-graph operations per second achieved.
    pub scene_ops_per_sec: f64,
    /// Synthetic hash throughput in MB/s.
    pub hash_throughput_mbps: f64,
    /// Unix timestamp (seconds) when calibration ran.
    pub timestamp: u64,
    /// Duration of the calibration run in microseconds.
    pub calibration_duration_us: u64,
}

// ─── Reference baselines ─────────────────────────────────────────────────────

/// Reference baseline: publish-workload scene operations per second.
///
/// The workload: 1,000 rounds of five publishes (a notification, a status-bar
/// entry, a subtitle line, two widget parameter updates) plus 100 hit tests,
/// about 5,100 operations. The constant is the earlier tile-mutation
/// workload's reference (550,000 ops/s) scaled by the ratio of the two
/// workloads' throughput on one host (336k / 110k ops/s, debug build), so a
/// host keeps the factor it had and factor-scaled budgets stay comparable.
const REFERENCE_SCENE_OPS_PER_SEC: f64 = 1_680_000.0;

/// Reference baseline: synthetic hash throughput in MB/s.
const REFERENCE_HASH_THROUGHPUT_MBPS: f64 = 800.0;

/// Publisher namespace of the calibration workload.
const AGENT: &str = "calibration";

/// Widget instances the workload publishes to.
const CALIBRATION_WIDGETS: usize = 8;

/// Publish rounds (five publishes each).
const CALIBRATION_ROUNDS: usize = 1_000;

/// Minimum speed factor (prevents unreasonably tight budgets).
const MIN_SPEED_FACTOR: f64 = 0.5;

/// Maximum speed factor (prevents unreasonably loose budgets on very slow machines).
const MAX_SPEED_FACTOR: f64 = 50.0;

/// Run the CPU calibration workload and compute the speed factor.
pub(crate) fn calibrate() -> CpuCalibrationResult {
    let overall_start = Instant::now();
    let scene_ops = run_scene_workload();
    let hash_mbps = run_hash_workload();

    // Scene ops dominate (80%) since that's what the budgets protect; hash
    // throughput (20%) catches general CPU speed differences.
    let scene_factor = REFERENCE_SCENE_OPS_PER_SEC / scene_ops.max(1.0);
    let hash_factor = REFERENCE_HASH_THROUGHPUT_MBPS / hash_mbps.max(0.01);
    let raw_factor = scene_factor * 0.8 + hash_factor * 0.2;
    let speed_factor = raw_factor.clamp(MIN_SPEED_FACTOR, MAX_SPEED_FACTOR);

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    CpuCalibrationResult {
        speed_factor,
        scene_ops_per_sec: scene_ops,
        hash_throughput_mbps: hash_mbps,
        timestamp: now_secs,
        calibration_duration_us: overall_start.elapsed().as_micros() as u64,
    }
}

/// A parameterized widget type with no SVG layers: the scene-side half of a
/// widget publish (validation, contention, parameter storage) without the
/// renderer's raster work, which the upload calibration measures.
pub(crate) fn level_widget(id: &str) -> WidgetDefinition {
    WidgetDefinition {
        id: id.to_string(),
        name: id.to_string(),
        description: "Benchmark level widget".to_string(),
        parameter_schema: vec![WidgetParameterDeclaration {
            audience: Default::default(),
            name: "level".to_string(),
            param_type: WidgetParamType::F32,
            default_value: WidgetParameterValue::F32(0.0),
            constraints: Some(WidgetParamConstraints {
                f32_min: Some(0.0),
                f32_max: Some(1.0),
                ..Default::default()
            }),
        }],
        layers: vec![],
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.2,
            height_pct: 0.1,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::LatestWins,
        max_publishers: WidgetDefinition::default_max_publishers(),
        ephemeral: false,
        hover_behavior: None,
    }
}

/// Register `count` instances `{prefix}-{i}` of widget type `type_name`.
pub(crate) fn register_instances(
    scene: &mut SceneGraph,
    tab_id: SceneId,
    type_name: &str,
    prefix: &str,
    count: usize,
    geometry: impl Fn(usize) -> Option<GeometryPolicy>,
) {
    for i in 0..count {
        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: type_name.to_string(),
            tab_id,
            geometry_override: geometry(i),
            contention_override: None,
            instance_name: format!("{prefix}-{i}"),
            current_params: HashMap::new(),
            auto_clear_ms: None,
        });
    }
}

/// One `level` parameter value.
pub(crate) fn level(value: f32) -> HashMap<String, WidgetParameterValue> {
    HashMap::from([("level".to_string(), WidgetParameterValue::F32(value))])
}

/// Run the publish workload and return scene operations per second.
///
/// The scene commits of what agents send, through the same scene calls
/// `hud_publish` makes: per round a notification, a status-bar entry, a
/// subtitle line and two widget parameter updates, then hit tests over the
/// resulting zones and widgets.
fn run_scene_workload() -> f64 {
    let start = Instant::now();
    let mut total_ops: u64 = 0;

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.zone_registry = ZoneRegistry::with_defaults();
    let tab_id = scene.create_tab("calibration", 0).expect("create_tab");
    scene
        .widget_registry
        .register_definition(level_widget("level"));
    register_instances(
        &mut scene,
        tab_id,
        "level",
        "level",
        CALIBRATION_WIDGETS,
        |_| None,
    );
    total_ops += 1;

    let mut count = |ok: bool| {
        assert!(ok, "a calibration publish was refused");
        total_ops += 1;
    };
    for round in 0..CALIBRATION_ROUNDS {
        let notification = ZoneContent::Notification(NotificationPayload {
            title: "Build".to_string(),
            text: format!("Step {round} passed."),
            ..Default::default()
        });
        count(
            scene
                .publish_to_zone("notification-area", notification, AGENT, None, None, None)
                .is_ok(),
        );
        let status = ZoneContent::StatusBar(StatusBarPayload {
            entries: HashMap::from([(format!("k{}", round % 4), format!("{round}"))]),
        });
        count(
            scene
                .publish_to_zone(
                    "status-bar",
                    status,
                    AGENT,
                    Some(format!("k{}", round % 4)),
                    None,
                    None,
                )
                .is_ok(),
        );
        let line = ZoneContent::StreamText(format!("line {round} of the calibration transcript"));
        count(
            scene
                .publish_to_zone("subtitle", line, AGENT, None, None, None)
                .is_ok(),
        );
        for w in [
            round % CALIBRATION_WIDGETS,
            (round + 3) % CALIBRATION_WIDGETS,
        ] {
            let value = (round % 101) as f32 / 100.0;
            count(
                scene
                    .publish_to_widget_for_lease(
                        &format!("level-{w}"),
                        level(value),
                        AGENT,
                        None,
                        0,
                        None,
                        None,
                    )
                    .is_ok(),
            );
        }
    }

    // Hit testing over the published zones and widgets is on the input path.
    for i in 0..100 {
        let x = (i as f32 * 19.2) % 1920.0;
        let y = (i as f32 * 10.8) % 1080.0;
        let _ = scene.hit_test(x, y);
        total_ops += 1;
    }

    let elapsed_secs = start.elapsed().as_secs_f64();
    total_ops as f64 / elapsed_secs.max(1e-9)
}

/// Run a synthetic CPU load workload and return throughput in MB/s.
///
/// Uses a simple iterative hash (FNV-1a inspired) to measure raw CPU
/// throughput without depending on external crates.
fn run_hash_workload() -> f64 {
    let start = Instant::now();
    let iterations = 100_000;
    let block_size = 64; // bytes per iteration
    let total_bytes = iterations * block_size;

    let mut state: u64 = 0xcbf29ce484222325; // FNV offset basis
    for i in 0..iterations {
        // FNV-1a style mixing
        for byte in 0..block_size {
            state ^= ((i * block_size + byte) & 0xFF) as u64;
            state = state.wrapping_mul(0x100000001b3); // FNV prime
        }
    }

    // Prevent the compiler from optimizing away the computation
    std::hint::black_box(state);

    let elapsed_secs = start.elapsed().as_secs_f64();
    let total_mb = total_bytes as f64 / (1024.0 * 1024.0);
    total_mb / elapsed_secs.max(1e-9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibrate_reports_a_clamped_positive_speed_factor() {
        let result = calibrate();
        assert!((MIN_SPEED_FACTOR..=MAX_SPEED_FACTOR).contains(&result.speed_factor));
        assert!(result.scene_ops_per_sec > 0.0);
        assert!(result.hash_throughput_mbps > 0.0);
    }
}
