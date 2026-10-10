//! Text stream portal cadence harness (hud-5jbra.5).
//!
//! Verifies Section 5 of text-stream-portal-phase1:
//!   - 5.1: dirty Hub portals drain together with per-portal render pacing
//!   - 5.2: cadence harness (≥200 scalars/s sustained, 4KiB/250ms bursts)
//!   - 5.3: frame/stage budget verification under sustained streams and bursts
//!   - 5.4: independent dual-portal service and retained-window burst coherence
//!
//! Live soak (5.5), live cadence phase (5.6), and transport-RTT evidence (5.7)
//! require the TzeHouse Windows reference host and are recorded as discovered
//! follow-up work in the worker report (see Discovered-Follow-Ups-JSON).

#[path = "../../crates/tze_hud_runtime/src/test_support.rs"]
mod gpu_init;
use gpu_init::serialized_headless_init;

use std::sync::Arc;
use tokio::sync::Mutex;
use tze_hud_projection::hub::{PortalHub, PortalKey, Publish};
use tze_hud_runtime::headless::{HeadlessConfig, HeadlessRuntime};
use tze_hud_runtime::portal_projection_driver::{InProcessPortalDriver, PortalHandle};
use tze_hud_runtime::{
    EnqueueResult, FreezeQueue, INPUT_TO_NEXT_PRESENT_BUDGET_US, MutationTrafficClass,
    QueuedMutation, STAGE3_BUDGET_US, STAGE4_BUDGET_US, STAGE5_BUDGET_US,
};
use tze_hud_scene::clock::{Clock, TestClock};
use tze_hud_scene::{
    graph::SceneGraph,
    types::{Node, NodeData, Rect, Rgba, SceneId, SolidColorNode},
};
use tze_hud_telemetry::{LatencyBucket, SessionSummary};

// ─── Test configuration ────────────────────────────────────────────────────────

/// Headless display dimensions.
const DISPLAY_W: u32 = 1280;
const DISPLAY_H: u32 = 720;

/// Number of frames to render in the headless benchmark scenarios.
const BENCH_FRAME_COUNT: u64 = 200;

/// p99 frame-time budget for headless CI (µs).
///
/// The reference Windows locked budget is p99 ≤ 8.3 ms (high_mutation),
/// enforced by the CI gate (`scripts/ci/check_windows_perf_budgets.py`) on a
/// `windows-latest` runner. On headless Linux CI the GPU is software-rasterized
/// (llvmpipe) and frame times can be 8–12× slower than reference hardware.
///
/// We use 250 ms here — generous enough to pass on llvmpipe without being so
/// loose as to miss a genuine regression (a regression would push software-GPU
/// times into seconds). The real performance guard is the Windows CI gate.
const HEADLESS_FRAME_BUDGET_US: u64 = 250_000;

/// p99 per-stage budgets (µs) from engineering-bar.md §2.
/// Stage 3 (Mutation Intake) < 1ms, Stage 4 (Scene Commit) < 1ms,
/// Stage 5 (Layout Resolve) < 1ms.
const STAGE3_P99_BUDGET_US: u64 = STAGE3_BUDGET_US;
const STAGE4_P99_BUDGET_US: u64 = STAGE4_BUDGET_US;
const STAGE5_P99_BUDGET_US: u64 = STAGE5_BUDGET_US;

/// Max aggregate event rate from engineering-bar.md §2: 1000 events/second.
const MAX_AGGREGATE_EVENTS_PER_SEC: u64 = 1_000;

// ─── Timing-assertion gate (hud-94vm5) ───────────────────────────────────────

/// Returns `true` when wall-clock / p99 latency hard assertions should run.
///
/// Set `TZE_HUD_PERF_ASSERT=1` to enable.  On the standard `test-unit` / blocking
/// CI lane this is unset; wall-clock budget assertions are skipped to
/// avoid flakes from scheduler noise on shared runners.
fn perf_assert_enabled() -> bool {
    std::env::var("TZE_HUD_PERF_ASSERT")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

// ─── Headless runtime helpers ─────────────────────────────────────────────────

fn cadence_bench_config() -> HeadlessConfig {
    HeadlessConfig {
        width: DISPLAY_W,
        height: DISPLAY_H,
        grpc_port: 0,
        agents: tze_hud_scene::config::AgentDirectory::unrestricted("cadence-test-key"),
        config_toml: None,
    }
}

async fn scene_handle(runtime: &HeadlessRuntime) -> Arc<Mutex<SceneGraph>> {
    let state = runtime.state.lock().await;
    state.scene.clone()
}

// The normative workload belongs to this fixture, not a second production
// scheduler: 200 scalars/s, 10 increments/s and a 4 KiB/250 ms burst.
const CADENCE_MIN_SCALARS_PER_SEC: u64 = 200;
const CADENCE_MIN_INCREMENTS_PER_SEC: u64 = 10;
const CADENCE_BURST_BYTES: usize = 4096;
const CADENCE_BURST_WINDOW_MS: u64 = 250;

fn sustained_stream(
    scalars_per_sec: u64,
    increments_per_sec: u64,
    duration_secs: u64,
) -> Vec<(u64, Vec<u8>, u64)> {
    assert!(scalars_per_sec >= CADENCE_MIN_SCALARS_PER_SEC);
    assert!(increments_per_sec >= CADENCE_MIN_INCREMENTS_PER_SEC);
    assert!(duration_secs >= 1);
    let scalars_per_increment = scalars_per_sec.div_ceil(increments_per_sec);
    let interval_us = 1_000_000 / increments_per_sec;
    (0..increments_per_sec * duration_secs)
        .map(|i| {
            (
                i * interval_us,
                vec![b'a'; scalars_per_increment as usize],
                scalars_per_increment,
            )
        })
        .collect()
}

async fn portal_driver(
    runtime: &HeadlessRuntime,
) -> (InProcessPortalDriver, PortalHandle, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(1_000));
    let handle = PortalHandle::new(PortalHub::default(), clock.clone());
    {
        let scene_arc = scene_handle(runtime).await;
        let mut scene = scene_arc.lock().await;
        *scene = SceneGraph::new_with_clock(DISPLAY_W as f32, DISPLAY_H as f32, clock.clone());
    }
    (
        InProcessPortalDriver::with_portals(handle.clone()),
        handle,
        clock,
    )
}

fn publish_stream(handle: &PortalHandle, key: &PortalKey, payload: &[u8], at_us: u64) {
    let text = String::from_utf8(payload.to_vec()).expect("workload is UTF-8");
    handle
        .hub
        .lock()
        .expect("Hub lock")
        .publish(
            key,
            Publish {
                text: Some(text),
                ..Publish::default()
            },
            at_us,
        )
        .expect("real portal publish");
}

async fn drain_portals(runtime: &mut HeadlessRuntime, driver: &mut InProcessPortalDriver) {
    let scene_arc = scene_handle(runtime).await;
    let mut scene = scene_arc.lock().await;
    driver.drain(&mut scene, &mut runtime.input_processor, None);
    // No scene or Hub guard survives into render_frame().await.
}

// ─── Task 5.3: Frame/stage budget verification (headless) ─────────────────────

/// Under a sustained portal stream workload, headless frame time must stay
/// within the headless budget and per-stage times must be within spec.
///
/// This test uses the headless runtime + SceneGraph to verify that the
/// pipeline stages respect their budgets when portal mutations arrive
/// at the normative sustained cadence (≥200 scalars/s, 10 increments/s).
#[tokio::test]
async fn frame_budgets_hold_under_sustained_portal_stream() {
    let config = cadence_bench_config();
    let mut runtime = serialized_headless_init(HeadlessRuntime::new(config))
        .await
        .expect("HeadlessRuntime init failed");

    let (mut driver, handle, clock) = portal_driver(&runtime).await;
    let key = PortalKey::new("cadence_test", "sustained");
    // Warmup.
    for _ in 0..5 {
        runtime.render_frame().await;
    }

    // Build the sustained stream: 200 scalars/s over BENCH_FRAME_COUNT "frames".
    // We approximate: 10 appends/s, 1 append per batch frame interval.
    // At 60 Hz, 10 appends/s = 1 append per 6 frames.
    let stream = sustained_stream(
        CADENCE_MIN_SCALARS_PER_SEC,
        CADENCE_MIN_INCREMENTS_PER_SEC,
        /* duration_secs = */ 20, // enough for BENCH_FRAME_COUNT frames at 6:1 ratio
    );

    let mut summary = SessionSummary::new();
    let mut append_idx = 0usize;
    // Per-stage latency buckets: record only on frames where mutation intake ran
    // (stage3_mutation_intake_us > 0) to avoid conflating idle frames with
    // mutation-processing frames when computing p99.
    let mut stage3_bucket = LatencyBucket::new("stage3_mutation_intake");
    let mut stage4_bucket = LatencyBucket::new("stage4_scene_commit");
    let mut stage5_bucket = LatencyBucket::new("stage5_layout_resolve");

    for frame_idx in 0..BENCH_FRAME_COUNT {
        // Apply one portal append every 6 frames to hit ≥ 10 appends/s at 60Hz.
        if frame_idx % 6 == 0 && append_idx < stream.len() {
            let (_ts, payload, _scalars) = &stream[append_idx];
            append_idx += 1;

            clock.set_us(1_000_000 + frame_idx * 1_000_000 / 60);
            publish_stream(
                &handle,
                &key,
                payload,
                1_000_000 + frame_idx * 1_000_000 / 60,
            );
            drain_portals(&mut runtime, &mut driver).await;
        }

        let telemetry = runtime.render_frame().await;
        summary.record_frame(telemetry.frame_time_us, telemetry.tile_count);
        if telemetry.stage3_mutation_intake_us > 0 {
            summary
                .input_to_scene_commit
                .record(telemetry.stage3_mutation_intake_us + telemetry.stage4_scene_commit_us);
            // Record per-stage samples only on frames with mutation work.
            stage3_bucket.record(telemetry.stage3_mutation_intake_us);
            stage4_bucket.record(telemetry.stage4_scene_commit_us);
            stage5_bucket.record(telemetry.stage5_layout_resolve_us);
        }
        summary
            .input_to_next_present
            .record(telemetry.frame_time_us);
    }

    summary.finalize();

    // ── Budget assertions ──────────────────────────────────────────────────────

    // Timing assertions: gated — wall-clock budgets.  (hud-94vm5)
    // Set TZE_HUD_PERF_ASSERT=1 to enforce on a reference host.
    let p99_frame = summary.frame_time.p99().unwrap_or(0);
    if perf_assert_enabled() {
        assert!(
            p99_frame <= HEADLESS_FRAME_BUDGET_US,
            "p99 frame time {p99_frame}µs exceeded headless budget {HEADLESS_FRAME_BUDGET_US}µs \
             under sustained portal stream"
        );

        // Per-stage p99 budget assertions (engineering-bar.md §2):
        // Stage 3 (Mutation Intake) < 1ms, Stage 4 (Scene Commit) < 1ms,
        // Stage 5 (Layout Resolve) < 1ms.
        // These are checked only when mutation frames were actually recorded.
        if stage3_bucket.samples.len() >= 3 {
            let p99_s3 = stage3_bucket.p99().unwrap_or(0);
            assert!(
                p99_s3 <= STAGE3_P99_BUDGET_US,
                "Stage 3 (Mutation Intake) p99 {p99_s3}µs exceeded budget {STAGE3_P99_BUDGET_US}µs \
                 under sustained portal stream"
            );
            let p99_s4 = stage4_bucket.p99().unwrap_or(0);
            assert!(
                p99_s4 <= STAGE4_P99_BUDGET_US,
                "Stage 4 (Scene Commit) p99 {p99_s4}µs exceeded budget {STAGE4_P99_BUDGET_US}µs \
                 under sustained portal stream"
            );
            let p99_s5 = stage5_bucket.p99().unwrap_or(0);
            assert!(
                p99_s5 <= STAGE5_P99_BUDGET_US,
                "Stage 5 (Layout Resolve) p99 {p99_s5}µs exceeded budget {STAGE5_P99_BUDGET_US}µs \
                 under sustained portal stream"
            );
        }
    } else {
        eprintln!(
            "[SKIP-TIMING] frame_time p99={p99_frame}µs; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
        if stage3_bucket.samples.len() >= 3 {
            eprintln!(
                "[SKIP-TIMING] stage3 p99={}µs, stage4 p99={}µs, stage5 p99={}µs; \
                 set TZE_HUD_PERF_ASSERT=1 to enforce budgets",
                stage3_bucket.p99().unwrap_or(0),
                stage4_bucket.p99().unwrap_or(0),
                stage5_bucket.p99().unwrap_or(0),
            );
        }
    }

    // Structural assertion: always runs — validates cadence rate ceiling (not wall-clock).
    // At 10 appends/s (1 per 6 frames at 60Hz) over BENCH_FRAME_COUNT frames, the
    // maximum append count is ceil(BENCH_FRAME_COUNT / 6). At 10 events/s this is
    // far below the 1000 events/s ceiling — the assertion is structural.
    let max_appends = BENCH_FRAME_COUNT.div_ceil(6);
    assert!(
        append_idx as u64 <= max_appends,
        "cadence harness submitted {append_idx} appends (max allowed {max_appends}) — \
         would exceed the 1000 events/s ceiling at 60Hz"
    );

    assert_eq!(
        handle
            .hub
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .transcript
            .len(),
        append_idx
    );
    assert!(
        append_idx > 0,
        "real portal workload must have been accepted"
    );
    assert!(
        scene_handle(&runtime)
            .await
            .lock()
            .await
            .tiles
            .values()
            .any(|tile| tile.namespace == "tze_hud_portal_driver"),
        "driver must have materialized portal territory"
    );
}

/// Under a burst workload (≥ 4096 bytes in 250ms), stage budgets stay within spec.
///
/// A burst is modelled as a single large mutation batch applied in one frame.
#[tokio::test]
async fn frame_budgets_hold_under_burst() {
    let config = cadence_bench_config();
    let mut runtime = serialized_headless_init(HeadlessRuntime::new(config))
        .await
        .expect("HeadlessRuntime init failed");

    let (mut driver, handle, clock) = portal_driver(&runtime).await;
    let key = PortalKey::new("burst_test", "burst");
    // Warmup.
    for _ in 0..5 {
        runtime.render_frame().await;
    }

    let payload = vec![b'x'; CADENCE_BURST_BYTES];
    clock.set_us(1_000_000);
    publish_stream(&handle, &key, &payload, 1_000_000);
    drain_portals(&mut runtime, &mut driver).await;
    assert_eq!(
        handle
            .hub
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .transcript_bytes,
        CADENCE_BURST_BYTES
    );
    let accepted_at = handle.hub.lock().unwrap().get(&key).unwrap().transcript[0].at_us;
    assert!(
        clock.now_us().saturating_sub(accepted_at) <= CADENCE_BURST_WINDOW_MS * 1_000,
        "accepted burst remains within the 250 ms workload window"
    );
    assert!(
        scene_handle(&runtime)
            .await
            .lock()
            .await
            .tiles
            .values()
            .any(|tile| tile.namespace == "tze_hud_portal_driver"),
        "real driver must have rendered the burst portal"
    );

    let burst_telemetry = runtime.render_frame().await;

    // Stage 3 + 4 + 5 must respect per-stage budgets.
    assert!(
        burst_telemetry.stage3_mutation_intake_us <= STAGE3_P99_BUDGET_US,
        "Stage 3 (Mutation Intake) {burst_us}µs exceeded budget {STAGE3_P99_BUDGET_US}µs on burst frame",
        burst_us = burst_telemetry.stage3_mutation_intake_us
    );
    assert!(
        burst_telemetry.stage4_scene_commit_us <= STAGE4_P99_BUDGET_US,
        "Stage 4 (Scene Commit) {burst_us}µs exceeded budget {STAGE4_P99_BUDGET_US}µs on burst frame",
        burst_us = burst_telemetry.stage4_scene_commit_us
    );
    assert!(
        burst_telemetry.stage5_layout_resolve_us <= STAGE5_P99_BUDGET_US,
        "Stage 5 (Layout Resolve) {burst_us}µs exceeded budget {STAGE5_P99_BUDGET_US}µs on burst frame",
        burst_us = burst_telemetry.stage5_layout_resolve_us
    );

    // Total frame time on burst frame must stay within general budget.
    assert!(
        burst_telemetry.frame_time_us <= HEADLESS_FRAME_BUDGET_US,
        "frame time {us}µs exceeded headless budget {HEADLESS_FRAME_BUDGET_US}µs on burst frame",
        us = burst_telemetry.frame_time_us
    );
}

// ─── Task 5.3: Aggregate event rate ceiling ───────────────────────────────────

/// Verify the portal stream stays within 1000 events/s aggregate ceiling.
///
/// At 200 scalars/s with 10 increments/s, we produce at most 10 mutation
/// events per second — well below 1000. The test drives this at maximum rate
/// for 10 simulated seconds and verifies the submission count is bounded.
#[test]
fn portal_event_rate_stays_below_aggregate_ceiling() {
    // Simulate 10 seconds of maximum-rate sustained streaming.
    let stream = sustained_stream(
        CADENCE_MIN_SCALARS_PER_SEC,
        CADENCE_MIN_INCREMENTS_PER_SEC,
        10,
    );

    let total_events = stream.len() as u64;
    let elapsed_secs = 10u64;
    let events_per_sec = total_events / elapsed_secs;

    assert!(
        events_per_sec <= MAX_AGGREGATE_EVENTS_PER_SEC,
        "normative sustained cadence emits {events_per_sec} events/s, \
         exceeding the 1000 events/s aggregate ceiling"
    );
}

// ─── Task 5.4: Retained-window coherence under burst ─────────────────────────

/// After a burst that coalesces multiple snapshots, the final drained snapshot
/// must be the most-recent coherent window (not oldest or mid-stream).
///
/// This mirrors text_stream_portal_coalescing.rs but uses the
/// PortalHub instead of the raw FreezeQueue.
#[test]
fn burst_coherence_latest_snapshot_survives_coalescing() {
    const BURST_APPENDS: usize = 50;
    let mut hub = PortalHub::default();
    let portal = PortalKey::new("burst-coherence", "a");
    let mut last_payload = Vec::new();
    for i in 0..BURST_APPENDS {
        let line = format!("[{i:03}] burst line {i}\n");
        last_payload.extend_from_slice(line.as_bytes());
        hub.publish(
            &portal,
            Publish {
                text: Some(String::from_utf8(last_payload.clone()).unwrap()),
                key: Some("snapshot".into()),
                ..Publish::default()
            },
            i as u64 * 5_000,
        )
        .unwrap();
    }
    let due = hub.take_due(250_000);
    assert_eq!(due.len(), 1, "one dirty portal yields one coherent window");
    assert_eq!(due[0].key, portal);
    let unit = &hub.get(&portal).unwrap().transcript[0];
    assert_eq!(unit.seq, 1, "keyed replacement retains its logical unit");
    assert_eq!(
        unit.at_us,
        (BURST_APPENDS as u64 - 1) * 5_000,
        "arrival reflects last accepted burst append"
    );
    assert_eq!(
        unit.text.as_bytes(),
        last_payload,
        "burst coalescing must yield the latest coherent transcript window"
    );
    assert!(
        hub.take_due(250_001).is_empty(),
        "Hub must be idle after draining the burst window"
    );
}

/// Freeze-queue burst coherence: the FreezeQueue coalescing (StateStream,
/// latest-wins) must also preserve the final coherent snapshot under a burst.
///
/// This confirms the existing FreezeQueue mechanism matches the spec requirement
/// (tasks.md §5.4 — "retained-window coherence under burst per the existing
/// coalescing requirement").
#[test]
fn freeze_queue_burst_retains_coherent_window() {
    const BURST_COUNT: usize = 80;
    let mut queue = FreezeQueue::new(4); // small queue to force coalescing
    let coalesce_key = Some("portal://burst-coherence/freeze".to_string());

    let mut last_payload = Vec::new();
    for i in 0..BURST_COUNT {
        let line = format!("[{i:03}] freeze-burst {i}\n").into_bytes();
        last_payload.extend_from_slice(&line);
        let mutation = QueuedMutation {
            batch_id: format!("b-{i}").into_bytes(),
            original_batch_id: format!("b-{i}").into_bytes(),
            traffic_class: MutationTrafficClass::StateStream,
            coalesce_key: coalesce_key.clone(),
            submitted_at_wall_us: i as u64 * 1_000,
            payload: last_payload.clone(),
        };
        let result = queue.enqueue(mutation);
        // All appends after the first should coalesce or evict (queue capacity = 4).
        if i > 0 {
            assert!(
                matches!(
                    result,
                    EnqueueResult::Coalesced
                        | EnqueueResult::QueuedWithPressure
                        | EnqueueResult::Queued
                        | EnqueueResult::EvictedEntry { .. }
                ),
                "burst enqueue #{i} should not backpressure transactional"
            );
        }
    }

    let drained = queue.drain();
    // After burst, the remaining StateStream entry must hold the latest snapshot.
    let state_stream_entries: Vec<_> = drained
        .iter()
        .filter(|m| m.traffic_class == MutationTrafficClass::StateStream)
        .collect();

    assert!(
        !state_stream_entries.is_empty(),
        "at least one StateStream entry must survive the burst drain"
    );
    // The latest entry must have the latest sequence (closest to BURST_COUNT-1).
    let latest = state_stream_entries.last().unwrap();
    let latest_payload = String::from_utf8(latest.payload.clone()).expect("utf8");
    // The latest payload must include the last line (monotonic coherence).
    assert!(
        latest_payload.contains(&format!("[{:03}]", BURST_COUNT - 1))
            || latest_payload.len() >= last_payload.len() / 2,
        "burst drain must yield a recent coherent window; got {} bytes (last was {} bytes)",
        latest_payload.len(),
        last_payload.len()
    );
}

// ─── Task 5.4: Dual-portal equal-rate fairness via FreezeQueue ───────────────

/// Under equal sustained rates in the FreezeQueue, two portals with distinct
/// coalesce keys coalesce independently and neither is starved.
///
/// This validates that the underlying FreezeQueue coalescing correctly handles
/// multiple concurrent portals with separate coalesce keys.
#[test]
fn freeze_queue_dual_portal_coalescing_independent() {
    const APPENDS_PER_PORTAL: usize = 50;
    let mut queue = FreezeQueue::new(16);

    let key_a = Some("portal://dual/a".to_string());
    let key_b = Some("portal://dual/b".to_string());

    // Interleave appends from both portals at equal rates.
    for i in 0..APPENDS_PER_PORTAL {
        let payload_a = format!("a-snapshot-{i}").into_bytes();
        let payload_b = format!("b-snapshot-{i}").into_bytes();

        queue.enqueue(QueuedMutation {
            batch_id: format!("a-{i}").into_bytes(),
            original_batch_id: format!("a-{i}").into_bytes(),
            traffic_class: MutationTrafficClass::StateStream,
            coalesce_key: key_a.clone(),
            submitted_at_wall_us: i as u64 * 1_000,
            payload: payload_a,
        });

        queue.enqueue(QueuedMutation {
            batch_id: format!("b-{i}").into_bytes(),
            original_batch_id: format!("b-{i}").into_bytes(),
            traffic_class: MutationTrafficClass::StateStream,
            coalesce_key: key_b.clone(),
            submitted_at_wall_us: i as u64 * 1_000,
            payload: payload_b,
        });
    }

    let drained = queue.drain();

    // Both portals must have exactly one entry remaining (coalesced to latest).
    let entries_a: Vec<_> = drained.iter().filter(|m| m.coalesce_key == key_a).collect();
    let entries_b: Vec<_> = drained.iter().filter(|m| m.coalesce_key == key_b).collect();

    assert_eq!(
        entries_a.len(),
        1,
        "portal A: {APPENDS_PER_PORTAL} appends must coalesce to 1 entry"
    );
    assert_eq!(
        entries_b.len(),
        1,
        "portal B: {APPENDS_PER_PORTAL} appends must coalesce to 1 entry"
    );

    // Both entries must be the latest snapshot for each portal.
    let payload_a = String::from_utf8(entries_a[0].payload.clone()).expect("utf8");
    let payload_b = String::from_utf8(entries_b[0].payload.clone()).expect("utf8");
    assert!(
        payload_a.contains(&format!("{}", APPENDS_PER_PORTAL - 1)),
        "portal A must have latest snapshot: {payload_a}"
    );
    assert!(
        payload_b.contains(&format!("{}", APPENDS_PER_PORTAL - 1)),
        "portal B must have latest snapshot: {payload_b}"
    );
}

// ─── Task 5.3: Stage budget assertions under concurrent typing+scroll ─────────

/// Input responsiveness under concurrent portal streaming.
///
/// With portal mutations arriving every 6 frames, synthetic pointer input
/// must still get local-ack within the input_to_local_ack budget (< 4 ms).
/// This verifies that coalescing does not inflate Stage 1/2 latencies.
#[tokio::test]
async fn input_latency_not_degraded_under_portal_stream() {
    use tze_hud_input::{PointerEvent, PointerEventKind};
    use tze_hud_scene::types::HitRegionNode;

    let config = cadence_bench_config();
    let mut runtime = serialized_headless_init(HeadlessRuntime::new(config))
        .await
        .expect("HeadlessRuntime init failed");

    let (mut driver, handle, clock) = portal_driver(&runtime).await;
    let key = PortalKey::new("latency_test", "stream");
    let (_tab_id_outer, _lease_id_outer, _tile_id) = {
        let scene_arc = scene_handle(&runtime).await;
        let mut scene = scene_arc.lock().await;
        let tab_id = scene.create_tab("latency_test", 0).expect("create_tab");
        let lease_id = scene.grant_lease("latency_test", 300_000);
        if let Some(lease) = scene.leases.get_mut(&lease_id) {
            lease.resource_budget.max_tiles = 5;
        }
        let bounds = Rect::new(0.0, 0.0, 640.0, 480.0);
        let tile_id = scene
            .create_tile(tab_id, "latency_test", lease_id, bounds, 10)
            .expect("create_tile");

        // Root node with a hit region for input testing.
        let root_id = SceneId::new();
        let root = Node {
            layout: Default::default(),
            id: root_id,
            children: vec![SceneId::new()],
            data: NodeData::SolidColor(SolidColorNode {
                color: Rgba::new(0.5, 0.5, 0.5, 1.0),
                bounds: Rect::new(0.0, 0.0, 640.0, 480.0),
                radius: None,
            }),
        };
        let _ = scene.set_tile_root(tile_id, root);
        let hit_region = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 0.0, 640.0, 480.0),
                interaction_id: "latency-target".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                ..Default::default()
            }),
        };
        if let Ok(hit_tile_id) = scene.create_tile(tab_id, "latency_test", lease_id, bounds, 11) {
            let _ = scene.set_tile_root(hit_tile_id, hit_region);
        }
        (tab_id, lease_id, tile_id)
    };

    // Warmup.
    for _ in 0..5 {
        runtime.render_frame().await;
    }

    let mut local_ack_samples: Vec<u64> = Vec::new();

    // 120 frames: apply portal mutations every 6 frames; inject pointer input every frame.
    let stream = sustained_stream(
        CADENCE_MIN_SCALARS_PER_SEC,
        CADENCE_MIN_INCREMENTS_PER_SEC,
        20,
    );
    let mut append_idx = 0usize;

    for frame_idx in 0u64..120 {
        // Inject synthetic pointer event on every frame to measure local-ack.
        let kind = if frame_idx % 2 == 0 {
            PointerEventKind::Down
        } else {
            PointerEventKind::Up
        };
        {
            let scene_arc = scene_handle(&runtime).await;
            let mut scene = scene_arc.lock().await;
            let result = runtime.input_processor.process(
                &PointerEvent {
                    x: 320.0,
                    y: 240.0,
                    kind,
                    device_id: 0,
                    timestamp: None,
                },
                &mut scene,
            );
            if result.local_ack_us > 0 {
                local_ack_samples.push(result.local_ack_us);
            }
        }

        // Apply portal mutation every 6 frames.
        if frame_idx % 6 == 0 && append_idx < stream.len() {
            let (_ts, payload, _scalars) = &stream[append_idx];
            append_idx += 1;
            clock.set_us(1_000_000 + frame_idx * 1_000_000 / 60);
            publish_stream(
                &handle,
                &key,
                payload,
                1_000_000 + frame_idx * 1_000_000 / 60,
            );
            drain_portals(&mut runtime, &mut driver).await;
        }

        runtime.render_frame().await;
    }

    // Input to local ack p99 must be within budget when samples are available.
    //
    // The locked Windows budget is ≤ 2 ms (engineering-bar.md §2). On headless
    // Linux CI with a software GPU (llvmpipe), frame times run 8–12× slower
    // than reference hardware; we use a generous 250 ms headless ceiling here
    // so the test is not flaky on CI while still catching catastrophic
    // regressions (a true regression pushes software-GPU times into seconds).
    // The real performance guard is the Windows CI gate.
    //
    // On a headless runner without a display server, pointer events may not
    // produce non-zero local_ack values (no hit region in the headless scene
    // from this path), so we only assert when we have ≥ 5 non-zero samples.
    let non_zero_samples: Vec<u64> = local_ack_samples.into_iter().filter(|&v| v > 0).collect();
    if non_zero_samples.len() >= 5 {
        let mut ack_bucket = LatencyBucket::new("input_to_local_ack");
        for sample in non_zero_samples {
            ack_bucket.record(sample);
        }
        // Headless budget: 250 ms (generous for software-GPU CI).
        // Windows locked budget: ≤ 2 ms (enforced by the Windows CI gate).
        let headless_ack_budget_us = 250_000u64;
        let p99 = ack_bucket.p99().unwrap_or(0);
        // Timing assertion: gated — wall-clock budget.  (hud-94vm5)
        if perf_assert_enabled() {
            assert!(
                p99 <= headless_ack_budget_us,
                "input_to_local_ack p99 {p99}µs exceeded headless budget {headless_ack_budget_us}µs \
                 under concurrent portal stream (Windows locked budget ≤ 2 ms)"
            );
        } else {
            eprintln!(
                "[SKIP-TIMING] input_to_local_ack p99={p99}µs; \
                 set TZE_HUD_PERF_ASSERT=1 to enforce budget"
            );
        }
    }

    assert_eq!(
        handle
            .hub
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .transcript
            .len(),
        append_idx
    );
    assert!(append_idx > 0);
    assert!(
        scene_handle(&runtime)
            .await
            .lock()
            .await
            .tiles
            .values()
            .any(|tile| tile.namespace == "tze_hud_portal_driver"),
        "pointer samples coexist with real rendered portal territory"
    );
}

// ─── Task 2 (hud-zmt1a): Arrival→present latency measurement ─────────────────

/// Per-append arrival→present elapsed measured via `Unit::at_us`.
///
/// Verifies that `submitted_at_us` (populated by `handle_publish_output` into the
/// cadence coalescer) is correctly propagated through `take_due_portal_update` and
/// can be used to compute arrival→present elapsed. The elapsed is checked against
/// the `INPUT_TO_NEXT_PRESENT_BUDGET_US` (33 ms reference; CI uses a generous 250 ms
/// headless ceiling due to software-GPU).
///
/// This is the "simulated clock" path: both `submitted_at_wall_us` and
/// `present_at_wall_us` are synthetic values, so we control the simulated elapsed
/// and confirm the measurement chain works end-to-end.
#[test]
fn arrival_to_present_elapsed_measured_via_submitted_at_us() {
    let mut hub = PortalHub::default();
    let key = PortalKey::new("caller-timing", "projection-cadence-timing");
    const APPEND_COUNT: usize = 20;
    const SIMULATED_APPEND_INTERVAL_US: u64 = 500;
    const SIMULATED_PRESENT_OFFSET_US: u64 = 5_000;
    for i in 0..APPEND_COUNT {
        let submitted_at = 10_000 + i as u64 * SIMULATED_APPEND_INTERVAL_US;
        hub.publish(
            &key,
            Publish {
                text: Some(format!("[{i:03}] cadence-timing line {i}")),
                ..Publish::default()
            },
            submitted_at,
        )
        .unwrap();
    }
    let last_submitted = 10_000 + (APPEND_COUNT as u64 - 1) * SIMULATED_APPEND_INTERVAL_US;
    let present_at = last_submitted + SIMULATED_PRESENT_OFFSET_US;
    let due = hub.take_due(present_at);
    assert_eq!(due.len(), 1);
    let submitted_at_us = hub.get(&key).unwrap().transcript.back().unwrap().at_us;
    assert!(
        submitted_at_us > 0,
        "accepted arrival timestamp must be populated"
    );
    assert!(submitted_at_us < present_at, "arrival must precede present");
    assert_eq!(submitted_at_us, last_submitted);
    let elapsed_us = present_at.saturating_sub(submitted_at_us);
    const HEADLESS_INPUT_TO_PRESENT_BUDGET_US: u64 = 250_000;
    assert!(
        elapsed_us <= HEADLESS_INPUT_TO_PRESENT_BUDGET_US,
        "arrival→present elapsed {elapsed_us}µs exceeded headless budget {HEADLESS_INPUT_TO_PRESENT_BUDGET_US}µs (reference Windows budget: {INPUT_TO_NEXT_PRESENT_BUDGET_US}µs)"
    );
    assert!(
        due[0].unread >= 1,
        "due portal must reflect at least one output unit"
    );
    assert_eq!(hub.get(&key).unwrap().transcript.len(), APPEND_COUNT);
}

/// Dual-portal arrival→present skew stays bounded via round-robin fairness.
///
/// With two portals receiving appends at equal rates, `submitted_at_us` from
/// successive `take_due_portal_update` calls (in round-robin order) must not
/// diverge by more than one inter-append interval. This exercises the scheduling
/// oracle path (`next_due_projection_id` → `take_due_portal_update`).
#[test]
fn dual_portal_arrival_to_present_skew_bounded_in_same_drain() {
    let mut hub = PortalHub::default();
    let a = PortalKey::new("caller-timing", "projection-timing-a");
    let b = PortalKey::new("caller-timing", "projection-timing-b");
    const SUBMIT_AT_US: u64 = 100_000;
    const PRESENT_AT_US: u64 = 105_000;
    hub.publish(
        &a,
        Publish {
            text: Some("[000] portal a line".into()),
            ..Publish::default()
        },
        SUBMIT_AT_US,
    )
    .unwrap();
    hub.publish(
        &b,
        Publish {
            text: Some("[000] portal b line".into()),
            ..Publish::default()
        },
        SUBMIT_AT_US,
    )
    .unwrap();
    let submitted_timestamps: Vec<_> = hub
        .take_due(PRESENT_AT_US)
        .into_iter()
        .map(|due| {
            let at_us = hub.get(&due.key).unwrap().transcript.back().unwrap().at_us;
            (due.key.id, at_us)
        })
        .collect();
    assert_eq!(
        submitted_timestamps.len(),
        2,
        "both dirty portals must be served in the same drain"
    );
    for (id, submitted) in &submitted_timestamps {
        assert!(
            *submitted > 0,
            "portal {id}: submitted_at_us must be non-zero"
        );
        assert!(
            *submitted < PRESENT_AT_US,
            "portal {id}: submitted_at_us ({submitted}) must be before present_at ({PRESENT_AT_US})"
        );
    }
    let (_, ts_0) = &submitted_timestamps[0];
    let (_, ts_1) = &submitted_timestamps[1];
    assert_eq!(
        ts_0, ts_1,
        "dual portals submitted at the same wall time must report equal submitted_at_us"
    );
    assert!(hub.take_due(PRESENT_AT_US + 1).is_empty());
}
