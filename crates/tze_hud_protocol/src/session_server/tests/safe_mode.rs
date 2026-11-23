use super::*;

/// Safe mode rejects durable publishing while heartbeats and resume remain usable.
#[tokio::test]
async fn test_safe_mode_rejects_mutations() {
    let (tx, mut stream, state, server, _client) = verb_agent("safe-mode-agent").await;
    assert!(
        request(
            &tx,
            &mut stream,
            2,
            ClientPayload::Publish(subtitle_publish("before", 0, 0, 0))
        )
        .await
        .ok
    );
    state
        .lock()
        .await
        .safe_mode_atomic
        .store(true, std::sync::atomic::Ordering::Release);
    let rejected = request(
        &tx,
        &mut stream,
        3,
        ClientPayload::Publish(subtitle_publish("paused", 0, 0, 0)),
    )
    .await;
    assert_eq!(rejected.code, "SAFE_MODE_ACTIVE");
    assert_eq!(
        subtitle_texts(&*state.lock().await.scene.lock().await),
        ["before"]
    );
    tx.send(ClientMessage {
        sequence: 4,
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 999,
        })),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(matches!(
        next_server_msg(&mut stream).await.payload,
        Some(ServerPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 999
        }))
    ));
    state
        .lock()
        .await
        .safe_mode_atomic
        .store(false, std::sync::atomic::Ordering::Release);
    assert!(
        request(
            &tx,
            &mut stream,
            5,
            ClientPayload::Publish(subtitle_publish("after", 0, 0, 0))
        )
        .await
        .ok
    );
    assert_eq!(
        subtitle_texts(&*state.lock().await.scene.lock().await),
        ["after"]
    );
    server.abort();

    // Fresh Hold requests must observe the same paused admission boundary as
    // Publish. Keep real active and delayed content on both transport surfaces.
    let service = setup_widget_service().await;
    let state = Arc::clone(&service.state);
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        scene.zone_registry = ZoneRegistry::with_defaults();
        scene
            .zone_registry
            .zones
            .get_mut("notification-area")
            .unwrap()
            .ephemeral = false;
    }
    let (mut client, server) = setup_widget_test_with_service(service).await;
    let (tx, _, mut stream) = handshake_with_capabilities(
        &mut client,
        "paused-hold-agent",
        "test-key",
        &["publish_zone:notification-area", "publish_widget:gauge"],
    )
    .await;
    let snapshot = |scene: &SceneGraph| {
        serde_json::json!({
            "scene": scene,
            "pending": scene.scheduled_publications.iter().map(|scheduled|
                (scheduled.present_at_wall_us, format!("{:?}", (&scheduled.publisher, &scheduled.publication)))).collect::<Vec<_>>(),
        })
    };
    let mut sequence = 2;
    let mut completed = Vec::new();
    for surface in ["zone:notification-area", "widget:gauge"] {
        for delayed in [false, true] {
            let mut publish = Publish {
                surface: surface.into(),
                ttl_ms: 5_000,
                present_at_us: if delayed {
                    now_wall_us() + 60_000_000
                } else {
                    0
                },
                ..Default::default()
            };
            if surface.starts_with("zone:") {
                publish.content = Some(crate::proto::ZoneContent {
                    payload: Some(crate::proto::zone_content::Payload::Notification(
                        crate::proto::NotificationPayload {
                            text: "Question".into(),
                            actions: vec![crate::proto::NotificationActionProto {
                                copy: None,
                                label: "Yes".into(),
                                callback_id: "yes".into(),
                                url: String::new(),
                            }],
                            ..Default::default()
                        },
                    )),
                });
            } else {
                publish.params = vec![crate::proto::WidgetParameterValueProto {
                    param_name: "level".into(),
                    value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                        0.5,
                    )),
                }];
            }
            assert!(
                request(&tx, &mut stream, sequence, ClientPayload::Publish(publish))
                    .await
                    .ok
            );
            sequence += 1;
        }
        let hold = ClientPayload::Hold(Hold {
            surface: surface.into(),
            ttl_ms: 2_500,
        });
        let result = request(&tx, &mut stream, sequence, hold.clone()).await;
        assert!(result.ok);
        completed.push((sequence, hold, result));
        sequence += 1;
    }
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        st.safe_mode_atomic
            .store(true, std::sync::atomic::Ordering::Release);
        scene.pause_widget_ttls();
        let now = scene.now_millis();
        scene.suspend_all_leases(now);
        assert_eq!(scene.scheduled_publications.len(), 2);
    }
    let mut failures = Vec::new();
    for surface in ["zone:notification-area", "widget:gauge"] {
        let before = snapshot(&*state.lock().await.scene.lock().await);
        let result = request(
            &tx,
            &mut stream,
            sequence,
            ClientPayload::Hold(Hold {
                surface: surface.into(),
                ttl_ms: 0,
            }),
        )
        .await;
        sequence += 1;
        if result.ok || result.code != "SAFE_MODE_ACTIVE" {
            failures.push(format!("paused {surface} Hold returned {result:?}"));
        }
        if snapshot(&*state.lock().await.scene.lock().await) != before {
            failures.push(format!(
                "paused {surface} Hold changed active/pending scene state"
            ));
        }
    }
    // A completed sequence replays its old success, even during pause, without
    // retiming anything. Refusing fresh requests must not invalidate that cache.
    for (old_sequence, hold, result) in completed {
        let before = snapshot(&*state.lock().await.scene.lock().await);
        assert_eq!(request(&tx, &mut stream, old_sequence, hold).await, result);
        assert_eq!(snapshot(&*state.lock().await.scene.lock().await), before);
    }
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let now = scene.now_millis();
        scene.resume_all_leases(now);
        scene.resume_widget_ttls();
        st.safe_mode_atomic
            .store(false, std::sync::atomic::Ordering::Release);
    }
    for surface in ["zone:notification-area", "widget:gauge"] {
        for ttl_ms in [3_000, 0] {
            let result = request(
                &tx,
                &mut stream,
                sequence,
                ClientPayload::Hold(Hold {
                    surface: surface.into(),
                    ttl_ms,
                }),
            )
            .await;
            assert!(result.ok, "resumed {surface}: {result:?}");
            assert_eq!(result.ttl_ms, ttl_ms);
            sequence += 1;
        }
    }
    {
        let st = state.lock().await;
        let scene = st.scene.lock().await;
        assert!(
            scene.zone_registry.active_for_zone("notification-area")[0]
                .expires_at_wall_us
                .is_none()
        );
        assert!(
            scene.widget_registry.active_for_widget("gauge")[0]
                .expires_at_wall_us
                .is_none()
        );
        for pending in &scene.scheduled_publications {
            match &pending.publication {
                tze_hud_scene::Publication::Zone {
                    expires_at_wall_us, ..
                }
                | tze_hud_scene::Publication::Widget {
                    expires_at_wall_us, ..
                } => assert!(
                    expires_at_wall_us.is_none(),
                    "resumed Hold0 reaches pending content"
                ),
            }
        }
    }
    server.abort();
    assert!(failures.is_empty(), "paused Hold failures: {failures:?}");
}
