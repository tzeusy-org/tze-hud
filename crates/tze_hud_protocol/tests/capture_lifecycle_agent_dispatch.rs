//! Integration test: `CaptureReleasedEvent` reaches agents via the `FOCUS_EVENTS`
//! gRPC channel.
//!
//! Verifies acceptance criteria for hud-46xq5:
//!
//! 1. A `CaptureReleasedEvent` injected on `input_event_tx` is delivered to an
//!    agent subscribed to `FOCUS_EVENTS` with the correct fields (tile_id,
//!    node_id, device_id, reason) preserved end-to-end.
//! 2. Agents NOT subscribed to `FOCUS_EVENTS` do NOT receive `CaptureReleasedEvent`.
//! 3. `CaptureReleasedEvent` is routed only to the owning namespace.
//!
//! All tests are headless Layer 0: no display server, GPU, or windowed runtime
//! required.  The end-to-end path exercised is:
//!
//!   `input_event_tx.send((namespace, EventBatch))` →
//!   session handler filters by `FOCUS_EVENTS` subscription →
//!   agent gRPC stream receives `ServerMessage::EventBatch` →
//!   `EventBatch.events[0]` == `InputEnvelope::CaptureReleased`
//!
//! The `dispatch_capture_released_event` wiring in the windowed runtime is tested
//! separately (the windowed.rs `extra_dispatches` routing).  This file tests
//! the protocol-layer delivery path: broadcast channel → session handler →
//! agent gRPC stream.

use tokio_stream::StreamExt;
use tze_hud_protocol::proto::input_envelope::Event as InputEvent;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{ClientMessage, ServerMessage, SessionInit};
use tze_hud_protocol::proto::{
    CaptureReleasedEvent, CaptureReleasedReason, EventBatch, InputEnvelope,
};
use tze_hud_protocol::session_server::HudSessionImpl;
use tze_hud_scene::graph::SceneGraph;

// ── Test infrastructure ───────────────────────────────────────────────────────

fn now_wall_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// Start an in-process HudSession server.
///
/// Returns `(client, server_join_handle, input_event_tx)`.
async fn start_server() -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    tze_hud_protocol::session_server::InputEventSender,
) {
    let scene = SceneGraph::new(1920.0, 1080.0);
    let service = HudSessionImpl::new(scene, "test-psk");
    let input_event_tx = service.input_event_tx.clone();

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    (client, handle, input_event_tx)
}

/// Handshake requesting `access_input_events` + subscribe to `FOCUS_EVENTS`.
///
/// Returns `(sender, stream)` positioned after `SessionEstablished` and
/// `SceneSnapshot`, ready for event traffic.
async fn perform_handshake_with_focus(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    tonic::Streaming<ServerMessage>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let inbound = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: agent_id.to_string(),
            agent_display_name: format!("{agent_id} (capture test)"),
            pre_shared_key: "test-psk".to_string(),
            requested_capabilities: vec!["access_input_events".to_string()],
            initial_subscriptions: vec!["FOCUS_EVENTS".to_string()],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: None,
        })),
    })
    .await
    .unwrap();

    let mut stream = client.session(inbound).await.unwrap().into_inner();

    // Drain SessionEstablished + SceneSnapshot + current DegradationNotice.
    for _ in 0..3 {
        let _ = stream.next().await;
    }

    (tx, stream)
}

/// Handshake WITHOUT `access_input_events` (no FOCUS_EVENTS subscription).
async fn perform_handshake_no_focus(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    tonic::Streaming<ServerMessage>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let inbound = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: agent_id.to_string(),
            agent_display_name: format!("{agent_id} (no-focus capture test)"),
            pre_shared_key: "test-psk".to_string(),
            requested_capabilities: vec![],
            initial_subscriptions: vec![],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: None,
        })),
    })
    .await
    .unwrap();

    let mut stream = client.session(inbound).await.unwrap().into_inner();

    // Drain SessionEstablished + SceneSnapshot + current DegradationNotice.
    for _ in 0..3 {
        let _ = stream.next().await;
    }

    (tx, stream)
}

// ── Batch helpers ─────────────────────────────────────────────────────────────

fn tile_id_fixture() -> Vec<u8> {
    uuid::Uuid::now_v7().as_bytes().to_vec()
}

fn capture_released_batch(
    tile_id: Vec<u8>,
    node_id: Vec<u8>,
    reason: CaptureReleasedReason,
) -> EventBatch {
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some(InputEvent::CaptureReleased(CaptureReleasedEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 3_000,
                device_id: "device-0".to_string(),
                reason: reason as i32,
            })),
        }],
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC 1: `CaptureReleasedEvent` injected on `input_event_tx` is delivered to a
/// FOCUS_EVENTS subscriber with correct fields preserved end-to-end.
///
/// This mirrors the `dispatch_capture_released_event` path in windowed.rs:
/// after a `release_on_up=true` capture, the InputProcessor puts CaptureReleased
/// in `extra_dispatches` following PointerUp, and windowed.rs routes it through
/// `dispatch_capture_released_event` onto the broadcast channel.
#[tokio::test]
async fn capture_released_event_delivered_to_subscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) =
        perform_handshake_with_focus(&mut client, "capture-released-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = capture_released_batch(
        tile_id.clone(),
        node_id.clone(),
        CaptureReleasedReason::PointerUp,
    );
    let _ = input_event_tx.send(("capture-released-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for CaptureReleasedEvent")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "batch must contain exactly 1 event");
            match &batch.events[0].event {
                Some(InputEvent::CaptureReleased(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert_eq!(ev.node_id, node_id, "node_id must round-trip");
                    assert_eq!(ev.timestamp_mono_us, 3_000, "timestamp must round-trip");
                    assert_eq!(ev.device_id, "device-0", "device_id must round-trip");
                    assert_eq!(
                        ev.reason,
                        CaptureReleasedReason::PointerUp as i32,
                        "reason must round-trip as POINTER_UP"
                    );
                }
                other => panic!("expected CaptureReleased, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 2: agents NOT subscribed to `FOCUS_EVENTS` must NOT receive
/// `CaptureReleasedEvent`.
#[tokio::test]
async fn capture_released_event_not_delivered_to_unsubscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_no_focus(&mut client, "no-focus-capture-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = capture_released_batch(tile_id, node_id, CaptureReleasedReason::PointerUp);
    let _ = input_event_tx.send(("no-focus-capture-agent".to_string(), batch));

    // Give the server time to process and (not) deliver the event.
    let result = tokio::time::timeout(tokio::time::Duration::from_millis(200), stream.next()).await;
    assert!(
        result.is_err(),
        "unsubscribed agent must NOT receive a CaptureReleasedEvent batch"
    );
}

/// AC 3: `CaptureReleasedEvent` is routed only to the owning namespace.
///
/// A second agent subscribed to FOCUS_EVENTS under a different namespace must
/// not receive a batch intended for the first agent's namespace.
#[tokio::test]
async fn capture_released_event_routed_to_owning_namespace_only() {
    let (mut client, _server, input_event_tx) = start_server().await;

    let (_tx_a, mut stream_a) = perform_handshake_with_focus(&mut client, "capture-owner").await;
    let (_tx_b, mut stream_b) =
        perform_handshake_with_focus(&mut client, "other-capture-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = capture_released_batch(tile_id, node_id, CaptureReleasedReason::AgentReleased);

    // Inject only for "capture-owner".
    let _ = input_event_tx.send(("capture-owner".to_string(), batch));

    // capture-owner must receive the event.
    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream_a.next())
        .await
        .expect("timed out waiting for CaptureReleasedEvent on capture-owner")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1);
            assert!(
                matches!(&batch.events[0].event, Some(InputEvent::CaptureReleased(_))),
                "capture-owner must receive CaptureReleasedEvent"
            );
        }
        other => panic!("capture-owner: expected EventBatch, got: {other:?}"),
    }

    // other-capture-agent must NOT receive the event (namespace mismatch).
    let result =
        tokio::time::timeout(tokio::time::Duration::from_millis(200), stream_b.next()).await;
    assert!(
        result.is_err(),
        "other-capture-agent must not receive CaptureReleasedEvent intended for capture-owner"
    );
}
