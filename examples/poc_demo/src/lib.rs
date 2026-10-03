//! # POC demo: one binary that drives a live tze_hud through every lifecycle stage
//!
//! Talks to a running app the way an agent would: MCP over HTTP
//! (`hud_surfaces`, `hud_publish`, `hud_input`, `hud_clear`) for zones and
//! widgets, and one gRPC `Session` for a resident tile (`ClaimTile` with
//! placement and root, `MutationBatch`, `Hold`, `Reclaimed`). Each stage is
//! narrated in `zone:subtitle`, and every MCP call prints the model-visible
//! tokens it cost (tool name + arguments + result text, o200k), the number
//! `docs/api.md` "Token budgets" holds each stage to.
//!
//! The portal stage is not here: it is the real Claude Code session running
//! the `hud-projection` skill ([`PORTAL_INSTRUCTIONS`]).
//!
//! PSKs come from the environment or a file and are never printed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tze_hud_protocol::auth::{RUNTIME_MAX_VERSION, RUNTIME_MIN_VERSION, psk_credential};
use tze_hud_protocol::proto::session::client_message::Payload as Request;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::server_message::Payload as Reply;
use tze_hud_protocol::proto::session::{
    self as wire, ClaimTile, Clear, ClientMessage, Hold, MutationBatch, RequestResult, SessionInit,
    TileAnchor, TilePlacement, TileSize,
};
use tze_hud_protocol::proto::{
    MutationProto, NodeProto, Rect, Rgba, TextMarkdownNodeProto, UpdateNodeContentMutation,
    mutation_proto, node_proto, update_node_content_mutation,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub const SUBTITLE: &str = "zone:subtitle";
pub const NOTIFICATION: &str = "zone:notification-area";
pub const GAUGE: &str = "widget:main-gauge";
pub const PROGRESS: &str = "widget:main-progress";

/// What to run in place of a portal stage.
pub const PORTAL_INSTRUCTIONS: &str = "Portal: not simulated here. In a Claude Code session, run the hud-projection skill \
     (.claude/skills/hud-projection/SKILL.md): its first hud_publish to portal:<id> attaches, \
     hud_input carries your reply, hud_clear detaches.";

/// Where the live app listens, who we are to it, and how fast to go.
pub struct Target {
    /// `host:port` of the MCP HTTP listener.
    pub mcp: String,
    /// `host:port` of the gRPC listener.
    pub grpc: String,
    pub mcp_psk: String,
    /// The paired agent id the tile PSK belongs to (the gRPC handshake names it).
    pub tile_agent: String,
    pub tile_psk: String,
    /// Pause between steps so a person can watch.
    pub pace: Duration,
    /// How long a stage waits on a human (notification press, tile dismiss).
    pub human_wait: Duration,
    /// Set once the tile stage's `Hold` is acked, so a driver (a test playing
    /// the viewer) can act at exactly that point.
    pub held: Option<Arc<AtomicBool>>,
}

/// MCP over HTTP: one tool call per connection, tokens counted per call.
pub struct Mcp<'a>(pub &'a Target);

impl Mcp<'_> {
    /// Call a tool, print its model-visible tokens, and return the parsed
    /// result. A tool error is returned as `code: hint`.
    pub async fn call(&self, tool: &str, arguments: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        })
        .to_string();
        let mut stream = TcpStream::connect(&self.0.mcp)
            .await
            .map_err(|e| format!("connect to MCP at {}: {e}", self.0.mcp))?;
        // HTTP/1.0: the server answers and closes, so no framing to parse.
        let request = format!(
            "POST /mcp HTTP/1.0\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.0.mcp_psk,
            body.len()
        );
        stream.write_all(request.as_bytes()).await?;
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await?;
        let (head, payload) = raw.split_once("\r\n\r\n").ok_or("no HTTP response")?;
        let status = head.lines().next().unwrap_or_default();
        if !status.contains(" 200") {
            return Err(format!("MCP {status}: is the MCP PSK paired?").into());
        }
        let response: Value = serde_json::from_str(payload)?;
        if let Some(error) = response.get("error") {
            return Err(format!("JSON-RPC error: {error}").into());
        }
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .ok_or("no result text")?;
        println!(
            "  {tool:<12} {:<26} {:>4} tokens",
            arguments["surface"].as_str().unwrap_or(""),
            model_tokens(&format!("{tool}{arguments}{text}"))
        );
        let parsed: Value = serde_json::from_str(text)?;
        if result["isError"] == json!(true) {
            return Err(format!("{}: {}", parsed["code"], parsed["hint"]).into());
        }
        Ok(parsed)
    }

    /// Say what the next stage shows, on `zone:subtitle`.
    pub async fn narrate(&self, text: &str) -> Result<()> {
        println!("\n{text}");
        self.call(
            "hud_publish",
            json!({"surface": SUBTITLE, "content": text, "ttl_ms": 8_000}),
        )
        .await?;
        tokio::time::sleep(self.0.pace).await;
        Ok(())
    }

    /// `hud_surfaces`, failing with a pointer to the config if any of
    /// `required` is not offered to this agent.
    pub async fn require(&self, required: &[&str]) -> Result<()> {
        let surfaces = self.call("hud_surfaces", json!({})).await?;
        for surface in required {
            let offered = surfaces["surfaces"]
                .as_array()
                .is_some_and(|all| all.iter().any(|entry| entry["s"] == *surface));
            if !offered {
                return Err(format!(
                    "{surface} is not offered: run the app with its production.toml and pair an agent that allows it"
                )
                .into());
            }
        }
        Ok(())
    }
}

/// o200k tokens, counted the way `token_footprint` counts them.
fn model_tokens(text: &str) -> usize {
    static BPE: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::o200k_base().expect("bundled o200k_base vocabulary"))
        .encode_with_special_tokens(text)
        .len()
}

/// Zones: a notification that expires on its TTL, `delay_ms` content, and a
/// notification action pressed by the viewer reaching `hud_input`.
pub async fn zones(target: &Target) -> Result<()> {
    let mcp = Mcp(target);
    mcp.require(&[SUBTITLE, NOTIFICATION]).await?;

    mcp.narrate("Zones: a notification that takes itself down after its TTL")
        .await?;
    mcp.call(
        "hud_publish",
        json!({
            "surface": NOTIFICATION,
            "content": {
                "title": "Deploy",
                "body": "Ship v2?",
                "actions": [
                    {"label": "Ship", "callback_id": "ship"},
                    {"label": "Hold", "callback_id": "hold"},
                ],
            },
            "ttl_ms": 20_000,
        }),
    )
    .await?;

    mcp.narrate("Zones: delay_ms content appears on schedule, nobody calls again")
        .await?;
    mcp.call(
        "hud_publish",
        json!({"surface": SUBTITLE, "content": "Scheduled 2 s ago (delay_ms)", "delay_ms": 2_000, "ttl_ms": 6_000}),
    )
    .await?;

    println!(
        "\nPress Ship or Hold on the notification ({} s)...",
        target.human_wait.as_secs()
    );
    let polled = mcp
        .call(
            "hud_input",
            json!({"wait_ms": target.human_wait.as_millis().min(30_000)}),
        )
        .await?;
    let items = polled["items"].as_array().cloned().unwrap_or_default();
    match items.first() {
        Some(item) => {
            println!("  pressed: {}", item["action"]);
            mcp.call("hud_input", json!({"ack": [item["id"]]})).await?;
        }
        None => println!("  no press; skipped"),
    }
    Ok(())
}

/// Widgets: typed parameter updates only; the runtime owns the SVG and the pixels.
pub async fn widgets(target: &Target) -> Result<()> {
    let mcp = Mcp(target);
    mcp.require(&[SUBTITLE, GAUGE, PROGRESS]).await?;

    mcp.narrate("Widgets: typed params, no styling or geometry in the call")
        .await?;
    for (level, label) in [(0.25, "building"), (0.6, "testing"), (0.95, "shipping")] {
        mcp.call(
            "hud_publish",
            json!({"surface": GAUGE, "params": {"level": level, "label": label}}),
        )
        .await?;
        mcp.call(
            "hud_publish",
            json!({"surface": PROGRESS, "params": {"progress": level, "label": label}}),
        )
        .await?;
        tokio::time::sleep(target.pace).await;
    }
    mcp.call("hud_clear", json!({"surface": GAUGE})).await?;
    mcp.call("hud_clear", json!({"surface": PROGRESS})).await?;
    Ok(())
}

/// One resident gRPC `Session`.
pub struct Session {
    tx: tokio::sync::mpsc::Sender<ClientMessage>,
    replies: tonic::Streaming<wire::ServerMessage>,
    sequence: u64,
}

impl Session {
    /// Open the stream and handshake: the first round trip.
    pub async fn connect(grpc: &str, agent: &str, psk: &str) -> Result<Self> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let replies = HudSessionClient::connect(format!("http://{grpc}"))
            .await
            .map_err(|e| format!("connect to gRPC at {grpc}: {e}"))?
            .session(ReceiverStream::new(rx))
            .await?
            .into_inner();
        let mut session = Self {
            tx,
            replies,
            sequence: 0,
        };
        session
            .send(Request::SessionInit(SessionInit {
                agent_id: agent.to_string(),
                min_protocol_version: RUNTIME_MIN_VERSION,
                max_protocol_version: RUNTIME_MAX_VERSION,
                auth_credential: Some(psk_credential(psk)),
                ..Default::default()
            }))
            .await?;
        loop {
            match session.next_reply().await? {
                Reply::SessionEstablished(_) => return Ok(session),
                Reply::SessionError(e) => {
                    return Err(format!("{}: is the tile PSK paired? {}", e.code, e.message).into());
                }
                _ => {}
            }
        }
    }

    async fn send(&mut self, payload: Request) -> Result<u64> {
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

    async fn next_reply(&mut self) -> Result<Reply> {
        loop {
            let message = self.replies.next().await.ok_or("stream closed")??;
            if let Some(payload) = message.payload {
                return Ok(payload);
            }
        }
    }

    /// Send a verb and return its `RequestResult`, or its `code: hint`.
    async fn request(&mut self, payload: Request) -> Result<RequestResult> {
        let seq = self.send(payload).await?;
        loop {
            match self.next_reply().await? {
                Reply::RequestResult(r) if r.seq == seq => {
                    return if r.ok {
                        Ok(r)
                    } else if r.code == "NOT_HELD" {
                        // The viewer dismissed the tile before this request landed.
                        Err(Box::new(Reclaimed(format!(
                            "{} dismissed: NOT_HELD",
                            r.hint
                        ))))
                    } else {
                        Err(format!("{}: {}", r.code, r.hint).into())
                    };
                }
                Reply::Reclaimed(r) => {
                    return Err(Box::new(Reclaimed(format!(
                        "{} reclaimed: {:?}",
                        r.surface,
                        r.why()
                    ))));
                }
                _ => {}
            }
        }
    }

    /// `ClaimTile` with a placement hint and a text root: a visible, filled
    /// tile in one round trip.
    async fn claim(&mut self, text: &str) -> Result<Claimed> {
        let claimed = self
            .request(Request::ClaimTile(ClaimTile {
                placement: Some(TilePlacement {
                    anchor: TileAnchor::TopRight as i32,
                    size: TileSize::Medium as i32,
                }),
                ttl_ms: 60_000,
                root: Some(text_root(text)),
            }))
            .await?;
        let [tile, node, ..] = claimed.ids.as_slice() else {
            return Err("ClaimTile returned no tile and root ids".into());
        };
        println!("  ClaimTile    {:<26} ttl {} ms", "tile", claimed.ttl_ms);
        Ok(Claimed {
            surface: tile_surface(tile),
            tile: tile.clone(),
            lease: claimed.lease_id.clone(),
            node: node.clone(),
        })
    }
}

/// The runtime took the tile back (the viewer dismissed it) while a request
/// was in flight, or before it was sent.
#[derive(Debug)]
struct Reclaimed(String);

impl std::fmt::Display for Reclaimed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Reclaimed {}

/// What `ClaimTile` returned.
struct Claimed {
    surface: String,
    tile: Vec<u8>,
    lease: Vec<u8>,
    /// The root node's id.
    node: Vec<u8>,
}

/// Tile: claim with placement and root (two round trips from init), update the
/// root, hold, then wait for the runtime's `Reclaimed` (the viewer's close
/// button) and clear the tile ourselves if none comes.
pub async fn tile(target: &Target) -> Result<()> {
    match claim_update_hold(target).await {
        // The viewer may dismiss at any point; that is the stage working.
        Err(e) if e.is::<Reclaimed>() => {
            println!("  Reclaimed    {e}");
            Ok(())
        }
        other => other,
    }
}

async fn claim_update_hold(target: &Target) -> Result<()> {
    let mcp = Mcp(target);
    mcp.narrate("Tile: a resident agent claims a placed, filled tile in two round trips")
        .await?;
    let mut session = Session::connect(&target.grpc, &target.tile_agent, &target.tile_psk).await?;
    let Claimed {
        surface,
        tile,
        lease,
        node,
    } = session.claim("building").await?;
    tokio::time::sleep(target.pace).await;

    session
        .request(Request::MutationBatch(MutationBatch {
            batch_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
            lease_id: lease,
            mutations: vec![MutationProto {
                mutation: Some(mutation_proto::Mutation::UpdateNodeContent(
                    UpdateNodeContentMutation {
                        tile_id: tile,
                        node_id: node,
                        data: Some(update_node_content_mutation::Data::TextMarkdown(text_node(
                            "tests passed",
                        ))),
                    },
                )),
            }],
            timing: None,
        }))
        .await?;
    println!("  MutationBatch {surface} updated");
    let held = session
        .request(Request::Hold(Hold {
            surface: surface.clone(),
            ttl_ms: 120_000,
        }))
        .await?;
    println!("  Hold         {surface} ttl {} ms", held.ttl_ms);
    if let Some(flag) = &target.held {
        flag.store(true, Ordering::SeqCst);
    }

    println!(
        "\nClose the tile with its close button to see Reclaimed ({} s)...",
        target.human_wait.as_secs()
    );
    let reclaimed = tokio::time::timeout(target.human_wait, async {
        loop {
            if let Reply::Reclaimed(r) = session.next_reply().await? {
                return Ok::<_, Box<dyn std::error::Error>>(r);
            }
        }
    })
    .await;
    match reclaimed {
        Ok(r) => {
            let r = r?;
            println!("  Reclaimed    {} {:?}", r.surface, r.why());
        }
        Err(_) => {
            session.request(Request::Clear(Clear { surface })).await?;
            println!("  Clear        tile released");
        }
    }
    Ok(())
}

/// Override: claim a tile, then idle for `human_wait` without reading the
/// stream or flooding the session. This only sets the scene for a person to
/// press the tile's close button or the safe-mode chord; it does not verify
/// the override (`tests/integration/poc_acceptance.rs`'s `HungAgent` backs the
/// server's send buffer up and asserts it). On exit there is no `SessionClose`,
/// so the runtime reclaims the orphan after its grace period.
pub async fn override_hang(target: &Target) -> Result<()> {
    let mcp = Mcp(target);
    mcp.narrate(
        "Override: this agent has hung. Press the tile's close button or the safe-mode chord",
    )
    .await?;
    let mut session = Session::connect(&target.grpc, &target.tile_agent, &target.tile_psk).await?;
    session.claim("agent hung: not reading").await?;
    println!(
        "\nNot reading the stream for {} s; the viewer must win...",
        target.human_wait.as_secs()
    );
    tokio::time::sleep(target.human_wait).await;
    println!(
        "  idle time over; exiting without SessionClose: the orphaned tile is reclaimed after grace"
    );
    Ok(())
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

fn text_node(text: &str) -> TextMarkdownNodeProto {
    TextMarkdownNodeProto {
        content: text.to_string(),
        bounds: Some(Rect {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 40.0,
        }),
        font_size_px: 16.0,
        color: Some(Rgba {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        }),
        ..Default::default()
    }
}

fn text_root(text: &str) -> NodeProto {
    NodeProto {
        data: Some(node_proto::Data::TextMarkdown(text_node(text))),
        ..Default::default()
    }
}

/// The PSK in a credential file's text: the bare key, or the JSON reply of
/// `POST /pair` (`curl .../pair > file`). Errors never echo the contents.
pub fn psk_from_file_text(text: &str) -> Result<String> {
    let psk = if text.trim_start().starts_with('{') {
        serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|reply| reply["psk"].as_str().map(str::to_string))
            .ok_or("JSON credential file has no \"psk\" field")?
    } else {
        text.to_string()
    };
    let psk = psk.trim();
    if psk.is_empty() {
        return Err("credential is empty".into());
    }
    Ok(psk.to_string())
}

#[cfg(test)]
mod tests {
    use super::psk_from_file_text;

    #[test]
    fn credential_files_hold_a_bare_psk_or_a_pair_reply() {
        assert_eq!(psk_from_file_text("abc123\n").unwrap(), "abc123");
        assert_eq!(
            psk_from_file_text(r#"{"agent":"c","psk":"abc123"}"#).unwrap(),
            "abc123"
        );
        assert!(psk_from_file_text(" \n").is_err());
        let err = psk_from_file_text(r#"{"agent":"secret-looking"}"#).unwrap_err();
        assert!(
            !err.to_string().contains("secret-looking"),
            "must not echo the file"
        );
    }
}
