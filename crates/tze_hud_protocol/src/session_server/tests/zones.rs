use super::*;

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

// ─── Zone publish acknowledgement ────────────────────────────────────────────

/// Scenario: Ephemeral zone publish is fire-and-forget — no ZonePublishResult.
/// WHEN agent publishes to an ephemeral zone (zone.ephemeral=true)
/// THEN runtime does NOT send a ZonePublishResult
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

/// Scenario: Durable zone publish is acknowledged.
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
