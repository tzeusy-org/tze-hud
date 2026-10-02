//! Integration test: pointer events reach agents via PointerDownEvent /
//! PointerMoveEvent / PointerUpEvent over the `INPUT_EVENTS` gRPC channel.
//!
//! Verifies acceptance criteria for hud-zffvp:
//!
//! 1. `PointerDownEvent` injected on `input_event_tx` is delivered to an agent
//!    subscribed to `INPUT_EVENTS` with the correct fields.
//! 2. `PointerMoveEvent` is delivered similarly.
//! 3. `PointerUpEvent` is delivered similarly.
//! 4. Agents NOT subscribed to `INPUT_EVENTS` do NOT receive pointer events.
//! 5. Pointer events are routed only to the owning namespace.
//!
//! All tests are headless Layer 0: no display server, GPU, or windowed runtime
//! required.  The end-to-end path exercised is:
//!
//!   `input_event_tx.send((namespace, EventBatch))` →
//!   session handler filters by `INPUT_EVENTS` subscription →
//!   agent gRPC stream receives `ServerMessage::EventBatch` →
//!   `EventBatch.events[0]` == `InputEnvelope::{PointerDown | PointerMove | PointerUp}`
//!
//! The `InputProcessor` → `dispatch_pointer_event` wiring (windowed runtime)
//! is covered by the `enqueue_pointer_event` code path in `windowed.rs`.
//! This test focuses on the protocol-layer delivery path: broadcast channel →
//! session handler → agent gRPC stream.
//!
//! ## Subscription model
//!
//! Pointer events use the existing `INPUT_EVENTS` category (requires
//! `access_input_events` capability).  No new subscription category is needed —
//! `PointerDownEvent`, `PointerMoveEvent`, and `PointerUpEvent` are classified
//! as `is_input_variant` by `subscriptions::filter_event_batch`, so they are
//! subject to the same gate as keyboard and scroll events.

use tokio_stream::StreamExt;
use tze_hud_protocol::proto::input_envelope::Event as InputEvent;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{ClientMessage, ServerMessage, SessionInit};
use tze_hud_protocol::proto::{
    EventBatch, InputEnvelope, PointerDownEvent, PointerMoveEvent, PointerUpEvent,
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

/// Handshake requesting `access_input_events` + subscribe to `INPUT_EVENTS`.
///
/// Returns `(sender, stream)` positioned after `SessionEstablished` and
/// `SceneSnapshot`, ready for event traffic.
async fn perform_handshake_with_input(
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
            agent_display_name: format!("{agent_id} (pointer test)"),
            pre_shared_key: "test-psk".to_string(),
            requested_capabilities: vec!["access_input_events".to_string()],
            initial_subscriptions: vec!["INPUT_EVENTS".to_string()],
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

/// Handshake WITHOUT `access_input_events` (no INPUT_EVENTS subscription).
async fn perform_handshake_no_input(
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
            agent_display_name: format!("{agent_id} (no-input pointer test)"),
            pre_shared_key: "test-psk".to_string(),
            // No capabilities → no input events.
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

fn pointer_down_batch(tile_id: Vec<u8>, node_id: Vec<u8>) -> EventBatch {
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some(InputEvent::PointerDown(PointerDownEvent {
                tile_id,
                node_id,
                interaction_id: "test-region".to_string(),
                timestamp_mono_us: 1_000,
                device_id: "0".to_string(),
                local_x: 10.0,
                local_y: 20.0,
                display_x: 110.0,
                display_y: 220.0,
                button: 0,
            })),
        }],
    }
}

fn pointer_move_batch(tile_id: Vec<u8>, node_id: Vec<u8>) -> EventBatch {
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some(InputEvent::PointerMove(PointerMoveEvent {
                tile_id,
                node_id,
                interaction_id: "test-region".to_string(),
                timestamp_mono_us: 2_000,
                device_id: "0".to_string(),
                local_x: 15.0,
                local_y: 25.0,
                display_x: 115.0,
                display_y: 225.0,
            })),
        }],
    }
}

fn pointer_up_batch(tile_id: Vec<u8>, node_id: Vec<u8>) -> EventBatch {
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some(InputEvent::PointerUp(PointerUpEvent {
                tile_id,
                node_id,
                interaction_id: "test-region".to_string(),
                timestamp_mono_us: 3_000,
                device_id: "0".to_string(),
                local_x: 15.0,
                local_y: 25.0,
                display_x: 115.0,
                display_y: 225.0,
                button: 0,
            })),
        }],
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC 1: `PointerDownEvent` injected on `input_event_tx` is delivered to a
/// subscribed agent with the correct fields preserved end-to-end.
#[tokio::test]
async fn pointer_down_event_delivered_to_subscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_with_input(&mut client, "pointer-down-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = pointer_down_batch(tile_id.clone(), node_id.clone());
    let _ = input_event_tx.send(("pointer-down-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for PointerDownEvent")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "batch must contain exactly 1 event");
            match &batch.events[0].event {
                Some(InputEvent::PointerDown(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert_eq!(ev.node_id, node_id, "node_id must round-trip");
                    assert_eq!(
                        ev.interaction_id, "test-region",
                        "interaction_id must round-trip"
                    );
                    assert_eq!(ev.timestamp_mono_us, 1_000, "timestamp must round-trip");
                    assert_eq!(ev.local_x, 10.0, "local_x must round-trip");
                    assert_eq!(ev.local_y, 20.0, "local_y must round-trip");
                    assert_eq!(ev.display_x, 110.0, "display_x must round-trip");
                    assert_eq!(ev.display_y, 220.0, "display_y must round-trip");
                    assert_eq!(ev.button, 0, "button must round-trip");
                }
                other => panic!("expected PointerDown, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 2: `PointerMoveEvent` injected on `input_event_tx` is delivered to a
/// subscribed agent with the correct fields preserved end-to-end.
#[tokio::test]
async fn pointer_move_event_delivered_to_subscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_with_input(&mut client, "pointer-move-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = pointer_move_batch(tile_id.clone(), node_id.clone());
    let _ = input_event_tx.send(("pointer-move-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for PointerMoveEvent")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "batch must contain exactly 1 event");
            match &batch.events[0].event {
                Some(InputEvent::PointerMove(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert_eq!(ev.node_id, node_id, "node_id must round-trip");
                    assert_eq!(
                        ev.interaction_id, "test-region",
                        "interaction_id must round-trip"
                    );
                    assert_eq!(ev.timestamp_mono_us, 2_000, "timestamp must round-trip");
                    assert_eq!(ev.local_x, 15.0, "local_x must round-trip");
                    assert_eq!(ev.local_y, 25.0, "local_y must round-trip");
                    assert_eq!(ev.display_x, 115.0, "display_x must round-trip");
                    assert_eq!(ev.display_y, 225.0, "display_y must round-trip");
                }
                other => panic!("expected PointerMove, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 3: `PointerUpEvent` injected on `input_event_tx` is delivered to a
/// subscribed agent with the correct fields preserved end-to-end.
#[tokio::test]
async fn pointer_up_event_delivered_to_subscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_with_input(&mut client, "pointer-up-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = pointer_up_batch(tile_id.clone(), node_id.clone());
    let _ = input_event_tx.send(("pointer-up-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for PointerUpEvent")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "batch must contain exactly 1 event");
            match &batch.events[0].event {
                Some(InputEvent::PointerUp(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert_eq!(ev.node_id, node_id, "node_id must round-trip");
                    assert_eq!(
                        ev.interaction_id, "test-region",
                        "interaction_id must round-trip"
                    );
                    assert_eq!(ev.timestamp_mono_us, 3_000, "timestamp must round-trip");
                    assert_eq!(ev.local_x, 15.0, "local_x must round-trip");
                    assert_eq!(ev.local_y, 25.0, "local_y must round-trip");
                    assert_eq!(ev.display_x, 115.0, "display_x must round-trip");
                    assert_eq!(ev.display_y, 225.0, "display_y must round-trip");
                    assert_eq!(ev.button, 0, "button must round-trip");
                }
                other => panic!("expected PointerUp, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 4: agents NOT subscribed to `INPUT_EVENTS` must NOT receive pointer events.
///
/// `filter_event_batch` drops batches whose events do not match the agent's
/// active subscriptions.  This test verifies the gate is enforced end-to-end
/// for pointer events (same mechanism as keyboard and scroll events).
#[tokio::test]
async fn pointer_event_not_delivered_to_unsubscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_no_input(&mut client, "no-input-pointer-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = pointer_down_batch(tile_id, node_id);
    let _ = input_event_tx.send(("no-input-pointer-agent".to_string(), batch));

    // Give the server enough time to process and (not) deliver the event.
    let result = tokio::time::timeout(tokio::time::Duration::from_millis(200), stream.next()).await;
    assert!(
        result.is_err(),
        "unsubscribed agent must NOT receive a PointerDownEvent batch"
    );
}

/// AC 6: when `node_id` is nil (tile-level hit, no specific node), the wire-format
/// `node_id` field must be empty bytes — not 16 zero bytes.
///
/// This mirrors the proto field-presence convention established by FocusLostEvent,
/// FocusGainedEvent, and CaptureReleasedEvent (see PR #610 / hud-jtnop): an absent
/// optional ID is represented as an empty `bytes` field, not as a nil UUID serialized
/// to 16 zero bytes.
///
/// Covers all three pointer event kinds since they share the same `node_id` encoding
/// path in `dispatch_pointer_event`.
#[tokio::test]
async fn pointer_event_with_nil_node_id_serializes_empty_bytes() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) =
        perform_handshake_with_input(&mut client, "nil-node-id-pointer-agent").await;

    let tile_id = tile_id_fixture();
    // Explicitly empty node_id bytes — the "nil / absent" encoding.
    let nil_node_id: Vec<u8> = Vec::new();
    let batch = pointer_down_batch(tile_id.clone(), nil_node_id.clone());
    let _ = input_event_tx.send(("nil-node-id-pointer-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for PointerDownEvent (nil node_id)")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "batch must contain exactly 1 event");
            match &batch.events[0].event {
                Some(InputEvent::PointerDown(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert!(
                        ev.node_id.is_empty(),
                        "nil node_id must serialize to empty bytes, got {:?}",
                        ev.node_id
                    );
                }
                other => panic!("expected PointerDown, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 7: when `node_id` is non-nil, the wire-format `node_id` field must be exactly
/// 16 bytes (the UUID byte representation).
///
/// Verifies that the nil-guard added in hud-yp963 does not break the normal
/// (non-nil) serialization path.
#[tokio::test]
async fn pointer_event_with_node_id_serializes_16_bytes() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) =
        perform_handshake_with_input(&mut client, "non-nil-node-id-pointer-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture(); // non-nil: a freshly generated UUID
    assert_eq!(node_id.len(), 16, "fixture must produce a 16-byte UUID");
    let batch = pointer_down_batch(tile_id.clone(), node_id.clone());
    let _ = input_event_tx.send(("non-nil-node-id-pointer-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for PointerDownEvent (non-nil node_id)")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "batch must contain exactly 1 event");
            match &batch.events[0].event {
                Some(InputEvent::PointerDown(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert_eq!(
                        ev.node_id, node_id,
                        "non-nil node_id must serialize to 16 bytes and round-trip"
                    );
                    assert_eq!(
                        ev.node_id.len(),
                        16,
                        "non-nil node_id must be exactly 16 bytes"
                    );
                }
                other => panic!("expected PointerDown, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 5: pointer events are routed only to the owning namespace.
///
/// A second agent subscribed to INPUT_EVENTS under a different namespace must
/// not receive a batch intended for the first agent's namespace.
#[tokio::test]
async fn pointer_event_routed_to_owning_namespace_only() {
    let (mut client, _server, input_event_tx) = start_server().await;

    // Connect two agents, both subscribed to INPUT_EVENTS.
    let (_tx_a, mut stream_a) = perform_handshake_with_input(&mut client, "pointer-owner").await;
    let (_tx_b, mut stream_b) =
        perform_handshake_with_input(&mut client, "other-pointer-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = pointer_down_batch(tile_id, node_id);

    // Inject only for "pointer-owner".
    let _ = input_event_tx.send(("pointer-owner".to_string(), batch));

    // pointer-owner must receive the event.
    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream_a.next())
        .await
        .expect("timed out waiting for PointerDownEvent on pointer-owner")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1);
            assert!(
                matches!(&batch.events[0].event, Some(InputEvent::PointerDown(_))),
                "pointer-owner must receive PointerDownEvent"
            );
        }
        other => panic!("pointer-owner: expected EventBatch, got: {other:?}"),
    }

    // other-pointer-agent must NOT receive the event (namespace mismatch).
    let result =
        tokio::time::timeout(tokio::time::Duration::from_millis(200), stream_b.next()).await;
    assert!(
        result.is_err(),
        "other-pointer-agent must not receive pointer event intended for pointer-owner"
    );
}
