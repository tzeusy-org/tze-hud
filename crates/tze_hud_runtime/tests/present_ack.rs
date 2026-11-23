//! Headless producer-half coverage for publication-to-GPU correlation.
//!
//! Publications are paired with a local GPU completion timestamp and
//! emitted through an explicitly wired standalone FramePresented sender. The
//! headless surface present is a no-op; this does not exercise a real gRPC
//! listener, subscription permission gate or the windowed main-thread present.
//!
//! These tests establish neither Windows on-screen completion nor synchronized
//! cross-host latency. Session forwarding has its own fake-broadcast tests.

use crate::gpu_init::serialized_headless_init;

use std::time::{SystemTime, UNIX_EPOCH};

use tze_hud_protocol::proto::FramePresented;
use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;
use tze_hud_scene::{Publication, Publisher, SceneId, ZoneContent, ZoneRegistry};

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
        agents: tze_hud_scene::config::AgentDirectory::unrestricted("test"),
        config_toml: None,
    };
    serialized_headless_init(HeadlessRuntime::new(config))
        .await
        .expect("runtime init")
}

fn subtitle(content: impl Into<String>) -> Publication {
    Publication::Zone {
        zone_name: "subtitle".into(),
        content: ZoneContent::StreamText(content.into()),
        merge_key: None,
        expires_at_wall_us: None,
        content_classification: None,
        breakpoints: Vec::new(),
        held: false,
    }
}

async fn publish_subtitle(runtime: &HeadlessRuntime) -> SceneId {
    let state = runtime.shared_state().lock().await;
    let mut scene = state.scene.lock().await;
    scene.zone_registry = ZoneRegistry::with_defaults();
    scene
        .publish(&Publisher::namespace("test-agent"), &subtitle("one"))
        .unwrap()
}

/// A committed publication is delivered on the frame-presented broadcast, correlated
/// to the presented frame's wall-clock present time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_publication_delivered_as_frame_presented() {
    let mut runtime = make_runtime().await;

    // Wire a standalone broadcast sender (stands in for HudSessionImpl's) and
    // subscribe before applying so the receiver observes the present ack.
    let (tx, mut rx) = tokio::sync::broadcast::channel::<FramePresented>(16);
    runtime.set_frame_presented_tx(tx);

    let batch_id = publish_subtitle(&runtime).await;
    let expected_bytes = batch_id.as_uuid().as_bytes().to_vec();

    // A present time sampled BEFORE the frame renders — the emitted
    // present_wall_us must be at least this (it is stamped at Stage 7).
    let before_present = wall_us_now();

    let telemetry = runtime.render_frame().await;

    let event = rx
        .try_recv()
        .expect("frame present ack must be delivered for the committed publication");

    assert_eq!(
        event.batch_ids,
        vec![expected_bytes],
        "present ack carries the committed publication_id"
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

/// A frame with no committed publication since the last present emits no ack, and the
/// pending queue is one-shot (a second frame does not re-deliver).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_publication_no_ack_and_drain_is_one_shot() {
    let mut runtime = make_runtime().await;
    let (tx, mut rx) = tokio::sync::broadcast::channel::<FramePresented>(16);
    runtime.set_frame_presented_tx(tx);

    publish_subtitle(&runtime).await;

    // First frame carries the publication → exactly one ack.
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
        "a frame with no committed publication must not emit a present ack"
    );
}

/// Multiple publications committed before a single present are all correlated to that
/// frame, in application order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_publications_share_one_presented_frame() {
    let mut runtime = make_runtime().await;
    let (tx, mut rx) = tokio::sync::broadcast::channel::<FramePresented>(16);
    runtime.set_frame_presented_tx(tx);

    // Set up scene once, then apply three separate publications before rendering.
    let mut expected = Vec::new();
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        scene.zone_registry = ZoneRegistry::with_defaults();
        let publisher = Publisher::namespace("test-agent");
        for i in 0..3 {
            let id = scene
                .publish(&publisher, &subtitle(format!("update {i}")))
                .unwrap();
            expected.push(id.as_uuid().as_bytes().to_vec());
        }
    }

    runtime.render_frame().await;

    let event = rx.try_recv().expect("present ack delivered");
    assert_eq!(
        event.batch_ids, expected,
        "all three batch_ids ride one presented frame, in application order"
    );
}
