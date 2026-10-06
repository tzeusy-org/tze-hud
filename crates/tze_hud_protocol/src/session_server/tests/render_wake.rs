use super::*;

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
        server_sequence: 0,
        resume_token: Vec::new(),
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
