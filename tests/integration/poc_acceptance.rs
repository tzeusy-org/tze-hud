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
use tze_hud_runtime::windowed::{HeadlessEventLoopHarness, WindowedConfig};
use tze_hud_scene::TestClock;

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

/// Agent identity, in one place. The runtime is seeded with an `agents.toml`-style
/// directory pairing `claude` (allow = ["*"]) by the SHA-256 of this PSK, and MCP
/// calls present the PSK as the bearer.
const CLAUDE_PSK: &str = "poc-acceptance-psk";

fn runtime_config() -> WindowedConfig {
    WindowedConfig {
        agents: {
            let mut agents = tze_hud_scene::config::AgentDirectory::default();
            agents.insert(
                "claude",
                tze_hud_scene::config::hash_psk(CLAUDE_PSK),
                vec!["*".to_string()],
            );
            agents.shared()
        },
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
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        })
        .to_string();
        let mut call = tokio::spawn(post(self.hud.mcp_addr(), request));
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
        let mut call = tokio::spawn(post(self.hud.mcp_addr(), request));
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

/// Minimal HTTP/1.0 POST: the MCP server answers and closes.
async fn post(addr: std::net::SocketAddr, body: String) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to MCP");
    let request = format!(
        "POST /mcp HTTP/1.0\r\nAuthorization: Bearer {CLAUDE_PSK}\r\n\
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
