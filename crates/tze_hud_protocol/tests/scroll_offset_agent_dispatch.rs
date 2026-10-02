//! Integration test: scroll events reach agents via ScrollOffsetChangedEvent.
//!
//! Verifies acceptance criteria for hud-8lpu:
//!
//! 1. Wheel scroll → agent receives ScrollOffsetChangedEvent via input_event_tx.
//! 2. Keyboard scroll → agent receives ScrollOffsetChangedEvent via input_event_tx.
//! 3. Agents NOT subscribed to INPUT_EVENTS do NOT receive the event.
//! 4. ScrollOffsetChangedEvent is routed only to the owning namespace.
//!
//! All tests are headless Layer 0: no display server or GPU required.
//! The end-to-end path exercised is:
//!   input_event_tx.send((namespace, EventBatch)) →
//!   session handler filters by INPUT_EVENTS subscription →
//!   agent gRPC stream receives ServerMessage::EventBatch →
//!   EventBatch.events[0] == InputEnvelope::ScrollOffsetChanged
//!
//! Local-first scroll behavior (AC §4) is tested in `tze_hud_input` (see
//! `crates/tze_hud_input/src/lib.rs::test_scroll_local_update_before_agent_event`).

use tokio_stream::StreamExt;
use tze_hud_protocol::proto::input_envelope::Event as InputEvent;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{ClientMessage, ServerMessage, SessionInit};
use tze_hud_protocol::proto::{EventBatch, InputEnvelope, ScrollOffsetChangedEvent};
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

/// Handshake WITHOUT input event access (no `access_input_events` capability).
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

/// Build a synthetic `EventBatch` containing a single `ScrollOffsetChangedEvent`.
fn scroll_event_batch(tile_id: Vec<u8>, offset_x: f32, offset_y: f32) -> EventBatch {
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some(InputEvent::ScrollOffsetChanged(ScrollOffsetChangedEvent {
                tile_id,
                timestamp_mono_us: 0,
                offset_x,
                offset_y,
            })),
        }],
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC §1 (wheel): agent subscribed to INPUT_EVENTS receives ScrollOffsetChangedEvent
/// injected via the input_event_tx broadcast channel (mirrors the windowed-runtime
/// `dispatch_scroll_offset_event` helper path for wheel scroll).
#[tokio::test]
async fn wheel_scroll_delivers_scroll_offset_changed_event_to_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_with_input(&mut client, "scroll-wheel-agent").await;

    let tile_id_bytes: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    let batch = scroll_event_batch(tile_id_bytes.clone(), 0.0, 120.0);
    let _ = input_event_tx.send(("scroll-wheel-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for EventBatch")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "batch must contain exactly 1 event");
            match &batch.events[0].event {
                Some(InputEvent::ScrollOffsetChanged(ev)) => {
                    assert_eq!(ev.tile_id, tile_id_bytes, "tile_id must match");
                    assert!(
                        (ev.offset_y - 120.0).abs() < 1e-4,
                        "offset_y must be 120.0, got {}",
                        ev.offset_y
                    );
                    assert!(
                        ev.offset_x.abs() < 1e-4,
                        "offset_x must be 0.0, got {}",
                        ev.offset_x
                    );
                }
                other => panic!("expected ScrollOffsetChanged, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC §1 (keyboard): keyboard scroll also produces a ScrollOffsetChangedEvent
/// that reaches the agent.  Same delivery path — only the producer differs.
/// Simulates the PgDn keyboard scroll (KEYBOARD_PAGE_SCROLL_PX = 160px).
#[tokio::test]
async fn keyboard_scroll_delivers_scroll_offset_changed_event_to_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) =
        perform_handshake_with_input(&mut client, "scroll-keyboard-agent").await;

    let tile_id_bytes: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    // Simulate the PgDn keyboard scroll delta (KEYBOARD_PAGE_SCROLL_PX = 160.0).
    let batch = scroll_event_batch(tile_id_bytes.clone(), 0.0, 160.0);
    let _ = input_event_tx.send(("scroll-keyboard-agent".to_string(), batch));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for EventBatch")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1);
            match &batch.events[0].event {
                Some(InputEvent::ScrollOffsetChanged(ev)) => {
                    assert_eq!(ev.tile_id, tile_id_bytes);
                    assert!(
                        (ev.offset_y - 160.0).abs() < 1e-4,
                        "PgDn scroll offset_y must be 160.0, got {}",
                        ev.offset_y
                    );
                }
                other => panic!("expected ScrollOffsetChanged, got: {other:?}"),
            }
        }
        other => panic!("expected EventBatch, got: {other:?}"),
    }
}

/// AC §3: agents not subscribed to INPUT_EVENTS must NOT receive the event.
///
/// `filter_event_batch` drops batches whose events do not match the agent's
/// active subscriptions.  This test verifies the gate is enforced end-to-end.
#[tokio::test]
async fn scroll_event_not_delivered_to_unsubscribed_agent() {
    let (mut client, _server, input_event_tx) = start_server().await;
    let (_tx, mut stream) = perform_handshake_no_input(&mut client, "no-input-scroll-agent").await;

    let tile_id_bytes: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    let batch = scroll_event_batch(tile_id_bytes, 0.0, 50.0);
    let _ = input_event_tx.send(("no-input-scroll-agent".to_string(), batch));

    // Give the server enough time to process and (not) deliver the event.
    let result = tokio::time::timeout(tokio::time::Duration::from_millis(200), stream.next()).await;
    assert!(
        result.is_err(),
        "unsubscribed agent must NOT receive a scroll event batch"
    );
}

/// AC §1 (namespace routing): the event is delivered only to the namespace
/// that owns the tile.  A second agent connected under a different namespace
/// must not receive the batch even though both are subscribed to INPUT_EVENTS.
#[tokio::test]
async fn scroll_event_routed_to_owning_namespace_only() {
    let (mut client, _server, input_event_tx) = start_server().await;

    // Connect two agents, both subscribed to INPUT_EVENTS.
    let (_tx_a, mut stream_a) = perform_handshake_with_input(&mut client, "owner-agent").await;
    let (_tx_b, mut stream_b) = perform_handshake_with_input(&mut client, "other-agent").await;

    let tile_id_bytes: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    let batch = scroll_event_batch(tile_id_bytes.clone(), 0.0, 30.0);

    // Inject for "owner-agent" only.
    let _ = input_event_tx.send(("owner-agent".to_string(), batch));

    // owner-agent must receive the event.
    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream_a.next())
        .await
        .expect("timed out waiting for EventBatch on owner-agent")
        .unwrap()
        .unwrap();

    match msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1);
            assert!(
                matches!(
                    &batch.events[0].event,
                    Some(InputEvent::ScrollOffsetChanged(_))
                ),
                "owner-agent must receive ScrollOffsetChanged"
            );
        }
        other => panic!("owner-agent: expected EventBatch, got: {other:?}"),
    }

    // other-agent must NOT receive the event (namespace mismatch).
    let result =
        tokio::time::timeout(tokio::time::Duration::from_millis(200), stream_b.next()).await;
    assert!(
        result.is_err(),
        "other-agent must not receive scroll event intended for owner-agent"
    );
}
