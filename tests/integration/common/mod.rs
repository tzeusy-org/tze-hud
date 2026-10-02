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

/// A live gRPC session with an established lease.
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

/// Connect an agent via gRPC, complete the handshake, and acquire a lease.
///
/// Parameters:
/// - `psk`: pre-shared key that the runtime was started with.
/// - `port`: gRPC port the runtime is listening on.
/// - `agent_id`: unique agent identifier string.
/// - `display_name_suffix`: appended to `"{agent_id} ({suffix})"` in SessionInit.
///
/// The returned [`AgentSession`] has `sequence` pre-set to `2` (the sequence
/// of the LeaseRequest) so the next `next_seq()` call returns `3`.
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

    // Request lease
    tx.send(session_proto::ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(session_proto::client_message::Payload::LeaseRequest(
            session_proto::LeaseRequest { ttl_ms: 120_000 },
        )),
    })
    .await?;

    // Wrap the stream in a temporary AgentSession so we can use next_server_msg.
    let mut partial_session = AgentSession {
        namespace,
        lease_id_bytes: vec![],
        tx: tx.clone(),
        rx: response_stream,
        sequence: 2,
    };

    // Read LeaseResponse.
    let msg = partial_session
        .next_server_msg()
        .await
        .ok_or("no lease response")??;
    let (lease_id_bytes, response_stream) = match &msg.payload {
        Some(session_proto::server_message::Payload::LeaseResponse(resp)) if resp.granted => {
            (resp.lease_id.clone(), partial_session.rx)
        }
        other => {
            return Err(format!(
                "agent {agent_id}: Expected LeaseResponse(granted), got: {other:?}"
            )
            .into());
        }
    };

    Ok(AgentSession {
        namespace: partial_session.namespace,
        lease_id_bytes,
        tx,
        rx: response_stream,
        sequence: 2,
    })
}

// ─── Tile mutations ──────────────────────────────────────────────────────────

/// Send a `CreateTile` mutation and return the created tile ID bytes.
///
/// Returns an error if the server rejects the mutation or if the accepted
/// `MutationResult` does not include a `created_id`. The latter case indicates
/// a server bug (accepted but did not return the ID); surfacing it as an error
/// prevents tests from silently operating on an empty tile ID.
pub async fn create_tile_via_grpc(
    session: &mut AgentSession,
    bounds: [f32; 4],
    z_order: u32,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let batch_id: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    let seq = session.next_seq();

    session
        .tx
        .send(session_proto::ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(session_proto::client_message::Payload::MutationBatch(
                session_proto::MutationBatch {
                    batch_id,
                    lease_id: session.lease_id_bytes.clone(),
                    mutations: vec![proto::MutationProto {
                        mutation: Some(proto::mutation_proto::Mutation::CreateTile(
                            proto::CreateTileMutation {
                                tab_id: vec![], // empty = server infers active tab
                                bounds: Some(proto::Rect {
                                    x: bounds[0],
                                    y: bounds[1],
                                    width: bounds[2],
                                    height: bounds[3],
                                }),
                                z_order,
                            },
                        )),
                    }],
                    timing: None,
                },
            )),
        })
        .await?;

    // Read MutationResult.
    let msg = session
        .next_server_msg()
        .await
        .ok_or("no mutation result")??;
    match &msg.payload {
        Some(session_proto::server_message::Payload::MutationResult(result)) if result.accepted => {
            let tile_id =
                result.created_ids.first().cloned().ok_or_else(|| {
                    "Server accepted mutation but returned no created ID".to_string()
                })?;
            Ok(tile_id)
        }
        Some(session_proto::server_message::Payload::MutationResult(result)) => Err(format!(
            "CreateTile rejected: {} — {}",
            result.error_code, result.error_message
        )
        .into()),
        other => Err(format!("Expected MutationResult, got: {other:?}").into()),
    }
}

// ─── Zone publish helpers ────────────────────────────────────────────────────

/// Low-level zone publish via a `ZonePublish` session message.
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
            payload: Some(session_proto::client_message::Payload::ZonePublish(
                session_proto::ZonePublish {
                    zone_name: zone_name.to_string(),
                    content: Some(content),
                    ttl_us: 0,
                    element_id: Vec::new(),
                    merge_key: String::new(),
                    breakpoints: Vec::new(),
                    // Snapshot parity fields (WM-S2b session.proto delta §fields 7-9); 0/empty = no constraint.
                    present_at_wall_us: 0,
                    expires_at_wall_us: 0,
                    content_classification: String::new(),
                },
            )),
        })
        .await?;

    // Read ZonePublishResult.
    let msg = session
        .next_server_msg()
        .await
        .ok_or("no zone publish result")??;
    match &msg.payload {
        Some(session_proto::server_message::Payload::ZonePublishResult(result))
            if result.accepted =>
        {
            Ok(())
        }
        Some(session_proto::server_message::Payload::ZonePublishResult(result)) => Err(format!(
            "ZonePublish to '{}' rejected: {} — {}",
            zone_name, result.error_code, result.error_message
        )
        .into()),
        other => Err(format!("Expected ZonePublishResult, got: {other:?}").into()),
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
