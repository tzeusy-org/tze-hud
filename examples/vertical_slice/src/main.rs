//! # Vertical slice: a resident agent over the gRPC lifecycle verbs
//!
//! Boots a headless runtime from `config/production.toml`, then drives it as
//! the one paired agent (`config/agents.toml`) over a single `Session` stream:
//!
//! 1. `SessionInit` with the agent's PSK (identity and permissions come from
//!    the config's `allow` list)
//! 2. `ClaimTile` with a placement hint and a text root: a visible, filled tile
//!    in one round trip
//! 3. `Publish` to `zone:status-bar`
//! 4. `Hold` the tile (renew without resending content)
//! 5. `Clear` the tile (releases the lease)
//!
//! See `docs/api.md` for the verbs. This is a development reference, not a
//! deployment: run the `tze_hud` app binary for a real display.
//!
//! ```sh
//! cargo run -p vertical_slice
//! ```

use std::collections::HashMap;

use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tze_hud_protocol::auth::psk_credential;
use tze_hud_protocol::proto::session::client_message::Payload as Request;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::server_message::Payload as Reply;
use tze_hud_protocol::proto::session::{
    self as session_proto, ClaimTile, Clear, ClientMessage, Hold, Publish, RequestResult,
    SessionInit, TileAnchor, TilePlacement, TileSize,
};
use tze_hud_protocol::proto::{
    NodeProto, Rect, Rgba, StatusBarPayload, TextMarkdownNodeProto, ZoneContent, node_proto,
    zone_content,
};
use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;

type BoxError = Box<dyn std::error::Error>;

/// Runtime config and paired agents, embedded at compile time.
const PRODUCTION_CONFIG: &str = include_str!("../config/production.toml");
const PRODUCTION_AGENTS: &str = include_str!("../config/agents.toml");

/// The demo PSK whose SHA-256 `config/agents.toml` stores.
const AGENT_PSK: &str = "vertical-slice-key";
const AGENT_ID: &str = "vertical-slice-agent";

const USAGE: &str = "vertical_slice: drive a headless tze_hud runtime as a resident gRPC agent

USAGE: cargo run -p vertical_slice [-- --help]

Boots the runtime with examples/vertical_slice/config/production.toml and runs
SessionInit, ClaimTile, Publish, Hold, and Clear (see docs/api.md).";

fn main() -> Result<(), BoxError> {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let runtime = boot(50051).await?;
            let granted =
                run_lifecycle(&mut Agent::connect(50051, AGENT_PSK).await?, async || {}).await?;
            println!(
                "granted ttl: claim {} ms, hold {} ms",
                granted.claim_ttl_ms, granted.hold_ttl_ms
            );
            println!("tiles left on the scene: {}", tile_count(&runtime).await);
            Ok(())
        })
}

/// Boot a headless runtime with the production config and serve gRPC on `port`.
async fn boot(port: u16) -> Result<HeadlessRuntime, BoxError> {
    let runtime = HeadlessRuntime::new(HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: port,
        agents: tze_hud_config::AgentsFile::parse(PRODUCTION_AGENTS)?.directory()?,
        config_toml: Some(PRODUCTION_CONFIG.to_string()),
    })
    .await?;
    runtime.start_grpc_server().await?;
    Ok(runtime)
}

async fn tile_count(runtime: &HeadlessRuntime) -> usize {
    let scene = runtime.shared_state().lock().await.scene.clone();
    scene.lock().await.tiles.len()
}

/// One resident session: sends requests and waits for each `RequestResult`.
struct Agent {
    tx: tokio::sync::mpsc::Sender<ClientMessage>,
    replies: tonic::Streaming<session_proto::ServerMessage>,
    sequence: u64,
}

impl Agent {
    /// Open the stream and complete the handshake.
    async fn connect(port: u16, psk: &str) -> Result<Self, BoxError> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut agent = Self {
            tx,
            replies: HudSessionClient::connect(format!("http://[::1]:{port}"))
                .await?
                .session(ReceiverStream::new(rx))
                .await?
                .into_inner(),
            sequence: 0,
        };
        agent
            .send(Request::SessionInit(SessionInit {
                agent_id: AGENT_ID.to_string(),
                initial_subscriptions: Vec::new(),
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(psk_credential(psk)),
            }))
            .await?;
        loop {
            match agent.next_reply().await? {
                Reply::SessionEstablished(est) => {
                    println!("session established, namespace {}", est.namespace);
                    return Ok(agent);
                }
                Reply::SessionError(e) => return Err(format!("{}: {}", e.code, e.message).into()),
                _ => {}
            }
        }
    }

    async fn send(&mut self, payload: Request) -> Result<u64, BoxError> {
        self.sequence += 1;
        self.tx
            .send(ClientMessage {
                sequence: self.sequence,
                timestamp_wall_us: 0,
                payload: Some(payload),
            })
            .await?;
        Ok(self.sequence)
    }

    async fn next_reply(&mut self) -> Result<Reply, BoxError> {
        loop {
            let msg = self.replies.next().await.ok_or("stream closed")??;
            if let Some(payload) = msg.payload {
                return Ok(payload);
            }
        }
    }

    /// Send a verb and return its `RequestResult`, or its `code: hint` as an
    /// error. Pushed messages (snapshot, heartbeat, events) are skipped.
    async fn request(&mut self, payload: Request) -> Result<RequestResult, BoxError> {
        let seq = self.send(payload).await?;
        loop {
            match self.next_reply().await? {
                Reply::RequestResult(r) if r.seq == seq => {
                    return if r.ok {
                        Ok(r)
                    } else {
                        Err(format!("{}: {}", r.code, r.hint).into())
                    };
                }
                Reply::Reclaimed(r) => return Err(format!("reclaimed: {:?}", r.why()).into()),
                _ => {}
            }
        }
    }
}

/// TTLs the runtime granted to the claim and the hold.
struct Granted {
    claim_ttl_ms: u64,
    hold_ttl_ms: u64,
}

/// Claim a filled tile, publish a status entry, hold the tile, then clear it.
async fn run_lifecycle(
    agent: &mut Agent,
    before_clear: impl AsyncFnOnce(),
) -> Result<Granted, BoxError> {
    // Claim: lease, tile, and content in one round trip. The runtime resolves
    // the placement hint into bounds and z-order.
    let claimed = agent
        .request(Request::ClaimTile(ClaimTile {
            placement: Some(TilePlacement {
                anchor: TileAnchor::TopLeft as i32,
                size: TileSize::Wide as i32,
            }),
            ttl_ms: 60_000,
            root: Some(text_root("Hello from the vertical slice")),
        }))
        .await?;
    let tile = tile_surface(claimed.ids.first().ok_or("ClaimTile returned no tile id")?);
    println!("claimed {tile}, ttl {} ms", claimed.ttl_ms);

    // Fill a zone: no geometry or styling, only content.
    agent
        .request(Request::Publish(Publish {
            surface: "zone:status-bar".to_string(),
            content: Some(ZoneContent {
                payload: Some(zone_content::Payload::StatusBar(StatusBarPayload {
                    entries: HashMap::from([("agent".to_string(), AGENT_ID.to_string())]),
                })),
            }),
            ..Default::default()
        }))
        .await?;
    println!("published zone:status-bar");

    // Hold: renew the lease without resending content.
    let held = agent
        .request(Request::Hold(Hold {
            surface: tile.clone(),
            ttl_ms: 120_000,
        }))
        .await?;
    println!("held {tile}, ttl {} ms", held.ttl_ms);

    before_clear().await;

    // Clear: release the tile and its lease (and with it the zone publication).
    agent
        .request(Request::Clear(Clear {
            surface: tile.clone(),
        }))
        .await?;
    println!("cleared {tile}");
    Ok(Granted {
        claim_ttl_ms: claimed.ttl_ms,
        hold_ttl_ms: held.ttl_ms,
    })
}

/// `tile:<uuid>` from the 16 id bytes a `ClaimTile` reply carries.
fn tile_surface(id: &[u8]) -> String {
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "tile:{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn text_root(text: &str) -> NodeProto {
    NodeProto {
        data: Some(node_proto::Data::TextMarkdown(TextMarkdownNodeProto {
            content: text.to_string(),
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 280.0,
                height: 60.0,
            }),
            font_size_px: 18.0,
            color: Some(Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            }),
            background: Some(Rgba {
                r: 0.1,
                g: 0.1,
                b: 0.2,
                a: 0.9,
            }),
            ..Default::default()
        })),
        ..Default::default()
    }
}

#[cfg(test)]
#[path = "../../../crates/tze_hud_runtime/src/test_support.rs"]
mod gpu_init;

#[cfg(test)]
mod tests {
    use super::*;

    /// The paired agent can claim, publish, hold, and clear: the claim and
    /// hold grant the requested TTLs, the status-bar entry is on the scene,
    /// and clearing the tile leaves no tile behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paired_agent_runs_the_full_lifecycle() {
        let port = free_port();
        let runtime = gpu_init::serialized_headless_init(boot(port))
            .await
            .expect("runtime boots");
        let mut agent = Agent::connect(port, AGENT_PSK).await.expect("handshake");
        let granted = run_lifecycle(&mut agent, async || {
            let scene = runtime.shared_state().lock().await.scene.clone();
            let scene = scene.lock().await;
            let published = scene.zone_registry.active_publishes.get("status-bar");
            assert!(
                published.is_some_and(|p| !p.is_empty()),
                "the status-bar Publish must be on the scene"
            );
        })
        .await
        .expect("lifecycle completes");
        assert_eq!(granted.claim_ttl_ms, 60_000);
        assert_eq!(granted.hold_ttl_ms, 120_000, "Hold must renew the lease");
        assert_eq!(tile_count(&runtime).await, 0, "Clear must remove the tile");
    }

    /// An unpaired PSK never gets a session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unpaired_psk_is_rejected() {
        let port = free_port();
        let _runtime = gpu_init::serialized_headless_init(boot(port))
            .await
            .expect("runtime boots");
        let err = Agent::connect(port, "not-the-paired-key").await.err();
        assert!(
            err.is_some_and(|e| e.to_string().contains("AUTH_FAILED")),
            "unpaired PSK must fail with AUTH_FAILED"
        );
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("[::1]:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
}
