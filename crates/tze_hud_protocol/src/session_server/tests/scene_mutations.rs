use super::*;

#[tokio::test]
async fn test_mutation_over_stream() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "mutator", "test-key").await;

    // First, request a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
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

    // Create a tab in the scene (needed for mutations)
    // We need to do this through shared state since tab creation
    // isn't exposed via the streaming protocol yet.
    // For the test, we'll send a mutation that doesn't require a tab.

    // Send a mutation batch
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: vec![crate::proto::MutationProto {
                mutation: Some(crate::proto::mutation_proto::Mutation::CreateTile(
                    crate::proto::CreateTileMutation {
                        tab_id: vec![], // empty = server infers active tab
                        bounds: Some(crate::proto::Rect {
                            x: 0.0,
                            y: 0.0,
                            width: 200.0,
                            height: 150.0,
                        }),
                        z_order: 1,
                    },
                )),
            }],
            timing: None,
        })),
    })
    .await
    .unwrap();

    let result_msg = stream.next().await.unwrap().unwrap();
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            // This will fail because no active tab exists, which is expected
            // in this isolated test. The important thing is that the protocol
            // round-trip works.
            assert_eq!(result.batch_id, batch_id);
            // accepted may be false due to "no active tab" -- that's fine
        }
        other => panic!("Expected MutationResult, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_create_tile_persists_element_store_entry() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let path = std::env::temp_dir().join(format!(
        "tze_hud_element_store_session_server_{}.toml",
        SceneId::new()
    ));
    let _ = std::fs::remove_file(&path);

    {
        let mut st = shared_state.lock().await;
        st.element_store = tze_hud_scene::element_store::ElementStore::default();
        st.element_store_path = Some(path.clone());
        st.scene
            .lock()
            .await
            .create_tab("main", 0)
            .expect("create tab");
    }

    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "persist-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .expect("lease request");

    let claim_msg = next_server_msg(&mut stream).await;
    let created_tile_id = match &claim_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(result.ok, "ClaimTile must be accepted");
            bytes_to_scene_id(&result.ids[0]).expect("valid created tile id bytes")
        }
        other => panic!("Expected RequestResult, got: {other:?}"),
    };

    let store = load_element_store_for_test(&path).expect("load persisted element store");
    let entry = store
        .entries
        .get(&created_tile_id)
        .expect("tile id should be persisted");
    assert_eq!(
        entry.element_type,
        tze_hud_scene::element_store::ElementType::Tile
    );
    assert_eq!(entry.namespace, "persist-agent");
    assert!(entry.created_at > 0);
    assert!(entry.last_published_at >= entry.created_at);

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn test_existing_tile_last_published_update_triggers_persist() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let path = std::env::temp_dir().join(format!(
        "tze_hud_element_store_last_published_{}.toml",
        SceneId::new()
    ));
    let _ = std::fs::remove_file(&path);

    {
        let mut st = shared_state.lock().await;
        st.element_store = tze_hud_scene::element_store::ElementStore::default();
        st.element_store_path = Some(path.clone());
        st.scene
            .lock()
            .await
            .create_tab("main", 0)
            .expect("create tab");
    }

    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "persist-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .expect("lease request");

    let claim_msg = next_server_msg(&mut stream).await;
    let created_tile_id = match &claim_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(result.ok, "ClaimTile must be accepted");
            bytes_to_scene_id(&result.ids[0]).expect("valid created tile id bytes")
        }
        other => panic!("Expected RequestResult, got: {other:?}"),
    };

    let baseline_store = load_element_store_for_test(&path).expect("load baseline element store");
    let baseline_entry = baseline_store
        .entries
        .get(&created_tile_id)
        .expect("baseline tile id should be persisted");
    let baseline_last_published = baseline_entry.last_published_at;

    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    let persist_request = {
        let mut st = shared_state.lock().await;
        let entry = st
            .element_store
            .entries
            .get_mut(&created_tile_id)
            .expect("in-memory tile entry should exist");
        entry.last_published_at = baseline_last_published;
        persist_created_tile_entries(&mut st, &[created_tile_id]).await
    };
    persist_element_store(persist_request).await;

    let updated_store = load_element_store_for_test(&path).expect("reload element store");
    let updated_entry = updated_store
        .entries
        .get(&created_tile_id)
        .expect("tile id should remain persisted");
    assert!(
        updated_entry.last_published_at > baseline_last_published,
        "last_published_at update must be persisted when it is the only changed field"
    );

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn test_mutation_result_echoes_client_batch_id() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "batch-id-regression", "test-key").await;

    // Acquire a lease so the batch reaches the batch_id mapping code
    // (lease validation runs first; an invalid lease returns early before
    // the batch_id mapping happens).
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let lease_msg = next_server_msg(&mut stream).await;
    let lease_id = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Send a mutation batch with a known, unique batch_id.
    let client_batch_id: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: client_batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: vec![crate::proto::MutationProto {
                mutation: Some(crate::proto::mutation_proto::Mutation::CreateTile(
                    crate::proto::CreateTileMutation {
                        tab_id: vec![],
                        bounds: Some(crate::proto::Rect {
                            x: 0.0,
                            y: 0.0,
                            width: 100.0,
                            height: 100.0,
                        }),
                        z_order: 0,
                    },
                )),
            }],
            timing: None,
        })),
    })
    .await
    .unwrap();

    // CreateTile is runtime-internal, so the batch is rejected.
    // Regardless of rejection, MutationResult.batch_id MUST equal client_batch_id.
    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert_eq!(
                result.batch_id, client_batch_id,
                "MutationResult.batch_id must echo the client-provided batch_id \
                     (batch_id must be echoed, not regenerated)"
            );
            assert!(!result.ok);
            assert_eq!(result.code, "INVALID_ARGUMENT");
            assert!(result.hint.contains("ClaimTile"), "{result:?}");
        }
        other => panic!("Expected MutationResult, got: {other:?}"),
    }

    let malformed_batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: malformed_batch_id.clone(),
            lease_id: vec![1, 2, 3],
            mutations: Vec::new(),
            timing: None,
        })),
    })
    .await
    .unwrap();
    let rejected = next_server_msg(&mut stream).await;
    let rejection = match rejected.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(!result.ok);
            assert_eq!(result.seq, 4);
            assert_eq!(result.batch_id, malformed_batch_id);
            assert_eq!(result.code, "INVALID_ARGUMENT");
            assert!(result.hint.starts_with("Invalid lease_id bytes"));
            assert!(
                result
                    .hint
                    .contains("16-byte lease_id returned by ClaimTile")
            );
            assert!(result.hint.contains("new request or batch_id"));
            result
        }
        other => panic!("Expected malformed lease rejection, got: {other:?}"),
    };

    // A corrected empty batch would succeed if reapplied. The same batch_id
    // must instead replay the rejection and its guidance with new correlation.
    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: malformed_batch_id,
            lease_id,
            mutations: Vec::new(),
            timing: None,
        })),
    })
    .await
    .unwrap();
    let replayed = next_server_msg(&mut stream).await;
    assert!(replayed.sequence > rejected.sequence);
    match replayed.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(!result.ok);
            assert_eq!(result.seq, 5);
            assert_eq!(result.batch_id, rejection.batch_id);
            assert_eq!(result.code, rejection.code);
            assert_eq!(result.hint, rejection.hint);
            assert_eq!(result.ids, rejection.ids);
        }
        other => panic!("Expected cached rejection, got: {other:?}"),
    }
}

// ─── Mutation deduplication ──────────────────────────────────────────────────

/// Scenario: duplicate batch_id within window returns cached MutationResult.
#[tokio::test]
async fn test_mutation_dedup_returns_cached_result() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "dedup-agent", "test-key").await;

    // Obtain a lease
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
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

    // Send first MutationBatch with a unique batch_id
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
    let first_result = next_server_msg(&mut stream).await;
    let first_accepted = match &first_result.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert_eq!(r.batch_id, batch_id);
            r.ok
        }
        other => panic!("Expected MutationResult, got: {other:?}"),
    };

    // Retransmit with the same batch_id but a new sequence number
    tx.send(ClientMessage {
        sequence: 4, // new sequence
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(), // same batch_id
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: None,
        })),
    })
    .await
    .unwrap();
    let dedup_result = stream.next().await.unwrap().unwrap();
    match &dedup_result.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert_eq!(
                r.batch_id, batch_id,
                "batch_id must be echoed from cached result"
            );
            assert_eq!(
                r.ok, first_accepted,
                "Dedup must return cached accepted flag"
            );
        }
        other => panic!("Expected cached MutationResult on retransmit, got: {other:?}"),
    }

    drop(tx);
}
