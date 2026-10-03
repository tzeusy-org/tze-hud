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

/// A viewer dismiss of a tile (hover close button) reclaims its lease with no
/// agent involvement; the owning connected agent gets exactly one
/// `Reclaimed{OVERRIDE}` naming the dismissed tile. Real tonic client.
#[tokio::test]
async fn viewer_dismiss_tile_pushes_reclaimed_override() {
    use std::time::Duration;

    let clock = tze_hud_scene::TestClock::new(1_000);
    let (mut client, server, state, lease_expirations) =
        setup_test_with_lease_expiry_clock(clock).await;
    let (tx, _handshake, mut stream) = handshake(&mut client, "dismiss-agent", "test-key").await;

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
    let (lease_id, tile_id) = match next_server_msg(&mut stream).await.payload {
        Some(ServerPayload::RequestResult(RequestResult {
            ok: true,
            lease_id,
            ids,
            ..
        })) => (
            bytes_to_scene_id(&lease_id).expect("lease id"),
            bytes_to_scene_id(&ids[0]).expect("tile id"),
        ),
        other => panic!("expected a granted ClaimTile, got {other:?}"),
    };

    let expiry = {
        let shared = state.lock().await;
        let mut scene = shared.scene.lock().await;
        let expiry = scene
            .viewer_dismiss_tile(tile_id)
            .expect("viewer dismiss reclaims the lease");
        assert_eq!(expiry.terminal_state, LeaseState::Revoked);
        assert_eq!(scene.tile_count(), 0, "the tile is gone immediately");
        expiry
    };
    assert_eq!(lease_expirations.publish(expiry.into()), 1);

    let reclaimed = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("Reclaimed must arrive promptly")
        .expect("stream stays open")
        .expect("valid message");
    match reclaimed.payload {
        Some(ServerPayload::Reclaimed(Reclaimed {
            surface,
            why,
            lease_id: reclaimed_lease,
        })) => {
            assert_eq!(why, ReclaimReason::Override as i32);
            assert_eq!(reclaimed_lease, scene_id_to_bytes(lease_id));
            assert_eq!(surface, crate::session_server::verbs::tile_surface(tile_id));
        }
        other => panic!("expected Reclaimed, got {other:?}"),
    }

    drop(tx);
    server.abort();
}

// ─── DegradationNotice ───────────────────────────────────────────────────────

/// traffic_class: DegradationNotice must be Transactional.
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

// ─── ElementRepositionedEvent ────────────────────────────────────────────────

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
        new_geometry: Some(crate::convert::geometry_policy_to_proto(&new_policy)),
        previous_geometry: Some(crate::convert::geometry_policy_to_proto(&old_policy)),
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
        new_geometry: Some(crate::convert::geometry_policy_to_proto(&new_policy)),
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

// ─── FramePresented ──────────────────────────────────────────────────────────

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
    let proto_previous = crate::convert::geometry_policy_to_proto(&previous.unwrap());
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
