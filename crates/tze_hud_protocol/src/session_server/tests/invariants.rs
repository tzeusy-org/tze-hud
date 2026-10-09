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

struct HeldTileSession {
    tx: tokio::sync::mpsc::Sender<ClientMessage>,
    stream: tonic::Streaming<ServerMessage>,
    resume_token: Vec<u8>,
    session_id: SceneId,
    lease_id: SceneId,
    tile_id: SceneId,
}

impl HeldTileSession {
    async fn disconnect(
        self,
        state: &Arc<tokio::sync::Mutex<crate::session::SharedState>>,
    ) -> (Vec<u8>, SceneId, SceneId) {
        let cleanup = state
            .lock()
            .await
            .sessions
            .observe_cleanup(&self.session_id);
        drop(self.tx);
        drop(self.stream);
        // Neither the state nor scene lock is held while the real handler runs.
        await_session_cleanup(cleanup).await;
        let st = state.lock().await;
        let scene = st.scene.lock().await;
        assert_eq!(scene.leases[&self.lease_id].state, LeaseState::Orphaned);
        (self.resume_token, self.lease_id, self.tile_id)
    }
}

/// Connect and take a lease that owns one tile through the real session stream.
async fn connect_hold_tile(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent: &str,
    state: &Arc<tokio::sync::Mutex<crate::session::SharedState>>,
) -> HeldTileSession {
    {
        let st = state.lock().await;
        configure_lifetime_surfaces(&mut *st.scene.lock().await);
    }
    let (tx, init, mut stream) = handshake(client, agent, "test-key").await;
    let (resume_token, session_id) = match &init[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => (
            e.resume_token.clone(),
            bytes_to_scene_id(&e.session_id)
                .expect("SessionEstablished carries a valid session ID"),
        ),
        other => panic!("expected SessionEstablished, got {other:?}"),
    };
    // Failed publication leaves no provenance; both real durable surfaces then
    // publish before the first claim, including a future zone publication.
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
    let mut zone = subtitle_publish("preclaim", 0, 0, 0);
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
    let prior_tiles = state.lock().await.scene.lock().await.tile_count();
    for (sequence, root) in [
        (6, crate::proto::NodeProto::default()), // malformed: no data
        (
            7,
            crate::proto::NodeProto {
                data: Some(crate::proto::node_proto::Data::StaticImage(
                    crate::proto::StaticImageNodeProto {
                        resource_id: vec![0; 32],
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
        ), // valid conversion, unregistered resource rejects root after tile creation
    ] {
        tx.send(ClientMessage {
            sequence,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::ClaimTile(ClaimTile {
                root: Some(root),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        match next_server_msg(&mut stream).await.payload {
            Some(ServerPayload::RequestResult(r)) => assert!(!r.ok),
            other => panic!("expected failed claim, got {other:?}"),
        }
        let st = state.lock().await;
        let scene = st.scene.lock().await;
        assert_lifetime_owner(&scene, session_id, None);
        assert_eq!(
            scene.tile_count(),
            prior_tiles,
            "failed root rolls back only its tile"
        );
    }
    tx.send(ClientMessage {
        sequence: 8,
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
    {
        let st = state.lock().await;
        let scene = st.scene.lock().await;
        assert_lifetime_owner(&scene, session_id, Some(lease_id));
        assert_eq!(
            scene.scheduled_batches.last().unwrap().batch.lease_id,
            Some(lease_id)
        );
    }
    HeldTileSession {
        tx,
        stream,
        resume_token,
        session_id,
        lease_id,
        tile_id,
    }
}

/// Register completion before transport drop, then await all real session cleanup.
async fn connect_hold_tile_and_disconnect(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    state: &Arc<tokio::sync::Mutex<crate::session::SharedState>>,
    agent: &str,
) -> (Vec<u8>, SceneId, SceneId) {
    connect_hold_tile(client, agent, state)
        .await
        .disconnect(state)
        .await
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
    let clock = wall_aligned_clock();
    let (mut client, server, state, _exp) = setup_test_with_lease_expiry_clock(clock.clone()).await;
    let mut session = connect_hold_tile(&mut client, "orphan-agent", &state).await;
    let peer = connect_hold_tile(&mut client, "orphan-agent", &state).await;
    // A later successful claim must not reattach content already owned by the first lease.
    session
        .tx
        .send(ClientMessage {
            sequence: 9,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::ClaimTile(ClaimTile {
                ttl_ms: 600_000,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
    let later_lease = match next_server_msg(&mut session.stream).await.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert!(r.ok);
            bytes_to_scene_id(&r.lease_id).unwrap()
        }
        other => panic!("expected later claim, got {other:?}"),
    };
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        assert_ne!(later_lease, session.lease_id);
        assert_lifetime_owner(&scene, session.session_id, Some(session.lease_id));
        scene
            .publish_to_zone_for_lease(
                "subtitle",
                tze_hud_scene::ZoneContent::StreamText("later lease".into()),
                "orphan-agent",
                Some("later-lease".into()),
                None,
                None,
                Vec::new(),
                Some(later_lease),
            )
            .unwrap();
    }
    assert_ne!(session.session_id, peer.session_id);
    let (duplicate_cleanup, mut peer_cleanup) = {
        let mut st = state.lock().await;
        (
            st.sessions.observe_cleanup(&session.session_id),
            st.sessions.observe_cleanup(&peer.session_id),
        )
    };
    let session_origin = session.session_id;
    let (_token, lease_id, tile_id) = session.disconnect(&state).await;
    // A second observer starts awaiting after cleanup already completed.
    await_session_cleanup(duplicate_cleanup).await;

    let st = state.lock().await;
    let scene = st.scene.lock().await;
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);
    let tile = scene.tiles.get(&tile_id).expect("orphaned content is kept");
    assert_eq!(
        tile.visual_hint,
        tze_hud_scene::lease::TileVisualHint::DisconnectionBadge
    );
    assert_eq!(
        st.sessions.session_count(),
        1,
        "only the peer remains connected"
    );
    assert_eq!(scene.leases[&peer.lease_id].state, LeaseState::Active);
    assert_eq!(
        scene.tiles[&peer.tile_id].visual_hint,
        tze_hud_scene::lease::TileVisualHint::None
    );
    assert_eq!(
        peer_cleanup.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    );
    drop(scene);
    drop(st);
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        // Independently leased MCP content in the same namespace is not session-origin owned.
        let independent = scene.grant_lease("orphan-agent", 600_000);
        scene
            .publish_to_zone_for_lease(
                "subtitle",
                tze_hud_scene::ZoneContent::StreamText("MCP".into()),
                "orphan-agent",
                Some("mcp".into()),
                None,
                None,
                Vec::new(),
                Some(independent),
            )
            .unwrap();
        clock.advance(crate::token::DEFAULT_GRACE_PERIOD_MS);
        scene.expire_leases();
        assert!(
            !scene.zone_registry.active_publishes["subtitle"]
                .iter()
                .any(|r| r.lease_id == Some(lease_id))
        );
        assert!(
            !scene.widget_registry.active_publishes["gauge"]
                .iter()
                .any(|r| r.lease_id == Some(lease_id))
        );
        assert!(
            scene.zone_registry.active_publishes["subtitle"]
                .iter()
                .any(|r| r.lease_id == Some(independent))
        );
        assert_lifetime_owner(&scene, peer.session_id, Some(peer.lease_id));
        assert!(
            !scene.zone_registry.active_publishes["subtitle"]
                .iter()
                .any(|r| r.lease_id == Some(later_lease))
        );
        // Reaping an adopted lease cancels its delayed visibility through the existing Active guard.
        clock.advance(60_000);
        assert!(scene.apply_due_batches().iter().any(|r| !r.applied));
        assert!(
            !scene.zone_registry.active_publishes["subtitle"]
                .iter()
                .any(|r| r.merge_key.as_deref() == Some(&format!("delayed-{session_origin}")))
        );
    }
    peer.disconnect(&state).await;
    await_session_cleanup(peer_cleanup).await;
    assert_eq!(state.lock().await.sessions.session_count(), 0);
    server.abort();
}

/// Invariant 4: resuming within the grace period restores the same lease and
/// tile, clears the badge, and creates nothing new.
#[tokio::test]
async fn grpc_resume_within_grace_restores_same_lease_and_tile() {
    let clock = wall_aligned_clock();
    let (mut client, server, state, _exp) = setup_test_with_lease_expiry_clock(clock.clone()).await;
    let (token, lease_id, tile_id) =
        connect_hold_tile_and_disconnect(&mut client, &state, "resume-agent").await;

    clock.advance(crate::token::DEFAULT_GRACE_PERIOD_MS - 1);
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
    // A second stream never claims a tile before disconnect. Its current
    // zone/widget and pending publish survive authenticated resume under the old origin.
    let (tx, init, mut published) = handshake(&mut client, "publication-only", "test-key").await;
    let (token, origin) = match &init[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => (
            e.resume_token.clone(),
            bytes_to_scene_id(&e.session_id).unwrap(),
        ),
        other => panic!("expected establishment, got {other:?}"),
    };
    let mut zone = subtitle_publish("unleased", 0, 0, 0);
    zone.key = origin.to_string();
    assert!(publish_and_ack(&tx, &mut published, 2, zone).await.ok);
    assert!(
        publish_and_ack(
            &tx,
            &mut published,
            3,
            lifetime_widget_publish(&origin.to_string(), 0.5)
        )
        .await
        .ok
    );
    let future = state.lock().await.scene.lock().await.now_wall_us() + 60_000_000;
    let mut delayed = subtitle_publish("resumed-delay", 0, future, 0);
    delayed.key = "resume-delay".into();
    assert!(publish_and_ack(&tx, &mut published, 4, delayed).await.ok);
    let cleanup = state.lock().await.sessions.observe_cleanup(&origin);
    drop(tx);
    drop(published);
    await_session_cleanup(cleanup).await;
    clock.advance(crate::token::DEFAULT_GRACE_PERIOD_MS - 1);
    let (tx, mut resumed) = send_resume(&mut client, "publication-only", token).await;
    match next_server_msg(&mut resumed).await.payload {
        Some(ServerPayload::SessionResumeResult(r)) => {
            assert!(r.accepted);
        }
        other => panic!("expected resume, got {other:?}"),
    };
    // Consume the same ordered snapshot and degradation messages as a new handshake.
    next_server_msg(&mut resumed).await;
    next_server_msg(&mut resumed).await;
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        clock.advance(2);
        scene.expire_leases();
        assert_lifetime_owner(&scene, origin, None);
    }
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
    let adopted = match next_server_msg(&mut resumed).await.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert!(r.ok);
            bytes_to_scene_id(&r.lease_id).unwrap()
        }
        other => panic!("expected claim, got {other:?}"),
    };
    let resumed_id = {
        let st = state.lock().await;
        let scene = st.scene.lock().await;
        assert_lifetime_owner(&scene, origin, Some(adopted));
        assert!(
            scene
                .scheduled_batches
                .iter()
                .any(|s| s.batch.lease_id == Some(adopted))
        );
        assert!(
            scene.zone_registry.active_publishes["subtitle"]
                .iter()
                .any(|r| r.lease_id == Some(lease_id))
        );
        let physical = scene.leases[&adopted].session_id;
        assert_ne!(physical, origin, "physical resumed id stays fresh");
        physical
    };
    let cleanup = state.lock().await.sessions.observe_cleanup(&resumed_id);
    drop(tx);
    drop(resumed);
    await_session_cleanup(cleanup).await;
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
        assert_eq!(st.sessions.session_count(), 0);
        assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);
        clock.advance(crate::token::DEFAULT_GRACE_PERIOD_MS + 1);
        // The compositor's per-frame sweep, as in production.
        let expiries = scene.expire_leases();
        assert_eq!(expiries.len(), 1);
        assert_eq!(expiries[0].lease_id, lease_id);
        assert_eq!(expiries[0].removed_tiles, vec![tile_id]);
        assert_eq!(scene.tile_count(), 0);
        clock.advance(60_000);
        assert!(scene.apply_due_batches().iter().all(|r| !r.applied));
    }

    let (_tx, mut stream) = send_resume(&mut client, "late-agent", token).await;
    match stream.next().await.unwrap().unwrap().payload {
        Some(ServerPayload::SessionError(e)) => assert_eq!(e.code, "SESSION_GRACE_EXPIRED"),
        other => panic!("expected SESSION_GRACE_EXPIRED, got {other:?}"),
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
