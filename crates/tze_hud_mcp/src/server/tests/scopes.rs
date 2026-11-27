//! Session scopes: each MCP session of one agent owns its own input,
//! content and portals; the agent keeps its budgets and human overrides.

use super::*;

/// Open a session as the test agent; returns its caller context.
async fn session(server: &McpServer, label: &str) -> CallerContext {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "_meta": {LABEL_META_KEY: label}},
    });
    match server.dispatch_session(&body.to_string(), &ctx()).await {
        SessionReply::Body {
            opened: Some(id), ..
        } => ctx().in_session(id),
        other => panic!("initialize opened no session: {other:?}"),
    }
}

fn scope_of(server: &McpServer, who: &CallerContext) -> Arc<crate::scopes::Scope> {
    let agent = AgentDirectory::unrestricted(psk())
        .resolve(&psk(), "")
        .unwrap()
        .agent_id;
    server
        .scopes
        .touch(&agent, who.session_id.as_deref().unwrap())
        .expect("live session")
}

async fn ok(server: &McpServer, who: &CallerContext, tool: &str, args: Value) -> Value {
    let (v, is_error) = call_as(server, who, tool, args).await;
    assert!(!is_error, "{tool} failed: {v}");
    v
}

fn question(title: &str) -> Value {
    json!({"surface": "zone:notification-area", "key": "build", "content": {
        "type": "notification", "title": title,
        "actions": [{"label": "Yes", "callback_id": "yes"}],
    }})
}

async fn card_of(server: &McpServer, namespace: &str) -> SceneId {
    server
        .scene
        .lock()
        .await
        .zone_registry
        .active_for_zone("notification-area")
        .iter()
        .find(|r| r.publisher_namespace == namespace)
        .expect("the session's card")
        .publication_id
}

async fn card_count(server: &McpServer) -> usize {
    server
        .scene
        .lock()
        .await
        .zone_registry
        .active_for_zone("notification-area")
        .len()
}

/// `initialize` hands back a session id without changing the result body;
/// an unknown id is refused before anything runs.
#[tokio::test]
async fn initialize_opens_a_session_and_unknown_ids_never_dispatch() {
    let (server, _) = server();
    let plain = rpc(&server, &ctx(), "initialize", json!({})).await;
    let a = session(&server, "tze-hud").await;
    let scoped = rpc(&server, &a, "initialize", json!({})).await;
    assert_eq!(scoped, plain, "the initialize result is unchanged");
    assert_eq!(scope_of(&server, &a).label().as_deref(), Some("tze-hud"));

    let ghost = ctx().in_session("0123456789abcdef0123456789abcdef");
    let publish = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "hud_publish", "arguments": question("ghost")}});
    assert_eq!(
        server.dispatch_session(&publish.to_string(), &ghost).await,
        SessionReply::UnknownSession
    );
    assert_eq!(card_count(&server).await, 0, "a refused call had no effect");
    // Another agent's PSK never reaches this agent's session.
    let stranger =
        CallerContext::with_bearer("other-key").in_session(a.session_id.clone().unwrap());
    assert!(matches!(
        server
            .dispatch_session(&publish.to_string(), &stranger)
            .await,
        SessionReply::Body { .. }
    ));
    assert_eq!(card_count(&server).await, 0);
}

/// R4.1: answers, ACKs and the backlog are per session; unscoped calls are
/// a session of their own.
#[tokio::test]
async fn input_reaches_only_the_session_that_asked() {
    let (server, _) = server();
    let (a, b) = (session(&server, "a").await, session(&server, "b").await);
    let unscoped = ctx();
    for (who, title) in [(&a, "A?"), (&b, "B?"), (&unscoped, "U?")] {
        ok(&server, who, "hud_publish", question(title)).await;
    }
    assert_eq!(card_count(&server).await, 3, "one build slot per session");
    let b_ns = scope_of(&server, &b).namespace.clone();
    activate(&server, card_of(&server, &b_ns).await, Some(0)).await;

    let empty = json!({"items": [], "remaining": 0});
    assert_eq!(ok(&server, &a, "hud_input", json!({})).await, empty);
    assert_eq!(ok(&server, &unscoped, "hud_input", json!({})).await, empty);
    let got = ok(&server, &b, "hud_input", json!({"max_items": 0})).await;
    assert_eq!(got["remaining"], 1, "only B's backlog: {got}");
    let got = ok(&server, &b, "hud_input", json!({})).await;
    assert_eq!(got["items"][0]["action"], "yes");
    let id = got["items"][0]["id"].clone();

    // A foreign ACK changes nothing for B.
    ok(&server, &a, "hud_input", json!({"ack": [id]})).await;
    let again = ok(&server, &b, "hud_input", json!({})).await;
    assert_eq!(again["items"][0]["id"], id);
    let acked = ok(&server, &b, "hud_input", json!({"ack": [id]})).await;
    assert_eq!(acked, empty);
}

#[tokio::test(start_paused = true)]
async fn typed_reply_reaches_only_its_exact_wait_or_publishing_session() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let (server, _) = server();
    let (a, b) = (session(&server, "a").await, session(&server, "b").await);
    let namespace = scope_of(&server, &a).namespace.clone();
    let args = json!({"surface":"zone:notification-area","key":"build","ttl_ms":60000,
        "content":{"title":"Which database?","actions":[{"label":"Other…","reply":"database"}]}});
    let empty = json!({"items":[],"remaining":0});
    let text = "use the staging DB — 東京";

    for wait in [false, true] {
        let mut args = args.clone();
        if wait {
            args["wait_ms"] = json!(30000);
        }
        let mut context = Context::from_waker(Waker::noop());
        let mut ask = Box::pin(ok(&server, &a, "hud_publish", args));
        if wait {
            assert!(ask.as_mut().poll(&mut context).is_pending());
        } else {
            assert!(ask.as_mut().poll(&mut context).is_ready());
        }
        let id = card_of(&server, &namespace).await;
        let (ready, waits) = {
            let mut scene = server.scene.lock().await;
            let region = render_card_control(&mut scene, id, Some(0));
            assert!(matches!(
                scene.activate_notification(&region, 0),
                Some(tze_hud_scene::graph::NotificationActivation::Reply(_))
            ));
            for rejected in [String::new(), "é".repeat(141), "two\nlines".into()] {
                assert!(
                    scene
                        .submit_notification_reply(&region, rejected, 0)
                        .is_none()
                );
            }
            let ready = scene
                .submit_notification_reply(&region, text.into(), 0)
                .expect("valid reply");
            assert!(
                scene
                    .submit_notification_reply(&region, text.into(), 0)
                    .is_none()
            );
            (ready, scene.take_notification_wait_notifiers())
        };
        ready.notify();
        for wake in waits {
            wake.notify();
        }
        assert_eq!(ok(&server, &b, "hud_input", json!({})).await, empty);
        if wait {
            assert_eq!(ok(&server, &a, "hud_input", json!({})).await, empty);
            let Poll::Ready(answer) = ask.as_mut().poll(&mut context) else {
                panic!("typed answer must resolve the armed wait");
            };
            assert_eq!(answer["action"], "database");
            assert_eq!(answer["text"], text);
            assert_eq!(ok(&server, &a, "hud_input", json!({})).await, empty);
        } else {
            let got = ok(&server, &a, "hud_input", json!({})).await;
            let id = got["items"][0]["id"].clone();
            assert!(id.is_string());
            assert_eq!(
                got,
                json!({"items":[{"id":id,"s":"zone:notification-area",
                "action":"database","text":text,"key":"build"}],"remaining":0})
            );
            assert_eq!(
                ok(&server, &b, "hud_input", json!({"ack":[id]})).await,
                empty
            );
            assert_eq!(ok(&server, &a, "hud_input", json!({})).await, got);
            assert_eq!(
                ok(&server, &a, "hud_input", json!({"ack":[id]})).await,
                empty
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn typed_reply_wait_expires_on_time_and_later_send_uses_the_fifo() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
        time::Duration,
    };
    use tze_hud_scene::graph::overlay::NotificationReplyView;

    let (server, clock) = server();
    let publisher = session(&server, "publisher").await;
    let namespace = scope_of(&server, &publisher).namespace.clone();
    let mut ask = Box::pin(ok(
        &server,
        &publisher,
        "hud_publish",
        json!({
            "surface":"zone:notification-area", "key":"build", "ttl_ms":1500, "wait_ms":1000,
            "content":{"title":"Where to deploy?", "actions":[{"label":"Other…", "reply":"target"}]}
        }),
    ));
    let mut context = Context::from_waker(Waker::noop());
    assert!(ask.as_mut().poll(&mut context).is_pending());
    let id = card_of(&server, &namespace).await;
    let text = "staging — 東京";
    let region = {
        let mut scene = server.scene.lock().await;
        let region = render_card_control(&mut scene, id, Some(0));
        assert!(scene.activate_notification(&region, 0).is_some());
        assert!(
            scene.set_notification_reply_view(Some(NotificationReplyView {
                publication_id: id,
                action_index: 0,
                text: text.into(),
                cursor_byte: text.len(),
                selection_anchor: text.len(),
                at_capacity: false,
            }))
        );
        region
    };
    clock.advance(1000);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        ask.as_mut().poll(&mut context),
        Poll::Ready(json!({"ok":true,"expires_in_ms":500}))
    );
    clock.advance(1000);
    tokio::time::advance(Duration::from_secs(1)).await;
    let wake = {
        let mut scene = server.scene.lock().await;
        assert_eq!(scene.drain_expired_zone_publications(), 0);
        scene
            .submit_notification_reply(&region, text.into(), 0)
            .unwrap()
    };
    wake.notify();
    let got = ok(&server, &publisher, "hud_input", json!({})).await;
    let item_id = got["items"][0]["id"].clone();
    assert!(item_id.is_string());
    assert_eq!(
        got,
        json!({"items":[{"id":item_id,"s":"zone:notification-area",
        "action":"target", "text":text,"key":"build"}],"remaining":0})
    );
    assert_eq!(ok(&server, &publisher, "hud_input", json!({})).await, got);
}

/// R4.2: keyless clears, `held` and `busy` are per session; R4.5: a session
/// may name its cards, unscoped zone and widget publishes still may not.
#[tokio::test]
async fn sessions_clear_and_hold_only_their_own_content() {
    let (server, _) = server();
    let (a, b) = (session(&server, "a").await, session(&server, "b").await);
    let unscoped = ctx();
    for who in [&a, &b, &unscoped] {
        ok(&server, who, "hud_publish", question("q")).await;
    }
    ok(
        &server,
        &a,
        "hud_clear",
        json!({"surface": "zone:notification-area"}),
    )
    .await;
    let b_ns = scope_of(&server, &b).namespace.clone();
    let left: Vec<String> = {
        let scene = server.scene.lock().await;
        scene
            .zone_registry
            .active_for_zone("notification-area")
            .iter()
            .map(|r| r.publisher_namespace.clone())
            .collect()
    };
    let agent = owner_agent_of(&b_ns);
    assert_eq!(left, [b_ns.clone(), agent.to_owned()]);

    ok(
        &server,
        &b,
        "hud_publish",
        json!({"surface": "zone:subtitle", "content": "b"}),
    )
    .await;
    let entry = |v: &Value, s: &str| {
        v["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["s"] == s)
            .unwrap()
            .clone()
    };
    let seen_by_a = ok(&server, &a, "hud_surfaces", json!({})).await;
    let subtitle = entry(&seen_by_a, "zone:subtitle");
    assert_eq!(subtitle["busy"], true);
    assert!(subtitle.get("held").is_none());
    assert!(
        entry(&seen_by_a, "zone:notification-area")
            .get("held")
            .is_none()
    );
    let seen_by_b = ok(&server, &b, "hud_surfaces", json!({})).await;
    assert_eq!(entry(&seen_by_b, "zone:subtitle")["held"], true);
    assert!(entry(&seen_by_b, "zone:subtitle").get("busy").is_none());

    // A refused publish leaves the label alone.
    let mut refused =
        json!({"surface": "zone:no-such-zone", "content": "x", "display_name": "nope"});
    let (_, is_error) = call_as(&server, &a, "hud_publish", refused.clone()).await;
    assert!(is_error);
    refused["surface"] = json!("zone:notification-area");
    refused["delay_ms"] = json!(u64::MAX);
    assert!(call_as(&server, &a, "hud_publish", refused).await.1);
    assert_eq!(scope_of(&server, &a).label().as_deref(), Some("a"));

    // display_name relabels a session; elsewhere it is still refused.
    let mut named = question("named");
    named["display_name"] = json!("release bot");
    ok(&server, &a, "hud_publish", named.clone()).await;
    assert_eq!(
        scope_of(&server, &a).label().as_deref(),
        Some("release bot")
    );
    // Its cards carry the label as their caption from that publish on.
    let captions: Vec<_> = server
        .scene
        .lock()
        .await
        .zone_registry
        .active_for_zone("notification-area")
        .iter()
        .map(|r| (r.caption.clone(), r.window_token.clone()))
        .collect();
    assert!(captions.contains(&(Some("release bot".into()), Some("release bot".into()))));
    let (err, is_error) = call_as(&server, &unscoped, "hud_publish", named).await;
    assert!(is_error);
    assert_eq!(err["code"], "INVALID_ARGUMENT");
    let widget = json!({"surface": "widget:gauge", "params": {"level": 0.5}, "display_name": "x"});
    let (err, is_error) = call_as(&server, &a, "hud_publish", widget).await;
    assert!(is_error);
    assert_eq!(err["code"], "INVALID_ARGUMENT");

    // The owner's Clear still reaches every session of the agent.
    server.scene.lock().await.reclaim_agent(agent);
    assert_eq!(card_count(&server).await, 0);
}

fn owner_agent_of(namespace: &str) -> &str {
    tze_hud_scene::scope::owner_agent(namespace)
}

/// R4.2 S3, R4.1 S3: a portal id belongs to the session that attached it;
/// replies reach only it; its header defaults to the session label.
#[tokio::test]
async fn a_portal_belongs_to_the_session_that_attached_it() {
    let (server, _) = server();
    let (server, hub) = hub_portal(server);
    let (a, b) = (
        session(&server, "tze-hud").await,
        session(&server, "docs").await,
    );
    let publish = json!({"surface": "portal:main", "content": "hello"});
    ok(&server, &a, "hud_publish", publish.clone()).await;
    let a_key = PortalKey::new(scope_of(&server, &a).namespace.as_str(), "main");
    assert_eq!(
        hub.lock().unwrap().get(&a_key).unwrap().display_name,
        "tze-hud"
    );
    let before = format!("{:?}", hub.lock().unwrap().get(&a_key).unwrap());

    let (err, is_error) = call_as(&server, &b, "hud_publish", publish.clone()).await;
    assert!(is_error);
    assert_eq!(err["code"], "NOT_ALLOWED");
    let hint = err["hint"].as_str().unwrap();
    assert_eq!(
        hint,
        "portal:main belongs to your session \"tze-hud\"; publish to another portal id"
    );
    assert!(
        hint.split_whitespace().count() <= 20,
        "well under 60 tokens"
    );
    assert_eq!(
        format!("{:?}", hub.lock().unwrap().get(&a_key).unwrap()),
        before
    );
    let listed = ok(&server, &b, "hud_surfaces", json!({})).await;
    assert!(!listed.to_string().contains("portal:main"));
    let (err, _) = call_as(&server, &ctx(), "hud_publish", publish.clone()).await;
    assert_eq!(
        err["code"], "NOT_ALLOWED",
        "unscoped is another session too"
    );

    hub.lock()
        .unwrap()
        .submit_reply(&a_key, "for a".into(), 1)
        .unwrap();
    let empty = json!({"items": [], "remaining": 0});
    assert_eq!(ok(&server, &b, "hud_input", json!({})).await, empty);
    let got = ok(&server, &a, "hud_input", json!({})).await;
    assert_eq!(got["items"][0]["text"], "for a");

    // Once the runtime reclaims it, any session may attach.
    hub.lock().unwrap().reclaim(&a_key);
    let reattached = ok(&server, &b, "hud_publish", publish).await;
    assert_eq!(
        reattached["reattached"], false,
        "B's own key was never reclaimed"
    );
}

/// R4.3 S1: ending a session clears its cards, widget, delayed publish,
/// portal and un-ACKed input, and leaves siblings alone.
#[tokio::test]
async fn ending_a_session_releases_everything_it_owns() {
    let (server, _) = server();
    let (server, hub) = hub_portal(server);
    let (a, b) = (session(&server, "a").await, session(&server, "b").await);
    let (a_ns, b_ns) = (
        scope_of(&server, &a).namespace.clone(),
        scope_of(&server, &b).namespace.clone(),
    );
    for (who, portal) in [(&a, "portal:a"), (&b, "portal:b")] {
        ok(&server, who, "hud_publish", question("q")).await;
        ok(
            &server,
            who,
            "hud_publish",
            json!({"surface": portal, "content": "x"}),
        )
        .await;
    }
    let gauge = json!({"surface": "widget:gauge", "params": {"level": 0.5}});
    ok(&server, &a, "hud_publish", gauge).await;
    let later = json!({"surface": "zone:subtitle", "content": "later", "delay_ms": 60000});
    ok(&server, &a, "hud_publish", later).await;
    activate(&server, card_of(&server, &a_ns).await, Some(0)).await;
    let a_key = PortalKey::new(a_ns.as_str(), "a");
    hub.lock()
        .unwrap()
        .submit_reply(&a_key, "unread".into(), 1)
        .unwrap();
    let delivered = ok(&server, &a, "hud_input", json!({"max_items": 1})).await;
    assert_eq!(
        delivered["remaining"], 1,
        "one delivered, one queued: {delivered}"
    );

    assert_eq!(server.end_session(&a).await, SessionEnd::Released);
    {
        let scene = server.scene.lock().await;
        assert!(
            scene.scope_holdings(&a_ns).is_empty(),
            "cards, widget, delayed"
        );
        assert!(
            !scene.pending_actions.contains_key(&a_ns),
            "un-ACKed input is gone"
        );
        assert_eq!(scene.scope_holdings(&b_ns).zones, 1, "B is untouched");
    }
    assert!(hub.lock().unwrap().list(&a_ns).is_empty());
    assert_eq!(hub.lock().unwrap().list(&b_ns).len(), 1);
    assert!(!server.state.knows(&a_ns));

    // The ended id is unknown now, for calls and for a second DELETE.
    let ping = json!({"jsonrpc": "2.0", "id": 9, "method": "ping"});
    assert_eq!(
        server.dispatch_session(&ping.to_string(), &a).await,
        SessionReply::UnknownSession
    );
    assert_eq!(server.end_session(&a).await, SessionEnd::Unknown);
    assert_eq!(
        server.end_session(&CallerContext::guest()).await,
        SessionEnd::Unauthenticated
    );
}

/// R4.3 S5: a released session's armed waits return at once: `hud_input`
/// with its deadline reply, a question wait with `gone: cleared`.
#[tokio::test]
async fn ending_a_session_returns_its_armed_waits_at_once() {
    let (server, _) = server();
    let a = session(&server, "a").await;
    let a_ns = scope_of(&server, &a).namespace.clone();
    let mut probe = server.input_notifiers.observe_input_wait(&a_ns);
    let wait_input = json!({"jsonrpc": "2.0", "id": probe.request_id(), "method": "tools/call",
        "params": {"name": "hud_input", "arguments": {"wait_ms": 30000}}});
    let wait_input = wait_input.to_string();
    let mut asking = question("wait");
    asking["wait_ms"] = json!(30000);
    let (input, (asked, is_error), ()) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                server.dispatch(&wait_input, &a),
                call_as(&server, &a, "hud_publish", asking),
                async {
                    assert!(probe.wait_until_waiting().await);
                    while card_count(&server).await == 0 {
                        tokio::task::yield_now().await;
                    }
                    assert_eq!(server.end_session(&a).await, SessionEnd::Released);
                }
            )
        })
        .await
        .expect("release ends armed waits at once");
    let input: Value = serde_json::from_str(&input).unwrap();
    let input: Value =
        serde_json::from_str(input["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(input, json!({"items": [], "remaining": 0}));
    assert!(!is_error, "{asked}");
    assert_eq!(asked["gone"], "cleared");
    assert_eq!(card_count(&server).await, 0);
}

/// R4.3 S6: the 65th session releases the least-recently-called idle one.
#[tokio::test]
async fn the_session_table_is_bounded_per_agent() {
    let (server, _) = server();
    let first = session(&server, "first").await;
    ok(&server, &first, "hud_publish", question("busy")).await;
    let mut rest = Vec::new();
    for i in 1..crate::scopes::MAX_SCOPES_PER_AGENT {
        rest.push(session(&server, &format!("s{i}")).await);
    }
    let newest = session(&server, "newest").await;
    assert_eq!(
        server.scopes.list().len(),
        crate::scopes::MAX_SCOPES_PER_AGENT
    );
    let ping = json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}).to_string();
    // The oldest holds a card, so the oldest idle session went instead.
    assert!(matches!(
        server.dispatch_session(&ping, &first).await,
        SessionReply::Body { .. }
    ));
    assert_eq!(
        server.dispatch_session(&ping, &rest[0]).await,
        SessionReply::UnknownSession
    );
    assert!(matches!(
        server.dispatch_session(&ping, &newest).await,
        SessionReply::Body { .. }
    ));
    assert_eq!(card_count(&server).await, 1);
}

/// Native unread is per session: each session's discovery lists only its
/// own portals with their unread counts, and its publish replies report its
/// own portal's unread; another session's output never adds to them.
#[tokio::test]
async fn portal_unread_is_per_session() {
    let (server, _) = server();
    let (server, _hub) = hub_portal(server);
    let (a, b) = (
        session(&server, "tze-hud").await,
        session(&server, "docs").await,
    );
    for text in ["one", "two", "three"] {
        ok(
            &server,
            &a,
            "hud_publish",
            json!({"surface": "portal:main", "content": text}),
        )
        .await;
    }
    ok(
        &server,
        &b,
        "hud_publish",
        json!({"surface": "portal:side", "content": "one"}),
    )
    .await;
    let portals = |listed: &Value| -> Vec<(String, Value)> {
        listed["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["s"].as_str().unwrap().starts_with("portal:"))
            .map(|s| (s["s"].as_str().unwrap().to_owned(), s["unread"].clone()))
            .collect()
    };
    let seen_by_a = ok(&server, &a, "hud_surfaces", json!({})).await;
    assert_eq!(portals(&seen_by_a), [("portal:main".to_owned(), json!(3))]);
    let seen_by_b = ok(&server, &b, "hud_surfaces", json!({})).await;
    assert_eq!(portals(&seen_by_b), [("portal:side".to_owned(), json!(1))]);
    let unscoped = ok(&server, &ctx(), "hud_surfaces", json!({})).await;
    assert!(portals(&unscoped).is_empty());
    // B's publish reports only its own portal's unread.
    let next = ok(
        &server,
        &b,
        "hud_publish",
        json!({"surface": "portal:side", "content": "two"}),
    )
    .await;
    assert_eq!(next["unread"], 1, "{next}");
}

/// A refused relabel leaves the session label alone. An accepted delayed
/// relabel changes the live dock caption without renaming earlier cards.
#[tokio::test]
async fn a_refused_relabel_leaves_the_caption_alone() {
    use std::num::NonZeroU64;
    use std::sync::atomic::AtomicUsize;
    use tze_hud_scene::Rect;
    use tze_hud_scene::dock::{DockLayout, DockMetrics, DockProjection};

    let (server, _) = server();
    let wakes = Arc::new(AtomicUsize::new(0));
    let server = server.with_render_wake_notifier(RenderWakeNotifier::new({
        let wakes = Arc::clone(&wakes);
        move || {
            wakes.fetch_add(1, Ordering::Relaxed);
        }
    }));
    let _first = session(&server, "tze-hud").await;
    let a = session(&server, "tze-hud").await;
    let caption_of = |scene: &tze_hud_scene::graph::SceneGraph, ns: &str| {
        scene
            .zone_registry
            .active_for_zone("notification-area")
            .iter()
            .rev()
            .find(|r| r.publisher_namespace == ns)
            .and_then(|r| r.caption.clone())
    };
    let ns = scope_of(&server, &a).namespace.clone();
    // The agent's one allowed card is already up.
    ok(&server, &a, "hud_publish", question("first")).await;
    let (instance, expiry) = {
        let mut scene = server.scene.lock().await;
        scene
            .zone_registry
            .zones
            .get_mut("notification-area")
            .unwrap()
            .max_publishers = 1;
        scene.set_dock_projection(
            &["gauge".into()],
            Some(DockProjection {
                target: Some("main".into()),
                layout: DockLayout::new(
                    Rect::new(0.0, 0.0, 1920.0, 1080.0),
                    4,
                    DockMetrics {
                        row_height: 52.0,
                        caption_width: 96.0,
                        gap: 8.0,
                    },
                    [2.0],
                ),
                pixels_per_dp: 1.0,
                aspect_ratios: vec![("gauge".into(), 2.0)],
            }),
            NonZeroU64::new(10_000).unwrap(),
        );
        scene
            .publish_to_dock("gauge", HashMap::new(), &ns, None, 0, 0)
            .unwrap();
        assert_eq!(scene.dock_caption(&ns), Some("tze-hud (2)"));
        let instance = scene
            .dock_instance(&ns, "gauge")
            .unwrap()
            .instance_name
            .clone();
        let expiry = scene.widget_registry.active_publishes[&instance][0].expires_at_wall_us;
        (instance, expiry)
    };
    let before_refusal = wakes.load(Ordering::Relaxed);
    let mut named = question("named");
    named["display_name"] = json!("release bot");
    named["key"] = json!("another"); // a keyed replacement is never capped
    let (err, is_error) = call_as(&server, &a, "hud_publish", named.clone()).await;
    assert!(is_error, "{err}");
    assert_ne!(
        err["code"], "INVALID_ARGUMENT",
        "refused by the scene: {err}"
    );
    assert_eq!(
        scope_of(&server, &a).label().as_deref(),
        Some("tze-hud (2)")
    );
    assert_eq!(
        server.scene.lock().await.dock_caption(&ns),
        Some("tze-hud (2)")
    );
    assert_eq!(wakes.load(Ordering::Relaxed), before_refusal);

    // With room again, an ordinary card carries the previous label.
    {
        let mut scene = server.scene.lock().await;
        scene
            .zone_registry
            .zones
            .get_mut("notification-area")
            .unwrap()
            .max_publishers = 8;
    }
    ok(&server, &a, "hud_publish", question("plain")).await;
    let (version, geometry) = {
        let scene = server.scene.lock().await;
        assert_eq!(caption_of(&scene, &ns).as_deref(), Some("tze-hud (2)"));
        (scene.version, scene.geometry_epoch)
    };
    let before_delay = wakes.load(Ordering::Relaxed);
    named["delay_ms"] = json!(5_000);
    ok(&server, &a, "hud_publish", named.clone()).await;
    {
        let scene = server.scene.lock().await;
        assert_eq!(scene.dock_caption(&ns), Some("release bot"));
        assert_eq!(caption_of(&scene, &ns).as_deref(), Some("tze-hud (2)"));
        assert!(
            scene.version > version,
            "existing lease touch invalidates rendering"
        );
        assert_eq!(scene.geometry_epoch, geometry);
    }
    assert_eq!(wakes.load(Ordering::Relaxed), before_delay + 1);
    ok(&server, &a, "hud_publish", named).await;
    let scene = server.scene.lock().await;
    assert_eq!(scene.dock_caption(&ns), Some("release bot"));
    assert_eq!(caption_of(&scene, &ns).as_deref(), Some("tze-hud (2)"));
    assert_eq!(
        scene.widget_registry.active_publishes[&instance][0].expires_at_wall_us,
        expiry
    );
    assert_eq!(scene.geometry_epoch, geometry);
}
