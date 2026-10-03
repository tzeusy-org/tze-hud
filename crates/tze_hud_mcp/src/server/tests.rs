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
/// the default ttl counted from presentation otherwise.
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
    let (server, _) = server();
    let v = call(
        &server,
        "hud_publish",
        json!({"surface": "widget:gauge", "params": {"level": 0.75, "label": "CPU"}}),
    )
    .await;
    assert_eq!(v, json!({"ok": true}), "widgets are durable by default");
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

/// Run a portal hub answering `PortalOp`s, as the runtime's driver does.
fn hub_portal(server: McpServer) -> (McpServer, Arc<std::sync::Mutex<PortalHub>>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PortalOp>();
    let hub = Arc::new(std::sync::Mutex::new(PortalHub::default()));
    let h = Arc::clone(&hub);
    tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            op.apply(&mut h.lock().unwrap(), 0);
        }
    });
    (server.with_portal_op_tx(tx), hub)
}

#[tokio::test]
async fn portal_output_too_large_is_content_rejected() {
    let (server, _) = server();
    let (server, _) = hub_portal(server);
    let e = call_err(
        &server,
        "hud_publish",
        json!({"surface": "portal:main", "content": "x".repeat(16 * 1024 + 1)}),
    )
    .await;
    assert_eq!(e["code"], "CONTENT_REJECTED");
    assert!(!e["hint"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn portal_flow_attach_publish_poll_ack_clear() {
    let (server, _) = server();
    let (server, hub) = hub_portal(server);
    let key = PortalKey::new(psk_agent(), "main");

    // The first publish attaches; identity is the caller's PSK, no token.
    let v = call(&server, "hud_publish", json!({"surface": "portal:main", "content": "Working on it", "status": "active", "expects_reply": true})).await;
    assert_eq!(v, json!({"ok": true}));
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
