use super::*;

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
            assert_eq!(result.seq, 2);
            assert!(result.hint.contains("nonexistent"), "{result:?}");
            assert!(
                result.hint.contains("Use a registered widget name"),
                "{result:?}"
            );
            assert!(result.hint.contains("resend Publish"), "{result:?}");
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
            assert_eq!(result.seq, 2);
            assert!(result.hint.contains("bogus_param"), "{result:?}");
            assert!(result.hint.contains("widget definition"), "{result:?}");
            assert!(result.hint.contains("resend Publish"), "{result:?}");
        }
        other => {
            panic!("Expected WidgetPublishResult(WIDGET_UNKNOWN_PARAMETER), got: {other:?}")
        }
    }

    // Follow the correction on the same outbound channel and permission set.
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Publish(Publish {
            surface: "widget:gauge".to_string(),
            params: vec![crate::proto::WidgetParameterValueProto {
                param_name: "value".to_string(),
                value: Some(crate::proto::widget_parameter_value_proto::Value::F32Value(
                    0.5,
                )),
            }],
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    match next_server_msg(&mut stream).await.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert!(result.ok, "{result:?}");
            assert_eq!(result.seq, 3);
            assert!(result.code.is_empty());
            assert!(result.hint.is_empty());
        }
        other => panic!("Expected corrected widget Publish result, got: {other:?}"),
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
