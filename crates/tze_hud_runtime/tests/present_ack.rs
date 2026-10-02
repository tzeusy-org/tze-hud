//! End-to-end headless coverage for the batch-correlated present-acknowledgment
//! path (hud-91uu6).
//!
//! Proves the correlation primitive delivered by the producer half of the
//! FramePresented protocol: a gRPC-style `MutationBatch` applied to the runtime
//! scene is paired, at true on-screen present, with the frame's present
//! wall-clock and emitted on the `frame_presented_tx` broadcast — the same
//! sender a session handler subscribes to and gates on TELEMETRY_FRAMES.
//!
//! This is the non-proxy present-time signal that de-blocks hud-vjlqh: the
//! present wall-clock is sampled at Stage 7 completion, distinct from the
//! transport RTT of the mutation submit.

use std::time::{SystemTime, UNIX_EPOCH};

use tze_hud_protocol::proto::FramePresented;
use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;
use tze_hud_scene::mutation::{MutationBatch, SceneMutation};
use tze_hud_scene::types::Rect;

fn wall_us_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

async fn make_runtime() -> HeadlessRuntime {
    let config = HeadlessConfig {
        width: 320,
        height: 240,
        grpc_port: 0,
        bind_all_interfaces: false,
        agents: tze_hud_scene::config::AgentDirectory::unrestricted("test"),
        config_toml: None,
    };
    HeadlessRuntime::new(config).await.expect("runtime init")
}

/// Apply a tile-creating `MutationBatch` to the runtime scene and return the
/// batch_id. Mirrors what the gRPC `handle_mutation_batch` path does under the
/// scene lock (`scene.apply_batch`).
async fn apply_create_tile_batch(runtime: &HeadlessRuntime) -> tze_hud_scene::SceneId {
    let state = runtime.shared_state().lock().await;
    let mut scene = state.scene.lock().await;
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("test-agent", 60_000);
    let batch = MutationBatch {
        batch_id: tze_hud_scene::SceneId::new(),
        agent_namespace: "test-agent".to_string(),
        mutations: vec![SceneMutation::CreateTile {
            tab_id: tab,
            namespace: "test-agent".to_string(),
            lease_id: lease,
            bounds: Rect::new(10.0, 10.0, 200.0, 100.0),
            z_order: 1,
        }],
        timing_hints: None,
        lease_id: Some(lease),
    };
    let batch_id = batch.batch_id;
    assert!(scene.apply_batch(&batch).applied, "batch must apply");
    batch_id
}

/// An applied batch is delivered on the frame-presented broadcast, correlated
/// to the presented frame's wall-clock present time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn applied_batch_delivered_as_frame_presented() {
    let mut runtime = make_runtime().await;

    // Wire a standalone broadcast sender (stands in for HudSessionImpl's) and
    // subscribe before applying so the receiver observes the present ack.
    let (tx, mut rx) = tokio::sync::broadcast::channel::<FramePresented>(16);
    runtime.set_frame_presented_tx(tx);

    let batch_id = apply_create_tile_batch(&runtime).await;
    let expected_bytes = batch_id.as_uuid().as_bytes().to_vec();

    // A present time sampled BEFORE the frame renders — the emitted
    // present_wall_us must be at least this (it is stamped at Stage 7).
    let before_present = wall_us_now();

    let telemetry = runtime.render_frame().await;

    let event = rx
        .try_recv()
        .expect("frame present ack must be delivered for the applied batch");

    assert_eq!(
        event.batch_ids,
        vec![expected_bytes],
        "present ack carries the applied batch_id"
    );
    assert_eq!(
        event.frame_number, telemetry.frame_number,
        "present ack frame_number matches the presented frame"
    );
    // The present timestamp is a true wall-clock present time (Stage 7), not a
    // zero placeholder and not sampled before the frame began rendering.
    assert!(
        event.present_wall_us >= before_present,
        "present_wall_us ({}) must be >= pre-render wall clock ({before_present})",
        event.present_wall_us
    );
    assert!(
        event.present_wall_us <= wall_us_now(),
        "present_wall_us must not be in the future"
    );
}

/// A frame with no applied batch since the last present emits no ack, and the
/// pending queue is one-shot (a second frame does not re-deliver).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_batch_no_ack_and_drain_is_one_shot() {
    let mut runtime = make_runtime().await;
    let (tx, mut rx) = tokio::sync::broadcast::channel::<FramePresented>(16);
    runtime.set_frame_presented_tx(tx);

    apply_create_tile_batch(&runtime).await;

    // First frame carries the batch → exactly one ack.
    runtime.render_frame().await;
    let first = rx.try_recv().expect("first frame delivers the ack");
    assert_eq!(first.batch_ids.len(), 1);

    // Second frame: no new applies → nothing pending → no ack.
    runtime.render_frame().await;
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "a frame with no applied batch must not emit a present ack"
    );
}

/// Multiple batches applied before a single present are all correlated to that
/// frame, in application order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_batches_share_one_presented_frame() {
    let mut runtime = make_runtime().await;
    let (tx, mut rx) = tokio::sync::broadcast::channel::<FramePresented>(16);
    runtime.set_frame_presented_tx(tx);

    // Set up scene once, then apply three separate batches before rendering.
    let mut expected = Vec::new();
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab = scene.create_tab("Main", 0).unwrap();
        let lease = scene.grant_lease("test-agent", 60_000);
        for i in 0..3u32 {
            let batch = MutationBatch {
                batch_id: tze_hud_scene::SceneId::new(),
                agent_namespace: "test-agent".to_string(),
                mutations: vec![SceneMutation::CreateTile {
                    tab_id: tab,
                    namespace: "test-agent".to_string(),
                    lease_id: lease,
                    bounds: Rect::new(10.0 * (i + 1) as f32, 10.0, 80.0, 60.0),
                    z_order: i + 1,
                }],
                timing_hints: None,
                lease_id: Some(lease),
            };
            expected.push(batch.batch_id.as_uuid().as_bytes().to_vec());
            assert!(scene.apply_batch(&batch).applied);
        }
    }

    runtime.render_frame().await;

    let event = rx.try_recv().expect("present ack delivered");
    assert_eq!(
        event.batch_ids, expected,
        "all three batch_ids ride one presented frame, in application order"
    );
}
