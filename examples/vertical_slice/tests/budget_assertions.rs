//! # Budget and pixel assertions for the vertical slice
//!
//! Validates the latency budgets defined in `heart-and-soul/validation.md`
//! (Layer 3) and pixel readback correctness (Layer 1).
//!
//! ## Budgets (from validation.md Layer 3)
//! - Frame time p99 < 16.6ms  (16_600 µs)
//! - input_to_local_ack p99 < 4ms  (4_000 µs)
//! - Hit-test p99 < 100µs
//! - Transaction validation p99 < 200µs
//! - Scene diff p99 < 500µs
//!
//! ## Pixel assertions (Layer 1)
//! Render a known scene and verify background, tile, and z-order pixels are
//! within ±tolerance per channel (±2 is the spec; wider for llvmpipe CI).
//!
//! ## Budgets and slack
//!
//! Budgets are reference-hardware targets widened by
//! `tze_hud_scene::perf_budget::test_budget`, a fixed slack factor
//! (`TZE_HUD_TEST_BUDGET_SLACK`, default 20×) that absorbs debug builds and
//! llvmpipe on CI.
//!
//! ## Timing-assertion quarantine (hud-1aswu.3)
//!
//! Wall-clock / p99 latency assertions are **gated** behind the
//! `TZE_HUD_PERF_ASSERT=1` environment variable so they never block the
//! standard `test-unit` CI lane on shared runners (where scheduler noise
//! causes spurious failures).  See `about/heart-and-soul/validation.md`
//! determinism doctrine: "flaky tests poison the feedback loop".
//!
//! - **Blocking everywhere**: structural assertions (sample counts, correctness
//!   invariants, pixel readback, scene rendering completeness).
//! - **Gated (`TZE_HUD_PERF_ASSERT=1`)**: all `assert_p99_under` hard-failure
//!   calls.
//!
//! To run wall-clock assertions locally or on a reference host:
//!   ```sh
//!   TZE_HUD_PERF_ASSERT=1 cargo test -p vertical_slice --test budget_assertions
//!   ```

use tze_hud_compositor::HeadlessSurface;
use tze_hud_input::{PointerEvent, PointerEventKind};
use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;
use tze_hud_scene::diff::SceneDiff;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::mutation::{MutationBatch, SceneMutation};
use tze_hud_scene::perf_budget::test_budget;
use tze_hud_scene::types::{
    Capability, FontFamily, HitRegionNode, Node, NodeData, Rect, Rgba, SceneId, SolidColorNode,
    TextAlign, TextMarkdownNode, TextOverflow,
};
use tze_hud_telemetry::LatencyBucket;

// ─── Timing-assertion gate (hud-1aswu.3) ─────────────────────────────────────

/// Returns `true` when wall-clock / p99 latency hard assertions should run.
///
/// Set `TZE_HUD_PERF_ASSERT=1` in the environment to enable timing assertions.
/// On the standard `test-unit` CI lane (shared Ubuntu runners) this is unset,
/// so budget assertions are skipped — structural correctness assertions
/// always run regardless of this flag.
///
/// On a reference host or the `windows-performance-budget` lane, callers that
/// want to enforce p99 budgets should set this variable before invoking the suite.
fn perf_assert_enabled() -> bool {
    std::env::var("TZE_HUD_PERF_ASSERT")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

// ─── Layer 3: p99 budget assertions ──────────────────────────────────────────

/// Assert that frame time p99 is under the 16.6ms budget (times the test slack).
///
/// Runs 20 frames headlessly and verifies the p99 telemetry bucket stays within
/// budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_frame_time_p99_within_budget() {
    const NOMINAL_BUDGET_US: u64 = 16_600;
    const FRAME_COUNT: usize = 20;

    let budget_us = test_budget(NOMINAL_BUDGET_US);

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "test".to_string(),
        config_toml: None,
    };
    let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    // Create a simple scene with one tile.
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab = scene.create_tab("Main", 0).unwrap();
        let lease = scene.grant_lease("test-agent", 60_000, vec![]);
        scene
            .create_tile(
                tab,
                "test-agent",
                lease,
                Rect::new(10.0, 10.0, 200.0, 100.0),
                1,
            )
            .unwrap();
    }

    // Discard the first frame: wgpu incurs pipeline/shader compilation overhead
    // on the first render that would otherwise inflate the p99 unrealistically.
    runtime.render_frame().await;
    runtime.telemetry = tze_hud_telemetry::TelemetryCollector::new();

    for _ in 0..FRAME_COUNT {
        runtime.render_frame().await;
    }

    let summary = runtime.telemetry.summary();
    // Structural assertion: always runs — validates telemetry plumbing, not speed.
    assert_eq!(
        summary.total_frames, FRAME_COUNT as u64,
        "expected {FRAME_COUNT} frames recorded"
    );

    // Timing assertion: gated — wall-clock budget that would flake on
    // shared CI runners.  Set TZE_HUD_PERF_ASSERT=1 to enable.  (hud-1aswu.3)
    if perf_assert_enabled() {
        let p99 = summary
            .frame_time
            .assert_p99_under(budget_us)
            .expect("frame_time p99 budget");
        eprintln!("[PASS] frame_time p99={p99}us within budget={budget_us}us");
    } else {
        let raw_p99 = summary.frame_time.p99().unwrap_or(0);
        eprintln!(
            "[SKIP-TIMING] frame_time raw_p99={raw_p99}us; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
    }
}

/// Assert that input_to_local_ack p99 is under the 4ms budget.
///
/// Simulates 30 pointer-press events and verifies each local-ack latency
/// (entirely local, no network roundtrip) is within budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_input_to_local_ack_p99_within_budget() {
    use tze_hud_scene::perf_budget::budgets::INPUT_ACK_BUDGET_US;
    let budget_us = test_budget(INPUT_ACK_BUDGET_US);
    const EVENT_COUNT: usize = 30;

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "test".to_string(),
        config_toml: None,
    };
    let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    // Set up a scene with a hit region
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab = scene.create_tab("Main", 0).unwrap();
        let lease = scene.grant_lease("test-agent", 60_000, vec![]);
        let tile = scene
            .create_tile(
                tab,
                "test-agent",
                lease,
                Rect::new(100.0, 100.0, 200.0, 200.0),
                1,
            )
            .unwrap();
        scene
            .set_tile_root(
                tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                        interaction_id: "test-button".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                    children: vec![],
                },
            )
            .unwrap();
    }

    for _ in 0..EVENT_COUNT {
        let (local_ack_us, hit_test_us) = {
            let state_arc = runtime.shared_state().clone();
            let state = state_arc.lock().await;
            let mut scene = state.scene.lock().await;
            let result = runtime.input_processor.process(
                &PointerEvent {
                    x: 150.0,
                    y: 150.0,
                    kind: PointerEventKind::Down,
                    device_id: 0,
                    timestamp: None,
                },
                &mut scene,
            );
            (result.local_ack_us, result.hit_test_us)
        };
        runtime
            .telemetry
            .summary_mut()
            .input_to_local_ack
            .record(local_ack_us);
        runtime
            .telemetry
            .summary_mut()
            .hit_test_latency
            .record(hit_test_us);
    }

    let summary = runtime.telemetry.summary();

    // Timing assertion: gated — wall-clock budget.  (hud-1aswu.3)
    if perf_assert_enabled() {
        summary
            .input_to_local_ack
            .assert_p99_under(budget_us)
            .expect("input_to_local_ack p99 budget");
    } else {
        let raw_p99 = summary.input_to_local_ack.p99().unwrap_or(0);
        eprintln!(
            "[SKIP-TIMING] input_to_local_ack raw_p99={raw_p99}us; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
    }
}

/// Assert that input_to_scene_commit p99 is under the 50ms budget.
///
/// Runs 30 headless frames and records the `input_to_scene_commit_us` field
/// emitted by `render_frame()`. Note: in the headless pipeline there is no
/// live agent applying mutations between frames, so `input_to_scene_commit_us`
/// will be 0 on frames with no applied mutations (see
/// `headless.rs` — the field is gated on `mutations_applied > 0`). This test
/// therefore validates the timing infrastructure plumbing rather than asserting
/// a representative agent round-trip latency. Budget assertion checks that any
/// non-zero samples are under 50ms (which they should be, even on slow CI).
///
/// ## Pipeline derivation
/// `render_frame()` sets `input_to_scene_commit_us` as wall time from
/// `frame_start` to end of Stage 4, which is the proxy for the
/// input-to-commit path from the frame start boundary.
///
/// ## CI note
/// The 50ms budget includes agent network round-trip time. The headless path
/// measures only the local pipeline, so no multiplier is needed — the local
/// commit should be far under 50ms even on slow CI machines.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_input_to_scene_commit_p99_within_budget() {
    const BUDGET_US: u64 = 50_000; // 50ms — covers agent network round-trip
    const CYCLE_COUNT: usize = 30;

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "test".to_string(),
        config_toml: None,
    };
    let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    // Set up a minimal scene
    {
        let state = runtime.shared_state().lock().await;
        state.scene.lock().await.create_tab("Main", 0).unwrap();
    }

    let mut bucket = LatencyBucket::new("input_to_scene_commit");

    for _ in 0..CYCLE_COUNT {
        // render_frame() executes the full pipeline and reports
        // input_to_scene_commit_us = stages 1–4 combined (local commit path)
        let telemetry = runtime.render_frame().await;
        bucket.record(telemetry.input_to_scene_commit_us);
    }

    // Timing assertion: gated — wall-clock budget.  (hud-1aswu.3)
    if perf_assert_enabled() {
        bucket
            .assert_p99_under(BUDGET_US)
            .expect("input_to_scene_commit p99 budget");
    } else {
        let raw_p99 = bucket.p99().unwrap_or(0);
        eprintln!(
            "[SKIP-TIMING] input_to_scene_commit raw_p99={raw_p99}us; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
    }

    // Also record into the shared summary for cross-test consistency
    runtime
        .telemetry
        .summary_mut()
        .input_to_scene_commit
        .samples
        .extend(bucket.samples.iter().copied());
}

/// Assert that input_to_next_present p99 is under the 33ms budget at 60Hz.
///
/// Runs 20 headless frames and verifies that the time from frame start (proxy
/// for input event arrival) to Stage 7 completion (GPU present) stays under
/// the 33ms two-frame budget at 60Hz.
///
/// ## Pipeline derivation
/// `render_frame()` sets `input_to_next_present_us = frame_time_us`, which is
/// the total wall time from Stage 1 start to Stage 7 end. This is the correct
/// measurement point: the present happens at Stage 7, and the frame pipeline
/// begins at the input drain boundary (Stage 1).
///
/// ## Hardware normalization
/// The 33ms budget is for real GPU hardware at 60Hz. On llvmpipe/SwiftShader
/// the same 10× headless multiplier used for frame-time tests applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_input_to_next_present_p99_within_budget() {
    const NOMINAL_BUDGET_US: u64 = 33_000; // 33ms at 60Hz (two frames)
    const HEADLESS_MULTIPLIER: u64 = 10;
    const BUDGET_US: u64 = NOMINAL_BUDGET_US * HEADLESS_MULTIPLIER;
    const FRAME_COUNT: usize = 20;

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "test".to_string(),
        config_toml: None,
    };
    let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    // Create a scene with one tile to exercise the full render path
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab = scene.create_tab("Main", 0).unwrap();
        let lease = scene.grant_lease("test-agent", 60_000, vec![]);
        scene
            .create_tile(
                tab,
                "test-agent",
                lease,
                Rect::new(10.0, 10.0, 200.0, 100.0),
                1,
            )
            .unwrap();
    }

    // Discard the first frame to avoid wgpu pipeline/shader compilation overhead.
    runtime.render_frame().await;
    runtime.telemetry = tze_hud_telemetry::TelemetryCollector::new();

    let mut bucket = LatencyBucket::new("input_to_next_present");

    for _ in 0..FRAME_COUNT {
        let telemetry = runtime.render_frame().await;
        bucket.record(telemetry.input_to_next_present_us);
    }

    // Structural assertion: always runs — validates sample collection plumbing.
    assert_eq!(
        bucket.samples.len(),
        FRAME_COUNT,
        "expected {FRAME_COUNT} samples in input_to_next_present bucket"
    );

    // Timing assertion: gated — wall-clock budget.  (hud-1aswu.3)
    if perf_assert_enabled() {
        bucket
            .assert_p99_under(BUDGET_US)
            .expect("input_to_next_present p99 budget");
    } else {
        let raw_p99 = bucket.p99().unwrap_or(0);
        eprintln!(
            "[SKIP-TIMING] input_to_next_present raw_p99={raw_p99}us; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
    }

    // Also record into the shared summary for cross-test consistency
    runtime
        .telemetry
        .summary_mut()
        .input_to_next_present
        .samples
        .extend(bucket.samples.iter().copied());
}

/// Assert that hit-test p99 is under the 100µs budget.
///
/// Exercises the hit-test path in isolation via repeated pointer-move events
/// over a large hit region.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hit_test_p99_within_budget() {
    use tze_hud_scene::perf_budget::budgets::HIT_TEST_BUDGET_US;
    let budget_us = test_budget(HIT_TEST_BUDGET_US);
    const EVENT_COUNT: usize = 50;

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "test".to_string(),
        config_toml: None,
    };
    let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab = scene.create_tab("Main", 0).unwrap();
        let lease = scene.grant_lease("test-agent", 60_000, vec![]);
        let tile = scene
            .create_tile(
                tab,
                "test-agent",
                lease,
                Rect::new(50.0, 50.0, 400.0, 400.0),
                1,
            )
            .unwrap();
        scene
            .set_tile_root(
                tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 400.0, 400.0),
                        interaction_id: "large-button".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                    children: vec![],
                },
            )
            .unwrap();
    }

    for _ in 0..EVENT_COUNT {
        let hit_test_us = {
            let state_arc = runtime.shared_state().clone();
            let state = state_arc.lock().await;
            let mut scene = state.scene.lock().await;
            let result = runtime.input_processor.process(
                &PointerEvent {
                    x: 200.0,
                    y: 200.0,
                    kind: PointerEventKind::Move,
                    device_id: 0,
                    timestamp: None,
                },
                &mut scene,
            );
            result.hit_test_us
        };
        runtime
            .telemetry
            .summary_mut()
            .hit_test_latency
            .record(hit_test_us);
    }

    let summary = runtime.telemetry.summary();
    // Timing assertion: gated — wall-clock budget.  (hud-1aswu.3)
    if perf_assert_enabled() {
        summary
            .hit_test_latency
            .assert_p99_under(budget_us)
            .expect("hit_test p99 budget");
    } else {
        let raw_p99 = summary.hit_test_latency.p99().unwrap_or(0);
        eprintln!(
            "[SKIP-TIMING] hit_test raw_p99={raw_p99}us; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
    }
}

/// Assert that transaction validation p99 is under the 200µs budget.
///
/// Applies a large sample of single-mutation `UpdateTileBounds` batches against
/// a fixed-size scene and records the round-trip latency of each `apply_batch`
/// call including validation and scene mutation.
#[test]
fn test_transaction_validation_p99_within_budget() {
    use tze_hud_scene::perf_budget::budgets::TRANSACTION_VALIDATION_BUDGET_US;

    let budget_us = test_budget(TRANSACTION_VALIDATION_BUDGET_US);
    // Keep a larger sample window so p99 is not effectively the max sample.
    const BATCH_COUNT: usize = 200;

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease(
        "agent",
        60_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    // Raise tile budget so budget enforcement doesn't reject batches in this timing test
    if let Some(l) = scene.leases.get_mut(&lease) {
        l.resource_budget.max_tiles = 256;
    }

    // Build a stable baseline scene once; each timed batch updates this tile.
    // This keeps the benchmark focused on transaction validation throughput
    // instead of allocator pressure from unbounded tile growth.
    let tile_id = scene
        .create_tile(tab, "agent", lease, Rect::new(0.0, 0.0, 80.0, 60.0), 1)
        .expect("baseline tile should be created");

    let mut validation_bucket = LatencyBucket::new("validation");

    for i in 0..BATCH_COUNT {
        let start = std::time::Instant::now();
        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: "agent".to_string(),
            mutations: vec![SceneMutation::UpdateTileBounds {
                tile_id,
                bounds: Rect::new(
                    (i as f32 * 2.5) % 1600.0,
                    (i as f32 * 1.5) % 950.0,
                    80.0,
                    60.0,
                ),
            }],
            timing_hints: None,
            lease_id: None,
        };
        let result = scene.apply_batch(&batch);
        let elapsed_us = start.elapsed().as_micros() as u64;
        validation_bucket.record(elapsed_us);
        assert!(result.applied, "batch {i} should have applied");
    }

    // Timing assertion: gated — wall-clock budget.  (hud-1aswu.3)
    if perf_assert_enabled() {
        validation_bucket
            .assert_p99_under(budget_us)
            .expect("transaction validation p99 budget");
    } else {
        let raw_p99 = validation_bucket.p99().unwrap_or(0);
        eprintln!(
            "[SKIP-TIMING] transaction_validation raw_p99={raw_p99}us; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
    }
}

/// Assert that scene diff p99 is under the 500µs budget.
///
/// Computes diffs between before/after snapshots of a scene with 10 tiles and
/// verifies the p99 latency across 50 iterations.
#[test]
fn test_scene_diff_p99_within_budget() {
    use tze_hud_scene::perf_budget::budgets::SCENE_DIFF_BUDGET_US;
    let budget_us = test_budget(SCENE_DIFF_BUDGET_US);
    const DIFF_COUNT: usize = 50;

    let mut diff_bucket = LatencyBucket::new("diff");

    for i in 0..DIFF_COUNT {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab = scene.create_tab("Main", 0).unwrap();
        let lease = scene.grant_lease("agent", 60_000, vec![]);

        // Build a scene with a handful of tiles
        for j in 0..10 {
            scene
                .create_tile(
                    tab,
                    "agent",
                    lease,
                    Rect::new(
                        (j as f32 * 100.0) % 1600.0,
                        (i as f32 * 20.0) % 900.0,
                        90.0,
                        70.0,
                    ),
                    j as u32 + 1,
                )
                .unwrap();
        }

        let snapshot = scene.clone();

        // Add one more tile
        scene
            .create_tile(tab, "agent", lease, Rect::new(5.0, 5.0, 50.0, 50.0), 99)
            .unwrap();

        let start = std::time::Instant::now();
        let diff = SceneDiff::compute(&snapshot, &scene);
        let elapsed_us = start.elapsed().as_micros() as u64;
        diff_bucket.record(elapsed_us);

        assert!(!diff.is_empty(), "diff should detect the new tile");
    }

    // Timing assertion: gated — wall-clock budget.  (hud-1aswu.3)
    if perf_assert_enabled() {
        diff_bucket
            .assert_p99_under(budget_us)
            .expect("scene diff p99 budget");
    } else {
        let raw_p99 = diff_bucket.p99().unwrap_or(0);
        eprintln!(
            "[SKIP-TIMING] scene_diff raw_p99={raw_p99}us; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
    }
}

/// Assert that texture upload (tile create + node assign + delete) p99 is within
/// budget.
///
/// The test creates `UPLOAD_ROUNDS` tiles with fresh `SolidColor` nodes (CPU proxy
/// for GPU texture upload). 100 samples so nearest-rank p99 drops one outlier
/// instead of being the max, plus 5 unrecorded warm-up rounds (hud-srnr5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_texture_upload_p99_within_budget() {
    /// Reference texture upload budget (µs) per tile creation on target hardware.
    ///
    /// On reference hardware, one create+assign+delete round takes ≈500µs.  The
    /// 2ms budget provides a defensible ×4 headroom over that baseline.
    ///
    /// This covers the CPU-side tile creation + node assignment path as a proxy
    /// for GPU texture upload throughput until GStreamer textures are implemented.
    const NOMINAL_BUDGET_US: u64 = 2_000; // 2ms per upload round
    /// Warm-up rounds to prime async lock machinery and CPU caches; not recorded.
    const WARMUP_ROUNDS: usize = 5;
    /// Measurement rounds. At 100 samples, nearest-rank p99 = sample[98] (0-based),
    /// discarding 1 outlier rather than using the raw maximum.
    const UPLOAD_ROUNDS: usize = 100;

    let budget_us = test_budget(NOMINAL_BUDGET_US);

    let config = HeadlessConfig {
        width: 400,
        height: 300,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "tex-upload-test".to_string(),
        config_toml: None,
    };
    let runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    // Ensure there is an active tab for tile creation.
    {
        let state = runtime.shared_state().lock().await;
        state
            .scene
            .lock()
            .await
            .create_tab("upload-test", 0)
            .unwrap();
    }

    // Create one lease up-front and reuse it for all rounds to avoid lease
    // bookkeeping accumulation that would skew p99 over later iterations.
    let (tab, lease) = {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab = scene.active_tab.expect("active tab");
        let lease = scene.grant_lease(
            "upload-test",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        if let Some(l) = scene.leases.get_mut(&lease) {
            l.resource_budget.max_tiles = 4;
        }
        (tab, lease)
    };

    // ── Warm-up rounds (not recorded) ─────────────────────────────────────────
    //
    // Prime async lock machinery and CPU caches before measurement.  Matches
    // `test_frame_time_p99_within_budget`, which discards its first render frame
    // for the same reason.  Warm-up uses z-order slots 0–4 to avoid collisions
    // with the measurement range (100–199).
    for w in 0..WARMUP_ROUNDS {
        let state_arc = runtime.shared_state().clone();
        let state = state_arc.lock().await;
        let mut scene = state.scene.lock().await;
        if let Ok(tile_id) = scene.create_tile(
            tab,
            "upload-test",
            lease,
            Rect::new(0.0, 0.0, 64.0, 64.0),
            w as u32,
        ) {
            let node = Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.5, 0.5, 0.5, 1.0),
                    bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
                    radius: None,
                }),
            };
            let _ = scene.set_tile_root(tile_id, node);
            let batch = MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "upload-test".to_string(),
                mutations: vec![SceneMutation::DeleteTile { tile_id }],
                timing_hints: None,
                lease_id: None,
            };
            let _ = scene.apply_batch(&batch);
        }
    }

    // ── Measurement rounds ─────────────────────────────────────────────────────
    let mut upload_bucket = LatencyBucket::new("texture_upload");

    for i in 0..UPLOAD_ROUNDS {
        let start = std::time::Instant::now();
        {
            let state_arc = runtime.shared_state().clone();
            let state = state_arc.lock().await;
            let mut scene = state.scene.lock().await;
            if let Ok(tile_id) = scene.create_tile(
                tab,
                "upload-test",
                lease,
                Rect::new(0.0, 0.0, 64.0, 64.0),
                100 + i as u32,
            ) {
                let node = Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::new(i as f32 / UPLOAD_ROUNDS as f32, 0.3, 0.7, 1.0),
                        bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
                        radius: None,
                    }),
                };
                let _ = scene.set_tile_root(tile_id, node);
                // Delete the tile to free up the slot for next round.
                let batch = MutationBatch {
                    batch_id: SceneId::new(),
                    agent_namespace: "upload-test".to_string(),
                    mutations: vec![SceneMutation::DeleteTile { tile_id }],
                    timing_hints: None,
                    lease_id: None,
                };
                let result = scene.apply_batch(&batch);
                assert!(
                    result.applied,
                    "DeleteTile mutation was not applied during texture upload budget test (round {}): {:?}",
                    i, result.error
                );
            }
        }
        upload_bucket.record(start.elapsed().as_micros() as u64);
    }

    // Timing assertion: gated — wall-clock budget that caused the
    // recurring flake hud-1aswu.3 on the blocking test-unit lane.
    // Set TZE_HUD_PERF_ASSERT=1 to enable hard p99 assertion on a reference host.
    if perf_assert_enabled() {
        let p99 = upload_bucket
            .assert_p99_under(budget_us)
            .expect("texture_upload p99 budget");
        eprintln!("[PASS] texture_upload p99={p99}us within budget={budget_us}us");
    } else {
        let raw_p99 = upload_bucket.p99().unwrap_or(0);
        eprintln!(
            "[SKIP-TIMING] texture_upload raw_p99={raw_p99}us; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
    }
}

/// Assert that Stage 6 (Render Encode) p99 is within budget with text rendering
/// active.
///
/// ## Spec Reference
///
/// From `runtime-kernel/spec.md` §Requirement: Stage 6 Render Encode (line 128–135):
/// > Stage 6 (Render Encode) MUST run on the compositor thread with a p99 budget
/// > of < 4ms. It SHALL build wgpu CommandEncoder from the RenderFrame, issue draw
/// > calls for tile nodes (solid color, text, image), encode alpha-blend passes for
/// > transparent tiles, and encode the chrome layer.
///
/// Text rasterization was added in hud-pmkf (PR#233) but no automated benchmark
/// validated the budget. This test fills that gap.
///
/// ## Scene
///
/// Creates `TEXT_TILE_COUNT` tiles each containing a `TextMarkdown` node with a
/// paragraph of text across multiple lines. This exercises the full text-rasterization
/// path (glyphon layout + atlas upload) on every frame.
///
/// ## Threshold
///
/// 4ms p99 on reference GPU hardware, widened by the test slack factor to
/// accommodate llvmpipe and slow CI runners. The goal in automation is catching
/// runaway regressions, not enforcing the 4ms boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stage6_render_encode_p99_within_budget() {
    /// Spec target: 4ms p99 on reference GPU hardware (runtime-kernel/spec.md line 135).
    const NOMINAL_BUDGET_US: u64 = 4_000;
    /// Number of text-content tiles in the benchmark scene.
    const TEXT_TILE_COUNT: usize = 5;
    /// Frames measured (excluding warmup).
    const FRAME_COUNT: usize = 30;

    let budget_us = test_budget(NOMINAL_BUDGET_US);

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "stage6-bench".to_string(),
        config_toml: None,
    };
    let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    // ── Build scene with TEXT_TILE_COUNT text tiles ───────────────────────────
    // Each tile has a multi-line TextMarkdown node to activate text rasterisation
    // and the glyphon layout path on every frame.
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab = scene.create_tab("BenchTab", 0).unwrap();
        let lease = scene.grant_lease("bench-agent", 120_000, vec![]);
        if let Some(l) = scene.leases.get_mut(&lease) {
            l.resource_budget.max_tiles = (TEXT_TILE_COUNT + 2) as u32;
        }

        for i in 0..TEXT_TILE_COUNT {
            // Lay tiles in a grid across the 800×600 surface.
            let col = i % 3;
            let row = i / 3;
            let x = 10.0 + col as f32 * 265.0;
            let y = 10.0 + row as f32 * 290.0;
            let w = 250.0_f32;
            let h = 275.0_f32;

            let tile = scene
                .create_tile(
                    tab,
                    "bench-agent",
                    lease,
                    Rect::new(x, y, w, h),
                    (i + 1) as u32,
                )
                .unwrap();

            // Paragraph text exercises the full glyphon rasterisation path.
            let content = format!(
                "# Widget {}\n\nStatus: **active**\nMetric: {:.1} ms\n\nLorem ipsum dolor sit amet, \
                 consectetur adipiscing elit. Pellentesque habitant morbi tristique \
                 senectus et netus et malesuada fames ac turpis egestas.",
                i + 1,
                i as f32 * 1.5 + 0.5,
            );

            scene
                .set_tile_root(
                    tile,
                    Node {
                        layout: Default::default(),
                        id: SceneId::new(),
                        children: vec![],
                        data: NodeData::TextMarkdown(TextMarkdownNode {
                            content,
                            bounds: Rect::new(0.0, 0.0, w, h),
                            font_size_px: 14.0,
                            font_family: FontFamily::SystemSansSerif,
                            color: Rgba::WHITE,
                            background: Some(Rgba::new(0.08, 0.10, 0.18, 1.0)),
                            alignment: TextAlign::Start,
                            overflow: TextOverflow::Clip,
                            color_runs: Box::default(),
                        }),
                    },
                )
                .unwrap();
        }
    }

    // ── Warmup frame — discard to absorb wgpu pipeline compilation overhead ───
    runtime.render_frame().await;
    runtime.telemetry = tze_hud_telemetry::TelemetryCollector::new();

    // ── Measurement loop ──────────────────────────────────────────────────────
    let mut bucket = LatencyBucket::new("stage6_render_encode");

    for _ in 0..FRAME_COUNT {
        let telemetry = runtime.render_frame().await;
        // stage6_render_encode_us is the wall-clock encode time reported by the
        // compositor for Stage 6 (render encode) — excludes GPU submit (Stage 7).
        bucket.record(telemetry.stage6_render_encode_us);
    }

    assert_eq!(
        bucket.samples.len(),
        FRAME_COUNT,
        "expected {FRAME_COUNT} stage6 samples"
    );

    // ── Budget assertion — gated (hud-1aswu.3) ───────────────────────────────
    // Set TZE_HUD_PERF_ASSERT=1 to enable the hard p99 assertion on a reference
    // host.  On the standard test-unit CI lane (shared runners) the timing
    // assertion is skipped; only the structural sample-count assertion above runs.
    if perf_assert_enabled() {
        let p99 = bucket
            .assert_p99_under(budget_us)
            .expect("stage6_render_encode p99 budget");
        eprintln!(
            "[PASS] stage6_render_encode p99={p99}us within budget={budget_us}us \
             (spec target={NOMINAL_BUDGET_US}us)"
        );
    } else {
        let raw_p99 = bucket.p99().unwrap_or(0);
        eprintln!(
            "[SKIP-TIMING] stage6_render_encode raw_p99={raw_p99}us; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
    }
}

// ─── Layer 1: Pixel readback assertions ──────────────────────────────────────

/// Verify background pixels match the compositor clear color after rendering
/// an empty scene.
///
/// Color math
/// ──────────
/// The compositor uses `Rgba8UnormSrgb` as the render target.  The GPU
/// converts linear vertex colors to sRGB on write, so pixels in the readback
/// buffer are sRGB-encoded.
///
/// Background clear: linear (0.05, 0.05, 0.10, 1.0)
///   → sRGB ≈ (64, 64, 89, 255)
///
/// Tolerance ±6 per channel to accommodate llvmpipe/SwiftShader variance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_layer1_pixel_readback_background() {
    // Background clear (0.05, 0.05, 0.10, 1.0) linear → sRGB ≈ (64, 64, 89, 255)
    const EXPECTED_BG: [u8; 4] = [64, 64, 89, 255];
    const TOLERANCE: u8 = 6;

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "test".to_string(),
        config_toml: None,
    };
    let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    // Empty scene — every pixel should be the background clear color
    runtime.render_frame().await;
    let pixels = runtime.read_pixels();
    assert_eq!(pixels.len(), 800 * 600 * 4, "pixel buffer size mismatch");

    // Sample corners and center
    for (x, y) in [(5u32, 5u32), (795, 5), (5, 595), (795, 595), (400, 300)] {
        HeadlessSurface::assert_pixel_color(
            &pixels,
            800,
            x,
            y,
            EXPECTED_BG,
            TOLERANCE,
            &format!("background ({x},{y})"),
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }
}

/// Verify that a tile region renders with its expected background color.
///
/// A TextMarkdown tile is created with `background = (0.10, 0.15, 0.30)`
/// linear.  After rendering, the pixel at the tile center should approximate
/// the sRGB encoding of that color: ≈ (89, 105, 148).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_layer1_pixel_readback_tile_color() {
    // Text tile background (0.10, 0.15, 0.30) linear → sRGB ≈ (89, 105, 148)
    const EXPECTED_TILE: [u8; 4] = [89, 105, 148, 255];
    const TOLERANCE: u8 = 8;

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "test".to_string(),
        config_toml: None,
    };
    let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    let (tile_x, tile_y, tile_w, tile_h) = (50u32, 50u32, 350u32, 250u32);
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        scene.create_tab("Main", 0).unwrap();
        let tab = scene.active_tab.unwrap();
        let lease = scene.grant_lease("agent", 60_000, vec![]);
        let tile = scene
            .create_tile(
                tab,
                "agent",
                lease,
                Rect::new(tile_x as f32, tile_y as f32, tile_w as f32, tile_h as f32),
                1,
            )
            .unwrap();
        scene
            .set_tile_root(
                tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "Layer 1 test".to_string(),
                        bounds: Rect::new(0.0, 0.0, tile_w as f32, tile_h as f32),
                        font_size_px: 16.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: Some(Rgba::new(0.10, 0.15, 0.30, 1.0)),
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                    children: vec![],
                },
            )
            .unwrap();
    }

    runtime.render_frame().await;
    let pixels = runtime.read_pixels();

    // Sample the tile interior, well away from edges
    let sample_x = tile_x + tile_w / 2;
    let sample_y = tile_y + tile_h / 2;

    HeadlessSurface::assert_pixel_color(
        &pixels,
        800,
        sample_x,
        sample_y,
        EXPECTED_TILE,
        TOLERANCE,
        "text tile center",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Verify that a higher-z tile paints over a lower-z tile at the overlap.
///
/// Tile A (z=1) is a blue solid rect.  Tile B (z=2, overlapping) is red.
/// The pixel at the overlap must be red (sRGB ≈ (255, 0, 0)).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_layer1_pixel_readback_z_order() {
    // Red solid (1.0, 0.0, 0.0) linear → sRGB = (255, 0, 0)
    const EXPECTED_HIGH_Z: [u8; 4] = [255, 0, 0, 255];
    const TOLERANCE: u8 = 4;

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "test".to_string(),
        config_toml: None,
    };
    let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        scene.create_tab("Main", 0).unwrap();
        let tab = scene.active_tab.unwrap();
        let lease = scene.grant_lease("agent", 60_000, vec![]);

        // Tile A at z=1 (blue)
        let tile_a = scene
            .create_tile(
                tab,
                "agent",
                lease,
                Rect::new(100.0, 100.0, 300.0, 200.0),
                1,
            )
            .unwrap();
        scene
            .set_tile_root(
                tile_a,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    data: NodeData::SolidColor(SolidColorNode {
                        bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
                        color: Rgba::new(0.20, 0.30, 0.50, 1.0),
                        radius: None,
                    }),
                    children: vec![],
                },
            )
            .unwrap();

        // Tile B at z=2 (red) — overlaps the center of Tile A
        let tile_b = scene
            .create_tile(
                tab,
                "agent",
                lease,
                Rect::new(150.0, 150.0, 100.0, 100.0),
                2,
            )
            .unwrap();
        scene
            .set_tile_root(
                tile_b,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    data: NodeData::SolidColor(SolidColorNode {
                        bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                        color: Rgba::new(1.0, 0.0, 0.0, 1.0), // red
                        radius: None,
                    }),
                    children: vec![],
                },
            )
            .unwrap();
    }

    runtime.render_frame().await;
    let pixels = runtime.read_pixels();

    // Point inside Tile B (z=2) — must be red
    HeadlessSurface::assert_pixel_color(
        &pixels,
        800,
        200,
        200,
        EXPECTED_HIGH_Z,
        TOLERANCE,
        "high-z tile (red) at overlap",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

// Unit tests for LatencyBucket::assert_p99_under live in
// crates/tze_hud_telemetry/src/record.rs.
//
// Unit tests for HeadlessSurface::assert_pixel_color live in
// crates/tze_hud_compositor/src/surface.rs.

// ─── Layer 1: 25-scene pixel readback assertions ──────────────────────────────
//
// Per validation-framework/spec.md Requirement: DR-V2 (line 186):
// "Compositor MUST render complete frame to offscreen texture with no window,
// no display server, no user interaction. Feature-equivalent to windowed for
// scene composition."
//
// Per validation-framework/spec.md Requirement: DR-V5 (line 228):
// "`cargo test --features headless` SHALL run full test suite (Layers 0-2)."
//
// These tests cover all 24 scenes in TestSceneRegistry.  For each scene:
// - Pixel buffer size is correct (width × height × 4).
// - The render completed: at least some pixels have been written.
// - For scenes with tiles, tile pixels differ from the pure-black default.
//
// For specific scenes (empty_scene, overlapping_tiles_zorder) more precise
// per-pixel color assertions are made.

use tze_hud_scene::test_scenes::{ClockMs, TestSceneRegistry};

// ─── Display dimensions used for all 25-scene tests ──────────────────────────
// 800×600: fast to render on llvmpipe/WARP, matches existing pixel tests.
const SCENE_W: u32 = 800;
const SCENE_H: u32 = 600;

/// Helper: build a HeadlessRuntime sized for the 25-scene tests.
async fn make_scene_runtime() -> tze_hud_runtime::HeadlessRuntime {
    HeadlessRuntime::new(HeadlessConfig {
        width: SCENE_W,
        height: SCENE_H,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "test".to_string(),
        config_toml: None,
    })
    .await
    .expect("HeadlessRuntime::new failed")
}

/// Run one frame with the given scene, then return the pixel buffer.
/// The runtime's scene is replaced via shared_state before rendering.
async fn render_scene_pixels(
    runtime: &mut tze_hud_runtime::HeadlessRuntime,
    scene: tze_hud_scene::graph::SceneGraph,
) -> Vec<u8> {
    {
        let state = runtime.shared_state().lock().await;
        *state.scene.lock().await = scene;
    }
    runtime.render_frame().await;
    runtime.read_pixels()
}

/// Background clear color in sRGB (compositor clears to linear [0.05, 0.05, 0.10, 1.0]).
/// sRGB ≈ [64, 64, 89, 255].  Tolerance ±8 for llvmpipe/SwiftShader.
const BG_SRGB: [u8; 4] = [64, 64, 89, 255];
const BG_TOLERANCE: u8 = 8;

// ─── empty_scene ─────────────────────────────────────────────────────────────

/// empty_scene: no tabs, no tiles — every pixel is the background clear color.
///
/// DR-V2: headless compositor renders a complete frame with no scene content.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scene_empty_scene_pixels() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::with_display(SCENE_W as f32, SCENE_H as f32);
    let (scene, _spec) = registry
        .build("empty_scene", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;
    assert_eq!(
        pixels.len(),
        (SCENE_W * SCENE_H * 4) as usize,
        "pixel buffer size"
    );

    // Sample every 50th pixel — all should be the background clear color (no tiles).
    for i in (0..SCENE_W * SCENE_H).step_by(50) {
        let x = i % SCENE_W;
        let y = i / SCENE_W;
        HeadlessSurface::assert_pixel_color(
            &pixels,
            SCENE_W,
            x,
            y,
            BG_SRGB,
            BG_TOLERANCE,
            "empty_scene background",
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }
}

// ─── single_tile_solid ────────────────────────────────────────────────────────

/// single_tile_solid: one tile with TextMarkdown content.
/// Background (0.08, 0.08, 0.15) linear → sRGB ≈ (75, 75, 106).
/// The tile occupies (100, 100, 900, 500) on an 800×600 surface, so we clip to
/// the visible portion.  Sampling the tile center (400, 300) should give the
/// tile background color, not the compositor clear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scene_single_tile_solid_pixels() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::with_display(SCENE_W as f32, SCENE_H as f32);
    let (scene, _spec) = registry
        .build("single_tile_solid", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;
    assert_eq!(
        pixels.len(),
        (SCENE_W * SCENE_H * 4) as usize,
        "pixel buffer size"
    );

    // Tile background (0.08, 0.08, 0.15) linear → sRGB ≈ (75, 75, 106)
    // We use a wide tolerance because the tile color is close to the background.
    // The center of the tile (400, 300) should definitely not be pure-BG.
    let tile_center = HeadlessSurface::pixel_at(&pixels, SCENE_W, 400, 300);
    assert_ne!(
        tile_center,
        [0u8, 0, 0, 0],
        "tile center must not be all-zero — compositor must render something"
    );
    // The tile background is darker than the compositor clear blue (89 on channel 2).
    // The exact sRGB value of 0.15 linear ≈ 106 on channel B.
    // We just check that the frame has non-clear pixels in the tile region.
    let non_bg = pixels.chunks(4).any(|p| {
        // Differs from background by more than tolerance on any channel
        let r_diff = (p[0] as i16 - BG_SRGB[0] as i16).unsigned_abs();
        let g_diff = (p[1] as i16 - BG_SRGB[1] as i16).unsigned_abs();
        let b_diff = (p[2] as i16 - BG_SRGB[2] as i16).unsigned_abs();
        r_diff > 10 || g_diff > 10 || b_diff > 10
    });
    assert!(
        non_bg,
        "single_tile_solid: tile pixels must differ from background"
    );
}

// ─── three_tiles_no_overlap ───────────────────────────────────────────────────

/// three_tiles_no_overlap: three non-overlapping tiles — text, hit-region, solid.
/// Just verifies the pixel buffer is correct size and rendering completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scene_three_tiles_no_overlap_pixels() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::with_display(SCENE_W as f32, SCENE_H as f32);
    let (scene, _spec) = registry
        .build("three_tiles_no_overlap", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;
    assert_eq!(
        pixels.len(),
        (SCENE_W * SCENE_H * 4) as usize,
        "pixel buffer size"
    );
    assert_eq!(pixels.len() % 4, 0, "pixel buffer must be RGBA8 aligned");
}

// ─── max_tiles_stress ────────────────────────────────────────────────────────

/// max_tiles_stress: many tiles — validates that the compositor handles high
/// tile counts headlessly (no OOM, no crash).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scene_max_tiles_stress_pixels() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::with_display(SCENE_W as f32, SCENE_H as f32);
    let (scene, _spec) = registry
        .build("max_tiles_stress", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;
    assert_eq!(
        pixels.len(),
        (SCENE_W * SCENE_H * 4) as usize,
        "pixel buffer size"
    );
    assert!(
        pixels.iter().any(|&b| b > 0),
        "max_tiles_stress: frame must not be all-zero"
    );
}

// ─── overlapping_tiles_zorder ────────────────────────────────────────────────

/// overlapping_tiles_zorder: three tiles at z=1 (red), z=2 (green), z=3 (blue),
/// overlapping.  The top tile (z=3, blue) must appear at the overlap region.
///
/// Blue (0.2, 0.2, 0.8) linear → sRGB ≈ (124, 124, 226).
/// The top tile occupies (300, 200, 600, 400) — center at (600, 400) but
/// on 800×600 display the tile is bounded to the display.
/// We sample (400, 250) which is inside all three tiles; z=3 (blue) must win.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scene_overlapping_tiles_zorder_pixels() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::with_display(SCENE_W as f32, SCENE_H as f32);
    let (scene, _spec) = registry
        .build("overlapping_tiles_zorder", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;
    assert_eq!(
        pixels.len(),
        (SCENE_W * SCENE_H * 4) as usize,
        "pixel buffer size"
    );

    // At the overlap region (400, 250), the z=3 tile (blue 0.2, 0.2, 0.8) should dominate.
    // sRGB ≈ (124, 124, 226).  Tolerance ±10 for software GPU variance.
    // The important assertion is: blue channel is significantly larger than red/green.
    let overlap_px = HeadlessSurface::pixel_at(&pixels, SCENE_W, 400, 250);
    assert!(
        overlap_px[2] > overlap_px[0] + 50 && overlap_px[2] > overlap_px[1] + 50,
        "z=3 (blue) tile must dominate at overlap: pixel={overlap_px:?}"
    );
}

// ─── overlay_transparency ────────────────────────────────────────────────────

/// overlay_transparency: chrome overlay with alpha blending.
/// Verifies that alpha blending produces non-trivial output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scene_overlay_transparency_pixels() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::with_display(SCENE_W as f32, SCENE_H as f32);
    let (scene, _spec) = registry
        .build("overlay_transparency", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;
    assert_eq!(
        pixels.len(),
        (SCENE_W * SCENE_H * 4) as usize,
        "pixel buffer size"
    );
    assert!(
        pixels.iter().any(|&b| b > 10),
        "overlay_transparency: frame must not be near-zero"
    );
}

// ─── Batch test for remaining 18 scenes ──────────────────────────────────────
//
// For the remaining scenes (tab_switch, lease_expiry, mobile_degraded,
// input_highlight, coalesced_dashboard, three_agents_contention,
// overlay_passthrough_regions, disconnect_reclaim_multiagent, privacy_redaction_mode,
// chatty_dashboard_touch, zone_publish_subtitle, zone_reject_wrong_type,
// zone_conflict_two_publishers, zone_orchestrate_then_publish,
// zone_geometry_adapts_profile, zone_disconnect_cleanup, policy_matrix_basic,
// policy_arbitration_collision) the assertion is:
//   - Pixel buffer size matches SCENE_W × SCENE_H × 4.
//   - At least one pixel is non-zero (compositor rendered something).
//
// These are structural validity tests — they confirm the headless pipeline
// completes for every scene without crash or OOM.

macro_rules! scene_render_test {
    ($test_name:ident, $scene_name:literal) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $test_name() {
            let mut runtime = make_scene_runtime().await;
            let registry = TestSceneRegistry::with_display(SCENE_W as f32, SCENE_H as f32);
            let (scene, _spec) = registry
                .build($scene_name, ClockMs::FIXED)
                .expect(concat!("build failed for ", $scene_name));
            let pixels = render_scene_pixels(&mut runtime, scene).await;
            assert_eq!(
                pixels.len(),
                (SCENE_W * SCENE_H * 4) as usize,
                concat!($scene_name, ": pixel buffer size mismatch")
            );
            // A frame with content should produce at least some non-zero pixels.
            // Note: empty_scene has its own test above that checks all pixels are BG.
            assert!(
                pixels.iter().any(|&b| b > 0),
                concat!($scene_name, ": frame must not be all-zero")
            );
        }
    };
}

scene_render_test!(test_scene_tab_switch_pixels, "tab_switch");
scene_render_test!(test_scene_lease_expiry_pixels, "lease_expiry");
scene_render_test!(test_scene_mobile_degraded_pixels, "mobile_degraded");
scene_render_test!(test_scene_input_highlight_pixels, "input_highlight");
scene_render_test!(test_scene_coalesced_dashboard_pixels, "coalesced_dashboard");
scene_render_test!(
    test_scene_three_agents_contention_pixels,
    "three_agents_contention"
);
scene_render_test!(
    test_scene_overlay_passthrough_regions_pixels,
    "overlay_passthrough_regions"
);
scene_render_test!(
    test_scene_disconnect_reclaim_multiagent_pixels,
    "disconnect_reclaim_multiagent"
);
scene_render_test!(
    test_scene_privacy_redaction_mode_pixels,
    "privacy_redaction_mode"
);
scene_render_test!(
    test_scene_chatty_dashboard_touch_pixels,
    "chatty_dashboard_touch"
);
scene_render_test!(
    test_scene_zone_publish_subtitle_pixels,
    "zone_publish_subtitle"
);
scene_render_test!(
    test_scene_zone_reject_wrong_type_pixels,
    "zone_reject_wrong_type"
);
scene_render_test!(
    test_scene_zone_conflict_two_publishers_pixels,
    "zone_conflict_two_publishers"
);
scene_render_test!(
    test_scene_zone_orchestrate_then_publish_pixels,
    "zone_orchestrate_then_publish"
);
scene_render_test!(
    test_scene_zone_geometry_adapts_profile_pixels,
    "zone_geometry_adapts_profile"
);
scene_render_test!(
    test_scene_zone_disconnect_cleanup_pixels,
    "zone_disconnect_cleanup"
);
scene_render_test!(test_scene_policy_matrix_basic_pixels, "policy_matrix_basic");
scene_render_test!(
    test_scene_policy_arbitration_collision_pixels,
    "policy_arbitration_collision"
);
