use super::tests::{http_raw, make_config, make_scene};
use super::*;
use serde_json::{Value, json};
use std::time::Duration;
use tze_hud_mcp::scopes::MAX_SCOPES_PER_AGENT;
use tze_hud_mcp::server::{CallerContext, SessionReply};
use tze_hud_scene::config::{AgentDirectory, DEFAULT_MCP_AGENT_ID, hash_psk};
use tze_hud_scene::scope::scoped_namespace;
use tze_hud_scene::types::{Rect, ZoneHitRegion, ZoneInteractionKind, ZoneRegistry};

fn initialize(meta: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"_meta":meta}})
}

fn linked(parent: &str) -> Value {
    initialize(json!({"tze.hud/parent":parent, "tze-hud/label":"ignored"}))
}

fn tool(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":name,"arguments":arguments}})
}

fn card() -> Value {
    tool(
        "hud_publish",
        json!({
            "surface":"zone:notification-area","key":"question","ttl_ms":600000,
            "content":{"type":"notification","title":"Fixture","body":"",
                       "actions":[{"label":"Proceed","callback_id":"366981ce14359a65.0"}]}
        }),
    )
}

async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    bearer: &str,
    session: Option<&str>,
    body: Option<Value>,
) -> String {
    let header = session
        .map(|id| format!("Mcp-Session-Id: {id}\r\n"))
        .unwrap_or_default();
    let body = body.map(|v| v.to_string()).unwrap_or_default();
    http_raw(
        addr,
        &format!("{method} {path} HTTP/1.1\r\nAuthorization: Bearer {bearer}\r\n{header}Content-Length: {}\r\n\r\n{body}", body.len()),
    )
    .await
}

async fn post(addr: SocketAddr, bearer: &str, session: Option<&str>, body: Value) -> String {
    request(addr, "POST", "/mcp", bearer, session, Some(body)).await
}

fn body(response: &str) -> Value {
    serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
}

fn id(response: &str) -> Option<&str> {
    response
        .lines()
        .find_map(|s| s.strip_prefix("Mcp-Session-Id: "))
}

fn tool_body(response: &str) -> Value {
    let body = body(response);
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{response}"));
    assert!(body["result"]["isError"].is_null(), "{response}");
    serde_json::from_str(text).unwrap()
}

async fn open(addr: SocketAddr, bearer: &str, init: Value) -> String {
    let response = post(addr, bearer, None, init).await;
    id(&response)
        .unwrap_or_else(|| panic!("{response}"))
        .to_owned()
}

#[tokio::test]
async fn sibling_initialization_requires_one_live_same_agent_primary() {
    let scene = make_scene();
    scene.lock().await.zone_registry = ZoneRegistry::with_defaults();
    let shutdown = ShutdownToken::new();
    let mut config = make_config(0, "");
    let mut agents = AgentDirectory::default();
    for agent in ["model", "other"] {
        agents.insert(
            agent,
            hash_psk(agent),
            vec!["*".into(), "operator_admin".into()],
        );
    }
    config.agents = agents.shared();
    let (handle, addr) = start_mcp_http_server(Arc::clone(&scene), config, shutdown.clone(), None)
        .await
        .unwrap();
    let addr = addr[0];
    let parent = open(addr, "model", initialize(json!({"tze-hud/label":"Work"}))).await;
    let other = open(addr, "other", initialize(json!({}))).await;
    let sibling = open(addr, "model", linked(&parent)).await;
    for (session, init) in [
        (None, linked("unknown")),
        (None, linked(&other)),
        (None, linked(&sibling)),
        (None, linked(&parent)),
        (Some(parent.as_str()), linked(&parent)),
        (None, initialize(json!({"tze.hud/parent":null}))),
    ] {
        let response = post(addr, "model", session, init).await;
        assert_eq!(body(&response)["error"]["code"], -32602, "{response}");
        assert!(id(&response).is_none(), "{response}");
    }
    let published = post(addr, "model", Some(&sibling), card()).await;
    assert!(
        body(&published)["result"]["isError"].is_null(),
        "{published}"
    );
    {
        let scene = scene.lock().await;
        let [record] = scene.zone_registry.active_for_zone("notification-area") else {
            panic!("expected one live card");
        };
        assert_eq!(record.caption.as_deref(), Some("Work"));
        assert_eq!(record.merge_key.as_deref(), Some("question"));
    }
    post(addr, "model", Some(&sibling),
        json!({"jsonrpc":"2.0","method":"notifications/tze/session_state","params":{"state":"working"}})).await;
    let admin = request(addr, "GET", "/admin/status?cpu=0", "model", None, None).await;
    let admin_body = body(&admin);
    let rows = admin_body["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let row = rows.iter().find(|r| r["agent"] == "model").unwrap();
    assert_eq!(row["label"], "Work");
    assert_eq!(row["holdings"], 0);
    assert_eq!(row["sibling"]["label"], "Work");
    assert_eq!(row["sibling"]["holdings"], 1);
    assert!(row["sibling"]["state"].is_null());
    let rows = admin_body["sessions"].to_string();
    assert!(!rows.contains(&parent) && !rows.contains(&sibling));
    shutdown.trigger(crate::threads::ShutdownReason::Clean);
    handle.await.unwrap();
}

#[tokio::test]
async fn sibling_release_follows_http_delete_eviction_and_restart() {
    let ping = json!({"jsonrpc":"2.0","id":3,"method":"ping"});
    for end in ["delete", "evict-parent", "evict-sibling", "restart"] {
        let scene = make_scene();
        scene.lock().await.zone_registry = ZoneRegistry::with_defaults();
        let mut shutdown = ShutdownToken::new();
        let (mut handle, mut addrs) = start_mcp_http_server(
            Arc::clone(&scene),
            make_config(0, "k"),
            shutdown.clone(),
            None,
        )
        .await
        .unwrap();
        let mut addr = addrs[0];
        let parent = open(addr, "k", initialize(json!({}))).await;
        let sibling = open(addr, "k", linked(&parent)).await;
        let published = post(addr, "k", Some(&sibling), card()).await;
        assert!(
            body(&published)["result"]["isError"].is_null(),
            "{published}"
        );
        if end == "evict-sibling" {
            // Touch the parent later and fill every scope with holdings so that
            // LRU can release the oldest sibling while its card is still live.
            post(addr, "k", Some(&parent), ping.clone()).await;
            scene
                .lock()
                .await
                .grant_lease(&scoped_namespace(DEFAULT_MCP_AGENT_ID, &parent), 60_000);
        }
        if end.starts_with("evict") {
            for _ in 0..MAX_SCOPES_PER_AGENT - 2 {
                let filler = open(addr, "k", initialize(json!({}))).await;
                if end == "evict-sibling" {
                    scene
                        .lock()
                        .await
                        .grant_lease(&scoped_namespace(DEFAULT_MCP_AGENT_ID, &filler), 60_000);
                }
            }
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            match end {
                "delete" => {
                    let response = request(addr, "DELETE", "/mcp", "k", Some(&parent), None).await;
                    assert!(response.starts_with("HTTP/1.1 202 "), "{response}");
                }
                "evict-parent" | "evict-sibling" => {
                    open(addr, "k", initialize(json!({}))).await;
                }
                _ => {}
            }
        })
        .await
        .unwrap();
        if end == "restart" {
            shutdown.trigger(crate::threads::ShutdownReason::Clean);
            handle.await.unwrap();
            shutdown = ShutdownToken::new();
            (handle, addrs) =
                start_mcp_http_server(make_scene(), make_config(0, "k"), shutdown.clone(), None)
                    .await
                    .unwrap();
            addr = addrs[0];
        } else {
            assert!(
                scene
                    .lock()
                    .await
                    .zone_registry
                    .active_for_zone("notification-area")
                    .is_empty(),
                "{end}"
            );
        }
        for (session, status) in [
            (&parent, if end == "evict-sibling" { 200 } else { 404 }),
            (&sibling, 404),
        ] {
            let response = post(addr, "k", Some(session), ping.clone()).await;
            assert!(
                response.starts_with(&format!("HTTP/1.1 {status} ")),
                "{end}, expected {status}: {response}"
            );
        }
        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn trusted_scope_release_clears_active_and_delayed_sibling_publications() {
    let scene = make_scene();
    scene.lock().await.zone_registry = ZoneRegistry::with_defaults();
    let server =
        McpServer::with_shared_scene(Arc::clone(&scene)).with_config(McpConfig::with_psk("k"));
    let ctx = CallerContext::with_bearer("k");
    let open = |init: Value| {
        let ctx = ctx.clone();
        let server = &server;
        async move {
            let SessionReply::Body {
                opened: Some(id), ..
            } = server.dispatch_session(&init.to_string(), &ctx).await
            else {
                panic!("scope failed to initialize");
            };
            id
        }
    };
    let parent = open(initialize(json!({}))).await;
    let sibling = open(linked(&parent)).await;
    let sibling_ctx = ctx.clone().in_session(&sibling);
    let mut delayed = card();
    delayed["params"]["arguments"]["delay_ms"] = json!(300000);
    for publication in [card(), delayed] {
        let SessionReply::Body { body: reply, .. } = server
            .dispatch_session(&publication.to_string(), &sibling_ctx)
            .await
        else {
            panic!("scope unexpectedly missing");
        };
        assert!(!reply.contains("isError"), "{reply}");
    }
    {
        let graph = scene.lock().await;
        assert_eq!(
            graph
                .zone_registry
                .active_for_zone("notification-area")
                .len(),
            1
        );
        let ns = scoped_namespace(DEFAULT_MCP_AGENT_ID, &sibling);
        assert!(graph.holding_namespaces().contains(&ns));
        assert_eq!(
            graph
                .scheduled_zone_publication_expiries("notification-area", &ns)
                .count(),
            1
        );
    }
    assert!(server.release_session(DEFAULT_MCP_AGENT_ID, &parent).await);
    let graph = scene.lock().await;
    assert!(
        graph
            .zone_registry
            .active_for_zone("notification-area")
            .is_empty()
    );
    assert!(graph.holding_namespaces().is_empty());
    assert!(server.scopes().list().is_empty());
}

#[tokio::test]
async fn sibling_receipts_retain_a_key_only_dismiss_across_all_delivery_stages() {
    let scene = make_scene();
    scene.lock().await.zone_registry = ZoneRegistry::with_defaults();
    let shutdown = ShutdownToken::new();
    let (handle, addrs) = start_mcp_http_server(
        Arc::clone(&scene),
        make_config(0, "k"),
        shutdown.clone(),
        None,
    )
    .await
    .unwrap();
    let addr = addrs[0];
    let parent = open(addr, "k", initialize(json!({}))).await;
    let sibling = open(addr, "k", linked(&parent)).await;
    let input = || tool("hud_input", json!({}));
    for source in ["scene", "retained", "encoded"] {
        assert_eq!(
            tool_body(&post(addr, "k", Some(&sibling), card()).await)["ok"],
            true
        );
        {
            let mut graph = scene.lock().await;
            let old = &graph.zone_registry.active_for_zone("notification-area")[0];
            let dismiss = ZoneHitRegion {
                publication_id: old.publication_id,
                zone_name: old.zone_name.clone(),
                published_at_wall_us: old.published_at_wall_us,
                publisher_namespace: old.publisher_namespace.clone(),
                bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
                kind: ZoneInteractionKind::Dismiss,
                interaction_id: "fixture-dismiss".into(),
                tab_order: 0,
            };
            graph.overlay.zone_hit_regions.push(dismiss.clone());
            assert!(graph.activate_notification(&dismiss, 0).is_some());
        }
        let first = if source == "scene" {
            None
        } else {
            Some(post(addr, "k", Some(&sibling), input()).await)
        };
        let mut replacement = card();
        replacement["params"]["arguments"]["content"]["title"] = json!("Replacement");
        assert_eq!(
            tool_body(&post(addr, "k", Some(&sibling), replacement).await)["ok"],
            true
        );
        let redelivered = tool_body(&post(addr, "k", Some(&sibling), input()).await);
        let items = redelivered["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{source}: {redelivered}");
        let old_item_id = items[0]["id"].as_str().unwrap();
        assert_eq!(
            items[0],
            json!({"id":old_item_id, "s":"zone:notification-area",
            "key":"question", "action":"dismiss", "kind":"dismiss"})
        );
        if let Some(first) = first {
            assert_eq!(tool_body(&first), redelivered);
        }
        let ack = || tool("hud_input", json!({"ack":[old_item_id]}));
        let primary = tool_body(&post(addr, "k", Some(&parent), ack()).await);
        assert_eq!(primary["items"], json!([]));
        assert_eq!(
            tool_body(&post(addr, "k", Some(&sibling), input()).await),
            redelivered
        );
        assert_eq!(
            tool_body(&post(addr, "k", Some(&sibling), ack()).await)["items"],
            json!([])
        );
    }
    shutdown.trigger(crate::threads::ShutdownReason::Clean);
    handle.await.unwrap();
}
