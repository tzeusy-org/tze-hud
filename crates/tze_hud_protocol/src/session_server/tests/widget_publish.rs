use super::*;

/// Helper: start a server with a widget service using explicit asset-store limits.
async fn setup_widget_test_with_asset_limits(
    max_total_bytes: u64,
    max_namespace_bytes: u64,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let service = setup_widget_service().await;
    {
        let mut st = service.state.lock().await;
        st.widget_asset_store =
            crate::session::WidgetAssetStore::new_with_limits(max_total_bytes, max_namespace_bytes);
    }
    setup_widget_test_with_service(service).await
}

/// Helper: start a server with a durable runtime widget store.
async fn setup_widget_test_with_durable_store(
    store_path: std::path::PathBuf,
    max_total_bytes: u64,
    max_agent_bytes: u64,
) -> (
    HudSessionClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
) {
    let service = setup_widget_service().await;
    {
        let mut st = service.state.lock().await;
        st.runtime_widget_store = Some(
            tze_hud_resource::RuntimeWidgetStore::open(
                tze_hud_resource::RuntimeWidgetStoreConfig {
                    store_path,
                    max_total_bytes,
                    max_agent_bytes,
                },
            )
            .expect("durable runtime widget store should open for tests"),
        );
    }
    setup_widget_test_with_service(service).await
}

/// Scenario: Durable WidgetPublish with valid params receives WidgetPublishResult(accepted=true).
#[tokio::test]
async fn test_durable_widget_publish_receives_result() {
    let (mut client, _handle) = setup_widget_test().await;

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "widget-agent",
        "test-key",
        &["publish_widget:gauge"],
    )
    .await;

    // Send a WidgetPublish for the durable "gauge" widget
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![crate::proto::WidgetParameterValueProto {
                param_name: "level".to_string(),
                value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                    0.75,
                )),
            }],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(
                result.ok,
                "Durable widget publish must be accepted, got error: {}",
                result.code
            );
            assert!(result.code.is_empty(), "No error code on success");
            assert_eq!(result.seq, 2, "request_sequence must echo client sequence");
        }
        other => panic!("Expected WidgetPublishResult, got: {other:?}"),
    }

    drop(tx);
}

/// Scenario: WidgetPublish with missing capability receives WIDGET_CAPABILITY_MISSING.
#[tokio::test]
async fn test_widget_publish_missing_capability_rejected() {
    let (mut client, _handle) = setup_widget_test().await;

    // Handshake WITHOUT publish_widget:gauge capability
    let (tx, _init_msgs, mut stream) =
        handshake(&mut client, "widget-no-cap-agent", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(!result.ok, "Expected rejection");
            assert_eq!(
                result.code, "NOT_ALLOWED",
                "Expected NOT_ALLOWED, got: {}",
                result.code
            );
        }
        other => panic!("Expected WidgetPublishResult(rejected), got: {other:?}"),
    }

    drop(tx);
}

/// Scenario: wildcard publish_widget capability authorizes any widget publish.
#[tokio::test]
async fn test_widget_publish_wildcard_capability_allows_publish() {
    let (mut client, _handle) = setup_widget_test().await;

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "widget-wildcard-agent",
        "test-key",
        &["publish_widget:*"],
    )
    .await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(
                result.ok,
                "Expected wildcard capability to authorize publish"
            );
        }
        other => panic!("Expected WidgetPublishResult, got: {other:?}"),
    }

    drop(tx);
}

/// Scenario: WidgetPublish targeting unknown widget receives WIDGET_NOT_FOUND.
#[tokio::test]
async fn test_widget_publish_not_found() {
    let (mut client, _handle) = setup_widget_test().await;

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "widget-notfound-agent",
        "test-key",
        &["publish_widget:nonexistent"],
    )
    .await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:nonexistent".to_string(),
            params: vec![],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(!result.ok, "Expected rejection");
            assert_eq!(
                result.code, "WIDGET_NOT_FOUND",
                "Expected WIDGET_NOT_FOUND, got: {}",
                result.code
            );
        }
        other => panic!("Expected WidgetPublishResult(WIDGET_NOT_FOUND), got: {other:?}"),
    }

    drop(tx);
}

/// Scenario: WidgetPublish with unknown parameter receives WIDGET_UNKNOWN_PARAMETER.
#[tokio::test]
async fn test_widget_publish_unknown_parameter() {
    let (mut client, _handle) = setup_widget_test().await;

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "widget-badparam-agent",
        "test-key",
        &["publish_widget:gauge"],
    )
    .await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![crate::proto::WidgetParameterValueProto {
                param_name: "bogus_param".to_string(),
                value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                    0.5,
                )),
            }],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(!result.ok, "Expected rejection");
            assert_eq!(
                result.code, "WIDGET_PARAMETER_INVALID",
                "Expected WIDGET_PARAMETER_INVALID, got: {}",
                result.code
            );
        }
        other => {
            panic!("Expected WidgetPublishResult(WIDGET_UNKNOWN_PARAMETER), got: {other:?}")
        }
    }

    drop(tx);
}

/// Scenario: repeated durable WidgetPublish requests to the same widget are
/// unambiguously correlated by request_sequence.
#[tokio::test]
async fn test_durable_widget_publish_repeated_requests_are_correlated() {
    let (mut client, _handle) = setup_widget_test().await;

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "widget-correlation-agent",
        "test-key",
        &["publish_widget:gauge"],
    )
    .await;

    for (sequence, level) in [(2u64, 0.25f32), (3u64, 0.75f32)] {
        tx.send(ClientMessage {
            sequence,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::Publish(Publish {
                surface: "widget:gauge".to_string(),
                params: vec![crate::proto::WidgetParameterValueProto {
                    param_name: "level".to_string(),
                    value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                        level,
                    )),
                }],
                transition_ms: 0,
                key: String::new(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();

        let result_msg = next_server_msg(&mut stream).await;
        match &result_msg.payload {
            Some(ServerPayload::RequestResult(result)) => {
                assert_eq!(result.seq, sequence);
                assert!(result.ok, "expected durable publish to be accepted");
                assert!(result.code.is_empty());
                assert!(result.hint.is_empty());
            }
            other => panic!("Expected WidgetPublishResult, got: {other:?}"),
        }
    }

    drop(tx);
}

/// Scenario: Ephemeral WidgetPublish is fire-and-forget (no WidgetPublishResult).
#[tokio::test]
async fn test_ephemeral_widget_no_publish_result() {
    use tze_hud_scene::types::{
        ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetDefinition, WidgetInstance,
        WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue,
    };

    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");
    {
        let st = service.state.lock().await;
        let mut s = st.scene.lock().await;

        // Register an EPHEMERAL widget type
        s.widget_registry.register_definition(WidgetDefinition {
            id: "live-bar".to_string(),
            name: "LiveBar".to_string(),
            description: "Ephemeral bar widget".to_string(),
            parameter_schema: vec![WidgetParameterDeclaration {
                name: "value".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            }],
            layers: vec![],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.8,
                width_pct: 1.0,
                height_pct: 0.05,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: true, // ephemeral!
            hover_behavior: None,
        });

        let tab_id = s.create_tab("main", 0).unwrap();
        s.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "live-bar".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "live-bar".to_string(),
            current_params: std::collections::HashMap::new(),
        });
    }

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let mut client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "ephemeral-widget-agent",
        "test-key",
        &["publish_widget:live-bar"],
    )
    .await;

    // Publish to ephemeral widget
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:live-bar".to_string(),
            params: vec![crate::proto::WidgetParameterValueProto {
                param_name: "value".to_string(),
                value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                    0.9,
                )),
            }],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    // Send a heartbeat — the next response should be the echo (no WidgetPublishResult)
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 77777,
        })),
    })
    .await
    .unwrap();

    let next_msg = stream.next().await.unwrap().unwrap();
    match &next_msg.payload {
        Some(ServerPayload::RequestResult(_)) => {
            panic!("Ephemeral widget publish must NOT produce a WidgetPublishResult")
        }
        Some(ServerPayload::Heartbeat(hb)) => {
            assert_eq!(hb.timestamp_mono_us, 77777, "expected heartbeat echo");
        }
        other => panic!("Expected Heartbeat echo, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_missing_capability_rejected() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) =
        handshake_with_capabilities(&mut client, "asset-no-cap", "test-key", &[]).await;

    let payload = b"<svg xmlns='http://www.w3.org/2000/svg'></svg>".to_vec();
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&payload).as_bytes().to_vec(),
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let msg = next_server_msg(&mut stream).await;
    match &msg.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_CAPABILITY_MISSING");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_metadata_preflight_dedup_hit() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "asset-dedup",
        "test-key",
        &["register_widget_asset"],
    )
    .await;

    let payload =
        b"<svg xmlns='http://www.w3.org/2000/svg'><rect width='1' height='1'/></svg>".to_vec();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: hash.clone(),
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let first = next_server_msg(&mut stream).await;
    match &first.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(!result.was_deduplicated);
        }
        other => panic!("expected WidgetAssetRegisterResult on first upload, got: {other:?}"),
    }

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: hash,
            transport_crc32c: 0,
            total_size_bytes: 0,
            inline_svg_bytes: Vec::new(),
            metadata_only_preflight: true,
        })),
    })
    .await
    .unwrap();

    let second = next_server_msg(&mut stream).await;
    match &second.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(result.was_deduplicated);
        }
        other => panic!("expected WidgetAssetRegisterResult on preflight, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_durable_store_dedups_after_restart() {
    let temp = tempfile::tempdir().expect("tempdir should be creatable");
    let store_path = temp.path().join("runtime-widget-store");
    let payload =
        b"<svg xmlns='http://www.w3.org/2000/svg'><rect width='3' height='2'/></svg>".to_vec();
    let hash = blake3::hash(&payload).as_bytes().to_vec();

    // First runtime instance writes the asset durably.
    let (mut client_a, handle_a) =
        setup_widget_test_with_durable_store(store_path.clone(), 0, 0).await;
    let (tx_a, _init_msgs_a, mut stream_a) = handshake_with_capabilities(
        &mut client_a,
        "asset-durable-a",
        "test-key",
        &["register_widget_asset"],
    )
    .await;
    tx_a.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: hash.clone(),
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();
    let first = next_server_msg(&mut stream_a).await;
    match &first.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(!result.was_deduplicated);
        }
        other => panic!("expected WidgetAssetRegisterResult on first upload, got: {other:?}"),
    }

    // New runtime instance should preflight-dedup from the same durable store.
    let (mut client_b, handle_b) = setup_widget_test_with_durable_store(store_path, 0, 0).await;
    let (tx_b, _init_msgs_b, mut stream_b) = handshake_with_capabilities(
        &mut client_b,
        "asset-durable-b",
        "test-key",
        &["register_widget_asset"],
    )
    .await;
    tx_b.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: hash,
            transport_crc32c: 0,
            total_size_bytes: 0,
            inline_svg_bytes: Vec::new(),
            metadata_only_preflight: true,
        })),
    })
    .await
    .unwrap();
    let second = next_server_msg(&mut stream_b).await;
    match &second.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(result.was_deduplicated);
        }
        other => {
            panic!("expected WidgetAssetRegisterResult on restart preflight, got: {other:?}")
        }
    }

    drop(handle_a);
    drop(handle_b);
}

#[tokio::test]
async fn test_widget_asset_register_unknown_hash_requires_payload_and_hash_validation() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "asset-require-payload",
        "test-key",
        &["register_widget_asset"],
    )
    .await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: vec![0x11; 32],
            transport_crc32c: 0,
            total_size_bytes: 0,
            inline_svg_bytes: Vec::new(),
            metadata_only_preflight: true,
        })),
    })
    .await
    .unwrap();

    let missing_payload = next_server_msg(&mut stream).await;
    match &missing_payload.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_HASH_MISMATCH");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    let payload = b"<svg xmlns='http://www.w3.org/2000/svg'></svg>".to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: vec![0xAA; 32], // wrong on purpose
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let hash_mismatch = next_server_msg(&mut stream).await;
    match &hash_mismatch.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_HASH_MISMATCH");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    let valid_payload = b"<svg xmlns='http://www.w3.org/2000/svg'><circle r='2'/></svg>".to_vec();
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&valid_payload).as_bytes().to_vec(),
            transport_crc32c: 0,
            total_size_bytes: valid_payload.len() as u64,
            inline_svg_bytes: valid_payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let uploaded = next_server_msg(&mut stream).await;
    match &uploaded.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(!result.was_deduplicated);
            assert!(result.error_code.is_empty());
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_checksum_svg_and_type_validation() {
    let (mut client, handle) = setup_widget_test().await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "asset-validation",
        "test-key",
        &["register_widget_asset"],
    )
    .await;

    // Invalid type id (must be kebab-case).
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "Gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: vec![0x44; 32],
            transport_crc32c: 0,
            total_size_bytes: 0,
            inline_svg_bytes: Vec::new(),
            metadata_only_preflight: true,
        })),
    })
    .await
    .unwrap();
    let invalid_type = next_server_msg(&mut stream).await;
    match &invalid_type.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_TYPE_INVALID");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    // Bad checksum.
    let crc_payload = b"<svg xmlns='http://www.w3.org/2000/svg'></svg>".to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&crc_payload).as_bytes().to_vec(),
            transport_crc32c: 1, // wrong on purpose
            total_size_bytes: crc_payload.len() as u64,
            inline_svg_bytes: crc_payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();
    let checksum_mismatch = next_server_msg(&mut stream).await;
    match &checksum_mismatch.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_CHECKSUM_MISMATCH");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    // Invalid SVG payload.
    let invalid_svg_payload = b"not-svg".to_vec();
    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&invalid_svg_payload).as_bytes().to_vec(),
            transport_crc32c: 0,
            total_size_bytes: invalid_svg_payload.len() as u64,
            inline_svg_bytes: invalid_svg_payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();
    let invalid_svg = next_server_msg(&mut stream).await;
    match &invalid_svg.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_INVALID_SVG");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_budget_exceeded_rejected() {
    let (mut client, handle) = setup_widget_test_with_asset_limits(24, 24).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "asset-budget",
        "test-key",
        &["register_widget_asset"],
    )
    .await;

    let payload =
        b"<svg xmlns='http://www.w3.org/2000/svg'><rect width='10' height='10'/></svg>".to_vec();
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&payload).as_bytes().to_vec(),
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload,
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let budget_denied = next_server_msg(&mut stream).await;
    match &budget_denied.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(!result.accepted);
            assert_eq!(result.error_code, "WIDGET_ASSET_BUDGET_EXCEEDED");
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    }

    drop(handle);
}

#[tokio::test]
async fn test_widget_asset_register_updates_runtime_widget_lifecycle_for_publish_path() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let wakes = Arc::new(AtomicU64::new(0));
    let callback_wakes = Arc::clone(&wakes);
    let notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
        callback_wakes.fetch_add(1, Ordering::AcqRel);
    });
    let mut service = setup_widget_service().await;
    service.render_wake = notifier;
    let shared_state = service.state.clone();
    let (mut client, handle) = setup_widget_test_with_service(service).await;
    let (tx, _init_msgs, mut stream) = handshake_with_capabilities(
        &mut client,
        "asset-lifecycle",
        "test-key",
        &["register_widget_asset", "publish_widget:gauge"],
    )
    .await;

    let payload =
        b"<svg xmlns='http://www.w3.org/2000/svg'><rect id='bar' width='1' height='1'/></svg>"
            .to_vec();
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: blake3::hash(&payload).as_bytes().to_vec(),
            transport_crc32c: 0,
            total_size_bytes: payload.len() as u64,
            inline_svg_bytes: payload.clone(),
            metadata_only_preflight: false,
        })),
    })
    .await
    .unwrap();

    let asset_handle = match next_server_msg(&mut stream).await.payload {
        Some(ServerPayload::WidgetAssetRegisterResult(result)) => {
            assert!(result.accepted);
            assert!(!result.was_deduplicated);
            result.asset_handle
        }
        other => panic!("expected WidgetAssetRegisterResult, got: {other:?}"),
    };
    assert_eq!(wakes.load(Ordering::Acquire), 1);

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::WidgetAssetRegister(WidgetAssetRegister {
            widget_type_id: "gauge".to_string(),
            svg_filename: "fill.svg".to_string(),
            content_hash_blake3: vec![0x77; 32],
            transport_crc32c: 0,
            total_size_bytes: 0,
            inline_svg_bytes: Vec::new(),
            metadata_only_preflight: true,
        })),
    })
    .await
    .unwrap();
    let no_enqueue = next_server_msg(&mut stream).await;
    assert!(matches!(
        no_enqueue.payload,
        Some(ServerPayload::WidgetAssetRegisterResult(
            WidgetAssetRegisterResult {
                accepted: false,
                ..
            }
        ))
    ));
    assert_eq!(wakes.load(Ordering::Acquire), 1);

    tx.send(ClientMessage {
        sequence: 4,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![crate::proto::WidgetParameterValueProto {
                param_name: "level".to_string(),
                value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                    0.42,
                )),
            }],
            transition_ms: 0,
            key: String::new(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();

    let publish_msg = next_server_msg(&mut stream).await;
    match &publish_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(result.ok, "publish should remain usable after registration");
        }
        other => panic!("expected WidgetPublishResult, got: {other:?}"),
    }

    {
        let st = shared_state.lock().await;
        let mut scene = st.scene.lock().await;
        assert_eq!(
            scene
                .widget_registry
                .runtime_svg_handle("gauge", "fill.svg"),
            Some(asset_handle.as_str())
        );

        let queued = scene.drain_pending_widget_svg_assets();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "gauge");
        assert_eq!(queued[0].1, "fill.svg");
        assert_eq!(queued[0].2, payload);
    }

    drop(handle);
}
