use super::*;

// ─── DegradationNotice ───────────────────────────────────────────────────────

#[tokio::test]
async fn new_session_receives_existing_degradation_after_established() {
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
    match &messages[1].payload {
        Some(ServerPayload::DegradationNotice(notice)) => {
            assert_eq!(notice.level, DegradationLevel::RenderingSimplified as i32);
        }
        other => panic!("expected current degradation second, got {other:?}"),
    }
}

/// Scenario: WHEN runtime enters RENDERING_SIMPLIFIED degradation level,
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

// Local geometry reset remains runtime-owned after resident feed retirement.

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
