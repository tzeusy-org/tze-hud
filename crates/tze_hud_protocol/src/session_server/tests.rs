use super::*;

#[tokio::test]
async fn transactional_command_input_does_not_lag_under_receiver_backpressure() {
    let service = HudSessionImpl::new(SceneGraph::new(800.0, 600.0), "test-psk");
    let mut receiver = service.input_event_tx.subscribe("agent-a");

    for interaction_id in 0..=BROADCAST_CHANNEL_CAPACITY as u64 {
        let batch = crate::proto::EventBatch {
            frame_number: 0,
            batch_ts_us: interaction_id,
            events: vec![crate::proto::InputEnvelope {
                event: Some(crate::proto::input_envelope::Event::CommandInput(
                    crate::proto::CommandInputEvent {
                        interaction_id: interaction_id.to_string(),
                        ..Default::default()
                    },
                )),
            }],
        };
        service.inject_input_event("agent-a", batch);
    }

    let (_, first_batch) = receiver
        .recv()
        .await
        .expect("transactional command input must never report receiver lag");
    let first_command = first_batch.events[0]
        .event
        .as_ref()
        .and_then(|event| match event {
            crate::proto::input_envelope::Event::CommandInput(command) => Some(command),
            _ => None,
        })
        .expect("first event must remain a command input event");
    assert_eq!(first_command.interaction_id, "0");
}

#[test]
fn transactional_input_is_not_enqueued_for_an_unrelated_namespace() {
    let service = HudSessionImpl::new(SceneGraph::new(800.0, 600.0), "test-psk");
    let mut agent_a_receiver = service.input_event_tx.subscribe("agent-a");
    let batch = crate::proto::EventBatch {
        frame_number: 0,
        batch_ts_us: 1,
        events: vec![crate::proto::InputEnvelope {
            event: Some(crate::proto::input_envelope::Event::CommandInput(
                crate::proto::CommandInputEvent {
                    interaction_id: "foreign-command".to_string(),
                    ..Default::default()
                },
            )),
        }],
    };

    service.inject_input_event("agent-b", batch);

    assert!(
        agent_a_receiver.try_recv().is_err(),
        "agent-a durable queue must not receive agent-b transactional input"
    );
}
use crate::proto::session::hud_session_client::HudSessionClient;
use crate::proto::session::hud_session_server::HudSessionServer;
use std::collections::HashMap;
use tokio_stream::StreamExt;
use tze_hud_scene::graph::SceneGraph;

/// Load an [`ElementStore`] from a TOML file on disk.
///
/// Test-only helper that replaces the former `ElementStore::load_or_default`
/// method.  Missing files are treated as first boot and return an empty store.
fn load_element_store_for_test(
    path: &std::path::Path,
) -> std::io::Result<tze_hud_scene::element_store::ElementStore> {
    match std::fs::read_to_string(path) {
        Ok(content) => toml::from_str(&content).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid element_store TOML: {err}"),
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(tze_hud_scene::element_store::ElementStore::default())
        }
        Err(err) => Err(err),
    }
}

/// Consume the next message from a stream.
async fn next_server_msg(
    stream: &mut tonic::Streaming<crate::proto::session::ServerMessage>,
) -> crate::proto::session::ServerMessage {
    stream.next().await.unwrap().unwrap()
}

/// Agents whose `allow` list is deliberately narrow, for the denial tests.
/// Every other agent id the dev PSK claims is unrestricted.
fn restricted_test_agents() -> HashMap<String, Vec<String>> {
    let agents: [(&str, &[&str]); 5] = [
        ("no-input-agent", &["create_tiles", "read_scene_topology"]),
        ("zone-only-agent", &["publish_zone:subtitle"]),
        ("widget-no-cap-agent", &["create_tiles"]),
        ("asset-no-cap", &["create_tiles"]),
        ("resource-no-cap", &["create_tiles"]),
    ];
    agents
        .into_iter()
        .map(|(id, perms)| {
            let perms = perms.iter().map(|p| p.to_string()).collect();
            (id.to_string(), perms)
        })
        .collect()
}

/// Start a test server and return a connected client.
async fn setup_test() -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let scene = SceneGraph::new(800.0, 600.0);
    let service =
        HudSessionImpl::new(scene, "test-key").with_agent_permissions(restricted_test_agents());

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    let client = connect_test_client_with_retry(addr.port()).await;

    (client, handle)
}

/// Start a connected test server whose scene clock can deterministically cross
/// a finite lease TTL. The caller receives the same durable runtime-to-session
/// publisher that the windowed compositor owns in production.
async fn setup_test_with_lease_expiry_clock(
    clock: tze_hud_scene::TestClock,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    Arc<tokio::sync::Mutex<crate::session::SharedState>>,
    LeaseExpirySender,
) {
    let scene = SceneGraph::new_with_clock(800.0, 600.0, Arc::new(clock));
    let service = HudSessionImpl::new(scene, "test-key");
    let state = Arc::clone(&service.state);
    let lease_expirations = service.lease_expirations.clone();

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    let client = connect_test_client_with_retry(addr.port()).await;
    (client, handle, state, lease_expirations)
}

fn direct_handler_test_session(namespace: &str, capabilities: Vec<String>) -> StreamSession {
    StreamSession {
        session_id: format!("{namespace}-direct-handler"),
        namespace: namespace.to_string(),
        agent_name: namespace.to_string(),
        capabilities,
        lease_ids: Vec::new(),
        scene_session_id: SceneId::new(),
        resource_budget: ResourceBudget::default(),
        budget_enforcer: None,
        subscriptions: Vec::new(),
        subscription_filters: std::collections::HashMap::new(),
        server_sequence: 0,
        resume_token: Vec::new(),
        last_heartbeat_ms: 0,
        state: SessionState::Active,
        last_client_sequence: 1,
        safe_mode_active: false,
        freeze_queue: SessionFreezeQueue::new(FREEZE_QUEUE_CAPACITY),
        session_open_at_wall_us: now_wall_us(),
        dedup_window: DedupWindow::new(1000, 60),
        lease_correlation_cache: LeaseCorrelationCache::new(
            DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY,
        ),
        resource_upload_rate_limiter: UploadByteRateLimiter::with_limit(
            tze_hud_resource::DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC,
        ),
    }
}

/// A finite lease TTL is reaped by the scene, then the exact terminal result is
/// delivered once to the still-connected owning agent. This exercises the
/// production boundary end to end: `expire_leases()` cleanup → durable runtime
/// bridge → session stream `LeaseResponse(result=EXPIRED)` + state transition.
#[tokio::test]
async fn short_ttl_expiry_reaches_owning_connected_agent_once_after_cleanup() {
    use std::time::Duration;

    let clock = tze_hud_scene::TestClock::new(1_000);
    let (mut client, server, state, lease_expirations) =
        setup_test_with_lease_expiry_clock(clock.clone()).await;
    let (tx, _handshake, mut stream) = handshake(&mut client, "expiry-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 1,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let granted = next_server_msg(&mut stream).await;
    let (lease_id, tile_id) = match granted.payload {
        Some(ServerPayload::RequestResult(RequestResult {
            ok: true,
            lease_id,
            ids,
            ..
        })) => (
            bytes_to_scene_id(&lease_id).expect("lease grant must carry a SceneId"),
            bytes_to_scene_id(&ids[0]).expect("claim must return the tile id"),
        ),
        other => panic!("expected a granted ClaimTile, got {other:?}"),
    };

    let expiry = {
        let shared = state.lock().await;
        let mut scene = shared.scene.lock().await;
        assert_eq!(
            scene.tile_count(),
            1,
            "lease owns one live resource before TTL"
        );

        clock.advance(2);
        let mut expiries = scene.expire_leases();
        assert_eq!(
            expiries.len(),
            1,
            "short TTL must produce one terminal result"
        );
        let expiry = expiries.pop().unwrap();
        assert_eq!(expiry.lease_id, lease_id);
        assert_eq!(expiry.previous_state, LeaseState::Active);
        assert_eq!(expiry.terminal_state, LeaseState::Expired);
        assert_eq!(expiry.removed_tiles, vec![tile_id]);
        assert_eq!(
            scene.tile_count(),
            0,
            "terminal expiry clears the owned tile"
        );
        expiry
    };

    assert_eq!(
        lease_expirations.publish(expiry.clone().into()),
        1,
        "the connected session handler must receive the terminal result"
    );
    assert_eq!(
        lease_expirations.publish(expiry.into()),
        1,
        "the durable fan-out may receive a duplicate runtime notice"
    );

    let terminal_response = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("terminal LeaseResponse must arrive promptly")
        .expect("connected stream must remain open")
        .expect("terminal LeaseResponse must be valid");
    match terminal_response.payload {
        Some(ServerPayload::Reclaimed(Reclaimed {
            lease_id: response_lease_id,
            why,
            ..
        })) => {
            assert_eq!(response_lease_id, scene_id_to_bytes(lease_id));
            assert_eq!(why, ReclaimReason::Expired as i32);
        }
        other => panic!("expected Reclaimed, got {other:?}"),
    }

    assert!(
        tokio::time::timeout(Duration::from_millis(50), stream.next())
            .await
            .is_err(),
        "one terminal lease must emit exactly one response even if its runtime notice is duplicated"
    );

    drop(tx);
    server.abort();
}

#[tokio::test]
async fn successful_claim_tile_wakes_before_capacity_one_response_send() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let service = HudSessionImpl::new(SceneGraph::new(800.0, 600.0), "test-key");
    let state = Arc::clone(&service.state);
    let mut session =
        direct_handler_test_session("lease-wake-ordering", vec!["create_tiles".to_string()]);
    let (outbound_tx, mut outbound_rx) =
        tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(1);
    outbound_tx
        .send(Ok(ServerMessage::default()))
        .await
        .expect("fill the sole outbound response slot");

    let wakes = Arc::new(AtomicU64::new(0));
    let callback_wakes = Arc::clone(&wakes);
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_wakes.fetch_add(1, Ordering::AcqRel);
    });
    let grant = super::verbs::handle_claim_tile(
        &state,
        &mut session,
        &outbound_tx,
        2,
        ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        },
        &render_wake,
    );
    tokio::pin!(grant);

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut grant)
            .await
            .is_err(),
        "the full capacity-one response channel must block the successful LeaseResponse"
    );
    assert_eq!(
        wakes.load(Ordering::Acquire),
        1,
        "the granted lease must publish its render wake before its blocked response send"
    );

    let _blocker = outbound_rx
        .recv()
        .await
        .expect("the prefilled response slot remains readable");
    let response = tokio::select! {
        response = outbound_rx.recv() => {
            let response = response
                .expect("LeaseResponse sender remains connected")
                .expect("LeaseResponse must be Ok");
            (&mut grant).await;
            response
        }
        () = &mut grant => {
            outbound_rx
                .recv()
                .await
                .expect("LeaseResponse must be enqueued before handler completion")
                .expect("LeaseResponse must be Ok")
        }
    };
    assert!(matches!(
        response.payload,
        Some(ServerPayload::RequestResult(RequestResult { ok: true, .. }))
    ));
}

#[tokio::test]
async fn successful_mutation_apply_wakes_before_capacity_one_response_send() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let namespace = "mutation-wake-ordering";
    let mut scene = SceneGraph::new(800.0, 600.0);
    let tab_id = scene.create_tab("Main", 0).expect("create active tab");
    let lease_id = scene.grant_lease(namespace, 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            namespace,
            lease_id,
            tze_hud_scene::Rect::new(10.0, 20.0, 200.0, 150.0),
            1,
        )
        .expect("create tile");
    let service = HudSessionImpl::new(scene, "test-key");
    let state = Arc::clone(&service.state);
    let mut session = direct_handler_test_session(namespace, vec!["modify_own_tiles".to_string()]);
    session.lease_ids.push(lease_id);
    let (outbound_tx, mut outbound_rx) =
        tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(1);
    outbound_tx
        .send(Ok(ServerMessage::default()))
        .await
        .expect("fill the sole outbound response slot");

    let wakes = Arc::new(AtomicU64::new(0));
    let callback_wakes = Arc::clone(&wakes);
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_wakes.fetch_add(1, Ordering::AcqRel);
    });
    let batch = MutationBatch {
        batch_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
        lease_id: scene_id_to_bytes(lease_id),
        mutations: vec![crate::proto::MutationProto {
            mutation: Some(crate::proto::mutation_proto::Mutation::UpdateTileOpacity(
                crate::proto::UpdateTileOpacityMutation {
                    tile_id: scene_id_to_bytes(tile_id),
                    opacity: 0.5,
                },
            )),
        }],
        timing: None,
    };
    let apply = handle_mutation_batch(&state, &mut session, &outbound_tx, 0, batch, &render_wake);
    tokio::pin!(apply);

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut apply)
            .await
            .is_err(),
        "the full capacity-one response channel must block MutationResult after apply"
    );
    assert_eq!(
        wakes.load(Ordering::Acquire),
        1,
        "the applied mutation must publish its render wake before its blocked result send"
    );
    {
        let shared = state.lock().await;
        let scene = shared.scene.lock().await;
        assert_eq!(
            scene.tiles[&tile_id].opacity, 0.5,
            "the scene mutation must already be visible while its result remains blocked"
        );
    }

    let _blocker = outbound_rx
        .recv()
        .await
        .expect("the prefilled response slot remains readable");
    (&mut apply).await;
    let response = outbound_rx
        .recv()
        .await
        .expect("MutationResult must be sent")
        .expect("MutationResult must be Ok");
    assert!(matches!(
        response.payload,
        Some(ServerPayload::RequestResult(RequestResult { ok: true, .. }))
    ));
}

async fn setup_test_with_input_capture_channel(
    input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::UnboundedReceiver<crate::session::InputCaptureCommand>,
    tze_hud_scene::SceneId,
    tze_hud_scene::SceneId,
) {
    let mut scene = SceneGraph::new(800.0, 600.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("capture-agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "capture-agent",
            lease_id,
            tze_hud_scene::Rect::new(10.0, 10.0, 100.0, 50.0),
            1,
        )
        .unwrap();
    let node_id = tze_hud_scene::SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            tze_hud_scene::Node {
                layout: Default::default(),
                id: node_id,
                data: tze_hud_scene::NodeData::HitRegion(tze_hud_scene::HitRegionNode {
                    bounds: tze_hud_scene::Rect::new(0.0, 0.0, 100.0, 50.0),
                    interaction_id: "capture-target".to_string(),
                    accepts_pointer: true,
                    auto_capture: true,
                    release_on_up: true,
                    ..Default::default()
                }),
                children: Vec::new(),
            },
        )
        .unwrap();
    let service = HudSessionImpl::new(scene, "test-key");
    let (capture_tx, capture_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let mut st = service.state.lock().await;
        st.input_capture_tx = Some(capture_tx);
        st.input_capture_wake = input_capture_wake;
    }

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    let client = connect_test_client_with_retry(addr.port()).await;

    (client, handle, capture_rx, tile_id, node_id)
}

#[test]
fn test_scene_node_contains_handles_deep_hierarchy_iteratively() {
    let mut scene = SceneGraph::new(800.0, 600.0);
    let root_id = tze_hud_scene::SceneId::new();
    scene.nodes.insert(
        root_id,
        tze_hud_scene::Node {
            layout: Default::default(),
            id: root_id,
            data: tze_hud_scene::NodeData::SolidColor(tze_hud_scene::SolidColorNode {
                bounds: tze_hud_scene::Rect::new(0.0, 0.0, 1.0, 1.0),
                color: tze_hud_scene::Rgba::WHITE,
                radius: None,
            }),
            children: Vec::new(),
        },
    );

    let mut parent_id = root_id;
    for _ in 0..2_048 {
        let child_id = tze_hud_scene::SceneId::new();
        scene.nodes.insert(
            child_id,
            tze_hud_scene::Node {
                layout: Default::default(),
                id: child_id,
                data: tze_hud_scene::NodeData::SolidColor(tze_hud_scene::SolidColorNode {
                    bounds: tze_hud_scene::Rect::new(0.0, 0.0, 1.0, 1.0),
                    color: tze_hud_scene::Rgba::WHITE,
                    radius: None,
                }),
                children: Vec::new(),
            },
        );
        scene
            .nodes
            .get_mut(&parent_id)
            .unwrap()
            .children
            .push(child_id);
        parent_id = child_id;
    }

    assert!(
        scene_node_contains(&scene, root_id, parent_id),
        "deep descendant should be found without recursive traversal"
    );
    assert!(
        !scene_node_contains(&scene, root_id, tze_hud_scene::SceneId::new()),
        "unrelated node should not be reported as contained"
    );
}

async fn connect_test_client_with_retry(port: u16) -> HudSessionClient<tonic::transport::Channel> {
    let endpoint = format!("http://[::1]:{port}");
    for attempt in 0..25 {
        if let Ok(client) = HudSessionClient::connect(endpoint.clone()).await {
            return client;
        }
        if attempt < 24 {
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }
    }
    panic!("failed to connect test client to {endpoint} after retries");
}

/// Helper: create a bidirectional stream and perform handshake.
/// Returns the sender and the three ordered handshake messages:
/// SessionEstablished, SceneSnapshot, and current DegradationNotice.
async fn handshake(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
    psk: &str,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    Vec<ServerMessage>,
    tonic::Streaming<ServerMessage>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    // Send SessionInit with canonical capability names (create_tiles, access_input_events)
    // and read_scene_topology so SCENE_TOPOLOGY subscription is granted.
    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: agent_id.to_string(),
            initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential(psk.to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();

    // Collect SessionEstablished, SceneSnapshot, and current degradation state.
    let mut messages = Vec::new();
    for _ in 0..3 {
        if let Some(msg) = response_stream.next().await {
            messages.push(msg.unwrap());
        }
    }

    (tx, messages, response_stream)
}

/// Helper: perform SessionInit with an explicit requested capability list.
async fn handshake_with_psk(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
    psk: &str,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    Vec<ServerMessage>,
    tonic::Streaming<ServerMessage>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: agent_id.to_string(),
            initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential(psk.to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let mut messages = Vec::new();
    for _ in 0..3 {
        if let Some(msg) = response_stream.next().await {
            messages.push(msg.unwrap());
        }
    }
    (tx, messages, response_stream)
}

#[tokio::test]
async fn test_handshake_init_established_and_snapshot() {
    let (mut client, _server) = setup_test().await;
    let (_tx, messages, _stream) = handshake(&mut client, "test-agent", "test-key").await;

    assert_eq!(messages.len(), 3);

    // First message: SessionEstablished
    match &messages[0].payload {
        Some(ServerPayload::SessionEstablished(established)) => {
            assert!(!established.session_id.is_empty());
            assert_eq!(established.namespace, "test-agent");
            assert!(!established.resume_token.is_empty());
            assert_eq!(
                established.heartbeat_interval_ms,
                DEFAULT_HEARTBEAT_INTERVAL_MS
            );
            // SCENE_TOPOLOGY is granted because agent has read_scene_topology capability
            assert!(
                established
                    .active_subscriptions
                    .contains(&"SCENE_TOPOLOGY".to_string()),
                "SCENE_TOPOLOGY should be active (agent has read_scene_topology)"
            );
            // Mandatory subscriptions always present
            assert!(
                established
                    .active_subscriptions
                    .contains(&"DEGRADATION_NOTICES".to_string()),
                "DEGRADATION_NOTICES must always be active"
            );
            assert!(
                established
                    .active_subscriptions
                    .contains(&"LEASE_CHANGES".to_string()),
                "LEASE_CHANGES must always be active"
            );
            // denied_subscriptions must be empty (all requested categories granted)
            assert!(
                established.denied_subscriptions.is_empty(),
                "no subscriptions should be denied"
            );
        }
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    }

    // Second message: SceneSnapshot
    match &messages[1].payload {
        Some(ServerPayload::SceneSnapshot(snapshot)) => {
            assert!(!snapshot.snapshot_json.is_empty());
        }
        other => panic!("Expected SceneSnapshot, got: {other:?}"),
    }

    match &messages[2].payload {
        Some(ServerPayload::DegradationNotice(notice)) => {
            assert_eq!(notice.level, DegradationLevel::Normal as i32);
        }
        other => panic!("Expected current DegradationNotice, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_handshake_auth_failure() {
    let (mut client, _server) = setup_test().await;

    let (_tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let (init_tx, init_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(init_rx);

    // Send SessionInit with wrong key
    init_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionInit(SessionInit {
                agent_id: "bad-agent".to_string(),
                initial_subscriptions: Vec::new(),
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(crate::auth::psk_credential("wrong-key".to_string())),
            })),
        })
        .await
        .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();

    match &msg.payload {
        Some(ServerPayload::SessionError(error)) => {
            assert_eq!(error.code, "AUTH_FAILED");
        }
        other => panic!("Expected SessionError, got: {other:?}"),
    }

    drop(_tx);
    drop(rx);
}

#[tokio::test]
async fn test_mutation_over_stream() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "mutator", "test-key").await;

    // First, request a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let lease_msg = stream.next().await.unwrap().unwrap();
    let lease_id = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Create a tab in the scene (needed for mutations)
    // We need to do this through shared state since tab creation
    // isn't exposed via the streaming protocol yet.
    // For the test, we'll send a mutation that doesn't require a tab.

    // Send a mutation batch
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: vec![crate::proto::MutationProto {
                mutation: Some(crate::proto::mutation_proto::Mutation::CreateTile(
                    crate::proto::CreateTileMutation {
                        tab_id: vec![], // empty = server infers active tab
                        bounds: Some(crate::proto::Rect {
                            x: 0.0,
                            y: 0.0,
                            width: 200.0,
                            height: 150.0,
                        }),
                        z_order: 1,
                    },
                )),
            }],
            timing: None,
        })),
    })
    .await
    .unwrap();

    let result_msg = stream.next().await.unwrap().unwrap();
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            // This will fail because no active tab exists, which is expected
            // in this isolated test. The important thing is that the protocol
            // round-trip works.
            assert_eq!(result.batch_id, batch_id);
            // accepted may be false due to "no active tab" -- that's fine
        }
        other => panic!("Expected MutationResult, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_create_tile_persists_element_store_entry() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let path = std::env::temp_dir().join(format!(
        "tze_hud_element_store_session_server_{}.toml",
        SceneId::new()
    ));
    let _ = std::fs::remove_file(&path);

    {
        let mut st = shared_state.lock().await;
        st.element_store = tze_hud_scene::element_store::ElementStore::default();
        st.element_store_path = Some(path.clone());
        st.scene
            .lock()
            .await
            .create_tab("main", 0)
            .expect("create tab");
    }

    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "persist-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .expect("lease request");

    let claim_msg = next_server_msg(&mut stream).await;
    let created_tile_id = match &claim_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(result.ok, "ClaimTile must be accepted");
            bytes_to_scene_id(&result.ids[0]).expect("valid created tile id bytes")
        }
        other => panic!("Expected RequestResult, got: {other:?}"),
    };

    let store = load_element_store_for_test(&path).expect("load persisted element store");
    let entry = store
        .entries
        .get(&created_tile_id)
        .expect("tile id should be persisted");
    assert_eq!(
        entry.element_type,
        tze_hud_scene::element_store::ElementType::Tile
    );
    assert_eq!(entry.namespace, "persist-agent");
    assert!(entry.created_at > 0);
    assert!(entry.last_published_at >= entry.created_at);

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn test_existing_tile_last_published_update_triggers_persist() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let path = std::env::temp_dir().join(format!(
        "tze_hud_element_store_last_published_{}.toml",
        SceneId::new()
    ));
    let _ = std::fs::remove_file(&path);

    {
        let mut st = shared_state.lock().await;
        st.element_store = tze_hud_scene::element_store::ElementStore::default();
        st.element_store_path = Some(path.clone());
        st.scene
            .lock()
            .await
            .create_tab("main", 0)
            .expect("create tab");
    }

    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "persist-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .expect("lease request");

    let claim_msg = next_server_msg(&mut stream).await;
    let created_tile_id = match &claim_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(result.ok, "ClaimTile must be accepted");
            bytes_to_scene_id(&result.ids[0]).expect("valid created tile id bytes")
        }
        other => panic!("Expected RequestResult, got: {other:?}"),
    };

    let baseline_store = load_element_store_for_test(&path).expect("load baseline element store");
    let baseline_entry = baseline_store
        .entries
        .get(&created_tile_id)
        .expect("baseline tile id should be persisted");
    let baseline_last_published = baseline_entry.last_published_at;

    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    let persist_request = {
        let mut st = shared_state.lock().await;
        let entry = st
            .element_store
            .entries
            .get_mut(&created_tile_id)
            .expect("in-memory tile entry should exist");
        entry.last_published_at = baseline_last_published;
        persist_created_tile_entries(&mut st, &[created_tile_id]).await
    };
    persist_element_store(persist_request).await;

    let updated_store = load_element_store_for_test(&path).expect("reload element store");
    let updated_entry = updated_store
        .entries
        .get(&created_tile_id)
        .expect("tile id should remain persisted");
    assert!(
        updated_entry.last_published_at > baseline_last_published,
        "last_published_at update must be persisted when it is the only changed field"
    );

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn test_recreated_portal_tile_adopts_orphaned_durable_override() {
    // hud-08nls: after a runtime restart, an adapter republishes a portal and
    // the runtime creates its member tile with a FRESH SceneId. The durable
    // per-member override (hud-8vejp) loaded from disk is keyed by the DEAD id.
    // persist_created_tile_entries must re-home that override onto the recreated
    // tile (not orphan it) and re-lock its viewer geometry, so the portal keeps
    // its resized/moved geometry across restart even though the tile id changed.
    let (_client, _server, shared_state) = setup_test_with_state().await;
    let path = std::env::temp_dir().join(format!(
        "tze_hud_element_store_reconcile_{}.toml",
        SceneId::new()
    ));
    let _ = std::fs::remove_file(&path);

    let override_policy = GeometryPolicy::Relative {
        x_pct: 0.25,
        y_pct: 0.6,
        width_pct: 0.4,
        height_pct: 0.3,
    };
    let dead_id = SceneId::new();
    let member_z_order = 7u32;

    let recreated_id;
    {
        let mut st = shared_state.lock().await;
        st.element_store = tze_hud_scene::element_store::ElementStore::default();
        st.element_store_path = Some(path.clone());

        // The store as loaded from disk after restart: an orphaned Tile entry
        // carrying the durable override, keyed by the pre-restart (now dead) id.
        st.element_store.entries.insert(
            dead_id,
            tze_hud_scene::element_store::ElementStoreEntry {
                element_type: tze_hud_scene::element_store::ElementType::Tile,
                namespace: "agent.portal".to_string(),
                created_at: 100,
                last_published_at: 100,
                z_order: member_z_order,
                unseen_restarts: 0,
                geometry_override: Some(override_policy),
            },
        );

        // The adapter republish recreates the member tile with a fresh id.
        let mut scene = st.scene.lock().await;
        let tab_id = scene.create_tab("main", 0).expect("create tab");
        let lease = scene.grant_lease("agent.portal", 60_000);
        recreated_id = scene
            .create_tile(
                tab_id,
                "agent.portal",
                lease,
                Rect::new(0.0, 0.0, 100.0, 80.0),
                member_z_order,
            )
            .expect("create recreated portal tile");
        assert_ne!(recreated_id, dead_id, "the recreated tile has a fresh id");
    }

    let persist_request = {
        let mut st = shared_state.lock().await;
        persist_created_tile_entries(&mut st, &[recreated_id]).await
    };
    persist_element_store(persist_request).await;

    {
        let st = shared_state.lock().await;
        let entry = st
            .element_store
            .entries
            .get(&recreated_id)
            .expect("recreated tile has an element-store entry");
        assert_eq!(
            entry.geometry_override,
            Some(override_policy),
            "durable override is re-applied to the recreated tile, not orphaned"
        );
        assert!(
            !st.element_store.entries.contains_key(&dead_id),
            "the orphaned dead-id entry is consumed"
        );
        assert!(
            st.scene
                .lock()
                .await
                .is_viewer_geometry_locked(recreated_id),
            "viewer geometry is re-locked so an adapter republish cannot reposition it"
        );
    }

    // The re-homed override also survives to disk.
    let persisted = load_element_store_for_test(&path).expect("reload persisted store");
    assert_eq!(
        persisted
            .entries
            .get(&recreated_id)
            .and_then(|e| e.geometry_override),
        Some(override_policy),
        "the reconciled override is persisted under the new id"
    );
    assert!(!persisted.entries.contains_key(&dead_id));

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn test_list_elements_request_supports_filters_and_override_metadata() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let tile_id: SceneId;
    let zone_id = SceneId::new();
    let widget_id = SceneId::new();

    {
        let mut st = shared_state.lock().await;
        st.element_store = tze_hud_scene::element_store::ElementStore::default();
        let mut scene = st.scene.lock().await;
        let tab_id = scene.create_tab("main", 0).expect("create tab");

        let bootstrap_lease = scene.grant_lease("agent-list", 60_000);

        tile_id = scene
            .create_tile(
                tab_id,
                "agent-list",
                bootstrap_lease,
                Rect::new(40.0, 30.0, 160.0, 120.0),
                1,
            )
            .expect("create tile");

        scene.zone_registry.register(ZoneDefinition {
            id: zone_id,
            name: "list-zone".to_string(),
            description: "ListElements test zone".to_string(),
            geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 1.0,
                height_pct: 0.1,
            },
            accepted_media_types: vec![ZoneMediaType::StreamText],
            rendering_policy: RenderingPolicy::default(),
            contention_policy: ContentionPolicy::LatestWins,
            max_publishers: 2,
            auto_clear_ms: None,
            ephemeral: false,
            layer_attachment: LayerAttachment::Content,
        });

        scene
            .publish_to_zone_with_lease(
                "list-zone",
                ZoneContent::StreamText("hello".to_string()),
                "agent-list",
                bootstrap_lease,
                None,
                None,
            )
            .expect("publish zone");

        scene.widget_registry.register_definition(WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "Gauge widget".to_string(),
            parameter_schema: vec![WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            }],
            layers: vec![],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.2,
                y_pct: 0.2,
                width_pct: 0.2,
                height_pct: 0.1,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false,
            hover_behavior: None,
        });
        scene.widget_registry.register_instance(WidgetInstance {
            id: widget_id,
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "gauge-main".to_string(),
            current_params: HashMap::new(),
        });
        scene
            .publish_to_widget(
                "gauge-main",
                HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.5))]),
                "agent-list",
                None,
                0,
                None,
            )
            .expect("publish widget");
        drop(scene);

        st.element_store.entries.insert(
            tile_id,
            tze_hud_scene::element_store::ElementStoreEntry {
                element_type: tze_hud_scene::element_store::ElementType::Tile,
                namespace: "agent-list".to_string(),
                created_at: 101,
                last_published_at: 202,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: Some(GeometryPolicy::Relative {
                    x_pct: 0.25,
                    y_pct: 0.1,
                    width_pct: 0.2,
                    height_pct: 0.2,
                }),
            },
        );
        st.element_store.entries.insert(
            zone_id,
            tze_hud_scene::element_store::ElementStoreEntry {
                element_type: tze_hud_scene::element_store::ElementType::Zone,
                namespace: "list-zone".to_string(),
                created_at: 303,
                last_published_at: 404,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: None,
            },
        );
        st.element_store.entries.insert(
            widget_id,
            tze_hud_scene::element_store::ElementStoreEntry {
                element_type: tze_hud_scene::element_store::ElementType::Widget,
                namespace: "gauge-main".to_string(),
                created_at: 505,
                last_published_at: 606,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: None,
            },
        );
    }

    let (tx, _init_messages, mut stream) = handshake(&mut client, "agent-list", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ListElementsRequest(
            crate::proto::ListElementsRequest {
                namespace_filter: Some("agent-".to_string()),
                element_type: Some("tile".to_string()),
            },
        )),
    })
    .await
    .expect("send list-elements request");

    let tile_only = next_server_msg(&mut stream).await;
    match tile_only.payload {
        Some(ServerPayload::ListElementsResponse(resp)) => {
            assert_eq!(
                resp.elements.len(),
                1,
                "tile filter should return one element"
            );
            let entry = &resp.elements[0];
            assert_eq!(entry.element_type, "tile");
            assert_eq!(entry.namespace, "agent-list");
            assert_eq!(
                bytes_to_scene_id(&entry.element_id).expect("tile entry id must decode"),
                tile_id
            );
            assert!(entry.has_user_override, "tile should report user override");
            assert_eq!(entry.created_at_ms, 101);
            assert_eq!(entry.last_published_at_ms, 202);
            match entry
                .current_geometry
                .as_ref()
                .and_then(|g| g.policy.as_ref())
            {
                Some(crate::proto::geometry_policy_proto::Policy::Relative(relative)) => {
                    assert!((relative.x_pct - 0.25).abs() < 1e-6);
                    assert!((relative.y_pct - 0.1).abs() < 1e-6);
                    assert!((relative.width_pct - 0.2).abs() < 1e-6);
                    assert!((relative.height_pct - 0.2).abs() < 1e-6);
                }
                other => panic!("expected relative geometry policy, got {other:?}"),
            }
        }
        other => panic!("Expected ListElementsResponse, got: {other:?}"),
    }

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ListElementsRequest(
            crate::proto::ListElementsRequest {
                namespace_filter: Some("list-".to_string()),
                element_type: Some("zone".to_string()),
            },
        )),
    })
    .await
    .expect("send list-elements zone filter request");

    let zone_only = next_server_msg(&mut stream).await;
    match zone_only.payload {
        Some(ServerPayload::ListElementsResponse(resp)) => {
            assert_eq!(
                resp.elements.len(),
                1,
                "zone filter should return one element"
            );
            assert_eq!(resp.elements[0].element_type, "zone");
            assert_eq!(resp.elements[0].namespace, "list-zone");
            assert_eq!(
                bytes_to_scene_id(&resp.elements[0].element_id).expect("zone entry id must decode"),
                zone_id
            );
        }
        other => panic!("Expected ListElementsResponse, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_mutation_result_echoes_client_batch_id() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "batch-id-regression", "test-key").await;

    // Acquire a lease so the batch reaches the batch_id mapping code
    // (lease validation runs first; an invalid lease returns early before
    // the batch_id mapping happens).
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let lease_msg = next_server_msg(&mut stream).await;
    let lease_id = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Send a mutation batch with a known, unique batch_id.
    let client_batch_id: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: client_batch_id.clone(),
            lease_id,
            mutations: vec![crate::proto::MutationProto {
                mutation: Some(crate::proto::mutation_proto::Mutation::CreateTile(
                    crate::proto::CreateTileMutation {
                        tab_id: vec![],
                        bounds: Some(crate::proto::Rect {
                            x: 0.0,
                            y: 0.0,
                            width: 100.0,
                            height: 100.0,
                        }),
                        z_order: 0,
                    },
                )),
            }],
            timing: None,
        })),
    })
    .await
    .unwrap();

    // The batch will be rejected (no active tab in setup_test).
    // Regardless of rejection, MutationResult.batch_id MUST equal client_batch_id.
    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert_eq!(
                result.batch_id, client_batch_id,
                "MutationResult.batch_id must echo the client-provided batch_id \
                     (regression for hud-wu32: batch_id was previously a fresh SceneId)"
            );
        }
        other => panic!("Expected MutationResult, got: {other:?}"),
    }
}

/// Regression: lease_id MUST be propagated into SceneMutationBatch so that
/// the five-stage validation pipeline (including lease/budget checks) fires.
///
/// Before this fix, `lease_id: None` was passed, which meant lease and budget
/// validation was skipped for non-CreateTile mutations in the gRPC path.
///
/// This test verifies that a mutation using an expired lease is rejected with
/// an error indicating lease/budget validation ran — not silently accepted.
#[tokio::test]
async fn test_mutation_rejected_with_expired_lease_id() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "lease-validation-regression", "test-key").await;

    // Create an active tab so mutations can reach the scene-apply path.
    {
        let st = shared_state.lock().await;
        st.scene
            .lock()
            .await
            .create_tab("test-tab", 0)
            .expect("create_tab");
    }

    // Acquire a lease.
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let lease_msg = next_server_msg(&mut stream).await;
    let lease_id_bytes = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Revoke the lease directly in shared state, simulating an expired lease.
    // The wire format encodes SceneId as uuid::Uuid::as_bytes() (big-endian UUID bytes),
    // matching bytes_to_scene_id in session_server.rs.
    {
        let st = shared_state.lock().await;
        let arr: [u8; 16] = lease_id_bytes
            .as_slice()
            .try_into()
            .expect("16-byte lease_id");
        let lease_id = tze_hud_scene::SceneId::from_uuid(uuid::Uuid::from_bytes(arr));
        let _ = st.scene.lock().await.revoke_lease(lease_id);
    }

    // Send a CreateTile mutation referencing the now-revoked lease.
    let batch_id: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id_bytes,
            mutations: vec![crate::proto::MutationProto {
                mutation: Some(crate::proto::mutation_proto::Mutation::CreateTile(
                    crate::proto::CreateTileMutation {
                        tab_id: vec![],
                        bounds: Some(crate::proto::Rect {
                            x: 0.0,
                            y: 0.0,
                            width: 100.0,
                            height: 100.0,
                        }),
                        z_order: 0,
                    },
                )),
            }],
            timing: None,
        })),
    })
    .await
    .unwrap();

    // The batch MUST be rejected (lease is revoked; validation pipeline runs).
    // batch_id must still be echoed back.
    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(
                !result.ok,
                "Mutation with revoked lease_id must be rejected \
                     (regression for hud-wu32: lease_id=None previously bypassed validation)"
            );
            assert_eq!(
                result.batch_id, batch_id,
                "MutationResult.batch_id must echo client batch_id even on rejection"
            );
        }
        other => panic!("Expected MutationResult, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_lease_over_stream() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "leasor", "test-key").await;

    // Request a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 30_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::RequestResult(resp)) => {
            assert!(resp.ok, "expected lease to be granted");
            assert!(!resp.lease_id.is_empty());
            assert_eq!(resp.lease_id.len(), 16);
            assert_eq!(resp.ttl_ms, 30_000);
        }
        other => panic!("Expected LeaseResponse, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_heartbeat_echo() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "heartbeater", "test-key").await;

    let mono_us = 12345678u64;
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: mono_us,
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::Heartbeat(hb)) => {
            assert_eq!(hb.timestamp_mono_us, mono_us);
        }
        other => panic!("Expected Heartbeat echo, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_resume_with_token() {
    let (mut client, _server) = setup_test().await;

    // Start initial session to get a resume token
    let (tx, init_messages, _stream) = handshake(&mut client, "resumable", "test-key").await;
    drop(tx); // Close the first stream
    drop(_stream);

    // Wait a bit for cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Now resume with the token
    let resume_token = match &init_messages[0].payload {
        Some(ServerPayload::SessionEstablished(established)) => established.resume_token.clone(),
        _ => panic!("Expected SessionEstablished"),
    };

    let (resume_tx, resume_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let resume_stream = tokio_stream::wrappers::ReceiverStream::new(resume_rx);

    resume_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionResume(SessionResume {
                agent_id: "resumable".to_string(),
                resume_token,
                last_seen_server_sequence: 2,
                pre_shared_key: "test-key".to_string(),
                auth_credential: None,
            })),
        })
        .await
        .unwrap();

    let mut response_stream = client.session(resume_stream).await.unwrap().into_inner();

    // Resume ordering is result, coherent snapshot, then current degradation
    // state before any live transition.
    let msg1 = response_stream.next().await.unwrap().unwrap();
    match &msg1.payload {
        Some(ServerPayload::SessionResumeResult(result)) => {
            assert!(result.accepted, "expected resume to be accepted");
            assert!(!result.new_session_token.is_empty());
            // version = major * 1000 + minor; runtime max = v1.1 = 1001
            assert_eq!(
                result.negotiated_protocol_version,
                crate::auth::RUNTIME_MAX_VERSION
            );
        }
        other => panic!("Expected SessionResumeResult on resume, got: {other:?}"),
    }

    let msg2 = response_stream.next().await.unwrap().unwrap();
    match &msg2.payload {
        Some(ServerPayload::SceneSnapshot(_)) => {}
        other => panic!("Expected SceneSnapshot on resume, got: {other:?}"),
    }

    let msg3 = response_stream.next().await.unwrap().unwrap();
    match &msg3.payload {
        Some(ServerPayload::DegradationNotice(notice)) => {
            assert_eq!(notice.level, DegradationLevel::Normal as i32);
        }
        other => panic!("Expected current DegradationNotice on resume, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_zone_publish_result() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "zone-publisher", "test-key").await;

    // Send a ZonePublish — expect ZonePublishResult correlated by client sequence
    let client_seq: u64 = 2;
    tx.send(ClientMessage {
        sequence: client_seq,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "zone:status".to_string(),
            content: Some(crate::proto::ZoneContent {
                payload: Some(crate::proto::zone_content::Payload::StreamText(
                    "hello zone".to_string(),
                )),
            }),
            ttl_ms: 0,
            key: String::new(),
            breakpoints: Vec::new(),
            present_at_us: 0,
            expires_at_us: 0,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            // request_sequence must echo the client envelope sequence
            assert_eq!(
                result.seq, client_seq,
                "ZonePublishResult.request_sequence must correlate with client ZonePublish sequence"
            );
            // Zone "status" doesn't exist in the default scene graph so it
            // will be rejected; we just verify the sequence correlation and
            // that error_code is populated on rejection.
            if !result.ok {
                assert!(
                    !result.code.is_empty(),
                    "rejected result must carry an error_code"
                );
            }
        }
        other => panic!("Expected ZonePublishResult, got: {other:?}"),
    }
}

/// Scenario: Add subscription mid-session with required capability (RFC 0005 §7.3).
/// Also validates subscription denied for missing capability.
#[tokio::test]
async fn test_subscription_change_result() {
    let (mut client, _server) = setup_test().await;

    // Use a custom handshake with access_input_events to test SubscriptionChange
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "subscriber".to_string(),
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
            initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
            resume_token: Vec::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();

    // Collect SessionEstablished, SceneSnapshot, and current degradation state.
    for _ in 0..3 {
        let _ = response_stream.next().await;
    }

    // Send a SubscriptionChange to add INPUT_EVENTS (has access_input_events)
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SubscriptionChange(SubscriptionChange {
            subscribe: vec!["INPUT_EVENTS".to_string()],
            unsubscribe: Vec::new(),
            subscribe_filter: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SubscriptionChangeResult(result)) => {
            // Initial SCENE_TOPOLOGY subscription should still be active
            assert!(
                result
                    .active_subscriptions
                    .contains(&"SCENE_TOPOLOGY".to_string()),
                "initial SCENE_TOPOLOGY subscription should still be active"
            );
            // Newly added INPUT_EVENTS should be active (agent has access_input_events)
            assert!(
                result
                    .active_subscriptions
                    .contains(&"INPUT_EVENTS".to_string()),
                "newly added INPUT_EVENTS subscription should be active"
            );
            // Mandatory subscriptions always present
            assert!(
                result
                    .active_subscriptions
                    .contains(&"DEGRADATION_NOTICES".to_string()),
                "DEGRADATION_NOTICES must always be active"
            );
            assert!(
                result
                    .active_subscriptions
                    .contains(&"LEASE_CHANGES".to_string()),
                "LEASE_CHANGES must always be active"
            );
            // No denied subscriptions (all requested categories have required capability)
            assert!(
                result.denied_subscriptions.is_empty(),
                "no subscriptions should be denied"
            );
        }
        other => panic!("Expected SubscriptionChangeResult, got: {other:?}"),
    }
    drop(tx);
}

/// Scenario: SubscriptionChange.subscribe_filter persists filter_prefix (RFC 0010 §7.2, spec line 179).
///
/// WHEN agent sends SubscriptionChange with subscribe_filter=[{SCENE_TOPOLOGY, "scene.zone."}]
/// THEN runtime accepts the subscription (no denial) and stores the filter_prefix in
///      session.subscription_filters so future event routing can apply the narrower filter.
///
/// Additionally verifies that a subsequent plain `subscribe` for the same category
/// clears the stored filter (resetting to category-default prefix behavior).
#[tokio::test]
async fn test_subscription_change_with_filter_prefix() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "filter-agent".to_string(),
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();

    // Collect SessionEstablished, SceneSnapshot, and current degradation state.
    for _ in 0..3 {
        let _ = response_stream.next().await;
    }

    // Step 1: Send SubscriptionChange with subscribe_filter: add SCENE_TOPOLOGY with "scene.zone." filter
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SubscriptionChange(SubscriptionChange {
            subscribe: Vec::new(),
            unsubscribe: Vec::new(),
            subscribe_filter: vec![crate::proto::session::SubscriptionEntry {
                category: "SCENE_TOPOLOGY".to_string(),
                filter_prefix: "scene.zone.".to_string(),
            }],
        })),
    })
    .await
    .unwrap();

    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SubscriptionChangeResult(result)) => {
            // SCENE_TOPOLOGY must be in the active set (subscribe_filter is processed as an add)
            assert!(
                result
                    .active_subscriptions
                    .contains(&"SCENE_TOPOLOGY".to_string()),
                "SCENE_TOPOLOGY must be active after subscribe_filter"
            );
            // No denials (agent has read_scene_topology capability)
            assert!(
                result.denied_subscriptions.is_empty(),
                "subscribe_filter with a valid capability must not produce denials"
            );
        }
        other => panic!("Expected SubscriptionChangeResult, got: {other:?}"),
    }

    // Step 2: Reset to default by sending a plain `subscribe` for SCENE_TOPOLOGY.
    // The stored filter must be cleared (empty filter_prefix resets to category default).
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SubscriptionChange(SubscriptionChange {
            subscribe: vec!["SCENE_TOPOLOGY".to_string()],
            unsubscribe: Vec::new(),
            subscribe_filter: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let msg2 = response_stream.next().await.unwrap().unwrap();
    match &msg2.payload {
        Some(ServerPayload::SubscriptionChangeResult(result2)) => {
            // SCENE_TOPOLOGY must still be active
            assert!(
                result2
                    .active_subscriptions
                    .contains(&"SCENE_TOPOLOGY".to_string()),
                "SCENE_TOPOLOGY must remain active after plain subscribe"
            );
            // No denials
            assert!(
                result2.denied_subscriptions.is_empty(),
                "plain subscribe for already-held category must not produce denials"
            );
        }
        other => panic!("Expected SubscriptionChangeResult for reset, got: {other:?}"),
    }

    // Step 3: Also verify that subscribe_filter with empty filter_prefix explicitly resets the filter.
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SubscriptionChange(SubscriptionChange {
            subscribe: Vec::new(),
            unsubscribe: Vec::new(),
            subscribe_filter: vec![crate::proto::session::SubscriptionEntry {
                category: "SCENE_TOPOLOGY".to_string(),
                filter_prefix: String::new(), // empty = reset to default
            }],
        })),
    })
    .await
    .unwrap();

    let msg3 = response_stream.next().await.unwrap().unwrap();
    match &msg3.payload {
        Some(ServerPayload::SubscriptionChangeResult(result3)) => {
            assert!(
                result3
                    .active_subscriptions
                    .contains(&"SCENE_TOPOLOGY".to_string()),
                "SCENE_TOPOLOGY must remain active after empty-prefix subscribe_filter"
            );
            assert!(
                result3.denied_subscriptions.is_empty(),
                "empty-prefix subscribe_filter for active category must not produce denials"
            );
        }
        other => {
            panic!("Expected SubscriptionChangeResult for empty-prefix reset, got: {other:?}")
        }
    }

    drop(tx);
}

/// Scenario: Subscription denied when capability is missing (RFC 0005 §7.1, spec lines 455-457).
/// WHEN agent requests INPUT_EVENTS without access_input_events capability
/// THEN subscription is denied and listed in denied_subscriptions.
#[tokio::test]
async fn test_subscription_denied_without_capability() {
    let (mut client, _server) = setup_test().await;

    // Handshake WITHOUT access_input_events capability
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "no-input-agent".to_string(),
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
            // Request INPUT_EVENTS without access_input_events capability
            initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string(), "INPUT_EVENTS".to_string()],
            resume_token: Vec::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();

    // First message: SessionEstablished
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionEstablished(established)) => {
            // INPUT_EVENTS should be in denied_subscriptions
            assert!(
                established
                    .denied_subscriptions
                    .contains(&"INPUT_EVENTS".to_string()),
                "INPUT_EVENTS must be denied without access_input_events capability"
            );
            // INPUT_EVENTS should NOT be in active_subscriptions
            assert!(
                !established
                    .active_subscriptions
                    .contains(&"INPUT_EVENTS".to_string()),
                "INPUT_EVENTS must not be active without access_input_events capability"
            );
            // SCENE_TOPOLOGY is granted (agent has read_scene_topology)
            assert!(
                established
                    .active_subscriptions
                    .contains(&"SCENE_TOPOLOGY".to_string()),
                "SCENE_TOPOLOGY should be active with read_scene_topology capability"
            );
        }
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    }
    drop(tx);
}

// ─── Sequence number validation tests (RFC 0005 §2.3) ────────────────────

/// Scenario: Sequence gap exceeds threshold (RFC 0005 §2.3)
/// WHEN client sends sequence 5 followed by 150 (gap > max_sequence_gap=100),
/// THEN runtime closes the stream with SEQUENCE_GAP_EXCEEDED.
#[tokio::test]
async fn test_sequence_gap_exceeded() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "seq-gap-agent", "test-key").await;

    // Handshake consumes sequence 1. Send a valid message at sequence 2.
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 100,
        })),
    })
    .await
    .unwrap();

    // Drain the heartbeat echo
    let _ = stream.next().await;

    // Now jump to sequence 5, then to 150 — gap of 145 > DEFAULT_MAX_SEQUENCE_GAP=100
    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 200,
        })),
    })
    .await
    .unwrap();
    let _ = stream.next().await; // drain heartbeat echo

    tx.send(ClientMessage {
        sequence: 150,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 300,
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "SEQUENCE_GAP_EXCEEDED",
                "Expected SEQUENCE_GAP_EXCEEDED, got: {}",
                err.code
            );
        }
        other => panic!("Expected SessionError(SEQUENCE_GAP_EXCEEDED), got: {other:?}"),
    }
}

/// Scenario: Sequence regression rejected (RFC 0005 §2.3)
/// WHEN client sends sequence 10 followed by sequence 8,
/// THEN runtime closes the stream with SEQUENCE_REGRESSION.
#[tokio::test]
async fn test_sequence_regression() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "seq-reg-agent", "test-key").await;

    // Send sequence 10
    tx.send(ClientMessage {
        sequence: 10,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 100,
        })),
    })
    .await
    .unwrap();
    let _ = stream.next().await; // drain heartbeat echo

    // Send sequence 8 — regression
    tx.send(ClientMessage {
        sequence: 8,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 200,
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "SEQUENCE_REGRESSION",
                "Expected SEQUENCE_REGRESSION, got: {}",
                err.code
            );
        }
        other => panic!("Expected SessionError(SEQUENCE_REGRESSION), got: {other:?}"),
    }
}

/// Scenario: Monotonically increasing sequence numbers accepted.
/// WHEN agent sends sequences 1, 2, 3,
/// THEN all are processed without error.
#[tokio::test]
async fn test_sequence_monotonic_accepted() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "seq-ok-agent", "test-key").await;

    for seq in 2u64..=4 {
        tx.send(ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::Heartbeat(Heartbeat {
                timestamp_mono_us: seq * 1000,
            })),
        })
        .await
        .unwrap();

        let msg = stream.next().await.unwrap().unwrap();
        match &msg.payload {
            Some(ServerPayload::Heartbeat(hb)) => {
                assert_eq!(hb.timestamp_mono_us, seq * 1000);
            }
            other => panic!("Expected Heartbeat echo at seq {seq}, got: {other:?}"),
        }
    }
}

// ─── Safe mode tests (RFC 0005 §3.7) ─────────────────────────────────────

/// Scenario: Mutations rejected during safe mode (RFC 0005 §3.7)
/// WHEN the runtime enters safe mode and sets `SharedState.safe_mode_atomic = true`,
/// THEN MutationBatch is rejected with SAFE_MODE_ACTIVE.
///
/// In this test we drive safe mode via `SharedState` directly (as the runtime
/// would do via a SessionSuspended broadcast to all sessions).
#[tokio::test]
async fn test_safe_mode_rejects_mutations() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "safe-mode-agent", "test-key").await;

    // Claim a tile before safe mode (ClaimTile creates a tile, so safe mode rejects it)
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 30_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let lease_msg = stream.next().await.unwrap().unwrap();
    let lease_id = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Enable safe mode in shared state (simulates runtime entering safe mode)
    {
        let st = shared_state.lock().await;
        st.safe_mode_atomic
            .store(true, std::sync::atomic::Ordering::Release);
    }

    // Send MutationBatch while safe mode is active — should be rejected
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: None,
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::RequestResult(err)) => {
            assert_eq!(
                err.code, "SAFE_MODE_ACTIVE",
                "Expected SAFE_MODE_ACTIVE, got: {}",
                err.code
            );
        }
        other => panic!("Expected RuntimeError(SAFE_MODE_ACTIVE), got: {other:?}"),
    }

    // Disable safe mode
    {
        let st = shared_state.lock().await;
        st.safe_mode_atomic
            .store(false, std::sync::atomic::Ordering::Release);
    }

    // Mutations should no longer be rejected with SAFE_MODE_ACTIVE.
    // We use a heartbeat to verify the session is still responsive and
    // the safe mode is cleared.
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 999,
        })),
    })
    .await
    .unwrap();

    let msg2 = stream.next().await.unwrap().unwrap();
    match &msg2.payload {
        Some(ServerPayload::Heartbeat(hb)) => {
            // Session still active after safe mode was cleared
            assert_eq!(hb.timestamp_mono_us, 999, "Heartbeat should echo correctly");
        }
        other => panic!("Expected Heartbeat after safe mode exit, got: {other:?}"),
    }

    // Now verify a MutationBatch is no longer blocked by SAFE_MODE_ACTIVE.
    // (It may still fail due to invalid lease, but not because of safe mode.)
    let batch_id2 = uuid::Uuid::now_v7().as_bytes().to_vec();
    // Use the real lease from earlier
    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id2.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: None,
        })),
    })
    .await
    .unwrap();

    let msg3 = stream.next().await.unwrap().unwrap();
    match &msg3.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert_ne!(
                result.code, "SAFE_MODE_ACTIVE",
                "safe mode should be cleared"
            );
            assert_eq!(result.batch_id, batch_id2);
        }
        other => panic!("Unexpected message after safe mode exit: {other:?}"),
    }
}

// ─── Freeze queue tests (system-shell/spec.md §Freeze Scene) ────────────

/// Scenario: Freeze queues mutations (spec line 146)
/// WHEN viewer activates freeze via SharedState.freeze_active = true
/// AND agent submits a MutationBatch
/// THEN mutations are queued (accepted = true), tile content does not update
#[tokio::test]
async fn test_freeze_queues_mutations_not_applied() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let generations = Arc::new(AtomicU64::new(0));
    let callback_generations = Arc::clone(&generations);
    let notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_generations.fetch_add(1, Ordering::AcqRel);
    });
    let (mut client, _server, shared_state) = setup_test_with_state_and_render_wake(notifier).await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "freeze-agent", "test-key").await;

    // Request a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 30_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let lease_msg = stream.next().await.unwrap().unwrap();
    let (lease_id, tile_id) = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => {
            (resp.lease_id.clone(), resp.ids[0].clone())
        }
        other => panic!("Expected a granted ClaimTile, got: {other:?}"),
    };
    let (scene_version_before, tile_count_before) = {
        let st = shared_state.lock().await;
        let scene = st.scene.lock().await;
        (scene.version, scene.tiles.len())
    };
    let parked_checkpoint = generations.load(Ordering::Acquire);

    // Activate freeze
    {
        let mut st = shared_state.lock().await;
        st.freeze_active = true;
    }

    // Submit a MutationBatch while frozen
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: vec![crate::proto::MutationProto {
                mutation: Some(crate::proto::mutation_proto::Mutation::UpdateTileOpacity(
                    crate::proto::UpdateTileOpacityMutation {
                        tile_id: tile_id.clone(),
                        opacity: 0.5,
                    },
                )),
            }],
            timing: None,
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            // Accepted=true: mutation was queued, not rejected
            assert_eq!(result.batch_id, batch_id);
            assert!(
                result.ok,
                "Mutation should be accepted (queued) during freeze, not rejected"
            );
            // Scene should NOT have been modified; error code should not be SAFE_MODE_ACTIVE
            assert_ne!(result.code, "SAFE_MODE_ACTIVE");
        }
        other => panic!("Expected MutationResult during freeze, got: {other:?}"),
    }
    assert_eq!(
        generations.load(Ordering::Acquire),
        parked_checkpoint,
        "freeze enqueue must not wake the compositor before any scene mutation"
    );
    {
        let st = shared_state.lock().await;
        let scene = st.scene.lock().await;
        assert_eq!(scene.version, scene_version_before);
        assert_eq!(scene.tiles.len(), tile_count_before);
    }

    // Deactivate freeze — queued mutation should be applied in next iteration
    {
        let mut st = shared_state.lock().await;
        st.freeze_active = false;
    }

    // Send a heartbeat to trigger the unfreeze drain on next loop iteration
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 9999,
        })),
    })
    .await
    .unwrap();

    // The unfreeze drain applies queued mutations (resulting in MutationResult(accepted))
    // before processing the heartbeat. We may get additional MutationResult messages.
    // Wait for the heartbeat echo to confirm the session is still active.
    let mut got_heartbeat = false;
    for _ in 0..5 {
        if let Some(Ok(msg)) = stream.next().await {
            match &msg.payload {
                Some(ServerPayload::Heartbeat(hb)) => {
                    assert_eq!(hb.timestamp_mono_us, 9999);
                    got_heartbeat = true;
                    break;
                }
                Some(ServerPayload::RequestResult(_)) => {
                    // Drained mutation result — expected, continue
                }
                other => panic!("Unexpected message after unfreeze: {other:?}"),
            }
        }
    }
    assert!(
        got_heartbeat,
        "Expected heartbeat echo after unfreeze drain"
    );
    assert_eq!(
        generations.load(Ordering::Acquire),
        parked_checkpoint + 1,
        "one applied queued batch must publish exactly one post-mutation wake"
    );
    {
        let st = shared_state.lock().await;
        let scene = st.scene.lock().await;
        assert!(scene.version > scene_version_before);
        assert_eq!(scene.tiles.len(), tile_count_before);
        let tile_id = bytes_to_scene_id(&tile_id).expect("claimed tile id");
        assert_eq!(scene.tiles[&tile_id].opacity, 0.5);
    }
}

/// Regression: FIFO ordering preserved when a new mutation arrives after unfreeze
/// but before the drain loop has emptied the queue.
///
/// Before the fix, `handle_mutation_batch` only checked `st.freeze_active`. Once
/// the shell cleared `freeze_active` the new mutation bypassed the queue and was
/// applied ahead of still-queued predecessors — a FIFO violation.
///
/// The fix: also check `!session.freeze_queue.is_empty()`. A new mutation that
/// arrives while the queue is non-empty is enqueued, preserving submission order
/// until the drain loop fully completes.
///
/// This is a direct unit test so it does not rely on timing: we manipulate the
/// freeze queue and shared state directly, then call `handle_mutation_batch` and
/// assert the queue depth increases (i.e., the new batch was enqueued, not
/// bypassed).
#[tokio::test]
async fn test_fifo_preserved_when_mutation_arrives_during_drain_window() {
    use tze_hud_resource::{ResourceStore, ResourceStoreConfig};
    use tze_hud_scene::graph::SceneGraph;

    // Build a minimal shared state with freeze_active = false (already unfrozen).
    let (outbound_tx, mut outbound_rx) =
        tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(16);

    let state: Arc<Mutex<SharedState>> = Arc::new(Mutex::new(SharedState {
        scene: Arc::new(Mutex::new(SceneGraph::new(800.0, 600.0))),
        sessions: crate::session::SessionRegistry::new(),
        resource_store: ResourceStore::new(ResourceStoreConfig::default()),
        widget_asset_store: crate::session::WidgetAssetStore::default(),
        runtime_widget_store: None,
        element_store: tze_hud_scene::element_store::ElementStore::default(),
        element_store_path: None,
        safe_mode_atomic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_tab_mirror: Arc::new(std::sync::Mutex::new(None)),
        token_store: crate::token::TokenStore::new(),
        freeze_active: false, // <-- already unfrozen
        input_capture_tx: None,
        input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        tile_placement: Default::default(),
    }));

    // Build a session whose freeze_queue already has one entry (simulates the
    // drain window: freeze was cleared but the drain loop has not run yet).
    let mut freeze_queue = SessionFreezeQueue::new(FREEZE_QUEUE_CAPACITY);
    let pre_queued_batch = MutationBatch {
        batch_id: b"pre-queued".to_vec(),
        lease_id: vec![0u8; 16],
        mutations: Vec::new(),
        ..Default::default()
    };
    freeze_queue.enqueue(pre_queued_batch, "test-ns");
    assert!(
        !freeze_queue.is_empty(),
        "Precondition: queue must be non-empty before the race window test"
    );

    let mut session = StreamSession {
        session_id: "fifo-test-session".to_string(),
        namespace: "test-ns".to_string(),
        agent_name: "test-agent".to_string(),
        capabilities: Vec::new(),
        lease_ids: Vec::new(),
        scene_session_id: SceneId::new(),
        resource_budget: ResourceBudget::default(),
        budget_enforcer: None,
        subscriptions: Vec::new(),
        subscription_filters: std::collections::HashMap::new(),
        server_sequence: 0,
        resume_token: Vec::new(),
        last_heartbeat_ms: 0,
        state: SessionState::Active,
        last_client_sequence: 1,
        safe_mode_active: false,
        freeze_queue,
        session_open_at_wall_us: 0,
        dedup_window: DedupWindow::new(1000, 60),
        lease_correlation_cache: LeaseCorrelationCache::new(
            DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY,
        ),
        resource_upload_rate_limiter: UploadByteRateLimiter::with_limit(
            tze_hud_resource::DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC,
        ),
    };

    // The new mutation arrives while freeze_active=false but the queue is non-empty
    // (the "drain window" race). With the fix this must be enqueued, not applied.
    let new_batch = MutationBatch {
        batch_id: b"new-in-drain-window".to_vec(),
        lease_id: vec![0u8; 16],
        mutations: Vec::new(),
        ..Default::default()
    };
    handle_mutation_batch(
        &state,
        &mut session,
        &outbound_tx,
        0,
        new_batch,
        &tze_hud_scene::render_wake::RenderWakeNotifier::default(),
    )
    .await;

    // The queue must now hold 2 entries: the pre-queued one plus the new arrival.
    // If the fix is absent, the new batch bypasses the queue (queue depth stays 1)
    // and the pre-queued mutations would be applied AFTER the bypassing one — FIFO
    // violation.
    let queue_depth = {
        // Drain the queue to count entries, then verify order by batch_id.
        let drained = session.freeze_queue.drain();
        drained.len()
    };
    assert_eq!(
        queue_depth, 2,
        "Both the pre-queued batch and the new batch must be in the freeze queue \
         (FIFO preserved). If this is 1, the new batch bypassed the queue — FIFO violated."
    );

    // The outbound channel should have received accepted=true for the new batch
    // (same as the enqueue path response, not an error).
    let response = outbound_rx
        .recv()
        .await
        .expect("expected a MutationResult response")
        .expect("expected Ok response");
    match &response.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert_eq!(r.batch_id, b"new-in-drain-window".to_vec());
            assert!(
                r.ok,
                "New batch must be accepted (enqueued) during drain window, not rejected"
            );
        }
        other => panic!(
            "Expected MutationResult(accepted=true) for new batch during drain window, got: {other:?}"
        ),
    }
}

/// Regression: a Transactional batch retransmitted while the scene is frozen must
/// be applied exactly once after drain, not twice.
///
/// Before the fix, the freeze enqueue path did not consult `dedup_window`.  A
/// retransmit (same `batch_id`) while frozen was pushed onto the freeze queue as a
/// second distinct entry and applied twice when the queue drained — producing a
/// duplicate-application that never occurs on the non-frozen path.
///
/// The fix: check `dedup_window` at enqueue time (symmetric with the non-frozen
/// path) and record the batch in the window immediately after a successful enqueue.
/// A retransmit hits the window and is suppressed before it can be pushed.
///
/// This is a direct unit test: we manipulate `StreamSession` and `SharedState`
/// directly so the assertion is deterministic (no timing dependency).
#[tokio::test]
async fn test_freeze_retransmit_deduped_applied_exactly_once() {
    use tze_hud_resource::{ResourceStore, ResourceStoreConfig};
    use tze_hud_scene::graph::SceneGraph;

    let (outbound_tx, mut outbound_rx) =
        tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(32);

    // Shared state: scene is frozen.
    let state: Arc<Mutex<SharedState>> = Arc::new(Mutex::new(SharedState {
        scene: Arc::new(Mutex::new(SceneGraph::new(800.0, 600.0))),
        sessions: crate::session::SessionRegistry::new(),
        resource_store: ResourceStore::new(ResourceStoreConfig::default()),
        widget_asset_store: crate::session::WidgetAssetStore::default(),
        runtime_widget_store: None,
        element_store: tze_hud_scene::element_store::ElementStore::default(),
        element_store_path: None,
        safe_mode_atomic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_tab_mirror: Arc::new(std::sync::Mutex::new(None)),
        token_store: crate::token::TokenStore::new(),
        freeze_active: true, // <-- scene is frozen
        input_capture_tx: None,
        input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        tile_placement: Default::default(),
    }));

    let mut session = StreamSession {
        session_id: "dedup-freeze-test".to_string(),
        namespace: "test-ns".to_string(),
        agent_name: "test-agent".to_string(),
        capabilities: Vec::new(),
        lease_ids: Vec::new(),
        scene_session_id: SceneId::new(),
        resource_budget: ResourceBudget::default(),
        budget_enforcer: None,
        subscriptions: Vec::new(),
        subscription_filters: std::collections::HashMap::new(),
        server_sequence: 0,
        resume_token: Vec::new(),
        last_heartbeat_ms: 0,
        state: SessionState::Active,
        last_client_sequence: 1,
        safe_mode_active: false,
        freeze_queue: SessionFreezeQueue::new(FREEZE_QUEUE_CAPACITY),
        session_open_at_wall_us: 0,
        dedup_window: DedupWindow::new(1000, 60),
        lease_correlation_cache: LeaseCorrelationCache::new(
            DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY,
        ),
        resource_upload_rate_limiter: UploadByteRateLimiter::with_limit(
            tze_hud_resource::DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC,
        ),
    };

    // Use a valid 16-byte batch_id so the dedup window key is populated.
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();

    // ── First send: batch enqueued while frozen ───────────────────────────────
    let original_batch = MutationBatch {
        batch_id: batch_id.clone(),
        lease_id: vec![0u8; 16],
        mutations: Vec::new(),
        ..Default::default()
    };
    handle_mutation_batch(
        &state,
        &mut session,
        &outbound_tx,
        0,
        original_batch,
        &tze_hud_scene::render_wake::RenderWakeNotifier::default(),
    )
    .await;

    // Queue must hold exactly one entry.
    assert!(
        !session.freeze_queue.is_empty(),
        "Precondition: first batch must have been enqueued"
    );

    // Consume the enqueue ack (accepted=true).
    let first_ack = outbound_rx
        .recv()
        .await
        .expect("expected MutationResult for first send")
        .expect("expected Ok result");
    match &first_ack.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert_eq!(r.batch_id, batch_id, "batch_id must match");
            assert!(r.ok, "first enqueue must be accepted");
        }
        other => panic!("Expected MutationResult for first send, got: {other:?}"),
    }

    // ── Retransmit: same batch_id, still frozen ───────────────────────────────
    let retransmit_batch = MutationBatch {
        batch_id: batch_id.clone(),
        lease_id: vec![0u8; 16],
        mutations: Vec::new(),
        ..Default::default()
    };
    handle_mutation_batch(
        &state,
        &mut session,
        &outbound_tx,
        0,
        retransmit_batch,
        &tze_hud_scene::render_wake::RenderWakeNotifier::default(),
    )
    .await;

    // Consume the dedup response — must be accepted=true (cached from first send).
    let dedup_ack = outbound_rx
        .recv()
        .await
        .expect("expected MutationResult for retransmit")
        .expect("expected Ok result");
    match &dedup_ack.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert_eq!(r.batch_id, batch_id, "batch_id must match on retransmit");
            assert!(r.ok, "dedup hit must return cached accepted=true");
        }
        other => {
            panic!("Expected cached MutationResult on retransmit while frozen, got: {other:?}")
        }
    }

    // ── Key assertion: queue has exactly ONE entry ────────────────────────────
    // If the retransmit was deduped (the fix works), the queue depth is 1.
    // Without the fix the retransmit was pushed as a second entry (depth 2)
    // and would have been applied twice on drain.
    let drained = session.freeze_queue.drain();
    assert_eq!(
        drained.len(),
        1,
        "Retransmit while frozen must be deduped: freeze queue must contain \
         exactly one entry, not two. If this is 2, the retransmit was enqueued \
         again and would have been applied twice after drain."
    );
    assert_eq!(
        drained[0].batch_id, batch_id,
        "The single queued entry must be the original batch"
    );

    // No further messages should be pending.
    assert!(
        outbound_rx.try_recv().is_err(),
        "No additional messages should be in the outbound channel after dedup"
    );
}

/// Scenario: Freeze ignored during safe mode (spec line 137)
/// WHEN safe mode is active AND freeze is set
/// THEN mutations are rejected with SAFE_MODE_ACTIVE (not queued)
#[tokio::test]
async fn test_safe_mode_takes_precedence_over_freeze() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "safe-freeze-agent", "test-key").await;

    // Request a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 30_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let lease_msg = stream.next().await.unwrap().unwrap();
    let lease_id = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Set BOTH safe mode and freeze (invariant: safe mode cancels freeze, but we test
    // that safe mode takes precedence in the session server check order)
    {
        let mut st = shared_state.lock().await;
        st.safe_mode_atomic
            .store(true, std::sync::atomic::Ordering::Release);
        st.freeze_active = false; // Invariant: safe_mode=true => freeze_active=false
    }

    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::RequestResult(err)) => {
            assert_eq!(err.code, "SAFE_MODE_ACTIVE");
        }
        other => panic!("Expected SAFE_MODE_ACTIVE RuntimeError, got: {other:?}"),
    }
}

/// Scenario: SessionFreezeQueue unit test — MUTATION_QUEUE_PRESSURE at 80% capacity
#[test]
fn test_session_freeze_queue_pressure_signal() {
    let mut q = SessionFreezeQueue::new(10);
    // Fill 7 entries (70%) without crossing threshold
    for i in 0..7 {
        let batch = MutationBatch {
            batch_id: format!("b{i}").into_bytes(),
            lease_id: vec![0u8; 16],
            mutations: Vec::new(),
            ..Default::default()
        };
        let r = q.enqueue(batch, "ns");
        assert!(
            matches!(
                r,
                FreezeEnqueueResult::Queued {
                    pressure_warning: false
                }
            ),
            "Expected no pressure warning at {i}/7"
        );
    }
    // 8th entry crosses 80%
    let batch = MutationBatch {
        batch_id: b"b7".to_vec(),
        lease_id: vec![0u8; 16],
        mutations: Vec::new(),
        ..Default::default()
    };
    let r = q.enqueue(batch, "ns");
    assert!(
        matches!(
            r,
            FreezeEnqueueResult::Queued {
                pressure_warning: true
            }
        ),
        "Expected pressure_warning=true at 80%"
    );
}

/// Scenario: SessionFreezeQueue transactional never evicted
#[test]
fn test_session_freeze_queue_transactional_never_evicted() {
    use crate::proto::mutation_proto::Mutation;
    use crate::proto::{CreateTileMutation, MutationProto};

    let mut q = SessionFreezeQueue::new(2);
    // Fill with non-empty (StateStream) batches
    for i in 0..2 {
        let batch = MutationBatch {
            batch_id: format!("ss{i}").into_bytes(),
            lease_id: vec![0u8; 16],
            mutations: vec![],
            ..Default::default()
        };
        q.enqueue(batch, "ns");
    }

    // Submit a transactional mutation (CreateTile) — should get backpressure
    let tx_batch = MutationBatch {
        batch_id: b"tx1".to_vec(),
        lease_id: vec![0u8; 16],
        mutations: vec![MutationProto {
            mutation: Some(Mutation::CreateTile(CreateTileMutation {
                tab_id: vec![], // empty = server infers active tab
                bounds: None,
                z_order: 0,
            })),
        }],
        ..Default::default()
    };
    let r = q.enqueue(tx_batch, "ns");
    assert!(
        matches!(r, FreezeEnqueueResult::BackpressureRequired),
        "Transactional mutation should require backpressure when queue is full, got: {r:?}"
    );
}

// ─── Session state machine tests (RFC 0005 §1.1) ─────────────────────────

/// Scenario: Successful session establishment transitions through Connecting→Handshaking→Active.
/// The state machine starts in Handshaking during the handle_session_init call and
/// transitions to Active after the handshake response is sent.
#[tokio::test]
async fn test_state_machine_successful_establishment() {
    let (mut client, _server) = setup_test().await;
    let (_tx, messages, _stream) = handshake(&mut client, "state-test-agent", "test-key").await;

    // The complete initial baseline is establishment, snapshot, then current policy.
    assert_eq!(
        messages.len(),
        3,
        "Expected SessionEstablished + SceneSnapshot + DegradationNotice"
    );
    assert!(
        matches!(
            messages[0].payload,
            Some(ServerPayload::SessionEstablished(_))
        ),
        "First message must be SessionEstablished"
    );
    assert!(
        matches!(messages[1].payload, Some(ServerPayload::SceneSnapshot(_))),
        "Second message must be SceneSnapshot"
    );
    assert!(
        matches!(
            messages[2].payload,
            Some(ServerPayload::DegradationNotice(_))
        ),
        "Third message must be current DegradationNotice"
    );
}

/// Scenario: Auth failure transitions Handshaking→Closed with SessionError.
#[tokio::test]
async fn test_state_machine_auth_failure_to_closed() {
    let (mut client, _server) = setup_test().await;

    let (init_tx, init_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(init_rx);

    init_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionInit(SessionInit {
                agent_id: "state-fail-agent".to_string(),
                initial_subscriptions: Vec::new(),
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(crate::auth::psk_credential("wrong-key".to_string())),
            })),
        })
        .await
        .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();

    // State machine should send SessionError (AUTH_FAILED) and transition to Closed
    match &msg.payload {
        Some(ServerPayload::SessionError(error)) => {
            assert_eq!(error.code, "AUTH_FAILED");
        }
        other => panic!("Expected SessionError(AUTH_FAILED), got: {other:?}"),
    }
}

/// Scenario: Graceful disconnect via SessionClose.
/// The session stream should terminate cleanly after SessionClose is sent.
#[tokio::test]
async fn test_graceful_disconnect_session_close() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "close-agent", "test-key").await;

    // Send SessionClose
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionClose(SessionClose {
            reason: "test shutdown".to_string(),
        })),
    })
    .await
    .unwrap();

    // Stream should close (no response expected for SessionClose)
    // Give the server a moment to process
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // The stream should be closed; next() should return None or an error
    // (The server closes the stream after transitioning to Closed state)
    drop(tx);
    // Drain any remaining messages
    let mut got_stream_end = false;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
    loop {
        if tokio::time::Instant::now() > deadline {
            break;
        }
        match tokio::time::timeout(tokio::time::Duration::from_millis(100), stream.next()).await {
            Ok(None) | Err(_) => {
                got_stream_end = true;
                break;
            }
            Ok(Some(_)) => {
                // Some message still in transit, keep draining
            }
        }
    }
    assert!(
        got_stream_end,
        "session stream did not terminate after SessionClose — graceful disconnect had no observable effect"
    );
}

// ─── Traffic class classification tests ─────────────────────────────────

/// Verify traffic class routing for server payloads.
#[test]
fn test_traffic_class_routing() {
    use crate::proto::session::*;

    // Transactional messages
    assert_eq!(
        classify_server_payload(&ServerPayload::SessionEstablished(
            SessionEstablished::default()
        )),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::RequestResult(RequestResult::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::RequestResult(RequestResult::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::SessionSuspended(SessionSuspended::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::SessionResumed(SessionResumed::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::RequestResult(RequestResult::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::ResourceUploadAccepted(
            ResourceUploadAccepted::default(),
        )),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::ResourceStored(ResourceStored::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::ResourceErrorResponse(
            ResourceErrorResponse::default(),
        )),
        TrafficClass::Transactional,
    );

    // StateStream messages
    assert_eq!(
        classify_server_payload(&ServerPayload::SceneSnapshot(SceneSnapshot::default())),
        TrafficClass::StateStream,
    );

    // DegradationNotice — transactional (RFC 0005 §3.4)
    assert_eq!(
        classify_server_payload(&ServerPayload::DegradationNotice(
            DegradationNotice::default()
        )),
        TrafficClass::Transactional,
    );

    // Ephemeral messages
    assert_eq!(
        classify_server_payload(&ServerPayload::Heartbeat(Heartbeat::default())),
        TrafficClass::Ephemeral,
    );
}

// ─── Sequence validation unit tests ─────────────────────────────────────

/// Unit tests for StreamSession::validate_client_sequence.
#[test]
fn test_validate_sequence_unit() {
    let mut session = StreamSession {
        session_id: "test".to_string(),
        namespace: "test".to_string(),
        agent_name: "test".to_string(),
        capabilities: Vec::new(),
        lease_ids: Vec::new(),
        scene_session_id: SceneId::new(),
        resource_budget: ResourceBudget::default(),
        budget_enforcer: None,
        subscriptions: Vec::new(),
        subscription_filters: std::collections::HashMap::new(),
        server_sequence: 0,
        resume_token: Vec::new(),
        last_heartbeat_ms: 0,
        state: SessionState::Active,
        last_client_sequence: 1,
        safe_mode_active: false,
        freeze_queue: SessionFreezeQueue::new(FREEZE_QUEUE_CAPACITY),
        session_open_at_wall_us: now_wall_us(),
        dedup_window: DedupWindow::new(1000, 60),
        lease_correlation_cache: LeaseCorrelationCache::new(
            DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY,
        ),
        resource_upload_rate_limiter: UploadByteRateLimiter::with_limit(
            tze_hud_resource::DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC,
        ),
    };

    // seq=2 (gap=1): OK
    assert!(session.validate_client_sequence(2, 100).is_ok());
    assert_eq!(session.last_client_sequence, 2);

    // seq=102 (gap=100): still OK (gap == max_gap, not >)
    assert!(session.validate_client_sequence(102, 100).is_ok());
    assert_eq!(session.last_client_sequence, 102);

    // seq=203 (gap=101): exceeds max_gap=100
    let err = session.validate_client_sequence(203, 100);
    assert!(err.is_err());
    let (code, _) = err.unwrap_err();
    assert_eq!(code, "SEQUENCE_GAP_EXCEEDED");
    // last_client_sequence unchanged on error
    assert_eq!(session.last_client_sequence, 102);

    // seq=50 (regression): error
    let err = session.validate_client_sequence(50, 100);
    assert!(err.is_err());
    let (code, _) = err.unwrap_err();
    assert_eq!(code, "SEQUENCE_REGRESSION");

    // seq=102 (same as last): regression (not strictly greater)
    let err = session.validate_client_sequence(102, 100);
    assert!(err.is_err());
    let (code, _) = err.unwrap_err();
    assert_eq!(code, "SEQUENCE_REGRESSION");
}

// ─── Handshake auth, version, capability, subscription tests (rig-8uqz) ──

/// Scenario: Structured AuthCredential (PSK) accepted (RFC 0005 §1.4)
/// WHEN agent sends SessionInit with a valid PreSharedKeyCredential in auth_credential,
/// THEN runtime authenticates and proceeds to SessionEstablished.
#[tokio::test]
async fn test_auth_structured_psk_credential_accepted() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "psk-agent".to_string(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::proto::session::AuthCredential {
                credential: Some(
                    crate::proto::session::auth_credential::Credential::PreSharedKey(
                        crate::proto::session::PreSharedKeyCredential {
                            key: "test-key".to_string(),
                        },
                    ),
                ),
            }),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionEstablished(_)) => {}
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    }
}

/// Scenario: Invalid structured PSK credential rejected with AUTH_FAILED (RFC 0005 §1.4)
/// WHEN agent sends SessionInit with a wrong PreSharedKeyCredential,
/// THEN runtime sends SessionError(AUTH_FAILED) and closes stream.
#[tokio::test]
async fn test_auth_structured_psk_credential_wrong_key() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "bad-psk-agent".to_string(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::proto::session::AuthCredential {
                credential: Some(
                    crate::proto::session::auth_credential::Credential::PreSharedKey(
                        crate::proto::session::PreSharedKeyCredential {
                            key: "wrong-key".to_string(),
                        },
                    ),
                ),
            }),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(err.code, "AUTH_FAILED");
        }
        other => panic!("Expected SessionError(AUTH_FAILED), got: {other:?}"),
    }
}

/// Scenario: LocalSocketCredential accepted (RFC 0005 §1.4)
/// WHEN agent sends SessionInit with a valid LocalSocketCredential,
/// THEN runtime authenticates and proceeds to SessionEstablished.
#[tokio::test]
async fn test_auth_local_socket_credential_accepted() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "local-agent".to_string(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::proto::session::AuthCredential {
                credential: Some(
                    crate::proto::session::auth_credential::Credential::LocalSocket(
                        crate::proto::session::LocalSocketCredential {
                            socket_path: "/run/tze_hud.sock".to_string(),
                            pid_hint: "42".to_string(),
                        },
                    ),
                ),
            }),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionEstablished(_)) => {}
        other => panic!("Expected SessionEstablished with LocalSocket cred, got: {other:?}"),
    }
}

// ── Wire-level LocalSocket non-loopback rejection (hud-stl9j / hud-1aswu.1) ──
//
// The gRPC integration tests above always connect from loopback (::1), so
// peer_ip is always loopback there.  These unit tests call handle_session_init
// and handle_session_resume directly — bypassing the TCP transport — so we can
// inject an arbitrary peer_ip and assert AUTH_FAILED on the non-loopback path.

fn local_socket_session_init(agent_id: &str) -> SessionInit {
    SessionInit {
        agent_id: agent_id.to_string(),
        initial_subscriptions: Vec::new(),
        resume_token: Vec::new(),
        min_protocol_version: 1000,
        max_protocol_version: 1001,
        auth_credential: Some(crate::proto::session::AuthCredential {
            credential: Some(
                crate::proto::session::auth_credential::Credential::LocalSocket(
                    crate::proto::session::LocalSocketCredential {
                        socket_path: "/run/tze_hud.sock".to_string(),
                        pid_hint: "42".to_string(),
                    },
                ),
            ),
        }),
    }
}

/// Scenario: LocalSocketCredential + non-loopback peer → wire AUTH_FAILED on init path.
///
/// GIVEN a SessionInit carrying a LocalSocketCredential,
/// WHEN handle_session_init is called with peer_ip = Some(10.0.0.5),
/// THEN the server message channel receives SessionError { code: "AUTH_FAILED" }
///      and handle_session_init returns None (session not established).
///
/// Security regression gate for hud-1aswu.1: a future refactor that removes
/// the loopback check would cause this test to fail instead of silently breaking.
#[tokio::test]
async fn test_handle_session_init_local_socket_non_loopback_auth_failed() {
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");
    let state = service.state.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(16);

    let init = local_socket_session_init("non-loopback-agent");
    let non_loopback_ip: std::net::IpAddr = "10.0.0.5".parse().unwrap();

    let agents = tze_hud_scene::config::AgentDirectory::unrestricted("test-key");
    let budget = ResourceBudget::default();
    let ctx = HandshakeCtx {
        state: &state,
        agents: &agents,
        resource_budget: &budget,
        budget_enforcer: None,
        peer_ip: Some(non_loopback_ip),
    };
    let session = handle_session_init(ctx, &tx, &init).await;

    assert!(
        session.is_none(),
        "handle_session_init must return None for non-loopback LocalSocket peer"
    );

    let server_msg = rx
        .recv()
        .await
        .expect("server must send a message on auth failure")
        .expect("message must not be a transport error");

    match server_msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "AUTH_FAILED",
                "non-loopback LocalSocket must produce AUTH_FAILED, got: {}",
                err.code
            );
            assert!(
                err.message.contains("not a loopback address"),
                "error message must mention loopback, got: {}",
                err.message
            );
        }
        other => panic!(
            "Expected SessionError(AUTH_FAILED) for non-loopback LocalSocket init, \
                 got: {other:?}"
        ),
    }
}

/// Scenario: LocalSocketCredential + non-loopback peer → wire AUTH_FAILED on resume path.
///
/// GIVEN a SessionResume carrying a LocalSocketCredential,
/// WHEN handle_session_resume is called with peer_ip = Some(10.0.0.5),
/// THEN the server message channel receives SessionError { code: "AUTH_FAILED" }
///      and handle_session_resume returns None (resume rejected before token check).
///
/// Security regression gate for hud-1aswu.1 resume path: the resume path re-
/// authenticates independently; this test pins it.
#[tokio::test]
async fn test_handle_session_resume_local_socket_non_loopback_auth_failed() {
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");
    let state = service.state.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(16);

    // A bogus resume token — auth is checked before the token, so this value is
    // irrelevant; the test asserts AUTH_FAILED fires before SESSION_GRACE_EXPIRED.
    let bogus_token = vec![0u8; 16];

    let resume = SessionResume {
        agent_id: "non-loopback-resume-agent".to_string(),
        resume_token: bogus_token,
        last_seen_server_sequence: 0,
        pre_shared_key: String::new(),
        auth_credential: Some(crate::proto::session::AuthCredential {
            credential: Some(
                crate::proto::session::auth_credential::Credential::LocalSocket(
                    crate::proto::session::LocalSocketCredential {
                        socket_path: "/run/tze_hud.sock".to_string(),
                        pid_hint: "99".to_string(),
                    },
                ),
            ),
        }),
    };

    let non_loopback_ip: std::net::IpAddr = "10.0.0.5".parse().unwrap();

    let agents = tze_hud_scene::config::AgentDirectory::unrestricted("test-key");
    let budget = ResourceBudget::default();
    let ctx = HandshakeCtx {
        state: &state,
        agents: &agents,
        resource_budget: &budget,
        budget_enforcer: None,
        peer_ip: Some(non_loopback_ip),
    };
    let session = handle_session_resume(ctx, &tx, &resume).await;

    assert!(
        session.is_none(),
        "handle_session_resume must return None for non-loopback LocalSocket peer"
    );

    let server_msg = rx
        .recv()
        .await
        .expect("server must send a message on auth failure")
        .expect("message must not be a transport error");

    match server_msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "AUTH_FAILED",
                "non-loopback LocalSocket resume must produce AUTH_FAILED, got: {}",
                err.code
            );
        }
        other => panic!(
            "Expected SessionError(AUTH_FAILED) for non-loopback LocalSocket resume, \
                 got: {other:?}"
        ),
    }
}

/// Scenario: Version negotiated successfully (RFC 0005 §4.1)
/// WHEN agent declares min=1000, max=1001 and runtime supports 1000-1001,
/// THEN SessionEstablished contains negotiated_protocol_version=1001.
#[tokio::test]
async fn test_version_negotiation_success() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "version-agent".to_string(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionEstablished(established)) => {
            assert_eq!(
                established.negotiated_protocol_version, 1001,
                "Should pick highest mutual version (1001)"
            );
        }
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    }
}

/// Scenario: Version negotiation failure — no mutual version (RFC 0005 §4.1)
/// WHEN agent declares min=2000, max=2001 and runtime only supports 1000-1001,
/// THEN runtime sends SessionError(code=UNSUPPORTED_PROTOCOL_VERSION) and closes stream.
#[tokio::test]
async fn test_version_negotiation_unsupported() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "old-agent".to_string(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 2000,
            max_protocol_version: 2001,
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "UNSUPPORTED_PROTOCOL_VERSION",
                "Expected UNSUPPORTED_PROTOCOL_VERSION, got: {}",
                err.code
            );
            // Hint should include runtime's supported range
            assert!(
                !err.hint.is_empty(),
                "Hint should contain runtime version range"
            );
        }
        other => panic!("Expected SessionError(UNSUPPORTED_PROTOCOL_VERSION), got: {other:?}"),
    }
}

#[test]
fn test_capability_set_covers_wildcard_grants() {
    let caps = vec!["publish_zone:*".to_string(), "publish_widget:*".to_string()];
    assert!(capability_set_covers(&caps, "publish_zone:subtitle"));
    assert!(capability_set_covers(&caps, "publish_widget:gauge"));
    assert!(!capability_set_covers(&caps, "create_tiles"));
}

/// Scenario: PSK agent with access_input_events capability successfully subscribes to
/// INPUT_EVENTS (RFC 0005 §7.1).
/// WHEN a PSK-authenticated agent requests INPUT_EVENTS subscription AND includes
/// access_input_events in requested_capabilities,
/// THEN SessionEstablished includes INPUT_EVENTS in active_subscriptions and
/// denied_subscriptions is empty.
///
/// Subscription gating uses the agent's explicitly granted capabilities (RFC 0005 §7.1).
/// Agents must request the required capability to subscribe to gated categories.
#[tokio::test]
async fn test_psk_with_capability_allows_input_events_subscription() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    // PSK agent requesting INPUT_EVENTS subscription WITH the required capability
    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "sub-test-agent".to_string(),
            initial_subscriptions: vec!["INPUT_EVENTS".to_string()],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionEstablished(established)) => {
            // Agent with access_input_events capability should have INPUT_EVENTS active
            assert!(
                established
                    .active_subscriptions
                    .contains(&"INPUT_EVENTS".to_string()),
                "Agent with access_input_events should have INPUT_EVENTS in active_subscriptions; \
                     active={:?}, denied={:?}",
                established.active_subscriptions,
                established.denied_subscriptions
            );
            assert!(
                established.denied_subscriptions.is_empty(),
                "Agent with required capability should have no denied subscriptions"
            );
        }
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    }
}

/// Helper that returns the shared state alongside the client for state-manipulation tests.
async fn setup_test_with_state() -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<SharedState>>,
) {
    setup_test_with_state_and_render_wake(tze_hud_scene::render_wake::RenderWakeNotifier::default())
        .await
}

async fn setup_test_with_state_and_render_wake(
    render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<SharedState>>,
) {
    let scene = SceneGraph::new(800.0, 600.0);
    let mut service =
        HudSessionImpl::new(scene, "test-key").with_agent_permissions(restricted_test_agents());
    service.render_wake = render_wake;
    let shared_state = service.state.clone();

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    (client, handle, shared_state)
}

// ─── Reconnection and resume tests (RFC 0005 §6.1–6.6, rig-3dou) ────────

/// Helper: perform a full handshake and return the resume token.
///
/// Drops the sender and response stream, waits for server-side cleanup,
/// then returns the resume token for use in subsequent resume attempts.
async fn handshake_and_disconnect(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
    psk: &str,
) -> Vec<u8> {
    let (tx, init_messages, stream) = handshake(client, agent_id, psk).await;
    let resume_token = match &init_messages[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => e.resume_token.clone(),
        _ => panic!("Expected SessionEstablished"),
    };
    drop(tx);
    drop(stream);
    // Allow server task to process EOF and register the resume token.
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    resume_token
}

/// Scenario (rig-3dou AC): Reconnect within grace period succeeds with
/// `SessionResumeResult(accepted=true)`.
/// RFC 0005 §6.1–6.3
#[tokio::test]
async fn test_reconnect_within_grace_accepted() {
    let (mut client, _server) = setup_test().await;
    let resume_token = handshake_and_disconnect(&mut client, "resume-ok-agent", "test-key").await;

    let (resume_tx, resume_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let resume_stream = tokio_stream::wrappers::ReceiverStream::new(resume_rx);

    resume_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionResume(SessionResume {
                agent_id: "resume-ok-agent".to_string(),
                resume_token: resume_token.clone(),
                last_seen_server_sequence: 2,
                pre_shared_key: "test-key".to_string(),
                auth_credential: None,
            })),
        })
        .await
        .unwrap();

    let mut response_stream = client.session(resume_stream).await.unwrap().into_inner();

    let msg1 = response_stream.next().await.unwrap().unwrap();
    match &msg1.payload {
        Some(ServerPayload::SessionResumeResult(result)) => {
            assert!(result.accepted, "expected resume to be accepted");
            assert!(
                !result.new_session_token.is_empty(),
                "new token must be issued"
            );
            assert_ne!(
                result.new_session_token, resume_token,
                "new token must differ from old token"
            );
            assert_eq!(
                result.negotiated_protocol_version,
                crate::auth::RUNTIME_MAX_VERSION
            );
        }
        other => panic!("Expected SessionResumeResult, got: {other:?}"),
    }

    // Full SceneSnapshot must follow SessionResumeResult (RFC 0005 §6.4).
    let msg2 = response_stream.next().await.unwrap().unwrap();
    match &msg2.payload {
        Some(ServerPayload::SceneSnapshot(_)) => {}
        other => panic!("Expected SceneSnapshot after resume, got: {other:?}"),
    }
}

/// Scenario (rig-3dou AC): New session token is issued on resume; old token
/// is single-use and consumed.
/// RFC 0005 §6.1 — "single-use for resumption"
#[tokio::test]
async fn test_resume_token_single_use() {
    let (mut client, _server) = setup_test().await;
    let resume_token = handshake_and_disconnect(&mut client, "single-use-agent", "test-key").await;

    // First resume: should succeed and consume the token.
    let (tx1, rx1) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let s1 = tokio_stream::wrappers::ReceiverStream::new(rx1);
    tx1.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionResume(SessionResume {
            agent_id: "single-use-agent".to_string(),
            resume_token: resume_token.clone(),
            last_seen_server_sequence: 2,
            pre_shared_key: "test-key".to_string(),
            auth_credential: None,
        })),
    })
    .await
    .unwrap();

    let mut r1 = client.session(s1).await.unwrap().into_inner();
    let first_resume = r1.next().await.unwrap().unwrap();
    match &first_resume.payload {
        Some(ServerPayload::SessionResumeResult(result)) => {
            assert!(result.accepted, "first resume must succeed");
        }
        other => panic!("Expected SessionResumeResult, got: {other:?}"),
    }
    drop(tx1);
    drop(r1);
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    // Second resume attempt with the same original token: must fail.
    let (tx2, rx2) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let s2 = tokio_stream::wrappers::ReceiverStream::new(rx2);
    tx2.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionResume(SessionResume {
            agent_id: "single-use-agent".to_string(),
            resume_token: resume_token.clone(),
            last_seen_server_sequence: 2,
            pre_shared_key: "test-key".to_string(),
            auth_credential: None,
        })),
    })
    .await
    .unwrap();

    let mut r2 = client.session(s2).await.unwrap().into_inner();
    let second_resume = r2.next().await.unwrap().unwrap();
    match &second_resume.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "SESSION_GRACE_EXPIRED",
                "second use of same token must fail with SESSION_GRACE_EXPIRED, got: {}",
                err.code
            );
        }
        other => panic!("Expected SessionError(SESSION_GRACE_EXPIRED), got: {other:?}"),
    }
}

/// Scenario (rig-3dou AC): Re-authentication required on resume.
/// Invalid credentials result in `SessionError(AUTH_FAILED)`.
/// RFC 0005 §6.2
#[tokio::test]
async fn test_resume_auth_required() {
    let (mut client, _server) = setup_test().await;
    let resume_token = handshake_and_disconnect(&mut client, "auth-check-agent", "test-key").await;

    let (resume_tx, resume_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let resume_stream = tokio_stream::wrappers::ReceiverStream::new(resume_rx);

    // Use wrong PSK on resume — must be rejected with AUTH_FAILED.
    resume_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionResume(SessionResume {
                agent_id: "auth-check-agent".to_string(),
                resume_token: resume_token.clone(),
                last_seen_server_sequence: 2,
                pre_shared_key: "wrong-key".to_string(),
                auth_credential: None,
            })),
        })
        .await
        .unwrap();

    let mut response_stream = client.session(resume_stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "AUTH_FAILED",
                "expected AUTH_FAILED, got: {}",
                err.code
            );
        }
        other => panic!("Expected SessionError(AUTH_FAILED), got: {other:?}"),
    }
}

/// Scenario (rig-3dou AC): Bogus token (as if runtime restarted and all tokens
/// cleared) is rejected with `SESSION_GRACE_EXPIRED`.
/// RFC 0005 §6.6
#[tokio::test]
async fn test_bogus_token_rejected_with_grace_expired() {
    let (mut client, _server) = setup_test().await;

    let bogus_token = uuid::Uuid::now_v7().as_bytes().to_vec();

    let (resume_tx, resume_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let resume_stream = tokio_stream::wrappers::ReceiverStream::new(resume_rx);

    resume_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionResume(SessionResume {
                agent_id: "restart-agent".to_string(),
                resume_token: bogus_token,
                last_seen_server_sequence: 0,
                pre_shared_key: "test-key".to_string(),
                auth_credential: None,
            })),
        })
        .await
        .unwrap();

    let mut response_stream = client.session(resume_stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "SESSION_GRACE_EXPIRED",
                "unknown token must produce SESSION_GRACE_EXPIRED, got: {}",
                err.code
            );
            assert!(
                !err.hint.is_empty(),
                "hint should direct client to SessionInit"
            );
        }
        other => panic!("Expected SessionError(SESSION_GRACE_EXPIRED), got: {other:?}"),
    }
}

/// Scenario (rig-3dou AC): SessionResumeResult carries complete subscription state.
/// RFC 0005 §6.3 — agents MUST use confirmed subscription state, not assume pre-disconnect set.
#[tokio::test]
async fn test_resume_result_carries_subscription_state() {
    let (mut client, _server) = setup_test().await;

    // Establish a session that requested a specific subscription.
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "sub-resume-agent".to_string(),
            // Include required capabilities for both subscriptions (canonical names)
            initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string(), "INPUT_EVENTS".to_string()],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let established_msg = response_stream.next().await.unwrap().unwrap();
    let resume_token = match &established_msg.payload {
        Some(ServerPayload::SessionEstablished(e)) => e.resume_token.clone(),
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    };

    drop(tx);
    drop(response_stream);
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    // Now resume.
    let (rtx, rrx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let rstream = tokio_stream::wrappers::ReceiverStream::new(rrx);

    rtx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionResume(SessionResume {
            agent_id: "sub-resume-agent".to_string(),
            resume_token,
            last_seen_server_sequence: 2,
            pre_shared_key: "test-key".to_string(),
            auth_credential: None,
        })),
    })
    .await
    .unwrap();

    let mut rs = client.session(rstream).await.unwrap().into_inner();
    let resume_result_msg = rs.next().await.unwrap().unwrap();
    match &resume_result_msg.payload {
        Some(ServerPayload::SessionResumeResult(result)) => {
            assert!(result.accepted);
            // Capabilities must be restored.
            // Subscriptions must be restored.
            assert!(
                result
                    .active_subscriptions
                    .contains(&"SCENE_TOPOLOGY".to_string()),
                "SCENE_TOPOLOGY subscription must be present in resume result"
            );
            assert!(
                result
                    .active_subscriptions
                    .contains(&"INPUT_EVENTS".to_string()),
                "INPUT_EVENTS subscription must be present in resume result"
            );
        }
        other => panic!("Expected SessionResumeResult, got: {other:?}"),
    }
}

// ─── DegradationNotice tests (RFC 0005 §3.4, §7.1) ───────────────────────

/// traffic_class: DegradationNotice must be Transactional (RFC 0005 §3.4).
#[test]
fn test_degradation_notice_is_transactional() {
    assert_eq!(
        classify_server_payload(&ServerPayload::DegradationNotice(
            DegradationNotice::default()
        )),
        TrafficClass::Transactional,
        "DegradationNotice must be Transactional — never dropped"
    );
}

#[tokio::test]
async fn new_session_receives_existing_degradation_after_snapshot() {
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");
    service
        .degradation_notices
        .publish(DegradationNotice {
            level: DegradationLevel::RenderingSimplified as i32,
            reason: "existing load".to_string(),
            timestamp_wall_us: now_wall_us(),
        })
        .await;

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let mut client = connect_test_client_with_retry(addr.port()).await;
    let (_tx, messages, _stream) = handshake(&mut client, "degraded-new-agent", "test-key").await;

    assert!(matches!(
        messages[0].payload,
        Some(ServerPayload::SessionEstablished(_))
    ));
    assert!(matches!(
        messages[1].payload,
        Some(ServerPayload::SceneSnapshot(_))
    ));
    match &messages[2].payload {
        Some(ServerPayload::DegradationNotice(notice)) => {
            assert_eq!(notice.level, DegradationLevel::RenderingSimplified as i32);
        }
        other => panic!("expected current degradation third, got {other:?}"),
    }
}

/// Scenario: WHEN runtime enters COALESCING_MORE degradation level,
/// THEN all active sessions receive DegradationNotice unconditionally.
#[tokio::test]
async fn test_degradation_notice_broadcast_to_active_session() {
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");
    let degradation_notices = service.degradation_notices.clone();

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _server = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let mut client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    let (tx, _init_messages, mut stream) = handshake(&mut client, "degrad-agent", "test-key").await;

    // Give the session task a brief moment to subscribe to the broadcast channel.
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Broadcast a RENDERING_SIMPLIFIED degradation notice from the "compositor side".
    let notice = DegradationNotice {
        level: DegradationLevel::RenderingSimplified as i32,
        reason: "high load".to_string(),
        timestamp_wall_us: now_wall_us(),
    };
    assert_eq!(degradation_notices.publish(notice.clone()).await, 1);

    // The session should receive DegradationNotice next.
    let timeout = tokio::time::Duration::from_millis(500);
    let msg = tokio::time::timeout(timeout, stream.next())
        .await
        .expect("timeout waiting for DegradationNotice")
        .expect("stream ended")
        .expect("stream error");

    match &msg.payload {
        Some(ServerPayload::DegradationNotice(dn)) => {
            assert_eq!(
                dn.level,
                DegradationLevel::RenderingSimplified as i32,
                "Expected RENDERING_SIMPLIFIED"
            );
            assert_eq!(dn.reason, "high load");
        }
        other => panic!("Expected DegradationNotice, got: {other:?}"),
    }

    drop(tx);
}

// ─── Deduplication tests (RFC 0005 §5.2) ─────────────────────────────────

/// Scenario: duplicate batch_id within window returns cached MutationResult.
#[tokio::test]
async fn test_mutation_dedup_returns_cached_result() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "dedup-agent", "test-key").await;

    // Obtain a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let lease_msg = stream.next().await.unwrap().unwrap();
    let lease_id = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Send first MutationBatch with a unique batch_id
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: None,
        })),
    })
    .await
    .unwrap();
    let first_result = next_server_msg(&mut stream).await;
    let first_accepted = match &first_result.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert_eq!(r.batch_id, batch_id);
            r.ok
        }
        other => panic!("Expected MutationResult, got: {other:?}"),
    };

    // Retransmit with the same batch_id but a new sequence number
    tx.send(ClientMessage {
        sequence: 4, // new sequence
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(), // same batch_id
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: None,
        })),
    })
    .await
    .unwrap();
    let dedup_result = stream.next().await.unwrap().unwrap();
    match &dedup_result.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert_eq!(
                r.batch_id, batch_id,
                "batch_id must be echoed from cached result"
            );
            assert_eq!(
                r.ok, first_accepted,
                "Dedup must return cached accepted flag"
            );
        }
        other => panic!("Expected cached MutationResult on retransmit, got: {other:?}"),
    }

    drop(tx);
}

// ─── TimingHints validation tests (RFC 0003 §3.5, RFC 0005 §3.3) ─────────

/// Unit test for validate_timing_hints: TIMESTAMP_TOO_OLD.
#[test]
fn test_timing_hints_too_old() {
    // present_at_wall_us = session_open - 61 seconds → TIMESTAMP_TOO_OLD
    let session_open = 200_000_000u64; // arbitrary µs baseline
    let present = session_open - 61_000_001; // > 60s before session open
    let hints = TimingHints {
        present_at_wall_us: present,
        expires_at_wall_us: 0,
    };
    let result = validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US);
    assert!(result.is_err());
    let (code, _) = result.unwrap_err();
    assert_eq!(code, "TIMESTAMP_TOO_OLD");
}

/// Unit test for validate_timing_hints: TIMESTAMP_TOO_FUTURE.
#[test]
fn test_timing_hints_too_future() {
    let session_open = now_wall_us();
    let max_future = DEFAULT_MAX_FUTURE_SCHEDULE_US;
    // Use session_open as baseline and a large margin (1 full second) to avoid
    // flakiness from the µs gap between now_wall_us() calls.
    // present must exceed current_wall_us + max_future, where current_wall_us is
    // re-sampled inside validate_timing_hints. The 1-second buffer ensures the
    // margin holds even under scheduler jitter.
    let present = session_open + max_future + 1_000_000; // 1s beyond horizon
    let hints = TimingHints {
        present_at_wall_us: present,
        expires_at_wall_us: 0,
    };
    let result = validate_timing_hints(&hints, session_open, max_future);
    assert!(result.is_err());
    let (code, _) = result.unwrap_err();
    assert_eq!(code, "TIMESTAMP_TOO_FUTURE");
}

/// Unit test for validate_timing_hints: TIMESTAMP_EXPIRY_BEFORE_PRESENT.
#[test]
fn test_timing_hints_expiry_before_present() {
    let session_open = now_wall_us().saturating_sub(1_000_000); // 1s ago
    let now = now_wall_us();
    let present = now + 1_000_000; // 1s in future (valid range)
    let expires = present - 1; // expires before present → invalid
    let hints = TimingHints {
        present_at_wall_us: present,
        expires_at_wall_us: expires,
    };
    let result = validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US);
    assert!(result.is_err());
    let (code, _) = result.unwrap_err();
    assert_eq!(code, "TIMESTAMP_EXPIRY_BEFORE_PRESENT");
}

/// Unit test for validate_timing_hints: valid future scheduling (present_at in future).
#[test]
fn test_timing_hints_valid_future() {
    let session_open = now_wall_us().saturating_sub(1_000_000); // 1s ago
    let now = now_wall_us();
    let present = now + 500_000; // 500ms in the future (well within 5 min)
    let expires = present + 2_000_000; // 2s after present → valid
    let hints = TimingHints {
        present_at_wall_us: present,
        expires_at_wall_us: expires,
    };
    assert!(
        validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US).is_ok(),
        "Valid future TimingHints should not be rejected"
    );
}

/// Unit test for validate_timing_hints: zero fields bypass validation.
#[test]
fn test_timing_hints_zero_bypasses_validation() {
    let session_open = now_wall_us();
    let hints = TimingHints {
        present_at_wall_us: 0,
        expires_at_wall_us: 0,
    };
    assert!(
        validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US).is_ok(),
        "Zero TimingHints should always be valid"
    );
}

/// Integration test: MutationBatch with TIMESTAMP_TOO_OLD is rejected via stream.
#[tokio::test]
async fn test_mutation_timing_too_old_rejected() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "timing-old-agent", "test-key").await;

    // Get a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let lease_msg = stream.next().await.unwrap().unwrap();
    let lease_id = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Send a mutation with present_at more than 60s before epoch 0 (which means
    // it's more than 60s before session open; session opened near now_wall_us(),
    // so session_open - 60s - 1 ≫ 0 for any real timestamp).
    //
    // Use present_at = 1 µs since epoch — guaranteed to be older than
    // session_open_at_wall_us - 60_000_000.
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: Some(TimingHints {
                present_at_wall_us: 1, // far in the past
                expires_at_wall_us: 0,
            }),
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(err)) => {
            assert_eq!(err.code, "TIMESTAMP_TOO_OLD");
        }
        other => panic!("Expected RuntimeError(TIMESTAMP_TOO_OLD), got: {other:?}"),
    }

    drop(tx);
}

/// Integration test: MutationBatch with TIMESTAMP_EXPIRY_BEFORE_PRESENT is rejected.
#[tokio::test]
async fn test_mutation_timing_expiry_before_present_rejected() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "timing-exp-agent", "test-key").await;

    // Get a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let lease_msg = stream.next().await.unwrap().unwrap();
    let lease_id = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    let now = now_wall_us();
    let present = now + 500_000; // 500ms in future
    let expires = present - 1; // expires 1µs before present → invalid

    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: Some(TimingHints {
                present_at_wall_us: present,
                expires_at_wall_us: expires,
            }),
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(err)) => {
            assert_eq!(err.code, "TIMESTAMP_EXPIRY_BEFORE_PRESENT");
        }
        other => {
            panic!("Expected RuntimeError(TIMESTAMP_EXPIRY_BEFORE_PRESENT), got: {other:?}")
        }
    }

    drop(tx);
}

// ─── Zone durability tests (RFC 0005 §3.1, §8.6) ─────────────────────────

/// Scenario: Ephemeral zone publish is fire-and-forget — no ZonePublishResult.
/// WHEN agent publishes to an ephemeral zone (zone.ephemeral=true)
/// THEN runtime does NOT send a ZonePublishResult (spec lines 624-626)
#[tokio::test]
async fn test_ephemeral_zone_no_publish_result() {
    use tze_hud_scene::types::{
        ContentionPolicy, GeometryPolicy, LayerAttachment, RenderingPolicy, ZoneDefinition,
        ZoneMediaType,
    };
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");

    // Register an ephemeral zone in the scene
    {
        let st = service.state.lock().await;
        st.scene
            .lock()
            .await
            .zone_registry
            .register(ZoneDefinition {
                id: tze_hud_scene::SceneId::new(),
                name: "live-caption".to_string(),
                description: "Ephemeral caption zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.1,
                    y_pct: 0.8,
                    width_pct: 0.8,
                    height_pct: 0.1,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 1,
                auto_clear_ms: None,
                ephemeral: true, // <-- ephemeral zone
                layer_attachment: LayerAttachment::Content,
            });
    }

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(crate::proto::session::hud_session_server::HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let mut client = crate::proto::session::hud_session_client::HudSessionClient::connect(format!(
        "http://[::1]:{}",
        addr.port()
    ))
    .await
    .unwrap();

    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "ephemeral-publisher", "test-key").await;

    // Publish to the ephemeral zone
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "zone:live-caption".to_string(),
            content: Some(crate::proto::ZoneContent {
                payload: Some(crate::proto::zone_content::Payload::StreamText(
                    "caption text".to_string(),
                )),
            }),
            ttl_ms: 0,
            key: String::new(),
            breakpoints: Vec::new(),
            present_at_us: 0,
            expires_at_us: 0,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    // Send a heartbeat so we can verify the next message is a heartbeat echo
    // (meaning no ZonePublishResult was sent for the ephemeral zone publish)
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 99999,
        })),
    })
    .await
    .unwrap();

    // The first message after the ephemeral zone publish should be the heartbeat echo,
    // NOT a ZonePublishResult (ephemeral zones are fire-and-forget)
    let next_msg = stream.next().await.unwrap().unwrap();
    match &next_msg.payload {
        Some(ServerPayload::RequestResult(_)) => {
            panic!("Ephemeral zone publish must NOT produce a ZonePublishResult")
        }
        Some(ServerPayload::Heartbeat(hb)) => {
            assert_eq!(hb.timestamp_mono_us, 99999, "expected heartbeat echo");
        }
        other => panic!("Expected Heartbeat echo (no ZonePublishResult), got: {other:?}"),
    }
    drop(handle);
}

/// Scenario: Durable zone publish is acknowledged (RFC 0005 §3.1, spec lines 620-622).
/// WHEN agent publishes to a durable zone (zone.ephemeral=false)
/// THEN runtime sends a ZonePublishResult.
#[tokio::test]
async fn test_durable_zone_publish_result() {
    use tze_hud_scene::types::{
        ContentionPolicy, GeometryPolicy, LayerAttachment, RenderingPolicy, ZoneDefinition,
        ZoneMediaType,
    };
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");

    // Register a durable zone
    {
        let st = service.state.lock().await;
        st.scene
            .lock()
            .await
            .zone_registry
            .register(ZoneDefinition {
                id: tze_hud_scene::SceneId::new(),
                name: "status-text".to_string(),
                description: "Durable status text zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 1.0,
                    height_pct: 0.05,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 4,
                auto_clear_ms: None,
                ephemeral: false, // <-- durable zone
                layer_attachment: LayerAttachment::Content,
            });
    }

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(crate::proto::session::hud_session_server::HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let mut client = crate::proto::session::hud_session_client::HudSessionClient::connect(format!(
        "http://[::1]:{}",
        addr.port()
    ))
    .await
    .unwrap();

    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "durable-publisher", "test-key").await;

    let client_seq: u64 = 2;
    tx.send(ClientMessage {
        sequence: client_seq,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "zone:status-text".to_string(),
            content: Some(crate::proto::ZoneContent {
                payload: Some(crate::proto::zone_content::Payload::StreamText(
                    "status: ok".to_string(),
                )),
            }),
            ttl_ms: 0,
            key: String::new(),
            breakpoints: Vec::new(),
            present_at_us: 0,
            expires_at_us: 0,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    // Durable zone: should receive ZonePublishResult
    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert_eq!(result.seq, client_seq);
            assert!(result.ok, "durable zone publish should be accepted");
        }
        other => panic!("Expected ZonePublishResult for durable zone, got: {other:?}"),
    }
    drop(handle);
}

// ─── Input control tests (RFC 0005 §3.8) ─────────────────────────────────

/// Scenario: InputFocusRequest → InputFocusResponse (synchronous, correlated by sequence).
/// WHEN agent sends InputFocusRequest at sequence N,
/// THEN runtime responds with InputFocusResponse (spec lines 567-569).
#[tokio::test]
async fn test_input_focus_request_response() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "focus-agent", "test-key").await;

    let tile_id_bytes = vec![1u8; 16];
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::InputFocusRequest(InputFocusRequest {
            tile_id: tile_id_bytes.clone(),
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::InputFocusResponse(resp)) => {
            assert_eq!(resp.tile_id, tile_id_bytes, "tile_id must match request");
            assert!(resp.granted, "focus should be granted in v1");
        }
        other => panic!("Expected InputFocusResponse, got: {other:?}"),
    }
}

/// Scenario: InputCaptureRequest → InputCaptureResponse (synchronous).
#[tokio::test]
async fn test_input_capture_request_response() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "capture-agent", "test-key").await;

    let tile_id_bytes = vec![2u8; 16];
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::InputCaptureRequest(InputCaptureRequest {
            tile_id: tile_id_bytes.clone(),
            device_kind: "pointer".to_string(),
            node_id: Vec::new(),
            device_id: String::new(),
            release_on_up: false,
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::InputCaptureResponse(resp)) => {
            assert_eq!(resp.tile_id, tile_id_bytes, "tile_id must match request");
            assert_eq!(resp.device_kind, "pointer");
            assert!(resp.granted, "capture should be granted in v1");
        }
        other => panic!("Expected InputCaptureResponse, got: {other:?}"),
    }
}

/// Scenario: InputCaptureRequest wires through to the runtime input processor bridge.
#[tokio::test]
async fn test_input_capture_request_sends_runtime_command() {
    let (mut client, _server, mut capture_rx, tile_id, node_id) =
        setup_test_with_input_capture_channel(
            tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        )
        .await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "capture-agent", "test-key").await;

    let tile_id_bytes = scene_id_to_bytes(tile_id);
    let node_id_bytes = scene_id_to_bytes(node_id);

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::InputCaptureRequest(InputCaptureRequest {
            tile_id: tile_id_bytes.clone(),
            device_kind: "pointer".to_string(),
            node_id: node_id_bytes,
            device_id: "7".to_string(),
            release_on_up: true,
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::InputCaptureResponse(resp)) => {
            assert_eq!(resp.tile_id, tile_id_bytes, "tile_id must match request");
            assert!(resp.granted, "capture bridge should accept valid request");
        }
        other => panic!("Expected InputCaptureResponse, got: {other:?}"),
    }

    let command = capture_rx
        .recv()
        .await
        .expect("capture command must be sent");
    assert_eq!(
        command,
        crate::session::InputCaptureCommand::Request {
            tile_id,
            node_id,
            device_id: 7,
            release_on_up: true,
        }
    );
}

#[tokio::test]
async fn input_capture_bridge_wakes_only_after_successful_command_enqueue() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let wakes = Arc::new(AtomicU64::new(0));
    let callback_wakes = Arc::clone(&wakes);
    let notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_wakes.fetch_add(1, Ordering::AcqRel);
    });
    let (mut client, _server, mut capture_rx, tile_id, node_id) =
        setup_test_with_input_capture_channel(notifier).await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "capture-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::InputCaptureRequest(InputCaptureRequest {
            tile_id: scene_id_to_bytes(tile_id),
            device_kind: "pointer".to_string(),
            node_id: scene_id_to_bytes(node_id),
            device_id: "7".to_string(),
            release_on_up: true,
        })),
    })
    .await
    .unwrap();
    let response = next_server_msg(&mut stream).await;
    assert!(matches!(
        response.payload,
        Some(ServerPayload::InputCaptureResponse(InputCaptureResponse {
            granted: true,
            ..
        }))
    ));
    assert!(matches!(
        capture_rx.recv().await,
        Some(crate::session::InputCaptureCommand::Request { .. })
    ));
    assert_eq!(wakes.load(Ordering::Acquire), 1);

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::InputCaptureRelease(InputCaptureRelease {
            tile_id: scene_id_to_bytes(tile_id),
            device_kind: "pointer".to_string(),
            device_id: "7".to_string(),
        })),
    })
    .await
    .unwrap();
    assert!(matches!(
        capture_rx.recv().await,
        Some(crate::session::InputCaptureCommand::Release { device_id: 7 })
    ));
    assert_eq!(wakes.load(Ordering::Acquire), 2);

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::InputCaptureRelease(InputCaptureRelease {
            tile_id: scene_id_to_bytes(tile_id),
            device_kind: "pointer".to_string(),
            device_id: "invalid".to_string(),
        })),
    })
    .await
    .unwrap();
    let rejected = next_server_msg(&mut stream).await;
    assert!(matches!(
        rejected.payload,
        Some(ServerPayload::RequestResult(_))
    ));
    assert_eq!(wakes.load(Ordering::Acquire), 2);

    drop(capture_rx);
    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::InputCaptureRequest(InputCaptureRequest {
            tile_id: scene_id_to_bytes(tile_id),
            device_kind: "pointer".to_string(),
            node_id: scene_id_to_bytes(node_id),
            device_id: "8".to_string(),
            release_on_up: true,
        })),
    })
    .await
    .unwrap();
    let unavailable = next_server_msg(&mut stream).await;
    assert!(matches!(
        unavailable.payload,
        Some(ServerPayload::InputCaptureResponse(InputCaptureResponse {
            granted: false,
            ..
        }))
    ));
    assert_eq!(wakes.load(Ordering::Acquire), 2);
}

/// Scenario: malformed capture-release device ids are reported to the caller.
#[tokio::test]
async fn test_input_capture_release_rejects_invalid_device_id() {
    let (mut client, _server, mut capture_rx, tile_id, _node_id) =
        setup_test_with_input_capture_channel(
            tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        )
        .await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "capture-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::InputCaptureRelease(InputCaptureRelease {
            tile_id: scene_id_to_bytes(tile_id),
            device_kind: "pointer".to_string(),
            device_id: "not-a-u32".to_string(),
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::RequestResult(err)) => {
            assert_eq!(err.code, "INVALID_ARGUMENT");
            assert!(
                err.hint.contains("invalid pointer device_id"),
                "error should name the malformed device id, got: {}",
                err.hint
            );
        }
        other => panic!("Expected RuntimeError, got: {other:?}"),
    }
    assert!(
        capture_rx.try_recv().is_err(),
        "invalid release must not enqueue a runtime capture command"
    );
}

/// Scenario: InputCaptureRelease → CaptureReleasedEvent in EventBatch (asynchronous).
/// WHEN agent sends InputCaptureRelease (field 29) for a captured device
/// THEN runtime delivers CaptureReleasedEvent in EventBatch (field 34), reason=AGENT_RELEASED
/// (spec lines 571-573). Only delivered if agent has FOCUS_EVENTS subscription.
#[tokio::test]
async fn test_input_capture_release_delivers_event() {
    let (mut client, _server) = setup_test().await;

    // Use a custom handshake with access_input_events (needed for FOCUS_EVENTS sub)
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream_rx = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "capture-release-agent".to_string(),
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
            initial_subscriptions: vec!["INPUT_EVENTS".to_string(), "FOCUS_EVENTS".to_string()],
            resume_token: Vec::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream_rx).await.unwrap().into_inner();

    // Drain SessionEstablished, SceneSnapshot, and current degradation state.
    for _ in 0..3 {
        let _ = response_stream.next().await;
    }

    // Send InputCaptureRelease
    let tile_id_bytes = vec![3u8; 16];
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::InputCaptureRelease(InputCaptureRelease {
            tile_id: tile_id_bytes.clone(),
            device_kind: "pointer".to_string(),
            device_id: String::new(),
        })),
    })
    .await
    .unwrap();

    // Should receive EventBatch with CaptureReleasedEvent
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::EventBatch(batch)) => {
            assert_eq!(batch.events.len(), 1, "should have exactly one event");
            match &batch.events[0].event {
                Some(crate::proto::input_envelope::Event::CaptureReleased(ev)) => {
                    assert_eq!(
                        ev.tile_id, tile_id_bytes,
                        "tile_id must match release request"
                    );
                    assert_eq!(
                        ev.reason,
                        crate::proto::CaptureReleasedReason::AgentReleased as i32,
                        "reason must be AGENT_RELEASED"
                    );
                    assert!(
                        ev.timestamp_mono_us > 0,
                        "synthetic capture-release fallback must carry a monotonic timestamp"
                    );
                }
                other => panic!("Expected CaptureReleasedEvent, got: {other:?}"),
            }
        }
        other => panic!("Expected EventBatch with CaptureReleasedEvent, got: {other:?}"),
    }
    drop(tx);
}

// ─── Lease management tests (rig-7bho) ───────────────────────────────────

/// Scenario: Lease acquisition via session stream (spec §Lease Management RPCs,
/// lease-governance spec §Lease State Machine).
///
/// WHEN agent sends LeaseRequest(action=ACQUIRE) on session stream,
/// THEN runtime responds with a single LeaseResponse(granted=true).
#[tokio::test]
async fn test_lease_acquire_sends_lease_response() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "lease-acquire-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 30_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    // First response: LeaseResponse(granted=true)
    let resp_msg = stream.next().await.unwrap().unwrap();
    match &resp_msg.payload {
        Some(ServerPayload::RequestResult(resp)) => {
            assert!(resp.ok, "Lease should be granted");
            assert_eq!(resp.lease_id.len(), 16, "lease_id must be 16-byte UUIDv7");
            assert_eq!(resp.ttl_ms, 30_000);
        }
        other => panic!("Expected LeaseResponse, got: {other:?}"),
    }
}

/// Scenario: lease_id is always a 16-byte UUIDv7 (SceneId spec §SceneId for Scene-Object Identifiers).
///
/// WHEN agent requests a lease,
/// THEN all lease_id fields in responses are exactly 16 bytes.
#[tokio::test]
async fn test_lease_id_is_16_byte_uuidv7() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "sceneid-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 10_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    // LeaseResponse
    let resp_msg = stream.next().await.unwrap().unwrap();
    match &resp_msg.payload {
        Some(ServerPayload::RequestResult(resp)) => {
            assert!(resp.ok);
            assert_eq!(
                resp.lease_id.len(),
                16,
                "lease_id in LeaseResponse must be 16 bytes (SceneId UUIDv7)"
            );
        }
        other => panic!("Expected LeaseResponse, got: {other:?}"),
    }
}

/// Scenario: Retransmit correlation — sending a lease request with the same
/// client sequence number returns the cached response (RFC 0005 §5.3).
///
/// The server must detect retransmits (same sequence) and replay the response
/// without re-applying the operation.
#[tokio::test]
async fn test_lease_retransmit_correlation_returns_cached_response() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "retransmit-agent", "test-key").await;

    let lease_req = ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 30_000,
            ..Default::default()
        })),
    };

    // Original request
    tx.send(lease_req.clone()).await.unwrap();

    // Consume the original LeaseResponse
    let orig_resp = stream.next().await.unwrap().unwrap();
    let orig_lease_id = match &orig_resp.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert!(r.ok);
            r.lease_id.clone()
        }
        other => panic!("Expected LeaseResponse, got: {other:?}"),
    };

    // Retransmit with same sequence number (simulates no-ack / lost response)
    tx.send(lease_req).await.unwrap();

    // The retransmit should return the cached LeaseResponse (no duplicate lease created)
    let retx_resp = stream.next().await.unwrap().unwrap();
    match &retx_resp.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert!(r.ok, "Retransmit should return cached grant");
            assert_eq!(
                r.lease_id, orig_lease_id,
                "Retransmit must return the same lease_id as the original response"
            );
            assert_eq!(r.ttl_ms, 30_000);
        }
        other => panic!("Expected LeaseResponse on retransmit, got: {other:?}"),
    }
}

/// Scenario: Three agents contending for leases.
///
/// Validates concurrent lease acquisition: all three agents can independently
/// acquire leases from the same runtime with unique lease IDs.
#[tokio::test]
async fn test_three_agents_lease_contention() {
    let (client1, _server) = setup_test().await;

    // Use a single shared server — connect 3 clients to the same port.
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let url = format!("http://[::1]:{}", addr.port());
    let mut c1 = HudSessionClient::connect(url.clone()).await.unwrap();
    let mut c2 = HudSessionClient::connect(url.clone()).await.unwrap();
    let mut c3 = HudSessionClient::connect(url.clone()).await.unwrap();

    let (tx1, _, mut s1) = handshake(&mut c1, "agent-alpha", "test-key").await;
    let (tx2, _, mut s2) = handshake(&mut c2, "agent-beta", "test-key").await;
    let (tx3, _, mut s3) = handshake(&mut c3, "agent-gamma", "test-key").await;

    // All three agents request leases concurrently (sequential sends for simplicity)
    for (tx, seq) in [(&tx1, 2u64), (&tx2, 2u64), (&tx3, 2u64)] {
        tx.send(ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::ClaimTile(ClaimTile {
                ttl_ms: 30_000,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
    }

    // Collect lease IDs
    let mut lease_ids = Vec::new();
    for stream in [&mut s1, &mut s2, &mut s3] {
        let msg = stream.next().await.unwrap().unwrap();
        match &msg.payload {
            Some(ServerPayload::RequestResult(r)) => {
                assert!(r.ok, "All agents should get leases granted");
                assert_eq!(r.lease_id.len(), 16);
                lease_ids.push(r.lease_id.clone());
            }
            other => panic!("Expected LeaseResponse, got: {other:?}"),
        }
    }

    // All lease IDs must be unique — use a HashSet for correct deduplication.
    let set: std::collections::HashSet<Vec<u8>> = lease_ids.iter().cloned().collect();
    assert_eq!(
        set.len(),
        3,
        "All three agents must receive unique lease IDs"
    );

    drop(client1);
}

/// Scenario: Lease expiry — runtime accepts a lease with a very short TTL.
///
/// This test verifies that the protocol accepts LeaseRequest with any valid TTL,
/// including very short ones used in expiry scenarios.
/// Full expiry notification behavior requires the timer loop (post-v1 scope for
/// push notifications); here we verify the initial grant succeeds and the correct
/// SceneId is returned.
#[tokio::test]
async fn test_lease_expiry_scenario_initial_grant() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "expiry-agent", "test-key").await;

    // Request a lease with a very short TTL (100ms — represents expiry scenario)
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 100,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let resp = stream.next().await.unwrap().unwrap();
    match &resp.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert!(r.ok);
            assert_eq!(
                r.ttl_ms, 100,
                "Short-TTL lease should be granted as requested"
            );
            assert_eq!(r.lease_id.len(), 16, "lease_id must be 16-byte SceneId");
        }
        other => panic!("Expected LeaseResponse for short-TTL lease, got: {other:?}"),
    }
}

/// LeaseResponse is Transactional (never dropped under backpressure).
#[test]
fn test_lease_response_is_transactional() {
    assert_eq!(
        classify_server_payload(&ServerPayload::RequestResult(RequestResult::default())),
        TrafficClass::Transactional,
        "LeaseResponse must be Transactional (never dropped)"
    );
}

/// Scenario: Disconnect orphan behavior — session cleanup does not panic
/// when leases are held.
///
/// WHEN an agent with active leases disconnects ungracefully,
/// THEN the session is removed from the registry without error.
///
/// Full orphan-to-expiry lifecycle requires a timer loop (post-v1); this test
/// verifies the session teardown path is safe when leases are present.
#[tokio::test]
async fn test_disconnect_with_active_leases_no_panic() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "disconnect-agent", "test-key").await;

    // Acquire a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    // Consume LeaseResponse
    let _r = stream.next().await.unwrap().unwrap();

    // Drop both tx and stream to simulate ungraceful disconnect
    drop(tx);
    drop(stream);

    // Give the server task time to clean up
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    // If we reach here without a panic, the cleanup path is safe.
}

// ─── Live capability revocation tests (RFC 0001 §3.3, GAP-G3-4) ────────────

// ─── Widget publish tests (widget-system spec §Requirement: Widget Publishing via gRPC) ──

/// Helper: create a test service with a durable widget registered.
async fn setup_widget_service() -> HudSessionImpl {
    use tze_hud_scene::types::{
        ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetDefinition, WidgetInstance,
        WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue, WidgetSvgLayer,
    };

    let scene = SceneGraph::new(800.0, 600.0);
    let service =
        HudSessionImpl::new(scene, "test-key").with_agent_permissions(restricted_test_agents());
    {
        let st = service.state.lock().await;
        let mut s = st.scene.lock().await;

        // Register a durable widget type "gauge"
        s.widget_registry.register_definition(WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "A simple gauge widget".to_string(),
            parameter_schema: vec![WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            }],
            layers: vec![WidgetSvgLayer {
                svg_file: "fill.svg".to_string(),
                bindings: vec![],
            }],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 0.1,
                height_pct: 0.1,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false, // durable
            hover_behavior: None,
        });

        // Create a tab and widget instance
        let tab_id = s.create_tab("main", 0).unwrap();
        s.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "gauge".to_string(),
            current_params: std::collections::HashMap::new(),
        });
    }
    service
}

/// Helper: start a server with a widget service and connect.
async fn setup_widget_test() -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let service = setup_widget_service().await;
    setup_widget_test_with_service(service).await
}

/// Helper: start a server with a widget service and explicit resident upload rate limit.
async fn setup_widget_test_with_upload_rate_limit(
    upload_rate_limit_bytes_per_sec: usize,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let service = setup_widget_service().await;
    {
        let mut st = service.state.lock().await;
        st.resource_store =
            tze_hud_resource::ResourceStore::new(tze_hud_resource::ResourceStoreConfig {
                upload_rate_limit_bytes_per_sec,
                ..tze_hud_resource::ResourceStoreConfig::default()
            });
    }
    setup_widget_test_with_service(service).await
}

/// Helper: start a server with a widget service using explicit asset-store limits.
async fn setup_widget_test_with_asset_limits(
    max_total_bytes: u64,
    max_namespace_bytes: u64,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let service = setup_widget_service().await;
    {
        let mut st = service.state.lock().await;
        st.widget_asset_store =
            crate::session::WidgetAssetStore::new_with_limits(max_total_bytes, max_namespace_bytes);
    }
    setup_widget_test_with_service(service).await
}

/// Helper: start a server with a durable runtime widget store.
async fn setup_widget_test_with_durable_store(
    store_path: std::path::PathBuf,
    max_total_bytes: u64,
    max_agent_bytes: u64,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let service = setup_widget_service().await;
    {
        let mut st = service.state.lock().await;
        st.runtime_widget_store = Some(
            tze_hud_resource::RuntimeWidgetStore::open(
                tze_hud_resource::RuntimeWidgetStoreConfig {
                    store_path,
                    max_total_bytes,
                    max_agent_bytes,
                },
            )
            .expect("durable runtime widget store should open for tests"),
        );
    }
    setup_widget_test_with_service(service).await
}

async fn setup_widget_test_with_service(
    service: HudSessionImpl,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    (client, handle)
}

/// Scenario: Durable WidgetPublish with valid params receives WidgetPublishResult(accepted=true).
#[tokio::test]
async fn test_durable_widget_publish_receives_result() {
    let (mut client, _handle) = setup_widget_test().await;

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "widget-agent",
        "test-key",
        &["publish_widget:gauge"],
    )
    .await;

    // Send a WidgetPublish for the durable "gauge" widget
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![crate::proto::WidgetParameterValueProto {
                param_name: "level".to_string(),
                value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                    0.75,
                )),
            }],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(
                result.ok,
                "Durable widget publish must be accepted, got error: {}",
                result.code
            );
            assert!(result.code.is_empty(), "No error code on success");
            assert_eq!(result.seq, 2, "request_sequence must echo client sequence");
        }
        other => panic!("Expected WidgetPublishResult, got: {other:?}"),
    }

    drop(tx);
}

/// Scenario: WidgetPublish with missing capability receives WIDGET_CAPABILITY_MISSING.
#[tokio::test]
async fn test_widget_publish_missing_capability_rejected() {
    let (mut client, _handle) = setup_widget_test().await;

    // Handshake WITHOUT publish_widget:gauge capability
    let (tx, _init_msgs, mut stream) =
        handshake(&mut client, "widget-no-cap-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(!result.ok, "Expected rejection");
            assert_eq!(
                result.code, "NOT_ALLOWED",
                "Expected NOT_ALLOWED, got: {}",
                result.code
            );
        }
        other => panic!("Expected WidgetPublishResult(rejected), got: {other:?}"),
    }

    drop(tx);
}

/// Scenario: wildcard publish_widget capability authorizes any widget publish.
#[tokio::test]
async fn test_widget_publish_wildcard_capability_allows_publish() {
    let (mut client, _handle) = setup_widget_test().await;

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "widget-wildcard-agent",
        "test-key",
        &["publish_widget:*"],
    )
    .await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(
                result.ok,
                "Expected wildcard capability to authorize publish"
            );
        }
        other => panic!("Expected WidgetPublishResult, got: {other:?}"),
    }

    drop(tx);
}

/// Scenario: WidgetPublish targeting unknown widget receives WIDGET_NOT_FOUND.
#[tokio::test]
async fn test_widget_publish_not_found() {
    let (mut client, _handle) = setup_widget_test().await;

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "widget-notfound-agent",
        "test-key",
        &["publish_widget:nonexistent"],
    )
    .await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:nonexistent".to_string(),
            params: vec![],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(!result.ok, "Expected rejection");
            assert_eq!(
                result.code, "WIDGET_NOT_FOUND",
                "Expected WIDGET_NOT_FOUND, got: {}",
                result.code
            );
        }
        other => panic!("Expected WidgetPublishResult(WIDGET_NOT_FOUND), got: {other:?}"),
    }

    drop(tx);
}

/// Scenario: WidgetPublish with unknown parameter receives WIDGET_UNKNOWN_PARAMETER.
#[tokio::test]
async fn test_widget_publish_unknown_parameter() {
    let (mut client, _handle) = setup_widget_test().await;

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "widget-badparam-agent",
        "test-key",
        &["publish_widget:gauge"],
    )
    .await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![crate::proto::WidgetParameterValueProto {
                param_name: "bogus_param".to_string(),
                value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                    0.5,
                )),
            }],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(!result.ok, "Expected rejection");
            assert_eq!(
                result.code, "WIDGET_PARAMETER_INVALID",
                "Expected WIDGET_PARAMETER_INVALID, got: {}",
                result.code
            );
        }
        other => {
            panic!("Expected WidgetPublishResult(WIDGET_UNKNOWN_PARAMETER), got: {other:?}")
        }
    }

    drop(tx);
}

/// Scenario: repeated durable WidgetPublish requests to the same widget are
/// unambiguously correlated by request_sequence.
#[tokio::test]
async fn test_durable_widget_publish_repeated_requests_are_correlated() {
    let (mut client, _handle) = setup_widget_test().await;

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "widget-correlation-agent",
        "test-key",
        &["publish_widget:gauge"],
    )
    .await;

    for (sequence, level) in [(2u64, 0.25f32), (3u64, 0.75f32)] {
        tx.send(ClientMessage {
            sequence,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::Publish(Publish {
                surface: "widget:gauge".to_string(),
                params: vec![crate::proto::WidgetParameterValueProto {
                    param_name: "level".to_string(),
                    value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                        level,
                    )),
                }],
                transition_ms: 0,
                key: String::new(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();

        let result_msg = next_server_msg(&mut stream).await;
        match &result_msg.payload {
            Some(ServerPayload::RequestResult(result)) => {
                assert_eq!(result.seq, sequence);
                assert!(result.ok, "expected durable publish to be accepted");
                assert!(result.code.is_empty());
                assert!(result.hint.is_empty());
            }
            other => panic!("Expected WidgetPublishResult, got: {other:?}"),
        }
    }

    drop(tx);
}

/// Scenario: Ephemeral WidgetPublish is fire-and-forget (no WidgetPublishResult).
#[tokio::test]
async fn test_ephemeral_widget_no_publish_result() {
    use tze_hud_scene::types::{
        ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetDefinition, WidgetInstance,
        WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue,
    };

    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");
    {
        let st = service.state.lock().await;
        let mut s = st.scene.lock().await;

        // Register an EPHEMERAL widget type
        s.widget_registry.register_definition(WidgetDefinition {
            id: "live-bar".to_string(),
            name: "LiveBar".to_string(),
            description: "Ephemeral bar widget".to_string(),
            parameter_schema: vec![WidgetParameterDeclaration {
                name: "value".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            }],
            layers: vec![],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.8,
                width_pct: 1.0,
                height_pct: 0.05,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: true, // ephemeral!
            hover_behavior: None,
        });

        let tab_id = s.create_tab("main", 0).unwrap();
        s.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "live-bar".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "live-bar".to_string(),
            current_params: std::collections::HashMap::new(),
        });
    }

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let mut client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "ephemeral-widget-agent",
        "test-key",
        &["publish_widget:live-bar"],
    )
    .await;

    // Publish to ephemeral widget
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:live-bar".to_string(),
            params: vec![crate::proto::WidgetParameterValueProto {
                param_name: "value".to_string(),
                value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                    0.9,
                )),
            }],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    // Send a heartbeat — the next response should be the echo (no WidgetPublishResult)
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 77777,
        })),
    })
    .await
    .unwrap();

    let next_msg = stream.next().await.unwrap().unwrap();
    match &next_msg.payload {
        Some(ServerPayload::RequestResult(_)) => {
            panic!("Ephemeral widget publish must NOT produce a WidgetPublishResult")
        }
        Some(ServerPayload::Heartbeat(hb)) => {
            assert_eq!(hb.timestamp_mono_us, 77777, "expected heartbeat echo");
        }
        other => panic!("Expected Heartbeat echo, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_missing_capability_rejected() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) =
        handshake_with_capabilities(&mut client, "asset-no-cap", "test-key", &[]).await;

    let payload = b"<svg xmlns='http://www.w3.org/2000/svg'></svg>".to_vec();
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&payload).as_bytes().to_vec(),
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_CAPABILITY_MISSING");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_metadata_preflight_dedup_hit() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "asset-dedup",
        "test-key",
        &["register_widget_asset"],
    )
    .await;

    let payload =
        b"<svg xmlns='http://www.w3.org/2000/svg'><rect width='1' height='1'/></svg>".to_vec();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: hash.clone(),
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let first = next_server_msg(&mut stream).await;
    match &first.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(!result.was_deduplicated);
        }
        other => panic!("expected WidgetAssetRegisterResult on first upload, got: {other:?}"),
    }

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: hash,
            transport_crc32c: 0,
            total_size_bytes: 0,
            inline_svg_bytes: Vec::new(),
            metadata_only_preflight: true,
        })),
    })
    .await
    .unwrap();

    let second = next_server_msg(&mut stream).await;
    match &second.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(result.was_deduplicated);
        }
        other => panic!("expected WidgetAssetRegisterResult on preflight, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_durable_store_dedups_after_restart() {
    let temp = tempfile::tempdir().expect("tempdir should be creatable");
    let store_path = temp.path().join("runtime-widget-store");
    let payload =
        b"<svg xmlns='http://www.w3.org/2000/svg'><rect width='3' height='2'/></svg>".to_vec();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    // First runtime instance writes the asset durably.
    let (mut client_a, handle_a) =
        setup_widget_test_with_durable_store(store_path.clone(), 0, 0).await;
    let (tx_a, _init_msgs_a, mut stream_a) = handshake_with_capabilities(
        &mut client_a,
        "asset-durable-a",
        "test-key",
        &["register_widget_asset"],
    )
    .await;
    tx_a.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: hash.clone(),
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();
    let first = next_server_msg(&mut stream_a).await;
    match &first.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(!result.was_deduplicated);
        }
        other => panic!("expected WidgetAssetRegisterResult on first upload, got: {other:?}"),
    }

    // New runtime instance should preflight-dedup from the same durable store.
    let (mut client_b, handle_b) = setup_widget_test_with_durable_store(store_path, 0, 0).await;
    let (tx_b, _init_msgs_b, mut stream_b) = handshake_with_capabilities(
        &mut client_b,
        "asset-durable-b",
        "test-key",
        &["register_widget_asset"],
    )
    .await;
    tx_b.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: hash,
            transport_crc32c: 0,
            total_size_bytes: 0,
            inline_svg_bytes: Vec::new(),
            metadata_only_preflight: true,
        })),
    })
    .await
    .unwrap();
    let second = next_server_msg(&mut stream_b).await;
    match &second.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(result.was_deduplicated);
        }
        other => {
            panic!("expected WidgetAssetRegisterResult on restart preflight, got: {other:?}")
        }
    }

    drop(handle_a);
    drop(handle_b);
}

#[tokio::test]
async fn test_widget_asset_register_unknown_hash_requires_payload_and_hash_validation() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "asset-require-payload",
        "test-key",
        &["register_widget_asset"],
    )
    .await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: vec![0x11; 32],
            transport_crc32c: 0,
            total_size_bytes: 0,
            inline_svg_bytes: Vec::new(),
            metadata_only_preflight: true,
        })),
    })
    .await
    .unwrap();

    let missing_payload = next_server_msg(&mut stream).await;
    match &missing_payload.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_HASH_MISMATCH");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    let payload = b"<svg xmlns='http://www.w3.org/2000/svg'></svg>".to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: vec![0xAA; 32], // wrong on purpose
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let hash_mismatch = next_server_msg(&mut stream).await;
    match &hash_mismatch.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_HASH_MISMATCH");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    let valid_payload = b"<svg xmlns='http://www.w3.org/2000/svg'><circle r='2'/></svg>".to_vec();
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&valid_payload).as_bytes().to_vec(),
            transport_crc32c: 0,
            total_size_bytes: valid_payload.len() as u64,
            inline_svg_bytes: valid_payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let uploaded = next_server_msg(&mut stream).await;
    match &uploaded.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(!result.was_deduplicated);
            assert!(result.error_code.is_empty());
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_checksum_svg_and_type_validation() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "asset-validation",
        "test-key",
        &["register_widget_asset"],
    )
    .await;

    // Invalid type id (must be kebab-case).
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "Gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: vec![0x44; 32],
            transport_crc32c: 0,
            total_size_bytes: 0,
            inline_svg_bytes: Vec::new(),
            metadata_only_preflight: true,
        })),
    })
    .await
    .unwrap();
    let invalid_type = next_server_msg(&mut stream).await;
    match &invalid_type.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_TYPE_INVALID");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    // Bad checksum.
    let crc_payload = b"<svg xmlns='http://www.w3.org/2000/svg'></svg>".to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&crc_payload).as_bytes().to_vec(),
            transport_crc32c: 1, // wrong on purpose
            total_size_bytes: crc_payload.len() as u64,
            inline_svg_bytes: crc_payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();
    let checksum_mismatch = next_server_msg(&mut stream).await;
    match &checksum_mismatch.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_CHECKSUM_MISMATCH");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    // Invalid SVG payload.
    let invalid_svg_payload = b"not-svg".to_vec();
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&invalid_svg_payload).as_bytes().to_vec(),
            transport_crc32c: 0,
            total_size_bytes: invalid_svg_payload.len() as u64,
            inline_svg_bytes: invalid_svg_payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();
    let invalid_svg = next_server_msg(&mut stream).await;
    match &invalid_svg.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_INVALID_SVG");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_budget_exceeded_rejected() {
    let (mut client, handle) = setup_widget_test_with_asset_limits(24, 24).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "asset-budget",
        "test-key",
        &["register_widget_asset"],
    )
    .await;

    let payload =
        b"<svg xmlns='http://www.w3.org/2000/svg'><rect width='10' height='10'/></svg>".to_vec();
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&payload).as_bytes().to_vec(),
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let budget_denied = next_server_msg(&mut stream).await;
    match &budget_denied.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_BUDGET_EXCEEDED");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_updates_runtime_widget_lifecycle_for_publish_path() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let wakes = Arc::new(AtomicU64::new(0));
    let callback_wakes = Arc::clone(&wakes);
    let notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_wakes.fetch_add(1, Ordering::AcqRel);
    });
    let mut service = setup_widget_service().await;
    service.render_wake = notifier;
    let shared_state = service.state.clone();
    let (mut client, handle) = setup_widget_test_with_service(service).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "asset-lifecycle",
        "test-key",
        &["register_widget_asset", "publish_widget:gauge"],
    )
    .await;

    let payload =
        b"<svg xmlns='http://www.w3.org/2000/svg'><rect id='bar' width='1' height='1'/></svg>"
            .to_vec();
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&payload).as_bytes().to_vec(),
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload.clone(),
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let asset_handle = match next_server_msg(&mut stream).await.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(!result.was_deduplicated);
            result.asset_handle
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    };
    assert_eq!(wakes.load(Ordering::Acquire), 1);

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: vec![0x77; 32],
            transport_crc32c: 0,
            total_size_bytes: 0,
            inline_svg_bytes: Vec::new(),
            metadata_only_preflight: true,
        })),
    })
    .await
    .unwrap();
    let no_enqueue = next_server_msg(&mut stream).await;
    assert!(matches!(
        no_enqueue.payload,
        Some(ServerPayload::WidgetAssetRegisterResult(
            WidgetAssetRegisterResult {
                accepted: false,
                ..
            }
        ))
    ));
    assert_eq!(wakes.load(Ordering::Acquire), 1);

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![crate::proto::WidgetParameterValueProto {
                param_name: "level".to_string(),
                value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                    0.42,
                )),
            }],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let publish_msg = next_server_msg(&mut stream).await;
    match &publish_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(result.ok, "publish should remain usable after registration");
        }
        other => panic!("expected WidgetPublishResult, got: {other:?}"),
    }

    {
        let st = shared_state.lock().await;
        let mut scene = st.scene.lock().await;
        assert_eq!(
            scene
                .widget_registry
                .runtime_svg_handle("gauge", "fill.svg"),
            Some(asset_handle.as_str())
        );

        let queued = scene.drain_pending_widget_svg_assets();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "gauge");
        assert_eq!(queued[0].1, "fill.svg");
        assert_eq!(queued[0].2, payload);
    }

    drop(handle);
}

fn tiny_png_1x1_rgba() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x56, 0xc7, 0x2f, 0x0d, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}

fn tiny_rgba_1x1(pixel: [u8; 4]) -> Vec<u8> {
    pixel.to_vec()
}

#[test]
fn upload_byte_rate_limiter_enforces_sliding_window() {
    let base = Instant::now();
    let mut limiter = UploadByteRateLimiter::with_limit(8);

    assert_eq!(limiter.available_bytes(base), 8);
    limiter.reserve_bytes(base, 8);
    assert_eq!(
        limiter.available_bytes(base + Duration::from_millis(100)),
        0
    );

    let delay = limiter.next_delay(base + Duration::from_millis(100));
    assert!(
        delay >= Duration::from_millis(850),
        "expected ~900ms wait, got {delay:?}"
    );

    assert_eq!(limiter.available_bytes(base + Duration::from_secs(1)), 8);
}

#[test]
fn upload_byte_rate_limiter_zero_limit_is_unbounded() {
    let base = Instant::now();
    let mut limiter = UploadByteRateLimiter::with_limit(0);

    assert_eq!(limiter.available_bytes(base), usize::MAX);
    assert_eq!(limiter.next_delay(base), Duration::ZERO);

    limiter.reserve_bytes(base, 1024);
    assert_eq!(
        limiter.available_bytes(base + Duration::from_millis(500)),
        usize::MAX
    );
}

#[tokio::test]
async fn test_resource_upload_chunk_transport_backpressure_from_rate_limit() {
    let (mut client, handle) = setup_widget_test_with_upload_rate_limit(8).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-rate-limit",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let chunk_a = vec![0xAB; 8];
    let chunk_b = vec![0xCD; 8];
    let payload = [chunk_a.clone(), chunk_b.clone()].concat();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 1, // IMAGE_RGBA8
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata {
                width: 2,
                height: 2,
                ..ResourceMetadata::default()
            }),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let accepted = next_server_msg(&mut stream).await;
    let upload_id = match &accepted.payload {
        Some(ServerPayload::ResourceUploadAccepted(accepted)) => {
            assert_eq!(accepted.request_sequence, 2);
            accepted.upload_id.clone()
        }
        other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
    };

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_id.clone(),
            chunk_index: 0,
            data: chunk_a,
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_id.clone(),
            chunk_index: 1,
            data: chunk_b,
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_id.clone(),
            },
        )),
    })
    .await
    .unwrap();

    let early = tokio::time::timeout(
        tokio::time::Duration::from_millis(300),
        next_server_msg(&mut stream),
    )
    .await;
    assert!(
        early.is_err(),
        "chunk stream should be back-pressured; completion arrived too quickly"
    );

    let stored = tokio::time::timeout(
        tokio::time::Duration::from_secs(3),
        next_server_msg(&mut stream),
    )
    .await
    .expect("expected ResourceStored after backpressure interval");

    match &stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 2);
            assert_eq!(stored.upload_id, upload_id);
            assert!(!stored.was_deduplicated);
        }
        other => panic!("expected ResourceStored after chunk backpressure, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_backpressure_keeps_heartbeat_responsive() {
    let (mut client, handle) = setup_widget_test_with_upload_rate_limit(8).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-heartbeat-backpressure",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let chunk_a = vec![0xAB; 8];
    let chunk_b = vec![0xCD; 8];
    let payload = [chunk_a.clone(), chunk_b.clone()].concat();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 1, // IMAGE_RGBA8
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata {
                width: 2,
                height: 2,
                ..ResourceMetadata::default()
            }),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let accepted = next_server_msg(&mut stream).await;
    let upload_id = match &accepted.payload {
        Some(ServerPayload::ResourceUploadAccepted(accepted)) => accepted.upload_id.clone(),
        other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
    };

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_id.clone(),
            chunk_index: 0,
            data: chunk_a,
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_id.clone(),
            chunk_index: 1,
            data: chunk_b,
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_id.clone(),
            },
        )),
    })
    .await
    .unwrap();

    let heartbeat_ts = 4242u64;
    tx.send(ClientMessage {
        sequence: 6,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: heartbeat_ts,
        })),
    })
    .await
    .unwrap();

    let heartbeat_echo = tokio::time::timeout(
        tokio::time::Duration::from_millis(300),
        next_server_msg(&mut stream),
    )
    .await
    .expect("heartbeat should not be blocked by upload backpressure");

    match &heartbeat_echo.payload {
        Some(ServerPayload::Heartbeat(hb)) => {
            assert_eq!(hb.timestamp_mono_us, heartbeat_ts);
        }
        other => panic!("expected Heartbeat echo, got: {other:?}"),
    }

    let stored = tokio::time::timeout(
        tokio::time::Duration::from_secs(3),
        next_server_msg(&mut stream),
    )
    .await
    .expect("expected ResourceStored after backpressure interval");

    match &stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 2);
            assert_eq!(stored.upload_id, upload_id);
            assert!(!stored.was_deduplicated);
        }
        other => panic!("expected ResourceStored after chunk backpressure, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_backpressure_preserves_transactional_chunk_order() {
    let (mut client, handle) = setup_widget_test_with_upload_rate_limit(8).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-transactional-backpressure",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let payload_a = vec![0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80];
    let payload_b = tiny_rgba_1x1([0xAA, 0xBB, 0xCC, 0xDD]);
    let hash_a = blake3::hash(&payload_a).as_bytes().to_vec();
    let hash_b = blake3::hash(&payload_b).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash_a,
            resource_type: 1, // IMAGE_RGBA8
            total_size_bytes: payload_a.len() as u64,
            metadata: Some(ResourceMetadata {
                width: 1,
                height: 2,
                ..Default::default()
            }),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash_b,
            resource_type: 1, // IMAGE_RGBA8
            total_size_bytes: payload_b.len() as u64,
            metadata: Some(ResourceMetadata {
                width: 1,
                height: 1,
                ..Default::default()
            }),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let accepted_a = next_server_msg(&mut stream).await;
    let upload_a = match &accepted_a.payload {
        Some(ServerPayload::ResourceUploadAccepted(accepted)) => {
            assert_eq!(accepted.request_sequence, 2);
            accepted.upload_id.clone()
        }
        other => panic!("expected ResourceUploadAccepted for upload A, got: {other:?}"),
    };

    let accepted_b = next_server_msg(&mut stream).await;
    let upload_b = match &accepted_b.payload {
        Some(ServerPayload::ResourceUploadAccepted(accepted)) => {
            assert_eq!(accepted.request_sequence, 3);
            accepted.upload_id.clone()
        }
        other => panic!("expected ResourceUploadAccepted for upload B, got: {other:?}"),
    };

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_a.clone(),
            chunk_index: 0,
            data: payload_a,
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_a.clone(),
            },
        )),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 6,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_b.clone(),
            chunk_index: 0,
            data: payload_b,
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 7,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_b.clone(),
            },
        )),
    })
    .await
    .unwrap();

    let first_stored = tokio::time::timeout(
        tokio::time::Duration::from_millis(300),
        next_server_msg(&mut stream),
    )
    .await
    .expect("first upload should complete before rate limiter delays second upload");

    match &first_stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 2);
            assert_eq!(stored.upload_id, upload_a);
            assert!(!stored.was_deduplicated);
        }
        other => panic!("expected first ResourceStored for request 2, got: {other:?}"),
    }

    let early_second = tokio::time::timeout(
        tokio::time::Duration::from_millis(300),
        next_server_msg(&mut stream),
    )
    .await;
    assert!(
        early_second.is_err(),
        "second upload result should be delayed by upload-rate backpressure"
    );

    let second_stored = tokio::time::timeout(
        tokio::time::Duration::from_secs(3),
        next_server_msg(&mut stream),
    )
    .await
    .expect("second upload should complete once backpressure window clears");

    match &second_stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 3);
            assert_eq!(stored.upload_id, upload_b);
            assert!(!stored.was_deduplicated);
        }
        other => panic!("expected second ResourceStored for request 3, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_inline_transport_backpressure_from_rate_limit() {
    // Inline data (upload_start with inline_data set) must pass through
    // apply_upload_transport_backpressure just like the chunk path does.
    // A rate limit of 8 bytes/s means an 8-byte inline payload exhausts the
    // window immediately; a second upload of equal size should be delayed.
    let (mut client, handle) = setup_widget_test_with_upload_rate_limit(8).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-inline-rate-limit",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let payload_a = vec![0x01u8; 8];
    let payload_b = vec![0x02u8; 8];
    let hash_a = blake3::hash(&payload_a).as_bytes().to_vec();
    let hash_b = blake3::hash(&payload_b).as_bytes().to_vec();

    // First inline upload (fills the rate window).
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash_a,
            resource_type: 1, // IMAGE_RGBA8
            total_size_bytes: payload_a.len() as u64,
            metadata: Some(ResourceMetadata {
                width: 2,
                height: 1,
                ..Default::default()
            }),
            inline_data: payload_a,
        })),
    })
    .await
    .unwrap();

    // Second inline upload (should be delayed by the rate limiter).
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash_b,
            resource_type: 1, // IMAGE_RGBA8
            total_size_bytes: payload_b.len() as u64,
            metadata: Some(ResourceMetadata {
                width: 2,
                height: 1,
                ..Default::default()
            }),
            inline_data: payload_b,
        })),
    })
    .await
    .unwrap();

    // First upload should complete quickly (no prior debt in window).
    let first_stored = tokio::time::timeout(
        tokio::time::Duration::from_millis(300),
        next_server_msg(&mut stream),
    )
    .await
    .expect("first inline upload should complete before rate limiter delays second");

    match &first_stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 2);
            assert!(
                stored.upload_id.is_empty(),
                "inline upload has no upload_id"
            );
            assert!(!stored.was_deduplicated);
        }
        other => panic!("expected ResourceStored for first inline upload, got: {other:?}"),
    }

    // Second upload result must be delayed by the rate window.
    let early_second = tokio::time::timeout(
        tokio::time::Duration::from_millis(300),
        next_server_msg(&mut stream),
    )
    .await;
    assert!(
        early_second.is_err(),
        "second inline upload should be rate-limited; result arrived too quickly"
    );

    let second_stored = tokio::time::timeout(
        tokio::time::Duration::from_secs(3),
        next_server_msg(&mut stream),
    )
    .await
    .expect("second inline upload should complete once rate-limit window clears");

    match &second_stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 3);
            assert!(
                stored.upload_id.is_empty(),
                "inline upload has no upload_id"
            );
            assert!(!stored.was_deduplicated);
        }
        other => panic!("expected ResourceStored for second inline upload, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_start_requires_upload_resource_capability() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) =
        handshake_with_capabilities(&mut client, "resource-no-cap", "test-key", &[]).await;
    let payload = tiny_png_1x1_rgba();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 2,
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: payload,
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::ResourceErrorResponse(err)) => {
            assert_eq!(err.request_sequence, 2);
            assert_eq!(err.error_code, 1);
            assert!(err.upload_id.is_empty());
        }
        other => panic!("expected ResourceErrorResponse, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_inline_and_dedup_short_circuit() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-inline",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let payload = tiny_png_1x1_rgba();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash.clone(),
            resource_type: 2,
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: payload.clone(),
        })),
    })
    .await
    .unwrap();

    let first = next_server_msg(&mut stream).await;
    match &first.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 2);
            assert!(!stored.was_deduplicated);
            assert!(stored.upload_id.is_empty());
        }
        other => panic!("expected ResourceStored, got: {other:?}"),
    }

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 2,
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let second = next_server_msg(&mut stream).await;
    match &second.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 3);
            assert!(stored.was_deduplicated);
            assert!(stored.upload_id.is_empty());
        }
        other => panic!("expected ResourceStored on dedup short-circuit, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_chunked_ack_then_complete() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-chunked",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let payload = tiny_png_1x1_rgba();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 2,
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let accepted = next_server_msg(&mut stream).await;
    let upload_id = match &accepted.payload {
        Some(ServerPayload::ResourceUploadAccepted(accepted)) => {
            assert_eq!(accepted.request_sequence, 2);
            assert_eq!(accepted.upload_id.len(), 16);
            accepted.upload_id.clone()
        }
        other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
    };

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_id.clone(),
            chunk_index: 0,
            data: payload.clone(),
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_id.clone(),
            },
        )),
    })
    .await
    .unwrap();

    let stored = next_server_msg(&mut stream).await;
    match &stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 2);
            assert_eq!(stored.upload_id, upload_id);
            assert!(!stored.was_deduplicated);
        }
        other => panic!("expected ResourceStored on complete, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_chunked_concurrent_limit_rejected() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-concurrent-limit",
        "test-key",
        &["upload_resource"],
    )
    .await;

    // ResourceStore allows at most 4 in-flight uploads per agent namespace.
    for offset in 0..5u8 {
        let seq = u64::from(offset) + 2;
        let payload = tiny_rgba_1x1([offset, 0, 0, 0xFF]);
        tx.send(ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
                expected_hash: blake3::hash(&payload).as_bytes().to_vec(),
                resource_type: 1, // IMAGE_RGBA8
                total_size_bytes: payload.len() as u64,
                metadata: Some(ResourceMetadata {
                    width: 1,
                    height: 1,
                    ..Default::default()
                }),
                inline_data: Vec::new(),
            })),
        })
        .await
        .unwrap();

        let msg = next_server_msg(&mut stream).await;
        if offset < 4 {
            match &msg.payload {
                Some(ServerPayload::ResourceUploadAccepted(accepted)) => {
                    assert_eq!(accepted.request_sequence, seq);
                    assert_eq!(accepted.upload_id.len(), 16);
                }
                other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
            }
        } else {
            match &msg.payload {
                Some(ServerPayload::ResourceErrorResponse(err)) => {
                    assert_eq!(err.request_sequence, seq);
                    assert_eq!(err.error_code, 8);
                    assert!(err.upload_id.is_empty());
                }
                other => panic!("expected ResourceErrorResponse, got: {other:?}"),
            }
        }
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_chunked_success_correlates_by_request_sequence() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-correlation",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let payload_a = tiny_rgba_1x1([0, 0, 0, 0xFF]);
    let payload_b = tiny_rgba_1x1([0xFF, 0, 0, 0xFF]);
    let expected_a = blake3::hash(&payload_a).as_bytes().to_vec();
    let expected_b = blake3::hash(&payload_b).as_bytes().to_vec();

    for (seq, expected_hash) in [(2u64, expected_a.clone()), (3u64, expected_b.clone())] {
        tx.send(ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
                expected_hash,
                resource_type: 1, // IMAGE_RGBA8
                total_size_bytes: 4,
                metadata: Some(ResourceMetadata {
                    width: 1,
                    height: 1,
                    ..Default::default()
                }),
                inline_data: Vec::new(),
            })),
        })
        .await
        .unwrap();
    }

    let mut upload_id_by_request = HashMap::new();
    for _ in 0..2 {
        let msg = next_server_msg(&mut stream).await;
        match &msg.payload {
            Some(ServerPayload::ResourceUploadAccepted(accepted)) => {
                upload_id_by_request.insert(accepted.request_sequence, accepted.upload_id.clone());
            }
            other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
        }
    }
    assert_eq!(upload_id_by_request.len(), 2);
    let upload_a = upload_id_by_request
        .get(&2)
        .expect("request 2 must have upload_id")
        .clone();
    let upload_b = upload_id_by_request
        .get(&3)
        .expect("request 3 must have upload_id")
        .clone();

    // Complete request 3 before request 2 to assert correlation semantics.
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_b.clone(),
            chunk_index: 0,
            data: payload_b.clone(),
        })),
    })
    .await
    .unwrap();
    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_b.clone(),
            },
        )),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 6,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_a.clone(),
            chunk_index: 0,
            data: payload_a.clone(),
        })),
    })
    .await
    .unwrap();
    tx.send(ClientMessage {
        sequence: 7,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_a.clone(),
            },
        )),
    })
    .await
    .unwrap();

    let mut stored_by_request = HashMap::new();
    for _ in 0..2 {
        let msg = next_server_msg(&mut stream).await;
        match &msg.payload {
            Some(ServerPayload::ResourceStored(stored)) => {
                let bytes = stored
                    .resource_id
                    .as_ref()
                    .expect("resource_id must be present")
                    .bytes
                    .clone();
                stored_by_request
                    .insert(stored.request_sequence, (stored.upload_id.clone(), bytes));
            }
            other => panic!("expected ResourceStored, got: {other:?}"),
        }
    }

    assert_eq!(stored_by_request.len(), 2);
    assert_eq!(
        stored_by_request
            .get(&2)
            .expect("request 2 stored result must exist")
            .0,
        upload_a
    );
    assert_eq!(
        stored_by_request
            .get(&3)
            .expect("request 3 stored result must exist")
            .0,
        upload_b
    );
    assert_eq!(
        stored_by_request
            .get(&2)
            .expect("request 2 stored result must exist")
            .1,
        expected_a
    );
    assert_eq!(
        stored_by_request
            .get(&3)
            .expect("request 3 stored result must exist")
            .1,
        expected_b
    );

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_chunked_zero_size_rejected() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-zero-size",
        "test-key",
        &["upload_resource"],
    )
    .await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: vec![0xAB; 32],
            resource_type: 2,
            total_size_bytes: 0,
            metadata: Some(ResourceMetadata::default()),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::ResourceErrorResponse(err)) => {
            assert_eq!(err.request_sequence, 2);
            assert_eq!(err.error_code, 3);
            assert!(err.upload_id.is_empty());
            assert!(
                err.message.contains("total_size_bytes"),
                "expected total_size guard message, got: {}",
                err.message
            );
        }
        other => panic!("expected ResourceErrorResponse, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_chunk_error_aborts_inflight_tracking() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-chunk-error",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let payload = tiny_png_1x1_rgba();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 2,
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let accepted = next_server_msg(&mut stream).await;
    let upload_id = match &accepted.payload {
        Some(ServerPayload::ResourceUploadAccepted(accepted)) => accepted.upload_id.clone(),
        other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
    };

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_id.clone(),
            chunk_index: 1,
            data: payload.clone(),
        })),
    })
    .await
    .unwrap();

    let first_error = next_server_msg(&mut stream).await;
    match &first_error.payload {
        Some(ServerPayload::ResourceErrorResponse(err)) => {
            assert_eq!(err.request_sequence, 2);
            assert_eq!(err.error_code, 7);
            assert_eq!(err.upload_id, upload_id);
        }
        other => panic!("expected ResourceErrorResponse after bad chunk, got: {other:?}"),
    }

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete { upload_id },
        )),
    })
    .await
    .unwrap();

    let second_error = next_server_msg(&mut stream).await;
    match &second_error.payload {
        Some(ServerPayload::ResourceErrorResponse(err)) => {
            assert_eq!(err.request_sequence, 4);
            assert_eq!(err.error_code, 9);
        }
        other => panic!("expected ResourceErrorResponse after aborted upload, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resident_upload_then_static_image_references_uploaded_resource_id() {
    let service = setup_widget_service().await;
    let shared_state = service.state.clone();
    let (mut client, handle) = setup_widget_test_with_service(service).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-scene-node",
        "test-key",
        &["upload_resource", "modify_own_tiles"],
    )
    .await;

    let payload = tiny_png_1x1_rgba();
    let hash = blake3::hash(&payload).as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 2, // IMAGE_PNG
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: payload,
        })),
    })
    .await
    .unwrap();

    let stored = next_server_msg(&mut stream).await;
    let resource_id_bytes = match stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            stored
                .resource_id
                .expect("resource_id must be present on success")
                .bytes
        }
        other => panic!("expected ResourceStored, got: {other:?}"),
    };
    let resource_id = ResourceId::from_bytes(
        resource_id_bytes
            .as_slice()
            .try_into()
            .expect("resource_id must be 32 bytes"),
    );
    {
        let st = shared_state.lock().await;
        let scene = st.scene.lock().await;
        assert!(
            scene.is_resource_registered(&resource_id),
            "uploaded resources must be registered for scene mutation validation"
        );
    }

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let claim_msg = next_server_msg(&mut stream).await;
    let (lease_id, tile_id_bytes) = match claim_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => (resp.lease_id, resp.ids[0].clone()),
        other => panic!("expected a granted ClaimTile, got: {other:?}"),
    };

    let root_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 1,
            height: 1,
            decoded_bytes: 4,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 1.0, 1.0),
        }),
    };

    let set_root_batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: set_root_batch_id.clone(),
            lease_id,
            mutations: vec![crate::proto::MutationProto {
                mutation: Some(crate::proto::mutation_proto::Mutation::SetTileRoot(
                    crate::proto::SetTileRootMutation {
                        tile_id: tile_id_bytes.clone(),
                        node: Some(crate::convert::scene_node_to_proto(&root_node)),
                    },
                )),
            }],
            timing: None,
        })),
    })
    .await
    .unwrap();

    let set_root_result = next_server_msg(&mut stream).await;
    match set_root_result.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(result.ok, "set_tile_root should be accepted");
            assert_eq!(result.batch_id, set_root_batch_id);
        }
        other => panic!("expected MutationResult for set_tile_root, got: {other:?}"),
    }

    let tile_id = bytes_to_scene_id(&tile_id_bytes).expect("tile id from mutation must decode");
    {
        let st = shared_state.lock().await;
        let scene = st.scene.lock().await;
        let tile = scene.tiles.get(&tile_id).expect("tile must exist");
        let root_id = tile.root_node.expect("tile must have root node");
        let root = scene.nodes.get(&root_id).expect("root node must exist");
        match &root.data {
            NodeData::StaticImage(static_image) => {
                assert_eq!(
                    static_image.resource_id, resource_id,
                    "scene node must reference uploaded ResourceId"
                );
            }
            other => panic!("expected StaticImage root node, got: {other:?}"),
        }
    }

    drop(handle);
}

#[tokio::test]
async fn uploaded_resource_notifies_after_the_parked_checkpoint() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let service = setup_widget_service().await;
    let wake_generation = Arc::new(AtomicU64::new(0));
    let callback_generation = Arc::clone(&wake_generation);
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_generation.fetch_add(1, Ordering::AcqRel);
    });
    let resource_id = tze_hud_resource::ResourceId::from_bytes([0x5a; 32]);
    let parked_checkpoint = wake_generation.load(Ordering::Acquire);

    upload::register_uploaded_scene_resource(&service.state, &resource_id, &render_wake).await;

    let st = service.state.lock().await;
    let scene = st.scene.lock().await;
    assert!(scene.is_resource_registered(&ResourceId::from_bytes([0x5a; 32])));
    assert_eq!(
        wake_generation.load(Ordering::Acquire),
        parked_checkpoint + 1,
        "successful registration must publish a generation after the parked checkpoint"
    );
    drop(scene);
    drop(st);

    upload::register_uploaded_scene_resource(&service.state, &resource_id, &render_wake).await;
    assert_eq!(
        wake_generation.load(Ordering::Acquire),
        parked_checkpoint + 1,
        "duplicate resource registration is a no-op and must not wake"
    );
}

/// Helper: handshake with specific capabilities in SessionInit.
///
/// Widget capability checks use `session.capabilities` which is populated
/// from the SessionInit `requested_capabilities` list.
async fn handshake_with_capabilities(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
    psk: &str,
    extra_caps: &[&str],
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    Vec<ServerMessage>,
    tonic::Streaming<ServerMessage>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    let mut caps = vec![
        "create_tiles".to_string(),
        "access_input_events".to_string(),
        "read_scene_topology".to_string(),
    ];
    for c in extra_caps {
        caps.push(c.to_string());
    }

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: agent_id.to_string(),
            initial_subscriptions: vec![],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential(psk.to_string())),
        })),
    })
    .await
    .unwrap();

    let mut streaming = client.session(stream).await.unwrap().into_inner();

    // Collect the full ordered handshake baseline.
    let mut init_messages = Vec::new();
    for _ in 0..3 {
        if let Some(msg) = streaming.next().await {
            init_messages.push(msg.unwrap());
        }
    }

    (tx, init_messages, streaming)
}

// ─── ElementRepositionedEvent tests (hud-bs2q.6) ─────────────────────────

/// Build a service + shared-state + element_repositioned broadcast channel.
///
/// Extracts the shared state and broadcast sender before moving the service
/// into the server task. The test can then call `broadcast_element_repositioned`
/// via the channel directly, or manipulate the shared state for reset tests.
async fn setup_test_with_reposition_tx() -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<SharedState>>,
    tokio::sync::broadcast::Sender<crate::proto::ElementRepositionedEvent>,
) {
    let scene = SceneGraph::new(1920.0, 1080.0);
    let service = HudSessionImpl::new(scene, "test-key");
    let shared_state = service.state.clone();
    let reposition_tx = service.element_repositioned_tx.clone();

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    (client, handle, shared_state, reposition_tx)
}

/// GIVEN element with geometry_override
/// WHEN reset_geometry_override is called on the element store
/// THEN override is cleared and the previous value is returned
#[test]
fn test_reset_geometry_override_clears_override_and_returns_previous() {
    use tze_hud_scene::element_store::{ElementStore, ElementStoreEntry, ElementType};

    let tile_id = SceneId::new();
    let override_policy = GeometryPolicy::Relative {
        x_pct: 0.5,
        y_pct: 0.5,
        width_pct: 0.2,
        height_pct: 0.1,
    };
    let mut store = ElementStore::default();
    store.entries.insert(
        tile_id,
        ElementStoreEntry {
            element_type: ElementType::Tile,
            namespace: "test-agent".to_string(),
            created_at: 1000,
            last_published_at: 2000,
            z_order: 0,
            unseen_restarts: 0,
            geometry_override: Some(override_policy),
        },
    );

    let previous = store.reset_geometry_override(tile_id);
    assert_eq!(
        previous,
        Some(override_policy),
        "reset must return the cleared override"
    );
    assert!(
        store
            .entries
            .get(&tile_id)
            .unwrap()
            .geometry_override
            .is_none(),
        "geometry_override must be None after reset"
    );
}

/// GIVEN element without geometry_override
/// WHEN reset_geometry_override is called
/// THEN returns None (no-op)
#[test]
fn test_reset_geometry_override_noop_when_no_override() {
    use tze_hud_scene::element_store::{ElementStore, ElementStoreEntry, ElementType};

    let tile_id = SceneId::new();
    let mut store = ElementStore::default();
    store.entries.insert(
        tile_id,
        ElementStoreEntry {
            element_type: ElementType::Tile,
            namespace: "test-agent".to_string(),
            created_at: 1000,
            last_published_at: 2000,
            z_order: 0,
            unseen_restarts: 0,
            geometry_override: None,
        },
    );

    let previous = store.reset_geometry_override(tile_id);
    assert!(
        previous.is_none(),
        "reset must return None when no override"
    );
}

/// GIVEN unknown element_id
/// WHEN reset_geometry_override is called
/// THEN returns None (no-op)
#[test]
fn test_reset_geometry_override_noop_for_unknown_element() {
    use tze_hud_scene::element_store::ElementStore;

    let mut store = ElementStore::default();
    let result = store.reset_geometry_override(SceneId::new());
    assert!(
        result.is_none(),
        "reset must return None for unknown element"
    );
}

/// GIVEN agent subscribed to SCENE_TOPOLOGY
/// WHEN broadcast_element_repositioned is called via the channel
/// THEN agent receives ElementRepositionedEvent
#[tokio::test]
async fn test_element_repositioned_delivered_to_scene_topology_subscriber() {
    let (mut client, _server, _shared_state, reposition_tx) = setup_test_with_reposition_tx().await;
    let (_tx, _msgs, mut stream) = handshake(&mut client, "test-agent", "test-key").await;

    // Give the session handler a moment to fully subscribe.
    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

    let element_id = SceneId::new();
    let new_policy = GeometryPolicy::Relative {
        x_pct: 0.3,
        y_pct: 0.2,
        width_pct: 0.25,
        height_pct: 0.15,
    };
    let old_policy = GeometryPolicy::Relative {
        x_pct: 0.1,
        y_pct: 0.1,
        width_pct: 0.25,
        height_pct: 0.15,
    };

    let event = crate::proto::ElementRepositionedEvent {
        element_id: scene_id_to_bytes(element_id),
        new_geometry: Some(convert::geometry_policy_to_proto(&new_policy)),
        previous_geometry: Some(convert::geometry_policy_to_proto(&old_policy)),
    };
    let _ = reposition_tx.send(event);

    // Collect next message from stream (with timeout to avoid hanging on failure).
    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for ElementRepositionedEvent")
        .expect("stream should not close")
        .expect("should not error");

    match msg.payload {
        Some(ServerPayload::ElementRepositioned(event)) => {
            // element_id must match
            let expected_id: Vec<u8> = scene_id_to_bytes(element_id);
            assert_eq!(event.element_id, expected_id, "element_id must match");
            // new_geometry must be set and match new_policy
            let ng = event.new_geometry.expect("new_geometry must be set");
            match ng.policy {
                Some(crate::proto::geometry_policy_proto::Policy::Relative(r)) => {
                    assert!((r.x_pct - 0.3_f32).abs() < 1e-4, "x_pct mismatch");
                    assert!((r.y_pct - 0.2_f32).abs() < 1e-4, "y_pct mismatch");
                }
                other => panic!("expected Relative geometry, got {other:?}"),
            }
            // previous_geometry must be set and match old_policy
            let pg = event
                .previous_geometry
                .expect("previous_geometry must be set");
            match pg.policy {
                Some(crate::proto::geometry_policy_proto::Policy::Relative(r)) => {
                    assert!((r.x_pct - 0.1_f32).abs() < 1e-4, "prev x_pct mismatch");
                }
                other => panic!("expected Relative previous geometry, got {other:?}"),
            }
        }
        other => panic!("expected ElementRepositioned, got {other:?}"),
    }
}

/// GIVEN agent NOT subscribed to SCENE_TOPOLOGY
/// WHEN broadcast_element_repositioned is called
/// THEN agent does not receive ElementRepositionedEvent
#[tokio::test]
async fn test_element_repositioned_not_delivered_without_scene_topology_subscription() {
    use crate::proto::session::hud_session_client::HudSessionClient;
    use crate::proto::session::hud_session_server::HudSessionServer;

    let scene = SceneGraph::new(1920.0, 1080.0);
    let service = HudSessionImpl::new(scene, "test-key");
    let reposition_tx = service.element_repositioned_tx.clone();

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let _handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let mut client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    // Handshake WITHOUT read_scene_topology capability → no SCENE_TOPOLOGY subscription.
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "no-topology-agent".to_string(),
            initial_subscriptions: vec![],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
        })),
    })
    .await
    .unwrap();
    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    // Drain handshake baseline.
    response_stream.next().await;
    response_stream.next().await;
    response_stream.next().await;

    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

    let element_id = SceneId::new();
    let new_policy = GeometryPolicy::Relative {
        x_pct: 0.3,
        y_pct: 0.2,
        width_pct: 0.25,
        height_pct: 0.15,
    };
    let event = crate::proto::ElementRepositionedEvent {
        element_id: scene_id_to_bytes(element_id),
        new_geometry: Some(convert::geometry_policy_to_proto(&new_policy)),
        previous_geometry: None,
    };
    let _ = reposition_tx.send(event);

    // Agent should NOT receive the event; timeout expected.
    let result = tokio::time::timeout(
        tokio::time::Duration::from_millis(200),
        response_stream.next(),
    )
    .await;
    // Timeout means no event was delivered — correct behaviour.
    // If no timeout: check it's not an ElementRepositioned.
    if let Ok(Some(Ok(msg))) = result {
        if let Some(ServerPayload::ElementRepositioned(_)) = msg.payload {
            panic!("ElementRepositioned must NOT be delivered without SCENE_TOPOLOGY subscription");
        }
        // Other messages (e.g., Heartbeat) are allowed.
    }
    drop(tx); // close stream
}

// ─── FramePresented tests (hud-91uu6) ────────────────────────────────────

/// Build a service + frame_presented broadcast channel behind a live server.
async fn setup_test_with_frame_presented_tx() -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    tokio::sync::broadcast::Sender<crate::proto::FramePresented>,
) {
    let scene = SceneGraph::new(1920.0, 1080.0);
    let service = HudSessionImpl::new(scene, "test-key");
    let frame_presented_tx = service.frame_presented_tx.clone();

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    (client, handle, frame_presented_tx)
}

/// Handshake requesting `read_telemetry` and subscribing to `TELEMETRY_FRAMES`.
/// Returns the client-send half and the server->client stream after draining the
/// SessionEstablished + SceneSnapshot + current DegradationNotice messages.
async fn handshake_telemetry(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
    psk: &str,
    subscribe_telemetry: bool,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    tonic::Streaming<ServerMessage>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    let initial_subscriptions = if subscribe_telemetry {
        vec!["TELEMETRY_FRAMES".to_string()]
    } else {
        vec![]
    };
    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: agent_id.to_string(),
            initial_subscriptions,
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential(psk.to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    // Drain the three-message handshake baseline.
    response_stream.next().await;
    response_stream.next().await;
    response_stream.next().await;
    (tx, response_stream)
}

fn sample_frame_presented(batch_id: SceneId) -> crate::proto::FramePresented {
    crate::proto::FramePresented {
        frame_number: 42,
        present_wall_us: now_wall_us(),
        batch_ids: vec![scene_id_to_bytes(batch_id)],
    }
}

/// GIVEN agent subscribed to TELEMETRY_FRAMES (holds read_telemetry)
/// WHEN a FramePresented is broadcast
/// THEN the agent receives it with the correlated batch_id
#[tokio::test]
async fn test_frame_presented_delivered_to_telemetry_subscriber() {
    let (mut client, _server, frame_presented_tx) = setup_test_with_frame_presented_tx().await;
    let (_tx, mut stream) =
        handshake_telemetry(&mut client, "telemetry-agent", "test-key", true).await;

    // Let the session handler finish subscribing to the broadcast channel.
    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

    let batch_id = SceneId::new();
    let _ = frame_presented_tx.send(sample_frame_presented(batch_id));

    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for FramePresented")
        .expect("stream should not close")
        .expect("should not error");

    match msg.payload {
        Some(ServerPayload::FramePresented(event)) => {
            assert_eq!(
                event.batch_ids,
                vec![scene_id_to_bytes(batch_id)],
                "present ack must carry the correlated batch_id"
            );
            assert_eq!(event.frame_number, 42);
            assert!(event.present_wall_us > 0);
        }
        other => panic!("expected FramePresented, got {other:?}"),
    }
}

/// GIVEN agent NOT subscribed to TELEMETRY_FRAMES
/// WHEN a FramePresented is broadcast
/// THEN the agent does not receive it (telemetry gate)
#[tokio::test]
async fn test_frame_presented_not_delivered_without_telemetry_subscription() {
    let (mut client, _server, frame_presented_tx) = setup_test_with_frame_presented_tx().await;
    let (tx, mut stream) =
        handshake_telemetry(&mut client, "no-telemetry-agent", "test-key", false).await;

    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

    let _ = frame_presented_tx.send(sample_frame_presented(SceneId::new()));

    let result = tokio::time::timeout(tokio::time::Duration::from_millis(200), stream.next()).await;
    // Timeout = no delivery = correct. If any message arrives it must not be a
    // FramePresented (Heartbeat etc. are allowed).
    if let Ok(Some(Ok(msg))) = result {
        if let Some(ServerPayload::FramePresented(_)) = msg.payload {
            panic!("FramePresented must NOT be delivered without TELEMETRY_FRAMES subscription");
        }
    }
    drop(tx);
}

/// GIVEN element with override and known agent tile bounds
/// WHEN the element store override is cleared and event is broadcast
/// THEN event carries previous_geometry=old_override and new_geometry=fallback
#[test]
fn test_reset_geometry_override_carries_correct_previous_and_new() {
    use tze_hud_scene::element_store::{ElementStore, ElementStoreEntry, ElementType};

    let tile_id = SceneId::new();
    let override_policy = GeometryPolicy::Relative {
        x_pct: 0.8,
        y_pct: 0.8,
        width_pct: 0.1,
        height_pct: 0.1,
    };
    let mut store = ElementStore::default();
    store.entries.insert(
        tile_id,
        ElementStoreEntry {
            element_type: ElementType::Tile,
            namespace: "test-agent".to_string(),
            created_at: 1000,
            last_published_at: 2000,
            z_order: 0,
            unseen_restarts: 0,
            geometry_override: Some(override_policy),
        },
    );

    // Simulate reset: clear override and note previous value.
    let previous = store.reset_geometry_override(tile_id);
    assert_eq!(
        previous,
        Some(override_policy),
        "previous must be the removed override"
    );

    // After reset, override must be gone.
    let entry = store.entries.get(&tile_id).unwrap();
    assert!(
        entry.geometry_override.is_none(),
        "override must be cleared"
    );

    // The fallback geometry (agent bounds) would be applied by the caller;
    // verify the store correctly reflects the cleared state.
    let proto_previous = convert::geometry_policy_to_proto(&previous.unwrap());
    match proto_previous.policy {
        Some(crate::proto::geometry_policy_proto::Policy::Relative(r)) => {
            assert!(
                (r.x_pct - 0.8_f32).abs() < 1e-4,
                "previous x_pct must match override"
            );
        }
        other => panic!("expected Relative previous_geometry proto, got {other:?}"),
    }
}

#[tokio::test]
async fn rejected_zone_publish_does_not_wake_the_compositor() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let generations = Arc::new(AtomicU64::new(0));
    let callback_generations = Arc::clone(&generations);
    let notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_generations.fetch_add(1, Ordering::AcqRel);
    });
    let (mut client, _server, _state) = setup_test_with_state_and_render_wake(notifier).await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "zone-reject-agent", "test-key").await;
    let before = generations.load(Ordering::Acquire);

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "zone:missing-zone".to_string(),
            content: None,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let result = next_server_msg(&mut stream).await;
    assert!(matches!(
        result.payload,
        Some(ServerPayload::RequestResult(RequestResult {
            ok: false,
            ..
        }))
    ));
    assert_eq!(
        generations.load(Ordering::Acquire),
        before,
        "rejected ZonePublish must not synthesize compositor work"
    );
}

// ─── Invariants 1 and 4 over the real gRPC session path ──────────────────────

use tze_hud_scene::Clock as _;

/// Wall-clock-aligned test clock so wire timestamps (validated against the
/// real wall clock) and scene deadlines (scene clock) agree.
fn wall_aligned_clock() -> tze_hud_scene::TestClock {
    tze_hud_scene::TestClock::new(now_ms())
}

/// Wait for the session task's async cleanup to orphan `lease_id`.
async fn wait_for_lease_state(
    state: &Arc<tokio::sync::Mutex<crate::session::SharedState>>,
    lease_id: SceneId,
    want: LeaseState,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        {
            let st = state.lock().await;
            let scene = st.scene.lock().await;
            if scene.leases.get(&lease_id).map(|l| l.state) == Some(want) {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "lease never reached {want:?}"
        );
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Connect, take a lease that owns one tile, then drop the stream.
/// Returns the resume token, lease id, and tile id.
async fn connect_hold_tile_and_disconnect(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    state: &Arc<tokio::sync::Mutex<crate::session::SharedState>>,
    agent: &str,
) -> (Vec<u8>, SceneId, SceneId) {
    let (tx, init, mut stream) = handshake(client, agent, "test-key").await;
    let resume_token = match &init[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => e.resume_token.clone(),
        other => panic!("expected SessionEstablished, got {other:?}"),
    };
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 600_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let (lease_id, tile_id) = match next_server_msg(&mut stream).await.payload {
        Some(ServerPayload::RequestResult(RequestResult {
            ok: true,
            lease_id,
            ids,
            ..
        })) => (
            bytes_to_scene_id(&lease_id).unwrap(),
            bytes_to_scene_id(&ids[0]).unwrap(),
        ),
        other => panic!("expected a granted ClaimTile, got {other:?}"),
    };
    drop(tx);
    drop(stream);
    wait_for_lease_state(state, lease_id, LeaseState::Orphaned).await;
    (resume_token, lease_id, tile_id)
}

async fn send_resume(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent: &str,
    resume_token: Vec<u8>,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    tonic::Streaming<ServerMessage>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionResume(SessionResume {
            agent_id: agent.to_string(),
            resume_token,
            last_seen_server_sequence: 0,
            pre_shared_key: "test-key".to_string(),
            auth_credential: None,
        })),
    })
    .await
    .unwrap();
    let stream = client
        .session(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    (tx, stream)
}

/// Invariant 4: a gRPC stream ending orphans the session's leases (badge
/// shown, content kept) instead of releasing them.
#[tokio::test]
async fn grpc_disconnect_orphans_leases_and_badges_tiles() {
    let (mut client, server, state, _exp) =
        setup_test_with_lease_expiry_clock(wall_aligned_clock()).await;
    let (_token, _lease_id, tile_id) =
        connect_hold_tile_and_disconnect(&mut client, &state, "orphan-agent").await;

    let st = state.lock().await;
    let scene = st.scene.lock().await;
    let tile = scene.tiles.get(&tile_id).expect("orphaned content is kept");
    assert_eq!(
        tile.visual_hint,
        tze_hud_scene::lease::TileVisualHint::DisconnectionBadge
    );
    drop(scene);
    drop(st);
    server.abort();
}

/// Invariant 4: resuming within the grace period restores the same lease and
/// tile, clears the badge, and creates nothing new.
#[tokio::test]
async fn grpc_resume_within_grace_restores_same_lease_and_tile() {
    let (mut client, server, state, _exp) =
        setup_test_with_lease_expiry_clock(wall_aligned_clock()).await;
    let (token, lease_id, tile_id) =
        connect_hold_tile_and_disconnect(&mut client, &state, "resume-agent").await;

    let (_tx, mut stream) = send_resume(&mut client, "resume-agent", token).await;
    match stream.next().await.unwrap().unwrap().payload {
        Some(ServerPayload::SessionResumeResult(r)) => assert!(r.accepted),
        other => panic!("expected SessionResumeResult, got {other:?}"),
    }

    let st = state.lock().await;
    let scene = st.scene.lock().await;
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
    assert_eq!(scene.tile_count(), 1, "resume must not duplicate surfaces");
    assert_eq!(
        scene.tiles[&tile_id].visual_hint,
        tze_hud_scene::lease::TileVisualHint::None
    );
    drop(scene);
    drop(st);
    server.abort();
}

/// Invariant 4: when the grace period ends, the runtime's lease sweep
/// reclaims the orphaned lease and its tile, and the resume token is dead.
#[tokio::test]
async fn grpc_grace_expiry_reclaims_orphaned_lease_and_rejects_resume() {
    let clock = wall_aligned_clock();
    let (mut client, server, state, _exp) = setup_test_with_lease_expiry_clock(clock.clone()).await;
    let (token, lease_id, tile_id) =
        connect_hold_tile_and_disconnect(&mut client, &state, "late-agent").await;

    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        clock.advance(crate::token::DEFAULT_GRACE_PERIOD_MS + 1);
        // The compositor's per-frame sweep, as in production.
        let expiries = scene.expire_leases();
        assert_eq!(expiries.len(), 1);
        assert_eq!(expiries[0].lease_id, lease_id);
        assert_eq!(expiries[0].removed_tiles, vec![tile_id]);
        assert_eq!(scene.tile_count(), 0);
    }

    let (_tx, mut stream) = send_resume(&mut client, "late-agent", token).await;
    match stream.next().await.unwrap().unwrap().payload {
        Some(ServerPayload::SessionError(e)) => assert_eq!(e.code, "SESSION_GRACE_EXPIRED"),
        other => panic!("expected SESSION_GRACE_EXPIRED, got {other:?}"),
    }
    server.abort();
}

fn subtitle_publish(text: &str, ttl_ms: u64, present_at: u64, expires_at: u64) -> Publish {
    Publish {
        surface: "zone:subtitle".to_string(),
        content: Some(crate::proto::ZoneContent {
            payload: Some(crate::proto::zone_content::Payload::StreamText(
                text.to_string(),
            )),
        }),
        ttl_ms,
        present_at_us: present_at,
        expires_at_us: expires_at,
        ..Default::default()
    }
}

/// Connect a zone publisher to a scene with the default zones.
async fn zone_publisher(
    clock: tze_hud_scene::TestClock,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    tonic::Streaming<ServerMessage>,
    Arc<tokio::sync::Mutex<crate::session::SharedState>>,
    tokio::task::JoinHandle<()>,
    HudSessionClient<tonic::transport::Channel>,
) {
    let (mut client, server, state, _exp) = setup_test_with_lease_expiry_clock(clock).await;
    {
        let st = state.lock().await;
        st.scene.lock().await.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
    }
    let (tx, _init, stream) = handshake_with_psk(&mut client, "zone-agent", "test-key").await;
    (tx, stream, state, server, client)
}

async fn publish_and_ack(
    tx: &tokio::sync::mpsc::Sender<ClientMessage>,
    stream: &mut tonic::Streaming<ServerMessage>,
    sequence: u64,
    publish: Publish,
) -> RequestResult {
    tx.send(ClientMessage {
        sequence,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(publish)),
    })
    .await
    .unwrap();
    match next_server_msg(stream).await.payload {
        Some(ServerPayload::RequestResult(r)) => r,
        other => panic!("expected RequestResult, got {other:?}"),
    }
}

fn subtitle_texts(scene: &SceneGraph) -> Vec<String> {
    scene
        .zone_registry
        .active_publishes
        .get("subtitle")
        .into_iter()
        .flatten()
        .filter_map(|r| match &r.content {
            ZoneContent::StreamText(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

/// Invariant 1: Publish `ttl_ms` sets a content expiry that the sweep
/// honors without the agent returning.
#[tokio::test]
async fn grpc_zone_publish_ttl_sets_expiry_and_is_swept() {
    let clock = wall_aligned_clock();
    let (tx, mut stream, state, server, _client) = zone_publisher(clock.clone()).await;

    let ack = publish_and_ack(&tx, &mut stream, 2, subtitle_publish("brief", 8_000, 0, 0)).await;
    assert!(ack.ok, "{ack:?}");

    let st = state.lock().await;
    let mut scene = st.scene.lock().await;
    let now_us = scene.now_wall_us();
    let record = &scene.zone_registry.active_publishes["subtitle"][0];
    assert_eq!(record.expires_at_wall_us, Some(now_us + 8_000_000));
    clock.advance(7_999);
    scene.drain_expired_zone_publications();
    assert_eq!(subtitle_texts(&scene), vec!["brief".to_string()]);
    clock.advance(2);
    scene.drain_expired_zone_publications();
    assert!(
        subtitle_texts(&scene).is_empty(),
        "expired content is swept"
    );
    drop(scene);
    drop(st);
    server.abort();
}

/// Invariant 1: ZonePublish `expires_at_wall_us` is honored as an absolute
/// content expiry.
#[tokio::test]
async fn grpc_zone_publish_expires_at_is_swept() {
    let clock = wall_aligned_clock();
    let (tx, mut stream, state, server, _client) = zone_publisher(clock.clone()).await;
    let expires_at = clock.now_us() + 3_000_000;

    let ack = publish_and_ack(
        &tx,
        &mut stream,
        2,
        subtitle_publish("until", 0, 0, expires_at),
    )
    .await;
    assert!(ack.ok, "{ack:?}");

    let st = state.lock().await;
    let mut scene = st.scene.lock().await;
    assert_eq!(
        scene.zone_registry.active_publishes["subtitle"][0].expires_at_wall_us,
        Some(expires_at)
    );
    clock.advance(3_001);
    scene.drain_expired_zone_publications();
    assert!(subtitle_texts(&scene).is_empty());
    drop(scene);
    drop(st);
    server.abort();
}

/// Invariant 1: a ZonePublish with a future `present_at` is accepted but not
/// shown until due; it then carries its ttl from the presentation time.
#[tokio::test]
async fn grpc_zone_publish_present_at_is_held_until_due() {
    let clock = wall_aligned_clock();
    let (tx, mut stream, state, server, _client) = zone_publisher(clock.clone()).await;
    let present_at = clock.now_us() + 5_000_000;

    let ack = publish_and_ack(
        &tx,
        &mut stream,
        2,
        subtitle_publish("later", 2_000, present_at, 0),
    )
    .await;
    assert!(ack.ok, "{ack:?}");

    let st = state.lock().await;
    let mut scene = st.scene.lock().await;
    assert!(subtitle_texts(&scene).is_empty(), "not shown early");
    assert_eq!(scene.next_timed_content_wall_us(), Some(present_at));
    clock.advance(4_999);
    assert!(scene.apply_due_batches().is_empty());
    assert!(subtitle_texts(&scene).is_empty(), "still not due");
    clock.advance(1);
    let applied = scene.apply_due_batches();
    assert_eq!(applied.len(), 1);
    assert!(applied[0].applied);
    assert_eq!(subtitle_texts(&scene), vec!["later".to_string()]);
    assert_eq!(
        scene.zone_registry.active_publishes["subtitle"][0].expires_at_wall_us,
        Some(present_at + 2_000_000)
    );
    drop(scene);
    drop(st);
    server.abort();
}

/// Connect a tile agent with a lease and one tile on an active tab.
async fn tile_agent(
    clock: tze_hud_scene::TestClock,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    tonic::Streaming<ServerMessage>,
    Arc<tokio::sync::Mutex<crate::session::SharedState>>,
    tokio::task::JoinHandle<()>,
    (Vec<u8>, Vec<u8>),
    HudSessionClient<tonic::transport::Channel>,
) {
    let (mut client, server, state, _exp) = setup_test_with_lease_expiry_clock(clock).await;
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let tab = scene.create_tab("Main", 0).unwrap();
        scene.active_tab = Some(tab);
    }
    let (tx, _init, mut stream) = handshake_with_psk(&mut client, "tile-agent", "test-key").await;
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 600_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let claimed = match next_server_msg(&mut stream).await.payload {
        Some(ServerPayload::RequestResult(r)) if r.ok => (r.lease_id, r.ids[0].clone()),
        other => panic!("expected ClaimTile result, got {other:?}"),
    };
    (tx, stream, state, server, claimed, client)
}

fn set_root_proto(tile_id: &[u8]) -> crate::proto::MutationProto {
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.2, 0.4, 0.6, 1.0),
            bounds: Rect::new(0.0, 0.0, 120.0, 48.0),
            radius: None,
        }),
    };
    crate::proto::MutationProto {
        mutation: Some(crate::proto::mutation_proto::Mutation::SetTileRoot(
            crate::proto::SetTileRootMutation {
                tile_id: tile_id.to_vec(),
                node: Some(crate::convert::scene_node_to_proto(&node)),
            },
        )),
    }
}

async fn send_batch(
    tx: &tokio::sync::mpsc::Sender<ClientMessage>,
    stream: &mut tonic::Streaming<ServerMessage>,
    sequence: u64,
    lease_id: &[u8],
    mutations: Vec<crate::proto::MutationProto>,
    timing: Option<TimingHints>,
) -> ServerMessage {
    tx.send(ClientMessage {
        sequence,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
            lease_id: lease_id.to_vec(),
            mutations,
            timing,
        })),
    })
    .await
    .unwrap();
    next_server_msg(stream).await
}

/// Invariant 1: a MutationBatch with a future `present_at` is accepted but
/// held; the tile's content appears only once the batch is due.
#[tokio::test]
async fn grpc_batch_present_at_holds_content_until_due() {
    let clock = wall_aligned_clock();
    let (tx, mut stream, state, server, (lease_id, tile_id_bytes), _client) =
        tile_agent(clock.clone()).await;
    let tile_id = bytes_to_scene_id(&tile_id_bytes).unwrap();

    let present_at = clock.now_us() + 5_000_000;
    let timing = Some(TimingHints {
        present_at_wall_us: present_at,
        expires_at_wall_us: 0,
    });
    match send_batch(
        &tx,
        &mut stream,
        4,
        &lease_id,
        vec![set_root_proto(&tile_id_bytes)],
        timing,
    )
    .await
    .payload
    {
        Some(ServerPayload::RequestResult(r)) => assert!(r.ok, "{r:?}"),
        other => panic!("expected MutationResult, got {other:?}"),
    }

    let st = state.lock().await;
    let mut scene = st.scene.lock().await;
    assert!(scene.tiles[&tile_id].root_node.is_none(), "not shown early");
    clock.advance(4_999);
    assert!(scene.apply_due_batches().is_empty());
    assert!(scene.tiles[&tile_id].root_node.is_none());
    clock.advance(1);
    let applied = scene.apply_due_batches();
    assert!(applied[0].applied, "{:?}", applied[0].error);
    assert!(scene.tiles[&tile_id].root_node.is_some(), "shown when due");
    drop(scene);
    drop(st);
    server.abort();
}

/// Invariant 1: a MutationBatch `expires_at` stamps the tiles it targets, and
/// the runtime sweep removes them on schedule.
#[tokio::test]
async fn grpc_batch_expires_at_sweeps_tile() {
    let clock = wall_aligned_clock();
    let (tx, mut stream, state, server, (lease_id, tile_id_bytes), _client) =
        tile_agent(clock.clone()).await;
    let tile_id = bytes_to_scene_id(&tile_id_bytes).unwrap();
    let expires_at = clock.now_us() + 8_000_000;
    let timing = Some(TimingHints {
        present_at_wall_us: 0,
        expires_at_wall_us: expires_at,
    });
    match send_batch(
        &tx,
        &mut stream,
        3,
        &lease_id,
        vec![set_root_proto(&tile_id_bytes)],
        timing,
    )
    .await
    .payload
    {
        Some(ServerPayload::RequestResult(r)) => assert!(r.ok, "{r:?}"),
        other => panic!("expected RequestResult, got {other:?}"),
    }

    let st = state.lock().await;
    let mut scene = st.scene.lock().await;
    assert_eq!(scene.tiles[&tile_id].expires_at, Some(expires_at));
    assert_eq!(scene.next_timed_content_wall_us(), Some(expires_at));
    clock.advance(7_999);
    assert!(scene.drain_expired_tiles().is_empty());
    clock.advance(2);
    assert_eq!(scene.drain_expired_tiles(), vec![tile_id]);
    assert_eq!(scene.tile_count(), 0);
    drop(scene);
    drop(st);
    server.abort();
}

// ─── Lifecycle verbs (docs/api.md) ──────────────────────────────────────────

/// Send one client payload and return the next `RequestResult`.
async fn request(
    tx: &tokio::sync::mpsc::Sender<ClientMessage>,
    stream: &mut tonic::Streaming<ServerMessage>,
    sequence: u64,
    payload: ClientPayload,
) -> RequestResult {
    tx.send(ClientMessage {
        sequence,
        timestamp_wall_us: now_wall_us(),
        payload: Some(payload),
    })
    .await
    .unwrap();
    loop {
        if let Some(ServerPayload::RequestResult(r)) = next_server_msg(stream).await.payload {
            return r;
        }
    }
}

/// A connected agent on a scene with an active tab and the default zones.
async fn verb_agent(
    agent_id: &str,
) -> (
    tokio::sync::mpsc::Sender<ClientMessage>,
    tonic::Streaming<ServerMessage>,
    Arc<Mutex<SharedState>>,
    tokio::task::JoinHandle<()>,
    HudSessionClient<tonic::transport::Channel>,
) {
    let (mut client, server, state) = setup_test_with_state().await;
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let tab = scene.create_tab("main", 0).expect("create tab");
        scene.active_tab = Some(tab);
        scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
    }
    let (tx, _init, stream) = handshake(&mut client, agent_id, "test-key").await;
    (tx, stream, state, server, client)
}

fn claim(
    anchor: TileAnchor,
    size: TileSize,
    root: Option<crate::proto::NodeProto>,
) -> ClientPayload {
    ClientPayload::ClaimTile(ClaimTile {
        placement: Some(TilePlacement {
            anchor: anchor as i32,
            size: size as i32,
        }),
        ttl_ms: 60_000,
        root,
    })
}

fn tile_surface_of(id: &[u8]) -> String {
    super::verbs::tile_surface(bytes_to_scene_id(id).unwrap())
}

/// ClaimTile grants the lease, places the tile from tokens, and applies the
/// initial tree in one round trip; client node ids come back in `ids`.
#[tokio::test]
async fn claim_tile_is_one_round_trip_with_root_and_ids() {
    let (tx, mut stream, state, server, _client) = verb_agent("claim-agent").await;
    let child_id = SceneId::new();
    let mut root = crate::convert::scene_node_to_proto(&Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.1, 0.2, 0.3, 1.0),
            bounds: Rect::new(0.0, 0.0, 100.0, 50.0),
            radius: None,
        }),
    });
    root.children
        .push(crate::convert::scene_node_to_proto(&Node {
            layout: Default::default(),
            id: child_id,
            children: vec![],
            data: NodeData::SolidColor(SolidColorNode {
                color: Rgba::new(0.4, 0.5, 0.6, 1.0),
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                radius: None,
            }),
        }));
    let r = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::TopRight, TileSize::Small, Some(root)),
    )
    .await;
    assert!(r.ok, "{r:?}");
    assert_eq!(r.seq, 2);
    assert_eq!(r.ids.len(), 3, "tile id, then root and child");
    assert_eq!(r.lease_id.len(), 16);
    assert_eq!(r.ttl_ms, 60_000);
    assert_eq!(bytes_to_scene_id(&r.ids[2]).unwrap(), child_id);

    let tile_id = bytes_to_scene_id(&r.ids[0]).unwrap();
    let st = state.lock().await;
    let scene = st.scene.lock().await;
    let tile = &scene.tiles[&tile_id];
    let tokens = tze_hud_scene::placement::TilePlacementTokens::default();
    assert_eq!(tile.bounds.width, tokens.small.0);
    assert_eq!(
        tile.bounds.x,
        scene.display_area.width - tokens.margin - tokens.small.0
    );
    assert_eq!(tile.bounds.y, tokens.margin);
    assert!(tile.root_node.is_some(), "root applied in the same request");
    drop(scene);
    drop(st);
    server.abort();
}

/// Tiles claimed at one anchor stack instead of overlapping.
#[tokio::test]
async fn claim_tile_stacks_at_shared_anchor() {
    let (tx, mut stream, state, server, _client) = verb_agent("stack-agent").await;
    let a = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::TopLeft, TileSize::Small, None),
    )
    .await;
    let b = request(
        &tx,
        &mut stream,
        3,
        claim(TileAnchor::TopLeft, TileSize::Small, None),
    )
    .await;
    assert!(a.ok && b.ok);
    let st = state.lock().await;
    let scene = st.scene.lock().await;
    let ta = &scene.tiles[&bytes_to_scene_id(&a.ids[0]).unwrap()];
    let tb = &scene.tiles[&bytes_to_scene_id(&b.ids[0]).unwrap()];
    assert!(
        !ta.bounds.intersects(&tb.bounds),
        "stacked, not overlapping"
    );
    assert!(tb.z_order > ta.z_order, "later claims sit above");
    drop(scene);
    drop(st);
    server.abort();
}

/// A retransmitted ClaimTile replays its reply rather than claiming twice.
#[tokio::test]
async fn claim_tile_retransmit_replays_cached_result() {
    let (tx, mut stream, state, server, _client) = verb_agent("retx-agent").await;
    let first = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Top, TileSize::Small, None),
    )
    .await;
    let again = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Top, TileSize::Small, None),
    )
    .await;
    assert_eq!(first, again);
    assert_eq!(state.lock().await.scene.lock().await.tile_count(), 1);
    server.abort();
}

/// Hold renews a tile's lease; Clear releases it and removes the tile.
#[tokio::test]
async fn hold_and_clear_tile() {
    let (tx, mut stream, state, server, _client) = verb_agent("hold-agent").await;
    let c = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Center, TileSize::Medium, None),
    )
    .await;
    let surface = tile_surface_of(&c.ids[0]);
    let held = request(
        &tx,
        &mut stream,
        3,
        ClientPayload::Hold(Hold {
            surface: surface.clone(),
            ttl_ms: 90_000,
        }),
    )
    .await;
    assert!(held.ok, "{held:?}");
    assert_eq!(held.ttl_ms, 90_000);
    assert_eq!(held.lease_id, c.lease_id);

    let cleared = request(
        &tx,
        &mut stream,
        4,
        ClientPayload::Clear(Clear {
            surface: surface.clone(),
        }),
    )
    .await;
    assert!(cleared.ok, "{cleared:?}");
    assert_eq!(state.lock().await.scene.lock().await.tile_count(), 0);

    let again = request(
        &tx,
        &mut stream,
        5,
        ClientPayload::Hold(Hold { surface, ttl_ms: 0 }),
    )
    .await;
    assert_eq!(again.code, "NOT_HELD");
    server.abort();
}

/// Hold on a zone extends the agent's publication; with nothing held it
/// answers NOT_HELD.
#[tokio::test]
async fn hold_zone_publication() {
    let (tx, mut stream, state, server, _client) = verb_agent("zone-hold-agent").await;
    let hold = || {
        ClientPayload::Hold(Hold {
            surface: "zone:subtitle".to_string(),
            ttl_ms: 0,
        })
    };
    let r = request(&tx, &mut stream, 2, hold()).await;
    assert_eq!(r.code, "NOT_HELD");
    let p = request(
        &tx,
        &mut stream,
        3,
        ClientPayload::Publish(subtitle_publish("x", 5_000, 0, 0)),
    )
    .await;
    assert!(p.ok, "{p:?}");
    let r = request(&tx, &mut stream, 4, hold()).await;
    assert!(r.ok, "{r:?}");
    let st = state.lock().await;
    let scene = st.scene.lock().await;
    let record = &scene.zone_registry.active_publishes["subtitle"][0];
    assert_eq!(
        record.expires_at_wall_us, None,
        "ttl_ms 0 holds until cleared"
    );
    drop(scene);
    drop(st);
    server.abort();
}

/// Publish then Clear on a zone surface.
#[tokio::test]
async fn clear_zone_publication() {
    let (tx, mut stream, state, server, _client) = verb_agent("zone-clear-agent").await;
    let p = request(
        &tx,
        &mut stream,
        2,
        ClientPayload::Publish(subtitle_publish("bye", 0, 0, 0)),
    )
    .await;
    assert!(p.ok, "{p:?}");
    let c = request(
        &tx,
        &mut stream,
        3,
        ClientPayload::Clear(Clear {
            surface: "zone:subtitle".to_string(),
        }),
    )
    .await;
    assert!(c.ok, "{c:?}");
    assert!(subtitle_texts(&*state.lock().await.scene.lock().await).is_empty());
    server.abort();
}

/// Scene validation reasons reach the agent as the hint (no flattening).
#[tokio::test]
async fn publish_to_unknown_zone_names_the_zone() {
    let (tx, mut stream, _state, server, _client) = verb_agent("unknown-zone-agent").await;
    let mut publish = subtitle_publish("x", 0, 0, 0);
    publish.surface = "zone:nope".to_string();
    let r = request(&tx, &mut stream, 2, ClientPayload::Publish(publish)).await;
    assert_eq!(r.code, "ZONE_NOT_FOUND");
    assert!(r.hint.contains("nope"), "{r:?}");
    server.abort();
}

// ─── Allow-list boundary checks ─────────────────────────────────────────────

/// A Publish to a zone outside the agent's allow list is rejected at the
/// boundary.
#[tokio::test]
async fn zone_publish_outside_allow_list_rejected() {
    let (tx, mut stream, _state, server, _client) = verb_agent("widget-no-cap-agent").await;
    let r = request(
        &tx,
        &mut stream,
        2,
        ClientPayload::Publish(subtitle_publish("hello", 0, 0, 0)),
    )
    .await;
    assert!(!r.ok);
    assert_eq!(r.code, "NOT_ALLOWED");
    assert!(r.hint.contains("zone:subtitle"));
    server.abort();
}

/// Clear needs the surface in the allow list.
#[tokio::test]
async fn clear_outside_allow_list_rejected() {
    let (tx, mut stream, _state, server, _client) = verb_agent("widget-no-cap-agent").await;
    for (seq, surface, entry) in [
        (2, "zone:subtitle", "zone:subtitle"),
        (3, "widget:gauge", "widget:gauge"),
    ] {
        let r = request(
            &tx,
            &mut stream,
            seq,
            ClientPayload::Clear(Clear {
                surface: surface.to_string(),
            }),
        )
        .await;
        assert_eq!(r.code, "NOT_ALLOWED");
        assert!(r.hint.contains(entry), "{r:?}");
    }
    server.abort();
}

/// ClaimTile needs `tiles` in the allow list.
#[tokio::test]
async fn claim_tile_without_tiles_allow_rejected() {
    let (tx, mut stream, _state, server, _client) = verb_agent("zone-only-agent").await;
    let r = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Top, TileSize::Small, None),
    )
    .await;
    assert_eq!(r.code, "NOT_ALLOWED");
    assert!(r.hint.contains("tiles"));
    server.abort();
}

/// With `tiles` allowed, the same ClaimTile succeeds.
#[tokio::test]
async fn claim_tile_with_tiles_allow_accepted() {
    let (tx, mut stream, _state, server, _client) = verb_agent("widget-no-cap-agent").await;
    let r = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Top, TileSize::Small, None),
    )
    .await;
    assert!(r.ok, "{}: {}", r.code, r.hint);
    server.abort();
}

/// CreateTile and the portal mutations are runtime-internal: an agent batch
/// carrying one is rejected with a hint naming the replacement.
#[tokio::test]
async fn internal_mutations_rejected_from_agents() {
    let (tx, mut stream, _state, server, _client) = verb_agent("internal-agent").await;
    let c = request(
        &tx,
        &mut stream,
        2,
        claim(TileAnchor::Top, TileSize::Small, None),
    )
    .await;
    let create =
        crate::proto::mutation_proto::Mutation::CreateTile(crate::proto::CreateTileMutation {
            tab_id: vec![],
            bounds: None,
            z_order: 1,
        });
    let accent = crate::proto::mutation_proto::Mutation::SetTileUnreadCount(
        crate::proto::SetTileUnreadCountMutation {
            tile_id: c.ids[0].clone(),
            count: 3,
        },
    );
    for (seq, m, needle) in [(3, create, "ClaimTile"), (4, accent, "runtime-internal")] {
        let r = request(
            &tx,
            &mut stream,
            seq,
            ClientPayload::MutationBatch(MutationBatch {
                batch_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
                lease_id: c.lease_id.clone(),
                mutations: vec![crate::proto::MutationProto { mutation: Some(m) }],
                timing: None,
            }),
        )
        .await;
        assert_eq!(r.code, "INVALID_ARGUMENT");
        assert!(r.hint.contains(needle), "{r:?}");
    }
    server.abort();
}
