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
    let (mut client, handle) = setup_widget_test().await;
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
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
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
