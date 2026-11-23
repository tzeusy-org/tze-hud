use super::*;

/// Manual directory changes apply to fresh authentication, while an
/// established stream keeps its captured permissions until reconnect.
#[tokio::test]
async fn fresh_init_uses_current_credentials_and_permissions() {
    use tze_hud_scene::config::{AgentDirectory, hash_psk};

    let directory = |key: Option<&str>, reduced: bool| {
        let mut paired = AgentDirectory::default();
        if let Some(key) = key {
            let permission = if reduced {
                "publish_zone:reload-control"
            } else {
                "*"
            };
            paired.insert(
                "auth-check-agent",
                hash_psk(key),
                vec![permission.to_string()],
            );
        }
        paired.insert("control", hash_psk("control-key"), vec!["*".to_string()]);
        paired
    };

    for (change, current_key) in [
        ("deleted", None),
        ("rekeyed", Some("new-key")),
        ("allow-reduced", Some("old-key")),
    ] {
        let mut scene = SceneGraph::new(800.0, 600.0);
        scene.create_tab("main", 0).unwrap();
        scene.zone_registry = ZoneRegistry::with_defaults();
        let mut control_zone = scene.zone_registry.get_by_name("subtitle").unwrap().clone();
        control_zone.id = SceneId::new();
        control_zone.name = "reload-control".to_string();
        scene.zone_registry.register(control_zone);
        let agents = directory(Some("old-key"), false).shared();
        let mut service = HudSessionImpl::new(scene, "unused-dev-key");
        service.agents = Arc::clone(&agents);
        let (mut client, server) = setup_widget_test_with_service(service).await;

        let (old_tx, _, mut old_stream) =
            handshake(&mut client, "auth-check-agent", "old-key").await;
        let before = request(
            &old_tx,
            &mut old_stream,
            2,
            ClientPayload::Publish(subtitle_publish("before reload", 0, 0, 0)),
        )
        .await;
        assert!(before.ok, "{change}: {before:?}");
        agents.store(Arc::new(directory(current_key, true)));
        let after = request(
            &old_tx,
            &mut old_stream,
            3,
            ClientPayload::Publish(subtitle_publish("established after reload", 0, 0, 0)),
        )
        .await;
        assert!(
            after.ok,
            "{change}: established permissions changed: {after:?}"
        );

        let mut attempts = vec!["old-key"];
        if current_key == Some("new-key") {
            attempts.push("new-key");
        }
        for key in attempts {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let credential = Some(crate::auth::psk_credential(key.to_string()));
            let payload = ClientPayload::SessionInit(SessionInit {
                agent_id: "auth-check-agent".to_string(),
                initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
                auth_credential: credential,
            });
            tx.send(ClientMessage {
                sequence: 1,
                timestamp_wall_us: now_wall_us(),
                payload: Some(payload),
            })
            .await
            .unwrap();
            let mut stream = client
                .session(tokio_stream::wrappers::ReceiverStream::new(rx))
                .await
                .unwrap()
                .into_inner();
            let first = next_server_msg(&mut stream).await;
            if current_key != Some(key) {
                match first.payload {
                    Some(ServerPayload::SessionError(err)) => {
                        assert_eq!(err.code, "AUTH_FAILED", "{change}: stale key")
                    }
                    other => panic!("{change}: stale key accepted: {other:?}"),
                }
                continue;
            }
            match first.payload {
                Some(ServerPayload::SessionEstablished(established)) => {
                    assert_eq!(established.namespace, "auth-check-agent");
                    assert!(
                        !established
                            .active_subscriptions
                            .iter()
                            .any(|s| s == "SCENE_TOPOLOGY")
                    );
                }
                other => panic!("{change}: handshake failed: {other:?}"),
            }
            // Drain the existing initial state through its terminal notice;
            // the assertions below exercise the real active session boundary.
            loop {
                if matches!(
                    next_server_msg(&mut stream).await.payload,
                    Some(ServerPayload::DegradationNotice(_))
                ) {
                    break;
                }
            }
            let denied = request(
                &tx,
                &mut stream,
                2,
                ClientPayload::Publish(subtitle_publish("new permissions", 0, 0, 0)),
            )
            .await;
            assert!(!denied.ok, "{change}: retained old allow");
            assert_eq!(denied.code, "NOT_ALLOWED");
            let mut retained = subtitle_publish("retained permission", 0, 0, 0);
            retained.surface = "zone:reload-control".to_string();
            let allowed = request(&tx, &mut stream, 3, ClientPayload::Publish(retained)).await;
            assert!(allowed.ok, "{change}: fresh init: {allowed:?}");
        }

        let (control_tx, _, mut control_stream) =
            handshake(&mut client, "control", "control-key").await;
        let unaffected = request(
            &control_tx,
            &mut control_stream,
            2,
            ClientPayload::Publish(subtitle_publish("unaffected agent", 0, 0, 0)),
        )
        .await;
        assert!(unaffected.ok, "{change}: unaffected agent: {unaffected:?}");
        server.abort();
    }
}
