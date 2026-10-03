//! Event delivery over the session stream: an `EventBatch` injected on
//! `input_event_tx` reaches exactly the agent that owns the namespace and has
//! subscribed to the event's category (`INPUT_EVENTS` or `FOCUS_EVENTS`), with
//! every field intact.
//!
//! One row per event kind in [`cases`]; three behaviors checked for each row:
//! delivered intact, withheld from an unsubscribed agent, withheld from another
//! namespace. Headless: no display server or GPU. The windowed runtime's
//! producers (`dispatch_*_event`) are covered in `tze_hud_runtime`/`tze_hud_input`.

use std::time::Duration;

use tokio_stream::StreamExt;
use tze_hud_protocol::proto::input_envelope::Event as InputEvent;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{ClientMessage, Heartbeat, ServerMessage, SessionInit};
use tze_hud_protocol::proto::{
    CaptureReleasedEvent, CaptureReleasedReason, CharacterEvent, ClickEvent, CommandAction,
    CommandInputEvent, CommandSource, EventBatch, FocusGainedEvent, FocusLostEvent,
    FocusLostReason, FocusSource, InputEnvelope, KeyDownEvent, KeyUpEvent, PointerDownEvent,
    PointerMoveEvent, PointerUpEvent, ScrollOffsetChangedEvent,
};
use tze_hud_protocol::session_server::{HudSessionImpl, InputEventSender};
use tze_hud_scene::graph::SceneGraph;

const INPUT: &str = "INPUT_EVENTS";
const FOCUS: &str = "FOCUS_EVENTS";

type Client = HudSessionClient<tonic::transport::Channel>;
type Stream = tonic::Streaming<ServerMessage>;
type Sender = tokio::sync::mpsc::Sender<ClientMessage>;

// ── Event table ───────────────────────────────────────────────────────────────

/// How a row fills the event's `node_id` field.
#[derive(Clone, Copy)]
enum Node {
    /// A fresh 16-byte UUID.
    Id,
    /// Empty bytes: the wire encoding of "tile-level, no specific node".
    Absent,
}

struct Case {
    name: &'static str,
    /// Subscription category that gates this event.
    category: &'static str,
    node: Node,
    build: fn(tile_id: Vec<u8>, node_id: Vec<u8>) -> InputEvent,
}

fn cases() -> Vec<Case> {
    let c = |name, category, build| Case {
        name,
        category,
        node: Node::Id,
        build,
    };
    vec![
        c("pointer_down", INPUT, pointer_down),
        // Tile-level hit: node_id stays empty bytes, not 16 zero bytes.
        Case {
            name: "pointer_down_tile_level_hit",
            category: INPUT,
            node: Node::Absent,
            build: pointer_down,
        },
        c("pointer_move", INPUT, |tile_id, node_id| {
            InputEvent::PointerMove(PointerMoveEvent {
                tile_id,
                node_id,
                interaction_id: "test-region".into(),
                timestamp_mono_us: 2_000,
                device_id: "0".into(),
                local_x: 15.0,
                local_y: 25.0,
                display_x: 115.0,
                display_y: 225.0,
            })
        }),
        c("pointer_up", INPUT, |tile_id, node_id| {
            InputEvent::PointerUp(PointerUpEvent {
                tile_id,
                node_id,
                interaction_id: "test-region".into(),
                timestamp_mono_us: 3_000,
                device_id: "0".into(),
                local_x: 15.0,
                local_y: 25.0,
                display_x: 115.0,
                display_y: 225.0,
                button: 0,
            })
        }),
        c("click_refresh_button", INPUT, |tile_id, node_id| {
            InputEvent::Click(ClickEvent {
                tile_id,
                node_id,
                interaction_id: "refresh-button".into(),
                timestamp_mono_us: 0,
                device_id: "pointer-0".into(),
                local_x: 104.0,
                local_y: 274.0,
                button: 0,
            })
        }),
        c("click_dismiss_button", INPUT, |tile_id, node_id| {
            InputEvent::Click(ClickEvent {
                tile_id,
                node_id,
                interaction_id: "dismiss-button".into(),
                timestamp_mono_us: 0,
                device_id: "pointer-0".into(),
                local_x: 296.0,
                local_y: 274.0,
                button: 0,
            })
        }),
        // Pointer-free activation (Enter on a focused button).
        c("command_activate_dismiss", INPUT, |tile_id, node_id| {
            InputEvent::CommandInput(CommandInputEvent {
                tile_id,
                node_id,
                interaction_id: "dismiss-button".into(),
                timestamp_mono_us: 0,
                device_id: "keyboard-0".into(),
                action: CommandAction::Activate as i32,
                source: CommandSource::Keyboard as i32,
            })
        }),
        c("command_activate_refresh", INPUT, |tile_id, node_id| {
            InputEvent::CommandInput(CommandInputEvent {
                tile_id,
                node_id,
                interaction_id: "refresh-button".into(),
                timestamp_mono_us: 0,
                device_id: "keyboard-0".into(),
                action: CommandAction::Activate as i32,
                source: CommandSource::Keyboard as i32,
            })
        }),
        c("key_down", INPUT, |tile_id, node_id| {
            InputEvent::KeyDown(KeyDownEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 1_000,
                key_code: "KeyA".into(),
                key: "a".into(),
                repeat: false,
                ctrl: false,
                shift: false,
                alt: false,
                meta: false,
            })
        }),
        c("key_up", INPUT, |tile_id, node_id| {
            InputEvent::KeyUp(KeyUpEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 2_000,
                key_code: "KeyA".into(),
                key: "a".into(),
                ctrl: false,
                shift: false,
                alt: false,
                meta: false,
            })
        }),
        c("character", INPUT, |tile_id, node_id| {
            InputEvent::Character(CharacterEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 3_000,
                character: "a".into(),
            })
        }),
        // Wheel scroll; scroll events carry no node_id.
        c("scroll_wheel", INPUT, |tile_id, _| scroll(tile_id, 120.0)),
        // Keyboard PgDn scroll (160px page).
        c("scroll_keyboard", INPUT, |tile_id, _| {
            scroll(tile_id, 160.0)
        }),
        c("focus_gained", FOCUS, |tile_id, node_id| {
            InputEvent::FocusGained(FocusGainedEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 1_000,
                source: FocusSource::Click as i32,
            })
        }),
        c("focus_lost", FOCUS, |tile_id, node_id| {
            InputEvent::FocusLost(FocusLostEvent {
                tile_id,
                node_id,
                timestamp_mono_us: 2_000,
                reason: FocusLostReason::ClickElsewhere as i32,
            })
        }),
        c("capture_released_pointer_up", FOCUS, |tile_id, node_id| {
            capture_released(tile_id, node_id, CaptureReleasedReason::PointerUp)
        }),
        c("capture_released_by_agent", FOCUS, |tile_id, node_id| {
            capture_released(tile_id, node_id, CaptureReleasedReason::AgentReleased)
        }),
    ]
}

fn pointer_down(tile_id: Vec<u8>, node_id: Vec<u8>) -> InputEvent {
    InputEvent::PointerDown(PointerDownEvent {
        tile_id,
        node_id,
        interaction_id: "test-region".into(),
        timestamp_mono_us: 1_000,
        device_id: "0".into(),
        local_x: 10.0,
        local_y: 20.0,
        display_x: 110.0,
        display_y: 220.0,
        button: 0,
    })
}

fn scroll(tile_id: Vec<u8>, offset_y: f32) -> InputEvent {
    InputEvent::ScrollOffsetChanged(ScrollOffsetChangedEvent {
        tile_id,
        timestamp_mono_us: 0,
        offset_x: 0.0,
        offset_y,
    })
}

fn capture_released(
    tile_id: Vec<u8>,
    node_id: Vec<u8>,
    reason: CaptureReleasedReason,
) -> InputEvent {
    InputEvent::CaptureReleased(CaptureReleasedEvent {
        tile_id,
        node_id,
        timestamp_mono_us: 3_000,
        device_id: "device-0".into(),
        reason: reason as i32,
    })
}

fn uuid_bytes() -> Vec<u8> {
    uuid::Uuid::now_v7().as_bytes().to_vec()
}

/// Build the batch a case injects; one event, fresh ids.
fn batch_for(case: &Case) -> EventBatch {
    let node_id = match case.node {
        Node::Id => uuid_bytes(),
        Node::Absent => Vec::new(),
    };
    EventBatch {
        frame_number: 0,
        batch_ts_us: now_wall_us(),
        events: vec![InputEnvelope {
            event: Some((case.build)(uuid_bytes(), node_id)),
        }],
    }
}

// ── Server and session helpers ────────────────────────────────────────────────

fn now_wall_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// In-process session server on an ephemeral loopback port.
async fn start_server() -> (Client, tokio::task::JoinHandle<()>, InputEventSender) {
    let service = HudSessionImpl::new(SceneGraph::new(1920.0, 1080.0), "test-psk");
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
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();
    (client, handle, input_event_tx)
}

/// Handshake subscribing to `subscriptions`; returns the stream positioned after
/// `SessionEstablished`, `SceneSnapshot`, and the initial `DegradationNotice`.
async fn connect(client: &mut Client, agent_id: &str, subscriptions: &[&str]) -> (Sender, Stream) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: agent_id.to_string(),
            initial_subscriptions: subscriptions.iter().map(|s| s.to_string()).collect(),
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
    let mut stream = client
        .session(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    for _ in 0..3 {
        let _ = stream.next().await;
    }
    (tx, stream)
}

/// Assert that nothing has been delivered to `stream`: a heartbeat sent after the
/// injection must be the next message, then the stream stays quiet.
async fn assert_nothing_delivered(tx: &Sender, stream: &mut Stream, who: &str) {
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 1,
        })),
    })
    .await
    .unwrap();
    let msg = tokio::time::timeout(Duration::from_millis(500), stream.next())
        .await
        .unwrap_or_else(|_| panic!("{who}: no heartbeat echo"))
        .unwrap()
        .unwrap();
    assert!(
        matches!(msg.payload, Some(ServerPayload::Heartbeat(_))),
        "{who}: must not receive the batch, got {:?}",
        msg.payload
    );
    let quiet = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
    assert!(quiet.is_err(), "{who}: unexpected late message {quiet:?}");
}

/// Next `EventBatch` on `stream`, within 500ms.
async fn next_batch(stream: &mut Stream, who: &str) -> EventBatch {
    let msg = tokio::time::timeout(Duration::from_millis(500), stream.next())
        .await
        .unwrap_or_else(|_| panic!("{who}: timed out waiting for the event"))
        .unwrap()
        .unwrap();
    match msg.payload {
        Some(ServerPayload::EventBatch(b)) => b,
        other => panic!("{who}: expected EventBatch, got {other:?}"),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// A subscribed agent receives the injected event with every field (including
/// the empty-vs-16-byte `node_id` encoding) unchanged.
#[tokio::test]
async fn subscribed_agent_receives_event_intact() {
    let (mut client, _server, input_event_tx) = start_server().await;
    for case in cases() {
        let agent = format!("deliver-{}", case.name);
        let (_tx, mut stream) = connect(&mut client, &agent, &[case.category]).await;
        let sent = batch_for(&case);
        let _ = input_event_tx.send((agent.clone(), sent.clone()));
        let got = next_batch(&mut stream, case.name).await;
        assert_eq!(
            got.events, sent.events,
            "{}: event must round-trip",
            case.name
        );
    }
}

/// An agent without the event's category subscription receives nothing.
#[tokio::test]
async fn unsubscribed_agent_receives_nothing() {
    let (mut client, _server, input_event_tx) = start_server().await;
    for case in cases() {
        let agent = format!("unsubscribed-{}", case.name);
        let (tx, mut stream) = connect(&mut client, &agent, &[]).await;
        let _ = input_event_tx.send((agent.clone(), batch_for(&case)));
        assert_nothing_delivered(&tx, &mut stream, case.name).await;
    }
}

/// A batch addressed to one namespace reaches that agent only, even when another
/// agent holds the same subscription.
#[tokio::test]
async fn event_reaches_owning_namespace_only() {
    let (mut client, _server, input_event_tx) = start_server().await;
    for case in cases() {
        let owner = format!("owner-{}", case.name);
        let other = format!("other-{}", case.name);
        let (_tx_a, mut owner_stream) = connect(&mut client, &owner, &[case.category]).await;
        let (tx_b, mut other_stream) = connect(&mut client, &other, &[case.category]).await;
        let sent = batch_for(&case);
        let _ = input_event_tx.send((owner, sent.clone()));
        let got = next_batch(&mut owner_stream, case.name).await;
        assert_eq!(
            got.events, sent.events,
            "{}: owner must get the event",
            case.name
        );
        assert_nothing_delivered(&tx_b, &mut other_stream, &other).await;
    }
}
