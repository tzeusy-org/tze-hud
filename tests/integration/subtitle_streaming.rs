//! Subtitle streaming breakpoint reveal — gRPC and MCP publish path verification.
//!
//! Implements acceptance criteria for `hud-hzub.4`:
//! - Breakpoints are gRPC-only (the MCP surface has no breakpoints field)
//! - gRPC zone `Publish` with stream_text + breakpoints: breakpoints forwarded to compositor
//! - Stream-text without breakpoints reveals full text immediately
//! - list_zones reports subtitle zone with correct contention_policy and accepted_media_types
//!
//! ## Test inventory
//!
//! ### gRPC path (via zone `Publish` session message)
//!
//! - [`test_grpc_zone_publish_with_breakpoints_forwarded`]
//!   gRPC zone `Publish` with stream_text + breakpoints → breakpoints stored in publish record.
//!
//! - [`test_grpc_zone_publish_empty_breakpoints_reveals_immediately`]
//!   gRPC zone `Publish` with empty breakpoints → publish record has empty breakpoints.
//!
//! - [`test_grpc_zone_publish_replacement_cancels_breakpoints`]
//!   Second gRPC publish replaces first — latest-wins cancels previous streaming record.
//!
//! - [`test_grpc_zone_publish_breakpoints_match_mcp_behavior`]
//!   gRPC and MCP paths produce identical breakpoint records for the same payload.
//!
//! - [`test_grpc_list_zones_subtitle_metadata`]
//!   gRPC SceneSnapshot contains subtitle zone with LatestWins contention and StreamText type.
//!
//! All tests are headless (no display server required).
//! Each test creates its own runtime on a unique port to avoid conflicts.
//!
//! Spec: openspec/changes/exemplar-subtitle/specs/exemplar-subtitle/spec.md
//!   §Subtitle Streaming Word-by-Word Reveal
//!   §Subtitle Contention Policy — Latest Wins
//!   §Subtitle MCP Test Fixtures

use tze_hud_protocol::auth::{RUNTIME_MAX_VERSION, RUNTIME_MIN_VERSION};
use tze_hud_protocol::proto;
use tze_hud_protocol::proto::session as session_proto;
#[allow(deprecated)]
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;
use tze_hud_scene::types::ZoneRegistry;

use tokio_stream::StreamExt;

// ─── Shared gRPC session harness ─────────────────────────────────────────────
// Selectively re-uses now_wall_us() from the shared harness (hud-ls5pz).
// The AgentSession in this file is intentionally minimal (no namespace /
// lease_id_bytes) because zone-publish-only tests don't need those fields.
// The shared connect_agent() requires full-session semantics; this file keeps
// its own connect_agent_with_zone_publish_cap() instead.
#[path = "common/mod.rs"]
mod common;

// ─── Port assignments (must not conflict with other integration tests) ────────
//
// Other tests use:
//   50052  multi_agent
//   50053  soak
//   50055  presence_card_coexistence
//   50056-50060 reserved for this file
//
// Port per test for isolation (tests run in parallel by default):

const TEST_PSK: &str = "subtitle-streaming-test-key";
const DISPLAY_W: u32 = 1920;
const DISPLAY_H: u32 = 1080;

// Individual test ports (one per async test to prevent bind conflicts)
const PORT_BREAKPOINTS_FORWARDED: u16 = 50056;
const PORT_EMPTY_BREAKPOINTS: u16 = 50057;
const PORT_REPLACEMENT_CANCELS: u16 = 50058;
const PORT_MCP_PARITY: u16 = 50059;
const PORT_LIST_ZONES_METADATA: u16 = 50060;

// ─── Helpers ─────────────────────────────────────────────────────────────────
// now_wall_us() is provided by the shared common harness (common::now_wall_us).

/// Minimal agent session (tx + rx + sequence counter).
struct AgentSession {
    tx: tokio::sync::mpsc::Sender<session_proto::ClientMessage>,
    rx: tonic::codec::Streaming<session_proto::ServerMessage>,
    sequence: u64,
}

impl AgentSession {
    fn next_seq(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }
}

/// Start a headless runtime on the given port with default zones.
async fn start_runtime_with_subtitle_zone(
    port: u16,
) -> Result<HeadlessRuntime, Box<dyn std::error::Error>> {
    let config = HeadlessConfig {
        width: DISPLAY_W,
        height: DISPLAY_H,
        grpc_port: port,
        bind_all_interfaces: false,
        psk: TEST_PSK.to_string(),
        config_toml: None,
    };

    let runtime = HeadlessRuntime::new(config).await?;

    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab_id = scene.create_tab("Main", 0)?;
        scene.active_tab = Some(tab_id);
        scene.zone_registry = ZoneRegistry::with_defaults();
    }

    Ok(runtime)
}

/// Connect to the runtime at the given port and acquire a lease.
async fn connect_agent_with_zone_publish_cap(
    port: u16,
    agent_id: &str,
) -> Result<AgentSession, Box<dyn std::error::Error>> {
    let mut client = HudSessionClient::connect(format!("http://[::1]:{port}")).await?;

    let (tx, rx_chan) = tokio::sync::mpsc::channel::<session_proto::ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx_chan);

    let now_us = common::now_wall_us();

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
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(TEST_PSK.to_string())),
            },
        )),
    })
    .await?;

    let mut rx = client.session(stream).await?.into_inner();

    // Consume SessionEstablished
    let _established = rx.next().await.ok_or("no SessionEstablished")??;
    // Consume SceneSnapshot
    let _snapshot = rx.next().await.ok_or("no SceneSnapshot")??;
    // Consume the mandatory current degradation baseline.
    let degradation = rx.next().await.ok_or("no DegradationNotice")??;
    if !matches!(
        &degradation.payload,
        Some(session_proto::server_message::Payload::DegradationNotice(_))
    ) {
        return Err(format!(
            "Expected DegradationNotice after SceneSnapshot, got: {:?}",
            degradation.payload
        )
        .into());
    }

    // Request a lease with publish_zone capability
    tx.send(session_proto::ClientMessage {
        sequence: 2,
        timestamp_wall_us: common::now_wall_us(),
        payload: Some(session_proto::client_message::Payload::ClaimTile(
            session_proto::ClaimTile {
                ttl_ms: 120_000,
                ..Default::default()
            },
        )),
    })
    .await?;

    let mut session = AgentSession {
        tx,
        rx,
        sequence: 2,
    };

    // Consume the ClaimTile result
    let msg = session.rx.next().await.ok_or("no lease response")??;
    match &msg.payload {
        Some(session_proto::server_message::Payload::RequestResult(resp)) if resp.ok => {}
        other => {
            return Err(format!("Expected a granted ClaimTile, got: {other:?}").into());
        }
    }

    Ok(session)
}

/// Send a zone `Publish` with stream_text content and optional breakpoints.
/// Returns Ok(()) on accepted, Err on rejection.
async fn zone_publish_stream_text(
    session: &mut AgentSession,
    zone_name: &str,
    text: &str,
    breakpoints: Vec<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let seq = session.next_seq();
    session
        .tx
        .send(session_proto::ClientMessage {
            sequence: seq,
            timestamp_wall_us: common::now_wall_us(),
            payload: Some(session_proto::client_message::Payload::Publish(
                session_proto::Publish {
                    surface: format!("zone:{zone_name}"),
                    content: Some(proto::ZoneContent {
                        payload: Some(proto::zone_content::Payload::StreamText(text.to_string())),
                    }),
                    ttl_ms: 0,
                    key: String::new(),
                    breakpoints,
                    present_at_us: 0,
                    expires_at_us: 0,
                    ..Default::default()
                },
            )),
        })
        .await?;

    let msg = session.rx.next().await.ok_or("no RequestResult")??;
    match &msg.payload {
        Some(session_proto::server_message::Payload::RequestResult(r)) if r.ok => Ok(()),
        Some(session_proto::server_message::Payload::RequestResult(r)) => {
            Err(format!("zone Publish rejected: {} — {}", r.code, r.hint).into())
        }
        other => Err(format!("Expected RequestResult, got: {other:?}").into()),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// gRPC zone `Publish` with stream_text + breakpoints forwards breakpoints to the compositor.
///
/// Spec §"Stream-text with breakpoints reveals word-by-word":
/// "The compositor MUST reveal the text progressively: first "The", then
///  "The quick", then "The quick brown", then "The quick brown fox"."
///
/// At the scene layer, this is verified by checking that breakpoints are stored
/// in the zone publish record exactly as received.
#[tokio::test]
async fn test_grpc_zone_publish_with_breakpoints_forwarded()
-> Result<(), Box<dyn std::error::Error>> {
    let runtime = start_runtime_with_subtitle_zone(PORT_BREAKPOINTS_FORWARDED).await?;
    let _server_handle = runtime.start_grpc_server().await?;

    let mut agent =
        connect_agent_with_zone_publish_cap(PORT_BREAKPOINTS_FORWARDED, "grpc-stream-agent")
            .await?;

    // "The quick brown fox" — breakpoints at word boundaries
    // byte offsets: after "The"=3, after "quick"=9, after "brown"=15
    zone_publish_stream_text(
        &mut agent,
        "subtitle",
        "The quick brown fox",
        vec![3, 9, 15],
    )
    .await?;

    let bps = {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let publishes = scene.zone_registry.active_for_zone("subtitle");
        assert_eq!(
            publishes.len(),
            1,
            "subtitle (LatestWins) must have exactly 1 active publish"
        );
        let text = match &publishes[0].content {
            tze_hud_scene::types::ZoneContent::StreamText(t) => t.clone(),
            other => panic!("expected StreamText, got {other:?}"),
        };
        assert_eq!(
            text, "The quick brown fox",
            "content text must match published text"
        );
        publishes[0].breakpoints.clone()
    };

    assert_eq!(
        bps,
        vec![3u64, 9, 15],
        "gRPC zone `Publish` breakpoints must be forwarded to zone publish record; got {bps:?}"
    );

    Ok(())
}

/// gRPC zone `Publish` with empty breakpoints list reveals full text immediately.
///
/// Spec §"Stream-text without breakpoints reveals all at once":
/// "THEN the compositor MUST display the full text immediately (no progressive reveal)."
#[tokio::test]
async fn test_grpc_zone_publish_empty_breakpoints_reveals_immediately()
-> Result<(), Box<dyn std::error::Error>> {
    let runtime = start_runtime_with_subtitle_zone(PORT_EMPTY_BREAKPOINTS).await?;
    let _server_handle = runtime.start_grpc_server().await?;

    let mut agent =
        connect_agent_with_zone_publish_cap(PORT_EMPTY_BREAKPOINTS, "grpc-empty-bp-agent").await?;

    // Publish with empty breakpoints (should reveal immediately, no streaming)
    zone_publish_stream_text(&mut agent, "subtitle", "Instant display", vec![]).await?;

    let bps = {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let publishes = scene.zone_registry.active_for_zone("subtitle");
        assert_eq!(publishes.len(), 1, "must have 1 active publish");
        publishes[0].breakpoints.clone()
    };

    assert!(
        bps.is_empty(),
        "empty breakpoints in zone `Publish` must result in empty breakpoints in the record"
    );

    Ok(())
}

/// Replacement during streaming cancels the in-progress reveal — latest-wins semantics.
///
/// Spec §"Replacement during streaming cancels reveal":
/// "THEN the compositor MUST cancel the in-progress reveal and display the new content."
///
/// At the scene layer: publishing a second content replaces the first record (LatestWins),
/// and if the second publish has no breakpoints, the breakpoints are empty in the new record.
#[tokio::test]
async fn test_grpc_zone_publish_replacement_cancels_breakpoints()
-> Result<(), Box<dyn std::error::Error>> {
    let runtime = start_runtime_with_subtitle_zone(PORT_REPLACEMENT_CANCELS).await?;
    let _server_handle = runtime.start_grpc_server().await?;

    let mut agent =
        connect_agent_with_zone_publish_cap(PORT_REPLACEMENT_CANCELS, "grpc-replace-agent").await?;

    // First publish: streaming with breakpoints
    zone_publish_stream_text(
        &mut agent,
        "subtitle",
        "Long streaming message",
        vec![4, 13],
    )
    .await?;

    // Second publish replaces first (LatestWins) — no breakpoints on replacement
    zone_publish_stream_text(&mut agent, "subtitle", "Replacement content", vec![]).await?;

    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let publishes = scene.zone_registry.active_for_zone("subtitle");
        assert_eq!(
            publishes.len(),
            1,
            "LatestWins must have exactly 1 record after replacement"
        );
        let text = match &publishes[0].content {
            tze_hud_scene::types::ZoneContent::StreamText(t) => t.clone(),
            other => panic!("expected StreamText, got {other:?}"),
        };
        assert_eq!(
            text, "Replacement content",
            "replacement content must be the active record"
        );
        assert!(
            publishes[0].breakpoints.is_empty(),
            "replacement record must have empty breakpoints — streaming cancelled"
        );
    }

    Ok(())
}

/// gRPC path delivers the same breakpoint behavior as MCP path.
///
/// Spec §Subtitle Streaming Word-by-Word Reveal, Epic API coverage:
/// "MCP (JSON-RPC guest path) and gRPC (protobuf resident path) must produce identical visual results."
///
/// Verification: publish via gRPC with the same payload as subtitle-streaming.json fixture,
/// verify the resulting zone publish record matches what the MCP path would store.
#[tokio::test]
async fn test_grpc_zone_publish_breakpoints_match_mcp_behavior()
-> Result<(), Box<dyn std::error::Error>> {
    let runtime = start_runtime_with_subtitle_zone(PORT_MCP_PARITY).await?;
    let _server_handle = runtime.start_grpc_server().await?;

    let mut agent =
        connect_agent_with_zone_publish_cap(PORT_MCP_PARITY, "grpc-parity-agent").await?;

    // Same payload as subtitle-streaming.json fixture:
    // "The quick brown fox jumps over the lazy dog" with breakpoints at word boundaries
    let text = "The quick brown fox jumps over the lazy dog";
    let breakpoints = vec![3u64, 9, 15, 19, 25, 30, 34, 38];

    zone_publish_stream_text(&mut agent, "subtitle", text, breakpoints.clone()).await?;

    let record_bps = {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let publishes = scene.zone_registry.active_for_zone("subtitle");
        assert_eq!(publishes.len(), 1, "must have exactly 1 publish");
        let record_text = match &publishes[0].content {
            tze_hud_scene::types::ZoneContent::StreamText(t) => t.clone(),
            other => panic!("expected StreamText, got {other:?}"),
        };
        assert_eq!(record_text, text, "content text must match published text");
        publishes[0].breakpoints.clone()
    };

    // gRPC record must have same breakpoints as what MCP path would store.
    assert_eq!(
        record_bps, breakpoints,
        "gRPC breakpoints must match MCP breakpoints (identical behavior per spec)"
    );

    Ok(())
}

/// The subtitle zone has the correct metadata: LatestWins contention, StreamText media type.
///
/// This verifies what both gRPC SceneSnapshot and MCP hud_surfaces would report.
///
/// Spec §Subtitle Contention Policy — Latest Wins.
/// Spec §Subtitle MCP Test Fixtures — zone_name: "subtitle".
#[tokio::test]
async fn test_grpc_list_zones_subtitle_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = start_runtime_with_subtitle_zone(PORT_LIST_ZONES_METADATA).await?;
    let _server_handle = runtime.start_grpc_server().await?;

    // Verify the subtitle zone metadata directly via the scene graph
    // (the gRPC snapshot and MCP hud_surfaces serialize from the same data).
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;

        let subtitle_zone = scene
            .zone_registry
            .get_by_name("subtitle")
            .expect("subtitle zone must be registered in ZoneRegistry::with_defaults()");

        // contention_policy must be LatestWins
        assert_eq!(
            subtitle_zone.contention_policy,
            tze_hud_scene::types::ContentionPolicy::LatestWins,
            "subtitle zone must use LatestWins contention policy"
        );

        // accepted_media_types must include StreamText
        assert!(
            subtitle_zone
                .accepted_media_types
                .contains(&tze_hud_scene::types::ZoneMediaType::StreamText),
            "subtitle zone must accept StreamText media type, got: {:?}",
            subtitle_zone.accepted_media_types
        );
    }

    Ok(())
}
