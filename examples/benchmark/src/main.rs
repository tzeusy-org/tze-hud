//! # tze_hud Layer-3 Performance Validation Benchmark
//!
//! Runs the hardware-normalized calibration harness and performance scenarios,
//! then validates timing against normalized budgets.
//!
//! ## Usage
//!
//! ```sh
//! cargo run --bin benchmark --features headless -- --emit telemetry.json
//! cargo run --bin benchmark --features headless -- --emit telemetry.json --frames 300
//! cargo run --bin benchmark --features headless -- --emit telemetry.json --cpu-only
//! taskset --cpu-list 0-1 target/release/benchmark --constrained-envelope --emit telemetry.json
//! ```
//!
//! ## Calibration workloads
//!
//! 1. **CPU** — scene-graph mutations via [`cpu_calibration::calibrate`].
//! 2. **GPU** — fill/composition: render a fixed multi-tile scene at target
//!    resolution, measure frames per second over a warmup + timed window.
//! 3. **Upload** — texture upload: create and update texture-backed tiles
//!    in rapid succession, measure throughput.
//!
//! ## Scenarios
//!
//! The timed sessions are the publish path agents use (see `publish.rs`): a
//! notification, a widget parameter update, and a portal append, each one
//! `hud_publish` per frame over the real MCP listener. A paced scene-lock
//! contention session races resident publishes against the frame loop's
//! `try_lock`.
//!
//! ## Output (--emit path)
//!
//! Emits a JSON object to the specified path containing:
//! - `calibration`: hardware factors for all three dimensions
//! - `sessions`: array of per-scenario session summaries
//! - `validation`: `ValidationReport` with per-metric assertion outcomes
//!
//! ## Without --features headless
//!
//! Without the `headless` feature this binary prints a usage message and exits.
//! Only unit tests (serialization round-trips) are compiled unconditionally.

// Shared types and serialization structures — always compiled.
use serde::{Deserialize, Serialize};

mod cpu_calibration;
#[cfg(feature = "headless")]
mod publish;
// Only the headless benchmark run builds a report; the stub binary keeps the types for its tests.
#[cfg_attr(not(feature = "headless"), allow(dead_code))]
mod validation;

use cpu_calibration::CpuCalibrationResult;
use tze_hud_telemetry::SessionSummary;
use validation::{HardwareFactors, ValidationReport};

#[cfg(feature = "headless")]
const CALIBRATION_VECTOR_VERSION: &str = "tze_hud.cpu-gpu-upload.v1";
#[cfg(feature = "headless")]
const BENCHMARK_WIDTH: u32 = 1920;
#[cfg(feature = "headless")]
const BENCHMARK_HEIGHT: u32 = 1080;

/// Operating-system identity captured by the constrained benchmark lane.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperatingSystemIdentity {
    pub family: String,
    pub name: String,
    pub version: String,
    pub architecture: String,
}

/// Proof that the benchmark process actually ran with the requested CPU limit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CpuConstraintIdentity {
    pub model: String,
    pub logical_cpu_limit: usize,
    pub allowed_cpu_list: String,
    pub enforcement_mechanism: String,
    pub enforced: bool,
}

/// Optional memory constraint recorded for the lane.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryConstraintIdentity {
    pub limit_bytes: Option<u64>,
    pub enforcement_mechanism: String,
}

/// Actual renderer and adapter selected by the headless compositor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RendererAdapterIdentity {
    pub requested_software: bool,
    pub backend: String,
    pub adapter_identity: String,
    pub device_type: String,
    pub driver: String,
    pub driver_info: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub verified_software: bool,
}

/// Render target used by the versioned calibration and benchmark vectors.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewportIdentity {
    pub width: u32,
    pub height: u32,
}

/// Complete execution identity for the constrained-envelope proxy lane.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConstrainedProfileIdentity {
    pub schema: String,
    pub lane: String,
    pub low_power_proxy: bool,
    pub device_qualification: bool,
    pub operating_system: OperatingSystemIdentity,
    pub cpu: CpuConstraintIdentity,
    pub memory: MemoryConstraintIdentity,
    pub renderer: RendererAdapterIdentity,
    pub viewport: ViewportIdentity,
    pub calibration_vector_version: String,
}

/// Count unique CPUs described by Linux's CPU-list syntax (`0-2,5`).
#[cfg(any(feature = "headless", test))]
fn logical_cpu_count(cpu_list: &str) -> Option<usize> {
    let mut cpus = std::collections::BTreeSet::new();
    if cpu_list.trim().is_empty() {
        return None;
    }

    for segment in cpu_list.split(',') {
        let segment = segment.trim();
        let mut bounds = segment.split('-');
        let first = bounds.next()?.parse::<usize>().ok()?;
        let last = match bounds.next() {
            Some(value) => value.parse::<usize>().ok()?,
            None => first,
        };
        if bounds.next().is_some() || last < first {
            return None;
        }
        cpus.extend(first..=last);
    }
    (!cpus.is_empty()).then_some(cpus.len())
}

// ─── Shared output types (always compiled — needed for tests) ─────────────────

/// GPU fill/composition calibration result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GpuCalibrationResult {
    /// GPU frames per second achieved during calibration.
    pub fps: f64,
    /// Hardware factor: reference_fps / observed_fps (>1 = slower than reference).
    pub gpu_factor: f64,
    /// Duration of the calibration run in microseconds.
    pub calibration_duration_us: u64,
    /// Actual adapter selected by the calibration runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<RendererAdapterIdentity>,
}

/// Texture-upload calibration result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadCalibrationResult {
    /// Tile operations per second achieved during upload calibration.
    pub tile_ops_per_sec: f64,
    /// Hardware factor: reference_ops / observed_ops.
    pub upload_factor: f64,
    /// Duration of the calibration run in microseconds.
    pub calibration_duration_us: u64,
}

/// Result of one benchmark scenario.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub name: String,
    pub summary: SessionSummary,
}

/// Serializable calibration summary.
#[derive(Debug, Serialize, Deserialize)]
pub struct CalibrationOutput {
    /// CPU scene-graph calibration result.
    pub cpu: CpuCalibrationResult,
    /// GPU fill/composition calibration result.  None if --cpu-only.
    pub gpu: Option<GpuCalibrationResult>,
    /// Upload calibration result.  None if --cpu-only.
    pub upload: Option<UploadCalibrationResult>,
    /// Normalized hardware factors used for budget assertions.
    pub(crate) factors: HardwareFactors,
}

/// Full benchmark output emitted to `--emit path`.
#[derive(Debug, Serialize, Deserialize)]
pub struct BenchmarkOutput {
    /// Hardware calibration results.
    pub calibration: CalibrationOutput,
    /// Per-scenario session summaries.
    pub sessions: Vec<ScenarioResult>,
    /// Layer-3 validation report (runs against the notification session).
    pub(crate) validation: ValidationReport,
    /// Present only when `--constrained-envelope` requests fail-closed proxy proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrained_profile: Option<ConstrainedProfileIdentity>,
}

// ─── Headless implementation ──────────────────────────────────────────────────
// Everything below requires the `headless` feature.

#[cfg(feature = "headless")]
mod headless_impl {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use std::time::{Duration, Instant};
    use tokio::sync::{Mutex, oneshot};
    use tracing::{info, warn};

    use tze_hud_telemetry::FrameTelemetry;

    use crate::cpu_calibration::calibrate as calibrate_cpu;
    use tze_hud_runtime::headless::{HeadlessConfig, HeadlessRuntime};
    use tze_hud_scene::graph::SceneGraph;
    use tze_hud_scene::mutation::{MutationBatch, SceneMutation};
    use tze_hud_scene::types::{Node, NodeData, Rect, Rgba, SceneId, SolidColorNode};

    // ── Budget constants ─────────────────────────────────────────────────────

    /// Reference GPU throughput: frames per second on reference hardware
    /// (modern dGPU, simple multi-tile scene, 1920x1080).
    const REFERENCE_GPU_FPS: f64 = 500.0;

    /// Number of calibration frames to render for GPU calibration.
    const GPU_CALIBRATION_FRAMES: u64 = 50;

    /// Tile count for GPU calibration scene (multi-tile with alpha blending).
    const GPU_CALIBRATION_TILES: usize = 20;

    /// Reference upload throughput: tile create+update+delete ops/sec on reference hardware.
    const REFERENCE_UPLOAD_OPS_PER_SEC: f64 = 5_000.0;

    /// Number of upload calibration cycles.
    const UPLOAD_CALIBRATION_CYCLES: usize = 100;

    /// Tiles per upload cycle.
    const UPLOAD_TILES_PER_CYCLE: usize = 10;

    async fn scene_handle(runtime: &HeadlessRuntime) -> Arc<Mutex<SceneGraph>> {
        let state = runtime.state.lock().await;
        state.scene.clone()
    }

    fn benchmark_config(psk: &str) -> HeadlessConfig {
        HeadlessConfig {
            width: BENCHMARK_WIDTH,
            height: BENCHMARK_HEIGHT,
            grpc_port: 0,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted(psk),
            config_toml: Some(String::new()),
        }
    }

    // ── CLI args ─────────────────────────────────────────────────────────────

    pub(crate) struct Args {
        /// Path to emit the telemetry JSON to.
        pub emit: Option<PathBuf>,
        /// Number of benchmark frames to render per scenario.
        pub frames: u64,
        /// Skip GPU and upload calibration (CPU-only mode for faster iteration).
        pub cpu_only: bool,
        /// Emit execution identity for the two-CPU software-renderer proxy lane.
        pub constrained_envelope: bool,
    }

    pub(crate) fn parse_args() -> Args {
        let args: Vec<String> = std::env::args().collect();
        let mut emit = None;
        let mut frames = 120u64;
        let mut cpu_only = false;
        let mut constrained_envelope = false;

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--emit" => {
                    i += 1;
                    if let Some(path_str) = args.get(i) {
                        emit = Some(PathBuf::from(path_str));
                    } else {
                        eprintln!("Error: --emit requires a path argument");
                        eprintln!("Usage: --emit <path>");
                        std::process::exit(1);
                    }
                }
                "--frames" => {
                    i += 1;
                    if let Some(n_str) = args.get(i) {
                        match n_str.parse::<u64>() {
                            Ok(n) => frames = n,
                            Err(_) => {
                                eprintln!(
                                    "Error: --frames requires a positive integer, got '{n_str}'"
                                );
                                eprintln!("Usage: --frames <number>");
                                std::process::exit(1);
                            }
                        }
                    } else {
                        eprintln!("Error: --frames requires a numeric argument");
                        eprintln!("Usage: --frames <number>");
                        std::process::exit(1);
                    }
                }
                "--cpu-only" => {
                    cpu_only = true;
                }
                "--constrained-envelope" => {
                    constrained_envelope = true;
                }
                _ => {}
            }
            i += 1;
        }

        Args {
            emit,
            frames,
            cpu_only,
            constrained_envelope,
        }
    }

    fn os_release_value(contents: &str, key: &str) -> Option<String> {
        contents.lines().find_map(|line| {
            let (candidate, value) = line.split_once('=')?;
            (candidate == key).then(|| value.trim_matches('"').to_string())
        })
    }

    fn cpu_model() -> String {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|contents| {
                contents.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    (key.trim() == "model name").then(|| value.trim().to_string())
                })
            })
            .or_else(|| std::env::var("PROCESSOR_IDENTIFIER").ok())
            .unwrap_or_default()
    }

    fn allowed_cpu_list() -> String {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|contents| {
                contents.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    (key.trim() == "Cpus_allowed_list").then(|| value.trim().to_string())
                })
            })
            .unwrap_or_default()
    }

    fn memory_constraint() -> MemoryConstraintIdentity {
        let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
            .ok()
            .and_then(|value| {
                let value = value.trim();
                (value != "max")
                    .then(|| value.parse::<u64>().ok())
                    .flatten()
            });
        MemoryConstraintIdentity {
            limit_bytes: limit,
            enforcement_mechanism: if limit.is_some() {
                "cgroup v2 memory.max".to_string()
            } else {
                "none".to_string()
            },
        }
    }

    fn collect_constrained_profile(
        renderer: Option<RendererAdapterIdentity>,
    ) -> ConstrainedProfileIdentity {
        let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let allowed_cpu_list = allowed_cpu_list();
        let logical_cpu_limit = logical_cpu_count(&allowed_cpu_list).unwrap_or(0);
        let family = std::env::consts::OS.to_string();

        ConstrainedProfileIdentity {
            schema: "tze_hud.constrained_profile.v1".to_string(),
            lane: if family == "linux" {
                "llvmpipe-two-logical-cpus".to_string()
            } else {
                "warp-two-logical-cpus".to_string()
            },
            low_power_proxy: true,
            device_qualification: false,
            operating_system: OperatingSystemIdentity {
                family,
                name: os_release_value(&os_release, "NAME").unwrap_or_default(),
                version: os_release_value(&os_release, "VERSION_ID").unwrap_or_default(),
                architecture: std::env::consts::ARCH.to_string(),
            },
            cpu: CpuConstraintIdentity {
                model: cpu_model(),
                logical_cpu_limit,
                allowed_cpu_list,
                enforcement_mechanism: "linux sched affinity (taskset)".to_string(),
                enforced: logical_cpu_limit == 2,
            },
            memory: memory_constraint(),
            renderer: renderer.unwrap_or(RendererAdapterIdentity {
                requested_software: false,
                backend: String::new(),
                adapter_identity: String::new(),
                device_type: String::new(),
                driver: String::new(),
                driver_info: String::new(),
                vendor_id: 0,
                device_id: 0,
                verified_software: false,
            }),
            viewport: ViewportIdentity {
                width: BENCHMARK_WIDTH,
                height: BENCHMARK_HEIGHT,
            },
            calibration_vector_version: CALIBRATION_VECTOR_VERSION.to_string(),
        }
    }

    /// Set the per-frame correctness fields `HeadlessRuntime::render_frame()`
    /// never writes, so the gated `invariant_violations` / `scene_lock_misses`
    /// counters come from a real computation, not `FrameTelemetry::new()`'s
    /// default. `invariant_violations` is the caller's own refused-work count
    /// for the frame; `scene_lock_miss_count` is the running total of real
    /// `scene.try_lock()` misses (the windowed loop's semantics).
    fn attach_frame_correctness(
        telemetry: &mut FrameTelemetry,
        invariant_violations: u32,
        scene_lock_miss_count: u64,
    ) {
        telemetry.invariant_violations_this_frame = invariant_violations;
        telemetry.scene_lock_miss_count = scene_lock_miss_count;
    }

    // ── GPU calibration ───────────────────────────────────────────────────────

    /// Run the GPU fill/composition calibration workload.
    ///
    /// Renders a fixed multi-tile scene with overlapping alpha-blended tiles at
    /// target resolution (1920x1080). Returns FPS and the hardware factor.
    pub(crate) async fn calibrate_gpu() -> GpuCalibrationResult {
        let start = Instant::now();

        let config = benchmark_config("calibration");

        let mut runtime = match HeadlessRuntime::new(config).await {
            Ok(r) => r,
            Err(e) => {
                warn!("GPU calibration failed to init runtime: {}", e);
                return GpuCalibrationResult {
                    fps: 0.0,
                    gpu_factor: f64::NAN,
                    calibration_duration_us: start.elapsed().as_micros() as u64,
                    adapter: None,
                };
            }
        };
        let adapter_info = runtime.compositor.adapter_info();
        let requested_software =
            std::env::var("HEADLESS_FORCE_SOFTWARE").is_ok_and(|value| value.trim() == "1");
        let adapter_name = adapter_info.name.to_lowercase();
        let verified_software = requested_software
            && adapter_info.device_type.eq_ignore_ascii_case("cpu")
            && ((cfg!(target_os = "linux")
                && adapter_info.backend.eq_ignore_ascii_case("vulkan")
                && (adapter_name.contains("llvmpipe") || adapter_name.contains("softpipe")))
                || (cfg!(target_os = "windows")
                    && adapter_info.backend.eq_ignore_ascii_case("dx12")
                    && adapter_name.contains("warp")));
        let selected_adapter = Some(RendererAdapterIdentity {
            requested_software,
            backend: adapter_info.backend.clone(),
            adapter_identity: adapter_info.name.clone(),
            device_type: adapter_info.device_type.clone(),
            driver: adapter_info.driver.clone(),
            driver_info: adapter_info.driver_info.clone(),
            vendor_id: adapter_info.vendor,
            device_id: adapter_info.device,
            verified_software,
        });

        // Set up a multi-tile scene with overlapping alpha-blended tiles
        {
            let scene_arc = scene_handle(&runtime).await;
            let mut scene = scene_arc.lock().await;
            let tab_id = match scene.create_tab("gpu_calibration", 0) {
                Ok(id) => id,
                Err(e) => {
                    warn!("GPU calibration: create_tab failed: {:?}", e);
                    return GpuCalibrationResult {
                        fps: 0.0,
                        gpu_factor: f64::NAN,
                        calibration_duration_us: start.elapsed().as_micros() as u64,
                        adapter: selected_adapter,
                    };
                }
            };

            let lease_id = scene.grant_lease("gpu_calibration", 300_000);
            if let Some(lease) = scene.leases.get_mut(&lease_id) {
                lease.resource_budget.max_tiles = (GPU_CALIBRATION_TILES + 5) as u32;
            }

            let cols = 5usize;
            for i in 0..GPU_CALIBRATION_TILES {
                let col = i % cols;
                let row = i / cols;
                // Tiles overlap slightly for alpha-blending composition load
                let bounds = Rect::new(col as f32 * 350.0, row as f32 * 250.0, 380.0, 270.0);
                if let Ok(tile_id) =
                    scene.create_tile(tab_id, "gpu_calibration", lease_id, bounds, (i + 1) as u32)
                {
                    let alpha = if i % 2 == 0 { 1.0f32 } else { 0.7f32 };
                    let node = Node {
                        layout: Default::default(),
                        id: SceneId::new(),
                        children: vec![],
                        data: NodeData::SolidColor(SolidColorNode {
                            color: Rgba::new(
                                i as f32 / GPU_CALIBRATION_TILES as f32,
                                0.5,
                                0.8,
                                alpha,
                            ),
                            bounds: Rect::new(0.0, 0.0, 380.0, 270.0),
                            radius: None,
                        }),
                    };
                    let _ = scene.set_tile_root(tile_id, node);
                }
            }
        }

        // Warmup: 5 frames (not measured)
        for _ in 0..5 {
            runtime.render_frame().await;
        }

        // Timed window: GPU_CALIBRATION_FRAMES frames
        let timed_start = Instant::now();
        for _ in 0..GPU_CALIBRATION_FRAMES {
            runtime.render_frame().await;
        }
        let timed_elapsed = timed_start.elapsed();

        let elapsed_secs = timed_elapsed.as_secs_f64();
        let fps = GPU_CALIBRATION_FRAMES as f64 / elapsed_secs.max(1e-9);
        let gpu_factor = (REFERENCE_GPU_FPS / fps).clamp(0.01, 100.0);

        GpuCalibrationResult {
            fps,
            gpu_factor,
            calibration_duration_us: start.elapsed().as_micros() as u64,
            adapter: selected_adapter,
        }
    }

    // ── Upload calibration ────────────────────────────────────────────────────

    /// Run the texture-upload calibration workload.
    ///
    /// Creates and destroys tiles in rapid succession to measure the create/update/delete
    /// throughput, which is the primary bottleneck for texture-upload-heavy workloads.
    pub(crate) async fn calibrate_upload() -> UploadCalibrationResult {
        let start = Instant::now();

        let config = benchmark_config("calibration");

        let mut runtime = match HeadlessRuntime::new(config).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Upload calibration failed to init runtime: {}", e);
                return UploadCalibrationResult {
                    tile_ops_per_sec: 0.0,
                    upload_factor: f64::NAN,
                    calibration_duration_us: start.elapsed().as_micros() as u64,
                };
            }
        };

        let tab_id;
        let lease_id;
        {
            let scene_arc = scene_handle(&runtime).await;
            let mut scene = scene_arc.lock().await;
            tab_id = match scene.create_tab("upload_calibration", 0) {
                Ok(id) => id,
                Err(e) => {
                    warn!("Upload calibration: create_tab failed: {:?}", e);
                    return UploadCalibrationResult {
                        tile_ops_per_sec: 0.0,
                        upload_factor: f64::NAN,
                        calibration_duration_us: start.elapsed().as_micros() as u64,
                    };
                }
            };
            lease_id = scene.grant_lease("upload_calibration", 300_000);
            if let Some(lease) = scene.leases.get_mut(&lease_id) {
                lease.resource_budget.max_tiles = (UPLOAD_TILES_PER_CYCLE + 5) as u32;
            }
        }

        // Timed upload cycle: create N tiles, render, delete N tiles, repeat
        let timed_start = Instant::now();
        let mut total_ops: u64 = 0;

        for cycle in 0..UPLOAD_CALIBRATION_CYCLES {
            let mut tile_ids = Vec::with_capacity(UPLOAD_TILES_PER_CYCLE);

            // Create UPLOAD_TILES_PER_CYCLE tiles
            {
                let scene_arc = scene_handle(&runtime).await;
                let mut scene = scene_arc.lock().await;
                for i in 0..UPLOAD_TILES_PER_CYCLE {
                    let bounds = Rect::new(
                        (i as f32 % 10.0) * 190.0,
                        (i as f32 / 10.0).floor() * 180.0,
                        180.0,
                        170.0,
                    );
                    if let Ok(tile_id) = scene.create_tile(
                        tab_id,
                        "upload_calibration",
                        lease_id,
                        bounds,
                        (i + 1) as u32,
                    ) {
                        let node = Node {
                            layout: Default::default(),
                            id: SceneId::new(),
                            children: vec![],
                            data: NodeData::SolidColor(SolidColorNode {
                                color: Rgba::new(
                                    cycle as f32 / UPLOAD_CALIBRATION_CYCLES as f32,
                                    0.5,
                                    0.5,
                                    1.0,
                                ),
                                bounds: Rect::new(0.0, 0.0, 180.0, 170.0),
                                radius: None,
                            }),
                        };
                        // Count create unconditionally (it succeeded); count
                        // set_root only if it also succeeds, to avoid inflating
                        // throughput when the scene mutation fails.
                        total_ops += 1; // create
                        if scene.set_tile_root(tile_id, node).is_ok() {
                            total_ops += 1; // set_root
                        }
                        tile_ids.push(tile_id);
                    }
                }
            }

            // Render one frame with the tiles visible
            runtime.render_frame().await;

            // Delete all tiles (models a full tile lifecycle per cycle)
            {
                let scene_arc = scene_handle(&runtime).await;
                let mut scene = scene_arc.lock().await;
                for tile_id in &tile_ids {
                    let batch = MutationBatch {
                        batch_id: SceneId::new(),
                        agent_namespace: "upload_calibration".to_string(),
                        mutations: vec![SceneMutation::DeleteTile { tile_id: *tile_id }],
                        timing_hints: None,
                        lease_id: None,
                    };
                    let _ = scene.apply_batch(&batch);
                    total_ops += 1;
                }
            }
        }

        let timed_elapsed_secs = timed_start.elapsed().as_secs_f64();
        let tile_ops_per_sec = total_ops as f64 / timed_elapsed_secs.max(1e-9);
        let upload_factor = (REFERENCE_UPLOAD_OPS_PER_SEC / tile_ops_per_sec).clamp(0.01, 100.0);

        UploadCalibrationResult {
            tile_ops_per_sec,
            upload_factor,
            calibration_duration_us: start.elapsed().as_micros() as u64,
        }
    }

    // ── Benchmark scenarios ───────────────────────────────────────────────────

    // ── Scene-lock contention scenario ────────────────────────────

    /// Number of concurrent background scene-mutation tasks to spawn for the
    /// contention scenario. Each models a gRPC/MCP session handler that takes
    /// the scene lock (`.lock().await`) to apply a batch — exactly the class of
    /// handler that contends the windowed compositor's `try_lock` frame loop.
    const CONTENTION_MUTATION_TASKS: usize = 3;

    /// How long each background mutation task holds the scene lock per
    /// acquisition. This is the contention window the frame loop's `try_lock`
    /// races against. Kept short so the scenario stays fast, but long enough to
    /// reliably overlap a frame attempt under the deterministic handshake below.
    const CONTENTION_HOLD: Duration = Duration::from_micros(400);

    /// CI's Windows performance gate currently runs the benchmark for 180
    /// frames. The paced production-shaped contention model below schedules 18
    /// real handler lock holds in that window; the committed ceiling keeps a
    /// two-miss margin over the observed local result while still rejecting
    /// saturation-style contention.
    pub(crate) const PRODUCTION_CONTENTION_CEILING_PER_180_FRAMES: u64 = 20;

    /// A 60Hz frame loop sees 30 frames per half second. The paced model
    /// schedules one mutation-handler lock hold for each of three resident
    /// agents in that half-second window (aggregate 6Hz), which is a bounded
    /// production-shaped fraction rather than a worst-case every-frame hold.
    const PRODUCTION_CONTENTION_PERIOD_FRAMES: u64 = 30;
    const PRODUCTION_CONTENTION_FRAME_PHASES: [u64; CONTENTION_MUTATION_TASKS] = [5, 15, 25];
    const PRODUCTION_CONTENTION_FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
    const PRODUCTION_CONTENTION_READY_TIMEOUT: Duration = Duration::from_millis(50);

    pub(crate) fn production_contention_task_index(frame_idx: u64) -> Option<usize> {
        let phase = frame_idx % PRODUCTION_CONTENTION_PERIOD_FRAMES;
        PRODUCTION_CONTENTION_FRAME_PHASES
            .iter()
            .position(|candidate| *candidate == phase)
    }

    pub(crate) fn production_contention_target_frames(frame_count: u64) -> u64 {
        (0..frame_count)
            .filter(|frame_idx| production_contention_task_index(*frame_idx).is_some())
            .count() as u64
    }

    async fn wait_for_contention_ready(ready: oneshot::Receiver<()>, frame_idx: u64) {
        if tokio::time::timeout(PRODUCTION_CONTENTION_READY_TIMEOUT, ready)
            .await
            .is_err()
        {
            warn!(
                "paced contention holder did not acquire the scene lock before frame {}",
                frame_idx
            );
        }
    }

    /// Test-only no-GPU probe for the paced contention model. It uses the same
    /// `tokio::sync::Mutex::try_lock` acquisition and telemetry rollup as the
    /// benchmark session, but avoids `HeadlessRuntime` so the model's counter
    /// behavior stays cheap to test.
    #[cfg(test)]
    pub(crate) async fn run_paced_contention_probe_for_test(frame_count: u64) -> u64 {
        let scene: Arc<Mutex<SceneGraph>> = Arc::new(Mutex::new(SceneGraph::new(1920.0, 1080.0)));
        let mut scene_lock_miss_count = 0u64;
        let mut summary = SessionSummary::new();

        for frame_idx in 0..frame_count {
            let holder = if production_contention_task_index(frame_idx).is_some() {
                let scene = Arc::clone(&scene);
                let (ready_tx, ready_rx) = oneshot::channel();
                let handle = tokio::spawn(async move {
                    let _guard = scene.lock().await;
                    let _ = ready_tx.send(());
                    tokio::time::sleep(CONTENTION_HOLD).await;
                });
                wait_for_contention_ready(ready_rx, frame_idx).await;
                Some(handle)
            } else {
                None
            };

            if scene.try_lock().is_err() {
                scene_lock_miss_count = scene_lock_miss_count.saturating_add(1);
            }

            let mut telemetry = FrameTelemetry::new(frame_idx);
            telemetry.scene_lock_miss_count = scene_lock_miss_count;
            summary.record_frame_correctness(&telemetry);

            if let Some(handle) = holder {
                let _ = handle.await;
            }
        }

        summary.scene_lock_misses
    }

    /// One resident agent's publish: the handler takes the scene lock and
    /// admits a status publish, then holds the lock for the contention window.
    fn spawn_paced_contention_holder(
        scene_arc: Arc<Mutex<SceneGraph>>,
        task_idx: usize,
        tick: u64,
    ) -> (oneshot::Receiver<()>, tokio::task::JoinHandle<bool>) {
        let (ready_tx, ready_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let mut scene = scene_arc.lock().await;
            let accepted = scene
                .publish_to_zone(
                    "status-bar",
                    tze_hud_scene::types::ZoneContent::StatusBar(
                        tze_hud_scene::types::StatusBarPayload {
                            entries: std::collections::HashMap::from([(
                                format!("agent-{task_idx}"),
                                format!("tick {tick}"),
                            )]),
                        },
                    ),
                    &format!("paced-contention-{task_idx}"),
                    Some(format!("agent-{task_idx}")),
                    None,
                    None,
                )
                .is_ok();
            let _ = ready_tx.send(());
            tokio::time::sleep(CONTENTION_HOLD).await;
            accepted
        });
        (ready_rx, handle)
    }

    /// Run the gated, production-shaped scene-lock contention scenario.
    ///
    /// It does **not** force a lock hold on every frame. It models a 60Hz frame
    /// loop with three resident agents admitting zone publishes at a bounded
    /// aggregate cadence (three contended frames per 30-frame half-second).
    /// Each scheduled hold still races the real `tokio::sync::Mutex::try_lock`
    /// frame-loop path and rolls misses through `FrameTelemetry` into
    /// `SessionSummary`; the difference is the production-shaped frequency.
    pub(crate) async fn run_scene_lock_paced_contention(frame_count: u64) -> ScenarioResult {
        info!(
            "Running paced scene-lock contention scenario ({} frames, {} targeted holds; CI ceiling <= {} per 180 frames)",
            frame_count,
            production_contention_target_frames(frame_count),
            PRODUCTION_CONTENTION_CEILING_PER_180_FRAMES,
        );

        let config = benchmark_config("paced_contention_bench");
        let runtime = HeadlessRuntime::new(config)
            .await
            .expect("HeadlessRuntime::new failed");

        {
            let scene_arc = scene_handle(&runtime).await;
            scene_arc.lock().await.zone_registry =
                tze_hud_scene::types::ZoneRegistry::with_defaults();
        }
        let scene_arc = scene_handle(&runtime).await;
        let mut scene_lock_miss_count: u64 = 0;
        let session_start = Instant::now();
        let mut summary = SessionSummary::new();

        for frame_idx in 0..frame_count {
            let holder = if let Some(task_idx) = production_contention_task_index(frame_idx) {
                let (ready, handle) =
                    spawn_paced_contention_holder(Arc::clone(&scene_arc), task_idx, frame_idx);
                wait_for_contention_ready(ready, frame_idx).await;
                Some(handle)
            } else {
                None
            };

            let frame_start = Instant::now();
            if scene_arc.try_lock().is_err() {
                scene_lock_miss_count = scene_lock_miss_count.saturating_add(1);
            }
            let frame_time_us = frame_start.elapsed().as_micros() as u64;
            let invariant_violations_this_frame = if let Some(handle) = holder {
                match handle.await {
                    Ok(true) => 0,
                    Ok(false) => 1,
                    Err(err) => {
                        warn!("paced contention holder failed to join: {}", err);
                        1
                    }
                }
            } else {
                0
            };

            let mut telemetry = FrameTelemetry::new(frame_idx);
            telemetry.frame_time_us = frame_time_us;
            attach_frame_correctness(
                &mut telemetry,
                invariant_violations_this_frame,
                scene_lock_miss_count,
            );
            summary.record_frame(telemetry.frame_time_us, telemetry.tile_count);
            summary.record_frame_correctness(&telemetry);
            summary
                .input_to_next_present
                .record(telemetry.frame_time_us);

            tokio::time::sleep(PRODUCTION_CONTENTION_FRAME_INTERVAL).await;
        }

        summary.elapsed_us = session_start.elapsed().as_micros() as u64;
        summary.finalize();

        info!(
            "  scene_lock_paced_contention: total_frames={}, scene_lock_misses={}",
            summary.total_frames, summary.scene_lock_misses,
        );

        ScenarioResult {
            name: "scene_lock_paced_contention".to_string(),
            summary,
        }
    }

    // ── Entry point ───────────────────────────────────────────────────────────

    pub(crate) async fn run() {
        use crate::validation::AssertionOutcome;

        tracing_subscriber::fmt()
            .with_env_filter(
                std::env::var("RUST_LOG")
                    .unwrap_or_else(|_| "benchmark=info,tze_hud_runtime=warn".to_string())
                    .as_str(),
            )
            .init();

        let args = parse_args();

        // ── Phase 1: Calibration ─────────────────────────────────────────────
        info!("=== tze_hud Layer-3 Benchmark ===");
        info!("Phase 1: Hardware calibration");

        info!("  Running CPU scene-graph calibration...");
        let cpu_result = calibrate_cpu();
        info!(
            "  CPU: speed_factor={:.2}, scene_ops/s={:.0}, hash_mbps={:.1}",
            cpu_result.speed_factor, cpu_result.scene_ops_per_sec, cpu_result.hash_throughput_mbps,
        );

        let (gpu_result, upload_result) = if args.cpu_only {
            warn!("  --cpu-only: skipping GPU and upload calibration");
            (None, None)
        } else {
            info!("  Running GPU fill/composition calibration...");
            let gpu = calibrate_gpu().await;
            if gpu.gpu_factor.is_nan() {
                warn!("  GPU calibration failed (no suitable adapter?); using uncalibrated mode");
            } else {
                info!(
                    "  GPU: fps={:.1}, gpu_factor={:.2}",
                    gpu.fps, gpu.gpu_factor
                );
            }

            info!("  Running texture-upload calibration...");
            let upload = calibrate_upload().await;
            if upload.upload_factor.is_nan() {
                warn!("  Upload calibration failed; using uncalibrated mode");
            } else {
                info!(
                    "  Upload: tile_ops/s={:.0}, upload_factor={:.2}",
                    upload.tile_ops_per_sec, upload.upload_factor,
                );
            }

            (Some(gpu), Some(upload))
        };

        // Build HardwareFactors (NaN → None for uncalibrated dimensions)
        let factors = HardwareFactors {
            cpu: Some(cpu_result.speed_factor),
            gpu: gpu_result.as_ref().and_then(|g| {
                if g.gpu_factor.is_nan() {
                    None
                } else {
                    Some(g.gpu_factor)
                }
            }),
            upload: upload_result.as_ref().and_then(|u| {
                if u.upload_factor.is_nan() {
                    None
                } else {
                    Some(u.upload_factor)
                }
            }),
        };

        info!(
            "  Hardware factors: cpu={:?}, gpu={:?}, upload={:?}",
            factors.cpu, factors.gpu, factors.upload,
        );

        // ── Phase 2: Benchmark scenarios ─────────────────────────────────────
        info!("Phase 2: Benchmark scenarios ({} frames each)", args.frames);

        // Publish-path scenarios: one agent call per frame over the real MCP
        // listener (admission, scene commit, reply), then the frame it causes.
        let mut publishes = Vec::new();
        for publish in [
            crate::publish::Publish::Notification,
            crate::publish::Publish::WidgetParams,
            crate::publish::Publish::PortalAppend,
        ] {
            let result = crate::publish::run(publish, args.frames).await;
            info!(
                "  {}: frames={}, p50_publish={}µs, p99_publish={}µs, p99_frame_time={}µs",
                result.name,
                result.summary.total_frames,
                result.summary.input_to_scene_commit.p50().unwrap_or(0),
                result.summary.input_to_scene_commit.p99().unwrap_or(0),
                result.summary.frame_time.p99().unwrap_or(0),
            );
            publishes.push(result);
        }

        // Production-shaped paced scene-lock contention: this is the gated
        // non-zero counter session. It uses the same real try_lock path as the
        // saturation reference above, but schedules contention on a bounded
        // 60Hz-shaped fraction of frames so the CI checker can enforce a
        // data-derived ceiling without weakening the zero baseline for
        // steady_state_render / high_mutation.
        let scene_lock_paced_contention = run_scene_lock_paced_contention(args.frames).await;
        info!(
            "  scene_lock_paced_contention: total_frames={}, scene_lock_misses={}",
            scene_lock_paced_contention.summary.total_frames,
            scene_lock_paced_contention.summary.scene_lock_misses,
        );

        // ── Phase 3: Validation ───────────────────────────────────────────────
        info!("Phase 3: Layer-3 budget validation");

        let validation = ValidationReport::run(&publishes[0].summary, &factors);
        info!("  Verdict: {}", validation.verdict);
        for assertion in &validation.assertions {
            match assertion {
                AssertionOutcome::Pass {
                    metric,
                    observed,
                    budget,
                    ..
                } => {
                    info!("  PASS  {}: {}µs ≤ {}µs", metric, observed, budget);
                }
                AssertionOutcome::Fail {
                    metric,
                    observed,
                    budget,
                    overage_pct,
                    ..
                } => {
                    warn!(
                        "  FAIL  {}: {}µs > {}µs ({:.1}% over budget)",
                        metric, observed, budget, overage_pct,
                    );
                }
                AssertionOutcome::Uncalibrated {
                    metric,
                    reason,
                    raw_value,
                } => {
                    warn!("  UNCAL {}: raw={}µs ({})", metric, raw_value, reason);
                }
                AssertionOutcome::NoSamples { metric } => {
                    warn!("  NOSAMPLES {}", metric);
                }
            }
        }

        // ── Emit output ───────────────────────────────────────────────────────
        let constrained_profile = args.constrained_envelope.then(|| {
            collect_constrained_profile(
                gpu_result
                    .as_ref()
                    .and_then(|result| result.adapter.clone()),
            )
        });
        let output = BenchmarkOutput {
            calibration: CalibrationOutput {
                cpu: cpu_result,
                gpu: gpu_result,
                upload: upload_result,
                factors,
            },
            sessions: publishes
                .into_iter()
                .chain([scene_lock_paced_contention])
                .collect(),
            validation,
            constrained_profile,
        };

        let json =
            serde_json::to_string_pretty(&output).expect("failed to serialize benchmark output");

        if let Some(path) = &args.emit {
            std::fs::write(path, &json).unwrap_or_else(|e| {
                eprintln!("Failed to write telemetry to {path:?}: {e}");
            });
            info!("Telemetry written to {:?}", path);
        } else {
            println!("{json}");
        }

        // Exit with non-zero if any definitive failures
        if output.validation.fail_count > 0 {
            std::process::exit(1);
        }
    }
} // mod headless_impl

// ─── Entry points ─────────────────────────────────────────────────────────────

#[cfg(feature = "headless")]
#[tokio::main]
async fn main() {
    headless_impl::run().await;
}

#[cfg(not(feature = "headless"))]
fn main() {
    eprintln!(
        "The benchmark binary requires the `headless` feature.\n\
         Run with:\n\
         \n  cargo run --bin benchmark --features headless -- --emit telemetry.json\n"
    );
    std::process::exit(1);
}

// ─── Unit tests (always compiled) ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constrained_cpu_list_counts_ranges_and_singletons() {
        assert_eq!(logical_cpu_count("0-1"), Some(2));
        assert_eq!(logical_cpu_count("0,2-3,7"), Some(4));
        assert_eq!(logical_cpu_count("3"), Some(1));
        assert_eq!(logical_cpu_count("3-1"), None);
        assert_eq!(logical_cpu_count(""), None);
    }

    #[test]
    fn constrained_profile_is_explicitly_a_non_device_proxy() {
        let profile = ConstrainedProfileIdentity {
            schema: "tze_hud.constrained_profile.v1".to_string(),
            lane: "llvmpipe-two-logical-cpus".to_string(),
            low_power_proxy: true,
            device_qualification: false,
            operating_system: OperatingSystemIdentity {
                family: "linux".to_string(),
                name: "Ubuntu".to_string(),
                version: "24.04".to_string(),
                architecture: "x86_64".to_string(),
            },
            cpu: CpuConstraintIdentity {
                model: "CI proxy".to_string(),
                logical_cpu_limit: 2,
                allowed_cpu_list: "0-1".to_string(),
                enforcement_mechanism: "linux sched affinity".to_string(),
                enforced: true,
            },
            memory: MemoryConstraintIdentity {
                limit_bytes: None,
                enforcement_mechanism: "none".to_string(),
            },
            renderer: RendererAdapterIdentity {
                requested_software: true,
                backend: "Vulkan".to_string(),
                adapter_identity: "llvmpipe".to_string(),
                device_type: "Cpu".to_string(),
                driver: "llvmpipe".to_string(),
                driver_info: "Mesa 24.2".to_string(),
                vendor_id: 0,
                device_id: 0,
                verified_software: true,
            },
            viewport: ViewportIdentity {
                width: 1920,
                height: 1080,
            },
            calibration_vector_version: "tze_hud.cpu-gpu-upload.v1".to_string(),
        };

        let json = serde_json::to_value(&profile).unwrap();
        assert_eq!(json["low_power_proxy"], true);
        assert_eq!(json["device_qualification"], false);
        assert_eq!(json["cpu"]["logical_cpu_limit"], 2);
        assert_eq!(json["renderer"]["verified_software"], true);
    }

    #[test]
    fn test_benchmark_output_serializes_round_trip() {
        let cpu = CpuCalibrationResult {
            speed_factor: 1.0,
            scene_ops_per_sec: 550_000.0,
            hash_throughput_mbps: 800.0,
            timestamp: 1_700_000_000,
            calibration_duration_us: 100_000,
        };

        let factors = HardwareFactors::new(1.0, 1.0, 1.0);
        let summary = SessionSummary::new();
        let validation = ValidationReport::run(&summary, &factors);

        let output = BenchmarkOutput {
            calibration: CalibrationOutput {
                cpu,
                gpu: None,
                upload: None,
                factors: factors.clone(),
            },
            sessions: vec![ScenarioResult {
                name: "test".to_string(),
                summary,
            }],
            validation,
            constrained_profile: None,
        };

        let json = serde_json::to_string_pretty(&output).unwrap();
        assert!(json.contains("calibration"));
        assert!(json.contains("sessions"));
        assert!(json.contains("validation"));

        // Round-trip
        let deserialized: BenchmarkOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.sessions.len(), 1);
        assert_eq!(deserialized.sessions[0].name, "test");
    }

    #[test]
    fn test_gpu_calibration_result_serializes() {
        let r = GpuCalibrationResult {
            fps: 350.0,
            gpu_factor: 1.43,
            calibration_duration_us: 500_000,
            adapter: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("fps"));
        assert!(json.contains("gpu_factor"));
    }

    #[test]
    fn test_upload_calibration_result_serializes() {
        let r = UploadCalibrationResult {
            tile_ops_per_sec: 3_500.0,
            upload_factor: 1.43,
            calibration_duration_us: 200_000,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("tile_ops_per_sec"));
        assert!(json.contains("upload_factor"));
    }

    #[test]
    fn test_scenario_result_contains_session_summary() {
        let mut summary = SessionSummary::new();
        // record_frame increments total_frames internally
        for _ in 0..120 {
            summary.record_frame(12_000, 10);
        }
        summary.elapsed_us = 2_000_000;
        summary.finalize();

        let result = ScenarioResult {
            name: "steady_state_render".to_string(),
            summary,
        };
        assert_eq!(result.name, "steady_state_render");
        assert_eq!(result.summary.total_frames, 120);
        assert!((result.summary.fps - 60.0).abs() < 0.001);
    }
}

// ─── Contention-mechanism tests (headless feature) ───────────────────────────
//
// These tests validate the load-bearing scene-lock contention logic — a
// concurrent task holding the scene lock while a frame loop attempts the
// production `tokio::sync::Mutex::try_lock` path, accumulating real misses into
// `SessionSummary::scene_lock_misses` — WITHOUT spinning up the full GPU
// `HeadlessRuntime`. They exercise the exact same mutex type and `try_lock`
// method the windowed compositor uses (windowed.rs Stage 4), so a non-zero
// result here is a genuine contention signal, not a poked counter.
#[cfg(all(test, feature = "headless"))]
mod contention_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use tokio::sync::Mutex;

    use tze_hud_scene::graph::SceneGraph;
    use tze_hud_telemetry::{FrameTelemetry, SessionSummary};

    /// A concurrent holder racing a `try_lock` frame loop must produce a
    /// genuine, non-zero `scene_lock_misses` accumulated through the real
    /// telemetry path. We assert only `> 0` (never an exact count) so the test
    /// is deterministic-not-flaky: the handshake guarantees at least one miss
    /// while the floating total stays unasserted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn try_lock_contention_yields_nonzero_scene_lock_misses() {
        let scene: Arc<Mutex<SceneGraph>> = Arc::new(Mutex::new(SceneGraph::new(1920.0, 1080.0)));
        let holding = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        // Background "handler" task: holds the scene lock in bursts, flagging
        // its hold window so the frame loop can race it deterministically.
        let holder = {
            let scene = Arc::clone(&scene);
            let holding = Arc::clone(&holding);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                while !stop.load(Ordering::Acquire) {
                    {
                        let _guard = scene.lock().await;
                        holding.store(true, Ordering::Release);
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        holding.store(false, Ordering::Release);
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
        };

        // Frame loop using the identical production acquisition: try_lock; on a
        // miss, bump the running total and snapshot it into FrameTelemetry.
        let mut scene_lock_miss_count: u64 = 0;
        let mut summary = SessionSummary::new();
        for frame_idx in 0..200u64 {
            // Wait until the holder is inside its hold window (bounded).
            let wait_start = Instant::now();
            while !holding.load(Ordering::Acquire)
                && wait_start.elapsed() < Duration::from_millis(20)
            {
                tokio::time::sleep(Duration::from_micros(100)).await;
            }
            if scene.try_lock().is_err() {
                scene_lock_miss_count = scene_lock_miss_count.saturating_add(1);
            }
            let mut telemetry = FrameTelemetry::new(frame_idx);
            telemetry.scene_lock_miss_count = scene_lock_miss_count;
            summary.record_frame_correctness(&telemetry);
        }

        stop.store(true, Ordering::Release);
        let _ = holder.await;

        assert!(
            summary.scene_lock_misses > 0,
            "concurrent holder racing a try_lock frame loop must yield a real \
             non-zero scene_lock_misses (got {})",
            summary.scene_lock_misses,
        );
    }

    /// With no concurrent holder, the same try_lock frame loop must report a
    /// genuine zero — proving the non-zero result above comes from real
    /// contention, not an always-incrementing counter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn try_lock_without_contention_stays_zero() {
        let scene: Arc<Mutex<SceneGraph>> = Arc::new(Mutex::new(SceneGraph::new(1920.0, 1080.0)));
        let mut scene_lock_miss_count: u64 = 0;
        let mut summary = SessionSummary::new();
        for frame_idx in 0..200u64 {
            if scene.try_lock().is_err() {
                scene_lock_miss_count = scene_lock_miss_count.saturating_add(1);
            }
            let mut telemetry = FrameTelemetry::new(frame_idx);
            telemetry.scene_lock_miss_count = scene_lock_miss_count;
            summary.record_frame_correctness(&telemetry);
        }
        assert_eq!(
            summary.scene_lock_misses, 0,
            "uncontended try_lock loop must report a genuine zero",
        );
    }
}

#[cfg(all(test, feature = "headless"))]
mod paced_contention_model_tests {
    use crate::headless_impl::{
        PRODUCTION_CONTENTION_CEILING_PER_180_FRAMES, production_contention_target_frames,
        run_paced_contention_probe_for_test,
    };

    #[test]
    fn paced_contention_model_targets_bounded_fraction_of_ci_frames() {
        let target_frames = production_contention_target_frames(180);

        assert_eq!(
            target_frames, 18,
            "the CI-shaped 180-frame benchmark should target 10% of frames",
        );
        assert!(
            target_frames < 180,
            "paced contention must not reproduce the saturation scenario",
        );
        assert!(
            target_frames <= PRODUCTION_CONTENTION_CEILING_PER_180_FRAMES,
            "the data ceiling must cover the model's scheduled contention frames",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paced_contention_probe_yields_bounded_nonzero_try_lock_misses() {
        let observed = run_paced_contention_probe_for_test(180).await;

        assert!(
            observed > 0,
            "paced contention should still produce a real non-zero try_lock miss signal",
        );
        assert!(
            observed <= PRODUCTION_CONTENTION_CEILING_PER_180_FRAMES,
            "paced contention observed {observed} misses, exceeding the committed ceiling",
        );
        assert!(
            observed < 180,
            "paced contention must not saturate every frame",
        );
    }
}
