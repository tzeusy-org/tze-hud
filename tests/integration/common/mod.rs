//! Shared gRPC session test harness for integration tests.
//!
//! Include this module in each integration test file with:
//! ```rust,ignore
//! #[path = "common/mod.rs"]
//! mod common;
//! use common::*;
//! ```
//!
//! ## Design
//!
//! Shared gRPC session helpers for the multi-agent integration suites
//! (`multi_agent.rs`, `presence_card_coexistence.rs`, `subtitle_streaming.rs`).
//! PSK and port are parameters so the helpers stay test-agnostic.

#![allow(dead_code)] // Items are selectively used across the test binaries.

use tokio_stream::StreamExt;
use tze_hud_protocol::auth::{RUNTIME_MAX_VERSION, RUNTIME_MIN_VERSION};
use tze_hud_protocol::proto;
use tze_hud_protocol::proto::session as session_proto;
#[allow(deprecated)]
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;

// ─── Timestamp ───────────────────────────────────────────────────────────────

/// Current wall-clock time in microseconds since UNIX epoch.
pub fn now_wall_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

// ─── Agent session ───────────────────────────────────────────────────────────

/// A live gRPC session; `lease_id_bytes` is set by the first tile claim.
///
/// Created by [`connect_agent`]. Fields are public so that test files that need
/// direct access (e.g., to inspect `namespace` or `lease_id_bytes`) can read them.
pub struct AgentSession {
    pub namespace: String,
    pub lease_id_bytes: Vec<u8>,
    pub tx: tokio::sync::mpsc::Sender<session_proto::ClientMessage>,
    pub rx: tonic::codec::Streaming<session_proto::ServerMessage>,
    pub sequence: u64,
}

impl AgentSession {
    /// Increment and return the next sequence number.
    pub fn next_seq(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }

    /// Receive the next server message.
    ///
    /// Returns `None` if the stream has ended, or `Some(Ok(msg))` / `Some(Err(…))`
    /// otherwise. Callers typically chain `.ok_or("…")?` to convert to a `Result`.
    pub async fn next_server_msg(
        &mut self,
    ) -> Option<Result<session_proto::ServerMessage, tonic::Status>> {
        self.rx.next().await
    }
}

// ─── Session establishment ───────────────────────────────────────────────────

/// Connect an agent via gRPC and complete the handshake.
///
/// Parameters:
/// - `psk`: pre-shared key that the runtime was started with.
/// - `port`: gRPC port the runtime is listening on.
/// - `agent_id`: unique agent identifier string.
/// - `display_name_suffix`: appended to `"{agent_id} ({suffix})"` in SessionInit.
///
/// The returned [`AgentSession`] has `sequence` pre-set to `1` (the
/// SessionInit) so the next `next_seq()` call returns `2`.
pub async fn connect_agent(
    psk: &str,
    port: u16,
    agent_id: &str,
) -> Result<AgentSession, Box<dyn std::error::Error>> {
    let mut client = HudSessionClient::connect(format!("http://[::1]:{port}")).await?;

    let (tx, rx_chan) = tokio::sync::mpsc::channel::<session_proto::ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx_chan);

    let now_us = now_wall_us();

    // Send SessionInit
    tx.send(session_proto::ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_us,
        payload: Some(session_proto::client_message::Payload::SessionInit(
            session_proto::SessionInit {
                agent_id: agent_id.to_string(),
                initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
                resume_token: Vec::new(),
                min_protocol_version: RUNTIME_MIN_VERSION,
                max_protocol_version: RUNTIME_MAX_VERSION,
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(psk.to_string())),
            },
        )),
    })
    .await?;

    let mut response_stream = client.session(stream).await?.into_inner();

    // Read SessionEstablished
    let msg = response_stream
        .next()
        .await
        .ok_or("no message received")??;
    let namespace = match &msg.payload {
        Some(session_proto::server_message::Payload::SessionEstablished(est)) => {
            est.namespace.clone()
        }
        other => {
            return Err(
                format!("agent {agent_id}: Expected SessionEstablished, got: {other:?}").into(),
            );
        }
    };

    // Read SceneSnapshot followed by the mandatory current degradation state.
    let _msg = response_stream.next().await.ok_or("no scene snapshot")??;
    let msg = response_stream
        .next()
        .await
        .ok_or("no current degradation notice")??;
    if !matches!(
        &msg.payload,
        Some(session_proto::server_message::Payload::DegradationNotice(_))
    ) {
        return Err(format!(
            "agent {agent_id}: Expected DegradationNotice after SceneSnapshot, got: {:?}",
            msg.payload
        )
        .into());
    }

    Ok(AgentSession {
        namespace,
        lease_id_bytes: vec![],
        tx,
        rx: response_stream,
        sequence: 1,
    })
}

// ─── Tile mutations ──────────────────────────────────────────────────────────

pub use session_proto::{TileAnchor, TileSize};

/// Build a `TilePlacement` from an anchor and size class.
pub fn placement(anchor: TileAnchor, size: TileSize) -> session_proto::TilePlacement {
    session_proto::TilePlacement {
        anchor: anchor as i32,
        size: size as i32,
    }
}

/// Claim a tile with `ClaimTile` and return the created tile ID bytes.
///
/// The runtime resolves geometry from `placement`. The first claim's lease
/// becomes `session.lease_id_bytes`, which later mutation batches present.
pub async fn claim_tile_via_grpc(
    session: &mut AgentSession,
    placement: session_proto::TilePlacement,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let seq = session.next_seq();
    session
        .tx
        .send(session_proto::ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(session_proto::client_message::Payload::ClaimTile(
                session_proto::ClaimTile {
                    placement: Some(placement),
                    ttl_ms: 120_000,
                    root: None,
                },
            )),
        })
        .await?;

    let msg = session.next_server_msg().await.ok_or("no claim result")??;
    match &msg.payload {
        Some(session_proto::server_message::Payload::RequestResult(result)) if result.ok => {
            let tile_id = result
                .ids
                .first()
                .cloned()
                .ok_or("ClaimTile accepted but returned no tile id")?;
            if session.lease_id_bytes.is_empty() {
                session.lease_id_bytes = result.lease_id.clone();
            }
            Ok(tile_id)
        }
        Some(session_proto::server_message::Payload::RequestResult(result)) => {
            Err(format!("ClaimTile rejected: {} — {}", result.code, result.hint).into())
        }
        other => Err(format!("Expected RequestResult, got: {other:?}").into()),
    }
}

// ─── Zone publish helpers ────────────────────────────────────────────────────

/// Low-level zone publish via a `Publish` session message.
///
/// All higher-level zone helpers (`publish_stream_text_to_zone_via_grpc`,
/// `publish_notification_to_zone_via_grpc`) delegate to this function.
pub async fn publish_zone_content_via_grpc(
    session: &mut AgentSession,
    zone_name: &str,
    content: proto::ZoneContent,
) -> Result<(), Box<dyn std::error::Error>> {
    let seq = session.next_seq();

    session
        .tx
        .send(session_proto::ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(session_proto::client_message::Payload::Publish(
                session_proto::Publish {
                    surface: format!("zone:{zone_name}"),
                    content: Some(content),
                    ttl_ms: 0,
                    key: String::new(),
                    breakpoints: Vec::new(),
                    present_at_us: 0,
                    expires_at_us: 0,
                    ..Default::default()
                },
            )),
        })
        .await?;

    // Read RequestResult.
    let msg = session
        .next_server_msg()
        .await
        .ok_or("no zone publish result")??;
    match &msg.payload {
        Some(session_proto::server_message::Payload::RequestResult(result)) if result.ok => Ok(()),
        Some(session_proto::server_message::Payload::RequestResult(result)) => Err(format!(
            "zone Publish to '{}' rejected: {} — {}",
            zone_name, result.code, result.hint
        )
        .into()),
        other => Err(format!("Expected RequestResult, got: {other:?}").into()),
    }
}

/// Publish `StreamText` content to a zone (e.g., the subtitle zone).
pub async fn publish_stream_text_to_zone_via_grpc(
    session: &mut AgentSession,
    zone_name: &str,
    text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    publish_zone_content_via_grpc(
        session,
        zone_name,
        proto::ZoneContent {
            payload: Some(proto::zone_content::Payload::StreamText(text.to_string())),
        },
    )
    .await
}

/// Publish a `Notification` payload to a zone (e.g., the notification-area zone).
pub async fn publish_notification_to_zone_via_grpc(
    session: &mut AgentSession,
    zone_name: &str,
    text: &str,
    urgency: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    publish_zone_content_via_grpc(
        session,
        zone_name,
        proto::ZoneContent {
            payload: Some(proto::zone_content::Payload::Notification(
                proto::NotificationPayload {
                    text: text.to_string(),
                    icon: String::new(),
                    urgency,
                    title: String::new(),
                    actions: Vec::new(),
                },
            )),
        },
    )
    .await
}
