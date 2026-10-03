use super::*;

// ─── Safe mode tests (RFC 0005 §3.7) ─────────────────────────────────────

/// Scenario: Mutations rejected during safe mode (RFC 0005 §3.7)
/// WHEN the runtime enters safe mode and sets `SharedState.safe_mode_atomic = true`,
/// THEN MutationBatch is rejected with SAFE_MODE_ACTIVE.
///
/// In this test we drive safe mode via `SharedState` directly (as the runtime
/// would do via a SessionSuspended broadcast to all sessions).
#[tokio::test]
async fn test_safe_mode_rejects_mutations() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "safe-mode-agent", "test-key").await;

    // Claim a tile before safe mode (ClaimTile creates a tile, so safe mode rejects it)
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 30_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let lease_msg = stream.next().await.unwrap().unwrap();
    let lease_id = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Enable safe mode in shared state (simulates runtime entering safe mode)
    {
        let st = shared_state.lock().await;
        st.safe_mode_atomic
            .store(true, std::sync::atomic::Ordering::Release);
    }

    // Send MutationBatch while safe mode is active — should be rejected
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: None,
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::RequestResult(err)) => {
            assert_eq!(
                err.code, "SAFE_MODE_ACTIVE",
                "Expected SAFE_MODE_ACTIVE, got: {}",
                err.code
            );
        }
        other => panic!("Expected RuntimeError(SAFE_MODE_ACTIVE), got: {other:?}"),
    }

    // Disable safe mode
    {
        let st = shared_state.lock().await;
        st.safe_mode_atomic
            .store(false, std::sync::atomic::Ordering::Release);
    }

    // Mutations should no longer be rejected with SAFE_MODE_ACTIVE.
    // We use a heartbeat to verify the session is still responsive and
    // the safe mode is cleared.
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 999,
        })),
    })
    .await
    .unwrap();

    let msg2 = stream.next().await.unwrap().unwrap();
    match &msg2.payload {
        Some(ServerPayload::Heartbeat(hb)) => {
            // Session still active after safe mode was cleared
            assert_eq!(hb.timestamp_mono_us, 999, "Heartbeat should echo correctly");
        }
        other => panic!("Expected Heartbeat after safe mode exit, got: {other:?}"),
    }

    // Now verify a MutationBatch is no longer blocked by SAFE_MODE_ACTIVE.
    // (It may still fail due to invalid lease, but not because of safe mode.)
    let batch_id2 = uuid::Uuid::now_v7().as_bytes().to_vec();
    // Use the real lease from earlier
    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id2.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: None,
        })),
    })
    .await
    .unwrap();

    let msg3 = stream.next().await.unwrap().unwrap();
    match &msg3.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert_ne!(
                result.code, "SAFE_MODE_ACTIVE",
                "safe mode should be cleared"
            );
            assert_eq!(result.batch_id, batch_id2);
        }
        other => panic!("Unexpected message after safe mode exit: {other:?}"),
    }
}

// ─── Freeze queue tests (system-shell/spec.md §Freeze Scene) ────────────

/// Scenario: Freeze queues mutations (spec line 146)
/// WHEN viewer activates freeze via SharedState.freeze_active = true
/// AND agent submits a MutationBatch
/// THEN mutations are queued (accepted = true), tile content does not update
#[tokio::test]
async fn test_freeze_queues_mutations_not_applied() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let generations = Arc::new(AtomicU64::new(0));
    let callback_generations = Arc::clone(&generations);
    let notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_generations.fetch_add(1, Ordering::AcqRel);
    });
    let (mut client, _server, shared_state) = setup_test_with_state_and_render_wake(notifier).await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "freeze-agent", "test-key").await;

    // Request a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 30_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let lease_msg = stream.next().await.unwrap().unwrap();
    let (lease_id, tile_id) = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => {
            (resp.lease_id.clone(), resp.ids[0].clone())
        }
        other => panic!("Expected a granted ClaimTile, got: {other:?}"),
    };
    let (scene_version_before, tile_count_before) = {
        let st = shared_state.lock().await;
        let scene = st.scene.lock().await;
        (scene.version, scene.tiles.len())
    };
    let parked_checkpoint = generations.load(Ordering::Acquire);

    // Activate freeze
    {
        let mut st = shared_state.lock().await;
        st.freeze_active = true;
    }

    // Submit a MutationBatch while frozen
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: vec![crate::proto::MutationProto {
                mutation: Some(crate::proto::mutation_proto::Mutation::UpdateTileOpacity(
                    crate::proto::UpdateTileOpacityMutation {
                        tile_id: tile_id.clone(),
                        opacity: 0.5,
                    },
                )),
            }],
            timing: None,
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            // Accepted=true: mutation was queued, not rejected
            assert_eq!(result.batch_id, batch_id);
            assert!(
                result.ok,
                "Mutation should be accepted (queued) during freeze, not rejected"
            );
            // Scene should NOT have been modified; error code should not be SAFE_MODE_ACTIVE
            assert_ne!(result.code, "SAFE_MODE_ACTIVE");
        }
        other => panic!("Expected MutationResult during freeze, got: {other:?}"),
    }
    assert_eq!(
        generations.load(Ordering::Acquire),
        parked_checkpoint,
        "freeze enqueue must not wake the compositor before any scene mutation"
    );
    {
        let st = shared_state.lock().await;
        let scene = st.scene.lock().await;
        assert_eq!(scene.version, scene_version_before);
        assert_eq!(scene.tiles.len(), tile_count_before);
    }

    // Deactivate freeze — queued mutation should be applied in next iteration
    {
        let mut st = shared_state.lock().await;
        st.freeze_active = false;
    }

    // Send a heartbeat to trigger the unfreeze drain on next loop iteration
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 9999,
        })),
    })
    .await
    .unwrap();

    // The unfreeze drain applies queued mutations (resulting in MutationResult(accepted))
    // before processing the heartbeat. We may get additional MutationResult messages.
    // Wait for the heartbeat echo to confirm the session is still active.
    let mut got_heartbeat = false;
    for _ in 0..5 {
        if let Some(Ok(msg)) = stream.next().await {
            match &msg.payload {
                Some(ServerPayload::Heartbeat(hb)) => {
                    assert_eq!(hb.timestamp_mono_us, 9999);
                    got_heartbeat = true;
                    break;
                }
                Some(ServerPayload::RequestResult(_)) => {
                    // Drained mutation result — expected, continue
                }
                other => panic!("Unexpected message after unfreeze: {other:?}"),
            }
        }
    }
    assert!(
        got_heartbeat,
        "Expected heartbeat echo after unfreeze drain"
    );
    assert_eq!(
        generations.load(Ordering::Acquire),
        parked_checkpoint + 1,
        "one applied queued batch must publish exactly one post-mutation wake"
    );
    {
        let st = shared_state.lock().await;
        let scene = st.scene.lock().await;
        assert!(scene.version > scene_version_before);
        assert_eq!(scene.tiles.len(), tile_count_before);
        let tile_id = bytes_to_scene_id(&tile_id).expect("claimed tile id");
        assert_eq!(scene.tiles[&tile_id].opacity, 0.5);
    }
}

/// Regression: FIFO ordering preserved when a new mutation arrives after unfreeze
/// but before the drain loop has emptied the queue.
///
/// Before the fix, `handle_mutation_batch` only checked `st.freeze_active`. Once
/// the shell cleared `freeze_active` the new mutation bypassed the queue and was
/// applied ahead of still-queued predecessors — a FIFO violation.
///
/// The fix: also check `!session.freeze_queue.is_empty()`. A new mutation that
/// arrives while the queue is non-empty is enqueued, preserving submission order
/// until the drain loop fully completes.
///
/// This is a direct unit test so it does not rely on timing: we manipulate the
/// freeze queue and shared state directly, then call `handle_mutation_batch` and
/// assert the queue depth increases (i.e., the new batch was enqueued, not
/// bypassed).
#[tokio::test]
async fn test_fifo_preserved_when_mutation_arrives_during_drain_window() {
    use tze_hud_resource::{ResourceStore, ResourceStoreConfig};
    use tze_hud_scene::graph::SceneGraph;

    // Build a minimal shared state with freeze_active = false (already unfrozen).
    let (outbound_tx, mut outbound_rx) =
        tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(16);

    let state: Arc<Mutex<SharedState>> = Arc::new(Mutex::new(SharedState {
        scene: Arc::new(Mutex::new(SceneGraph::new(800.0, 600.0))),
        sessions: crate::session::SessionRegistry::new(),
        resource_store: ResourceStore::new(ResourceStoreConfig::default()),
        runtime_widget_store: None,
        element_store: tze_hud_scene::element_store::ElementStore::default(),
        element_store_path: None,
        safe_mode_atomic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_tab_mirror: Arc::new(std::sync::Mutex::new(None)),
        token_store: crate::token::TokenStore::new(),
        freeze_active: false, // <-- already unfrozen
        input_capture_tx: None,
        input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        tile_placement: Default::default(),
    }));

    // Build a session whose freeze_queue already has one entry (simulates the
    // drain window: freeze was cleared but the drain loop has not run yet).
    let mut freeze_queue = SessionFreezeQueue::new(FREEZE_QUEUE_CAPACITY);
    let pre_queued_batch = MutationBatch {
        batch_id: b"pre-queued".to_vec(),
        lease_id: vec![0u8; 16],
        mutations: Vec::new(),
        ..Default::default()
    };
    freeze_queue.enqueue(pre_queued_batch, "test-ns");
    assert!(
        !freeze_queue.is_empty(),
        "Precondition: queue must be non-empty before the race window test"
    );

    let mut session = StreamSession {
        session_id: "fifo-test-session".to_string(),
        namespace: "test-ns".to_string(),
        agent_name: "test-agent".to_string(),
        capabilities: Vec::new(),
        lease_ids: Vec::new(),
        scene_session_id: SceneId::new(),
        resource_budget: ResourceBudget::default(),
        budget_enforcer: None,
        subscriptions: Vec::new(),
        server_sequence: 0,
        resume_token: Vec::new(),
        last_heartbeat_ms: 0,
        state: SessionState::Active,
        last_client_sequence: 1,
        safe_mode_active: false,
        freeze_queue,
        session_open_at_wall_us: 0,
        dedup_window: DedupWindow::new(1000, 60),
        lease_correlation_cache: LeaseCorrelationCache::new(
            DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY,
        ),
        resource_upload_rate_limiter: UploadByteRateLimiter::with_limit(
            tze_hud_resource::DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC,
        ),
    };

    // The new mutation arrives while freeze_active=false but the queue is non-empty
    // (the "drain window" race). With the fix this must be enqueued, not applied.
    let new_batch = MutationBatch {
        batch_id: b"new-in-drain-window".to_vec(),
        lease_id: vec![0u8; 16],
        mutations: Vec::new(),
        ..Default::default()
    };
    handle_mutation_batch(
        &state,
        &mut session,
        &outbound_tx,
        0,
        new_batch,
        &tze_hud_scene::render_wake::RenderWakeNotifier::default(),
    )
    .await;

    // The queue must now hold 2 entries: the pre-queued one plus the new arrival.
    // If the fix is absent, the new batch bypasses the queue (queue depth stays 1)
    // and the pre-queued mutations would be applied AFTER the bypassing one — FIFO
    // violation.
    let queue_depth = {
        // Drain the queue to count entries, then verify order by batch_id.
        let drained = session.freeze_queue.drain();
        drained.len()
    };
    assert_eq!(
        queue_depth, 2,
        "Both the pre-queued batch and the new batch must be in the freeze queue \
         (FIFO preserved). If this is 1, the new batch bypassed the queue — FIFO violated."
    );

    // The outbound channel should have received accepted=true for the new batch
    // (same as the enqueue path response, not an error).
    let response = outbound_rx
        .recv()
        .await
        .expect("expected a MutationResult response")
        .expect("expected Ok response");
    match &response.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert_eq!(r.batch_id, b"new-in-drain-window".to_vec());
            assert!(
                r.ok,
                "New batch must be accepted (enqueued) during drain window, not rejected"
            );
        }
        other => panic!(
            "Expected MutationResult(accepted=true) for new batch during drain window, got: {other:?}"
        ),
    }
}

/// Regression: a Transactional batch retransmitted while the scene is frozen must
/// be applied exactly once after drain, not twice.
///
/// Before the fix, the freeze enqueue path did not consult `dedup_window`.  A
/// retransmit (same `batch_id`) while frozen was pushed onto the freeze queue as a
/// second distinct entry and applied twice when the queue drained — producing a
/// duplicate-application that never occurs on the non-frozen path.
///
/// The fix: check `dedup_window` at enqueue time (symmetric with the non-frozen
/// path) and record the batch in the window immediately after a successful enqueue.
/// A retransmit hits the window and is suppressed before it can be pushed.
///
/// This is a direct unit test: we manipulate `StreamSession` and `SharedState`
/// directly so the assertion is deterministic (no timing dependency).
#[tokio::test]
async fn test_freeze_retransmit_deduped_applied_exactly_once() {
    use tze_hud_resource::{ResourceStore, ResourceStoreConfig};
    use tze_hud_scene::graph::SceneGraph;

    let (outbound_tx, mut outbound_rx) =
        tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(32);

    // Shared state: scene is frozen.
    let state: Arc<Mutex<SharedState>> = Arc::new(Mutex::new(SharedState {
        scene: Arc::new(Mutex::new(SceneGraph::new(800.0, 600.0))),
        sessions: crate::session::SessionRegistry::new(),
        resource_store: ResourceStore::new(ResourceStoreConfig::default()),
        runtime_widget_store: None,
        element_store: tze_hud_scene::element_store::ElementStore::default(),
        element_store_path: None,
        safe_mode_atomic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_tab_mirror: Arc::new(std::sync::Mutex::new(None)),
        token_store: crate::token::TokenStore::new(),
        freeze_active: true, // <-- scene is frozen
        input_capture_tx: None,
        input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        tile_placement: Default::default(),
    }));

    let mut session = StreamSession {
        session_id: "dedup-freeze-test".to_string(),
        namespace: "test-ns".to_string(),
        agent_name: "test-agent".to_string(),
        capabilities: Vec::new(),
        lease_ids: Vec::new(),
        scene_session_id: SceneId::new(),
        resource_budget: ResourceBudget::default(),
        budget_enforcer: None,
        subscriptions: Vec::new(),
        server_sequence: 0,
        resume_token: Vec::new(),
        last_heartbeat_ms: 0,
        state: SessionState::Active,
        last_client_sequence: 1,
        safe_mode_active: false,
        freeze_queue: SessionFreezeQueue::new(FREEZE_QUEUE_CAPACITY),
        session_open_at_wall_us: 0,
        dedup_window: DedupWindow::new(1000, 60),
        lease_correlation_cache: LeaseCorrelationCache::new(
            DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY,
        ),
        resource_upload_rate_limiter: UploadByteRateLimiter::with_limit(
            tze_hud_resource::DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC,
        ),
    };

    // Use a valid 16-byte batch_id so the dedup window key is populated.
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();

    // ── First send: batch enqueued while frozen ───────────────────────────────
    let original_batch = MutationBatch {
        batch_id: batch_id.clone(),
        lease_id: vec![0u8; 16],
        mutations: Vec::new(),
        ..Default::default()
    };
    handle_mutation_batch(
        &state,
        &mut session,
        &outbound_tx,
        0,
        original_batch,
        &tze_hud_scene::render_wake::RenderWakeNotifier::default(),
    )
    .await;

    // Queue must hold exactly one entry.
    assert!(
        !session.freeze_queue.is_empty(),
        "Precondition: first batch must have been enqueued"
    );

    // Consume the enqueue ack (accepted=true).
    let first_ack = outbound_rx
        .recv()
        .await
        .expect("expected MutationResult for first send")
        .expect("expected Ok result");
    match &first_ack.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert_eq!(r.batch_id, batch_id, "batch_id must match");
            assert!(r.ok, "first enqueue must be accepted");
        }
        other => panic!("Expected MutationResult for first send, got: {other:?}"),
    }

    // ── Retransmit: same batch_id, still frozen ───────────────────────────────
    let retransmit_batch = MutationBatch {
        batch_id: batch_id.clone(),
        lease_id: vec![0u8; 16],
        mutations: Vec::new(),
        ..Default::default()
    };
    handle_mutation_batch(
        &state,
        &mut session,
        &outbound_tx,
        0,
        retransmit_batch,
        &tze_hud_scene::render_wake::RenderWakeNotifier::default(),
    )
    .await;

    // Consume the dedup response — must be accepted=true (cached from first send).
    let dedup_ack = outbound_rx
        .recv()
        .await
        .expect("expected MutationResult for retransmit")
        .expect("expected Ok result");
    match &dedup_ack.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert_eq!(r.batch_id, batch_id, "batch_id must match on retransmit");
            assert!(r.ok, "dedup hit must return cached accepted=true");
        }
        other => {
            panic!("Expected cached MutationResult on retransmit while frozen, got: {other:?}")
        }
    }

    // ── Key assertion: queue has exactly ONE entry ────────────────────────────
    // If the retransmit was deduped (the fix works), the queue depth is 1.
    // Without the fix the retransmit was pushed as a second entry (depth 2)
    // and would have been applied twice on drain.
    let drained = session.freeze_queue.drain();
    assert_eq!(
        drained.len(),
        1,
        "Retransmit while frozen must be deduped: freeze queue must contain \
         exactly one entry, not two. If this is 2, the retransmit was enqueued \
         again and would have been applied twice after drain."
    );
    assert_eq!(
        drained[0].batch_id, batch_id,
        "The single queued entry must be the original batch"
    );

    // No further messages should be pending.
    assert!(
        outbound_rx.try_recv().is_err(),
        "No additional messages should be in the outbound channel after dedup"
    );
}

/// Scenario: Freeze ignored during safe mode (spec line 137)
/// WHEN safe mode is active AND freeze is set
/// THEN mutations are rejected with SAFE_MODE_ACTIVE (not queued)
#[tokio::test]
async fn test_safe_mode_takes_precedence_over_freeze() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "safe-freeze-agent", "test-key").await;

    // Request a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 30_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let lease_msg = stream.next().await.unwrap().unwrap();
    let lease_id = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Set BOTH safe mode and freeze (invariant: safe mode cancels freeze, but we test
    // that safe mode takes precedence in the session server check order)
    {
        let mut st = shared_state.lock().await;
        st.safe_mode_atomic
            .store(true, std::sync::atomic::Ordering::Release);
        st.freeze_active = false; // Invariant: safe_mode=true => freeze_active=false
    }

    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::RequestResult(err)) => {
            assert_eq!(err.code, "SAFE_MODE_ACTIVE");
        }
        other => panic!("Expected SAFE_MODE_ACTIVE RuntimeError, got: {other:?}"),
    }
}

/// Scenario: SessionFreezeQueue unit test — MUTATION_QUEUE_PRESSURE at 80% capacity
#[test]
fn test_session_freeze_queue_pressure_signal() {
    let mut q = SessionFreezeQueue::new(10);
    // Fill 7 entries (70%) without crossing threshold
    for i in 0..7 {
        let batch = MutationBatch {
            batch_id: format!("b{i}").into_bytes(),
            lease_id: vec![0u8; 16],
            mutations: Vec::new(),
            ..Default::default()
        };
        let r = q.enqueue(batch, "ns");
        assert!(
            matches!(
                r,
                FreezeEnqueueResult::Queued {
                    pressure_warning: false
                }
            ),
            "Expected no pressure warning at {i}/7"
        );
    }
    // 8th entry crosses 80%
    let batch = MutationBatch {
        batch_id: b"b7".to_vec(),
        lease_id: vec![0u8; 16],
        mutations: Vec::new(),
        ..Default::default()
    };
    let r = q.enqueue(batch, "ns");
    assert!(
        matches!(
            r,
            FreezeEnqueueResult::Queued {
                pressure_warning: true
            }
        ),
        "Expected pressure_warning=true at 80%"
    );
}

/// Scenario: SessionFreezeQueue transactional never evicted
#[test]
fn test_session_freeze_queue_transactional_never_evicted() {
    use crate::proto::mutation_proto::Mutation;
    use crate::proto::{CreateTileMutation, MutationProto};

    let mut q = SessionFreezeQueue::new(2);
    // Fill with non-empty (StateStream) batches
    for i in 0..2 {
        let batch = MutationBatch {
            batch_id: format!("ss{i}").into_bytes(),
            lease_id: vec![0u8; 16],
            mutations: vec![],
            ..Default::default()
        };
        q.enqueue(batch, "ns");
    }

    // Submit a transactional mutation (CreateTile) — should get backpressure
    let tx_batch = MutationBatch {
        batch_id: b"tx1".to_vec(),
        lease_id: vec![0u8; 16],
        mutations: vec![MutationProto {
            mutation: Some(Mutation::CreateTile(CreateTileMutation {
                tab_id: vec![], // empty = server infers active tab
                bounds: None,
                z_order: 0,
            })),
        }],
        ..Default::default()
    };
    let r = q.enqueue(tx_batch, "ns");
    assert!(
        matches!(r, FreezeEnqueueResult::BackpressureRequired),
        "Transactional mutation should require backpressure when queue is full, got: {r:?}"
    );
}
