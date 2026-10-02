//! Integration test: focus events reach agents via FocusGainedEvent /
//! FocusLostEvent over the `FOCUS_EVENTS` gRPC channel.
//!
//! Verifies acceptance criteria for hud-b2csq:
//!
//! 1. `FocusGainedEvent` injected on `input_event_tx` is delivered to an agent
//!    subscribed to `FOCUS_EVENTS` with the correct fields.
//! 2. `FocusLostEvent` is delivered similarly.
//! 3. Agents NOT subscribed to `FOCUS_EVENTS` do NOT receive focus events.
//! 4. Focus events are routed only to the owning namespace.
//!
//! All tests are headless Layer 0: no display server, GPU, or windowed runtime
//! required.  The end-to-end path exercised is:
//!
//!   `input_event_tx.send((namespace, EventBatch))` →
//!   session handler filters by `FOCUS_EVENTS` subscription →
//!   agent gRPC stream receives `ServerMessage::EventBatch` →
//!   `EventBatch.events[0]` == `InputEnvelope::{FocusGained | FocusLost}`
//!
//! The `FocusManager` → `dispatch_focus_event` wiring (windowed runtime) is
//! covered by the `enqueue_pointer_event` code path in `windowed.rs`.  This
//! test focuses on the protocol-layer delivery path: broadcast channel →
//! session handler → agent gRPC stream.

use tokio_stream::StreamExt;
use tze_hud_protocol::proto::input_envelope::Event as InputEvent;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{ClientMessage, ServerMessage, SessionInit};
use tze_hud_protocol::proto::{
    EventBatch, FocusGainedEvent, FocusLostEvent, FocusLostReason, FocusSource, InputEnvelope,
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
            initial_subscriptions: vec!["FOCUS_EVENTS".to_string()],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(tze_hud_protocol::auth::psk_credential(
                "test-psk".to_string(),
            )),
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
            // No capabilities → no focus events.
            initial_subscriptions: vec![],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(tze_hud_protocol::auth::psk_credential(
                "test-psk".to_string(),
            )),
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

fn focus_gained_batch(tile_id: Vec<u8>, node_id: Vec<u8>) -> EventBatch {
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some(InputEvent::FocusGained(FocusGainedEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 1_000,
                source: FocusSource::Click as i32,
            })),
        }],
    }
}

fn focus_lost_batch(tile_id: Vec<u8>, node_id: Vec<u8>) -> EventBatch {
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some(InputEvent::FocusLost(FocusLostEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 2_000,
                reason: FocusLostReason::ClickElsewhere as i32,
            })),
        }],
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC 1: `FocusGainedEvent` injected on `input_event_tx` is delivered to a
/// subscribed agent with the correct fields preserved end-to-end.
#[tokio::test]
async fn focus_gained_event_delivered_to_subscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_with_focus(&mut client, "focus-gained-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = focus_gained_batch(tile_id.clone(), node_id.clone());
    let _ = input_event_tx.send(("focus-gained-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for FocusGainedEvent")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "batch must contain exactly 1 event");
            match &batch.events[0].event {
                Some(InputEvent::FocusGained(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert_eq!(ev.node_id, node_id, "node_id must round-trip");
                    assert_eq!(ev.timestamp_mono_us, 1_000, "timestamp must round-trip");
                    assert_eq!(
                        ev.source,
                        FocusSource::Click as i32,
                        "source must round-trip"
                    );
                }
                other => panic!("expected FocusGained, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 2: `FocusLostEvent` is delivered to a subscribed agent with the correct
/// fields preserved end-to-end.
#[tokio::test]
async fn focus_lost_event_delivered_to_subscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_with_focus(&mut client, "focus-lost-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = focus_lost_batch(tile_id.clone(), node_id.clone());
    let _ = input_event_tx.send(("focus-lost-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for FocusLostEvent")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "batch must contain exactly 1 event");
            match &batch.events[0].event {
                Some(InputEvent::FocusLost(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert_eq!(ev.node_id, node_id, "node_id must round-trip");
                    assert_eq!(ev.timestamp_mono_us, 2_000, "timestamp must round-trip");
                    assert_eq!(
                        ev.reason,
                        FocusLostReason::ClickElsewhere as i32,
                        "reason must round-trip"
                    );
                }
                other => panic!("expected FocusLost, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 3: agents NOT subscribed to `FOCUS_EVENTS` must NOT receive focus events.
///
/// `filter_event_batch` drops batches whose events do not match the agent's
/// active subscriptions.  This test verifies the gate is enforced end-to-end
/// for focus events (same mechanism as keyboard events).
#[tokio::test]
async fn focus_event_not_delivered_to_unsubscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_no_focus(&mut client, "no-focus-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = focus_gained_batch(tile_id, node_id);
    let _ = input_event_tx.send(("no-focus-agent".to_string(), batch));

    // Give the server enough time to process and (not) deliver the event.
    let result = tokio::time::timeout(tokio::time::Duration::from_millis(200), stream.next()).await;
    assert!(
        result.is_err(),
        "unsubscribed agent must NOT receive a FocusGainedEvent batch"
    );
}

/// AC 4: focus events are routed only to the owning namespace.
///
/// A second agent subscribed to FOCUS_EVENTS under a different namespace must
/// not receive a batch intended for the first agent's namespace.
#[tokio::test]
async fn focus_event_routed_to_owning_namespace_only() {
    let (mut client, _server, input_event_tx) = start_server().await;

    // Connect two agents, both subscribed to FOCUS_EVENTS.
    let (_tx_a, mut stream_a) = perform_handshake_with_focus(&mut client, "focus-owner").await;
    let (_tx_b, mut stream_b) = perform_handshake_with_focus(&mut client, "other-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = focus_gained_batch(tile_id, node_id);

    // Inject only for "focus-owner".
    let _ = input_event_tx.send(("focus-owner".to_string(), batch));

    // focus-owner must receive the event.
    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream_a.next())
        .await
        .expect("timed out waiting for FocusGainedEvent on focus-owner")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1);
            assert!(
                matches!(&batch.events[0].event, Some(InputEvent::FocusGained(_))),
                "focus-owner must receive FocusGainedEvent"
            );
        }
        other => panic!("focus-owner: expected EventBatch, got: {other:?}"),
    }

    // other-agent must NOT receive the event (namespace mismatch).
    let result =
        tokio::time::timeout(tokio::time::Duration::from_millis(200), stream_b.next()).await;
    assert!(
        result.is_err(),
        "other-agent must not receive focus event intended for focus-owner"
    );
}
