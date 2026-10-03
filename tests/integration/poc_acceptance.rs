//! POC acceptance (`docs/scope.md`): the Portal and Zones bullets, end to end.
//!
//! Each test boots the runtime from `app/tze_hud_app/config/production.toml`
//! in the GPU-free event-loop harness, calls the real MCP server over loopback
//! HTTP, synthesizes the user's clicks and keystrokes through the windowed
//! input path, and advances one injected `TestClock` (invariant 9: no
//! wall-clock sleeps). Every MCP call's model-visible tokens are counted the
//! way `token_footprint` counts them and checked against `docs/api.md`
//! "Token budgets".
//!
//! This file is the MCP-level regression guard for the T7 portal rewrite, so
//! it observes only what an agent or a user can: MCP results, and what is on
//! screen (tiles, zone publications, the composer draft).

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_stream::StreamExt;
use tze_hud_protocol::proto;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{self as wire, ClientMessage, ServerMessage};
use tze_hud_runtime::windowed::{HeadlessEventLoopHarness, WindowedConfig};
use tze_hud_scene::TestClock;
use tze_hud_scene::lease::ORPHAN_GRACE_PERIOD_MS;
use tze_hud_scene::placement::TilePlacementTokens;

const PRODUCTION_CONFIG: &str = include_str!("../../app/tze_hud_app/config/production.toml");
const PORTAL: &str = "portal:claude-main";
const NOTIFICATION: &str = "zone:notification-area";
const SUBTITLE: &str = "zone:subtitle";

// docs/api.md "Token budgets" (model-visible tokens). Discover is the
// production.toml scene: 6 zones plus the 3 built-in widgets (~230 tokens).
const DISCOVER_BUDGET: usize = 250;
const ZONE_PUBLISH_BUDGET: usize = 80;
const PORTAL_FLOW_BUDGET: usize = 250;

// Portal liveness window and lease grace (docs/api.md `hud_hold`).
const PORTAL_DEGRADE_MS: u64 = 30_000;
const PORTAL_RECLAIM_MS: u64 = 60_000;

/// Agent identity, in one place. The runtime is seeded with the directory an
/// `agents.toml` describes (agents paired by the SHA-256 of their PSK), and MCP
/// calls present the PSK as the bearer. `claude` (allow = ["*"]) is the
/// operator.
const CLAUDE_PSK: &str = "poc-acceptance-psk";

/// The resident tile agent: its own `agents.toml` entry, allowed tiles and the
/// notification zone only. It is a different identity (and namespace) from
/// `claude`, the MCP operator.
const RESIDENT_PSK: &str = "poc-acceptance-resident-psk";

fn runtime_config() -> WindowedConfig {
    WindowedConfig {
        agents: tze_hud_config::agents_file::AgentsFile::default()
            .with_agent("claude", CLAUDE_PSK, &["*"])
            .with_agent(
                "resident",
                RESIDENT_PSK,
                &["tiles", "zone:notification-area"],
            )
            .directory()
            .expect("valid agents.toml entries")
            .shared(),
        config_toml: Some(PRODUCTION_CONFIG.to_string()),
        config_file_path: Some(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../app/tze_hud_app/config/production.toml"
            )
            .to_string(),
        ),
        ..WindowedConfig::default()
    }
}

/// o200k tokens, as `token_footprint_calibration` counts them.
fn tokens(text: &str) -> usize {
    static BPE: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::o200k_base().expect("bundled o200k_base vocabulary"))
        .encode_with_special_tokens(text)
        .len()
}

/// One MCP tool result.
struct Reply {
    /// The parsed result text.
    body: Value,
    /// Tool name + arguments + result text: what enters the model's context.
    model_tokens: usize,
}

struct Poc {
    hud: HeadlessEventLoopHarness,
    clock: TestClock,
}

impl Poc {
    async fn boot() -> Self {
        let start_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as u64;
        let clock = TestClock::new(start_ms);
        let hud = HeadlessEventLoopHarness::with_network(runtime_config(), Arc::new(clock.clone()))
            .await
            .expect("boot the runtime from production.toml");
        let mut poc = Poc { hud, clock };
        poc.settle().await;
        poc
    }

    /// Call an MCP tool over loopback HTTP while the event loop keeps turning,
    /// as it does in production (portal verbs complete on the event loop).
    async fn call(&mut self, tool: &str, arguments: Value) -> Reply {
        self.call_as(CLAUDE_PSK, tool, arguments).await
    }

    /// [`Self::call`] presenting `psk` as the MCP bearer.
    async fn call_as(&mut self, psk: &'static str, tool: &str, arguments: Value) -> Reply {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        })
        .to_string();
        let mut call = tokio::spawn(post(self.hud.mcp_addr(), psk, request));
        let deadline = Instant::now() + Duration::from_secs(10);
        let response = loop {
            self.hud.tick();
            tokio::select! {
                biased;
                done = &mut call => break done.expect("MCP call task"),
                () = tokio::task::yield_now() => {}
            }
            assert!(Instant::now() < deadline, "{tool} did not complete");
        };
        self.settle().await;

        let response: Value = serde_json::from_str(&response).expect("JSON-RPC response");
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: no text result in {response}"));
        assert_ne!(result["isError"], json!(true), "{tool} failed: {text}");
        Reply {
            body: serde_json::from_str(text).expect("result text is JSON"),
            model_tokens: tokens(&format!("{tool}{arguments}{text}")),
        }
    }

    /// Call an MCP tool that must fail; returns the error `code`.
    async fn call_err(&mut self, tool: &str, arguments: Value) -> String {
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        })
        .to_string();
        let mut call = tokio::spawn(post(self.hud.mcp_addr(), CLAUDE_PSK, request));
        let response = loop {
            self.hud.tick();
            tokio::select! {
                biased;
                done = &mut call => break done.expect("MCP call task"),
                () = tokio::task::yield_now() => {}
            }
        };
        self.settle().await;
        let response: Value = serde_json::from_str(&response).expect("JSON-RPC response");
        let result = &response["result"];
        assert_eq!(
            result["isError"],
            json!(true),
            "{tool} should fail: {response}"
        );
        let text = result["content"][0]["text"].as_str().expect("error text");
        let body: Value = serde_json::from_str(text).expect("error text is JSON");
        body["code"].as_str().expect("error code").to_string()
    }

    /// `hud_surfaces`, returning this agent's entry for `surface` (if listed)
    /// after checking the discover budget.
    async fn surface(&mut self, surface: &str) -> Option<Value> {
        let reply = self.call("hud_surfaces", json!({})).await;
        assert!(
            reply.model_tokens <= DISCOVER_BUDGET,
            "discover took {} model-visible tokens (budget {DISCOVER_BUDGET})",
            reply.model_tokens
        );
        reply.body["surfaces"]
            .as_array()
            .expect("surfaces array")
            .iter()
            .find(|entry| entry["s"] == surface)
            .cloned()
    }

    /// Advance the injected clock, then let the runtime catch up.
    async fn advance(&mut self, ms: u64) {
        self.clock.advance(ms);
        self.settle().await;
    }

    /// Turn the event loop until a full turn completes without deferral.
    async fn settle(&mut self) {
        for _ in 0..1_000 {
            if self.hud.tick() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("event loop never settled");
    }

    fn shown(&self, zone_surface: &str) -> usize {
        let zone = zone_surface.strip_prefix("zone:").expect("zone surface");
        self.hud.zone_publication_count(zone)
    }
}

impl Poc {
    /// Turn the event loop until `ready` holds, within a bounded number of
    /// turns (the work it waits on runs on other tasks, never on a timer).
    async fn wait_for(&mut self, what: &str, ready: impl Fn(&HeadlessEventLoopHarness) -> bool) {
        for _ in 0..200_000 {
            self.hud.tick();
            if ready(&self.hud) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("{what}: not reached within 200000 event-loop turns");
    }

    /// Connect a gRPC agent and complete the handshake: one round trip.
    async fn resident(&self) -> Resident {
        Resident::connect(self.hud.grpc_addr(), "resident", RESIDENT_PSK)
            .await
            .expect("resident handshake")
    }
}

/// A resident gRPC agent: one `Session` stream, counting its round trips.
struct Resident {
    tx: tokio::sync::mpsc::Sender<ClientMessage>,
    stream: tonic::Streaming<ServerMessage>,
    sequence: u64,
    round_trips: usize,
    namespace: String,
}

/// What `ClaimTile` returned.
struct Claimed {
    tile: Vec<u8>,
    node: Vec<u8>,
    lease: Vec<u8>,
}

fn now_wall_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_micros() as u64
}

/// Stream messages the test is waiting on arrive from runtime tasks, never a
/// timer; this only turns a lost message into a failure instead of a hang.
async fn next_message(stream: &mut tonic::Streaming<ServerMessage>) -> ServerMessage {
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("a server message")
        .expect("stream open")
        .expect("no stream error")
}

impl Resident {
    /// The handshake: `SessionInit` out, `SessionEstablished`, the snapshot,
    /// and the degradation level back. Errors with the `SessionError` code.
    async fn connect(
        addr: std::net::SocketAddr,
        agent_id: &str,
        psk: &str,
    ) -> Result<Self, String> {
        let mut client = HudSessionClient::connect(format!("http://{addr}"))
            .await
            .expect("connect to gRPC");
        let (tx, rx) = tokio::sync::mpsc::channel(4_096);
        tx.send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionInit(wire::SessionInit {
                agent_id: agent_id.to_string(),
                min_protocol_version: tze_hud_protocol::auth::RUNTIME_MIN_VERSION,
                max_protocol_version: tze_hud_protocol::auth::RUNTIME_MAX_VERSION,
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(psk.to_string())),
                ..Default::default()
            })),
        })
        .await
        .expect("send SessionInit");
        let mut stream = client
            .session(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await
            .expect("open session")
            .into_inner();
        let namespace = match next_message(&mut stream).await.payload {
            Some(ServerPayload::SessionEstablished(e)) => e.namespace,
            Some(ServerPayload::SessionError(e)) => return Err(e.code),
            other => panic!("expected SessionEstablished, got {other:?}"),
        };
        // The snapshot and degradation level follow in the same round trip.
        for _ in 0..2 {
            next_message(&mut stream).await;
        }
        Ok(Resident {
            tx,
            stream,
            sequence: 1,
            round_trips: 1,
            namespace,
        })
    }

    fn message(&mut self, payload: ClientPayload) -> ClientMessage {
        self.sequence += 1;
        ClientMessage {
            sequence: self.sequence,
            timestamp_wall_us: now_wall_us(),
            payload: Some(payload),
        }
    }

    /// One request and its `RequestResult`.
    async fn request(&mut self, payload: ClientPayload) -> wire::RequestResult {
        let message = self.message(payload);
        self.tx.send(message).await.expect("send request");
        self.round_trips += 1;
        match next_message(&mut self.stream).await.payload {
            Some(ServerPayload::RequestResult(result)) => result,
            other => panic!("expected RequestResult, got {other:?}"),
        }
    }

    /// `ClaimTile` with a placement hint and a one-node text root.
    async fn claim(
        &mut self,
        anchor: wire::TileAnchor,
        size: wire::TileSize,
        text: &str,
    ) -> Claimed {
        let result = self.request(claim_payload(anchor, size, text)).await;
        Claimed::from(result)
    }

    /// Replace the text of a claimed tile's root node.
    async fn set_text(&mut self, claimed: &Claimed, text: &str) -> wire::RequestResult {
        self.request(ClientPayload::MutationBatch(wire::MutationBatch {
            batch_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
            lease_id: claimed.lease.clone(),
            mutations: vec![proto::MutationProto {
                mutation: Some(proto::mutation_proto::Mutation::UpdateNodeContent(
                    proto::UpdateNodeContentMutation {
                        tile_id: claimed.tile.clone(),
                        node_id: claimed.node.clone(),
                        data: Some(proto::update_node_content_mutation::Data::TextMarkdown(
                            text_node(text),
                        )),
                    },
                )),
            }],
            timing: None,
        }))
        .await
    }
}

fn claim_payload(anchor: wire::TileAnchor, size: wire::TileSize, text: &str) -> ClientPayload {
    ClientPayload::ClaimTile(wire::ClaimTile {
        placement: Some(wire::TilePlacement {
            anchor: anchor as i32,
            size: size as i32,
        }),
        ttl_ms: 600_000,
        root: Some(proto::NodeProto {
            data: Some(proto::node_proto::Data::TextMarkdown(text_node(text))),
            ..Default::default()
        }),
    })
}

impl From<wire::RequestResult> for Claimed {
    fn from(result: wire::RequestResult) -> Self {
        assert!(result.ok, "ClaimTile: {result:?}");
        assert_eq!(result.ids.len(), 2, "tile id, then the root's node id");
        Claimed {
            tile: result.ids[0].clone(),
            node: result.ids[1].clone(),
            lease: result.lease_id,
        }
    }
}

/// A resident agent whose process has stopped. A raw HTTP/2 client: it
/// handshakes and claims a tile, and from then on never reads (or releases
/// flow-control capacity for) another reply, so the server's sends to it block
/// once the stream window and its send buffer fill.
struct HungAgent {
    send: h2::SendStream<bytes::Bytes>,
    /// Kept open and unread.
    replies: GrpcReplies,
    sequence: u64,
}

impl HungAgent {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        let tcp = TcpStream::connect(addr).await.expect("connect to gRPC");
        let (requests, connection) = h2::client::Builder::new()
            .initial_window_size(16 * 1024)
            .initial_connection_window_size(16 * 1024)
            .handshake::<_, bytes::Bytes>(tcp)
            .await
            .expect("h2 handshake");
        tokio::spawn(connection);
        let request = http::Request::post(format!(
            "http://{addr}/tze_hud.protocol.v1.session.HudSession/Session"
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .expect("request");
        let (response, send) = requests
            .ready()
            .await
            .expect("h2 ready")
            .send_request(request, false)
            .expect("open stream");

        let mut agent = HungAgent {
            send,
            replies: GrpcReplies {
                body: None,
                buf: bytes::BytesMut::new(),
            },
            sequence: 0,
        };
        agent
            .send_all(vec![ClientPayload::SessionInit(wire::SessionInit {
                agent_id: "resident".to_string(),
                min_protocol_version: tze_hud_protocol::auth::RUNTIME_MIN_VERSION,
                max_protocol_version: tze_hud_protocol::auth::RUNTIME_MAX_VERSION,
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(
                    RESIDENT_PSK.to_string(),
                )),
                ..Default::default()
            })])
            .await;
        agent.replies.body = Some(response.await.expect("response headers").into_body());
        for _ in 0..3 {
            agent.replies.next().await; // established, snapshot, degradation level
        }
        agent
            .send_all(vec![claim_payload(
                wire::TileAnchor::TopRight,
                wire::TileSize::Small,
                "working",
            )])
            .await;
        match agent.replies.next().await.payload {
            Some(ServerPayload::RequestResult(result)) => drop(Claimed::from(result)),
            other => panic!("expected RequestResult, got {other:?}"),
        }
        agent
    }

    /// Send `requests`, packed into as few HTTP/2 DATA frames as flow control
    /// allows (a server drops a client that floods it with tiny frames), waiting
    /// for capacity. Never reads a reply.
    async fn send_all(&mut self, requests: Vec<ClientPayload>) {
        use bytes::BufMut;
        use prost::Message;
        let mut frames = bytes::BytesMut::new();
        for payload in requests {
            self.sequence += 1;
            let body = ClientMessage {
                sequence: self.sequence,
                timestamp_wall_us: now_wall_us(),
                payload: Some(payload),
            }
            .encode_to_vec();
            frames.put_u8(0);
            frames.put_u32(body.len() as u32);
            frames.extend_from_slice(&body);
        }
        let mut frames = frames.freeze();
        while !frames.is_empty() {
            self.send.reserve_capacity(frames.len());
            // The server may drop a connection that floods it; that ends the send.
            let Some(Ok(granted)) = std::future::poll_fn(|cx| self.send.poll_capacity(cx)).await
            else {
                return;
            };
            let chunk = frames.split_to(granted.min(frames.len()));
            if self.send.send_data(chunk, false).is_err() {
                return;
            }
        }
    }

    /// Keep sending `requests` in the background, then hold the connection
    /// open, silent.
    fn keep_sending(mut self, requests: Vec<ClientPayload>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            for batch in requests.chunks(250) {
                self.send_all(batch.to_vec()).await;
            }
            std::future::pending::<()>().await;
        })
    }
}

/// The reply side of a raw gRPC stream.
struct GrpcReplies {
    body: Option<h2::RecvStream>,
    buf: bytes::BytesMut,
}

impl GrpcReplies {
    async fn next(&mut self) -> ServerMessage {
        use prost::Message;
        loop {
            if self.buf.len() >= 5 {
                let len = u32::from_be_bytes(self.buf[1..5].try_into().expect("4 bytes")) as usize;
                if self.buf.len() >= 5 + len {
                    let frame = self.buf.split_to(5 + len);
                    return ServerMessage::decode(&frame[5..]).expect("a ServerMessage");
                }
            }
            let chunk = tokio::time::timeout(
                Duration::from_secs(10),
                self.body.as_mut().expect("response").data(),
            )
            .await
            .expect("a server message")
            .expect("stream open")
            .expect("no stream error");
            self.body
                .as_mut()
                .expect("response")
                .flow_control()
                .release_capacity(chunk.len())
                .expect("release capacity");
            self.buf.extend_from_slice(&chunk);
        }
    }
}

fn text_node(content: &str) -> proto::TextMarkdownNodeProto {
    proto::TextMarkdownNodeProto {
        content: content.to_string(),
        bounds: Some(proto::Rect {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 40.0,
        }),
        font_size_px: 16.0,
        color: Some(proto::Rgba {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        }),
        ..Default::default()
    }
}

/// Minimal HTTP/1.0 POST: the MCP server answers and closes.
async fn post(addr: std::net::SocketAddr, psk: &str, body: String) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to MCP");
    let request = format!(
        "POST /mcp HTTP/1.0\r\nAuthorization: Bearer {psk}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.expect("read");
    raw.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .expect("HTTP response body")
}

fn input_items(reply: &Reply) -> &Vec<Value> {
    reply.body["items"].as_array().expect("items array")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_portal_attach_stream_reply_detach() {
    let mut poc = Poc::boot().await;
    let mut flow_tokens = 0;

    // The first publish attaches the portal; later ones stream output.
    for line in ["Running the test suite.", "All green. Ship it?"] {
        let reply = poc
            .call(
                "hud_publish",
                json!({"surface": PORTAL, "content": line, "expects_reply": true}),
            )
            .await;
        assert_eq!(reply.body, json!({"ok": true}));
        flow_tokens += reply.model_tokens;
    }
    assert_eq!(poc.hud.tile_count(), 1, "the portal is on screen");

    // The user clicks into the composer and types. The draft echoes locally,
    // before any further event-loop turn or MCP call.
    let (x, y) = poc
        .hud
        .composer_center()
        .expect("expects_reply arms a composer");
    poc.hud.click(x, y);
    poc.hud.type_text("yes ship it");
    assert_eq!(poc.hud.composer_draft().as_deref(), Some("yes ship it"));
    poc.hud.press_named_key("Enter");
    poc.settle().await;

    // The reply reaches hud_input, and the ack removes it.
    let poll = poc.call("hud_input", json!({})).await;
    flow_tokens += poll.model_tokens;
    let items = input_items(&poll);
    assert_eq!(items.len(), 1, "one typed reply: {}", poll.body);
    assert_eq!(items[0]["s"], PORTAL);
    assert_eq!(items[0]["text"], "yes ship it");
    let id = items[0]["id"].clone();
    let ack = poc.call("hud_input", json!({"ack": [id]})).await;
    flow_tokens += ack.model_tokens;
    assert_eq!(ack.body, json!({"items": [], "remaining": 0}));

    let clear = poc.call("hud_clear", json!({"surface": PORTAL})).await;
    flow_tokens += clear.model_tokens;
    assert_eq!(clear.body, json!({"ok": true}));
    assert_eq!(poc.hud.tile_count(), 0, "detach removes the portal");
    assert_eq!(poc.surface(PORTAL).await, None);

    assert!(
        flow_tokens <= PORTAL_FLOW_BUDGET,
        "portal flow took {flow_tokens} model-visible tokens (budget {PORTAL_FLOW_BUDGET})"
    );
}

/// docs/api.md "Viewer dismiss": after the viewer closes a portal, the agent's
/// cached holding is dead. `hud_hold` is NOT_HELD, `hud_clear` is an ok no-op,
/// and the next `hud_publish` attaches a fresh portal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_portal_viewer_dismiss_then_mcp_verbs() {
    let publish = json!({"surface": PORTAL, "content": "Working."});
    let mut poc = Poc::boot().await;
    poc.call("hud_publish", publish.clone()).await;
    assert_eq!(poc.hud.tile_count(), 1);

    poc.hud.viewer_dismiss_all_tiles();
    poc.settle().await;
    assert_eq!(poc.hud.tile_count(), 0, "the viewer's dismiss is immediate");

    let hold = poc
        .call_err("hud_hold", json!({"surface": PORTAL, "ttl_ms": 1_000}))
        .await;
    assert_eq!(hold, "NOT_HELD");

    // hud_hold dropped the stale holding; re-attach, dismiss again, and prove
    // hud_clear is a no-op success on a stale (still cached) token.
    poc.call("hud_publish", publish.clone()).await;
    assert_eq!(
        poc.hud.tile_count(),
        1,
        "publish re-attaches a fresh portal"
    );
    poc.hud.viewer_dismiss_all_tiles();
    poc.settle().await;
    let clear = poc.call("hud_clear", json!({"surface": PORTAL})).await;
    assert_eq!(clear.body, json!({"ok": true}));

    let again = poc.call("hud_publish", publish).await;
    assert_eq!(again.body, json!({"ok": true}));
    assert_eq!(
        poc.hud.tile_count(),
        1,
        "publish after clear attaches fresh"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_portal_abandoned_is_reclaimed() {
    let mut poc = Poc::boot().await;
    let attach = poc
        .call(
            "hud_publish",
            json!({"surface": PORTAL, "content": "Thinking."}),
        )
        .await;
    assert!(attach.model_tokens <= PORTAL_FLOW_BUDGET);
    assert_eq!(poc.hud.tile_count(), 1);

    // The agent goes silent: no publish, poll, or hold.
    poc.advance(PORTAL_DEGRADE_MS - 1).await;
    let live = poc.surface(PORTAL).await.expect("portal listed");
    assert_ne!(live["state"], "degraded", "{live}");

    poc.advance(1).await;
    let degraded = poc.surface(PORTAL).await.expect("portal listed");
    assert_eq!(degraded["state"], "degraded", "{degraded}");
    assert_eq!(poc.hud.tile_count(), 1, "degraded is still shown");

    poc.advance(PORTAL_RECLAIM_MS - PORTAL_DEGRADE_MS).await;
    assert_eq!(poc.hud.tile_count(), 0, "the runtime reclaimed the portal");
    assert_eq!(poc.surface(PORTAL).await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_zone_notification_ttl_disappears_unattended() {
    let mut poc = Poc::boot().await;
    let publish = poc
        .call(
            "hud_publish",
            json!({
                "surface": NOTIFICATION,
                "content": {"title": "Build", "body": "main is green"},
                "ttl_ms": 8_000,
            }),
        )
        .await;
    assert_eq!(publish.body, json!({"ok": true, "expires_in_ms": 8_000}));
    assert!(
        publish.model_tokens <= ZONE_PUBLISH_BUDGET,
        "zone publish took {} model-visible tokens (budget {ZONE_PUBLISH_BUDGET})",
        publish.model_tokens
    );
    let held = poc.surface(NOTIFICATION).await.expect("zone listed");
    assert_eq!(held["held"], true);
    assert_eq!(poc.shown(NOTIFICATION), 1);

    // Nobody calls again: the runtime alone takes it down at the TTL.
    poc.advance(7_999).await;
    assert_eq!(poc.shown(NOTIFICATION), 1);
    poc.advance(1).await;
    assert_eq!(poc.shown(NOTIFICATION), 0);
    let released = poc.surface(NOTIFICATION).await.expect("zone listed");
    assert_eq!(released.get("held"), None, "{released}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_zone_delay_ms_appears_on_schedule() {
    let mut poc = Poc::boot().await;
    let publish = poc
        .call(
            "hud_publish",
            json!({"surface": SUBTITLE, "content": "Standup in 2 minutes", "delay_ms": 2_000, "ttl_ms": 5_000}),
        )
        .await;
    // The expiry counts from presentation.
    assert_eq!(publish.body, json!({"ok": true, "expires_in_ms": 7_000}));
    assert!(publish.model_tokens <= ZONE_PUBLISH_BUDGET);

    // Arrival is not presentation (invariant 1).
    assert_eq!(poc.shown(SUBTITLE), 0);
    poc.advance(1_999).await;
    assert_eq!(poc.shown(SUBTITLE), 0);
    poc.advance(1).await;
    assert_eq!(poc.shown(SUBTITLE), 1);
    poc.advance(4_999).await;
    assert_eq!(poc.shown(SUBTITLE), 1);
    poc.advance(1).await;
    assert_eq!(poc.shown(SUBTITLE), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_notification_action_reaches_hud_input() {
    let mut poc = Poc::boot().await;
    let publish = poc
        .call(
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
            }),
        )
        .await;
    assert!(publish.model_tokens <= ZONE_PUBLISH_BUDGET);

    let (x, y) = poc
        .hud
        .notification_action_center("ship")
        .expect("the Ship button is on screen");
    poc.hud.click(x, y);
    poc.settle().await;

    let poll = poc.call("hud_input", json!({})).await;
    let items = input_items(&poll);
    assert_eq!(items.len(), 1, "one action press: {}", poll.body);
    assert_eq!(items[0]["s"], NOTIFICATION);
    assert_eq!(items[0]["action"], "ship");
    let id = items[0]["id"].clone();
    let ack = poc.call("hud_input", json!({"ack": [id]})).await;
    assert_eq!(ack.body, json!({"items": [], "remaining": 0}));
    // Publish, poll, and ack fit the same per-stage budgets as the portal loop.
    assert!(publish.model_tokens + poll.model_tokens + ack.model_tokens <= PORTAL_FLOW_BUDGET);
}

/// docs/scope.md "Tiles": a resident gRPC agent, authenticated as its own
/// `agents.toml` identity, gets a placed, filled tile in two round trips, and
/// the runtime resolves the placement hint (the agent sends no geometry).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_tile_claim_with_placement_two_round_trips() {
    let poc = Poc::boot().await;

    // The claude PSK is not the resident's credential.
    let wrong = Resident::connect(poc.hud.grpc_addr(), "resident", CLAUDE_PSK).await;
    assert!(
        wrong.is_err(),
        "another agent's PSK must not open a session"
    );

    // Round trip 1: the handshake. Round trip 2: claim and fill.
    let mut agent = poc.resident().await;
    assert_eq!(agent.namespace, "resident");
    let claimed = agent
        .claim(
            wire::TileAnchor::BottomLeft,
            wire::TileSize::Large,
            "build running",
        )
        .await;
    assert_eq!(
        agent.round_trips, 2,
        "init to a filled tile is two round trips"
    );
    assert!(!claimed.lease.is_empty());

    assert_eq!(poc.hud.tile_texts(), ["build running"]);
    let (tokens, window) = (TilePlacementTokens::default(), runtime_config().window);
    let (width, height) = tokens.size(tze_hud_scene::placement::TileSize::Large);
    let bounds = poc.hud.tile_bounds();
    assert_eq!(bounds.len(), 1);
    assert_eq!(
        (bounds[0].x, bounds[0].y, bounds[0].width, bounds[0].height),
        (
            tokens.margin,
            window.height as f32 - tokens.margin - height,
            width,
            height
        ),
        "bottom-left, large, inside the screen margin"
    );

    // A second claim at the same anchor stacks away from the edge.
    let second = agent
        .claim(wire::TileAnchor::BottomLeft, wire::TileSize::Large, "tests")
        .await;
    assert_ne!(second.tile, claimed.tile);
    let mut stacked = poc.hud.tile_bounds();
    stacked.sort_by(|a, b| a.y.total_cmp(&b.y));
    assert_eq!(
        stacked[1].y - (stacked[0].y + stacked[0].height),
        tokens.gap
    );
}

/// docs/scope.md "Tiles": updates show, a dropped session orphans the tile
/// (content kept, badge shown), and the runtime reclaims it when the grace
/// period ends, with no agent and no wall clock involved. The same agent's MCP
/// publication, held under a different lease, survives the reap (#1250).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_tile_update_then_orphan_then_reclaim_after_grace() {
    let mut poc = Poc::boot().await;
    let notice = poc
        .call_as(
            RESIDENT_PSK,
            "hud_publish",
            json!({"surface": NOTIFICATION, "content": {"title": "Deploy", "body": "finished"}, "ttl_ms": 600_000}),
        )
        .await;
    assert!(
        notice.model_tokens <= ZONE_PUBLISH_BUDGET,
        "{}",
        notice.model_tokens
    );
    assert_eq!(poc.shown(NOTIFICATION), 1);

    let mut agent = poc.resident().await;
    let claimed = agent
        .claim(
            wire::TileAnchor::TopRight,
            wire::TileSize::Medium,
            "building",
        )
        .await;
    let update = agent.set_text(&claimed, "tests passed").await;
    assert!(update.ok, "{update:?}");
    assert_eq!(
        poc.hud.tile_texts(),
        ["tests passed"],
        "the update is on screen"
    );

    // The agent vanishes: no SessionClose.
    drop(agent);
    poc.wait_for("orphan badge", |hud| hud.badged_tile_count() == 1)
        .await;
    assert_eq!(
        poc.hud.tile_texts(),
        ["tests passed"],
        "orphaned content is kept"
    );

    poc.advance(ORPHAN_GRACE_PERIOD_MS - 1).await;
    assert_eq!(poc.hud.tile_count(), 1, "still within grace");
    poc.advance(1).await;
    assert_eq!(poc.hud.tile_count(), 0, "reclaimed when grace ends");
    assert_eq!(
        poc.shown(NOTIFICATION),
        1,
        "the MCP notification survives the reap"
    );
}

impl Poc {
    /// Turn the event loop until the server's send buffer to `agent` is full:
    /// the agent has stopped reading and its next reply blocks its handler.
    async fn wait_until_hung(&mut self, agent: &str) {
        for _ in 0..200_000 {
            if self.hud.session_backed_up(agent).await {
                return;
            }
            self.hud.tick();
            tokio::task::yield_now().await;
        }
        panic!("{agent} never backed up");
    }

    /// A `hud_input` long-poll the operator never reads: parked in the MCP
    /// server for the whole test.
    fn park_long_poll(&self) -> tokio::task::JoinHandle<String> {
        tokio::spawn(post(
            self.hud.mcp_addr(),
            CLAUDE_PSK,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "hud_input", "arguments": {"wait_ms": 30_000}},
            })
            .to_string(),
        ))
    }
}

/// docs/scope.md "Override": the human wins while an agent is hung. The agent
/// claims a tile, then sends requests it never reads replies to until the
/// server's send path to it is full (a few hundred ~2 KB replies). Safe mode
/// (the hotkey bridge) then suspends its lease and refuses MCP writes, even
/// with a `hud_input` long-poll parked; the viewer's dismiss removes its tile.
/// Neither waits on the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_override_safe_mode_wins_with_hung_grpc_agent() {
    let mut poc = Poc::boot().await;
    let hung = HungAgent::connect(poc.hud.grpc_addr()).await;
    assert_eq!(poc.hud.tile_count(), 1);
    let big_clear = || {
        ClientPayload::Clear(wire::Clear {
            surface: format!("zone:{}", "z".repeat(2_000)),
        })
    };
    let flood = hung.keep_sending((0..800).map(|_| big_clear()).collect());
    poc.wait_until_hung("resident").await;
    let poll = poc.park_long_poll();

    poc.hud.press_safe_mode_hotkey();
    poc.wait_for("safe mode", |hud| hud.safe_mode_active())
        .await;
    assert_eq!(
        poc.hud.suspended_lease_count(),
        1,
        "the hung agent's lease is suspended"
    );
    let code = poc
        .call_err("hud_publish", json!({"surface": SUBTITLE, "content": "hi"}))
        .await;
    assert_eq!(code, "SAFE_MODE_ACTIVE");
    assert!(!poll.is_finished(), "the long-poll is still parked");

    poc.hud.press_safe_mode_hotkey();
    poc.wait_for("safe mode off", |hud| !hud.safe_mode_active())
        .await;
    assert_eq!(poc.hud.suspended_lease_count(), 0);

    poc.hud.viewer_dismiss_all_tiles();
    assert_eq!(
        poc.hud.tile_count(),
        0,
        "dismiss does not wait for the agent"
    );
    poll.abort();
    flood.abort();
}

/// The other order: the agent hangs while safe mode is on, by flooding writes
/// the runtime refuses with `SAFE_MODE_ACTIVE`. Refusing them must not hold
/// the shared state while it waits on the agent, or the human could never
/// leave safe mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_override_exit_safe_mode_with_agent_hung_during_it() {
    let mut poc = Poc::boot().await;
    let hung = HungAgent::connect(poc.hud.grpc_addr()).await;
    poc.hud.press_safe_mode_hotkey();
    poc.wait_for("safe mode", |hud| hud.safe_mode_active())
        .await;

    let write = || {
        ClientPayload::MutationBatch(wire::MutationBatch {
            batch_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
            lease_id: vec![0; 16],
            mutations: vec![],
            timing: None,
        })
    };
    let flood = hung.keep_sending((0..8_000).map(|_| write()).collect());
    poc.wait_until_hung("resident").await;

    poc.hud.press_safe_mode_hotkey();
    poc.wait_for("safe mode off", |hud| !hud.safe_mode_active())
        .await;
    assert_eq!(poc.hud.suspended_lease_count(), 0, "the lease resumes");
    flood.abort();
}

/// A responsive agent is told what the human did (docs/api.md): safe mode
/// arrives as `SessionSuspended` and writes are refused with `SAFE_MODE_ACTIVE`
/// until `SessionResumed`; a viewer dismiss arrives as `Reclaimed{OVERRIDE}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_override_notifies_the_agent() {
    let mut poc = Poc::boot().await;
    let mut agent = poc.resident().await;
    let claimed = agent
        .claim(wire::TileAnchor::TopRight, wire::TileSize::Small, "working")
        .await;

    poc.hud.press_safe_mode_hotkey();
    match next_message(&mut agent.stream).await.payload {
        Some(ServerPayload::SessionSuspended(_)) => {}
        other => panic!("expected SessionSuspended, got {other:?}"),
    }
    let refused = agent.set_text(&claimed, "ignored").await;
    assert_eq!(refused.code, "SAFE_MODE_ACTIVE");

    poc.hud.press_safe_mode_hotkey();
    match next_message(&mut agent.stream).await.payload {
        Some(ServerPayload::SessionResumed(_)) => {}
        other => panic!("expected SessionResumed, got {other:?}"),
    }
    assert!(agent.set_text(&claimed, "back").await.ok);

    poc.hud.viewer_dismiss_all_tiles();
    poc.settle().await;
    match next_message(&mut agent.stream).await.payload {
        Some(ServerPayload::Reclaimed(reclaimed)) => {
            assert_eq!(reclaimed.why, wire::ReclaimReason::Override as i32);
            assert_eq!(reclaimed.lease_id, claimed.lease);
            assert!(reclaimed.surface.starts_with("tile:"), "{reclaimed:?}");
        }
        other => panic!("expected Reclaimed, got {other:?}"),
    }
    assert_eq!(poc.hud.tile_count(), 0);
}
