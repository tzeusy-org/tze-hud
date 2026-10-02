//! Integration test: keyboard events reach agents via KeyDownEvent / KeyUpEvent /
//! CharacterEvent over the `INPUT_EVENTS` gRPC channel.
//!
//! Verifies acceptance criteria for hud-rpcr1:
//!
//! 1. `KeyDownEvent` injected on `input_event_tx` is delivered to an agent
//!    subscribed to `INPUT_EVENTS` with the correct fields.
//! 2. `KeyUpEvent` is delivered similarly.
//! 3. `CharacterEvent` is delivered similarly.
//! 4. Agents NOT subscribed to `INPUT_EVENTS` do NOT receive keyboard events.
//! 5. Keyboard events are routed only to the owning namespace.
//!
//! All tests are headless Layer 0: no display server, GPU, or windowed runtime
//! required.  The end-to-end path exercised is:
//!
//!   `input_event_tx.send((namespace, EventBatch))` →
//!   session handler filters by `INPUT_EVENTS` subscription →
//!   agent gRPC stream receives `ServerMessage::EventBatch` →
//!   `EventBatch.events[0]` == `InputEnvelope::{KeyDown | KeyUp | Character}`
//!
//! The `KeyboardProcessor` → `dispatch_keyboard_event` wiring (windowed runtime)
//! is covered by the `text_stream_portal_surface` integration tests which drive
//! the keyboard processor directly at the `tze_hud_input` level.

use tokio_stream::StreamExt;
use tze_hud_protocol::proto::input_envelope::Event as InputEvent;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{ClientMessage, ServerMessage, SessionInit};
use tze_hud_protocol::proto::{
    CharacterEvent, EventBatch, InputEnvelope, KeyDownEvent, KeyUpEvent,
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
            requested_capabilities: vec!["access_input_events".to_string()],
            initial_subscriptions: vec!["INPUT_EVENTS".to_string()],
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
            // No capabilities → no input events.
            requested_capabilities: vec![],
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

fn key_down_batch(tile_id: Vec<u8>, node_id: Vec<u8>, key_code: &str, key: &str) -> EventBatch {
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some(InputEvent::KeyDown(KeyDownEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 1_000,
                key_code: key_code.to_string(),
                key: key.to_string(),
                repeat: false,
                ctrl: false,
                shift: false,
                alt: false,
                meta: false,
            })),
        }],
    }
}

fn key_up_batch(tile_id: Vec<u8>, node_id: Vec<u8>, key_code: &str, key: &str) -> EventBatch {
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some(InputEvent::KeyUp(KeyUpEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 2_000,
                key_code: key_code.to_string(),
                key: key.to_string(),
                ctrl: false,
                shift: false,
                alt: false,
                meta: false,
            })),
        }],
    }
}

fn character_batch(tile_id: Vec<u8>, node_id: Vec<u8>, character: &str) -> EventBatch {
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some(InputEvent::Character(CharacterEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 3_000,
                character: character.to_string(),
            })),
        }],
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC 1: `KeyDownEvent` injected on `input_event_tx` is delivered to a
/// subscribed agent with the correct fields preserved end-to-end.
#[tokio::test]
async fn key_down_event_delivered_to_subscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_with_input(&mut client, "key-down-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = key_down_batch(tile_id.clone(), node_id.clone(), "KeyA", "a");
    let _ = input_event_tx.send(("key-down-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for KeyDownEvent")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "batch must contain exactly 1 event");
            match &batch.events[0].event {
                Some(InputEvent::KeyDown(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert_eq!(ev.node_id, node_id, "node_id must round-trip");
                    assert_eq!(ev.key_code, "KeyA", "key_code must round-trip");
                    assert_eq!(ev.key, "a", "key must round-trip");
                    assert_eq!(ev.timestamp_mono_us, 1_000, "timestamp must round-trip");
                    assert!(!ev.repeat, "repeat flag must be false");
                    assert!(!ev.ctrl, "ctrl flag must be false");
                }
                other => panic!("expected KeyDown, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 2: `KeyUpEvent` is delivered to a subscribed agent.
#[tokio::test]
async fn key_up_event_delivered_to_subscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_with_input(&mut client, "key-up-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = key_up_batch(tile_id.clone(), node_id.clone(), "KeyA", "a");
    let _ = input_event_tx.send(("key-up-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for KeyUpEvent")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1);
            match &batch.events[0].event {
                Some(InputEvent::KeyUp(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert_eq!(ev.node_id, node_id, "node_id must round-trip");
                    assert_eq!(ev.key_code, "KeyA");
                    assert_eq!(ev.key, "a");
                    assert_eq!(ev.timestamp_mono_us, 2_000);
                }
                other => panic!("expected KeyUp, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 3: `CharacterEvent` is delivered to a subscribed agent.
#[tokio::test]
async fn character_event_delivered_to_subscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_with_input(&mut client, "character-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = character_batch(tile_id.clone(), node_id.clone(), "a");
    let _ = input_event_tx.send(("character-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for CharacterEvent")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1);
            match &batch.events[0].event {
                Some(InputEvent::Character(ev)) => {
                    assert_eq!(ev.tile_id, tile_id, "tile_id must round-trip");
                    assert_eq!(ev.node_id, node_id, "node_id must round-trip");
                    assert_eq!(ev.character, "a", "character must round-trip");
                    assert_eq!(ev.timestamp_mono_us, 3_000);
                }
                other => panic!("expected Character, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC 4: agents NOT subscribed to `INPUT_EVENTS` must NOT receive keyboard events.
///
/// `filter_event_batch` drops batches whose events do not match the agent's
/// active subscriptions.  This test verifies the gate is enforced end-to-end
/// for keyboard events (same mechanism as scroll events).
#[tokio::test]
async fn key_down_not_delivered_to_unsubscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) =
        perform_handshake_no_input(&mut client, "no-input-keyboard-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = key_down_batch(tile_id, node_id, "KeyA", "a");
    let _ = input_event_tx.send(("no-input-keyboard-agent".to_string(), batch));

    // Give the server enough time to process and (not) deliver the event.
    let result = tokio::time::timeout(tokio::time::Duration::from_millis(200), stream.next()).await;
    assert!(
        result.is_err(),
        "unsubscribed agent must NOT receive a KeyDownEvent batch"
    );
}

/// AC 5: keyboard events are routed only to the owning namespace.
///
/// A second agent subscribed to INPUT_EVENTS under a different namespace must
/// not receive a batch intended for the first agent's namespace.
#[tokio::test]
async fn key_down_routed_to_owning_namespace_only() {
    let (mut client, _server, input_event_tx) = start_server().await;

    // Connect two agents, both subscribed to INPUT_EVENTS.
    let (_tx_a, mut stream_a) = perform_handshake_with_input(&mut client, "composer-owner").await;
    let (_tx_b, mut stream_b) = perform_handshake_with_input(&mut client, "other-agent").await;

    let tile_id = tile_id_fixture();
    let node_id = tile_id_fixture();
    let batch = key_down_batch(tile_id, node_id, "KeyH", "h");

    // Inject only for "composer-owner".
    let _ = input_event_tx.send(("composer-owner".to_string(), batch));

    // composer-owner must receive the event.
    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream_a.next())
        .await
        .expect("timed out waiting for KeyDownEvent on composer-owner")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1);
            assert!(
                matches!(&batch.events[0].event, Some(InputEvent::KeyDown(_))),
                "composer-owner must receive KeyDownEvent"
            );
        }
        other => panic!("composer-owner: expected EventBatch, got: {other:?}"),
    }

    // other-agent must NOT receive the event (namespace mismatch).
    let result =
        tokio::time::timeout(tokio::time::Duration::from_millis(200), stream_b.next()).await;
    assert!(
        result.is_err(),
        "other-agent must not receive keyboard event intended for composer-owner"
    );
}

// Note: The no-focus invariant ("KeyboardProcessor returns None for FocusOwner::None,
// so dispatch_keyboard_event is never called without a focused session") is already
// verified by `keyboard_processor_no_dispatch_without_focus` in
// `tests/integration/text_stream_portal_surface.rs` (added in hud-opkvq / PR #601).
// It is not duplicated here to keep the protocol-layer test focused on the gRPC
// delivery path (broadcast channel → session handler → agent stream).
