use super::*;

fn tiny_png_1x1_rgba() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x56, 0xc7, 0x2f, 0x0d, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}

fn tiny_rgba_1x1(pixel: [u8; 4]) -> Vec<u8> {
    pixel.to_vec()
}

#[tokio::test]
async fn test_resource_upload_backpressure_keeps_heartbeat_responsive() {
    let service = setup_widget_service().await;
    service.state.lock().await.resource_store =
        tze_hud_resource::ResourceStore::new(tze_hud_resource::ResourceStoreConfig {
            upload_rate_limit_bytes_per_sec: 8,
            ..tze_hud_resource::ResourceStoreConfig::default()
        });
    let (mut client, handle) = setup_widget_test_with_service(service).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-heartbeat-backpressure",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let chunk_a = vec![0xAB; 8];
    let chunk_b = vec![0xCD; 8];
    let payload = [chunk_a.clone(), chunk_b.clone()].concat();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 1, // IMAGE_RGBA8
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata {
                width: 2,
                height: 2,
                ..ResourceMetadata::default()
            }),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let accepted = next_server_msg(&mut stream).await;
    let upload_id = match &accepted.payload {
        Some(ServerPayload::ResourceUploadAccepted(accepted)) => accepted.upload_id.clone(),
        other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
    };

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_id.clone(),
            chunk_index: 0,
            data: chunk_a,
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_id.clone(),
            chunk_index: 1,
            data: chunk_b,
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_id.clone(),
            },
        )),
    })
    .await
    .unwrap();

    let heartbeat_ts = 4242u64;
    tx.send(ClientMessage {
        sequence: 6,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: heartbeat_ts,
        })),
    })
    .await
    .unwrap();

    let heartbeat_echo = tokio::time::timeout(
        tokio::time::Duration::from_millis(300),
        next_server_msg(&mut stream),
    )
    .await
    .expect("heartbeat should not be blocked by upload backpressure");

    match &heartbeat_echo.payload {
        Some(ServerPayload::Heartbeat(hb)) => {
            assert_eq!(hb.timestamp_mono_us, heartbeat_ts);
        }
        other => panic!("expected Heartbeat echo, got: {other:?}"),
    }

    let stored = tokio::time::timeout(
        tokio::time::Duration::from_secs(3),
        next_server_msg(&mut stream),
    )
    .await
    .expect("expected ResourceStored after backpressure interval");

    match &stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 2);
            assert_eq!(stored.upload_id, upload_id);
            assert!(!stored.was_deduplicated);
        }
        other => panic!("expected ResourceStored after chunk backpressure, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_start_requires_upload_resource_capability() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) =
        handshake_with_capabilities(&mut client, "resource-no-cap", "test-key", &[]).await;
    let payload = tiny_png_1x1_rgba();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 2,
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: payload,
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::ResourceErrorResponse(err)) => {
            assert_eq!(err.request_sequence, 2);
            assert_eq!(err.error_code, 1);
            assert!(err.upload_id.is_empty());
        }
        other => panic!("expected ResourceErrorResponse, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_inline_and_dedup_short_circuit() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-inline",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let payload = tiny_png_1x1_rgba();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash.clone(),
            resource_type: 2,
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: payload.clone(),
        })),
    })
    .await
    .unwrap();

    let first = next_server_msg(&mut stream).await;
    match &first.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 2);
            assert!(!stored.was_deduplicated);
            assert!(stored.upload_id.is_empty());
        }
        other => panic!("expected ResourceStored, got: {other:?}"),
    }

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 2,
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let second = next_server_msg(&mut stream).await;
    match &second.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 3);
            assert!(stored.was_deduplicated);
            assert!(stored.upload_id.is_empty());
        }
        other => panic!("expected ResourceStored on dedup short-circuit, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_chunked_ack_then_complete() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-chunked",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let payload = tiny_png_1x1_rgba();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 2,
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let accepted = next_server_msg(&mut stream).await;
    let upload_id = match &accepted.payload {
        Some(ServerPayload::ResourceUploadAccepted(accepted)) => {
            assert_eq!(accepted.request_sequence, 2);
            assert_eq!(accepted.upload_id.len(), 16);
            accepted.upload_id.clone()
        }
        other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
    };

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_id.clone(),
            chunk_index: 0,
            data: payload.clone(),
        })),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_id.clone(),
            },
        )),
    })
    .await
    .unwrap();

    let stored = next_server_msg(&mut stream).await;
    match &stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            assert_eq!(stored.request_sequence, 2);
            assert_eq!(stored.upload_id, upload_id);
            assert!(!stored.was_deduplicated);
        }
        other => panic!("expected ResourceStored on complete, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_chunked_concurrent_limit_rejected() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-concurrent-limit",
        "test-key",
        &["upload_resource"],
    )
    .await;

    // ResourceStore allows at most 4 in-flight uploads per agent namespace.
    for offset in 0..5u8 {
        let seq = u64::from(offset) + 2;
        let payload = tiny_rgba_1x1([offset, 0, 0, 0xFF]);
        tx.send(ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
                expected_hash: blake3::hash(&payload).as_bytes().to_vec(),
                resource_type: 1, // IMAGE_RGBA8
                total_size_bytes: payload.len() as u64,
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

        let msg = next_server_msg(&mut stream).await;
        if offset < 4 {
            match &msg.payload {
                Some(ServerPayload::ResourceUploadAccepted(accepted)) => {
                    assert_eq!(accepted.request_sequence, seq);
                    assert_eq!(accepted.upload_id.len(), 16);
                }
                other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
            }
        } else {
            match &msg.payload {
                Some(ServerPayload::ResourceErrorResponse(err)) => {
                    assert_eq!(err.request_sequence, seq);
                    assert_eq!(err.error_code, 8);
                    assert!(err.upload_id.is_empty());
                }
                other => panic!("expected ResourceErrorResponse, got: {other:?}"),
            }
        }
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_chunked_success_correlates_by_request_sequence() {
    let service = setup_widget_service().await;
    let shared_state = service.state.clone();
    let (mut client, handle) = setup_widget_test_with_service(service).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-correlation",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let payload_a = tiny_rgba_1x1([0, 0, 0, 0xFF]);
    let payload_b = tiny_rgba_1x1([0xFF, 0, 0, 0xFF]);
    let expected_a = blake3::hash(&payload_a).as_bytes().to_vec();
    let expected_b = blake3::hash(&payload_b).as_bytes().to_vec();

    for (seq, expected_hash) in [(2u64, expected_a.clone()), (3u64, expected_b.clone())] {
        tx.send(ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
                expected_hash,
                resource_type: 1, // IMAGE_RGBA8
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
    }

    let mut upload_id_by_request = HashMap::new();
    for _ in 0..2 {
        let msg = next_server_msg(&mut stream).await;
        match &msg.payload {
            Some(ServerPayload::ResourceUploadAccepted(accepted)) => {
                upload_id_by_request.insert(accepted.request_sequence, accepted.upload_id.clone());
            }
            other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
        }
    }
    assert_eq!(upload_id_by_request.len(), 2);
    let upload_a = upload_id_by_request
        .get(&2)
        .expect("request 2 must have upload_id")
        .clone();
    let upload_b = upload_id_by_request
        .get(&3)
        .expect("request 3 must have upload_id")
        .clone();

    // Complete request 3 before request 2 to assert correlation semantics.
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_b.clone(),
            chunk_index: 0,
            data: payload_b.clone(),
        })),
    })
    .await
    .unwrap();
    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_b.clone(),
            },
        )),
    })
    .await
    .unwrap();

    tx.send(ClientMessage {
        sequence: 6,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_a.clone(),
            chunk_index: 0,
            data: payload_a.clone(),
        })),
    })
    .await
    .unwrap();
    tx.send(ClientMessage {
        sequence: 7,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete {
                upload_id: upload_a.clone(),
            },
        )),
    })
    .await
    .unwrap();

    let mut stored_by_request = HashMap::new();
    for _ in 0..2 {
        let msg = next_server_msg(&mut stream).await;
        match &msg.payload {
            Some(ServerPayload::ResourceStored(stored)) => {
                let bytes = stored
                    .resource_id
                    .as_ref()
                    .expect("resource_id must be present")
                    .bytes
                    .clone();
                stored_by_request
                    .insert(stored.request_sequence, (stored.upload_id.clone(), bytes));
            }
            other => panic!("expected ResourceStored, got: {other:?}"),
        }
    }

    assert_eq!(stored_by_request.len(), 2);
    assert_eq!(
        stored_by_request
            .get(&2)
            .expect("request 2 stored result must exist")
            .0,
        upload_a
    );
    assert_eq!(
        stored_by_request
            .get(&3)
            .expect("request 3 stored result must exist")
            .0,
        upload_b
    );
    assert_eq!(
        stored_by_request
            .get(&2)
            .expect("request 2 stored result must exist")
            .1,
        expected_a
    );
    assert_eq!(
        stored_by_request
            .get(&3)
            .expect("request 3 stored result must exist")
            .1,
        expected_b
    );

    // A real disconnect must clean only its worker's pending IDs, even when
    // another authenticated connection has the same namespace.
    let mut retained_check_sequence = 8;
    for (owner, peer, color) in [
        ("resource-correlation", "resource-correlation", 31u8),
        ("resource-owner", "resource-peer", 47u8),
    ] {
        let (owner_tx, owner_init, mut owner_stream) =
            handshake_with_capabilities(&mut client, owner, "test-key", &["upload_resource"]).await;
        let (peer_tx, peer_init, mut peer_stream) =
            handshake_with_capabilities(&mut client, peer, "test-key", &["upload_resource"]).await;
        let owner_session_id = match &owner_init[0].payload {
            Some(ServerPayload::SessionEstablished(e)) => bytes_to_scene_id(&e.session_id).unwrap(),
            other => panic!("expected owner SessionEstablished, got {other:?}"),
        };
        let peer_session_id = match &peer_init[0].payload {
            Some(ServerPayload::SessionEstablished(e)) => bytes_to_scene_id(&e.session_id).unwrap(),
            other => panic!("expected peer SessionEstablished, got {other:?}"),
        };
        assert_ne!(owner_session_id, peer_session_id);
        let peer_bytes = tiny_rgba_1x1([color, 1, 2, 255]);
        let start = |sequence, bytes: &[u8]| ClientMessage {
            sequence,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
                expected_hash: blake3::hash(bytes).as_bytes().to_vec(),
                resource_type: 1,
                total_size_bytes: bytes.len() as u64,
                metadata: Some(ResourceMetadata {
                    width: 1,
                    height: 1,
                    ..Default::default()
                }),
                inline_data: Vec::new(),
            })),
        };
        owner_tx.send(start(2, &[color, 9, 9, 255])).await.unwrap();
        peer_tx.send(start(2, &peer_bytes)).await.unwrap();
        let owner_accepted =
            tokio::time::timeout(Duration::from_secs(5), next_server_msg(&mut owner_stream))
                .await
                .unwrap();
        assert!(matches!(
            owner_accepted.payload,
            Some(ServerPayload::ResourceUploadAccepted(_))
        ));
        let peer_accepted =
            tokio::time::timeout(Duration::from_secs(5), next_server_msg(&mut peer_stream))
                .await
                .unwrap();
        let peer_upload = match peer_accepted.payload {
            Some(ServerPayload::ResourceUploadAccepted(a)) => {
                assert_eq!(a.request_sequence, 2);
                a.upload_id
            }
            other => panic!("expected peer upload acknowledged before disconnect, got {other:?}"),
        };
        let (cleanup, duplicate_cleanup) = {
            let mut st = shared_state.lock().await;
            (
                st.sessions.observe_cleanup(&owner_session_id),
                st.sessions.observe_cleanup(&owner_session_id),
            )
        };
        drop(owner_tx);
        drop(owner_stream);
        for witness in [cleanup, duplicate_cleanup] {
            tokio::time::timeout(Duration::from_secs(5), witness)
                .await
                .expect("actual owner cleanup must finish")
                .unwrap();
        }
        for (hash, bytes) in [(&expected_a, &payload_a), (&expected_b, &payload_b)] {
            tx.send(ClientMessage {
                sequence: retained_check_sequence,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
                    expected_hash: hash.clone(),
                    resource_type: 1,
                    total_size_bytes: bytes.len() as u64,
                    metadata: Some(ResourceMetadata {
                        width: 1,
                        height: 1,
                        ..Default::default()
                    }),
                    inline_data: bytes.clone(),
                })),
            })
            .await
            .unwrap();
            let retained =
                tokio::time::timeout(Duration::from_secs(5), next_server_msg(&mut stream))
                    .await
                    .unwrap();
            match retained.payload {
                Some(ServerPayload::ResourceStored(stored)) => {
                    assert_eq!(stored.request_sequence, retained_check_sequence);
                    assert!(
                        stored.was_deduplicated,
                        "completed immutable resource survives owner cleanup"
                    );
                    assert_eq!(stored.resource_id.unwrap().bytes, *hash);
                }
                other => panic!("expected retained completed resource, got {other:?}"),
            }
            retained_check_sequence += 1;
        }
        peer_tx
            .send(ClientMessage {
                sequence: 3,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
                    upload_id: peer_upload.clone(),
                    chunk_index: 0,
                    data: peer_bytes.clone(),
                })),
            })
            .await
            .unwrap();
        peer_tx
            .send(ClientMessage {
                sequence: 4,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ClientPayload::ResourceUploadComplete(
                    ResourceUploadComplete {
                        upload_id: peer_upload.clone(),
                    },
                )),
            })
            .await
            .unwrap();
        let completed =
            tokio::time::timeout(Duration::from_secs(5), next_server_msg(&mut peer_stream))
                .await
                .unwrap();
        match completed.payload {
            Some(ServerPayload::ResourceStored(stored)) => {
                assert_eq!(stored.request_sequence, 2);
                assert_eq!(stored.upload_id, peer_upload);
                assert_eq!(
                    stored.resource_id.unwrap().bytes,
                    blake3::hash(&peer_bytes).as_bytes().to_vec()
                );
            }
            other => {
                panic!("peer {peer} upload must survive {owner} actual cleanup, got {other:?}")
            }
        }
        let peer_cleanup = shared_state
            .lock()
            .await
            .sessions
            .observe_cleanup(&peer_session_id);
        drop(peer_tx);
        drop(peer_stream);
        tokio::time::timeout(Duration::from_secs(5), peer_cleanup)
            .await
            .unwrap()
            .unwrap();
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_chunked_zero_size_rejected() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-zero-size",
        "test-key",
        &["upload_resource"],
    )
    .await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: vec![0xAB; 32],
            resource_type: 2,
            total_size_bytes: 0,
            metadata: Some(ResourceMetadata::default()),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::ResourceErrorResponse(err)) => {
            assert_eq!(err.request_sequence, 2);
            assert_eq!(err.error_code, 3);
            assert!(err.upload_id.is_empty());
            assert!(
                err.message.contains("total_size_bytes"),
                "expected total_size guard message, got: {}",
                err.message
            );
        }
        other => panic!("expected ResourceErrorResponse, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_resource_upload_chunk_error_aborts_inflight_tracking() {
    let service = setup_widget_service().await;
    let shared_state = service.state.clone();
    let (mut client, handle) = setup_widget_test_with_service(service).await;
    let (tx, init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-chunk-error",
        "test-key",
        &["upload_resource"],
    )
    .await;

    let payload = tiny_png_1x1_rgba();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 2,
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: Vec::new(),
        })),
    })
    .await
    .unwrap();

    let accepted = next_server_msg(&mut stream).await;
    let upload_id = match &accepted.payload {
        Some(ServerPayload::ResourceUploadAccepted(accepted)) => accepted.upload_id.clone(),
        other => panic!("expected ResourceUploadAccepted, got: {other:?}"),
    };

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadChunk(ResourceUploadChunk {
            upload_id: upload_id.clone(),
            chunk_index: 1,
            data: payload.clone(),
        })),
    })
    .await
    .unwrap();

    let first_error = next_server_msg(&mut stream).await;
    match &first_error.payload {
        Some(ServerPayload::ResourceErrorResponse(err)) => {
            assert_eq!(err.request_sequence, 2);
            assert_eq!(err.error_code, 7);
            assert_eq!(err.upload_id, upload_id);
        }
        other => panic!("expected ResourceErrorResponse after bad chunk, got: {other:?}"),
    }

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadComplete(
            ResourceUploadComplete { upload_id },
        )),
    })
    .await
    .unwrap();

    let second_error = next_server_msg(&mut stream).await;
    match &second_error.payload {
        Some(ServerPayload::ResourceErrorResponse(err)) => {
            assert_eq!(err.request_sequence, 4);
            assert_eq!(err.error_code, 9);
        }
        other => panic!("expected ResourceErrorResponse after aborted upload, got: {other:?}"),
    }

    // All four default slots must be available after the failed chunk. The
    // fifth Start must still be rejected: cleanup does not relax admission.
    let start = |sequence, color| ClientMessage {
        sequence,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: blake3::hash(&[color, 2, 3, 255]).as_bytes().to_vec(),
            resource_type: 1,
            total_size_bytes: 4,
            metadata: Some(ResourceMetadata {
                width: 1,
                height: 1,
                ..Default::default()
            }),
            inline_data: Vec::new(),
        })),
    };
    let mut recovered_after_chunk = 0;
    for offset in 0..5u8 {
        let sequence = 5 + u64::from(offset);
        tx.send(start(sequence, offset)).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), next_server_msg(&mut stream))
            .await
            .unwrap();
        match reply.payload {
            Some(ServerPayload::ResourceUploadAccepted(a)) => {
                assert_eq!(a.request_sequence, sequence);
                recovered_after_chunk += 1;
            }
            Some(ServerPayload::ResourceErrorResponse(e)) => {
                assert_eq!(e.request_sequence, sequence);
                assert_eq!(e.error_code, 8);
            }
            other => panic!("expected capacity admission decision, got {other:?}"),
        }
    }
    let session_id = match &init_msgs[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => bytes_to_scene_id(&e.session_id).unwrap(),
        other => panic!("expected SessionEstablished, got {other:?}"),
    };
    let cleanup = shared_state
        .lock()
        .await
        .sessions
        .observe_cleanup(&session_id);
    drop(tx);
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .unwrap()
        .unwrap();

    // These are the actual production worker and channels. Closing its event
    // receiver models the session loop ending with Start/Chunk/Complete queued;
    // no late work may leave a pending store slot after the worker terminates.
    let (commands, command_rx) = tokio::sync::mpsc::channel(64);
    let (events, event_rx) = tokio::sync::mpsc::channel(1);
    let late_id_bytes = tiny_rgba_1x1([99, 98, 97, 255]);
    commands
        .send(UploadWorkerCommand::Start {
            request_sequence: 2,
            capabilities: vec!["upload_resource".to_string()],
            start: ResourceUploadStart {
                expected_hash: blake3::hash(&late_id_bytes).as_bytes().to_vec(),
                resource_type: 1,
                total_size_bytes: 4,
                metadata: Some(ResourceMetadata {
                    width: 1,
                    height: 1,
                    ..Default::default()
                }),
                inline_data: Vec::new(),
            },
        })
        .await
        .unwrap();
    commands
        .send(UploadWorkerCommand::Chunk {
            request_sequence: 3,
            chunk: ResourceUploadChunk {
                upload_id: vec![0; 16],
                chunk_index: 0,
                data: late_id_bytes,
            },
        })
        .await
        .unwrap();
    commands
        .send(UploadWorkerCommand::Complete {
            request_sequence: 4,
            capabilities: vec!["upload_resource".to_string()],
            complete: ResourceUploadComplete {
                upload_id: vec![0; 16],
            },
        })
        .await
        .unwrap();
    drop(commands);
    drop(event_rx);
    tokio::time::timeout(
        Duration::from_secs(5),
        run_upload_worker(
            shared_state.clone(),
            "resource-chunk-error".to_string(),
            command_rx,
            events,
            0,
            Default::default(),
        ),
    )
    .await
    .expect("closed event lane must not prevent worker termination");
    let (fresh_tx, fresh_init, mut fresh_stream) = handshake_with_capabilities(
        &mut client,
        "resource-chunk-error",
        "test-key",
        &["upload_resource"],
    )
    .await;
    let mut recovered_after_worker = 0;
    for offset in 0..5u8 {
        let sequence = 2 + u64::from(offset);
        fresh_tx.send(start(sequence, offset + 10)).await.unwrap();
        let reply =
            tokio::time::timeout(Duration::from_secs(5), next_server_msg(&mut fresh_stream))
                .await
                .unwrap();
        match reply.payload {
            Some(ServerPayload::ResourceUploadAccepted(a)) => {
                assert_eq!(a.request_sequence, sequence);
                recovered_after_worker += 1;
            }
            Some(ServerPayload::ResourceErrorResponse(e)) => {
                assert_eq!(e.request_sequence, sequence);
                assert_eq!(e.error_code, 8);
            }
            other => panic!("expected post-worker capacity admission decision, got {other:?}"),
        }
    }
    assert_eq!(
        (recovered_after_chunk, recovered_after_worker),
        (4, 4),
        "failed chunk and terminated worker must both recover owned slots, retaining cap4"
    );

    let fresh_session_id = match &fresh_init[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => bytes_to_scene_id(&e.session_id).unwrap(),
        other => panic!("expected fresh SessionEstablished, got {other:?}"),
    };
    let cleanup = shared_state
        .lock()
        .await
        .sessions
        .observe_cleanup(&fresh_session_id);
    drop(fresh_tx);
    drop(fresh_stream);
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .unwrap()
        .unwrap();

    // Saturate a real worker event lane without consuming its first accepted
    // reply. Closing that full lane must wake a blocked send and finish cleanup.
    let (commands, command_rx) = tokio::sync::mpsc::channel(64);
    let (events, event_rx) = tokio::sync::mpsc::channel(1);
    let event_capacity = events.clone();
    for color in [70u8, 71] {
        commands
            .send(UploadWorkerCommand::Start {
                request_sequence: u64::from(color),
                capabilities: vec!["upload_resource".to_string()],
                start: ResourceUploadStart {
                    expected_hash: blake3::hash(&[color, 2, 3, 255]).as_bytes().to_vec(),
                    resource_type: 1,
                    total_size_bytes: 4,
                    metadata: Some(ResourceMetadata {
                        width: 1,
                        height: 1,
                        ..Default::default()
                    }),
                    inline_data: Vec::new(),
                },
            })
            .await
            .unwrap();
    }
    drop(commands);
    let worker = tokio::spawn(run_upload_worker(
        shared_state.clone(),
        "resource-chunk-error".to_string(),
        command_rx,
        events,
        0,
        Default::default(),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while event_capacity.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual accepted event must fill the worker lane");
    assert_eq!(event_capacity.capacity(), 0);
    drop(event_rx);
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .expect("full event lane must not hang worker join")
        .unwrap();
    let (full_tx, _full_init, mut full_stream) = handshake_with_capabilities(
        &mut client,
        "resource-chunk-error",
        "test-key",
        &["upload_resource"],
    )
    .await;
    let mut recovered_after_full_lane = 0;
    for offset in 0..5u8 {
        let sequence = 2 + u64::from(offset);
        full_tx.send(start(sequence, offset + 20)).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), next_server_msg(&mut full_stream))
            .await
            .unwrap();
        match reply.payload {
            Some(ServerPayload::ResourceUploadAccepted(a)) => {
                assert_eq!(a.request_sequence, sequence);
                recovered_after_full_lane += 1;
            }
            Some(ServerPayload::ResourceErrorResponse(e)) => {
                assert_eq!(e.request_sequence, sequence);
                assert_eq!(e.error_code, 8);
            }
            other => panic!("expected post-full-lane capacity admission decision, got {other:?}"),
        }
    }
    assert_eq!(recovered_after_full_lane, 4);

    drop(handle);
}

#[tokio::test]
async fn test_resident_upload_then_static_image_references_uploaded_resource_id() {
    let service = setup_widget_service().await;
    let shared_state = service.state.clone();
    let (mut client, handle) = setup_widget_test_with_service(service).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "resource-scene-node",
        "test-key",
        &["upload_resource", "modify_own_tiles"],
    )
    .await;

    let payload = tiny_png_1x1_rgba();
    let hash = blake3::hash(&payload).as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ResourceUploadStart(ResourceUploadStart {
            expected_hash: hash,
            resource_type: 2, // IMAGE_PNG
            total_size_bytes: payload.len() as u64,
            metadata: Some(ResourceMetadata::default()),
            inline_data: payload,
        })),
    })
    .await
    .unwrap();

    let stored = next_server_msg(&mut stream).await;
    let resource_id_bytes = match stored.payload {
        Some(ServerPayload::ResourceStored(stored)) => {
            stored
                .resource_id
                .expect("resource_id must be present on success")
                .bytes
        }
        other => panic!("expected ResourceStored, got: {other:?}"),
    };
    let resource_id = ResourceId::from_bytes(
        resource_id_bytes
            .as_slice()
            .try_into()
            .expect("resource_id must be 32 bytes"),
    );
    {
        let st = shared_state.lock().await;
        let scene = st.scene.lock().await;
        assert!(
            scene.is_resource_registered(&resource_id),
            "uploaded resources must be registered for scene mutation validation"
        );
    }

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ClaimTile(ClaimTile {
            ttl_ms: 60_000,
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let claim_msg = next_server_msg(&mut stream).await;
    let (lease_id, tile_id_bytes) = match claim_msg.payload {
        Some(ServerPayload::RequestResult(resp)) if resp.ok => (resp.lease_id, resp.ids[0].clone()),
        other => panic!("expected a granted ClaimTile, got: {other:?}"),
    };

    let root_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 1,
            height: 1,
            decoded_bytes: 4,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 1.0, 1.0),
        }),
    };

    let set_root_batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: set_root_batch_id.clone(),
            lease_id,
            mutations: vec![crate::proto::MutationProto {
                mutation: Some(crate::proto::mutation_proto::Mutation::SetTileRoot(
                    crate::proto::SetTileRootMutation {
                        tile_id: tile_id_bytes.clone(),
                        node: Some(crate::convert::scene_node_to_proto(&root_node)),
                    },
                )),
            }],
            timing: None,
        })),
    })
    .await
    .unwrap();

    let set_root_result = next_server_msg(&mut stream).await;
    match set_root_result.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(result.ok, "set_tile_root should be accepted");
            assert_eq!(result.batch_id, set_root_batch_id);
        }
        other => panic!("expected MutationResult for set_tile_root, got: {other:?}"),
    }

    let tile_id = bytes_to_scene_id(&tile_id_bytes).expect("tile id from mutation must decode");
    {
        let st = shared_state.lock().await;
        let scene = st.scene.lock().await;
        let tile = scene.tiles.get(&tile_id).expect("tile must exist");
        let root_id = tile.root_node.expect("tile must have root node");
        let root = scene.nodes.get(&root_id).expect("root node must exist");
        match &root.data {
            NodeData::StaticImage(static_image) => {
                assert_eq!(
                    static_image.resource_id, resource_id,
                    "scene node must reference uploaded ResourceId"
                );
            }
            other => panic!("expected StaticImage root node, got: {other:?}"),
        }
    }

    drop(handle);
}

#[tokio::test]
async fn uploaded_resource_notifies_after_the_parked_checkpoint() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let service = setup_widget_service().await;
    let wake_generation = Arc::new(AtomicU64::new(0));
    let callback_generation = Arc::clone(&wake_generation);
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_generation.fetch_add(1, Ordering::AcqRel);
    });
    let resource_id = tze_hud_resource::ResourceId::from_bytes([0x5a; 32]);
    let parked_checkpoint = wake_generation.load(Ordering::Acquire);

    upload::register_uploaded_scene_resource(&service.state, &resource_id, &render_wake).await;

    let st = service.state.lock().await;
    let scene = st.scene.lock().await;
    assert!(scene.is_resource_registered(&ResourceId::from_bytes([0x5a; 32])));
    assert_eq!(
        wake_generation.load(Ordering::Acquire),
        parked_checkpoint + 1,
        "successful registration must publish a generation after the parked checkpoint"
    );
    drop(scene);
    drop(st);

    upload::register_uploaded_scene_resource(&service.state, &resource_id, &render_wake).await;
    assert_eq!(
        wake_generation.load(Ordering::Acquire),
        parked_checkpoint + 1,
        "duplicate resource registration is a no-op and must not wake"
    );
}
