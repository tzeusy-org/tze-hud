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
use tze_hud_scene::{Clock, TestClock};

const PRODUCTION_CONFIG: &str = include_str!("../../app/tze_hud_app/config/production.toml");
const PORTAL: &str = "portal:claude-main";
const NOTIFICATION: &str = "zone:notification-area";
const SUBTITLE: &str = "zone:subtitle";

// docs/api.md "Token budgets" (model-visible tokens). Discover is the
// production.toml scene: 6 zones plus four widget instances (<=250 tokens).
const DISCOVER_BUDGET: usize = 250;
// Existing live-state allowance stays 40 tokens over the measured unheld
// scene (formerly 210+40=250, now 245+40=285). Only the new countdown
// row adds 35; no required holding/portal state is omitted to fit fresh discovery.
const DISCOVER_WITH_HOLDINGS_BUDGET: usize = 285;
const ZONE_PUBLISH_BUDGET: usize = 80;
const PORTAL_FLOW_BUDGET: usize = 250;

// Portal liveness window and lease grace (docs/api.md `hud_hold`).
const PORTAL_DEGRADE_MS: u64 = 30_000;
const PORTAL_RECLAIM_MS: u64 = 60_000;

/// Real-time cap on every wait in this file, and how often waits poll. These
/// bound the test's own patience (a regression fails instead of hanging); the
/// runtime under test still reads only the injected `TestClock`.
const WAIT: Duration = Duration::from_secs(45);
const POLL: Duration = Duration::from_millis(1);

/// Agent identity, in one place. The runtime is seeded with the directory an
/// `agents.toml` describes (agents paired by the SHA-256 of their PSK), and MCP
/// calls present the PSK as the bearer. `claude` (allow = ["*"]) is the
/// operator.
const CLAUDE_PSK: &str = "poc-acceptance-psk";

/// The resident zone agent: its own `agents.toml` entry, allowed the status-bar
/// and notification zones. It is a different identity (and namespace) from
/// `claude`, the MCP operator.
const RESIDENT_PSK: &str = "poc-acceptance-resident-psk";

fn runtime_config() -> WindowedConfig {
    WindowedConfig {
        agents: tze_hud_config::agents_file::AgentsFile::default()
            .with_agent("claude", CLAUDE_PSK, &["*"])
            .with_agent(
                "resident",
                RESIDENT_PSK,
                &["zone:status-bar", "zone:notification-area"],
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
    model_bytes: usize,
    wire_tokens: usize,
    wire_bytes: usize,
}

struct Poc {
    hud: HeadlessEventLoopHarness,
    clock: TestClock,
    /// The scratch `agents.toml` the control panel edits, if any.
    agents_file: Option<(tempfile::TempDir, std::path::PathBuf)>,
}

/// `runtime_config`'s agents as an `agents.toml`.
fn agents_file() -> tze_hud_config::AgentsFile {
    tze_hud_config::agents_file::AgentsFile::default()
        .with_agent("claude", CLAUDE_PSK, &["*"])
        .with_agent(
            "resident",
            RESIDENT_PSK,
            &["zone:status-bar", "zone:notification-area"],
        )
}

impl Poc {
    async fn boot() -> Self {
        Self::boot_with(None).await
    }

    /// Boot with the agents also in a scratch `agents.toml`, so the control
    /// panel can unpair them and edit their allow lists.
    async fn boot_with_agents_file() -> Self {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = dir.path().join("agents.toml");
        for (agent, record) in agents_file().agents {
            tze_hud_config::agents_file::edit(
                &path,
                &tze_hud_config::AgentChange::Pair { agent, record },
            )
            .expect("agents.toml");
        }
        Self::boot_with(Some((dir, path))).await
    }

    async fn boot_with(agents_file: Option<(tempfile::TempDir, std::path::PathBuf)>) -> Self {
        Self::boot_from(runtime_config(), agents_file).await
    }

    async fn boot_from(
        cfg: WindowedConfig,
        agents_file: Option<(tempfile::TempDir, std::path::PathBuf)>,
    ) -> Self {
        let start_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as u64;
        let clock = TestClock::new(start_ms);
        let hud = HeadlessEventLoopHarness::with_network_and_agents_file(
            cfg,
            Arc::new(clock.clone()),
            agents_file.as_ref().map(|(_, path)| path.clone()),
        )
        .await
        .expect("boot the runtime from production.toml");
        let mut poc = Poc {
            hud,
            clock,
            agents_file,
        };
        poc.settle().await;
        poc
    }

    fn paired(&self) -> Vec<String> {
        let (_, path) = self
            .agents_file
            .as_ref()
            .expect("booted with an agents file");
        tze_hud_config::agents_file::load(path)
            .expect("agents.toml")
            .agents
            .into_keys()
            .collect()
    }

    /// Open the panel (agents keep running) if it is closed.
    async fn open_panel(&mut self) {
        if !self.hud.control_panel_open() {
            self.hud
                .press_chord(tze_hud_scene::config::ChordRole::Panel);
            self.wait_for("the panel opens", |hud| hud.control_panel_open())
                .await;
        }
    }

    /// Click panel row `row` once it is listed, then let the panel apply it.
    async fn click(&mut self, row: &str) {
        self.wait_for(&format!("row {row}"), |hud| {
            hud.control_panel_row(row).is_some()
        })
        .await;
        self.hud.control_panel_click(row);
    }

    /// Click a confirmed action or grant twice: the first click asks, the
    /// second does it.
    async fn click_confirmed(&mut self, row: &str, question: &str) {
        self.click(row).await;
        self.wait_for(&format!("{row} asks"), |hud| {
            hud.control_panel_row(row)
                .and_then(|item| item.detail)
                .is_some_and(|detail| detail.contains(question))
        })
        .await;
        self.hud.control_panel_click(row);
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
        let request_bytes = request.len();
        let request_tokens = tokens(&request);
        let mut call = tokio::spawn(post(self.hud.mcp_addr(), psk, request));
        let deadline = Instant::now() + WAIT;
        let response = loop {
            if tool != "hud_input" || arguments["wait_ms"].as_u64().unwrap_or(0) > 0 {
                self.hud.tick();
            }
            tokio::select! {
                biased;
                done = &mut call => break done.expect("MCP call task"),
                () = tokio::task::yield_now() => {}
            }
            assert!(Instant::now() < deadline, "{tool} did not complete");
        };
        self.settle().await;

        let wire_bytes = request_bytes + response.len();
        let wire_tokens = request_tokens + tokens(&response);
        let response: Value = serde_json::from_str(&response).expect("JSON-RPC response");
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: no text result in {response}"));
        assert_ne!(result["isError"], json!(true), "{tool} failed: {text}");
        Reply {
            body: serde_json::from_str(text).expect("result text is JSON"),
            model_tokens: tokens(&format!("{tool}{arguments}{text}")),
            model_bytes: format!("{tool}{arguments}{text}").len(),
            wire_tokens,
            wire_bytes,
        }
    }

    /// Call an MCP tool that must fail; retain the actual model-visible bytes.
    async fn call_error_body(&mut self, tool: &str, arguments: Value) -> Value {
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        })
        .to_string();
        let mut call = tokio::spawn(post(self.hud.mcp_addr(), CLAUDE_PSK, request));
        let deadline = Instant::now() + WAIT;
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
        assert_eq!(
            result["isError"],
            json!(true),
            "{tool} should fail: {response}"
        );
        let text = result["content"][0]["text"].as_str().expect("error text");
        let body: Value = serde_json::from_str(text).expect("error text is JSON");
        body
    }

    async fn call_err(&mut self, tool: &str, arguments: Value) -> String {
        self.call_error_body(tool, arguments).await["code"]
            .as_str()
            .expect("error code")
            .to_string()
    }

    /// `hud_surfaces`, returning this agent's entry for `surface` (if listed)
    /// after checking the discover budget.
    async fn surface(&mut self, surface: &str) -> Option<Value> {
        let reply = self.call("hud_surfaces", json!({})).await;
        assert!(
            reply.model_tokens <= DISCOVER_WITH_HOLDINGS_BUDGET,
            "discover took {} model-visible tokens (budget {DISCOVER_WITH_HOLDINGS_BUDGET})",
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
        let deadline = Instant::now() + WAIT;
        while !self.hud.tick() {
            assert!(Instant::now() < deadline, "event loop never settled");
            tokio::time::sleep(POLL).await;
        }
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
        let deadline = Instant::now() + WAIT;
        loop {
            self.hud.tick();
            if ready(&self.hud) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: not reached within {WAIT:?}"
            );
            tokio::time::sleep(POLL).await;
        }
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
    /// The handshake: `SessionInit` out, `SessionEstablished` and the current
    /// degradation level back. Errors with the `SessionError` code.
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
        match next_message(&mut stream).await.payload {
            Some(ServerPayload::SessionEstablished(_)) => {}
            Some(ServerPayload::SessionError(e)) => return Err(e.code),
            other => panic!("expected SessionEstablished, got {other:?}"),
        };
        // The current degradation level follows in the same round trip.
        assert!(matches!(
            next_message(&mut stream).await.payload,
            Some(ServerPayload::DegradationNotice(_))
        ));
        Ok(Resident {
            tx,
            stream,
            sequence: 1,
            round_trips: 1,
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

    /// Replace the resident's durable status publication.
    async fn publish(&mut self, text: &str) -> wire::RequestResult {
        self.request(resident_publish(text)).await
    }
}

fn resident_publish(text: &str) -> ClientPayload {
    ClientPayload::Publish(wire::Publish {
        surface: "zone:status-bar".into(),
        content: Some(proto::ZoneContent {
            payload: Some(proto::zone_content::Payload::StatusBar(
                proto::StatusBarPayload {
                    entries: std::collections::HashMap::from([("resident".into(), text.into())]),
                },
            )),
        }),
        ..Default::default()
    })
}

/// A resident agent whose process has stopped. A raw HTTP/2 client: it
/// handshakes and publishes durable status, and from then on never reads (or releases
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
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(
                    RESIDENT_PSK.to_string(),
                )),
                ..Default::default()
            })])
            .await;
        agent.replies.body = Some(response.await.expect("response headers").into_body());
        for _ in 0..2 {
            agent.replies.next().await; // established, current degradation level
        }
        agent.send_all(vec![resident_publish("working")]).await;
        match agent.replies.next().await.payload {
            Some(ServerPayload::RequestResult(result)) => assert!(result.ok, "{result:?}"),
            other => panic!("expected RequestResult, got {other:?}"),
        }
        agent
    }

    /// Send `requests`, packed into as few HTTP/2 DATA frames as flow control
    /// allows (a server drops a client that floods it with tiny frames), waiting
    /// for capacity. Never reads a reply.
    async fn send_all(&mut self, requests: Vec<ClientPayload>) -> bool {
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
                return false;
            };
            let chunk = frames.split_to(granted.min(frames.len()));
            if self.send.send_data(chunk, false).is_err() {
                return false;
            }
        }
        true
    }

    /// Keep sending `request()`s in the background until aborted. Once the
    /// server stops reading (its handler is blocked on a full send buffer)
    /// flow control parks the sender, so this stops by itself.
    fn keep_sending(
        mut self,
        request: impl Fn() -> ClientPayload + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(
            async move { while self.send_all((0..250).map(|_| request()).collect()).await {} },
        )
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

/// `POST /pair` as an agent following the Connect prompt sends it.
async fn post_pair(addr: std::net::SocketAddr, body: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to MCP");
    let request = format!(
        "POST /pair HTTP/1.0\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
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

/// Production model discovery and rejections must be useful within one call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_widget_discovery_and_errors_are_self_sufficient() {
    // Measure the previous production instance set through the same public
    // handler, preserving every other config field and the actual serializer.
    let mut prior_config = runtime_config();
    let mut prior: toml::Value = PRODUCTION_CONFIG.parse().unwrap();
    prior["tabs"][0]["widgets"]
        .as_array_mut()
        .unwrap()
        .retain(|widget| widget["widget_type"].as_str() != Some("countdown"));
    prior_config.config_toml = Some(toml::to_string(&prior).unwrap());
    let mut previous = Poc::boot_from(prior_config, None).await;
    let old_discovery = previous.call("hud_surfaces", json!({})).await;
    println!(
        "production_discovery_old={}",
        json!({"model_tokens":old_discovery.model_tokens,"model_bytes":old_discovery.model_bytes,"wire_tokens":old_discovery.wire_tokens,"wire_bytes":old_discovery.wire_bytes,"body":old_discovery.body})
    );
    assert_eq!(old_discovery.model_tokens, 210);
    drop(previous);
    let mut poc = Poc::boot().await;
    let discovery = poc.call("hud_surfaces", json!({})).await;
    let mut retained = discovery.body.clone();
    retained["surfaces"]
        .as_array_mut()
        .unwrap()
        .retain(|surface| surface["s"] != "widget:main-countdown");
    assert_eq!(
        retained, old_discovery.body,
        "all prior model parameters and occupancy hints survive"
    );
    assert!(
        discovery.model_tokens <= DISCOVER_BUDGET,
        "{} tokens: {}",
        discovery.model_tokens,
        discovery.body
    );
    println!(
        "production_discovery={}",
        json!({"model_tokens":discovery.model_tokens,"model_bytes":discovery.model_bytes,"wire_tokens":discovery.wire_tokens,"wire_bytes":discovery.wire_bytes,"body":discovery.body})
    );
    let surfaces = discovery.body["surfaces"].as_array().unwrap();
    let widget = |name: &str| surfaces.iter().find(|s| s["s"] == name).unwrap();
    assert_eq!(
        widget("widget:main-countdown")["params"]["ends_in_ms"],
        "deadline<=24h"
    );
    assert_eq!(
        surfaces
            .iter()
            .filter(|surface| surface["s"]
                .as_str()
                .is_some_and(|name| name.starts_with("widget:")))
            .count(),
        4
    );
    assert!(surfaces.iter().all(|surface| {
        !["widget:kpi", "widget:stepper"].contains(&surface["s"].as_str().unwrap())
    }));
    for surface in surfaces {
        if let Some(params) = surface["params"].as_object() {
            for hidden in ["fill_color", "theme", "tooltip_visible"] {
                assert!(!params.contains_key(hidden), "{surface}");
            }
        }
        if let Some(accepts) = surface["accepts"].as_str() {
            assert!(!accepts.contains("static_image"));
        }
    }
    assert_eq!(
        widget("widget:main-progress")["params"]["severity"],
        "enum info|warning|error"
    );
    assert_eq!(
        widget("widget:main-progress")["params"]["label"],
        "string<=32"
    );
    assert_eq!(
        widget("widget:main-status")["params"]["reason"],
        "string<=160"
    );
    for (surface, params, hint) in [
        (
            "widget:main-gauge",
            json!({"level":0.8,"fill_color":"wrong-type"}),
            "owner-set",
        ),
        (
            "widget:main-status",
            json!({"status":"online","theme":"friendly"}),
            "owner-set",
        ),
        (
            "widget:main-progress",
            json!({"progress":0.8,"tooltip_visible":1}),
            "runtime-set",
        ),
        ("widget:main-gauge", json!({"level":-0.1}), "0..1"),
        ("widget:main-gauge", json!({"level":"high"}), "f32"),
        (
            "widget:main-gauge",
            json!({"severity":"critical"}),
            "info|warning|error",
        ),
        (
            "widget:main-progress",
            json!({"severity":3}),
            "info|warning|error",
        ),
        ("widget:main-status", json!({"reason":false}), "160"),
        (
            "widget:main-countdown",
            json!({"ends_in_ms":-1}),
            "deadline<=24h",
        ),
        (
            "widget:main-countdown",
            json!({"ends_in_ms":1.5}),
            "deadline<=24h",
        ),
        (
            "widget:main-countdown",
            json!({"ends_in_ms":86400001}),
            "deadline<=24h",
        ),
    ] {
        let error = poc
            .call_error_body("hud_publish", json!({"surface":surface,"params":params}))
            .await;
        assert_eq!(error["code"], "WIDGET_PARAMETER_INVALID");
        assert!(error["hint"].as_str().unwrap().contains(hint), "{error}");
        assert!(
            tokens(&error.to_string()) <= 60,
            "error over budget: {error}"
        );
    }
    println!(
        "production discovery: {} model-visible tokens; {}",
        discovery.model_tokens, discovery.body
    );
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
        assert_eq!(
            reply.body,
            json!({"ok": true, "reattached": false, "pending_input": 0})
        );
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

/// §10 / spec R5.1: the hide chord pressed while typing in a portal
/// composer hides everything and blurs it, so keys can never reach a hidden
/// surface; pressed while paused it hides and shows without leaving pause.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_hide_chord_blurs_the_composer_and_works_while_paused() {
    use tze_hud_scene::config::ChordRole;

    let mut poc = Poc::boot().await;
    poc.call(
        "hud_publish",
        json!({"surface": PORTAL, "content": "Ship it?", "expects_reply": true}),
    )
    .await;
    let (x, y) = poc.hud.composer_center().expect("a composer");
    poc.hud.click(x, y);
    poc.hud.type_text("draft");
    assert_eq!(poc.hud.composer_draft().as_deref(), Some("draft"));

    poc.hud.press_chord(ChordRole::Hide);
    poc.wait_for("everything hides", |hud| hud.all_hidden())
        .await;
    poc.wait_for("the composer blurs", |hud| hud.composer_draft().is_none())
        .await;
    poc.hud.type_text("lost");
    poc.settle().await;
    assert_eq!(
        poc.hud.composer_draft(),
        None,
        "no keys reach a hidden composer"
    );
    poc.hud.press_chord(ChordRole::Hide);
    poc.wait_for("everything shows", |hud| !hud.all_hidden())
        .await;

    poc.hud.press_safe_mode_hotkey();
    poc.wait_for("paused", |hud| hud.safe_mode_active()).await;
    poc.hud.press_chord(ChordRole::Hide);
    poc.wait_for("hidden while paused", |hud| hud.all_hidden())
        .await;
    assert!(poc.hud.safe_mode_active(), "hiding never resumes");
    poc.hud.press_chord(ChordRole::Hide);
    poc.wait_for("shown while paused", |hud| !hud.all_hidden())
        .await;
    assert!(poc.hud.safe_mode_active(), "showing never resumes");
    assert_eq!(poc.hud.tile_count(), 1, "hiding keeps the portal");
}

/// docs/api.md "Viewer dismiss": after the viewer closes a portal, the agent's
/// cached holding is dead. `hud_hold` is NOT_HELD, `hud_clear` is an ok no-op,
/// and the next `hud_publish` attaches a fresh portal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_portal_viewer_dismiss_then_mcp_verbs() {
    let publish = json!({"surface": PORTAL, "content": "Working."});
    let mut poc = Poc::boot().await;
    let first = poc.call("hud_publish", publish.clone()).await;
    assert_eq!(
        first.body,
        json!({"ok": true, "reattached": false, "pending_input": 0})
    );
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
    let reattached = poc.call("hud_publish", publish.clone()).await;
    assert_eq!(
        reattached.body,
        json!({"ok": true, "reattached": true, "pending_input": 0})
    );
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
    assert_eq!(
        again.body,
        json!({"ok": true, "reattached": false, "pending_input": 0})
    );
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

    assert!(
        poc.hud.notification_action_center("ship").is_none(),
        "chosen exit is immediately noninteractive"
    );
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

    // Each callback is independently republished and clicked through the real
    // runtime, then polled/ACKed through authenticated MCP. The original Ship
    // transaction and its budget above remain intact.
    for callback in ["approve", "reject"] {
        let foreign = poc
            .call_as(
                RESIDENT_PSK,
                "hud_publish",
                json!({
                    "surface": NOTIFICATION,
                    "content": {"title": "Other", "body": "Unclicked",
                        "actions": [{"label": "Other", "callback_id": "foreign"}]},
                }),
            )
            .await;
        assert!(foreign.model_tokens <= ZONE_PUBLISH_BUDGET);
        let replaced = poc
            .call(
                "hud_publish",
                json!({
                    "surface": NOTIFICATION,
                    "content": {"title": "Old", "body": "Unclicked",
                        "actions": [{"label": "Old", "callback_id": "replaced"}]},
                }),
            )
            .await;
        assert!(replaced.model_tokens <= ZONE_PUBLISH_BUDGET);
        // Clear-and-republish is an actual holding replacement, not an edit to
        // a scene fixture. Clear affects this publisher only, leaving Other.
        poc.call("hud_clear", json!({"surface": NOTIFICATION}))
            .await;
        poc.advance(1).await;
        let publish = poc
            .call(
                "hud_publish",
                json!({
                    "surface": NOTIFICATION,
                    "content": {"title": "Deploy", "body": "Ship v2?",
                        "actions": [{"label": "Approve", "callback_id": "approve"},
                                    {"label": "Reject", "callback_id": "reject"},
                                    {"label": "Docs", "url": "https://example.com/poc"}]},
                }),
            )
            .await;
        assert!(publish.model_tokens <= ZONE_PUBLISH_BUDGET);
        assert_eq!(
            poc.shown(NOTIFICATION),
            2,
            "foreign holding survives own replacement"
        );
        assert!(input_items(&poc.call("hud_input", json!({})).await).is_empty());
        assert!(input_items(&poc.call_as(RESIDENT_PSK, "hud_input", json!({})).await).is_empty());
        let (lx, ly) = poc
            .hud
            .notification_link_center("https://example.com/poc")
            .unwrap();
        poc.hud.click(lx, ly);
        poc.settle().await;
        assert!(
            poc.hud
                .notification_link_center("https://example.com/poc")
                .is_some()
        );
        assert!(input_items(&poc.call("hud_input", json!({})).await).is_empty());
        assert!(input_items(&poc.call_as(RESIDENT_PSK, "hud_input", json!({})).await).is_empty());
        let (x, y) = poc.hud.notification_action_center(callback).unwrap();
        poc.hud.click(x, y);
        poc.settle().await;
        let poll = poc.call("hud_input", json!({})).await;
        let items = input_items(&poll);
        assert_eq!(
            items.len(),
            1,
            "only the intentional {callback}: {}",
            poll.body
        );
        assert_eq!(items[0]["s"], NOTIFICATION);
        assert_eq!(items[0]["action"], callback);
        let id = items[0]["id"].clone();
        let foreign_poll = poc
            .call_as(RESIDENT_PSK, "hud_input", json!({"ack": [id.clone()]}))
            .await;
        assert!(
            input_items(&foreign_poll).is_empty(),
            "no foreign action or cross-owner ACK"
        );
        let repeated = poc.call("hud_input", json!({})).await;
        assert_eq!(
            input_items(&repeated),
            items,
            "same retained event and id until ACK"
        );
        let ack = poc.call("hud_input", json!({"ack": [id]})).await;
        assert_eq!(ack.body, json!({"items": [], "remaining": 0}));
        assert!(input_items(&poc.call("hud_input", json!({})).await).is_empty());
        assert!(publish.model_tokens + poll.model_tokens + ack.model_tokens <= PORTAL_FLOW_BUDGET);
        poc.call_as(RESIDENT_PSK, "hud_clear", json!({"surface": NOTIFICATION}))
            .await;
        let tokens = std::collections::HashMap::from([(
            "motion.exit.ms".to_string(),
            poc.hud
                .design_token("motion.exit.ms")
                .expect("resolved exit token"),
        )]);
        let exit_ms = tze_hud_config::tokens::resolve_motion_duration_ms(&tokens, "motion.exit.ms");
        poc.advance(u64::from(exit_ms) + 1).await;
        assert!(
            input_items(&poc.call("hud_input", json!({})).await).is_empty(),
            "chosen completion emits no human X"
        );
        assert_eq!(poc.shown(NOTIFICATION), 0);
    }

    // Additive keyed path leaves the original unkeyed budget fixture above intact.
    poc.call("hud_publish", json!({"surface":NOTIFICATION,"key":"question:session","ttl_ms":0,"content":{"body":"Keyed?","actions":[{"label":"Approve","callback_id":"keyed-approve"}]}})).await;
    let (x, y) = poc.hud.notification_action_center("keyed-approve").unwrap();
    poc.hud.click(x, y);
    poc.settle().await;
    poc.call("hud_publish", json!({"surface":NOTIFICATION,"key":"question:session","ttl_ms":0,"content":{"body":"Replacement"}})).await;
    poc.call(
        "hud_clear",
        json!({"surface":NOTIFICATION,"key":"question:session"}),
    )
    .await;
    let accepted = poc.call("hud_input", json!({})).await;
    let items = input_items(&accepted);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["key"], "question:session");
    assert_eq!(items[0]["action"], "keyed-approve");
    let repeated = poc.call("hud_input", json!({})).await;
    assert_eq!(input_items(&repeated), items);
    let ack = poc.call("hud_input", json!({"ack":[items[0]["id"]]})).await;
    assert!(input_items(&ack).is_empty());
}

impl Poc {
    /// Turn the event loop until the server's send buffer to `agent` is full:
    /// the agent has stopped reading and its next reply blocks its handler.
    async fn wait_until_hung(&mut self, agent: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            // A handler wedged holding the shared state would hang this lock.
            let backed_up = tokio::time::timeout(WAIT, self.hud.session_backed_up(agent)).await;
            match backed_up {
                Ok(true) => return,
                Ok(false) => {}
                Err(_) => panic!("shared state locked for {WAIT:?} while waiting on {agent}"),
            }
            assert!(
                Instant::now() < deadline,
                "{agent} never backed up within {WAIT:?}"
            );
            self.hud.tick();
            tokio::time::sleep(POLL).await;
        }
    }

    /// Wait for this real HTTP request's authoritative, armed empty-queue wait
    /// before the owner starts ticking or sending hotkeys. The returned closure
    /// owns the observer without borrowing this fixture.
    async fn park_long_poll(&self) -> (tokio::task::JoinHandle<String>, impl Fn() -> bool + use<>) {
        let mut probe = self.hud.observe_input_wait("claude");
        let mut poll = tokio::spawn(post(
            self.hud.mcp_addr(),
            CLAUDE_PSK,
            json!({
                "jsonrpc": "2.0", "id": probe.request_id(), "method": "tools/call",
                "params": {"name": "hud_input", "arguments": {"wait_ms": 30_000}},
            })
            .to_string(),
        ));
        let armed = tokio::time::timeout(WAIT, async {
            tokio::select! {
                biased;
                joined = &mut poll => panic!("hud_input returned before its armed wait: {joined:?}"),
                waiting = probe.wait_until_waiting() => {
                    if !waiting {
                        let joined = (&mut poll).await;
                        panic!("hud_input closed before its armed wait: {joined:?}");
                    }
                }
            }
        }).await;
        if armed.is_err() {
            poll.abort();
            panic!("hud_input did not reach its armed wait within {WAIT:?}");
        }
        (poll, move || probe.is_waiting())
    }
}

/// docs/scope.md "Override": the human wins while an agent is hung. The agent
/// publishes to the status-bar zone, then sends requests it never reads replies
/// to until the server's send path is full (a few hundred ~2 KB replies). Safe
/// mode (the hotkey bridge) refuses MCP writes while preserving the unleased
/// publication, even with a `hud_input` long-poll parked. The owner's Clear
/// removes the publication without waiting on the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_override_safe_mode_wins_with_hung_grpc_agent() {
    let mut poc = Poc::boot().await;
    // A completed zero-wait HTTP request must never certify an armed long-poll.
    let mut zero = poc.hud.observe_input_wait("claude");
    let zero_request = json!({
        "jsonrpc":"2.0", "id":zero.request_id(), "method":"tools/call",
        "params":{"name":"hud_input", "arguments":{"wait_ms":0}},
    })
    .to_string();
    let raw = tokio::time::timeout(WAIT, async {
        let raw = post(poc.hud.mcp_addr(), CLAUDE_PSK, zero_request).await;
        assert!(!zero.is_waiting(), "zero-wait response: {raw}");
        assert!(
            !zero.wait_until_waiting().await,
            "zero-wait response: {raw}"
        );
        raw
    })
    .await
    .expect("zero-wait HTTP response and closed observer");
    let response: Value = serde_json::from_str(&raw).expect("zero-wait JSON-RPC response");
    assert_ne!(response["result"]["isError"], true, "{raw}");
    let input: Value = serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("zero-wait tool result"),
    )
    .expect("zero-wait input");
    assert_eq!(input, json!({"items":[],"remaining":0}), "{raw}");

    let hung = HungAgent::connect(poc.hud.grpc_addr()).await;
    assert_eq!(poc.shown("zone:status-bar"), 1);
    let big_clear = || {
        ClientPayload::Clear(wire::Clear {
            key: String::new(),
            surface: format!("zone:{}", "z".repeat(2_000)),
        })
    };
    let flood = hung.keep_sending(big_clear);
    poc.wait_until_hung("resident").await;
    let (mut poll, poll_is_waiting) = poc.park_long_poll().await;
    if !poll_is_waiting() || poll.is_finished() {
        let joined = tokio::time::timeout(WAIT, &mut poll).await;
        panic!("hud_input left its armed wait before owner action: {joined:?}");
    }

    poc.hud.tray_command("pause");
    poc.wait_for("safe mode", |hud| hud.safe_mode_active())
        .await;
    assert_eq!(
        poc.shown("zone:status-bar"),
        1,
        "safe mode preserves the held publication"
    );
    let code = poc
        .call_err("hud_publish", json!({"surface": SUBTITLE, "content": "hi"}))
        .await;
    assert_eq!(code, "SAFE_MODE_ACTIVE");
    if !poll_is_waiting() || poll.is_finished() {
        let joined = tokio::time::timeout(WAIT, &mut poll).await;
        panic!("hud_input left its armed wait during owner action: {joined:?}");
    }
    assert!(poll_is_waiting(), "the server's input wait is still armed");
    assert!(!poll.is_finished(), "the long-poll is still parked");

    poc.hud.press_safe_mode_hotkey();
    poc.wait_for("safe mode off and panel closed", |hud| {
        !hud.safe_mode_active() && !hud.control_panel_open()
    })
    .await;
    assert_eq!(
        poc.hud.suspended_lease_count(),
        0,
        "safe-mode exit leaves no leases suspended"
    );

    // Both independent entry points work while winit is not being stepped.
    // The menu's explicit Resume also closes a panel opened by the pause chord.
    poc.hud.press_safe_mode_hotkey();
    poc.wait_for("chord pause with the same hung agent", |hud| {
        hud.safe_mode_active()
    })
    .await;
    poc.hud.tray_command("resume");
    poc.wait_for("tray resume and panel close", |hud| {
        !hud.safe_mode_active() && !hud.control_panel_open()
    })
    .await;

    // The owner's Clear does not depend on the hung response stream.
    poc.open_panel().await;
    poc.click("agents.manage:resident").await;
    poc.click_confirmed("agents.clear:resident", "Clear resident?")
        .await;
    poc.wait_for("the held publication clears", |hud| {
        hud.zone_publication_count("status-bar") == 0
    })
    .await;
    // Client abort closes its socket; it does not prove server dispatch cancellation.
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

    let write = || resident_publish("ignored");
    let flood = hung.keep_sending(write);
    poc.wait_until_hung("resident").await;

    poc.hud.press_safe_mode_hotkey();
    poc.wait_for("safe mode off and panel closed", |hud| {
        !hud.safe_mode_active() && !hud.control_panel_open()
    })
    .await;
    assert_eq!(
        poc.hud.suspended_lease_count(),
        0,
        "safe-mode exit leaves no leases suspended"
    );
    flood.abort();
}

/// CP-8 with a hung agent: the owner clears, then unpairs, a resident agent
/// that has stopped reading, from the control panel. Neither waits on it: its
/// zone publication clears at once, its session ends, its key stops working,
/// and the other agent's content and key are untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_override_clear_and_unpair_with_hung_grpc_agent() {
    let mut poc = Poc::boot_with_agents_file().await;
    poc.call(
        "hud_publish",
        json!({"surface": SUBTITLE, "content": "bystander"}),
    )
    .await;
    let hung = HungAgent::connect(poc.hud.grpc_addr()).await;
    assert_eq!(poc.shown("zone:status-bar"), 1);
    let flood = hung.keep_sending(|| {
        ClientPayload::Clear(wire::Clear {
            key: String::new(),
            surface: format!("zone:{}", "z".repeat(2_000)),
        })
    });
    poc.wait_until_hung("resident").await;

    poc.open_panel().await;
    poc.click("agents.manage:resident").await;
    poc.click_confirmed("agents.clear:resident", "Clear resident?")
        .await;
    poc.wait_for("the hung agent's publication goes", |hud| {
        hud.zone_publication_count("status-bar") == 0
    })
    .await;
    assert_eq!(poc.shown(SUBTITLE), 1, "the other agent's content stays");

    poc.click_confirmed("agents.unpair:resident", "Unpair resident?")
        .await;
    poc.wait_for("resident leaves the roster", |hud| {
        hud.control_panel_row("agents.manage:resident").is_none()
    })
    .await;
    assert_eq!(
        poc.hud.agent_sessions("resident").await,
        0,
        "its session ended"
    );
    assert_eq!(poc.paired(), ["claude"]);
    let refused = Resident::connect(poc.hud.grpc_addr(), "resident", RESIDENT_PSK).await;
    assert_eq!(
        refused.err().as_deref(),
        Some("AUTH_FAILED"),
        "its key stops working"
    );
    poc.call(
        "hud_publish",
        json!({"surface": SUBTITLE, "content": "still here"}),
    )
    .await;
    flood.abort();

    // Unpairing the last agent reopens pairing, so the HUD is never left
    // with no way in.
    assert!(!poc.hud.pairing_open());
    poc.click("agents.manage:claude").await;
    poc.click_confirmed("agents.unpair:claude", "Unpair claude?")
        .await;
    poc.wait_for("pairing reopens", |hud| hud.pairing_open())
        .await;
    assert!(poc.paired().is_empty());
}

/// CP-8 with responsive agents: Clear takes an MCP agent's portal and zone
/// content and the next publish reattaches; a connected gRPC agent is told
/// without waiting for a reply. Withdrawing a grant applies at once; adding one
/// asks first; the agent's next request honours both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_panel_clear_reattaches_and_grants_apply_to_the_next_request() {
    let mut poc = Poc::boot_with_agents_file().await;
    poc.call(
        "hud_publish",
        json!({"surface": PORTAL, "content": "Working."}),
    )
    .await;
    poc.call(
        "hud_publish",
        json!({"surface": SUBTITLE, "content": "hello"}),
    )
    .await;
    let mut agent = poc.resident().await;
    assert!(agent.publish("working").await.ok);
    assert_eq!(poc.hud.tile_count(), 1);
    assert_eq!(poc.shown("zone:status-bar"), 1);

    poc.open_panel().await;
    poc.click("agents.manage:claude").await;
    poc.click_confirmed("agents.clear:claude", "Clear claude?")
        .await;
    poc.wait_for("claude's portal goes", |hud| hud.tile_count() == 0)
        .await;
    assert_eq!(poc.shown(SUBTITLE), 0);
    let again = poc
        .call(
            "hud_publish",
            json!({"surface": PORTAL, "content": "Back."}),
        )
        .await;
    assert_eq!(again.body["reattached"], json!(true));

    poc.click("agents.manage:resident").await;
    poc.click_confirmed("agents.clear:resident", "Clear resident?")
        .await;
    poc.wait_for("resident content clears", |hud| {
        hud.zone_publication_count("status-bar") == 0
    })
    .await;

    let zones = "agents.grant.zones:claude";
    let toggle = |on: bool| {
        move |hud: &HeadlessEventLoopHarness| {
            hud.control_panel_row(zones)
                .is_some_and(|item| item.control == tze_hud_compositor::PanelControl::Toggle(on))
        }
    };
    let subtitle = json!({"surface": SUBTITLE, "content": "hi"});
    poc.click("agents.manage:claude").await;
    poc.click(zones).await;
    poc.wait_for("zones withdrawn at once", toggle(false)).await;
    assert_eq!(
        poc.call_err("hud_publish", subtitle.clone()).await,
        "NOT_ALLOWED"
    );

    poc.click_confirmed(zones, "Let claude use zones?").await;
    poc.wait_for("zones granted after the confirm", toggle(true))
        .await;
    poc.call("hud_publish", subtitle).await;
}

/// CP-8/CP-3: the roster follows agents as they connect, while the panel is
/// open, without moving keyboard focus.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_panel_roster_updates_live_without_moving_focus() {
    let mut poc = Poc::boot_with_agents_file().await;
    poc.open_panel().await;
    poc.click("agents.manage:claude").await;
    let row = "agents.manage:resident";
    let detail = move |hud: &HeadlessEventLoopHarness| {
        hud.control_panel_row(row).and_then(|item| item.detail)
    };
    poc.wait_for("resident listed idle", move |hud| {
        detail(hud).as_deref() == Some("Idle · holds nothing")
    })
    .await;
    let focus = Some(tze_hud_compositor::PanelFocus::Row(
        "agents.manage:claude".into(),
    ));
    let focused = focus.clone();
    poc.wait_for("the click focuses claude's row", move |hud| {
        hud.control_panel_focus() == focused
    })
    .await;

    let mut agent = poc.resident().await;
    assert!(agent.publish("working").await.ok);
    poc.wait_for("resident shows as connected", move |hud| {
        detail(hud).as_deref() == Some("Connected · 1 zone")
    })
    .await;
    poc.wait_for("the header counts it", |hud| {
        hud.control_panel_state_line().as_deref()
            == Some("1 agent · 0 portals · 1 zone · 0 widgets")
    })
    .await;
    assert_eq!(poc.hud.control_panel_focus(), focus, "focus did not move");

    // A hand edit to agents.toml adds a row above the focused one, then
    // takes it away; the focus stays on claude's row throughout.
    let path = poc.agents_file.as_ref().expect("agents file").1.clone();
    let hand_edit = |change| tze_hud_config::agents_file::edit(&path, &change).expect("edit");
    hand_edit(tze_hud_config::AgentChange::Pair {
        agent: "aardvark".into(),
        record: tze_hud_config::AgentRecord {
            psk_sha256: tze_hud_config::agents_file::hash_psk_hex("aardvark-psk"),
            allow: vec!["zone:status-bar".into()],
            paired_at: None,
        },
    });
    let listed =
        |hud: &HeadlessEventLoopHarness| hud.control_panel_row("agents.manage:aardvark").is_some();
    poc.wait_for("a hand-paired agent appears", listed).await;
    assert_eq!(poc.hud.control_panel_focus(), focus, "focus did not move");
    hand_edit(tze_hud_config::AgentChange::Unpair("aardvark".into()));
    poc.wait_for("a hand-unpaired agent leaves", move |hud| !listed(hud))
        .await;
    assert_eq!(poc.hud.control_panel_focus(), focus, "focus did not move");
}

/// CP-13 end to end: Connect an agent shows a prompt with a fresh one-time
/// code; the owner names the agent and copies; an agent following the prompt
/// pairs with that code (and gets a key the prompt never held); the card says
/// so and the agent joins the roster. The pairing chord opens the card too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_panel_connect_prompt_pairs_an_agent() {
    let mut poc = Poc::boot_with_agents_file().await;
    poc.open_panel().await;
    poc.click("agents.connect").await;
    poc.wait_for("Connect shows", |hud| hud.control_panel_connect().is_some())
        .await;
    assert!(poc.hud.pairing_open(), "a fresh code is open");
    let card = poc.hud.control_panel_connect().unwrap();
    let prompt = card.block.join("\n");
    let mcp = poc.hud.mcp_addr();
    assert!(
        prompt.contains(&format!("reachable ONLY from that PC itself, at {mcp}")),
        "{prompt}"
    );
    assert!(
        prompt.contains(r#""agent":"agent","code":"#),
        "a free default name: {prompt}"
    );

    poc.hud.panel_type("Laptop 2");
    poc.wait_for("the name is typed", |hud| {
        hud.control_panel_row("connect.name").is_some_and(|item| {
            item.control
                == tze_hud_compositor::PanelControl::Value {
                    text: "laptop-2".into(),
                    mono: true,
                }
        })
    })
    .await;
    poc.hud.panel_press("Enter");
    poc.wait_for("the prompt is copied", |hud| hud.clipboard().len() == 1)
        .await;
    let copied = poc.hud.clipboard().remove(0);
    let code = copied
        .split(r#""code":""#)
        .nth(1)
        .and_then(|rest| rest.get(..6))
        .expect("the prompt carries the code")
        .to_owned();
    assert!(
        copied.contains(r#"{"agent":"laptop-2","code":""#),
        "{copied}"
    );
    let status = |hud: &HeadlessEventLoopHarness| {
        hud.control_panel_connect()
            .and_then(|card| card.status)
            .map(|(text, _)| text)
    };
    poc.wait_for("the card waits for the agent", move |hud| {
        status(hud).as_deref() == Some("Copied. Waiting for laptop-2 to pair…")
    })
    .await;

    // The agent follows the prompt.
    let reply = post_pair(mcp, &format!(r#"{{"agent":"laptop-2","code":"{code}"}}"#)).await;
    let reply: Value = serde_json::from_str(&reply).expect("pair reply");
    let psk = reply["psk"].as_str().expect("a key").to_owned();
    assert!(!copied.contains(&psk), "the prompt never holds a key");
    assert_eq!(reply["mcp"], json!(format!("http://{mcp}/mcp")));
    poc.wait_for("the card says it paired", move |hud| {
        status(hud).as_deref() == Some("laptop-2 paired. It shows in Agents.")
    })
    .await;
    assert!(poc.paired().contains(&"laptop-2".to_owned()));
    assert!(!poc.hud.pairing_open(), "the code worked once");
    assert_eq!(
        poc.hud.control_panel_focus(),
        Some(tze_hud_compositor::PanelFocus::Row("connect.name".into())),
        "pairing does not move focus"
    );

    // Copying again, with the code spent, copies a fresh one.
    poc.hud.panel_press("Enter");
    poc.wait_for("the prompt is copied again", |hud| {
        hud.clipboard().len() == 2
    })
    .await;
    let again = poc.hud.clipboard().remove(1);
    assert!(!again.contains(&format!(r#""code":"{code}""#)), "{again}");
    assert!(poc.hud.pairing_open(), "the fresh code is live");
    // A Connect code cannot ask for admin.
    let refused = post_pair(mcp, r#"{"agent":"boss","code":"000000","admin":true}"#).await;
    assert!(refused.contains("NOT_ADMIN"), "{refused}");

    // Esc goes back to the sections, where the new agent is listed.
    poc.hud.panel_press("Escape");
    poc.wait_for("laptop-2 is in the roster", |hud| {
        hud.control_panel_row("agents.manage:laptop-2").is_some()
    })
    .await;
    poc.hud.panel_press("Escape");
    poc.wait_for("the panel closes", |hud| !hud.control_panel_open())
        .await;

    // The pairing chord opens the panel, agents running, at Connect.
    poc.hud
        .press_chord(tze_hud_scene::config::ChordRole::Pairing);
    poc.wait_for("the chord opens Connect", |hud| {
        hud.control_panel_connect().is_some()
    })
    .await;
    assert!(poc.hud.pairing_open());
    assert!(!poc.hud.safe_mode_active());
}

/// Layout (T11, hud-vniri.7): a widget is moved by typing its box and its
/// content limit is chosen, both live in the scene and saved into its own
/// `[[tabs.widgets]]` entry with the owner's comments kept; a box off the
/// display is refused and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_panel_layout_moves_widgets_and_saves_their_entry() {
    use tze_hud_compositor::PanelControl;
    use tze_hud_scene::types::GeometryPolicy;
    let dir = tempfile::tempdir().expect("scratch dir");
    let config = dir.path().join("tze_hud.toml");
    std::fs::write(&config, PRODUCTION_CONFIG).expect("config copy");
    let mut cfg = runtime_config();
    cfg.config_file_path = Some(config.display().to_string());
    let mut poc = Poc::boot_from(cfg, None).await;
    poc.open_panel().await;
    let saved = || std::fs::read_to_string(&config).expect("config");
    let row = "layout.main-gauge.geometry";

    poc.wait_for("the gauge's box shows", |hud| {
        hud.control_panel_row(row).is_some_and(|item| {
            matches!(item.control, PanelControl::Value { ref text, .. } if text == "1.25%, 42.22% · 2.81×15.56%")
        })
    })
    .await;
    poc.click(row).await;
    poc.hud.panel_type("95 10 10 10");
    poc.hud.panel_press("Enter");
    poc.wait_for("a box off the display is refused", |hud| {
        hud.control_panel_row(row)
            .and_then(|item| item.detail)
            .is_some_and(|d| d.contains("fit on the display"))
    })
    .await;
    assert_eq!(saved(), PRODUCTION_CONFIG, "nothing written");

    poc.hud.panel_type("4 8 18 12");
    poc.hud.panel_press("Enter");
    poc.wait_for("the gauge moves", |hud| {
        hud.widget_layout("main-gauge").is_some_and(|(g, _)| {
            g == Some(GeometryPolicy::Relative {
                x_pct: 0.04,
                y_pct: 0.08,
                width_pct: 0.18,
                height_pct: 0.12,
            })
        })
    })
    .await;
    poc.wait_for("and is saved", |_| saved().contains("x_pct      = 0.04"))
        .await;
    let out = saved();
    assert!(
        out.contains("# MCP widget_name: \"main-gauge\""),
        "comments kept"
    );
    assert!(
        out.contains("x_pct      = 0.4296875"),
        "other widgets untouched"
    );

    poc.click("layout.main-gauge.clear").await;
    poc.wait_for("clears after 5 s, live and saved", |hud| {
        hud.widget_layout("main-gauge")
            .is_some_and(|(_, ms)| ms == Some(5_000))
            && saved().contains("auto_clear_ms = 5000")
    })
    .await;
}

/// Appearance (T11 CP-5, hud-h51u7.5): the theme and any token change the
/// tokens the HUD draws with on its next turn and are saved to
/// `[design_tokens]` with the owner's comments kept; a token takes only a
/// value of its kind; Reset all overrides removes every override, even one
/// that hides text; a read-only config keeps the change for the session and
/// says why; a restart keeps what was saved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_panel_appearance_restyles_live_and_saves() {
    use tze_hud_config::themes::resolve_config_tokens;
    let dir = tempfile::tempdir().expect("scratch dir");
    let config = dir.path().join("tze_hud.toml");
    // A portal color override (not a canonical token): Reset all removes it
    // too.
    let production = PRODUCTION_CONFIG.replace(
        "theme = \"tonal-glass\"\n",
        "theme = \"tonal-glass\"\n\"portal.header.text_color\" = \"#00000000\"\n",
    );
    assert_ne!(production, PRODUCTION_CONFIG);
    std::fs::write(&config, &production).expect("config copy");
    let mut cfg = runtime_config();
    cfg.config_toml = Some(production.clone());
    cfg.config_file_path = Some(config.display().to_string());
    let mut poc = Poc::boot_from(cfg.clone(), None).await;
    poc.open_panel().await;
    let saved = || std::fs::read_to_string(&config).expect("config");
    poc.wait_for("the portal override is listed", |hud| {
        hud.control_panel_row("tokens.portal.header.text_color")
            .is_some()
    })
    .await;
    let theme = |name: &str| resolve_config_tokens(&[("theme".to_owned(), name.to_owned())].into());
    let key = "color.backdrop.default";
    assert_ne!(theme("tonal-glass")[key], theme("classic")[key]);

    // A key previews the next theme (tonal-glass → classic) without
    // saving it; Enter keeps it.
    poc.hud.panel_type("theme");
    poc.wait_for("the theme row has focus", |hud| {
        hud.control_panel_focus()
            == Some(tze_hud_compositor::PanelFocus::Row(
                "appearance.theme".into(),
            ))
    })
    .await;
    poc.hud.panel_press("ArrowRight");
    poc.wait_for("the HUD previews classic", |hud| {
        hud.design_token(key).as_deref() == Some(theme("classic")[key].as_str())
    })
    .await;
    assert!(
        !saved().contains("theme = \"classic\""),
        "a preview saves nothing"
    );
    poc.hud.panel_press("Enter");
    poc.wait_for("classic is kept and saved", |_| {
        saved().contains("theme = \"classic\"")
    })
    .await;
    // Leaving a preview (Esc clears the search, which moves focus) puts
    // the kept theme back.
    poc.hud.panel_press("ArrowRight");
    poc.wait_for("the next theme previews", |hud| {
        hud.design_token(key).as_deref() != Some(theme("classic")[key].as_str())
    })
    .await;
    poc.hud.panel_press("Escape");
    poc.wait_for("back to classic, still saved", |hud| {
        hud.design_token(key).as_deref() == Some(theme("classic")[key].as_str())
            && saved().contains("theme = \"classic\"")
    })
    .await;
    assert!(
        saved().contains("# Keep overrides for deployment-specific"),
        "comments kept"
    );

    let token = "tokens.color.on_surface";
    poc.click(token).await;
    poc.hud.panel_type("blue");
    poc.hud.panel_press("Enter");
    poc.wait_for("a word is not a color", |hud| {
        hud.control_panel_row(token)
            .and_then(|item| item.detail)
            .is_some_and(|d| d.contains("#RRGGBB"))
    })
    .await;
    // Transparent text: the panel keeps its own text legible.
    poc.hud.panel_type("#00000000");
    poc.hud.panel_press("Enter");
    poc.wait_for("the override applies and is saved", |hud| {
        hud.design_token("color.on_surface").as_deref() == Some("#00000000")
            && saved().contains("\"color.on_surface\" = \"#00000000\"")
    })
    .await;

    poc.click("tokens.reset_all").await;
    poc.wait_for("every override is gone, the theme stays", |hud| {
        hud.design_token("color.on_surface") == theme("classic").get("color.on_surface").cloned()
            && !saved().contains("\"color.on_surface\"")
            && !saved().contains("portal.header.text_color")
            && saved().contains("theme = \"classic\"")
    })
    .await;

    // A read-only config: the change applies for the session and says why.
    let mut perms = std::fs::metadata(&config).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&config, perms).unwrap();
    poc.click("appearance.font.mono").await;
    poc.hud.panel_type("Consolas");
    poc.hud.panel_press("Enter");
    poc.wait_for("live, not saved, with the reason and path", |hud| {
        hud.design_token("font.mono").as_deref() == Some("Consolas")
            && hud
                .control_panel_row("appearance.font.mono")
                .and_then(|item| item.detail)
                .is_some_and(|d| {
                    d.contains("not saved") && d.contains(&config.display().to_string())
                })
    })
    .await;
    assert!(!saved().contains("Consolas"));
    #[allow(clippy::permissions_set_readonly_false)]
    {
        let mut perms = std::fs::metadata(&config).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&config, perms).unwrap();
    }
    drop(poc);

    // A restart reads what was saved.
    let mut cfg = cfg;
    cfg.config_toml = Some(saved());
    let poc = Poc::boot_from(cfg, None).await;
    assert_eq!(
        poc.hud.design_token(key).as_deref(),
        Some(theme("classic")[key].as_str())
    );
}

/// Chords (T11 CP-10): a chord is typed in place, registered at once (the
/// harness records Windows' registrar) and saved to `[runtime]`; a chord
/// another role has is refused before Windows is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_panel_chords_rebind_live_and_save() {
    use tze_hud_compositor::PanelControl;
    let dir = tempfile::tempdir().expect("scratch dir");
    let config = dir.path().join("tze_hud.toml");
    std::fs::write(&config, PRODUCTION_CONFIG).expect("config copy");
    let mut cfg = runtime_config();
    cfg.config_file_path = Some(config.display().to_string());
    let mut poc = Poc::boot_from(cfg, None).await;
    poc.open_panel().await;
    let saved = || std::fs::read_to_string(&config).expect("config");
    let row = "input.panel_chord";

    poc.click(row).await;
    poc.hud.panel_type("Ctrl+Shift+F12");
    poc.hud.panel_press("Enter");
    poc.wait_for("a chord in use is refused", |hud| {
        hud.control_panel_row(row)
            .and_then(|item| item.detail)
            .is_some_and(|d| d == "Ctrl+Shift+F12 is already the pause chord")
    })
    .await;
    assert!(poc.hud.panel_effects().is_empty(), "Windows was not asked");

    poc.hud.panel_type("ctrl+alt+f9");
    poc.hud.panel_press("Enter");
    poc.wait_for("the new chord is in and saved", |hud| {
        hud.control_panel_row(row).is_some_and(|item| {
            matches!(item.control, PanelControl::Value { ref text, .. } if text == "Ctrl+Alt+F9")
        }) && saved().contains("panel_hotkey = \"Ctrl+Alt+F9\"")
    })
    .await;
    assert_eq!(poc.hud.panel_effects(), ["rebind Panel Ctrl+Alt+F9"]);
    assert!(saved().contains("# Open the control panel with agents running."));
}

/// CP-7 and CP-10 through the panel: a live setting applies at once and is
/// saved to `[runtime]` with the owner's comments kept; a value a flag set is
/// read-only and names the flag; a port is saved, marked "Restart to apply"
/// and counted by Restart now, whose failure leaves this instance up and says
/// why; System's toggles and openers reach the OS (recorded by the harness).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_panel_runtime_rows_apply_save_and_stage_a_restart() {
    use tze_hud_compositor::PanelControl;
    use tze_hud_runtime::launch::Source;
    let dir = tempfile::tempdir().expect("scratch dir");
    let config = dir.path().join("tze_hud.toml");
    std::fs::write(&config, PRODUCTION_CONFIG).expect("config copy");
    let mut cfg = runtime_config();
    cfg.config_file_path = Some(config.display().to_string());
    cfg.debug_zones = true;
    cfg.sources.debug_zones = Source::Flag("--debug-zones");
    let mut poc = Poc::boot_from(cfg, None).await;
    poc.open_panel().await;
    let saved = || std::fs::read_to_string(&config).expect("config");
    // A key the config sets (not a commented example).
    let sets = |key: &str| saved().lines().any(|l| l.trim_start().starts_with(key));

    // Both production server bridges share the panel's live omission default.
    let accepted_at = poc.clock.now_us();
    let question = json!({"body":"Continue?", "actions":[{"label":"Yes", "callback_id":"yes"}]});
    for (key, delay) in [("active-old", 0), ("scheduled-old", 5000)] {
        let result = poc
            .call(
                "hud_publish",
                json!({"surface":NOTIFICATION,"content":question,"key":key,"delay_ms":delay}),
            )
            .await;
        assert_eq!(result.body["expires_in_ms"], 60_000 + delay);
    }
    let mut resident = poc.resident().await;
    let grpc_question = |key: &str, present_at_us| wire::Publish {
        surface: NOTIFICATION.into(),
        key: key.into(),
        present_at_us,
        content: Some(proto::ZoneContent {
            payload: Some(proto::zone_content::Payload::Notification(
                proto::NotificationPayload {
                    text: "Continue?".into(),
                    actions: vec![proto::NotificationActionProto {
                        label: "Yes".into(),
                        callback_id: "yes".into(),
                        url: String::new(),
                    }],
                    ..Default::default()
                },
            )),
        }),
        ..Default::default()
    };
    let shown_at = poc.clock.now_us() + 5_000_000;
    for (key, present_at) in [("grpc-active-old", 0), ("grpc-scheduled-old", shown_at)] {
        assert!(
            resident
                .request(ClientPayload::Publish(grpc_question(key, present_at)))
                .await
                .ok
        );
    }
    poc.hud.panel_type("question");
    poc.wait_for("search finds and focuses the question window", |hud| {
        hud.control_panel_focus()
            == Some(tze_hud_compositor::PanelFocus::Row(
                "agents.answer_window".into(),
            ))
    })
    .await;
    poc.hud.panel_press("ArrowRight");
    poc.wait_for("the answer window saves and shows its source", |hud| {
        saved().contains("answer_window_ms = 120000")
            && hud
                .control_panel_row("agents.answer_window")
                .is_some_and(|item| {
                    item.detail
                        .as_deref()
                        .is_some_and(|text| text.ends_with(" · Saved to config"))
                })
    })
    .await;
    for psk in [CLAUDE_PSK, RESIDENT_PSK] {
        let surfaces = poc.call_as(psk, "hud_surfaces", json!({})).await;
        let held = surfaces.body["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["s"] == NOTIFICATION)
            .unwrap();
        assert_eq!(
            held["expires_in_ms"], 65_000,
            "accepted active and scheduled deadlines remain frozen"
        );
    }
    // Check each stored deadline, rather than only the maximum exposed by
    // discovery: one retimed active card must not hide behind a scheduled one.
    for namespace in ["claude", "resident"] {
        assert_eq!(
            poc.hud
                .zone_publication_deadlines("notification-area", namespace),
            [
                Some(accepted_at + 60_000_000),
                Some(accepted_at + 65_000_000)
            ],
            "{namespace}'s active and scheduled deadlines remain unchanged"
        );
    }
    let result = poc
        .call(
            "hud_publish",
            json!({"surface":NOTIFICATION,"content":question,"key":"new"}),
        )
        .await;
    assert_eq!(result.body["expires_in_ms"], 120_000);
    assert!(
        resident
            .request(ClientPayload::Publish(grpc_question("grpc-new", 0)))
            .await
            .ok
    );
    let surfaces = poc.call_as(RESIDENT_PSK, "hud_surfaces", json!({})).await;
    let held = surfaces.body["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["s"] == NOTIFICATION)
        .unwrap();
    assert_eq!(
        held["expires_in_ms"], 120_000,
        "gRPC sees the same panel change"
    );
    for namespace in ["claude", "resident"] {
        assert_eq!(
            poc.hud
                .zone_publication_deadlines("notification-area", namespace),
            [
                Some(accepted_at + 60_000_000),
                Some(accepted_at + 65_000_000),
                Some(accepted_at + 120_000_000),
            ]
        );
    }
    // Race the next save with admission. Either complete value is coherent.
    poc.hud.panel_press("ArrowRight");
    let result = poc
        .call(
            "hud_publish",
            json!({"surface":NOTIFICATION,"content":question,"key":"racing"}),
        )
        .await;
    let lifetime = result.body["expires_in_ms"].as_u64().unwrap();
    assert!([120_000, 300_000].contains(&lifetime));
    let surfaces = poc.call("hud_surfaces", json!({})).await;
    let held = surfaces.body["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["s"] == NOTIFICATION)
        .unwrap();
    assert_eq!(
        held["expires_in_ms"], lifetime,
        "the admitted expiry and reply use one snapshot"
    );
    poc.wait_for("the last answer window save completes", |_| {
        saved().contains("answer_window_ms = 300000")
    })
    .await;
    // Delete deliberately does not reset search results. Leave search, then
    // pick the row with the pointer before using its keyboard Reset.
    poc.hud.panel_press("Escape");
    poc.wait_for("search is cleared", |hud| {
        hud.control_panel_row("diagnostics.fps").is_some()
    })
    .await;
    poc.click("agents.answer_window").await;
    poc.wait_for(
        "the pointer choice saves and focuses the answer window",
        |hud| {
            saved().contains("answer_window_ms = 15000")
                && hud.control_panel_focus()
                    == Some(tze_hud_compositor::PanelFocus::Row(
                        "agents.answer_window".into(),
                    ))
                && hud.control_panel_row("agents.answer_window").is_some_and(
                    |item| matches!(item.control, PanelControl::Choice(ref text) if text == "15 s"),
                )
        },
    )
    .await;
    poc.hud.panel_press("Delete");
    poc.wait_for(
        "Reset removes the override and restores the default",
        |hud| {
            !sets("answer_window_ms")
                && hud
                    .control_panel_row("agents.answer_window")
                    .is_some_and(|item| {
                        !item.reset
                            && item
                                .detail
                                .as_deref()
                                .is_some_and(|text| text.ends_with(" · Default"))
                            && matches!(item.control, PanelControl::Choice(ref text) if text == "1 min")
                    })
        },
    )
    .await;

    // Pinned by a flag: read-only, naming it.
    poc.wait_for("zone outlines row", |hud| {
        hud.control_panel_row("diagnostics.zone_outlines")
            .is_some_and(|item| {
                item.detail.as_deref() == Some("Set by --debug-zones")
                    && matches!(item.control, PanelControl::ReadOnly { ref text, .. } if text == "On")
            })
    })
    .await;

    // Live: the cap moves to the next choice now and is saved.
    poc.click("diagnostics.fps").await;
    // Applied live first, then saved.
    poc.wait_for("fps 90 applies and is saved", |hud| {
        hud.live_settings().0 == 90 && saved().contains("\nfps = 90")
    })
    .await;
    assert!(
        saved().starts_with(&PRODUCTION_CONFIG[..PRODUCTION_CONFIG.find("[runtime]").unwrap()]),
        "the owner's header survives"
    );
    assert!(saved().contains("# Global chords (work while the overlay is unfocused"));

    // A port: typed, refused when invalid, then saved and staged.
    poc.click("network.grpc_port").await;
    poc.hud.panel_type("99999");
    poc.hud.panel_press("Enter");
    poc.wait_for("the bad port is refused", |hud| {
        hud.control_panel_row("network.grpc_port")
            .and_then(|item| item.detail)
            .is_some_and(|d| d.starts_with("Type a port"))
    })
    .await;
    assert!(!sets("grpc_port"));
    poc.hud.panel_type("50061");
    poc.hud.panel_press("Enter");
    poc.wait_for("the port is staged", |hud| {
        hud.control_panel_row("network.grpc_port").is_some_and(|item| {
            item.badge.as_ref().is_some_and(|b| b.text == "Restart to apply")
                && matches!(item.control, PanelControl::Value { ref text, .. } if text == "50061")
        })
    })
    .await;
    assert!(saved().contains("grpc_port = 50061"), "{}", saved());
    poc.wait_for("restart counts it", |hud| {
        hud.control_panel_row("system.restart").is_some_and(|item| {
            item.detail.as_deref() == Some("Applies 1 change · agent content clears")
                && item.badge.as_ref().is_some_and(|b| b.text == "gRPC port")
        })
    })
    .await;
    poc.wait_for("the header says a restart is pending", |hud| {
        hud.control_panel_state_line()
            .is_some_and(|line| line.contains("restart pending"))
    })
    .await;

    // Restart: confirmed; the new instance never starts, so this one stays
    // up and the row says why.
    poc.click_confirmed("system.restart", "Restart the HUD now?")
        .await;
    poc.wait_for("the failed restart shows", |hud| {
        hud.control_panel_row("system.restart")
            .and_then(|item| item.detail)
            .is_some_and(|d| d.starts_with("Last restart failed:"))
    })
    .await;
    assert!(poc.hud.control_panel_open(), "this instance stays up");

    // System's OS effects.
    poc.wait_for("autostart is read", |hud| {
        hud.control_panel_row("system.autostart")
            .is_some_and(|item| item.control == PanelControl::Toggle(false))
    })
    .await;
    poc.click("system.autostart").await;
    poc.wait_for("autostart turns on", |hud| {
        hud.control_panel_row("system.autostart")
            .is_some_and(|item| item.control == PanelControl::Toggle(true))
    })
    .await;
    poc.click("system.open_config").await;
    poc.click("system.open_logs").await;
    poc.wait_for("the openers ran", |hud| hud.panel_effects().len() == 3)
        .await;
    let effects = poc.hud.panel_effects();
    assert_eq!(effects[0], "autostart true");
    assert_eq!(effects[1], format!("open {}", config.display()));
    // The folder the row names is the one opened.
    let logs = poc
        .hud
        .control_panel_row("system.open_logs")
        .and_then(|item| item.detail)
        .expect("the log folder row names its folder");
    assert_eq!(effects[2], format!("open {logs}"));
}

/// A responsive agent is told what the human did (docs/api.md): safe mode
/// arrives as `SessionSuspended` and writes are refused with `SAFE_MODE_ACTIVE`
/// until `SessionResumed`; the owner can Clear without waiting for the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_override_notifies_the_agent() {
    let mut poc = Poc::boot().await;
    let mut agent = poc.resident().await;
    assert!(agent.publish("working").await.ok);

    poc.hud.press_safe_mode_hotkey();
    match next_message(&mut agent.stream).await.payload {
        Some(ServerPayload::SessionSuspended(_)) => {}
        other => panic!("expected SessionSuspended, got {other:?}"),
    }
    let refused = agent.publish("ignored").await;
    assert_eq!(refused.code, "SAFE_MODE_ACTIVE");

    poc.hud.press_safe_mode_hotkey();
    match next_message(&mut agent.stream).await.payload {
        Some(ServerPayload::SessionResumed(_)) => {}
        other => panic!("expected SessionResumed, got {other:?}"),
    }
    assert!(agent.publish("back").await.ok);

    poc.open_panel().await;
    poc.click("agents.manage:resident").await;
    poc.click_confirmed("agents.clear:resident", "Clear resident?")
        .await;
    poc.wait_for("resident content clears", |hud| {
        hud.zone_publication_count("status-bar") == 0
    })
    .await;
    assert!(
        agent.publish("again").await.ok,
        "Clear leaves the session usable"
    );
}

/// §3: agents never learn the panel is open or what the owner hid. Every
/// agent-visible answer (discovery, zone, widget and portal publishes, a
/// resident tile claim's reply, and no unsolicited stream message before
/// it) reads the same with the panel closed, open, with each surface class
/// hidden, and with Hide all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poc_agents_see_the_same_with_the_panel_open_or_content_hidden() {
    use tze_hud_compositor::PanelControl;

    let mut poc = Poc::boot_with_agents_file().await;
    let publishes = [
        json!({"surface": PORTAL, "content": "Working."}),
        json!({"surface": SUBTITLE, "content": "hello"}),
        json!({"surface": "widget:main-gauge", "params": {"level": 0.5}}),
    ];
    let mut agent = poc.resident().await;
    let answers = async |poc: &mut Poc, agent: &mut Resident| {
        let mut bodies = vec![poc.call("hud_surfaces", json!({})).await.body];
        for publish in &publishes {
            bodies.push(poc.call("hud_publish", publish.clone()).await.body);
        }
        let mut published = agent.request(resident_publish("working")).await;
        // Fresh each claim: the echoed sequence, the new ids and lease.
        // Everything else (granted, code, hint, TTL) must not move.
        published.seq = 0;
        (bodies, published)
    };
    // The first publishes attach and create; compare from the second round.
    answers(&mut poc, &mut agent).await;
    let baseline = answers(&mut poc, &mut agent).await;

    poc.open_panel().await;
    assert_eq!(
        answers(&mut poc, &mut agent).await,
        baseline,
        "panel open, nothing hidden"
    );
    let toggle = |row: &'static str, on: bool| {
        move |hud: &HeadlessEventLoopHarness| {
            hud.control_panel_row(row)
                .is_some_and(|item| item.control == PanelControl::Toggle(on))
        }
    };
    for row in [
        "visibility.portals",
        "visibility.zones",
        "visibility.widgets",
    ] {
        // A class row is on while its class shows.
        poc.click(row).await;
        poc.wait_for("the class hides", toggle(row, false)).await;
        assert_eq!(
            answers(&mut poc, &mut agent).await,
            baseline,
            "{row} hidden"
        );
        poc.click(row).await;
        poc.wait_for("the class shows again", toggle(row, true))
            .await;
    }
    poc.click("visibility.hide_all").await;
    poc.wait_for("everything hides", toggle("visibility.hide_all", true))
        .await;
    assert_eq!(answers(&mut poc, &mut agent).await, baseline, "Hide all");
}
