use super::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use tze_hud_projection::hub::{PortalHub, PortalKey, PortalStatus};
use tze_hud_scene::config::hash_psk;
use tze_hud_scene::{
    PendingAction, SceneId, TestClock,
    types::{
        ContentionPolicy, GeometryPolicy, LeaseState, RenderingPolicy, Rgba, WidgetDefinition,
        WidgetInstance, WidgetParamConstraints, WidgetParamType, WidgetParameterDeclaration,
        WidgetParameterValue, ZoneContent, ZoneRegistry,
    },
};

const TEST_PSK: &str = "test-psk-do-not-use-in-production";

fn psk() -> String {
    std::env::var("MCP_TEST_PSK").unwrap_or_else(|_| TEST_PSK.to_string())
}

fn ctx() -> CallerContext {
    CallerContext::with_bearer(psk())
}

/// A scene with the default zones and a `gauge` widget, on a test clock.
fn scene(clock: &TestClock) -> SceneGraph {
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
    scene.zone_registry = ZoneRegistry::with_defaults();
    let tab_id = scene.create_tab("Main", 0).expect("create tab");
    let f32_range = WidgetParamConstraints {
        f32_min: Some(0.0),
        f32_max: Some(1.0),
        string_max_bytes: None,
        enum_allowed_values: vec![],
    };
    scene.widget_registry.register_definition(WidgetDefinition {
        id: "gauge".into(),
        name: "Gauge".into(),
        description: "gauge".into(),
        parameter_schema: vec![
            WidgetParameterDeclaration {
                name: "level".into(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: Some(f32_range),
            },
            WidgetParameterDeclaration {
                name: "label".into(),
                param_type: WidgetParamType::String,
                default_value: WidgetParameterValue::String(String::new()),
                constraints: None,
            },
            WidgetParameterDeclaration {
                name: "fill_color".into(),
                param_type: WidgetParamType::Color,
                default_value: WidgetParameterValue::Color(Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 1.0,
                    a: 1.0,
                }),
                constraints: None,
            },
        ],
        layers: vec![],
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.2,
            height_pct: 0.5,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::LatestWins,
        max_publishers: WidgetDefinition::default_max_publishers(),
        ephemeral: false,
        hover_behavior: None,
    });
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".into(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "gauge".into(),
        current_params: HashMap::from([
            ("level".into(), WidgetParameterValue::F32(0.0)),
            ("label".into(), WidgetParameterValue::String(String::new())),
        ]),
    });
    scene
}

fn server() -> (McpServer, TestClock) {
    let clock = TestClock::new(1_000);
    let server = McpServer::new(scene(&clock)).with_config(McpConfig::with_psk(psk()));
    (server, clock)
}

/// A server whose only agent `bot` may use `zone:subtitle` and `widget:gauge`.
fn restricted_server() -> McpServer {
    let clock = TestClock::new(1_000);
    let mut agents = AgentDirectory::default();
    agents.insert(
        "bot",
        hash_psk("bot-key"),
        vec![
            "publish_zone:subtitle".to_string(),
            "publish_widget:gauge".to_string(),
        ],
    );
    McpServer::new(scene(&clock)).with_config(McpConfig::with_agents(agents.shared()))
}

async fn rpc(server: &McpServer, ctx: &CallerContext, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let raw = server.dispatch(&body.to_string(), ctx).await;
    serde_json::from_str(&raw).expect("JSON response")
}

/// Call a tool; returns (decoded text block, isError).
async fn call_as(
    server: &McpServer,
    ctx: &CallerContext,
    tool: &str,
    args: Value,
) -> (Value, bool) {
    let resp = rpc(
        server,
        ctx,
        "tools/call",
        json!({"name": tool, "arguments": args}),
    )
    .await;
    assert!(resp["error"].is_null(), "protocol error: {resp}");
    let result = &resp["result"];
    let content = result["content"].as_array().expect("content blocks");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
    let text: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    (text, result["isError"] == true)
}

async fn call(server: &McpServer, tool: &str, args: Value) -> Value {
    let (v, is_error) = call_as(server, &ctx(), tool, args).await;
    assert!(!is_error, "{tool} failed: {v}");
    v
}

async fn call_err(server: &McpServer, tool: &str, args: Value) -> Value {
    let (v, is_error) = call_as(server, &ctx(), tool, args).await;
    assert!(is_error, "{tool} should fail: {v}");
    v
}

// ── Transport ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn initialize_handshake_and_tools_list() {
    let (server, _) = server();
    let init = rpc(
        &server,
        &ctx(),
        "initialize",
        json!({"protocolVersion": "2025-06-18"}),
    )
    .await;
    assert_eq!(
        init["result"]["protocolVersion"],
        crate::schema::PROTOCOL_VERSION
    );
    assert_eq!(
        init["result"]["capabilities"]["tools"]["listChanged"],
        false
    );

    let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    assert_eq!(
        server.dispatch(&note.to_string(), &ctx()).await,
        "",
        "notifications get no response"
    );

    let list = rpc(&server, &ctx(), "tools/list", json!({})).await;
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn bare_tool_methods_are_gone() {
    let (server, _) = server();
    for method in [
        "hud_publish",
        "publish_to_zone",
        "list_zones",
        "portal_projection_attach",
    ] {
        let resp = rpc(&server, &ctx(), method, json!({})).await;
        assert_eq!(resp["error"]["code"], -32601, "{method}");
    }
}

#[tokio::test]
async fn unknown_tool_is_a_protocol_error() {
    let (server, _) = server();
    let resp = rpc(
        &server,
        &ctx(),
        "tools/call",
        json!({"name": "create_tile", "arguments": {}}),
    )
    .await;
    assert_eq!(resp["error"]["code"], -32602);
}

#[tokio::test]
async fn missing_or_unknown_psk_is_unauthenticated() {
    let (server, _) = server();
    for c in [CallerContext::guest(), CallerContext::with_bearer("nope")] {
        let resp = rpc(&server, &c, "tools/list", json!({})).await;
        assert_eq!(resp["error"]["code"], -32004);
    }
    let unconfigured = McpServer::new(SceneGraph::new(10.0, 10.0));
    let resp = rpc(&unconfigured, &ctx(), "tools/list", json!({})).await;
    assert_eq!(resp["error"]["code"], -32004);
}

/// Pairing swaps the shared directory; the next request sees the new agent
/// without a restart.
#[tokio::test]
async fn swapping_shared_agents_admits_a_new_psk_on_the_next_request() {
    let agents = AgentDirectory::default().shared();
    let server = McpServer::new(SceneGraph::new(10.0, 10.0))
        .with_config(McpConfig::with_agents(agents.clone()));
    let new_agent = CallerContext::with_bearer("new-key");
    let resp = rpc(&server, &new_agent, "tools/list", json!({})).await;
    assert_eq!(resp["error"]["code"], -32004);

    let mut paired = AgentDirectory::default();
    paired.insert("new", hash_psk("new-key"), vec!["*".to_string()]);
    agents.store(std::sync::Arc::new(paired));
    let resp = rpc(&server, &new_agent, "tools/list", json!({})).await;
    assert!(resp["result"]["tools"].is_array(), "{resp}");
}

#[tokio::test]
async fn malformed_json_is_parse_error() {
    let (server, _) = server();
    let raw = server.dispatch("{not json", &ctx()).await;
    assert_eq!(
        serde_json::from_str::<Value>(&raw).unwrap()["error"]["code"],
        -32700
    );
}

// ── Errors ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_structured_error_has_hint_field() {
    let (server, _) = server();
    let err = call_err(
        &server,
        "hud_publish",
        json!({"surface": "zone:nope", "content": "x"}),
    )
    .await;
    assert_eq!(err["code"], "ZONE_NOT_FOUND");
    assert!(err["hint"].as_str().unwrap().contains("subtitle"), "{err}");
    assert_eq!(err.as_object().unwrap().len(), 2, "one shape: {err}");
}

#[tokio::test]
async fn bad_arguments_are_invalid_argument_tool_errors() {
    let (server, _) = server();
    let err = call_err(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "x", "zone_name": "y"}),
    )
    .await;
    assert_eq!(err["code"], "INVALID_ARGUMENT");
    let err = call_err(
        &server,
        "hud_publish",
        json!({"surface": "tile:1", "content": "x"}),
    )
    .await;
    assert_eq!(err["code"], "INVALID_ARGUMENT");
    let err = call_err(&server, "hud_publish", json!({"surface": "zone:subtitle"})).await;
    assert_eq!(err["code"], "INVALID_ARGUMENT");
    let err = call_err(
        &server,
        "hud_publish",
        json!({"surface": "widget:gauge", "content": "x", "params": {}}),
    )
    .await;
    assert_eq!(err["code"], "INVALID_ARGUMENT");
    assert!(err["hint"].as_str().unwrap().contains("content"), "{err}");

    // Typed zone content: a wrong shape fails with a hint, never renders defaults.
    for (surface, content, hint) in [
        (
            "zone:notification-area",
            json!({"notification": {"title": "Hi", "body": "there"}}),
            "not nested",
        ),
        (
            "zone:status-bar",
            json!({"entries": {"a": "1"}, "label": "x"}),
            "allowed: type, entries",
        ),
        (
            "zone:pip",
            json!({"type": "solid_color", "r": 1, "g": 0, "b": 0, "alpha": 1}),
            "allowed: type, r, g, b, a",
        ),
        (
            "zone:subtitle",
            json!({"type": "stream_text", "body": "x"}),
            "allowed: type, text",
        ),
        (
            "zone:notification-area",
            json!({"urgency": 2}),
            "title or body",
        ),
    ] {
        let err = call_err(
            &server,
            "hud_publish",
            json!({"surface": surface, "content": content}),
        )
        .await;
        assert_eq!(err["code"], "INVALID_ARGUMENT", "{content}");
        assert!(err["hint"].as_str().unwrap().contains(hint), "{err}");
    }
}

#[tokio::test]
async fn every_returned_code_is_in_the_closed_set() {
    let (server, _) = server();
    let cases = [
        (
            "hud_publish",
            json!({"surface": "zone:nope", "content": "x"}),
        ),
        (
            "hud_publish",
            json!({"surface": "widget:nope", "params": {}}),
        ),
        (
            "hud_publish",
            json!({"surface": "widget:gauge", "params": {"level": "high"}}),
        ),
        (
            "hud_publish",
            json!({"surface": "portal:p", "content": "x"}),
        ),
        ("hud_hold", json!({"surface": "zone:subtitle", "ttl_ms": 1})),
        ("hud_clear", json!({"surface": "portal:p"})),
        ("hud_input", json!({"bogus": 1})),
    ];
    for (tool, args) in cases {
        let err = call_err(&server, tool, args.clone()).await;
        let code = err["code"].as_str().unwrap();
        assert!(
            crate::error::ERROR_CODES.contains(&code),
            "{tool} {args}: {code}"
        );
    }
}

// ── Allow list ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn surfaces_lists_only_allowed_surfaces() {
    let server = restricted_server();
    let bot = CallerContext::with_bearer("bot-key");
    let (v, _) = call_as(&server, &bot, "hud_surfaces", json!({})).await;
    assert_eq!(
        v["surfaces"][0],
        json!({"s": "zone:subtitle", "accepts": "text"}),
        "held only when true"
    );
    let names: Vec<_> = v["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["s"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["zone:subtitle", "widget:gauge"]);
}

#[tokio::test]
async fn disallowed_surface_returns_not_allowed_with_hint() {
    let server = restricted_server();
    let bot = CallerContext::with_bearer("bot-key");
    let (err, is_error) = call_as(
        &server,
        &bot,
        "hud_publish",
        json!({"surface": "zone:pip", "content": "x"}),
    )
    .await;
    assert!(is_error);
    assert_eq!(err["code"], "NOT_ALLOWED");
    let hint = err["hint"].as_str().unwrap();
    assert!(
        hint.contains("\"zone:pip\"") && hint.contains("[agents.bot]"),
        "{hint}"
    );
    let (err, _) = call_as(
        &server,
        &bot,
        "hud_publish",
        json!({"surface": "portal:p", "content": "x"}),
    )
    .await;
    assert_eq!(err["code"], "NOT_ALLOWED");
}

#[tokio::test]
async fn publish_uses_identity_namespace() {
    let server = restricted_server();
    let bot = CallerContext::with_bearer("bot-key");
    let (_, is_error) = call_as(
        &server,
        &bot,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "hi"}),
    )
    .await;
    assert!(!is_error);
    let scene = server.scene.lock().await;
    assert_eq!(
        scene.zone_registry.active_publishes["subtitle"][0].publisher_namespace,
        "bot"
    );
}

// ── Zones ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn zone_publish_returns_only_ok_and_expiry() {
    let (server, _) = server();
    let v = call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "hello"}),
    )
    .await;
    assert_eq!(v, json!({"ok": true, "expires_in_ms": 60000}));
    let v = call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "held", "ttl_ms": 0}),
    )
    .await;
    assert_eq!(v, json!({"ok": true}));
}

#[tokio::test]
async fn test_hud_publish_zone_ttl_sets_content_expiry_and_is_swept() {
    let (server, clock) = server();
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "expiring", "ttl_ms": 20000}),
    )
    .await;
    let mut scene = server.scene.lock().await;
    let rec = &scene.zone_registry.active_publishes["subtitle"][0];
    assert_eq!(
        rec.expires_at_wall_us,
        Some(21_000_000),
        "ttl_ms becomes an absolute expiry in µs"
    );
    clock.advance(19_000);
    assert_eq!(scene.drain_expired_zone_publications(), 0);
    clock.advance(1_000);
    assert_eq!(scene.drain_expired_zone_publications(), 1);
    assert!(
        scene
            .zone_registry
            .active_publishes
            .get("subtitle")
            .is_none_or(Vec::is_empty)
    );
}

#[tokio::test]
async fn test_hud_publish_zone_contention_policy_latest_wins() {
    let (server, _) = server();
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "first"}),
    )
    .await;
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "second"}),
    )
    .await;
    let scene = server.scene.lock().await;
    let publishes = &scene.zone_registry.active_publishes["subtitle"];
    assert_eq!(publishes.len(), 1);
    assert!(matches!(&publishes[0].content, ZoneContent::StreamText(s) if s == "second"));
}

#[tokio::test]
async fn structured_zone_content_infers_type_and_accepts_body() {
    let (server, _) = server();
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:notification-area", "content": {"title": "Build", "body": "green", "urgency": 1}}),
    )
    .await;
    let scene = server.scene.lock().await;
    let rec = &scene.zone_registry.active_publishes["notification-area"][0];
    assert!(
        matches!(&rec.content, ZoneContent::Notification(n) if n.text == "green"),
        "{:?}",
        rec.content
    );
}

#[tokio::test]
async fn delay_ms_holds_content_until_due() {
    let (server, clock) = server();
    let v = call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "later", "delay_ms": 5000, "ttl_ms": 1000}),
    )
    .await;
    assert_eq!(v["expires_in_ms"], 6000);
    let mut scene = server.scene.lock().await;
    scene.apply_due_batches();
    assert!(
        scene
            .zone_registry
            .active_publishes
            .get("subtitle")
            .is_none_or(Vec::is_empty),
        "not shown before present_at"
    );
    clock.advance(5_000);
    scene.apply_due_batches();
    let rec = &scene.zone_registry.active_publishes["subtitle"][0];
    assert_eq!(
        rec.expires_at_wall_us,
        Some(7_000_000),
        "expiry counts from presentation"
    );
}

/// A delayed notification takes the same expiry as an immediate one: held
/// for `ttl_ms:0` (and `hud_hold` then retimes it once it has materialized),
/// otherwise the explicit expiry stamped at schedule time (the 60 s
/// `DEFAULT_ZONE_TTL_MS` when `ttl_ms` is absent), counted from presentation.
/// That case is unchanged from before the fix.
#[tokio::test]
async fn delayed_notification_expiry_matches_immediate() {
    for (ttl, expected) in [
        (Some(0), None),
        (None, Some(1_000_000 + 5_000_000 + 60_000_000)),
    ] {
        let (server, clock) = server();
        let mut args = json!({"surface": "zone:notification-area",
            "content": {"title": "t", "body": "b"}, "delay_ms": 5000});
        if let Some(ttl) = ttl {
            args["ttl_ms"] = json!(ttl);
        }
        call(&server, "hud_publish", args).await;
        clock.advance(5_000);
        {
            let mut scene = server.scene.lock().await;
            scene.apply_due_batches();
            let rec = &scene.zone_registry.active_publishes["notification-area"][0];
            assert_eq!(rec.expires_at_wall_us, expected, "ttl {ttl:?}");
            clock.advance(120_000);
            let drained = scene.drain_expired_zone_publications();
            assert_eq!(drained, usize::from(expected.is_some()), "ttl {ttl:?}");
        }
        if ttl == Some(0) {
            let v = call(
                &server,
                "hud_hold",
                json!({"surface": "zone:notification-area", "ttl_ms": 5000}),
            )
            .await;
            assert_eq!(v, json!({"ok": true, "expires_in_ms": 5000}));
            let scene = server.scene.lock().await;
            assert_eq!(
                scene.zone_registry.active_publishes["notification-area"][0].expires_at_wall_us,
                Some(scene.now_wall_us() + 5_000_000)
            );
        }
    }
}

/// Two agents that may both use `zone:subtitle`, with their bearers.
fn two_agents() -> (McpServer, TestClock, CallerContext, CallerContext) {
    let clock = TestClock::new(1_000);
    let mut agents = AgentDirectory::default();
    for (id, key) in [("a", "a-key"), ("b", "b-key")] {
        agents.insert(id, hash_psk(key), vec!["publish_zone:subtitle".to_string()]);
    }
    let server = McpServer::new(scene(&clock)).with_config(McpConfig::with_agents(agents.shared()));
    (
        server,
        clock,
        CallerContext::with_bearer("a-key"),
        CallerContext::with_bearer("b-key"),
    )
}

async fn delayed_subtitle(server: &McpServer, who: &CallerContext, delay_ms: u64, ttl_ms: u64) {
    let (v, is_error) = call_as(
        server,
        who,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "x", "delay_ms": delay_ms, "ttl_ms": ttl_ms}),
    )
    .await;
    assert!(!is_error, "{v}");
}

/// `hud_clear` before the due time cancels the caller's pending publish and
/// its wake deadline (idle costs nothing); only the caller's, never another
/// agent's.
#[tokio::test]
async fn clear_cancels_only_own_pending_delayed_publish() {
    let (server, clock, a, b) = two_agents();
    delayed_subtitle(&server, &a, 5000, 0).await;
    delayed_subtitle(&server, &b, 8000, 0).await;
    for who in [&a, &b] {
        let (v, _) = call_as(&server, who, "hud_surfaces", json!({})).await;
        let own = v["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["s"] == "zone:subtitle")
            .unwrap();
        assert_eq!(own["held"], true);
        assert!(own.get("expires_in_ms").is_none());
    }
    let (v, _) = call_as(
        &server,
        &a,
        "hud_clear",
        json!({"surface": "zone:subtitle"}),
    )
    .await;
    assert_eq!(v, json!({"ok": true}));
    for (who, held) in [(&a, false), (&b, true)] {
        let (v, _) = call_as(&server, who, "hud_surfaces", json!({})).await;
        let own = v["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["s"] == "zone:subtitle")
            .unwrap();
        assert_eq!(own.get("held").is_some(), held);
    }
    {
        let scene = server.scene.lock().await;
        assert_eq!(scene.scheduled_batches.len(), 1, "b's batch survives");
        assert_eq!(
            scene.next_timed_content_wall_us(),
            Some(1_000_000 + 8_000_000)
        );
    }
    clock.advance(8_000);
    let mut scene = server.scene.lock().await;
    scene.apply_due_batches();
    let pubs = &scene.zone_registry.active_publishes["subtitle"];
    assert_eq!(pubs.len(), 1);
    assert_eq!(pubs[0].publisher_namespace, "b");
    // Own cancellation leaves no stale deadline behind.
    assert_eq!(scene.next_timed_content_wall_us(), None);
}

/// With nothing else pending, cancelling leaves no wake deadline at all.
#[tokio::test]
async fn clear_of_last_pending_publish_drops_the_wake_deadline() {
    let (server, clock, a, _) = two_agents();
    delayed_subtitle(&server, &a, 5000, 0).await;
    call_as(
        &server,
        &a,
        "hud_clear",
        json!({"surface": "zone:subtitle"}),
    )
    .await;
    clock.advance(10_000);
    let mut scene = server.scene.lock().await;
    assert_eq!(scene.next_timed_content_wall_us(), None);
    scene.apply_due_batches();
    assert!(scene.zone_registry.active_publishes.is_empty());
}

/// `hud_hold` reaches a pending publish: the new ttl counts from
/// presentation (`0` = until cleared), and another agent's hold finds nothing.
#[tokio::test]
async fn hold_retimes_own_pending_delayed_publish() {
    let (server, clock, a, b) = two_agents();
    delayed_subtitle(&server, &a, 5000, 1000).await;
    for (who, expiry) in [(&a, Some(6000)), (&b, None)] {
        let (v, _) = call_as(&server, who, "hud_surfaces", json!({})).await;
        let own = v["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["s"] == "zone:subtitle")
            .unwrap();
        assert_eq!(own.get("expires_in_ms").and_then(Value::as_u64), expiry);
        assert_eq!(own.get("held").is_some(), expiry.is_some());
    }
    let hold = json!({"surface": "zone:subtitle", "ttl_ms": 3000});
    let (err, is_error) = call_as(&server, &b, "hud_hold", hold.clone()).await;
    assert!(is_error);
    assert_eq!(err["code"], "NOT_HELD");
    let (v, _) = call_as(&server, &a, "hud_hold", hold).await;
    assert_eq!(v, json!({"ok": true, "expires_in_ms": 3000}));
    let (v, _) = call_as(&server, &a, "hud_surfaces", json!({})).await;
    let own = v["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["s"] == "zone:subtitle")
        .unwrap();
    assert_eq!(own["expires_in_ms"], 8000);
    clock.advance(5_000);
    let mut scene = server.scene.lock().await;
    scene.apply_due_batches();
    let rec = &scene.zone_registry.active_publishes["subtitle"][0];
    assert_eq!(
        rec.expires_at_wall_us,
        Some(1_000_000 + 5_000_000 + 3_000_000)
    );
}

#[tokio::test]
async fn delay_beyond_horizon_is_timestamp_too_future() {
    let (server, _) = server();
    let err = call_err(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "x", "delay_ms": 300001}),
    )
    .await;
    assert_eq!(err["code"], "TIMESTAMP_TOO_FUTURE");
}

#[tokio::test]
async fn hold_extends_and_requires_a_holding() {
    let (server, _) = server();
    let err = call_err(
        &server,
        "hud_hold",
        json!({"surface": "zone:subtitle", "ttl_ms": 1000}),
    )
    .await;
    assert_eq!(err["code"], "NOT_HELD");
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "x", "ttl_ms": 1000}),
    )
    .await;
    let v = call(
        &server,
        "hud_hold",
        json!({"surface": "zone:subtitle", "ttl_ms": 90000}),
    )
    .await;
    assert_eq!(v, json!({"ok": true, "expires_in_ms": 90000}));
    let scene = server.scene.lock().await;
    assert_eq!(
        scene.zone_registry.active_publishes["subtitle"][0].expires_at_wall_us,
        Some(91_000_000)
    );
}

/// Notifications otherwise get an urgency-derived expiry, which would drop a
/// `ttl_ms:0` notification after 8 s. Held means held until cleared, and a
/// hold re-times (or releases) it.
#[tokio::test]
async fn notification_ttl_zero_is_held_and_hold_retimes_it() {
    let (server, clock) = server();
    let notif = json!({"surface": "zone:notification-area",
        "content": {"title": "t", "body": "b"}});
    let mut held = notif.clone();
    held["ttl_ms"] = json!(0);
    let v = call(&server, "hud_publish", held).await;
    assert_eq!(v, json!({"ok": true}));
    {
        let mut scene = server.scene.lock().await;
        let rec = &scene.zone_registry.active_publishes["notification-area"][0];
        assert_eq!(rec.expires_at_wall_us, None, "no urgency-derived expiry");
        clock.advance(120_000);
        assert_eq!(scene.drain_expired_zone_publications(), 0);
    }
    // Hold with a ttl counts from now; hold 0 releases it again.
    let v = call(
        &server,
        "hud_hold",
        json!({"surface": "zone:notification-area", "ttl_ms": 5000}),
    )
    .await;
    assert_eq!(v, json!({"ok": true, "expires_in_ms": 5000}));
    {
        let scene = server.scene.lock().await;
        assert_eq!(
            scene.zone_registry.active_publishes["notification-area"][0].expires_at_wall_us,
            Some(scene.now_wall_us() + 5_000_000)
        );
    }
    // A shorter hold replaces the expiry (it is not the later of old and new).
    call(
        &server,
        "hud_hold",
        json!({"surface": "zone:notification-area", "ttl_ms": 1000}),
    )
    .await;
    {
        let scene = server.scene.lock().await;
        assert_eq!(
            scene.zone_registry.active_publishes["notification-area"][0].expires_at_wall_us,
            Some(scene.now_wall_us() + 1_000_000)
        );
    }
    call(
        &server,
        "hud_hold",
        json!({"surface": "zone:notification-area", "ttl_ms": 0}),
    )
    .await;
    {
        let mut scene = server.scene.lock().await;
        clock.advance(120_000);
        assert_eq!(scene.drain_expired_zone_publications(), 0);
    }
    call(
        &server,
        "hud_clear",
        json!({"surface": "zone:notification-area"}),
    )
    .await;
    let scene = server.scene.lock().await;
    assert!(
        scene
            .zone_registry
            .active_publishes
            .get("notification-area")
            .is_none_or(Vec::is_empty)
    );
}

#[tokio::test]
async fn clear_removes_own_zone_content() {
    let (server, _) = server();
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "x"}),
    )
    .await;
    assert_eq!(
        call(&server, "hud_clear", json!({"surface": "zone:subtitle"})).await,
        json!({"ok": true})
    );
    let scene = server.scene.lock().await;
    assert!(
        scene
            .zone_registry
            .active_publishes
            .get("subtitle")
            .is_none_or(Vec::is_empty)
    );
}

#[tokio::test]
async fn surfaces_report_holdings_compactly() {
    let (server, _) = server();
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "x", "ttl_ms": 8000}),
    )
    .await;
    let v = call(&server, "hud_surfaces", json!({})).await;
    let entries = v["surfaces"].as_array().unwrap();
    let sub = entries.iter().find(|e| e["s"] == "zone:subtitle").unwrap();
    assert_eq!(
        sub,
        &json!({"s": "zone:subtitle", "accepts": "text", "held": true, "expires_in_ms": 8000})
    );
    let gauge = entries.iter().find(|e| e["s"] == "widget:gauge").unwrap();
    assert_eq!(
        gauge["params"],
        json!({"level": "f32 0..1", "label": "string", "fill_color": "color"})
    );
    let text = v.to_string();
    assert!(
        !text.contains("-4") && !text.contains("_us"),
        "no ids or timestamps: {text}"
    );
}

#[tokio::test]
async fn mcp_lease_is_reused_across_publishes() {
    let (server, _) = server();
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "a"}),
    )
    .await;
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:pip", "content": {"type": "solid_color", "r": 1, "g": 0, "b": 0}}),
    )
    .await;
    call(
        &server,
        "hud_publish",
        json!({"surface": "widget:gauge", "params": {"level": 0.5}}),
    )
    .await;
    let scene = server.scene.lock().await;
    assert_eq!(scene.leases.len(), 1, "one MCP lease per agent");
}

// ── Widgets ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn widget_publish_hold_clear() {
    let (server, clock) = server();
    let mut config = McpConfig::with_psk(psk());
    config.widget_transition_ms = 200;
    let server = server.with_config(config);
    let invalid = call_err(
        &server,
        "hud_publish",
        json!({"surface":"widget:gauge","params":{"level":2.0}}),
    )
    .await;
    assert_eq!(invalid["code"], "WIDGET_PARAMETER_INVALID");
    assert!(server.scene.lock().await.leases.is_empty());
    let v = call(
        &server,
        "hud_publish",
        json!({"surface": "widget:gauge", "params": {"level": 0.75, "label": "CPU",
            "fill_color": {"r": 1.0, "g": 0.0, "b": 0.0, "a": 1.0}}}),
    )
    .await;
    assert_eq!(v, json!({"ok": true}), "widgets are durable by default");
    {
        let scene = server.scene.lock().await;
        let record = &scene.widget_registry.active_publishes["gauge"][0];
        assert_eq!(record.transition_ms, 200);
        assert_eq!(record.params["level"], WidgetParameterValue::F32(0.75));
        assert_eq!(
            record.params["label"],
            WidgetParameterValue::String("CPU".into())
        );
        assert_eq!(
            record.params["fill_color"],
            WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0))
        );
    }
    let before = {
        let scene = server.scene.lock().await;
        (
            scene.version,
            scene.widget_registry.instances["gauge"]
                .current_params
                .clone(),
            scene.widget_registry.active_publishes["gauge"][0]
                .params
                .clone(),
            scene.leases.clone(),
        )
    };
    clock.advance(1000);
    let mut bad = vec![
        (json!({"level": -0.1}), "0..1"),
        (json!({"level": 2.0}), "0..1"),
        (json!({"level": 1.0000000001}), "0..1"),
        (json!({"level": 1e100}), "finite"),
        (json!({"level": "high"}), "f32"),
    ];
    for value in [
        json!({}),
        json!({"r": 0, "g": 1}),
        json!({"r": "0", "g": 1, "b": 0}),
        json!({"r": 0, "g": 1, "b": 0, "a": null}),
        json!("#123"),
        json!("#12345678"),
        json!("#GG0000"),
        json!("red"),
        json!("#ééé"),
        json!([0, 1, 0]),
    ] {
        bad.push((json!({"fill_color": value}), "0..1"));
    }
    for channel in ["r", "g", "b", "a"] {
        for value in [json!(true), json!("0.5"), json!(null)] {
            let mut color = json!({"r": 0.2, "g": 0.3, "b": 0.4, "a": 1});
            color[channel] = value;
            bad.push((json!({"fill_color": color}), "numeric"));
        }
        if channel != "a" {
            let mut color = json!({"r": 0.2, "g": 0.3, "b": 0.4});
            color.as_object_mut().unwrap().remove(channel);
            bad.push((json!({"fill_color": color}), "numeric"));
        }
        for value in [-0.1, 1.0000000001, 255.0, 1e100] {
            let mut color = json!({"r": 0.2, "g": 0.3, "b": 0.4, "a": 1});
            color[channel] = json!(value);
            bad.push((json!({"fill_color": color}), "0..1"));
        }
    }
    for (mut params, hint) in bad {
        params["label"] = json!("changed");
        for _ in 0..2 {
            let error = call_err(
                &server,
                "hud_publish",
                json!({"surface": "widget:gauge", "params": params}),
            )
            .await;
            assert_eq!(error["code"], "WIDGET_PARAMETER_INVALID", "{error}");
            assert!(error["hint"].as_str().unwrap().contains(hint), "{error}");
            let scene = server.scene.lock().await;
            assert_eq!(scene.version, before.0);
            assert_eq!(scene.leases, before.3);
            assert_eq!(
                scene.widget_registry.instances["gauge"].current_params,
                before.1
            );
            let pubs = &scene.widget_registry.active_publishes["gauge"];
            assert_eq!(pubs.len(), 1);
            assert_eq!(pubs[0].params, before.2);
            assert_eq!(pubs[0].transition_ms, 200);
        }
    }
    for (level, color, expected) in [
        (
            0.0,
            json!("#4A9eFF"),
            Rgba::new(74.0 / 255.0, 158.0 / 255.0, 1.0, 1.0),
        ),
        (
            0.5,
            json!({"r": 0, "g": 1, "b": 0.5}),
            Rgba::new(0.0, 1.0, 0.5, 1.0),
        ),
        (
            1.0,
            json!({"r": 1, "g": 0, "b": 0, "a": 0}),
            Rgba::new(1.0, 0.0, 0.0, 0.0),
        ),
    ] {
        call(
            &server,
            "hud_publish",
            json!({"surface":"widget:gauge", "params":{"level":level, "fill_color":color}}),
        )
        .await;
        assert_eq!(
            server.scene.lock().await.widget_registry.instances["gauge"].current_params["fill_color"],
            WidgetParameterValue::Color(expected)
        );
    }
    let err = call_err(
        &server,
        "hud_publish",
        json!({"surface": "widget:gauge", "params": {"nope": 1}}),
    )
    .await;
    assert_eq!(err["code"], "WIDGET_PARAMETER_INVALID");
    let v = call(
        &server,
        "hud_hold",
        json!({"surface": "widget:gauge", "ttl_ms": 5000}),
    )
    .await;
    assert_eq!(v["expires_in_ms"], 5000);
    call(&server, "hud_clear", json!({"surface": "widget:gauge"})).await;
    let scene = server.scene.lock().await;
    assert!(
        scene
            .widget_registry
            .active_publishes
            .get("gauge")
            .is_none_or(Vec::is_empty)
    );
    drop(scene);
    let mut instant = McpConfig::with_psk(psk());
    instant.widget_transition_ms = 0;
    let server = server.with_config(instant);
    call(
        &server,
        "hud_publish",
        json!({"surface":"widget:gauge", "params":{"level":0.75}}),
    )
    .await;
    assert_eq!(
        server.scene.lock().await.widget_registry.active_publishes["gauge"][0].transition_ms,
        0
    );
}

// ── Input: notification actions ──────────────────────────────────────────────

#[tokio::test]
async fn action_presses_are_delivered_until_acked() {
    let (server, _) = server();
    server
        .scene
        .lock()
        .await
        .push_pending_action(PendingAction {
            publisher_namespace: AgentDirectory::unrestricted(psk())
                .resolve(&psk(), "")
                .unwrap()
                .agent_id,
            zone_name: "notification-area".into(),
            callback_id: "approve".into(),
        });
    let v = call(&server, "hud_input", json!({})).await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["s"], "zone:notification-area");
    assert_eq!(items[0]["action"], "approve");
    let id = items[0]["id"].clone();
    let again = call(&server, "hud_input", json!({})).await;
    assert_eq!(again["items"][0]["id"], id, "unacked items are redelivered");
    let acked = call(&server, "hud_input", json!({"ack": [id]})).await;
    assert_eq!(acked, json!({"items": [], "remaining": 0}));
}

// ── Portal ───────────────────────────────────────────────────────────────────

/// Share the real portal hub directly, with the deterministic test wall clock.
fn hub_portal(server: McpServer) -> (McpServer, Arc<std::sync::Mutex<PortalHub>>) {
    let portals = PortalHandle::new(
        PortalHub::default(),
        Arc::new(tze_hud_scene::TestClock::new(0)),
    );
    let hub = Arc::clone(&portals.hub);
    (server.with_portals(portals), hub)
}

#[tokio::test]
async fn portal_output_too_large_is_content_rejected() {
    let (server, _) = server();
    let (server, hub) = hub_portal(server);
    let e = call_err(
        &server,
        "hud_publish",
        json!({"surface": "portal:main", "content": "x".repeat(16 * 1024 + 1), "ttl_ms": 0}),
    )
    .await;
    assert_eq!(e["code"], "CONTENT_REJECTED");
    assert!(!e["hint"].as_str().unwrap().is_empty());
    assert!(
        hub.lock()
            .unwrap()
            .get(&PortalKey::new(psk_agent(), "main"))
            .is_none()
    );
}

#[tokio::test]
async fn portal_flow_attach_publish_poll_ack_clear() {
    let (server, _) = server();
    let (server, hub) = hub_portal(server);
    let key = PortalKey::new(psk_agent(), "main");

    let empty = call(&server, "hud_publish", json!({"surface": "portal:main"})).await;
    assert_eq!(
        empty,
        json!({"ok": true, "reattached": false, "pending_input": 0})
    );
    assert!(hub.lock().unwrap().get(&key).unwrap().transcript.is_empty());

    // The first publish attaches; identity is the caller's PSK, no token.
    let v = call(&server, "hud_publish", json!({"surface": "portal:main", "content": "Working on it", "status": "active", "expects_reply": true})).await;
    assert_eq!(
        v,
        json!({"ok": true, "reattached": false, "pending_input": 0})
    );
    call(
        &server,
        "hud_publish",
        json!({"surface": "portal:main", "content": "Done?"}),
    )
    .await;
    {
        let hub = hub.lock().unwrap();
        let portal = hub.get(&key).expect("attached under the caller's id");
        let units: Vec<_> = portal
            .transcript
            .iter()
            .map(|u| (u.text.as_str(), u.expects_reply))
            .collect();
        assert_eq!(units, [("Working on it", true), ("Done?", false)]);
        assert_eq!(portal.status, PortalStatus::Active);
    }

    let id = hub
        .lock()
        .unwrap()
        .submit_reply(&key, "ship it".into(), 0)
        .unwrap();
    let status = call(
        &server,
        "hud_publish",
        json!({"surface": "portal:main", "status": "degraded", "ttl_ms": 120000}),
    )
    .await;
    assert_eq!(
        status,
        json!({"ok": true, "reattached": false, "pending_input": 1, "expires_in_ms": 120000})
    );
    assert_eq!(
        hub.lock().unwrap().get(&key).unwrap().hold_until_us,
        Some(120_000_000)
    );
    for bad in [
        json!({"surface": "portal:main", "status": "attached", "ttl_ms": 0}),
        json!({"surface": "portal:main", "status": "detached", "ttl_ms": 0}),
        json!({"surface": "portal:main", "status": "hud_unavailable", "ttl_ms": 0}),
        json!({"surface": "portal:main", "content": {}, "ttl_ms": 0}),
    ] {
        let e = call_err(&server, "hud_publish", bad).await;
        assert_eq!(e["code"], "INVALID_ARGUMENT");
        let hub = hub.lock().unwrap();
        let p = hub.get(&key).unwrap();
        assert_eq!(p.transcript.len(), 2);
        assert_eq!(p.status, PortalStatus::Degraded);
        assert_eq!(p.hold_until_us, Some(120_000_000));
    }
    let e = call_err(
        &server,
        "hud_clear",
        json!({"surface": "portal:main", "reason": "done"}),
    )
    .await;
    assert_eq!(e["code"], "INVALID_ARGUMENT");
    call(
        &server,
        "hud_publish",
        json!({"surface": "portal:main", "status": "active"}),
    )
    .await;
    let v = call(&server, "hud_surfaces", json!({})).await;
    let portal = v["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["s"] == "portal:main")
        .unwrap()
        .clone();
    assert_eq!(
        portal,
        json!({"s": "portal:main", "state": "active", "pending_input": 1})
    );

    let v = call(&server, "hud_input", json!({"wait_ms": 1000})).await;
    assert_eq!(
        v,
        json!({"items": [{"id": id, "s": "portal:main", "text": "ship it"}], "remaining": 0})
    );
    let v = call(&server, "hud_input", json!({})).await;
    assert_eq!(
        v["items"][0]["id"], id,
        "unacked portal input is redelivered"
    );
    let v = call(&server, "hud_input", json!({"ack": [id]})).await;
    assert_eq!(v, json!({"items": [], "remaining": 0}));

    for text in ["progress 1", "progress 2"] {
        call(
            &server,
            "hud_publish",
            json!({"surface": "portal:main", "content": text, "key": "progress", "ttl_ms": 0}),
        )
        .await;
    }
    {
        let hub = hub.lock().unwrap();
        let p = hub.get(&key).unwrap();
        assert_eq!(p.transcript.len(), 3);
        assert_eq!(p.transcript.back().unwrap().text, "progress 2");
        assert_eq!(p.hold_until_us, Some(u64::MAX));
    }

    let v = call(
        &server,
        "hud_hold",
        json!({"surface": "portal:main", "ttl_ms": 120000}),
    )
    .await;
    assert_eq!(v["ok"], true);
    assert_eq!(
        hub.lock().unwrap().get(&key).unwrap().hold_until_us,
        Some(120_000_000),
        "portal hold reaches the runtime"
    );

    call(&server, "hud_clear", json!({"surface": "portal:main"})).await;
    assert!(hub.lock().unwrap().get(&key).is_none());
    for (verb, args) in [
        ("hud_hold", json!({"surface": "portal:main", "ttl_ms": 1})),
        ("hud_clear", json!({"surface": "portal:main"})),
    ] {
        assert_eq!(call_err(&server, verb, args).await["code"], "NOT_HELD");
    }

    // The real zero-portal caller polls the shared Hub across several passes.
    // An unrelated pre-existing dirty portal must neither wake nor get touched.
    use std::sync::atomic::{AtomicUsize, Ordering};
    let ingress = Arc::new(AtomicUsize::new(0));
    let render = Arc::new(AtomicUsize::new(0));
    let locked_notification = Arc::new(AtomicUsize::new(0));
    let server = server
        .with_portal_ingress_wake_notifier(tze_hud_scene::render_wake::RenderWakeNotifier::new({
            let ingress = Arc::clone(&ingress);
            let hub = Arc::clone(&hub);
            let locked_notification = Arc::clone(&locked_notification);
            move || {
                ingress.fetch_add(1, Ordering::SeqCst);
                if hub.try_lock().is_err() {
                    locked_notification.fetch_add(1, Ordering::SeqCst);
                }
            }
        }))
        .with_render_wake_notifier(tze_hud_scene::render_wake::RenderWakeNotifier::new({
            let render = Arc::clone(&render);
            move || {
                render.fetch_add(1, Ordering::SeqCst);
            }
        }));
    let other = PortalKey::new("another-agent", "main");
    hub.lock()
        .unwrap()
        .publish(
            &other,
            tze_hud_projection::hub::Publish {
                text: Some("untouched other-agent output".into()),
                ..Default::default()
            },
            0,
        )
        .unwrap();
    let other_before = hub.lock().unwrap().get(&other).unwrap().clone();
    let empty = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        call(&server, "hud_input", json!({"wait_ms": 350})),
    )
    .await
    .unwrap();
    assert_eq!(empty, json!({"items": [], "remaining": 0}));
    assert_eq!(
        ingress.load(Ordering::SeqCst),
        0,
        "empty passes do not ring main work"
    );
    assert_eq!(
        render.load(Ordering::SeqCst),
        0,
        "empty passes do not ring rendering"
    );
    assert_eq!(
        format!("{:?}", hub.lock().unwrap().get(&other).unwrap()),
        format!("{other_before:?}")
    );

    // First attachment arrives after earlier empty passes; no cached zero count.
    let waiting = call(&server, "hud_input", json!({"wait_ms": 1500}));
    let first_arrival = async {
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        assert_eq!(
            call(
                &server,
                "hud_publish",
                json!({"surface":"portal:main", "content":"first during wait"})
            )
            .await,
            json!({"ok":true, "reattached":false, "pending_input":0})
        );
        hub.lock()
            .unwrap()
            .submit_reply(&key, "reply during wait".into(), 0)
            .unwrap()
    };
    let (found, first_id) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(waiting, first_arrival)
    })
    .await
    .unwrap();
    assert_eq!(
        found,
        json!({"items":[{"id":first_id,"s":"portal:main","text":"reply during wait"}],"remaining":0})
    );
    assert_eq!(
        ingress.load(Ordering::SeqCst),
        2,
        "publish and actual first delivery wake after unlock"
    );
    call(&server, "hud_surfaces", json!({})).await;
    assert_eq!(ingress.load(Ordering::SeqCst), 2, "discovery is read-only");
    assert_eq!(
        call(&server, "hud_input", json!({})).await["items"][0]["id"],
        first_id
    );
    assert_eq!(
        ingress.load(Ordering::SeqCst),
        2,
        "redelivery of already-dirty content changes nothing"
    );
    assert_eq!(
        call(&server, "hud_input", json!({"ack":[first_id]})).await,
        json!({"items":[],"remaining":0})
    );
    assert_eq!(
        ingress.load(Ordering::SeqCst),
        3,
        "ack changes rendered pending input"
    );
    server
        .scene
        .lock()
        .await
        .push_pending_action(PendingAction {
            publisher_namespace: psk_agent(),
            zone_name: "notification-area".into(),
            callback_id: "approve".into(),
        });
    let action = call(&server, "hud_input", json!({})).await;
    assert_eq!(action["items"].as_array().unwrap().len(), 1);
    assert_eq!(action["items"][0]["action"], "approve");
    let action_id = action["items"][0]["id"].clone();
    assert_eq!(
        call(&server, "hud_input", json!({})).await["items"][0]["id"],
        action_id
    );
    assert_eq!(
        call(&server, "hud_input", json!({"ack":[action_id]})).await,
        json!({"items":[],"remaining":0})
    );
    assert_eq!(
        ingress.load(Ordering::SeqCst),
        3,
        "zone actions do not create portal work"
    );
    assert_eq!(render.load(Ordering::SeqCst), 0);
    assert_eq!(
        locked_notification.load(Ordering::SeqCst),
        0,
        "callbacks run outside the Hub guard"
    );
    assert_eq!(
        format!("{:?}", hub.lock().unwrap().get(&other).unwrap()),
        format!("{other_before:?}")
    );
    // Portal hold/clear ring main work after the Hub guard, never pre-driver rendering.
    assert_eq!(
        call(
            &server,
            "hud_hold",
            json!({"surface":"portal:main", "ttl_ms":1000})
        )
        .await,
        json!({"ok":true,"expires_in_ms":1000})
    );
    assert_eq!(ingress.load(Ordering::SeqCst), 4);
    assert_eq!(render.load(Ordering::SeqCst), 0);
    assert_eq!(locked_notification.load(Ordering::SeqCst), 0);
    assert_eq!(
        call(&server, "hud_clear", json!({"surface":"portal:main"})).await,
        json!({"ok":true})
    );
    assert!(hub.lock().unwrap().get(&key).is_none());
    assert_eq!(ingress.load(Ordering::SeqCst), 5);
    assert_eq!(render.load(Ordering::SeqCst), 0);
    assert_eq!(locked_notification.load(Ordering::SeqCst), 0);
    assert_eq!(
        format!("{:?}", hub.lock().unwrap().get(&other).unwrap()),
        format!("{other_before:?}")
    );
}

#[tokio::test]
async fn portal_without_authority_is_unavailable() {
    let (server, _) = server();
    let err = call_err(
        &server,
        "hud_publish",
        json!({"surface": "portal:main", "content": "x"}),
    )
    .await;
    assert_eq!(err["code"], "UNAVAILABLE");

    let portals = PortalHandle::default();
    let hub = Arc::clone(&portals.hub);
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = hub.lock().unwrap();
        panic!("fixture poison");
    }));
    assert!(poisoned.is_err() && hub.is_poisoned());
    let server = server.with_portals(portals);
    for (verb, args) in [
        (
            "hud_publish",
            json!({"surface":"portal:main","content":"x"}),
        ),
        ("hud_hold", json!({"surface":"portal:main","ttl_ms":0})),
        ("hud_clear", json!({"surface":"portal:main"})),
        ("hud_input", json!({})),
        ("hud_surfaces", json!({})),
    ] {
        let err = call_err(&server, verb, args).await;
        assert_eq!(err["code"], "INTERNAL", "present failed Hub refuses {verb}");
        assert!(!err["hint"].as_str().unwrap().is_empty());
    }
}

fn psk_agent() -> String {
    AgentDirectory::unrestricted(psk())
        .resolve(&psk(), "")
        .unwrap()
        .agent_id
}

#[tokio::test]
async fn status_bar_keys_merge_and_update() {
    let (server, _) = server();
    for (key, value) in [("weather", "72F"), ("build", "green"), ("weather", "75F")] {
        call(
            &server,
            "hud_publish",
            json!({"surface": "zone:status-bar", "content": {"entries": {key: value}}, "key": key}),
        )
        .await;
    }
    let scene = server.scene.lock().await;
    let publishes = &scene.zone_registry.active_publishes["status-bar"];
    assert_eq!(publishes.len(), 2, "same key replaces, new key adds");
    let weather = publishes
        .iter()
        .find(|r| r.merge_key.as_deref() == Some("weather"))
        .unwrap();
    assert!(matches!(&weather.content, ZoneContent::StatusBar(p) if p.entries["weather"] == "75F"));
}

// ── Safe mode ────────────────────────────────────────────────────────────────

fn set_safe_mode(server: &McpServer, on: bool) {
    server
        .safe_mode
        .store(on, std::sync::atomic::Ordering::Release);
}

#[tokio::test]
async fn hud_publish_in_safe_mode_returns_safe_mode_active() {
    let (server, _) = server();
    let (server, hub) = hub_portal(server);
    // Held before safe mode, so hud_hold has something to extend.
    for surface in ["zone:subtitle", "widget:gauge"] {
        let args = match surface {
            "widget:gauge" => json!({"surface": surface, "params": {"level": 0.5}}),
            _ => json!({"surface": surface, "content": "a"}),
        };
        call(&server, "hud_publish", args).await;
    }
    call(
        &server,
        "hud_publish",
        json!({"surface": "portal:main", "content": "before"}),
    )
    .await;
    server
        .scene
        .lock()
        .await
        .push_pending_action(PendingAction {
            publisher_namespace: AgentDirectory::unrestricted(psk())
                .resolve(&psk(), "")
                .unwrap()
                .agent_id,
            zone_name: "notification-area".into(),
            callback_id: "approve".into(),
        });

    set_safe_mode(&server, true);
    for surface in ["zone:subtitle", "widget:gauge", "portal:main"] {
        let publish = match surface {
            "widget:gauge" => json!({"surface": surface, "params": {"level": 0.9}}),
            _ => json!({"surface": surface, "content": "during"}),
        };
        let err = call_err(&server, "hud_publish", publish).await;
        assert_eq!(err["code"], "SAFE_MODE_ACTIVE", "publish {surface}");
        let err = call_err(
            &server,
            "hud_hold",
            json!({"surface": surface, "ttl_ms": 1000}),
        )
        .await;
        assert_eq!(err["code"], "SAFE_MODE_ACTIVE", "hold {surface}");
    }
    let key = PortalKey::new(psk_agent(), "main");
    assert_eq!(
        hub.lock().unwrap().get(&key).unwrap().transcript.len(),
        1,
        "nothing reached the portal driver during safe mode"
    );

    // Reading stays open: already-queued input is still delivered.
    let v = call(&server, "hud_input", json!({})).await;
    assert_eq!(v["items"][0]["action"], "approve");
}

#[tokio::test]
async fn safe_mode_does_not_regrant_suspended_mcp_lease() {
    let (server, _) = server();
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "a"}),
    )
    .await;
    // The runtime suspends leases on safe-mode entry.
    let lease = {
        let mut scene = server.scene.lock().await;
        let id = *scene.leases.keys().next().expect("one MCP lease");
        scene.suspend_lease(&id, 1).expect("suspend");
        id
    };
    for args in [
        json!({"surface": "zone:subtitle", "content": "b"}),
        json!({"surface": "widget:gauge", "params": {"level": 0.5}}),
    ] {
        let err = call_err(&server, "hud_publish", args).await;
        assert_eq!(err["code"], "SAFE_MODE_ACTIVE");
    }
    let scene = server.scene.lock().await;
    assert_eq!(
        scene.leases.len(),
        1,
        "no fresh lease around the Suspended one"
    );
    assert_eq!(scene.leases[&lease].state, LeaseState::Suspended);
}

#[tokio::test]
async fn resume_restores_mcp_publishing() {
    let (server, _) = server();
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "a"}),
    )
    .await;
    set_safe_mode(&server, true);
    let lease = {
        let mut scene = server.scene.lock().await;
        let id = *scene.leases.keys().next().unwrap();
        scene.suspend_lease(&id, 1).unwrap();
        id
    };
    call_err(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "b"}),
    )
    .await;

    set_safe_mode(&server, false);
    server.scene.lock().await.resume_lease(&lease, 2).unwrap();
    call(
        &server,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "c"}),
    )
    .await;
    call(
        &server,
        "hud_publish",
        json!({"surface": "widget:gauge", "params": {"level": 0.5}}),
    )
    .await;
    let scene = server.scene.lock().await;
    assert_eq!(scene.leases.len(), 1, "the original lease serves again");
}

#[tokio::test]
async fn user_test_zone_fixtures_and_documented_examples_match_production_parser() {
    fn zone_messages(value: Value, origin: &str) -> Vec<(String, Value)> {
        if let Some(surface) = value.get("surface").and_then(Value::as_str) {
            return value
                .get("content")
                .map(|content| vec![(surface.to_string(), content.clone())])
                .unwrap_or_default();
        }
        if value.is_object() {
            return Vec::new(); // Other API response examples have no publish content.
        }
        value
            .as_array()
            .unwrap_or_else(|| panic!("{origin}: expected a message array"))
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                assert!(message.is_object(), "{origin}[{index}]: expected a message");
                if message.get("zone").is_none() && message.get("content").is_none() {
                    return None; // Widget messages have no zone content.
                }
                let zone = message["zone"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{origin}[{index}]: missing zone"));
                let content = message
                    .get("content")
                    .unwrap_or_else(|| panic!("{origin}[{index}]: missing content"));
                Some((format!("zone:{zone}"), content.clone()))
            })
            .collect()
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let skill = root.join(".claude/skills/user-test");
    let mut fixtures: Vec<_> = std::fs::read_dir(skill.join("scripts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    fixtures.sort();
    let mut messages = Vec::new();
    for path in fixtures {
        let value = serde_json::from_slice(&std::fs::read(&path).unwrap())
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        messages.extend(zone_messages(value, &path.display().to_string()));
    }
    assert!(
        !messages.is_empty(),
        "no user-test zone fixtures were checked"
    );
    let fixture_count = messages.len();

    for path in [
        skill.join("references/message-payloads.md"),
        root.join("docs/api.md"),
    ] {
        let reference = std::fs::read_to_string(&path).unwrap();
        let before = messages.len();
        let mut block = None::<String>;
        for line in reference.lines() {
            match line.trim() {
                "```json" => {
                    assert!(block.is_none(), "nested JSON fence in {}", path.display());
                    block = Some(String::new());
                }
                "```" => {
                    if let Some(text) = block.take() {
                        let value = serde_json::from_str(&text)
                            .expect("documented JSON examples must remain executable");
                        messages.extend(zone_messages(value, &path.display().to_string()));
                    }
                }
                _ => {
                    if let Some(text) = block.as_mut() {
                        text.push_str(line);
                        text.push('\n');
                    }
                }
            }
        }
        assert!(
            block.is_none(),
            "unterminated JSON example in {}",
            path.display()
        );
        assert!(
            messages.len() > before,
            "no publish example in {}",
            path.display()
        );
    }
    assert!(
        messages.len() > fixture_count,
        "no documented zone examples were checked"
    );

    // Exercise hud_publish's real type inference, strict parser and blank prefilter.
    // A fresh CPU scene keeps stack capacity and TTL separate from content validity.
    for (surface, content) in messages {
        let (server, _) = server();
        call(
            &server,
            "hud_publish",
            json!({"surface": surface, "content": content}),
        )
        .await;
    }

    for (surface, content) in [
        (
            "zone:notification-area",
            json!({"type": "notification", "body": "valid"}),
        ),
        (
            "zone:status-bar",
            json!({"type": "status_bar", "entries": {"build": "passing"}}),
        ),
        (
            "zone:pip",
            json!({"type": "solid_color", "r": 0.2, "g": 0.6, "b": 0.9}),
        ),
        (
            "zone:subtitle",
            json!({"type": "stream_text", "text": "valid"}),
        ),
    ] {
        let mut unknown = content.clone();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("unexpected_fixture_key".into(), json!(true));
        let kind = content["type"].as_str().unwrap();
        let mut nested = serde_json::Map::new();
        nested.insert("type".into(), json!(kind));
        nested.insert(kind.into(), content);
        for (bad, hint) in [
            (unknown, "unknown field"),
            (Value::Object(nested), "not nested"),
        ] {
            let (server, _) = server();
            let error = call_err(
                &server,
                "hud_publish",
                json!({"surface": surface, "content": bad}),
            )
            .await;
            assert_eq!(error["code"], "INVALID_ARGUMENT", "{error}");
            assert!(error["hint"].as_str().unwrap().contains(hint), "{error}");
        }
    }
    for (surface, content, hint) in [
        ("zone:subtitle", json!(""), "needs content"),
        (
            "zone:subtitle",
            json!({"type": "stream_text", "text": ""}),
            "non-empty",
        ),
        (
            "zone:notification-area",
            json!({"urgency": 1}),
            "title or body",
        ),
        ("zone:pip", json!({"type": "solid_color"}), "numeric"),
    ] {
        let (server, _) = server();
        let error = call_err(
            &server,
            "hud_publish",
            json!({"surface": surface, "content": content}),
        )
        .await;
        assert_eq!(error["code"], "INVALID_ARGUMENT", "{error}");
        assert!(error["hint"].as_str().unwrap().contains(hint), "{error}");
    }
    let mut invalid = Vec::new();
    for urgency in [
        json!("high"),
        json!(-1),
        json!(0.5),
        json!(true),
        json!(null),
        json!(4),
        json!(4294967297_u64),
        json!(1e100),
    ] {
        invalid.push((
            "zone:notification-area",
            json!({"body":"valid", "urgency":urgency}),
            "urgency",
        ));
    }
    for ttl in [json!(1000), json!(0), json!(null), json!("1000"), json!({})] {
        invalid.push((
            "zone:notification-area",
            json!({"body":"valid", "ttl_ms":ttl}),
            "top-level",
        ));
    }
    for value in [json!(1), json!(true), json!(null), json!({}), json!([])] {
        invalid.push((
            "zone:status-bar",
            json!({"entries":{"good":"ok", "bad":value}}),
            "entries.bad",
        ));
    }
    for (surface, content, hint) in invalid {
        let (server, clock) = server();
        let before = server.scene.lock().await.version;
        for _ in 0..2 {
            let error = call_err(
                &server,
                "hud_publish",
                json!({"surface":surface,"content":content,"delay_ms":1000}),
            )
            .await;
            assert_eq!(error["code"], "INVALID_ARGUMENT", "{error}");
            assert!(error["hint"].as_str().unwrap().contains(hint), "{error}");
            clock.advance(2000);
            let mut scene = server.scene.lock().await;
            assert_eq!(scene.version, before);
            assert!(
                scene.leases.is_empty(),
                "bad content must not grant a lease"
            );
            assert!(
                scene.apply_due_batches().is_empty(),
                "bad content must not schedule"
            );
        }
        let seed = if surface == "zone:status-bar" {
            json!({"entries":{"existing":"kept"}})
        } else {
            json!({"body":"kept","urgency":1})
        };
        call(
            &server,
            "hud_publish",
            json!({"surface":surface,"content":seed,"ttl_ms":0}),
        )
        .await;
        let zone = surface.strip_prefix("zone:").unwrap();
        let before = {
            let scene = server.scene.lock().await;
            (
                scene.version,
                scene.leases.clone(),
                scene.zone_registry.active_publishes[zone][0]
                    .content
                    .clone(),
            )
        };
        let error = call_err(
            &server,
            "hud_publish",
            json!({"surface":surface,"content":content,"delay_ms":1000}),
        )
        .await;
        assert_eq!(error["code"], "INVALID_ARGUMENT", "{error}");
        assert!(error["hint"].as_str().unwrap().contains(hint), "{error}");
        clock.advance(2000);
        let mut scene = server.scene.lock().await;
        assert_eq!(scene.version, before.0);
        assert_eq!(scene.leases, before.1);
        assert!(scene.apply_due_batches().is_empty());
        let pubs = &scene.zone_registry.active_publishes[zone];
        assert_eq!(pubs.len(), 1);
        assert_eq!(pubs[0].content, before.2);
        assert_eq!(pubs[0].expires_at_wall_us, None);
    }
    for urgency in [None, Some(0), Some(1), Some(2), Some(3)] {
        let (server, _) = server();
        let mut content = json!({"body":"valid"});
        if let Some(urgency) = urgency {
            content["urgency"] = json!(urgency);
        }
        call(
            &server,
            "hud_publish",
            json!({"surface":"zone:notification-area","content":content,"ttl_ms":1000}),
        )
        .await;
        let scene = server.scene.lock().await;
        let record = &scene.zone_registry.active_publishes["notification-area"][0];
        let ZoneContent::Notification(payload) = &record.content else {
            panic!("notification")
        };
        assert_eq!(payload.urgency, urgency.unwrap_or(1));
        assert_eq!(record.expires_at_wall_us, Some(2_000_000));
    }
}
