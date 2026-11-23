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
            audience: Default::default(),
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
    session_id: SceneId,
}

impl PublicationSession {
    async fn disconnect(self, state: &Arc<Mutex<SharedState>>) -> SceneId {
        let cleanup = state
            .lock()
            .await
            .sessions
            .observe_cleanup(&self.session_id);
        drop(self.tx);
        drop(self.stream);
        await_session_cleanup(cleanup).await;
        self.session_id
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
        session_id,
    }
}

/// Every resident termination releases its own retained work, while another
/// stream, MCP replacements and independent local lease owners keep theirs.
#[tokio::test]
async fn grpc_termination_clears_owned_publications_without_touching_peer() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    for ending in ["transport", "close", "timeout", "unpair"] {
        let wakes = Arc::new(AtomicU64::new(0));
        let wake_counter = Arc::clone(&wakes);
        let notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            wake_counter.fetch_add(1, Ordering::AcqRel);
        });
        let (mut client, server, state) = setup_test_with_state_and_render_wake(notifier).await;
        let mut owner = connect_publications(&mut client, "lifetime-owner", &state).await;
        let peer_agent = if ending == "unpair" {
            "lifetime-peer"
        } else {
            "lifetime-owner"
        };
        let mut peer = connect_publications(&mut client, peer_agent, &state).await;
        let origin = owner.session_id;
        let future = state.lock().await.scene.lock().await.now_wall_us() + 60_000_000;
        // A pending notification has its own waiter, even before it materializes.
        owner
            .tx
            .send(ClientMessage {
                sequence: 6,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ClientPayload::Publish(Publish {
                    surface: "zone:notification-area".into(),
                    key: "pending-question".into(),
                    content: Some(crate::proto::ZoneContent {
                        payload: Some(crate::proto::zone_content::Payload::Notification(
                            crate::proto::NotificationPayload {
                                text: "pending".into(),
                                ..Default::default()
                            },
                        )),
                    }),
                    present_at_us: future,
                    ..Default::default()
                })),
            })
            .await
            .unwrap();
        // Notifications are ephemeral; a heartbeat orders observation after admission.
        owner
            .tx
            .send(ClientMessage {
                sequence: 7,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ClientPayload::Heartbeat(Heartbeat {
                    timestamp_mono_us: 7,
                })),
            })
            .await
            .unwrap();
        assert!(matches!(
            next_server_msg(&mut owner.stream).await.payload,
            Some(ServerPayload::Heartbeat(_))
        ));
        let ready = Arc::new(AtomicU64::new(0));
        let ready_counter = Arc::clone(&ready);
        let (notification, local_lease) = {
            let st = state.lock().await;
            let mut scene = st.scene.lock().await;
            let notification = scene
                .scheduled_publications
                .iter()
                .find(|s| {
                    s.publisher.origin == Some(origin) && s.notification_publication.is_some()
                })
                .and_then(|s| s.notification_publication)
                .unwrap();
            scene.arm_notification_wait(
                notification,
                "lifetime-owner",
                Arc::new(AtomicBool::new(true)),
                u64::MAX,
                tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
                    ready_counter.fetch_add(1, Ordering::AcqRel);
                }),
            );
            scene
                .publish_to_zone(
                    "subtitle",
                    ZoneContent::StreamText("MCP replacement".into()),
                    "lifetime-owner",
                    Some(origin.to_string()),
                    None,
                    None,
                )
                .unwrap();
            let lease = scene.grant_lease("local-owner", 60_000);
            scene
                .publish_to_zone_for_lease(
                    "subtitle",
                    ZoneContent::StreamText("local lease".into()),
                    "local-owner",
                    Some("local".into()),
                    None,
                    None,
                    Vec::new(),
                    Some(lease),
                )
                .unwrap();
            let now = scene.now_millis();
            scene.disconnect_lease(&lease, now).unwrap();
            (notification, lease)
        };
        let store = state.lock().await.resource_store.clone();
        owner
            .tx
            .send(ClientMessage {
                sequence: 8,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
                    expected_hash: blake3::hash(&[1, 2, 3, 255]).as_bytes().to_vec(),
                    resource_type: 1,
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
        assert!(matches!(
            next_server_msg(&mut owner.stream).await.payload,
            Some(ServerPayload::ResourceUploadAccepted(_))
        ));
        assert_eq!(store.in_flight_count("lifetime-owner").await, 1);
        let cleanup = state.lock().await.sessions.observe_cleanup(&origin);
        let before = wakes.load(Ordering::Acquire);
        let mut peer_sequence = 6;
        match ending {
            "close" => owner
                .tx
                .send(ClientMessage {
                    sequence: 9,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ClientPayload::SessionClose(SessionClose::default())),
                })
                .await
                .unwrap(),
            "unpair" => {
                let closers = state
                    .lock()
                    .await
                    .sessions
                    .close_namespace("lifetime-owner");
                assert_eq!(closers.len(), 1);
                for closer in closers {
                    tokio::time::timeout(Duration::from_secs(5), closer.ended())
                        .await
                        .unwrap();
                }
            }
            "timeout" => {
                // The peer remains responsive while only the owner misses its receive window.
                for _ in 0..3 {
                    tokio::time::pause();
                    tokio::time::advance(Duration::from_millis(DEFAULT_HEARTBEAT_INTERVAL_MS))
                        .await;
                    tokio::time::resume();
                    peer.tx
                        .send(ClientMessage {
                            sequence: peer_sequence,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(ClientPayload::Heartbeat(Heartbeat {
                                timestamp_mono_us: peer_sequence,
                            })),
                        })
                        .await
                        .unwrap();
                    assert!(matches!(
                        next_server_msg(&mut peer.stream).await.payload,
                        Some(ServerPayload::Heartbeat(_))
                    ));
                    peer_sequence += 1;
                }
            }
            "transport" => {}
            _ => unreachable!(),
        }
        if ending == "transport" {
            drop(owner.tx);
            drop(owner.stream);
            await_session_cleanup(cleanup).await;
        } else {
            // The trigger itself must clean up while the transport is retained;
            // dropping it first would let EOF conceal a broken close/timeout.
            await_session_cleanup(cleanup).await;
            drop(owner.tx);
            drop(owner.stream);
        }
        assert_eq!(store.in_flight_count("lifetime-owner").await, 0);
        assert!(
            wakes.load(Ordering::Acquire) > before,
            "{ending}: cleanup must wake presentation"
        );
        assert_eq!(
            ready.load(Ordering::Acquire),
            1,
            "{ending}: waiter receives one readiness wake"
        );
        {
            let st = state.lock().await;
            let scene = st.scene.lock().await;
            assert_eq!(st.sessions.session_count(), 1);
            assert_lifetime_owner(&scene, peer.session_id, None);
            assert!(
                scene
                    .zone_registry
                    .active_publishes
                    .values()
                    .flatten()
                    .all(|r| r.publication_origin != Some(origin))
            );
            assert!(
                scene
                    .widget_registry
                    .active_publishes
                    .values()
                    .flatten()
                    .all(|r| r.publication_origin != Some(origin))
            );
            assert!(
                scene
                    .scheduled_publications
                    .iter()
                    .all(|s| s.publisher.origin != Some(origin))
            );
            assert_eq!(
                scene.scheduled_publications.len(),
                1,
                "only the peer retains scheduled work"
            );
            assert!(subtitle_texts(&scene).contains(&"MCP replacement".to_string()));
            assert_eq!(
                scene.leases[&local_lease].state,
                tze_hud_scene::LeaseState::Orphaned
            );
            assert!(subtitle_texts(&scene).contains(&"local lease".to_string()));
            assert_eq!(
                scene.notification_wait_outcome(notification),
                Some(tze_hud_scene::graph::NotificationOutcome::Gone(
                    tze_hud_scene::graph::NotificationGone::Cleared
                ))
            );
        }
        // Reconnect is a new authenticated origin and starts without old content.
        let (fresh_tx, fresh_init, fresh_stream) =
            handshake(&mut client, "lifetime-owner", "test-key").await;
        let Some(ServerPayload::SessionEstablished(fresh)) = &fresh_init[0].payload else {
            panic!("expected fresh init");
        };
        let fresh_id = bytes_to_scene_id(&fresh.session_id).unwrap();
        assert_ne!(fresh_id, origin);
        let fresh_cleanup = state.lock().await.sessions.observe_cleanup(&fresh_id);
        drop(fresh_tx);
        drop(fresh_stream);
        await_session_cleanup(fresh_cleanup).await;
        peer.tx
            .send(ClientMessage {
                sequence: peer_sequence,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ClientPayload::Heartbeat(Heartbeat {
                    timestamp_mono_us: peer_sequence,
                })),
            })
            .await
            .unwrap();
        assert!(matches!(
            next_server_msg(&mut peer.stream).await.payload,
            Some(ServerPayload::Heartbeat(_))
        ));
        peer.disconnect(&state).await;
        server.abort();
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
                                copy: None,
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
                assert!(scene.apply_due_publications().is_empty(), "still not due");
                clock.advance(1);
                let applied = scene.apply_due_publications();
                assert_eq!(applied.len(), 1);
                assert!(applied[0].result.is_ok());
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
            assert!(scene.scheduled_publications.is_empty());
            clock.advance(5_001);
            assert!(scene.apply_due_publications().is_empty());
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
                            copy: None,
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
    assert!(scene.apply_due_publications().is_empty());
    assert!(subtitle_texts(&scene).is_empty(), "still not due");
    clock.advance(1);
    let applied = scene.apply_due_publications();
    assert_eq!(applied.len(), 1);
    assert!(applied[0].result.is_ok());
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
                            copy: None,
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
        let admitted_expiry = match &scene.scheduled_publications[0].publication {
            tze_hud_scene::Publication::Zone {
                expires_at_wall_us, ..
            } => *expires_at_wall_us,
            other => panic!("expected scheduled zone publish, got {other:?}"),
        };
        assert_eq!(admitted_expiry, Some(present_at + 60_000_000));
        clock.advance(6_000);
        assert!(
            scene
                .apply_due_publications()
                .iter()
                .all(|result| result.result.is_ok())
        );
        assert_eq!(
            scene.zone_registry.active_publishes["notification-area"][0].expires_at_wall_us,
            admitted_expiry
        );
    }
    server.abort();
}
