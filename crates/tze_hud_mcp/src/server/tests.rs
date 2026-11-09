use super::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use tze_hud_portal::hub::{PortalHub, PortalKey, PortalStatus};
use tze_hud_scene::config::hash_psk;
use tze_hud_scene::graph::PendingActionKind;
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
                audience: Default::default(),
                name: "level".into(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: Some(f32_range),
            },
            WidgetParameterDeclaration {
                audience: Default::default(),
                name: "label".into(),
                param_type: WidgetParamType::String,
                default_value: WidgetParameterValue::String(String::new()),
                constraints: None,
            },
            WidgetParameterDeclaration {
                audience: Default::default(),
                name: "color".into(),
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
        auto_clear_ms: None,
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
    let clock = TestClock::new(1_000);
    let server = McpServer::new(scene(&clock)).with_config(McpConfig::with_agents(agents.clone()));
    let new_agent = CallerContext::with_bearer("new-key");
    let resp = rpc(&server, &new_agent, "tools/list", json!({})).await;
    assert_eq!(resp["error"]["code"], -32004);

    let mut paired = AgentDirectory::default();
    paired.insert("new", hash_psk("new-key"), vec!["*".to_string()]);
    agents.store(std::sync::Arc::new(paired));
    let resp = rpc(&server, &new_agent, "tools/list", json!({})).await;
    assert!(resp["result"]["tools"].is_array(), "{resp}");

    let (_, is_error) = call_as(
        &server,
        &new_agent,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "before reload"}),
    )
    .await;
    assert!(!is_error);

    let mut changed = AgentDirectory::default();
    changed.insert(
        "new",
        hash_psk("rotated-key"),
        vec!["publish_zone:subtitle".to_string()],
    );
    changed.insert("control", hash_psk("control-key"), vec!["*".to_string()]);
    agents.store(Arc::new(changed));
    let resp = rpc(&server, &new_agent, "tools/list", json!({})).await;
    assert_eq!(resp["error"]["code"], -32004, "old key after rekey");
    let rotated = CallerContext::with_bearer("rotated-key");
    let (_, is_error) = call_as(
        &server,
        &rotated,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "after reload"}),
    )
    .await;
    assert!(!is_error, "the current key retains the allowed zone");
    let (err, is_error) = call_as(
        &server,
        &rotated,
        "hud_publish",
        json!({"surface": "zone:pip", "content": "denied"}),
    )
    .await;
    assert!(is_error);
    assert_eq!(
        err["code"], "NOT_ALLOWED",
        "reduced allow applies next request"
    );

    let mut removed = AgentDirectory::default();
    removed.insert("control", hash_psk("control-key"), vec!["*".to_string()]);
    agents.store(Arc::new(removed));
    let resp = rpc(&server, &rotated, "tools/list", json!({})).await;
    assert_eq!(resp["error"]["code"], -32004, "removed agent");
    let control = CallerContext::with_bearer("control-key");
    let resp = rpc(&server, &control, "tools/list", json!({})).await;
    assert!(
        resp["result"]["tools"].is_array(),
        "unaffected agent: {resp}"
    );
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

    // Target exclusivity is checked before either admission or scheduling.
    for delay in [0, 1_000] {
        let err = call_err(
            &server,
            "hud_publish",
            json!({
                "surface":"zone:notification-area", "delay_ms":delay,
                "content":{"body":"Open", "actions":[
                    {"label":"Open", "callback_id":"open", "url":"https://example.com/"}
                ]}
            }),
        )
        .await;
        assert_eq!(err["code"], "INVALID_ARGUMENT");
        assert_eq!(err["hint"], "action needs callback_id or url, not both");
    }

    // The same cardinality rule applies before immediate admission and scheduling.
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tze_hud_scene::types::MAX_NOTIFICATION_ACTIONS;
    let snapshot = |scene: &SceneGraph| {
        assert!(
            scene.pending_actions.is_empty(),
            "publish must not enqueue input"
        );
        (
            serde_json::to_value(scene).unwrap(),
            scene
                .scheduled_batches
                .iter()
                .map(|s| (s.present_at_wall_us, s.batch.batch_id))
                .collect::<Vec<_>>(),
            scene.next_timed_content_wall_us(),
        )
    };
    for delay_ms in [0, 1000] {
        let (server, _, a, b) = two_agents_for_zones(&["subtitle", "notification-area"]);
        let wakes = Arc::new(AtomicUsize::new(0));
        let observed = wakes.clone();
        let server = server.with_render_wake_notifier(
            tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
                observed.fetch_add(1, Ordering::Relaxed);
            }),
        );
        let foreign = call_as(
            &server,
            &b,
            "hud_publish",
            json!({"surface":"zone:subtitle", "content":"other holding", "ttl_ms":0}),
        )
        .await;
        assert!(!foreign.1, "{:?}", foreign.0);
        let args = |count| {
            json!({
                "surface":"zone:notification-area", "delay_ms":delay_ms, "ttl_ms":0,
                "content":{"body":"Choose", "actions":(0..count)
                    .map(|i| if i%2==0 {json!({"label":format!("Option {i}"),"callback_id":format!("answer-{i}")})}
                        else {json!({"label":format!("Option {i}"),"url":format!("https://example.com/{i}")})})
                    .collect::<Vec<_>>()}
            })
        };
        // Also reject before this caller has any lease or publication.
        for seeded in [false, true] {
            if seeded {
                for count in [MAX_NOTIFICATION_ACTIONS - 1, MAX_NOTIFICATION_ACTIONS] {
                    let (result, error) = call_as(&server, &a, "hud_publish", args(count)).await;
                    assert!(!error, "{result}");
                }
                let scene = server.scene.lock().await;
                let counts = if delay_ms == 0 {
                    scene.zone_registry.active_publishes["notification-area"]
                        .iter()
                        .map(|p| &p.content)
                        .collect::<Vec<_>>()
                } else {
                    scene
                        .scheduled_batches
                        .iter()
                        .flat_map(|s| &s.batch.mutations)
                        .filter_map(|m| match m {
                            tze_hud_scene::mutation::SceneMutation::PublishToZone {
                                content,
                                ..
                            } => Some(content),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    counts
                        .iter()
                        .map(|c| match c {
                            ZoneContent::Notification(n) => n.actions.len(),
                            _ => panic!("expected notification"),
                        })
                        .collect::<Vec<_>>(),
                    [MAX_NOTIFICATION_ACTIONS - 1, MAX_NOTIFICATION_ACTIONS]
                );
            }
            let before = snapshot(&*server.scene.lock().await);
            let wake_count = wakes.load(Ordering::Relaxed);
            for _ in 0..2 {
                let (err, error) = call_as(
                    &server,
                    &a,
                    "hud_publish",
                    args(MAX_NOTIFICATION_ACTIONS + 1),
                )
                .await;
                assert!(error, "{err}");
                assert_eq!(err["code"], "INVALID_ARGUMENT");
                assert_eq!(
                    err["hint"],
                    format!("notification allows at most {MAX_NOTIFICATION_ACTIONS} actions")
                );
                assert_eq!(snapshot(&*server.scene.lock().await), before);
                assert_eq!(wakes.load(Ordering::Relaxed), wake_count);
            }
        }
        let (result, error) = call_as(&server, &a, "hud_publish", args(0)).await;
        assert!(!error, "valid publish after rejection: {result}");
        for who in [&a, &b] {
            let (input, error) = call_as(&server, who, "hud_input", json!({})).await;
            assert!(!error, "{input}");
            assert_eq!(
                input["items"],
                json!([]),
                "no action/FIFO items from rejection"
            );
        }
    }

    // HTTPS target validation uses the same authority for direct and delayed publish.
    let wakes = Arc::new(AtomicUsize::new(0));
    let observed = wakes.clone();
    let server = server.with_render_wake_notifier(
        tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        }),
    );
    let target_hint = "action needs callback_id or url, not both";
    let url_hint = "action url must be https";
    let prefix = "https://example.com/";
    let boundary = format!("{prefix}{}", "a".repeat(2048 - prefix.len()));
    let unicode_boundary = format!("{prefix}{}", "é".repeat((2048 - prefix.len()) / 2));
    let mut rejected = vec![
        (json!({"label":"Open"}), target_hint),
        (
            json!({"label":"Open","callback_id":"","url":"https://example.com/"}),
            target_hint,
        ),
        (
            json!({"label":"Open","callback_id":"go","url":""}),
            target_hint,
        ),
        (
            json!({"label":"Open","callback_id":"","url":""}),
            target_hint,
        ),
        (json!({"label":"Open","url":""}), url_hint),
        (
            json!({"label":"Open","callback_id":""}),
            "notification.actions[1].callback_id must be a non-empty string",
        ),
        (
            json!({"label":"Open","url":null}),
            "notification.actions[1].url must be a string",
        ),
        (
            json!({"label":"Open","callback_id":42}),
            "notification.actions[1].callback_id must be a string",
        ),
    ];
    for url in [
        "http://example.com/".into(),
        "file:///a".into(),
        "javascript:alert(1)".into(),
        "/relative".into(),
        "https://".into(),
        "https://[bad]/".into(),
        format!("{boundary}a"),
        format!("{unicode_boundary}é"),
    ] {
        rejected.push((json!({"label":"Open","url":url}), url_hint));
    }
    for delay_ms in [0, 1000] {
        for (action, hint) in &rejected {
            let before = snapshot(&*server.scene.lock().await);
            let wake_count = wakes.load(Ordering::Relaxed);
            let err = call_err(&server,"hud_publish",json!({"surface":"zone:notification-area","delay_ms":delay_ms,
                "content":{"body":"Target validation","actions":[{"label":"Legacy","callback_id":"legacy"},action]}})).await;
            assert_eq!(err["code"], "INVALID_ARGUMENT");
            assert_eq!(err["hint"], *hint, "{action}");
            assert_eq!(snapshot(&*server.scene.lock().await), before);
            assert_eq!(wakes.load(Ordering::Relaxed), wake_count);
        }
        for (action, encoded) in [
            (json!({"label":"Legacy","callback_id":"legacy"}), None),
            (
                json!({"label":"Open","url":boundary}),
                Some(boundary.clone()),
            ),
            (
                json!({"label":"Open","url":unicode_boundary}),
                Some(format!(
                    "{prefix}{}",
                    "%C3%A9".repeat((2048 - prefix.len()) / 2)
                )),
            ),
            (
                json!({"label":"Open","url":"https://éxample.com/a\"b"}),
                Some("https://xn--xample-9ua.com/a%22b".into()),
            ),
        ] {
            call(
                &server,
                "hud_publish",
                json!({"surface":"zone:notification-area","delay_ms":delay_ms,
                "content":{"body":"Valid target","actions":[action]}}),
            )
            .await;
            {
                let scene = server.scene.lock().await;
                let content = if delay_ms == 0 {
                    &scene
                        .zone_registry
                        .active_for_zone("notification-area")
                        .last()
                        .unwrap()
                        .content
                } else {
                    match &scene.scheduled_batches.last().unwrap().batch.mutations[0] {
                        tze_hud_scene::mutation::SceneMutation::PublishToZone {
                            content, ..
                        } => content,
                        _ => panic!("publish"),
                    }
                };
                let ZoneContent::Notification(n) = content else {
                    panic!("notification")
                };
                assert_eq!(n.actions[0].url.is_some(), encoded.is_some());
                if let Some(encoded) = encoded {
                    assert_eq!(n.actions[0].encoded_url().unwrap(), encoded);
                }
            }
            call(
                &server,
                "hud_clear",
                json!({"surface":"zone:notification-area"}),
            )
            .await;
        }
    }

    for key in ["x".repeat(257), format!("{}x", "é".repeat(128))] {
        let before = {
            let scene = server.scene.lock().await;
            (
                scene.zone_registry.active_publishes.clone(),
                scene.scheduled_batches.len(),
                scene.leases.len(),
            )
        };
        for delay in [0, 1000] {
            let error = call_err(&server, "hud_publish", json!({"surface":"zone:notification-area", "content":{"body":"bad"}, "key":key, "delay_ms":delay})).await;
            assert_eq!(error["code"], "INVALID_ARGUMENT");
        }
        let error = call_err(
            &server,
            "hud_clear",
            json!({"surface":"zone:notification-area", "key":key}),
        )
        .await;
        assert_eq!(error["code"], "INVALID_ARGUMENT");
        let scene = server.scene.lock().await;
        assert_eq!(
            (
                scene.zone_registry.active_publishes.clone(),
                scene.scheduled_batches.len(),
                scene.leases.len()
            ),
            before
        );
    }
    for surface in ["zone:subtitle", "widget:gauge", "portal:main"] {
        let error = call_err(
            &server,
            "hud_clear",
            json!({"surface":surface,"key":"slot"}),
        )
        .await;
        assert_eq!(error["code"], "INVALID_ARGUMENT");
    }
    for key in [
        "x".repeat(256),
        "é".repeat(128),
        " ".into(),
        "A".into(),
        "a".into(),
        "é".into(),
        "e\u{301}".into(),
        String::new(),
    ] {
        call(
            &server,
            "hud_clear",
            json!({"surface":"zone:notification-area"}),
        )
        .await;
        let value = call(&server, "hud_publish", json!({"surface":"zone:notification-area","content":{"body":"valid"},"key":key,"ttl_ms":0})).await;
        assert_eq!(value, json!({"ok":true}));
        let scene = server.scene.lock().await;
        assert_eq!(
            scene.zone_registry.active_for_zone("notification-area")[0]
                .merge_key
                .as_deref(),
            (!key.is_empty()).then_some(key.as_str())
        );
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
        json!({"s": "zone:subtitle", "accepts": "text", "one": true}),
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
    let defaults = server.config.publication_defaults.clone();
    defaults.set_answer_window_ms(30_000);
    for (actions, ttl, expected) in [
        (false, None, Some(60_000)),
        (true, None, Some(30_000)),
        (true, Some(5_000), Some(5_000)),
        (true, Some(600_000), Some(600_000)),
        (true, Some(0), None),
    ] {
        call(
            &server,
            "hud_clear",
            json!({"surface":"zone:notification-area"}),
        )
        .await;
        let mut args =
            json!({"surface": "zone:notification-area", "content": {"body": "Continue?"}});
        if actions {
            args["content"]["actions"] = json!([{"label":"Yes","callback_id":"yes"}]);
        }
        if let Some(ttl) = ttl {
            args["ttl_ms"] = json!(ttl);
        }
        let result = call(&server, "hud_publish", args).await;
        assert_eq!(
            result.get("expires_in_ms").and_then(Value::as_u64),
            expected
        );
        let expiry = server.scene.lock().await.zone_registry.active_publishes["notification-area"]
            [0]
        .expires_at_wall_us;
        assert_eq!(expiry, expected.map(|ms| 1_000_000 + ms * 1_000));
        let surfaces = call(&server, "hud_surfaces", json!({})).await;
        let held = surfaces["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["s"] == "zone:notification-area")
            .unwrap();
        assert_eq!(held.get("expires_in_ms").and_then(Value::as_u64), expected);
    }
    // Concurrent updates may choose old or new; stored expiry and reply must agree.
    let writer = std::thread::spawn(move || {
        for _ in 0..4096 {
            defaults.set_answer_window_ms(15_000);
            defaults.set_answer_window_ms(120_000);
        }
    });
    for _ in 0..32 {
        call(
            &server,
            "hud_clear",
            json!({"surface":"zone:notification-area"}),
        )
        .await;
        let result = call(&server, "hud_publish", json!({"surface":"zone:notification-area", "content":{"body":"Choose", "actions":[{"label":"Yes","callback_id":"yes"}]}})).await;
        let ms = result["expires_in_ms"].as_u64().unwrap();
        assert!([15_000, 30_000, 120_000].contains(&ms));
        assert_eq!(
            server.scene.lock().await.zone_registry.active_publishes["notification-area"][0]
                .expires_at_wall_us,
            Some(1_000_000 + ms * 1_000)
        );
    }
    writer.join().unwrap();
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
    drop(scene);
    server
        .config
        .publication_defaults
        .set_answer_window_ms(30_000);
    let result = call(&server, "hud_publish", json!({"surface":"zone:notification-area", "content":{"body":"Choose", "actions":[{"label":"Yes","callback_id":"yes"}]}})).await;
    assert_eq!(result["expires_in_ms"], 30_000);
    let mut scene = server.scene.lock().await;
    let expected = scene.now_wall_us() + 30_000_000;
    assert_eq!(
        scene.zone_registry.active_publishes["notification-area"][0].expires_at_wall_us,
        Some(expected)
    );
    server
        .config
        .publication_defaults
        .set_answer_window_ms(300_000);
    clock.advance(30_000);
    assert_eq!(
        scene.drain_expired_zone_publications(),
        1,
        "active expiry remains fixed"
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
    drop(scene);

    // Opt-in release comparison of the real JSON-RPC publish path. Unrelated
    // retained transcript size must not turn a tiny subtitle into scene-wide work.
    if std::env::var_os("TZE_HUD_ZONE_SCALING").is_some() {
        use std::time::Instant;
        use tze_hud_scene::types::{
            FontFamily, Node, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
        };
        assert!(!cfg!(debug_assertions), "scaling proof requires --release");
        let mut servers = Vec::new();
        for tile_count in [1, 64] {
            let (candidate, _) = super::tests::server();
            {
                let mut scene = candidate.scene.lock().await;
                let tab = scene.active_tab.unwrap();
                let lease = scene.grant_lease("unrelated-transcript", 60_000);
                for index in 0..tile_count {
                    let tile = scene
                        .create_tile(
                            tab,
                            "unrelated-transcript",
                            lease,
                            Rect::new(0.0, 0.0, 100.0, 100.0),
                            index,
                        )
                        .unwrap();
                    scene
                        .set_tile_root(
                            tile,
                            Node {
                                id: SceneId::new(),
                                children: Vec::new(),
                                layout: Default::default(),
                                data: NodeData::TextMarkdown(TextMarkdownNode {
                                    content: "x".repeat(65_535),
                                    bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                                    font_size_px: 16.0,
                                    font_family: FontFamily::SystemSansSerif,
                                    color: Rgba::WHITE,
                                    background: None,
                                    alignment: TextAlign::Start,
                                    overflow: TextOverflow::Clip,
                                    color_runs: Box::default(),
                                }),
                            },
                        )
                        .unwrap();
                }
            }
            servers.push(candidate);
        }
        let payload = json!({"surface":"zone:subtitle", "content":"tiny update", "ttl_ms":20000});
        for candidate in &servers {
            for _ in 0..10 {
                call(candidate, "hud_publish", payload.clone()).await;
            }
        }
        let mut samples = [Vec::new(), Vec::new()];
        // Alternate order each round to limit drift; setup and warmup are untimed.
        for round in 0..7 {
            for index in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
                let start = Instant::now();
                for _ in 0..100 {
                    let reply = call(&servers[index], "hud_publish", payload.clone()).await;
                    assert_eq!(reply, json!({"ok":true,"expires_in_ms":20000}));
                }
                samples[index].push(start.elapsed().as_nanos() / 100);
            }
        }
        println!(
            "ZONE_SCALING payload={payload} tiles=[1,64] text_bytes_per_tile=65535 samples_ns_per_publish={samples:?}"
        );
        for sample in &mut samples {
            sample.sort_unstable();
        }
        let ratio = samples[1][3] as f64 / samples[0][3] as f64;
        println!(
            "ZONE_SCALING median_ns=[{},{}] growth_ratio={ratio:.3}",
            samples[0][3], samples[1][3]
        );
        assert!(
            ratio < 4.0,
            "unrelated scene growth amplified publish cost by {ratio:.3}x"
        );
    }
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
/// otherwise the explicit or omission-default expiry stamped at acceptance,
/// counted from presentation. A later owner change never retimes it.
#[tokio::test]
async fn delayed_notification_expiry_matches_immediate() {
    for actions in [false, true] {
        for ttl in [None, Some(0), Some(10_000), Some(600_000)] {
            let (server, clock) = server();
            server
                .config
                .publication_defaults
                .set_answer_window_ms(30_000);
            let lifetime = ttl.unwrap_or(if actions { 30_000 } else { 60_000 });
            let expected = (lifetime > 0).then_some(6_000_000 + lifetime * 1_000);
            let mut args = json!({"surface": "zone:notification-area",
                "content": {"title": "t", "body": "b"}, "delay_ms": 5000});
            if actions {
                args["content"]["actions"] = json!([{"label":"Yes","callback_id":"yes"}]);
            }
            if let Some(ttl) = ttl {
                args["ttl_ms"] = json!(ttl);
            }
            let response = call(&server, "hud_publish", args).await;
            assert_eq!(
                response.get("expires_in_ms").and_then(Value::as_u64),
                expected.map(|at| (at - 1_000_000) / 1_000)
            );
            server
                .config
                .publication_defaults
                .set_answer_window_ms(120_000);
            let surfaces = call(&server, "hud_surfaces", json!({})).await;
            let held = surfaces["surfaces"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["s"] == "zone:notification-area")
                .unwrap();
            assert_eq!(
                held.get("expires_in_ms").and_then(Value::as_u64),
                response.get("expires_in_ms").and_then(Value::as_u64)
            );
            clock.advance(5_000);
            {
                let mut scene = server.scene.lock().await;
                scene.apply_due_batches();
                assert_eq!(
                    scene.zone_registry.active_publishes["notification-area"][0].expires_at_wall_us,
                    expected
                );
                clock.advance(600_001);
                assert_eq!(
                    scene.drain_expired_zone_publications(),
                    usize::from(expected.is_some())
                );
            }
            if ttl == Some(0) {
                let response = call(
                    &server,
                    "hud_hold",
                    json!({"surface":"zone:notification-area", "ttl_ms":5000}),
                )
                .await;
                assert_eq!(response, json!({"ok":true,"expires_in_ms":5000}));
                let scene = server.scene.lock().await;
                assert_eq!(
                    scene.zone_registry.active_publishes["notification-area"][0].expires_at_wall_us,
                    Some(scene.now_wall_us() + 5_000_000)
                );
            }
        }
    }
}

/// Two agents that may both use `zone:subtitle`, with their bearers.
fn two_agents() -> (McpServer, TestClock, CallerContext, CallerContext) {
    two_agents_for_zones(&["subtitle"])
}

fn two_agents_for_zones(zones: &[&str]) -> (McpServer, TestClock, CallerContext, CallerContext) {
    let clock = TestClock::new(1_000);
    let mut agents = AgentDirectory::default();
    for (id, key) in [("a", "a-key"), ("b", "b-key")] {
        agents.insert(
            id,
            hash_psk(key),
            zones
                .iter()
                .map(|zone| format!("publish_zone:{zone}"))
                .collect(),
        );
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
    let (server, clock, a, b) = two_agents_for_zones(&["subtitle", "notification-area"]);
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

    drop(scene);
    for (who, key, delay) in [
        (&a, "first", 0),
        (&a, "first", 5000),
        (&a, "second", 5000),
        (&b, "first", 5000),
    ] {
        let (value, error) = call_as(&server, who, "hud_publish", json!({"surface":"zone:notification-area","content":{"body":key},"key":key,"ttl_ms":0,"delay_ms":delay})).await;
        assert!(!error, "{value}");
    }
    {
        let mut scene = server.scene.lock().await;
        let pending = scene
            .scheduled_batches
            .iter_mut()
            .find(|s| s.batch.agent_namespace == "a")
            .unwrap();
        let mut remainder = pending.batch.mutations[0].clone();
        if let tze_hud_scene::mutation::SceneMutation::PublishToZone {
            zone_name,
            content,
            merge_key,
            ..
        } = &mut remainder
        {
            *zone_name = "subtitle".into();
            *content = tze_hud_scene::types::ZoneContent::StreamText("mixed remainder".into());
            *merge_key = None;
        }
        pending.batch.mutations.push(remainder);
    }
    let (value, error) = call_as(
        &server,
        &a,
        "hud_clear",
        json!({"surface":"zone:notification-area","key":"first"}),
    )
    .await;
    assert!(!error, "{value}");
    assert!(
        server
            .scene
            .lock()
            .await
            .zone_registry
            .active_for_zone("notification-area")
            .is_empty()
    );
    clock.advance(5000);
    let mut scene = server.scene.lock().await;
    assert!(scene.apply_due_batches().iter().all(|r| r.applied));
    let notifications = scene.zone_registry.active_for_zone("notification-area");
    assert_eq!(notifications.len(), 2);
    assert!(
        notifications
            .iter()
            .any(|r| r.publisher_namespace == "a" && r.merge_key.as_deref() == Some("second"))
    );
    assert!(
        notifications
            .iter()
            .any(|r| r.publisher_namespace == "b" && r.merge_key.as_deref() == Some("first"))
    );
    assert!(scene.zone_registry.active_for_zone("subtitle").iter().any(|r| matches!(&r.content, tze_hud_scene::types::ZoneContent::StreamText(s) if s == "mixed remainder")));
    assert_eq!(scene.next_timed_content_wall_us(), None);
}

/// With nothing else pending, cancelling leaves no wake deadline at all.
#[tokio::test]
async fn clear_of_last_pending_publish_drops_the_wake_deadline() {
    let (server, clock, a, _) = two_agents_for_zones(&["subtitle", "notification-area"]);
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

    drop(scene);
    let (value, error) = call_as(&server, &a, "hud_publish", json!({"surface":"zone:notification-area","content":{"body":"later"},"key":"only","delay_ms":5000,"ttl_ms":0})).await;
    assert!(!error, "{value}");
    let (value, error) = call_as(
        &server,
        &a,
        "hud_clear",
        json!({"surface":"zone:notification-area","key":"only"}),
    )
    .await;
    assert!(!error, "{value}");
    clock.advance(5000);
    let mut scene = server.scene.lock().await;
    assert!(scene.scheduled_batches.is_empty());
    assert_eq!(scene.next_timed_content_wall_us(), None);
    assert!(scene.apply_due_batches().is_empty());
    assert!(
        scene
            .zone_registry
            .active_for_zone("notification-area")
            .is_empty()
    );
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
    server
        .config
        .publication_defaults
        .set_answer_window_ms(15_000);
    let notif = json!({"surface": "zone:notification-area",
        "content": {"title": "t", "body": "b", "actions":[{"label":"Yes","callback_id":"yes"}]}});
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

    drop(scene);
    for ttl in [1000, 2500] {
        let mut keyed = notif.clone();
        keyed["key"] = json!("slot");
        keyed["ttl_ms"] = json!(ttl);
        assert_eq!(
            call(&server, "hud_publish", keyed).await,
            json!({"ok":true,"expires_in_ms":ttl})
        );
        let scene = server.scene.lock().await;
        let records = scene.zone_registry.active_for_zone("notification-area");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].expires_at_wall_us,
            Some(scene.now_wall_us() + ttl * 1000)
        );
    }
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
    let (server, clock) = server();
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
        &json!({"s": "zone:subtitle", "accepts": "text", "one": true, "held": true, "expires_in_ms": 8000})
    );
    let gauge = entries.iter().find(|e| e["s"] == "widget:gauge").unwrap();
    assert_eq!(
        gauge["params"],
        json!({"level": "number 0..1", "label": "string", "color": "color"})
    );
    let text = v.to_string();
    assert!(
        !text.contains("-4") && !text.contains("_us"),
        "no ids or timestamps: {text}"
    );
    let notice = entries
        .iter()
        .find(|e| e["s"] == "zone:notification-area")
        .unwrap();
    assert_eq!(notice["max"], 5);
    assert!(notice.get("one").is_none() && notice.get("busy").is_none());
    assert!(
        entries
            .iter()
            .find(|e| e["s"] == "zone:status-bar")
            .unwrap()
            .get("one")
            .is_none()
    );
    assert!(entries.iter().all(|e| {
        !e["accepts"]
            .as_str()
            .unwrap_or_default()
            .contains("static_image")
    }));
    assert!(sub.get("busy").is_none(), "own occupant is not foreign");
    server
        .scene
        .lock()
        .await
        .zone_registry
        .active_publishes
        .get_mut("subtitle")
        .unwrap()[0]
        .publisher_namespace = "foreign".into();
    let foreign = call(&server, "hud_surfaces", json!({})).await;
    let sub = foreign["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["s"] == "zone:subtitle")
        .unwrap();
    assert_eq!(sub["busy"], true);
    assert!(sub.get("held").is_none());
    clock.advance(8000);
    let expired = call(&server, "hud_surfaces", json!({})).await;
    let sub = expired["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["s"] == "zone:subtitle")
        .unwrap();
    assert!(sub.get("busy").is_none());
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

/// The owner's "Clears after" limit is what agents are told: a durable
/// publish, a hold until cleared and `hud_surfaces` all report it, since a
/// hold cannot outlast it.
#[tokio::test]
async fn widget_auto_clear_limit_is_reported_to_agents() {
    let (server, clock) = server();
    assert!(
        server
            .scene
            .lock()
            .await
            .set_widget_auto_clear("gauge", Some(30_000))
    );
    let v = call(
        &server,
        "hud_publish",
        json!({"surface": "widget:gauge", "params": {"level": 0.5}}),
    )
    .await;
    assert_eq!(v, json!({"ok": true, "expires_in_ms": 30_000}));
    clock.advance(10_000);
    let v = call(
        &server,
        "hud_hold",
        json!({"surface": "widget:gauge", "ttl_ms": 0}),
    )
    .await;
    assert_eq!(
        v,
        json!({"ok": true, "expires_in_ms": 20_000}),
        "a hold cannot outlast it"
    );
    let v = call(
        &server,
        "hud_hold",
        json!({"surface": "widget:gauge", "ttl_ms": 5_000}),
    )
    .await;
    assert_eq!(
        v,
        json!({"ok": true, "expires_in_ms": 5_000}),
        "a shorter one stands"
    );
    let surfaces = call(&server, "hud_surfaces", json!({})).await;
    let gauge = surfaces["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["s"] == "widget:gauge")
        .unwrap()
        .clone();
    assert_eq!(gauge["expires_in_ms"], 5_000);
}

/// A keyed publish reports its own expiry, not a sibling key's.
#[tokio::test]
async fn keyed_widget_publish_reports_its_own_expiry() {
    let (server, _clock) = server();
    {
        let mut scene = server.scene.lock().await;
        let gauge = scene
            .widget_registry
            .instances
            .values_mut()
            .find(|i| i.instance_name == "gauge")
            .unwrap();
        gauge.contention_override = Some(ContentionPolicy::MergeByKey { max_keys: 4 });
    }
    let publish = |key: &'static str, ttl: Option<u64>| {
        let mut args = json!({"surface": "widget:gauge", "params": {"level": 0.5}, "key": key});
        if let Some(ttl) = ttl {
            args["ttl_ms"] = json!(ttl);
        }
        call(&server, "hud_publish", args)
    };
    assert_eq!(
        publish("a", Some(60_000)).await,
        json!({"ok": true, "expires_in_ms": 60_000})
    );
    assert_eq!(
        publish("b", Some(5_000)).await,
        json!({"ok": true, "expires_in_ms": 5_000})
    );
    assert_eq!(publish("c", None).await, json!({"ok": true}), "durable");
}

#[tokio::test]
async fn widget_publish_hold_clear() {
    let (server, clock) = server();
    let mut config = McpConfig::with_psk(psk());
    config.widget_transition_ms = 200;
    let server = server.with_config(config);
    {
        use tze_hud_scene::WidgetParamAudience::{Owner, Runtime};
        let mut scene = server.scene.lock().await;
        let schema = &mut scene
            .widget_registry
            .definitions
            .get_mut("gauge")
            .unwrap()
            .parameter_schema;
        for (name, audience) in [
            ("fill_color", Owner),
            ("theme", Owner),
            ("tooltip_visible", Runtime),
        ] {
            schema.push(WidgetParameterDeclaration {
                name: name.into(),
                audience,
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            });
        }
        schema.push(WidgetParameterDeclaration {
            name: "severity".into(),
            audience: Default::default(),
            param_type: WidgetParamType::Enum,
            default_value: WidgetParameterValue::Enum("info".into()),
            constraints: Some(WidgetParamConstraints {
                enum_allowed_values: vec!["info".into(), "warning".into(), "error".into()],
                ..Default::default()
            }),
        });
    }
    let initial = {
        let scene = server.scene.lock().await;
        (
            scene.version,
            scene.widget_registry.instances["gauge"]
                .current_params
                .clone(),
        )
    };
    for (name, hint) in [
        ("fill_color", "severity"),
        ("theme", "owner-set"),
        ("tooltip_visible", "runtime-set"),
    ] {
        for value in [json!(0.5), json!("wrong-type")] {
            let error = call_err(
                &server,
                "hud_publish",
                json!({"surface":"widget:gauge", "params": {"level":0.8, name: value}}),
            )
            .await;
            assert_eq!(error["code"], "WIDGET_PARAMETER_INVALID");
            assert!(error["hint"].as_str().unwrap().contains(hint), "{error}");
            let scene = server.scene.lock().await;
            assert_eq!(scene.version, initial.0);
            assert_eq!(
                scene.widget_registry.instances["gauge"].current_params,
                initial.1
            );
            assert!(scene.leases.is_empty());
            assert!(scene.widget_registry.active_for_widget("gauge").is_empty());
        }
    }
    let discovery = call(&server, "hud_surfaces", json!({})).await;
    let gauge = discovery["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["s"] == "widget:gauge")
        .unwrap();
    for name in ["fill_color", "theme", "tooltip_visible"] {
        assert!(gauge["params"].get(name).is_none());
    }
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
            "color": {"r": 1.0, "g": 0.0, "b": 0.0, "a": 1.0}}}),
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
            record.params["color"],
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
        (json!({"severity": "critical"}), "info|warning|error"),
        (json!({"severity": 42}), "info|warning|error"),
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
        bad.push((json!({"color": value}), "0..1"));
    }
    for channel in ["r", "g", "b", "a"] {
        for value in [json!(true), json!("0.5"), json!(null)] {
            let mut color = json!({"r": 0.2, "g": 0.3, "b": 0.4, "a": 1});
            color[channel] = value;
            bad.push((json!({"color": color}), "numeric"));
        }
        if channel != "a" {
            let mut color = json!({"r": 0.2, "g": 0.3, "b": 0.4});
            color.as_object_mut().unwrap().remove(channel);
            bad.push((json!({"color": color}), "numeric"));
        }
        for value in [-0.1, 1.0000000001, 255.0, 1e100] {
            let mut color = json!({"r": 0.2, "g": 0.3, "b": 0.4, "a": 1});
            color[channel] = json!(value);
            bad.push((json!({"color": color}), "0..1"));
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
            json!({"surface":"widget:gauge", "params":{"level":level, "color":color}}),
        )
        .await;
        assert_eq!(
            server.scene.lock().await.widget_registry.instances["gauge"].current_params["color"],
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
            publication_id: None,
            key: None,
            publisher_namespace: AgentDirectory::unrestricted(psk())
                .resolve(&psk(), "")
                .unwrap()
                .agent_id,
            zone_name: "notification-area".into(),
            kind: PendingActionKind::Callback("approve".into()),
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

    // Each agent exceeds the former global cap before either polls. Collection
    // and acknowledgement by one publisher must not evict the other's actions.
    let (server, _, a, b) = two_agents_for_zones(&["subtitle", "notification-area"]);
    let actions_per_agent = 257;
    {
        let mut scene = server.scene.lock().await;
        for index in 0..actions_per_agent {
            for namespace in ["a", "b"] {
                scene.push_pending_action(PendingAction {
                    publication_id: None,
                    key: None,
                    publisher_namespace: namespace.into(),
                    zone_name: "subtitle".into(),
                    kind: PendingActionKind::Callback(format!("{namespace}-{index}")),
                });
            }
        }
    }
    let (zero, is_error) = call_as(&server, &a, "hud_input", json!({"max_items": 0})).await;
    assert!(!is_error, "{zero}");
    assert_eq!(zero, json!({"items": [], "remaining": actions_per_agent}));
    let (a_page, is_error) = call_as(&server, &a, "hud_input", json!({"max_items": 7})).await;
    assert!(!is_error, "{a_page}");
    assert_eq!(a_page["items"].as_array().unwrap().len(), 7);
    assert_eq!(a_page["remaining"], actions_per_agent - 7);
    let (b_page, is_error) = call_as(&server, &b, "hud_input", json!({"max_items": 11})).await;
    assert!(!is_error, "{b_page}");
    assert_eq!(b_page["items"].as_array().unwrap().len(), 11);
    assert_eq!(b_page["remaining"], actions_per_agent - 11);

    let (a_again, is_error) = call_as(
        &server,
        &a,
        "hud_input",
        json!({"max_items": 7, "ack": [b_page["items"][0]["id"], b_page["items"][0]["id"], "unknown-action"]}),
    )
    .await;
    assert!(!is_error, "{a_again}");
    assert_eq!(a_again, a_page, "foreign and unknown ACKs leave A intact");
    let (b_again, is_error) = call_as(
        &server,
        &b,
        "hud_input",
        json!({"max_items": 11, "ack": [a_page["items"][0]["id"], "unknown-action"]}),
    )
    .await;
    assert!(!is_error, "{b_again}");
    assert_eq!(b_again, b_page, "A's poll and ACKs leave B intact");

    let (a_all, is_error) = call_as(&server, &a, "hud_input", json!({})).await;
    assert!(!is_error, "{a_all}");
    let (b_all, is_error) = call_as(&server, &b, "hud_input", json!({})).await;
    assert!(!is_error, "{b_all}");
    for (namespace, all) in [("a", &a_all), ("b", &b_all)] {
        let items = all["items"].as_array().unwrap();
        assert_eq!(items.len(), actions_per_agent);
        assert_eq!(all["remaining"], 0);
        for (index, item) in items.iter().enumerate() {
            assert_eq!(item["s"], "zone:subtitle");
            assert_eq!(item["action"], format!("{namespace}-{index}"));
        }
    }
    let a_items = a_all["items"].as_array().unwrap();
    let b_items = b_all["items"].as_array().unwrap();
    assert_eq!(a_page["items"], json!(&a_items[..7]));
    assert_eq!(b_page["items"], json!(&b_items[..11]));
    let a_ids: std::collections::HashSet<_> = a_items
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    let b_ids: std::collections::HashSet<_> = b_items
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(a_ids.len(), actions_per_agent);
    assert_eq!(b_ids.len(), actions_per_agent);
    assert!(a_ids.is_disjoint(&b_ids));

    let mut a_ack: Vec<Value> = a_items[..7].iter().map(|item| item["id"].clone()).collect();
    a_ack.push(a_ack[0].clone());
    a_ack.push(b_items[0]["id"].clone());
    a_ack.push(json!("unknown-action"));
    let (a_limited, is_error) = call_as(
        &server,
        &a,
        "hud_input",
        json!({"ack": a_ack, "max_items": 13}),
    )
    .await;
    assert!(!is_error, "{a_limited}");
    assert_eq!(a_limited["items"], json!(&a_items[7..20]));
    assert_eq!(a_limited["remaining"], actions_per_agent - 20);
    let (b_after_a_ack, is_error) = call_as(&server, &b, "hud_input", json!({})).await;
    assert!(!is_error, "{b_after_a_ack}");
    assert_eq!(b_after_a_ack, b_all);

    let b_ack: Vec<Value> = b_items.iter().map(|item| item["id"].clone()).collect();
    let (b_empty, is_error) = call_as(&server, &b, "hud_input", json!({"ack": b_ack})).await;
    assert!(!is_error, "{b_empty}");
    assert_eq!(b_empty, json!({"items": [], "remaining": 0}));
    let (a_retained, is_error) = call_as(&server, &a, "hud_input", json!({})).await;
    assert!(!is_error, "{a_retained}");
    assert_eq!(a_retained, json!({"items": &a_items[7..], "remaining": 0}));
    let a_ack: Vec<Value> = a_items[7..].iter().map(|item| item["id"].clone()).collect();
    let (a_empty, is_error) = call_as(&server, &a, "hud_input", json!({"ack": a_ack})).await;
    assert!(!is_error, "{a_empty}");
    assert_eq!(a_empty, json!({"items": [], "remaining": 0}));
    // A callback literally named dismiss is distinct from the runtime's X.
    {
        let mut scene = server.scene.lock().await;
        scene.push_pending_action(PendingAction {
            publication_id: None,
            key: None,
            publisher_namespace: "a".into(),
            zone_name: "subtitle".into(),
            kind: PendingActionKind::Callback("dismiss".into()),
        });
        scene.push_pending_action(PendingAction {
            publication_id: None,
            key: None,
            publisher_namespace: "a".into(),
            zone_name: "subtitle".into(),
            kind: PendingActionKind::Dismiss,
        });
    }
    let (callback, error) = call_as(&server, &a, "hud_input", json!({"max_items":1})).await;
    assert!(!error);
    assert_eq!(callback["items"].as_array().unwrap().len(), 1);
    assert_eq!(callback["items"][0]["action"], "dismiss");
    assert!(
        callback["items"][0].get("kind").is_none(),
        "callback JSON stays unchanged"
    );
    assert_eq!(callback["remaining"], 1);
    let callback_id = callback["items"][0]["id"].clone();
    let (dismiss, error) = call_as(
        &server,
        &a,
        "hud_input",
        json!({"ack":[callback_id],"max_items":1}),
    )
    .await;
    assert!(!error);
    let dismiss_id = dismiss["items"][0]["id"].clone();
    assert_ne!(dismiss_id, callback_id);
    assert_eq!(
        dismiss,
        json!({"items":[{"id":dismiss_id,"s":"zone:subtitle","action":"dismiss","kind":"dismiss"}],"remaining":0})
    );
    let (again, error) = call_as(&server, &a, "hud_input", json!({})).await;
    assert!(!error);
    assert_eq!(
        again, dismiss,
        "typed dismiss retains its stable id until ack"
    );
    let (foreign, error) = call_as(&server, &b, "hud_input", json!({"ack":[dismiss_id]})).await;
    assert!(!error);
    assert_eq!(foreign, json!({"items":[],"remaining":0}));
    let (zero, error) = call_as(&server, &a, "hud_input", json!({"max_items":0})).await;
    assert!(!error);
    assert_eq!(zero, json!({"items":[],"remaining":1}));
    let (acked, error) = call_as(
        &server,
        &a,
        "hud_input",
        json!({"ack":[dismiss_id,dismiss_id,"unknown"]}),
    )
    .await;
    assert!(!error);
    assert_eq!(acked, json!({"items":[],"remaining":0}));

    // Accepted A is a value, not a lookup of the card currently occupying its key.
    for retained in [false, true] {
        for kind in [
            PendingActionKind::Callback("dismiss".into()),
            PendingActionKind::Dismiss,
        ] {
            server
                .scene
                .lock()
                .await
                .push_pending_action(PendingAction {
                    publication_id: None,
                    publisher_namespace: "a".into(),
                    zone_name: "notification-area".into(),
                    kind: kind.clone(),
                    key: Some("slot".into()),
                });
            let encoded = if retained {
                Some(call_as(&server, &a, "hud_input", json!({})).await.0)
            } else {
                None
            };
            let (value, error) = call_as(&server, &a, "hud_publish", json!({"surface":"zone:notification-area","content":{"body":"B"},"key":"slot","ttl_ms":0})).await;
            assert!(!error, "{value}");
            let (value, error) = call_as(
                &server,
                &a,
                "hud_clear",
                json!({"surface":"zone:notification-area","key":"slot"}),
            )
            .await;
            assert!(!error, "{value}");
            let (accepted, error) = call_as(&server, &a, "hud_input", json!({})).await;
            assert!(!error, "{accepted}");
            assert_eq!(accepted["items"].as_array().unwrap().len(), 1);
            assert_eq!(accepted["items"][0]["key"], "slot");
            assert_eq!(accepted["items"][0]["action"], "dismiss");
            assert_eq!(
                accepted["items"][0].get("kind").is_some(),
                kind == PendingActionKind::Dismiss
            );
            if let Some(encoded) = encoded {
                assert_eq!(accepted, encoded);
            }
            let (again, _) = call_as(&server, &a, "hud_input", json!({})).await;
            assert_eq!(again, accepted);
            let (foreign, _) = call_as(
                &server,
                &b,
                "hud_input",
                json!({"ack":[accepted["items"][0]["id"]]}),
            )
            .await;
            assert_eq!(foreign["items"], json!([]));
            let (done, _) = call_as(
                &server,
                &a,
                "hud_input",
                json!({"ack":[accepted["items"][0]["id"]]}),
            )
            .await;
            assert_eq!(done, json!({"items":[],"remaining":0}));
        }
    }
}

/// The shared pointer/keyboard producer, with the compositor's exact live card
/// identity. Tests inject hit geometry rather than inventing input queue items.
async fn activate_card(server: &McpServer, id: SceneId, dismiss: bool) {
    use tze_hud_scene::types::{Rect, ZoneHitRegion, ZoneInteractionKind};
    let (activation, wakes) = {
        let mut scene = server.scene.lock().await;
        let record = scene
            .zone_registry
            .active_for_zone("notification-area")
            .iter()
            .find(|record| record.publication_id == id)
            .unwrap()
            .clone();
        let region = ZoneHitRegion {
            publication_id: id,
            zone_name: record.zone_name,
            published_at_wall_us: record.published_at_wall_us,
            publisher_namespace: record.publisher_namespace,
            bounds: Rect::new(0.0, 0.0, 100.0, 30.0),
            kind: if dismiss {
                ZoneInteractionKind::Dismiss
            } else {
                ZoneInteractionKind::Action { index: 0 }
            },
            interaction_id: format!("test:{id}:{dismiss}"),
            tab_order: 0,
        };
        scene.overlay.zone_hit_regions.push(region.clone());
        let activation = scene
            .activate_notification(&region, 0)
            .expect("accepted local control");
        (activation, scene.take_notification_wait_notifiers())
    };
    for wake in wakes {
        wake.notify();
    }
    match activation {
        tze_hud_scene::graph::NotificationActivation::Callback(wake) => wake.notify(),
        _ => panic!("callback fixture"),
    }
}

fn question(wait_ms: u64) -> Value {
    json!({"surface":"zone:notification-area","key":"question","ttl_ms":60000,"wait_ms":wait_ms,
        "content":{"title":"Ship?","actions":[{"label":"Ship","callback_id":"ship"}]}})
}

async fn latest_card(server: &McpServer) -> SceneId {
    server
        .scene
        .lock()
        .await
        .zone_registry
        .active_for_zone("notification-area")
        .last()
        .unwrap()
        .publication_id
}

#[tokio::test(start_paused = true)]
async fn publish_wait_reserves_only_its_exact_answer() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let mut context = Context::from_waker(Waker::noop());
    for dismiss in [false, true] {
        for cancel in [false, true] {
            let (server, _) = self::server();
            let (server, _) = hub_portal(server);
            let scene = server.scene_handle();
            let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed_wakes = Arc::clone(&wakes);
            let server = server.with_render_wake_notifier(RenderWakeNotifier::new(move || {
                assert!(scene.try_lock().is_ok(), "admission wake is post-unlock");
                observed_wakes.fetch_add(1, Ordering::SeqCst);
            }));
            call(&server, "hud_publish", json!({"surface":"portal:other"})).await;
            let mut ask = Box::pin(call(&server, "hud_publish", question(30000)));
            assert!(ask.as_mut().poll(&mut context).is_pending());
            assert_eq!(
                wakes.load(Ordering::SeqCst),
                1,
                "question wakes before waiting"
            );
            let id = latest_card(&server).await;
            let mut input = Box::pin(call(&server, "hud_input", json!({"wait_ms":30000})));
            assert!(input.as_mut().poll(&mut context).is_pending());
            activate_card(&server, id, dismiss).await;
            assert!(
                input.as_mut().poll(&mut context).is_pending(),
                "armed answer stays private"
            );

            // A portal reply and a different card both remain ordinary input.
            server
                .portals
                .as_ref()
                .unwrap()
                .submit_reply(&PortalKey::new(psk_agent(), "other"), "elsewhere".into(), 0)
                .unwrap();
            let Poll::Ready(portal) = input.as_mut().poll(&mut context) else {
                panic!("unrelated reply wakes input");
            };
            assert_eq!(portal["items"][0]["text"], "elsewhere");
            assert_eq!(portal["items"].as_array().unwrap().len(), 1);
            call(
                &server,
                "hud_input",
                json!({"ack":[portal["items"][0]["id"]]}),
            )
            .await;
            call(&server, "hud_publish", json!({"surface":"zone:notification-area","content":{"title":"Other","actions":[{"label":"Other","callback_id":"other"}]},"ttl_ms":0})).await;
            let other = latest_card(&server).await;
            assert_ne!(other, id);
            activate_card(&server, other, false).await;
            let unrelated = call(&server, "hud_input", json!({})).await;
            assert_eq!(unrelated["items"].as_array().unwrap().len(), 1);
            assert_eq!(unrelated["items"][0]["action"], "other");
            call(
                &server,
                "hud_input",
                json!({"ack":[unrelated["items"][0]["id"]]}),
            )
            .await;

            if cancel {
                drop(ask);
                let accepted = call(&server, "hud_input", json!({})).await;
                assert_eq!(
                    accepted["items"][0]["action"],
                    if dismiss { "dismiss" } else { "ship" }
                );
                assert_eq!(accepted["items"][0].get("kind").is_some(), dismiss);
                assert_eq!(
                    call(&server, "hud_input", json!({})).await,
                    accepted,
                    "cancellation preserves stable FIFO id"
                );
            } else {
                let Poll::Ready(answer) = ask.as_mut().poll(&mut context) else {
                    panic!("answer completes publish");
                };
                assert_eq!(answer["action"], if dismiss { "dismiss" } else { "ship" });
                assert_eq!(answer.get("kind").is_some(), dismiss);
                assert_eq!(
                    call(&server, "hud_input", json!({})).await,
                    json!({"items":[],"remaining":0}),
                    "encoded answer consumed without ACK"
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn publish_wait_endings_deadline_delay_and_validation() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let mut context = Context::from_waker(Waker::noop());
    for (ending, delayed) in [
        ("expired", false),
        ("replaced", false),
        ("displaced", false),
        ("cleared", false),
        ("cleared", true),
    ] {
        let (server, clock) = self::server();
        let mut args = question(30000);
        if delayed {
            args["delay_ms"] = json!(20000);
        }
        let mut ask = Box::pin(call(&server, "hud_publish", args));
        assert!(ask.as_mut().poll(&mut context).is_pending());
        match ending {
            "expired" => {
                clock.advance(60000);
                let wakes = {
                    let mut scene = server.scene.lock().await;
                    scene.drain_expired_zone_publications();
                    scene.take_notification_wait_notifiers()
                };
                for wake in wakes {
                    wake.notify();
                }
            }
            "replaced" => {
                call(&server, "hud_publish", json!({"surface":"zone:notification-area","key":"question","content":{"title":"Replacement"}})).await;
            }
            "displaced" => {
                // Exercise the configured real stack's oldest-entry trim.
                let depth = match server.scene.lock().await.zone_registry.zones["notification-area"]
                    .contention_policy
                {
                    ContentionPolicy::Stack { max_depth } => max_depth,
                    _ => panic!("production notification stack"),
                };
                for index in 0..depth {
                    call(&server, "hud_publish", json!({"surface":"zone:notification-area","key":format!("peer{index}"),"content":{"title":"Peer"}})).await;
                }
            }
            _ => {
                call(
                    &server,
                    "hud_clear",
                    json!({"surface":"zone:notification-area","key":"question"}),
                )
                .await;
            }
        }
        let Poll::Ready(answer) = ask.as_mut().poll(&mut context) else {
            panic!("immediate {ending}, delayed={delayed}");
        };
        assert_eq!(answer, json!({"ok":true,"gone":ending}));
        assert_eq!(
            call(&server, "hud_input", json!({})).await,
            json!({"items":[],"remaining":0})
        );
    }

    // Delay counts from call; values above 30s clamp without a wall-clock wait.
    for delayed in [false, true] {
        let (server, clock) = self::server();
        let mut args = question(90000);
        if delayed {
            args["delay_ms"] = json!(40000);
        }
        let mut ask = Box::pin(call(&server, "hud_publish", args));
        assert!(ask.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(std::time::Duration::from_millis(29999)).await;
        clock.advance(29999);
        assert!(ask.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        clock.advance(1);
        let Poll::Ready(reply) = ask.as_mut().poll(&mut context) else {
            panic!("clamped absolute deadline");
        };
        assert_eq!(
            reply,
            json!({"ok":true,"expires_in_ms":if delayed {70000} else {30000}})
        );
        if delayed {
            clock.advance(10000);
            assert!(server.scene.lock().await.apply_due_batches()[0].applied);
        }
        activate_card(&server, latest_card(&server).await, false).await;
        let late = call(&server, "hud_input", json!({})).await;
        assert_eq!(late["items"][0]["action"], "ship");
        assert_eq!(call(&server, "hud_input", json!({})).await, late);
    }

    let (server, _) = self::server();
    for args in [
        json!({"surface":"widget:gauge","params":{"level":0.5},"wait_ms":1}),
        json!({"surface":"portal:new","wait_ms":1}),
        json!({"surface":"zone:notification-area","content":{"title":"No actions"},"wait_ms":0}),
        json!({"surface":"zone:subtitle","content":"Text","wait_ms":1}),
    ] {
        assert_eq!(
            call_err(&server, "hud_publish", args).await["code"],
            "INVALID_ARGUMENT"
        );
    }
    let scene = server.scene.lock().await;
    assert!(scene.zone_registry.active_publishes.is_empty());
    assert!(scene.widget_registry.active_publishes.is_empty());
    assert!(
        scene.leases.is_empty(),
        "invalid wait does not renew or create a lease"
    );
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
    let handle = server.portals.as_ref().unwrap().clone();
    assert!(Arc::ptr_eq(
        &server.input_notifiers.for_agent(&key.agent).unwrap(),
        &handle.input_notify(&key.agent).unwrap(),
    ));
    assert!(Arc::ptr_eq(&hub, &handle.hub));
    assert!(!Arc::ptr_eq(
        &handle.input_notify(&key.agent).unwrap(),
        &handle.input_notify("another-agent").unwrap(),
    ));

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
            tze_hud_portal::hub::Publish {
                text: Some("untouched other-agent output".into()),
                ..Default::default()
            },
            0,
        )
        .unwrap();
    let other_before = hub.lock().unwrap().get(&other).unwrap().clone();
    let deadline_probe = handle.observe_input_wait(&key.agent);
    let deadline_request = json!({
        "jsonrpc":"2.0", "id":deadline_probe.request_id(), "method":"tools/call",
        "params":{"name":"hud_input", "arguments":{"wait_ms":350}},
    })
    .to_string();
    let empty = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        server.dispatch(&deadline_request, &ctx()),
    )
    .await
    .unwrap();
    let empty: Value = serde_json::from_str(&empty).unwrap();
    assert_ne!(empty["result"]["isError"], true);
    let empty: Value =
        serde_json::from_str(empty["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(!deadline_probe.is_waiting());
    assert!(
        deadline_probe.waiting.has_changed().is_err(),
        "deadline closes the witness"
    );
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

    // Arm the real zero-portal call, then attach and submit through the same
    // production handle. One poll after commit must finish, without a timer.
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    let mut cx = Context::from_waker(Waker::noop());

    // A witness belongs to one authenticated request and only its armed wait.
    {
        let clock = TestClock::new(1_000);
        let mut agents = AgentDirectory::default();
        for (id, key) in [
            ("probe-a", "synthetic-probe-a"),
            ("probe-b", "synthetic-probe-b"),
        ] {
            agents.insert(
                id,
                hash_psk(key),
                vec!["resident_mcp".into(), "publish_zone:subtitle".into()],
            );
        }
        let portals = PortalHandle::new(PortalHub::default(), Arc::new(clock.clone()));
        let isolated = McpServer::new(scene(&clock))
            .with_config(McpConfig::with_agents(agents.shared()))
            .with_portals(portals.clone());
        let a = CallerContext::with_bearer("synthetic-probe-a");
        let b = CallerContext::with_bearer("synthetic-probe-b");
        let request = |id: &str, wait_ms| {
            json!({
                "jsonrpc":"2.0", "id":id, "method":"tools/call",
                "params":{"name":"hud_input", "arguments":{"wait_ms":wait_ms}},
            })
            .to_string()
        };
        let mut probe = portals.observe_input_wait("probe-a");
        // Neither another namespace nor another request id can fulfill it.
        for (caller, id) in [(&b, probe.request_id()), (&a, "unreserved-input-request")] {
            let body = request(id, 1_000);
            let mut unrelated = Box::pin(isolated.dispatch(&body, caller));
            assert!(unrelated.as_mut().poll(&mut cx).is_pending());
            assert!(!probe.is_waiting());
            drop(unrelated);
        }
        let other_tool = json!({
            "jsonrpc":"2.0", "id":probe.request_id(), "method":"tools/call",
            "params":{"name":"hud_surfaces", "arguments":{}},
        })
        .to_string();
        isolated.dispatch(&other_tool, &a).await;
        assert!(!probe.is_waiting());

        let body = request(probe.request_id(), 1_000);
        let held_scene = isolated.scene.lock().await;
        let mut waiting = Box::pin(isolated.dispatch(&body, &a));
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(
            !probe.is_waiting(),
            "scene acquisition precedes queue checks"
        );
        drop(held_scene);
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(
            probe.is_waiting(),
            "both empty predicates and Notify are armed"
        );
        let held_scene = isolated.scene.lock().await;
        portals.input_notify("probe-a").unwrap().notify_waiters();
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(
            !probe.is_waiting(),
            "wake clears readiness before rechecking"
        );
        drop(held_scene);
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(probe.is_waiting(), "empty recheck arms the next wait");
        let wake = isolated
            .scene
            .lock()
            .await
            .push_pending_action(PendingAction {
                publication_id: None,
                key: None,
                publisher_namespace: "probe-a".into(),
                zone_name: "subtitle".into(),
                kind: PendingActionKind::Callback("probe-ready".into()),
            });
        wake.notify();
        let Poll::Ready(raw) = waiting.as_mut().poll(&mut cx) else {
            panic!("committed input completes the correlated request");
        };
        let response: Value = serde_json::from_str(&raw).unwrap();
        let input: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(input["items"][0]["action"], "probe-ready");
        assert!(!probe.is_waiting());
        assert_eq!(
            std::pin::pin!(probe.wait_until_waiting())
                .as_mut()
                .poll(&mut cx),
            Poll::Ready(false),
            "response closes the witness"
        );
        drop(waiting);
        let (acked, error) = call_as(
            &isolated,
            &a,
            "hud_input",
            json!({"ack":[input["items"][0]["id"]]}),
        )
        .await;
        assert!(!error);
        assert_eq!(acked, json!({"items":[],"remaining":0}));

        let mut cancelled = portals.observe_input_wait("probe-a");
        let body = request(cancelled.request_id(), 1_000);
        let mut waiting = Box::pin(isolated.dispatch(&body, &a));
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(cancelled.is_waiting());
        drop(waiting); // Actual dispatch cancellation, not merely HTTP client abort.
        assert!(!cancelled.is_waiting());
        assert_eq!(
            std::pin::pin!(cancelled.wait_until_waiting())
                .as_mut()
                .poll(&mut cx),
            Poll::Ready(false)
        );

        let mut zero = portals.observe_input_wait("probe-a");
        let raw = isolated.dispatch(&request(zero.request_id(), 0), &a).await;
        assert!(!zero.is_waiting());
        assert_eq!(
            std::pin::pin!(zero.wait_until_waiting())
                .as_mut()
                .poll(&mut cx),
            Poll::Ready(false),
            "zero wait cannot arm: {raw}"
        );
        let response: Value = serde_json::from_str(&raw).unwrap();
        assert_ne!(response["result"]["isError"], true, "{raw}");

        let observer = portals.observe_input_wait("probe-a");
        let body = request(observer.request_id(), 1_000);
        let mut waiting = Box::pin(isolated.dispatch(&body, &a));
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(observer.is_waiting());
        drop(observer);
        assert!(
            waiting.as_mut().poll(&mut cx).is_pending(),
            "observer drop does not cancel dispatch"
        );
        drop(waiting);
        let unclaimed = portals.observe_input_wait("probe-a");
        let unclaimed_id = unclaimed.request_id().to_owned();
        drop(unclaimed);
        assert!(
            !portals
                .input_notifiers
                .wait_probes
                .lock()
                .unwrap()
                .pending
                .contains_key(&("probe-a".into(), unclaimed_id))
        );
    }

    let mut waiting = Box::pin(call(&server, "hud_input", json!({"wait_ms": 1500})));
    assert!(waiting.as_mut().poll(&mut cx).is_pending());
    assert_eq!(
        call(
            &server,
            "hud_publish",
            json!({"surface":"portal:main", "content":"first during wait"})
        )
        .await,
        json!({"ok":true, "reattached":false, "pending_input":0})
    );
    let first_id = handle
        .submit_reply(&key, "reply during wait".into(), 0)
        .unwrap()
        .result
        .unwrap();
    let found = match waiting.as_mut().poll(&mut cx) {
        Poll::Ready(found) => found,
        Poll::Pending => panic!("committed reply must finish input in one scheduler turn"),
    };
    drop(waiting);
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
            publication_id: None,
            key: None,
            publisher_namespace: psk_agent(),
            zone_name: "notification-area".into(),
            kind: PendingActionKind::Callback("approve".into()),
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
    // A producer before registration cannot lose the authoritative action.
    {
        let (isolated, _, a, _) = two_agents();
        let mut scene = isolated.scene.lock().await;
        let mut waiting = Box::pin(call_as(&isolated, &a, "hud_input", json!({"wait_ms":1000})));
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        scene.push_pending_action(PendingAction {
            publication_id: None,
            key: None,
            publisher_namespace: "a".into(),
            zone_name: "subtitle".into(),
            kind: PendingActionKind::Callback("before-check".into()),
        });
        drop(scene);
        let Poll::Ready((found, false)) = waiting.as_mut().poll(&mut cx) else {
            panic!("pre-check action must survive an unregistered notification");
        };
        assert_eq!(found["items"][0]["action"], "before-check");
    }

    // Two armed waits share retained ids; another agent stays asleep.
    {
        let (isolated, _, a, b) = two_agents();
        let mut first = Box::pin(call_as(&isolated, &a, "hud_input", json!({"wait_ms":1000})));
        let mut second = Box::pin(call_as(&isolated, &a, "hud_input", json!({"wait_ms":1000})));
        let mut other = Box::pin(call_as(&isolated, &b, "hud_input", json!({"wait_ms":1000})));
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert!(other.as_mut().poll(&mut cx).is_pending());
        let wake = isolated
            .scene
            .lock()
            .await
            .push_pending_action(PendingAction {
                publication_id: None,
                key: None,
                publisher_namespace: "a".into(),
                zone_name: "subtitle".into(),
                kind: PendingActionKind::Callback("after-check".into()),
            });
        wake.notify();
        let Poll::Ready((one, false)) = first.as_mut().poll(&mut cx) else {
            panic!("first owner wait");
        };
        let Poll::Ready((two, false)) = second.as_mut().poll(&mut cx) else {
            panic!("second owner wait");
        };
        assert_eq!(
            one, two,
            "coalesced wakes use retained input, not notification counts"
        );
        assert_eq!(one["items"][0]["action"], "after-check");
        assert!(other.as_mut().poll(&mut cx).is_pending());
        drop(other);
        let (acked, error) = call_as(
            &isolated,
            &a,
            "hud_input",
            json!({"ack":[one["items"][0]["id"]]}),
        )
        .await;
        assert!(!error);
        assert_eq!(acked, json!({"items":[],"remaining":0}));
    }

    // Cancellation before transfer and an error after transfer both retain input.
    {
        let (isolated, _, a, _) = two_agents();
        let mut scene = isolated.scene.lock().await;
        let mut waiting = Box::pin(call_as(&isolated, &a, "hud_input", json!({"wait_ms":1000})));
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        scene.push_pending_action(PendingAction {
            publication_id: None,
            key: None,
            publisher_namespace: "a".into(),
            zone_name: "subtitle".into(),
            kind: PendingActionKind::Callback("cancelled".into()),
        });
        drop(waiting);
        drop(scene);
        let (found, error) = call_as(&isolated, &a, "hud_input", json!({})).await;
        assert!(!error);
        assert_eq!(found["items"][0]["action"], "cancelled");
    }
    {
        let (failed, _) = self::server();
        let (mut failed, failed_hub) = hub_portal(failed);
        failed
            .scene
            .lock()
            .await
            .push_pending_action(PendingAction {
                publication_id: None,
                key: None,
                publisher_namespace: psk_agent(),
                zone_name: "subtitle".into(),
                kind: PendingActionKind::Callback("after-error".into()),
            });
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = failed_hub.lock().unwrap();
            panic!("fixture poison after action admission");
        }));
        assert!(poisoned.is_err());
        assert_eq!(
            call_err(&failed, "hud_input", json!({})).await["code"],
            "INTERNAL"
        );
        failed.portals = None;
        let found = call(&failed, "hud_input", json!({})).await;
        assert_eq!(found["items"][0]["action"], "after-error");
        assert_eq!(
            call(&failed, "hud_input", json!({})).await["items"][0]["id"],
            found["items"][0]["id"]
        );
    }

    // Deterministic model time exercises a real pending call's finite lease.
    // Small fixture limits expose boundaries; production defaults are untouched.
    {
        let (live, clock) = self::server();
        let portals = PortalHandle::new(
            PortalHub::new(tze_hud_portal::hub::PortalLimits {
                degrade_after_us: 10_000,
                reclaim_after_us: 10_000,
                input_ttl_us: 5_000,
                ..Default::default()
            }),
            Arc::new(clock.clone()),
        );
        let live_hub = Arc::clone(&portals.hub);
        let producer = portals.clone();
        let live = live.with_portals(portals);
        let own = PortalKey::new(psk_agent(), "late");
        let outsider = PortalKey::new("other", "late");
        let start = 1_000_000;
        let mut waiting = Box::pin(call(&live, "hud_input", json!({"wait_ms":1000})));
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(
            live_hub.lock().unwrap().next_deadline().is_none(),
            "zero portals adds no periodic main wake"
        );
        call(
            &live,
            "hud_publish",
            json!({"surface":"portal:late", "content":"late attach"}),
        )
        .await;
        {
            let mut hub = live_hub.lock().unwrap();
            hub.publish(&outsider, Default::default(), start).unwrap();
            hub.take_due(start);
        }
        producer
            .submit_reply(&own, "expires normally".into(), start)
            .unwrap()
            .result
            .unwrap();
        clock.advance_us(20_000);
        {
            let mut hub = live_hub.lock().unwrap();
            hub.sweep(start + 20_000);
            assert_eq!(hub.get(&own).unwrap().degraded_since_us, None);
            assert_eq!(
                hub.get(&own).unwrap().last_seen_us,
                start,
                "lease never fakes a future touch"
            );
            assert!(
                hub.get(&own).unwrap().inputs.is_empty(),
                "input TTL ignores active polls"
            );
            assert_eq!(
                hub.get(&outsider).unwrap().degraded_since_us,
                Some(start + 20_000)
            );
            assert!(
                hub.degrade(&own, start + 20_000),
                "explicit upstream loss ignores active polls"
            );
            hub.take_due(start + 20_000);
            assert_eq!(hub.next_deadline().unwrap().at_us, start + 30_000);
        }
        clock.advance_us(20_000);
        {
            let mut hub = live_hub.lock().unwrap();
            hub.sweep(start + 40_000);
            assert!(
                hub.get(&own).is_none(),
                "explicit degradation still reclaims on time"
            );
            assert!(hub.get(&outsider).is_none());
        }
        call(
            &live,
            "hud_publish",
            json!({"surface":"portal:late", "content":"new incarnation"}),
        )
        .await;
        // Hold the Hub during cancellation: Drop cannot acquire it, but the
        // atomic marker must make the next sweep ignore the cancelled lease.
        {
            let mut cancellation_guard = live_hub.lock().unwrap();
            drop(waiting);
            clock.advance_us(20_000);
            {
                let hub = &mut cancellation_guard;
                hub.sweep(start + 60_000);
                assert_eq!(hub.get(&own).unwrap().last_seen_us, start + 40_000);
                assert_eq!(
                    hub.get(&own).unwrap().degraded_since_us,
                    Some(start + 60_000),
                    "cancel releases without heartbeat"
                );
            }
            drop(cancellation_guard);
        }
        // Normal return touches actual now and releases only that finite request.
        let mut waiting = Box::pin(call(&live, "hud_input", json!({"wait_ms":1000})));
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        let id = producer
            .submit_reply(&own, "ready".into(), start + 60_000)
            .unwrap()
            .result
            .unwrap();
        let Poll::Ready(found) = waiting.as_mut().poll(&mut cx) else {
            panic!("live reply must finish");
        };
        assert_eq!(found["items"][0]["id"], id);
        drop(waiting);
        assert_eq!(
            live_hub.lock().unwrap().get(&own).unwrap().last_seen_us,
            start + 60_000
        );
        call(&live, "hud_input", json!({"ack":[id]})).await;
        let mut waiting = Box::pin(call(&live, "hud_input", json!({"wait_ms":1000})));
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        clock.advance_us(2_000_000);
        {
            let mut hub = live_hub.lock().unwrap();
            hub.sweep(start + 2_060_000);
            assert_eq!(
                hub.get(&own).unwrap().degraded_since_us,
                Some(start + 2_060_000),
                "finite expiry defeats a stalled owner"
            );
        }
        drop(waiting);
        call(&live, "hud_clear", json!({"surface":"portal:late"})).await;
        let mut zero = Box::pin(call(&live, "hud_input", json!({"wait_ms":1000})));
        assert!(zero.as_mut().poll(&mut cx).is_pending());
        drop(zero);
        call(
            &live,
            "hud_publish",
            json!({"surface":"portal:late", "content":"after cancelled zero-portal wait"}),
        )
        .await;
        clock.advance_us(20_000);
        {
            let mut hub = live_hub.lock().unwrap();
            hub.sweep(start + 2_080_000);
            assert_eq!(
                hub.get(&own).unwrap().degraded_since_us,
                Some(start + 2_080_000),
                "zero-portal cancellation leaves no hold for first attachment"
            );
            drop(hub);
        }

        // These two actual portal-backed requests own distinct liveness tokens.
        // Ending one must not end the other; ending the final one is immediate.
        call(&live, "hud_clear", json!({"surface":"portal:late"})).await;
        call(
            &live,
            "hud_publish",
            json!({"surface":"portal:late", "content":"overlap"}),
        )
        .await;
        let overlap_at = start + 2_080_000;
        let mut first = Box::pin(call(&live, "hud_input", json!({"wait_ms":1000})));
        let mut last = Box::pin(call(&live, "hud_input", json!({"wait_ms":1000})));
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(last.as_mut().poll(&mut cx).is_pending());
        {
            let mut hub = live_hub.lock().unwrap();
            hub.take_due(overlap_at);
            assert_eq!(hub.get(&own).unwrap().last_seen_us, overlap_at);
            let deadline = hub.next_deadline().unwrap();
            assert_eq!(deadline.kind, tze_hud_portal::hub::DeadlineKind::Liveness);
            assert!(deadline.at_us > overlap_at + 10_000);
            assert!(deadline.at_us <= overlap_at + 1_000_000);
        }
        drop(first);
        clock.advance_us(20_000);
        let mut hub = live_hub.lock().unwrap();
        hub.sweep(overlap_at + 20_000);
        assert_eq!(
            hub.get(&own).unwrap().degraded_since_us,
            None,
            "the remaining real request still protects its agent"
        );
        assert_eq!(
            hub.get(&own).unwrap().last_seen_us,
            overlap_at,
            "ending one request is not a heartbeat"
        );
        assert!(hub.next_deadline().unwrap().at_us > overlap_at + 20_000);
        // Contended final Drop still deactivates atomically before a Hub scan.
        drop(last);
        let deadline = hub.next_deadline().unwrap();
        assert_eq!(deadline.kind, tze_hud_portal::hub::DeadlineKind::Liveness);
        assert_eq!(
            deadline.at_us,
            overlap_at + 10_000,
            "final cancellation restores the original idle boundary immediately"
        );
        hub.sweep(overlap_at + 20_000);
        assert_eq!(
            hub.get(&own).unwrap().degraded_since_us,
            Some(overlap_at + 20_000)
        );
        assert_eq!(
            hub.get(&own).unwrap().last_seen_us,
            overlap_at,
            "final cancellation adds no heartbeat"
        );
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
    let (mut server, hub) = hub_portal(server);
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
            publication_id: None,
            key: None,
            publisher_namespace: AgentDirectory::unrestricted(psk())
                .resolve(&psk(), "")
                .unwrap()
                .agent_id,
            zone_name: "notification-area".into(),
            kind: PendingActionKind::Callback("approve".into()),
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

    // An admission already holding the Hub commits before pause. The pause
    // transition takes that same lock, then refuses every later publication.
    #[derive(Debug)]
    struct AdmissionClock {
        entered: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl tze_hud_scene::Clock for AdmissionClock {
        fn now_us(&self) -> u64 {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
            1_000_000
        }
        fn monotonic_us(&self) -> u64 {
            9_000_000_000
        }
    }
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    set_safe_mode(&server, false);
    server.portals.as_mut().unwrap().clock = Arc::new(AdmissionClock {
        entered: std::sync::Mutex::new(Some(entered_tx)),
        release: std::sync::Mutex::new(release_rx),
    });
    let server = Arc::new(server);
    let publishing = Arc::clone(&server);
    let admission = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(call(
                &publishing,
                "hud_publish",
                json!({"surface":"portal:main", "content":"committed before pause"}),
            ))
    });
    entered_rx.recv().unwrap();
    let pausing = Arc::clone(&server);
    let (pause_started_tx, pause_started_rx) = std::sync::mpsc::sync_channel(0);
    let pause = std::thread::spawn(move || {
        pause_started_tx.send(()).unwrap();
        let mut hub = pausing.portals.as_ref().unwrap().hub.lock().unwrap();
        hub.suspend_liveness(1_000_000);
        pausing
            .safe_mode
            .store(true, std::sync::atomic::Ordering::Release);
    });
    pause_started_rx.recv().unwrap();
    release_tx.send(()).unwrap();
    admission.join().unwrap();
    pause.join().unwrap();
    assert_eq!(hub.lock().unwrap().get(&key).unwrap().transcript.len(), 2);
    let rejected = call_err(
        &server,
        "hud_publish",
        json!({"surface":"portal:main", "content":"after pause"}),
    )
    .await;
    assert_eq!(rejected["code"], "SAFE_MODE_ACTIVE");
    assert!(!rejected["hint"].as_str().unwrap().is_empty());
    assert_eq!(hub.lock().unwrap().get(&key).unwrap().transcript.len(), 2);

    // Observe the existing phase after the early flag check but before Hub
    // acquisition. The observer coordinates timing, not a log-text assertion.
    struct PortalLockWait {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl tracing::Subscriber for PortalLockWait {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.target() == "tze_hud_mcp_phases"
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            #[derive(Default)]
            struct Phase(bool);
            impl tracing::field::Visit for Phase {
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0 |= field.name() == "phase" && value == "portal_lock_wait";
                }
            }
            let mut phase = Phase::default();
            event.record(&mut phase);
            if phase.0 {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
        }
    }
    for (name, args) in [
        (
            "hud_publish",
            json!({"surface":"portal:main", "content":"stale early pass"}),
        ),
        ("hud_hold", json!({"surface":"portal:main", "ttl_ms":9000})),
    ] {
        let mut guarded_hub = hub.lock().unwrap();
        guarded_hub.resume_liveness(1_000_000);
        server
            .safe_mode
            .store(false, std::sync::atomic::Ordering::Release);
        let before = guarded_hub.get(&key).unwrap().clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let requesting = Arc::clone(&server);
        let request = std::thread::spawn(move || {
            tracing::subscriber::with_default(
                PortalLockWait {
                    entered: entered_tx,
                    release: std::sync::Mutex::new(release_rx),
                },
                || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(call_err(&requesting, name, args))
                },
            )
        });
        // This receive can only complete after the request passed its early
        // safe-mode gate. Pause now linearizes while owning Hub admission.
        entered_rx.recv().unwrap();
        guarded_hub.suspend_liveness(1_000_000);
        server
            .safe_mode
            .store(true, std::sync::atomic::Ordering::Release);
        release_tx.send(()).unwrap();
        drop(guarded_hub);
        let refused = request.join().unwrap();
        assert_eq!(
            refused["code"], "SAFE_MODE_ACTIVE",
            "{name} queued behind pause"
        );
        let guarded_hub = hub.lock().unwrap();
        let after = guarded_hub.get(&key).unwrap();
        assert_eq!(after.transcript, before.transcript);
        assert_eq!(after.hold_until_us, before.hold_until_us);
        assert_eq!(after.last_seen_us, before.last_seen_us);
    }
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
