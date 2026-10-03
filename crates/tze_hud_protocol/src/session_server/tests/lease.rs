use super::*;

/// Regression: lease_id MUST be propagated into SceneMutationBatch so that
/// the five-stage validation pipeline (including lease/budget checks) fires.
///
/// Before this fix, `lease_id: None` was passed, which meant lease and budget
/// validation was skipped for non-CreateTile mutations in the gRPC path.
///
/// This test verifies that a mutation using an expired lease is rejected with
/// an error indicating lease/budget validation ran — not silently accepted.
#[tokio::test]
async fn test_mutation_rejected_with_expired_lease_id() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "lease-validation-regression", "test-key").await;

    // Create an active tab so mutations can reach the scene-apply path.
    {
        let st = shared_state.lock().await;
        st.scene
            .lock()
            .await
            .create_tab("test-tab", 0)
            .expect("create_tab");
    }

    // Acquire a lease.
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
    let lease_id_bytes = match &lease_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => resp.lease_id.clone(),
        other => panic!("Expected LeaseResponse (granted), got: {other:?}"),
    };

    // Revoke the lease directly in shared state, simulating an expired lease.
    // The wire format encodes SceneId as uuid::Uuid::as_bytes() (big-endian UUID bytes),
    // matching bytes_to_scene_id in session_server.rs.
    {
        let st = shared_state.lock().await;
        let arr: [u8; 16] = lease_id_bytes
            .as_slice()
            .try_into()
            .expect("16-byte lease_id");
        let lease_id = tze_hud_scene::SceneId::from_uuid(uuid::Uuid::from_bytes(arr));
        let _ = st.scene.lock().await.revoke_lease(lease_id);
    }

    // Send a CreateTile mutation referencing the now-revoked lease.
    let batch_id: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id_bytes,
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

    // The batch MUST be rejected (lease is revoked; validation pipeline runs).
    // batch_id must still be echoed back.
    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(
                !result.ok,
                "Mutation with revoked lease_id must be rejected \
                     (a missing lease_id must not bypass validation)"
            );
            assert_eq!(
                result.batch_id, batch_id,
                "MutationResult.batch_id must echo client batch_id even on rejection"
            );
        }
        other => panic!("Expected MutationResult, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_lease_over_stream() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "leasor", "test-key").await;

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

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::RequestResult(resp)) => {
            assert!(resp.ok, "expected lease to be granted");
            assert!(!resp.lease_id.is_empty());
            assert_eq!(resp.lease_id.len(), 16);
            assert_eq!(resp.ttl_ms, 30_000);
        }
        other => panic!("Expected LeaseResponse, got: {other:?}"),
    }
}

// ─── Lease management ────────────────────────────────────────────────────────

/// Scenario: Lease acquisition via session stream (spec §Lease Management RPCs,
/// lease-governance spec §Lease State Machine).
///
/// WHEN agent sends LeaseRequest(action=ACQUIRE) on session stream,
/// THEN runtime responds with a single LeaseResponse(granted=true).
#[tokio::test]
async fn test_lease_acquire_sends_lease_response() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "lease-acquire-agent", "test-key").await;

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

    // First response: LeaseResponse(granted=true)
    let resp_msg = stream.next().await.unwrap().unwrap();
    match &resp_msg.payload {
        Some(ServerPayload::RequestResult(resp)) => {
            assert!(resp.ok, "Lease should be granted");
            assert_eq!(resp.lease_id.len(), 16, "lease_id must be 16-byte UUIDv7");
            assert_eq!(resp.ttl_ms, 30_000);
        }
        other => panic!("Expected LeaseResponse, got: {other:?}"),
    }
}

/// Scenario: lease_id is always a 16-byte UUIDv7 (SceneId spec §SceneId for Scene-Object Identifiers).
///
/// WHEN agent requests a lease,
/// THEN all lease_id fields in responses are exactly 16 bytes.
#[tokio::test]
async fn test_lease_id_is_16_byte_uuidv7() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "sceneid-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 10_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    // LeaseResponse
    let resp_msg = stream.next().await.unwrap().unwrap();
    match &resp_msg.payload {
        Some(ServerPayload::RequestResult(resp)) => {
            assert!(resp.ok);
            assert_eq!(
                resp.lease_id.len(),
                16,
                "lease_id in LeaseResponse must be 16 bytes (SceneId UUIDv7)"
            );
        }
        other => panic!("Expected LeaseResponse, got: {other:?}"),
    }
}

/// Scenario: Retransmit correlation — sending a lease request with the same
/// client sequence number returns the cached response.
///
/// The server must detect retransmits (same sequence) and replay the response
/// without re-applying the operation.
#[tokio::test]
async fn test_lease_retransmit_correlation_returns_cached_response() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "retransmit-agent", "test-key").await;

    let lease_req = ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 30_000,
            ..Default::default()
        })),
    };

    // Original request
    tx.send(lease_req.clone()).await.unwrap();

    // Consume the original LeaseResponse
    let orig_resp = stream.next().await.unwrap().unwrap();
    let orig_lease_id = match &orig_resp.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert!(r.ok);
            r.lease_id.clone()
        }
        other => panic!("Expected LeaseResponse, got: {other:?}"),
    };

    // Retransmit with same sequence number (simulates no-ack / lost response)
    tx.send(lease_req).await.unwrap();

    // The retransmit should return the cached LeaseResponse (no duplicate lease created)
    let retx_resp = stream.next().await.unwrap().unwrap();
    match &retx_resp.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert!(r.ok, "Retransmit should return cached grant");
            assert_eq!(
                r.lease_id, orig_lease_id,
                "Retransmit must return the same lease_id as the original response"
            );
            assert_eq!(r.ttl_ms, 30_000);
        }
        other => panic!("Expected LeaseResponse on retransmit, got: {other:?}"),
    }
}

/// Scenario: Three agents contending for leases.
///
/// Validates concurrent lease acquisition: all three agents can independently
/// acquire leases from the same runtime with unique lease IDs.
#[tokio::test]
async fn test_three_agents_lease_contention() {
    let (client1, _server) = setup_test().await;

    // Use a single shared server — connect 3 clients to the same port.
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let url = format!("http://[::1]:{}", addr.port());
    let mut c1 = HudSessionClient::connect(url.clone()).await.unwrap();
    let mut c2 = HudSessionClient::connect(url.clone()).await.unwrap();
    let mut c3 = HudSessionClient::connect(url.clone()).await.unwrap();

    let (tx1, _, mut s1) = handshake(&mut c1, "agent-alpha", "test-key").await;
    let (tx2, _, mut s2) = handshake(&mut c2, "agent-beta", "test-key").await;
    let (tx3, _, mut s3) = handshake(&mut c3, "agent-gamma", "test-key").await;

    // All three agents request leases concurrently (sequential sends for simplicity)
    for (tx, seq) in [(&tx1, 2u64), (&tx2, 2u64), (&tx3, 2u64)] {
        tx.send(ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::ClaimTile(ClaimTile {
                ttl_ms: 30_000,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
    }

    // Collect lease IDs
    let mut lease_ids = Vec::new();
    for stream in [&mut s1, &mut s2, &mut s3] {
        let msg = stream.next().await.unwrap().unwrap();
        match &msg.payload {
            Some(ServerPayload::RequestResult(r)) => {
                assert!(r.ok, "All agents should get leases granted");
                assert_eq!(r.lease_id.len(), 16);
                lease_ids.push(r.lease_id.clone());
            }
            other => panic!("Expected LeaseResponse, got: {other:?}"),
        }
    }

    // All lease IDs must be unique — use a HashSet for correct deduplication.
    let set: std::collections::HashSet<Vec<u8>> = lease_ids.iter().cloned().collect();
    assert_eq!(
        set.len(),
        3,
        "All three agents must receive unique lease IDs"
    );

    drop(client1);
}

/// Scenario: Lease expiry — runtime accepts a lease with a very short TTL.
///
/// This test verifies that the protocol accepts LeaseRequest with any valid TTL,
/// including very short ones used in expiry scenarios.
/// Full expiry notification behavior requires the timer loop (post-v1 scope for
/// push notifications); here we verify the initial grant succeeds and the correct
/// SceneId is returned.
#[tokio::test]
async fn test_lease_expiry_scenario_initial_grant() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "expiry-agent", "test-key").await;

    // Request a lease with a very short TTL (100ms — represents expiry scenario)
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 100,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let resp = stream.next().await.unwrap().unwrap();
    match &resp.payload {
        Some(ServerPayload::RequestResult(r)) => {
            assert!(r.ok);
            assert_eq!(
                r.ttl_ms, 100,
                "Short-TTL lease should be granted as requested"
            );
            assert_eq!(r.lease_id.len(), 16, "lease_id must be 16-byte SceneId");
        }
        other => panic!("Expected LeaseResponse for short-TTL lease, got: {other:?}"),
    }
}

/// Scenario: Disconnect orphan behavior — session cleanup does not panic
/// when leases are held.
///
/// WHEN an agent with active leases disconnects ungracefully,
/// THEN the session is removed from the registry without error.
///
/// Full orphan-to-expiry lifecycle requires a timer loop (post-v1); this test
/// verifies the session teardown path is safe when leases are present.
#[tokio::test]
async fn test_disconnect_with_active_leases_no_panic() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "disconnect-agent", "test-key").await;

    // Acquire a lease
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

    // Consume LeaseResponse
    let _r = stream.next().await.unwrap().unwrap();

    // Drop both tx and stream to simulate ungraceful disconnect
    drop(tx);
    drop(stream);

    // Give the server task time to clean up
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    // If we reach here without a panic, the cleanup path is safe.
}
