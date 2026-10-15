use super::*;

/// Scenario: Durable WidgetPublish with valid params receives WidgetPublishResult(accepted=true).
#[tokio::test]
async fn test_durable_widget_publish_receives_result() {
    let service = setup_widget_service().await;
    let scene_ref = service.state.lock().await.scene.clone();
    {
        let mut scene = scene_ref.lock().await;
        let def = scene.widget_registry.definitions.get_mut("gauge").unwrap();
        def.parameter_schema[0].constraints = Some(tze_hud_scene::WidgetParamConstraints {
            f32_min: Some(0.0),
            f32_max: Some(1.0),
            ..Default::default()
        });
        def.parameter_schema
            .push(tze_hud_scene::WidgetParameterDeclaration {
                name: "color".into(),
                param_type: tze_hud_scene::WidgetParamType::Color,
                default_value: tze_hud_scene::WidgetParameterValue::Color(
                    tze_hud_scene::Rgba::WHITE,
                ),
                constraints: None,
            });
    }
    let (mut client, _handle) = setup_widget_test_with_service(service).await;

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

    use crate::proto::widget_parameter_value_proto::Value as Param;
    let before = {
        let scene = scene_ref.lock().await;
        (
            scene.version,
            scene.widget_registry.instances["gauge"]
                .current_params
                .clone(),
        )
    };
    let mut bad = vec![
        ("level", Param::F32Value(-0.1), "0..1"),
        ("level", Param::F32Value(1.1), "0..1"),
        ("level", Param::F32Value(f32::NAN), "finite"),
        ("level", Param::F32Value(f32::INFINITY), "finite"),
    ];
    for channel in 0..4 {
        for value in [-0.1, 1.1, f32::NAN, f32::INFINITY] {
            let mut c = [0.5; 4];
            c[channel] = value;
            bad.push((
                "color",
                Param::ColorValue(crate::proto::Rgba {
                    r: c[0],
                    g: c[1],
                    b: c[2],
                    a: c[3],
                }),
                "0..1",
            ));
        }
    }
    for (index, (param, value, hint)) in bad.into_iter().enumerate() {
        let sequence = 3 + index as u64;
        let mut params = vec![crate::proto::WidgetParameterValueProto {
            param_name: param.into(),
            value: Some(value),
        }];
        params.push(if param == "level" {
            crate::proto::WidgetParameterValueProto {
                param_name: "color".into(),
                value: Some(Param::ColorValue(crate::proto::Rgba {
                    r: 0.2,
                    g: 0.3,
                    b: 0.4,
                    a: 1.0,
                })),
            }
        } else {
            crate::proto::WidgetParameterValueProto {
                param_name: "level".into(),
                value: Some(Param::F32Value(0.5)),
            }
        });
        tx.send(ClientMessage {
            sequence,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::Publish(Publish {
                surface: "widget:gauge".into(),
                transition_ms: 100,
                params,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let message = next_server_msg(&mut stream).await;
        let Some(ServerPayload::RequestResult(result)) = message.payload else {
            panic!("request result")
        };
        assert!(!result.ok);
        assert_eq!(result.seq, sequence);
        assert_eq!(result.code, "WIDGET_PARAMETER_INVALID");
        assert!(
            result.hint.contains(param) && result.hint.contains(hint),
            "{}",
            result.hint
        );
        let scene = scene_ref.lock().await;
        assert_eq!(scene.version, before.0);
        assert_eq!(
            scene.widget_registry.instances["gauge"].current_params,
            before.1
        );
        let pubs = &scene.widget_registry.active_publishes["gauge"];
        assert_eq!(pubs.len(), 1);
        assert_eq!(pubs[0].transition_ms, 0);
        assert_eq!(
            pubs[0].params["level"],
            tze_hud_scene::WidgetParameterValue::F32(0.75)
        );
    }
    for (sequence, level) in [(30, 0.0), (31, 1.0)] {
        tx.send(ClientMessage {
            sequence,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::Publish(Publish {
                surface: "widget:gauge".into(),
                params: vec![
                    crate::proto::WidgetParameterValueProto {
                        param_name: "level".into(),
                        value: Some(Param::F32Value(level)),
                    },
                    crate::proto::WidgetParameterValueProto {
                        param_name: "color".into(),
                        value: Some(Param::ColorValue(crate::proto::Rgba {
                            r: 0.0,
                            g: 1.0,
                            b: 0.5,
                            a: 1.0,
                        })),
                    },
                ],
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let message = next_server_msg(&mut stream).await;
        assert!(matches!(message.payload, Some(ServerPayload::RequestResult(result)) if result.ok));
        assert_eq!(
            scene_ref.lock().await.widget_registry.instances["gauge"].current_params["level"],
            tze_hud_scene::WidgetParameterValue::F32(level)
        );
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
            assert_eq!(result.seq, 2);
            assert!(result.hint.contains("widget:gauge"));
            assert!(result.hint.contains("operator"));
            assert!(result.hint.contains("allow"));
            assert!(result.hint.contains("reconnect"));
            assert!(result.hint.contains("fresh SessionInit"));
            assert!(result.hint.contains("valid SessionResume"));
            assert!(result.hint.contains("retry"));
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
                param_name: "level".to_string(),
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
            auto_clear_ms: None,
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
