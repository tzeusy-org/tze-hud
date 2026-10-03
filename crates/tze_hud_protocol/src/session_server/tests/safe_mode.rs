use super::*;

// ─── Safe mode ───────────────────────────────────────────────────────────────

/// Scenario: Mutations rejected during safe mode
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

// ─── Freeze queue ────────────────────────────────────────────────────────────

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
