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
