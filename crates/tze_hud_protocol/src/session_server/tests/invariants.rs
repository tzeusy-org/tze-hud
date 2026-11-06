use super::*;

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
            auth_credential: Some(crate::auth::psk_credential(psk.to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let mut messages = Vec::new();
    for _ in 0..2 {
        if let Some(msg) = response_stream.next().await {
            messages.push(msg.unwrap());
        }
    }
    (tx, messages, response_stream)
}

/// Wall-clock-aligned test clock so wire timestamps (validated against the
/// real wall clock) and scene deadlines (scene clock) agree.
fn wall_aligned_clock() -> tze_hud_scene::TestClock {
    tze_hud_scene::TestClock::new(now_ms())
}

/// A watchdog bounds a stuck harness; only the actual cleanup signal establishes
/// completion. Semantic TTL/grace time is advanced exclusively by TestClock.
async fn await_session_cleanup(cleanup: tokio::sync::oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .expect("real session cleanup did not complete")
        .expect("cleanup witness dropped before normal cleanup completed");
}

/// Finite surfaces for the retained lifecycle fixtures, with independent keys
/// so same-namespace sessions can coexist without changing contention semantics.
fn configure_lifetime_surfaces(scene: &mut SceneGraph) {
    use tze_hud_scene::types::*;
    if scene.widget_registry.instances.contains_key("gauge") {
        return;
    }
    scene.zone_registry = ZoneRegistry::with_defaults();
    scene
        .zone_registry
        .zones
        .get_mut("subtitle")
        .unwrap()
        .contention_policy = ContentionPolicy::MergeByKey { max_keys: 16 };
    let tab_id = scene
        .active_tab
        .unwrap_or_else(|| scene.create_tab("Main", 0).unwrap());
    scene.widget_registry.register_definition(WidgetDefinition {
        id: "gauge".into(),
        name: "Gauge".into(),
        description: String::new(),
        parameter_schema: vec![WidgetParameterDeclaration {
            name: "level".into(),
            param_type: WidgetParamType::F32,
            default_value: WidgetParameterValue::F32(0.0),
            constraints: None,
        }],
        layers: Vec::new(),
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.1,
            height_pct: 0.1,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::MergeByKey { max_keys: 16 },
        max_publishers: 16,
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
        current_params: Default::default(),
        auto_clear_ms: None,
    });
}

fn lifetime_widget_publish(key: &str, level: f32) -> Publish {
    Publish {
        surface: "widget:gauge".into(),
        key: key.into(),
        params: vec![crate::proto::WidgetParameterValueProto {
            param_name: "level".into(),
            value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                level,
            )),
        }],
        ..Default::default()
    }
}

fn assert_lifetime_owner(scene: &SceneGraph, origin: SceneId, lease: Option<SceneId>) {
    let key = origin.to_string();
    let zone = scene.zone_registry.active_publishes["subtitle"]
        .iter()
        .find(|r| r.merge_key.as_deref() == Some(&key))
        .unwrap();
    let widget = scene.widget_registry.active_publishes["gauge"]
        .iter()
        .find(|r| r.merge_key.as_deref() == Some(&key))
        .unwrap();
    assert_eq!(zone.lease_id, lease);
    assert_eq!(widget.lease_id, lease);
    assert_eq!(zone.publication_origin, lease.is_none().then_some(origin));
    assert_eq!(widget.publication_origin, lease.is_none().then_some(origin));
}

struct PublicationSession {
    tx: tokio::sync::mpsc::Sender<ClientMessage>,
    stream: tonic::Streaming<ServerMessage>,
    resume_token: Vec<u8>,
    session_id: SceneId,
}

impl PublicationSession {
    async fn disconnect(self, state: &Arc<Mutex<SharedState>>) -> (Vec<u8>, SceneId) {
        let cleanup = state
            .lock()
            .await
            .sessions
            .observe_cleanup(&self.session_id);
        drop(self.tx);
        drop(self.stream);
        await_session_cleanup(cleanup).await;
        let st = state.lock().await;
        let scene = st.scene.lock().await;
        assert_lifetime_owner(&scene, self.session_id, None);
        assert_eq!(
            scene.next_lease_deadline_ms(SceneGraph::DEFAULT_MAX_SUSPENSION_MS),
            Some(scene.now_millis() + DEFAULT_GRACE_PERIOD_MS)
        );
        (self.resume_token, self.session_id)
    }
}

/// Connect and publish both durable surfaces plus delayed content through the real stream.
async fn connect_publications(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent: &str,
    state: &Arc<Mutex<SharedState>>,
) -> PublicationSession {
    {
        let st = state.lock().await;
        configure_lifetime_surfaces(&mut *st.scene.lock().await);
    }
    let (tx, init, mut stream) = handshake(client, agent, "test-key").await;
    let Some(ServerPayload::SessionEstablished(e)) = &init[0].payload else {
        panic!("expected establishment")
    };
    let resume_token = e.resume_token.clone();
    let session_id = bytes_to_scene_id(&e.session_id).unwrap();
    let invalid = publish_and_ack(
        &tx,
        &mut stream,
        2,
        Publish {
            surface: "widget:gauge".into(),
            params: vec![crate::proto::WidgetParameterValueProto {
                param_name: "unknown".into(),
                value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                    1.0,
                )),
            }],
            ..Default::default()
        },
    )
    .await;
    assert!(!invalid.ok);
    let key = session_id.to_string();
    let mut zone = subtitle_publish("held", 0, 0, 0);
    zone.key = key.clone();
    assert!(publish_and_ack(&tx, &mut stream, 3, zone).await.ok);
    assert!(
        publish_and_ack(&tx, &mut stream, 4, lifetime_widget_publish(&key, 0.75))
            .await
            .ok
    );
    let future = state.lock().await.scene.lock().await.now_wall_us() + 60_000_000;
    let mut delayed = subtitle_publish("delayed", 0, future, 0);
    delayed.key = format!("delayed-{key}");
    assert!(publish_and_ack(&tx, &mut stream, 5, delayed).await.ok);
    {
        let st = state.lock().await;
        assert_lifetime_owner(&*st.scene.lock().await, session_id, None);
    }
    PublicationSession {
        tx,
        stream,
        resume_token,
        session_id,
    }
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

            auth_credential: Some(crate::auth::psk_credential("test-key")),
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

/// Disconnect retains content and sets a finite origin deadline without touching a peer.
#[tokio::test]
async fn grpc_disconnect_orphans_publications_without_touching_peer() {
    let clock = wall_aligned_clock();
    let (mut client, server, state) = setup_test_with_clock(clock.clone()).await;
    let session = connect_publications(&mut client, "orphan-agent", &state).await;
    let peer = connect_publications(&mut client, "orphan-agent", &state).await;
    assert_ne!(session.session_id, peer.session_id);
    let (duplicate_cleanup, mut peer_cleanup) = {
        let mut st = state.lock().await;
        (
            st.sessions.observe_cleanup(&session.session_id),
            st.sessions.observe_cleanup(&peer.session_id),
        )
    };
    let (_, origin) = session.disconnect(&state).await;
    await_session_cleanup(duplicate_cleanup).await;
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        assert_eq!(st.sessions.session_count(), 1);
        assert_lifetime_owner(&scene, origin, None);
        assert_lifetime_owner(&scene, peer.session_id, None);
        assert_eq!(
            peer_cleanup.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        // Independent MCP content in this same namespace is outside the disconnected origin.
        scene
            .publish_to_zone(
                "subtitle",
                ZoneContent::StreamText("MCP".into()),
                "orphan-agent",
                Some("mcp".into()),
                None,
                None,
            )
            .unwrap();
        clock.advance(DEFAULT_GRACE_PERIOD_MS);
        scene.expire_leases();
        assert!(
            !scene.zone_registry.active_publishes["subtitle"]
                .iter()
                .any(|r| r.publication_origin == Some(origin))
        );
        assert!(
            !scene.widget_registry.active_publishes["gauge"]
                .iter()
                .any(|r| r.publication_origin == Some(origin))
        );
        assert!(subtitle_texts(&scene).contains(&"MCP".to_string()));
        assert_lifetime_owner(&scene, peer.session_id, None);
        assert_eq!(
            scene.scheduled_batches.len(),
            1,
            "only the connected peer keeps its delayed publication"
        );
        clock.advance(60_000);
        assert!(scene.apply_due_batches().iter().all(|r| r.applied));
        assert!(
            !scene.zone_registry.active_publishes["subtitle"]
                .iter()
                .any(|r| r.merge_key.as_deref() == Some(&format!("delayed-{origin}")))
        );
    }
    peer.disconnect(&state).await;
    await_session_cleanup(peer_cleanup).await;
    assert_eq!(state.lock().await.sessions.session_count(), 0);
    server.abort();
}

/// Resume restores the stable publication origin while the physical session stays fresh.
#[tokio::test]
async fn grpc_resume_within_grace_restores_publication_origin() {
    let clock = wall_aligned_clock();
    let (mut client, server, state) = setup_test_with_clock(clock.clone()).await;
    let session = connect_publications(&mut client, "resume-agent", &state).await;
    let (token, origin) = session.disconnect(&state).await;
    clock.advance(DEFAULT_GRACE_PERIOD_MS - 1);
    let (tx, mut stream) = send_resume(&mut client, "resume-agent", token).await;
    assert!(matches!(
        next_server_msg(&mut stream).await.payload,
        Some(ServerPayload::SessionResumeResult(SessionResumeResult {
            accepted: true,
            ..
        }))
    ));
    assert!(matches!(
        next_server_msg(&mut stream).await.payload,
        Some(ServerPayload::DegradationNotice(_))
    ));
    let registry_id = {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        clock.advance(2);
        scene.expire_leases();
        assert_lifetime_owner(&scene, origin, None);
        assert_eq!(scene.scheduled_batches.len(), 1);
        assert_eq!(
            scene.next_lease_deadline_ms(SceneGraph::DEFAULT_MAX_SUSPENSION_MS),
            None
        );
        let id = st
            .sessions
            .session_for_namespace("resume-agent")
            .unwrap()
            .session_id
            .clone();
        assert_ne!(id, origin.as_uuid().to_string());
        id
    };
    let cleanup = state
        .lock()
        .await
        .sessions
        .observe_registered_cleanup(&registry_id);
    drop(tx);
    drop(stream);
    await_session_cleanup(cleanup).await;
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        clock.advance(DEFAULT_GRACE_PERIOD_MS);
        scene.expire_leases();
        assert!(scene.scheduled_batches.is_empty());
        assert!(
            !scene
                .widget_registry
                .active_publishes
                .get("gauge")
                .is_some_and(|r| r.iter().any(|r| r.publication_origin == Some(origin)))
        );
    }
    server.abort();
}

/// Invariant 4: when the grace period ends, the runtime's lease sweep
/// reclaims the orphaned lease and its tile, and the resume token is dead.
#[tokio::test]
async fn grpc_grace_expiry_reclaims_publications_and_rejects_resume() {
    let clock = wall_aligned_clock();
    let (mut client, server, state) = setup_test_with_clock(clock.clone()).await;
    {
        let st = state.lock().await;
        configure_lifetime_surfaces(&mut *st.scene.lock().await);
    }
    let (tx, init, mut pending) = handshake(&mut client, "never-claim", "test-key").await;
    let (token, origin) = match &init[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => (
            e.resume_token.clone(),
            bytes_to_scene_id(&e.session_id).unwrap(),
        ),
        other => panic!("expected establishment, got {other:?}"),
    };
    let mut zone = subtitle_publish("held", 0, 0, 0);
    zone.key = origin.to_string();
    assert!(publish_and_ack(&tx, &mut pending, 2, zone).await.ok);
    assert!(
        publish_and_ack(
            &tx,
            &mut pending,
            3,
            lifetime_widget_publish(&origin.to_string(), 0.9)
        )
        .await
        .ok
    );
    let future = state.lock().await.scene.lock().await.now_wall_us() + 60_000_000;
    let mut delayed = subtitle_publish("must-not-reappear", 0, future, 0);
    delayed.key = "pending".into();
    assert!(publish_and_ack(&tx, &mut pending, 4, delayed).await.ok);
    let cleanup = state.lock().await.sessions.observe_cleanup(&origin);
    drop(tx);
    drop(pending);
    await_session_cleanup(cleanup).await;
    let (wrong_tx, mut wrong_stream) = send_resume(&mut client, "wrong-agent", token.clone()).await;
    match next_server_msg(&mut wrong_stream).await.payload {
        Some(ServerPayload::SessionError(e)) => assert_eq!(e.code, "SESSION_GRACE_EXPIRED"),
        other => panic!("expected wrong-owner refusal, got {other:?}"),
    }
    drop(wrong_tx);
    drop(wrong_stream);
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        assert_lifetime_owner(&scene, origin, None);
        // A same-key MCP replacement is current content, so it must survive origin cleanup.
        scene
            .publish_to_zone(
                "subtitle",
                tze_hud_scene::ZoneContent::StreamText("replacement".into()),
                "never-claim",
                Some(origin.to_string()),
                None,
                None,
            )
            .unwrap();
        clock.advance(crate::token::DEFAULT_GRACE_PERIOD_MS);
        // Production applies due batches before lease expiry; grace cancellation still wins.
        assert!(scene.apply_due_batches().is_empty());
        scene.expire_leases();
        assert_eq!(
            scene.zone_registry.active_publishes["subtitle"][0].publication_origin,
            None
        );
        assert!(
            scene
                .widget_registry
                .active_publishes
                .get("gauge")
                .is_none_or(|v| v.iter().all(|r| r.publication_origin != Some(origin)))
        );
        assert_eq!(
            scene.widget_registry.instances["gauge"].current_params["level"],
            tze_hud_scene::WidgetParameterValue::F32(0.0)
        );
        assert!(scene.scheduled_batches.is_empty());
        clock.advance(60_000);
        assert!(scene.apply_due_batches().is_empty());
        assert_eq!(scene.zone_registry.active_publishes["subtitle"].len(), 1);
    }
    let (_tx, mut expired) = send_resume(&mut client, "never-claim", token).await;
    match next_server_msg(&mut expired).await.payload {
        Some(ServerPayload::SessionError(e)) => assert_eq!(e.code, "SESSION_GRACE_EXPIRED"),
        other => panic!("expected expired token, got {other:?}"),
    }
    // Two publication-only origins become due before grace expires. Applying
    // one must not prune the other's deadline while it is still scheduled.
    let present_at = state.lock().await.scene.lock().await.now_wall_us() + 1_000_000;
    let mut scheduled_sessions = Vec::new();
    for agent in ["pending-a", "pending-b"] {
        let (tx, init, mut stream) = handshake(&mut client, agent, "test-key").await;
        let origin = match &init[0].payload {
            Some(ServerPayload::SessionEstablished(e)) => bytes_to_scene_id(&e.session_id).unwrap(),
            other => panic!("expected establishment, got {other:?}"),
        };
        let publish = Publish {
            surface: "zone:status-bar".into(),
            content: Some(crate::proto::ZoneContent {
                payload: Some(crate::proto::zone_content::Payload::StatusBar(
                    crate::proto::StatusBarPayload {
                        entries: HashMap::from([(agent.to_string(), "pending".into())]),
                    },
                )),
            }),
            present_at_us: present_at,
            key: agent.into(),
            ..Default::default()
        };
        assert!(publish_and_ack(&tx, &mut stream, 2, publish).await.ok);
        scheduled_sessions.push((tx, stream, origin));
    }
    let origins: Vec<_> = scheduled_sessions
        .iter()
        .map(|(_, _, origin)| *origin)
        .collect();
    for (tx, stream, origin) in scheduled_sessions {
        let cleanup = state.lock().await.sessions.observe_cleanup(&origin);
        drop(tx);
        drop(stream);
        await_session_cleanup(cleanup).await;
    }
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        assert!(
            scene
                .zone_registry
                .active_publishes
                .get("status-bar")
                .is_none_or(Vec::is_empty)
        );
        assert_eq!(scene.scheduled_batches.len(), 2);
        assert!(
            scene
                .scheduled_batches
                .iter()
                .all(|s| s.batch.lease_id.is_none())
        );
        let grace_deadline = scene.now_wall_us() / 1_000 + crate::token::DEFAULT_GRACE_PERIOD_MS;
        assert_eq!(
            scene.next_lease_deadline_ms(SceneGraph::DEFAULT_MAX_SUSPENSION_MS),
            Some(grace_deadline)
        );
        scene
            .publish_to_zone(
                "status-bar",
                tze_hud_scene::ZoneContent::StatusBar(tze_hud_scene::StatusBarPayload {
                    entries: HashMap::from([("operator".into(), "keep".into())]),
                }),
                "operator",
                Some("operator".into()),
                None,
                None,
            )
            .unwrap();
        clock.advance(1_000);
        let results = scene.apply_due_batches();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.applied), "{results:?}");
        assert!(scene.scheduled_batches.is_empty());
        let records = &scene.zone_registry.active_publishes["status-bar"];
        assert_eq!(records.len(), 3);
        for origin in origins {
            let record = records
                .iter()
                .find(|r| r.publication_origin == Some(origin))
                .unwrap();
            assert_eq!(record.lease_id, None);
            assert_eq!(record.expires_at_wall_us, None);
        }
        assert_eq!(
            scene.next_lease_deadline_ms(SceneGraph::DEFAULT_MAX_SUSPENSION_MS),
            Some(grace_deadline)
        );
        clock.advance(crate::token::DEFAULT_GRACE_PERIOD_MS - 1_000);
        scene.expire_leases();
        let remaining = &scene.zone_registry.active_publishes["status-bar"];
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].publisher_namespace, "operator");
        assert_eq!(remaining[0].publication_origin, None);
        assert_eq!(remaining[0].lease_id, None);
        assert_eq!(remaining[0].expires_at_wall_us, None);
        assert!(scene.apply_due_batches().is_empty());
        assert_eq!(
            scene.next_lease_deadline_ms(SceneGraph::DEFAULT_MAX_SUSPENSION_MS),
            None
        );
    }

    server.abort();
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
    let (mut client, server, state) = setup_test_with_clock(clock).await;
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

    // Publish's omitted/zero timing retains urgency defaults. Hold is a
    // separate request that removes expiry, including before presentation.
    let surface = "zone:notification-area";
    let mut sequence = 3;
    for (urgency, default_ttl_ms, scheduled, actions, explicit_ttl) in [
        (0, 8_000, false, false, 0),
        (1, 8_000, false, false, 0),
        (2, 15_000, true, false, 0),
        (3, 30_000, true, false, 0),
        (0, 60_000, false, true, 0),
        (3, 60_000, true, true, 0),
        (1, 5_000, false, true, 5_000),
        (2, 600_000, true, true, 600_000),
    ] {
        let notification = Publish {
            surface: surface.into(),
            ttl_ms: explicit_ttl,
            content: Some(crate::proto::ZoneContent {
                payload: Some(crate::proto::zone_content::Payload::Notification(
                    crate::proto::NotificationPayload {
                        text: "Keep this notification".into(),
                        urgency,
                        actions: if actions {
                            vec![crate::proto::NotificationActionProto {
                                label: "Yes".into(),
                                callback_id: "yes".into(),

                                url: String::new(),
                            }]
                        } else {
                            Vec::new()
                        },
                        ..Default::default()
                    },
                )),
            }),
            ..Default::default()
        };
        let shown_at = clock.now_us() + if scheduled { 5_000_000 } else { 0 };
        let publish = if scheduled {
            Publish {
                expires_at_us: 0,
                present_at_us: shown_at,
                ..notification.clone()
            }
        } else {
            notification.clone()
        };
        let ack = publish_and_ack(&tx, &mut stream, sequence, publish).await;
        sequence += 1;
        assert!(ack.ok, "{ack:?}");
        {
            let st = state.lock().await;
            let scene = st.scene.lock().await;
            let records = scene
                .zone_registry
                .active_publishes
                .get("notification-area");
            if scheduled {
                assert!(records.is_none_or(Vec::is_empty), "not shown early");
            } else {
                assert_eq!(
                    records.unwrap()[0].expires_at_wall_us,
                    Some(shown_at + default_ttl_ms * 1_000),
                    "omitted timing uses the notification's urgency default"
                );
            }
        }
        let held = request(
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
        assert!(held.ok, "{held:?}");
        {
            let st = state.lock().await;
            let mut scene = st.scene.lock().await;
            if scheduled {
                clock.advance(4_999);
                assert!(scene.apply_due_batches().is_empty(), "still not due");
                clock.advance(1);
                let applied = scene.apply_due_batches();
                assert_eq!(applied.len(), 1);
                assert!(applied[0].applied);
            }
            let records = &scene.zone_registry.active_publishes["notification-area"];
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].expires_at_wall_us, None);
            clock.advance(default_ttl_ms + 1);
            assert_eq!(scene.drain_expired_zone_publications(), 0);
            assert_eq!(
                scene.zone_registry.active_publishes["notification-area"].len(),
                1
            );
        }
        let cleared = request(
            &tx,
            &mut stream,
            sequence,
            ClientPayload::Clear(Clear {
                key: String::new(),
                surface: surface.into(),
            }),
        )
        .await;
        sequence += 1;
        assert!(cleared.ok, "{cleared:?}");
        {
            let st = state.lock().await;
            let scene = st.scene.lock().await;
            assert!(
                scene
                    .zone_registry
                    .active_publishes
                    .get("notification-area")
                    .is_none_or(Vec::is_empty)
            );
        }

        if scheduled {
            // Clear also cancels a held publication that has not presented.
            let ack = publish_and_ack(
                &tx,
                &mut stream,
                sequence,
                Publish {
                    present_at_us: clock.now_us() + 5_000_000,
                    ..notification
                },
            )
            .await;
            sequence += 1;
            assert!(ack.ok, "{ack:?}");
            let held = request(
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
            assert!(held.ok, "{held:?}");
            let cleared = request(
                &tx,
                &mut stream,
                sequence,
                ClientPayload::Clear(Clear {
                    key: String::new(),
                    surface: surface.into(),
                }),
            )
            .await;
            sequence += 1;
            assert!(cleared.ok, "{cleared:?}");
            let st = state.lock().await;
            let mut scene = st.scene.lock().await;
            assert!(scene.scheduled_batches.is_empty());
            clock.advance(5_001);
            assert!(scene.apply_due_batches().is_empty());
            assert!(
                scene
                    .zone_registry
                    .active_publishes
                    .get("notification-area")
                    .is_none_or(Vec::is_empty)
            );
        }
    }
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
    let expires_at = clock.now_us() + 4_000_000;
    let ack = publish_and_ack(
        &tx,
        &mut stream,
        3,
        Publish {
            surface: "zone:notification-area".into(),
            ttl_ms: 600_000,
            expires_at_us: expires_at,
            content: Some(crate::proto::ZoneContent {
                payload: Some(crate::proto::zone_content::Payload::Notification(
                    crate::proto::NotificationPayload {
                        text: "Choose".into(),
                        actions: vec![crate::proto::NotificationActionProto {
                            label: "Yes".into(),
                            callback_id: "yes".into(),

                            url: String::new(),
                        }],
                        ..Default::default()
                    },
                )),
            }),
            ..Default::default()
        },
    )
    .await;
    assert!(ack.ok, "{ack:?}");
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        assert_eq!(
            scene.zone_registry.active_publishes["notification-area"][0].expires_at_wall_us,
            Some(expires_at)
        );
        clock.advance(4_001);
        assert_eq!(scene.drain_expired_zone_publications(), 1);
    }
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
    let present_at = clock.now_us() + 5_000_000;
    let ack = publish_and_ack(
        &tx,
        &mut stream,
        3,
        Publish {
            surface: "zone:notification-area".into(),
            present_at_us: present_at,
            content: Some(crate::proto::ZoneContent {
                payload: Some(crate::proto::zone_content::Payload::Notification(
                    crate::proto::NotificationPayload {
                        text: "Choose".into(),
                        actions: vec![crate::proto::NotificationActionProto {
                            label: "Yes".into(),
                            callback_id: "yes".into(),

                            url: String::new(),
                        }],
                        ..Default::default()
                    },
                )),
            }),
            ..Default::default()
        },
    )
    .await;
    assert!(ack.ok, "{ack:?}");
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let admitted_expiry = match &scene.scheduled_batches[0].batch.mutations[0] {
            tze_hud_scene::SceneMutation::PublishToZone {
                expires_at_wall_us, ..
            } => *expires_at_wall_us,
            other => panic!("expected scheduled zone publish, got {other:?}"),
        };
        assert_eq!(admitted_expiry, Some(present_at + 60_000_000));
        clock.advance(6_000);
        assert!(
            scene
                .apply_due_batches()
                .iter()
                .all(|result| result.applied)
        );
        assert_eq!(
            scene.zone_registry.active_publishes["notification-area"][0].expires_at_wall_us,
            admitted_expiry
        );
    }
    server.abort();
}
