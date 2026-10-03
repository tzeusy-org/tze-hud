use super::*;

#[test]
fn test_scene_node_contains_handles_deep_hierarchy_iteratively() {
    let mut scene = SceneGraph::new(800.0, 600.0);
    let root_id = tze_hud_scene::SceneId::new();
    scene.nodes.insert(
        root_id,
        tze_hud_scene::Node {
            layout: Default::default(),
            id: root_id,
            data: tze_hud_scene::NodeData::SolidColor(tze_hud_scene::SolidColorNode {
                bounds: tze_hud_scene::Rect::new(0.0, 0.0, 1.0, 1.0),
                color: tze_hud_scene::Rgba::WHITE,
                radius: None,
            }),
            children: Vec::new(),
        },
    );

    let mut parent_id = root_id;
    for _ in 0..2_048 {
        let child_id = tze_hud_scene::SceneId::new();
        scene.nodes.insert(
            child_id,
            tze_hud_scene::Node {
                layout: Default::default(),
                id: child_id,
                data: tze_hud_scene::NodeData::SolidColor(tze_hud_scene::SolidColorNode {
                    bounds: tze_hud_scene::Rect::new(0.0, 0.0, 1.0, 1.0),
                    color: tze_hud_scene::Rgba::WHITE,
                    radius: None,
                }),
                children: Vec::new(),
            },
        );
        scene
            .nodes
            .get_mut(&parent_id)
            .unwrap()
            .children
            .push(child_id);
        parent_id = child_id;
    }

    assert!(
        scene_node_contains(&scene, root_id, parent_id),
        "deep descendant should be found without recursive traversal"
    );
    assert!(
        !scene_node_contains(&scene, root_id, tze_hud_scene::SceneId::new()),
        "unrelated node should not be reported as contained"
    );
}

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
async fn test_list_elements_request_supports_filters_and_override_metadata() {
    let (mut client, _server, shared_state) = setup_test_with_state().await;
    let tile_id: SceneId;
    let zone_id = SceneId::new();
    let widget_id = SceneId::new();

    {
        let mut st = shared_state.lock().await;
        st.element_store = tze_hud_scene::element_store::ElementStore::default();
        let mut scene = st.scene.lock().await;
        let tab_id = scene.create_tab("main", 0).expect("create tab");

        let bootstrap_lease = scene.grant_lease("agent-list", 60_000);

        tile_id = scene
            .create_tile(
                tab_id,
                "agent-list",
                bootstrap_lease,
                Rect::new(40.0, 30.0, 160.0, 120.0),
                1,
            )
            .expect("create tile");

        scene.zone_registry.register(ZoneDefinition {
            id: zone_id,
            name: "list-zone".to_string(),
            description: "ListElements test zone".to_string(),
            geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 1.0,
                height_pct: 0.1,
            },
            accepted_media_types: vec![ZoneMediaType::StreamText],
            rendering_policy: RenderingPolicy::default(),
            contention_policy: ContentionPolicy::LatestWins,
            max_publishers: 2,
            auto_clear_ms: None,
            ephemeral: false,
            layer_attachment: LayerAttachment::Content,
        });

        scene
            .publish_to_zone_with_lease(
                "list-zone",
                ZoneContent::StreamText("hello".to_string()),
                "agent-list",
                bootstrap_lease,
                None,
                None,
            )
            .expect("publish zone");

        scene.widget_registry.register_definition(WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "Gauge widget".to_string(),
            parameter_schema: vec![WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            }],
            layers: vec![],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.2,
                y_pct: 0.2,
                width_pct: 0.2,
                height_pct: 0.1,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false,
            hover_behavior: None,
        });
        scene.widget_registry.register_instance(WidgetInstance {
            id: widget_id,
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "gauge-main".to_string(),
            current_params: HashMap::new(),
        });
        scene
            .publish_to_widget(
                "gauge-main",
                HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.5))]),
                "agent-list",
                None,
                0,
                None,
            )
            .expect("publish widget");
        drop(scene);

        st.element_store.entries.insert(
            tile_id,
            tze_hud_scene::element_store::ElementStoreEntry {
                element_type: tze_hud_scene::element_store::ElementType::Tile,
                namespace: "agent-list".to_string(),
                created_at: 101,
                last_published_at: 202,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: Some(GeometryPolicy::Relative {
                    x_pct: 0.25,
                    y_pct: 0.1,
                    width_pct: 0.2,
                    height_pct: 0.2,
                }),
            },
        );
        st.element_store.entries.insert(
            zone_id,
            tze_hud_scene::element_store::ElementStoreEntry {
                element_type: tze_hud_scene::element_store::ElementType::Zone,
                namespace: "list-zone".to_string(),
                created_at: 303,
                last_published_at: 404,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: None,
            },
        );
        st.element_store.entries.insert(
            widget_id,
            tze_hud_scene::element_store::ElementStoreEntry {
                element_type: tze_hud_scene::element_store::ElementType::Widget,
                namespace: "gauge-main".to_string(),
                created_at: 505,
                last_published_at: 606,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: None,
            },
        );
    }

    let (tx, _init_messages, mut stream) = handshake(&mut client, "agent-list", "test-key").await;

    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ListElementsRequest(
            crate::proto::ListElementsRequest {
                namespace_filter: Some("agent-".to_string()),
                element_type: Some("tile".to_string()),
            },
        )),
    })
    .await
    .expect("send list-elements request");

    let tile_only = next_server_msg(&mut stream).await;
    match tile_only.payload {
        Some(ServerPayload::ListElementsResponse(resp)) => {
            assert_eq!(
                resp.elements.len(),
                1,
                "tile filter should return one element"
            );
            let entry = &resp.elements[0];
            assert_eq!(entry.element_type, "tile");
            assert_eq!(entry.namespace, "agent-list");
            assert_eq!(
                bytes_to_scene_id(&entry.element_id).expect("tile entry id must decode"),
                tile_id
            );
            assert!(entry.has_user_override, "tile should report user override");
            assert_eq!(entry.created_at_ms, 101);
            assert_eq!(entry.last_published_at_ms, 202);
            match entry
                .current_geometry
                .as_ref()
                .and_then(|g| g.policy.as_ref())
            {
                Some(crate::proto::geometry_policy_proto::Policy::Relative(relative)) => {
                    assert!((relative.x_pct - 0.25).abs() < 1e-6);
                    assert!((relative.y_pct - 0.1).abs() < 1e-6);
                    assert!((relative.width_pct - 0.2).abs() < 1e-6);
                    assert!((relative.height_pct - 0.2).abs() < 1e-6);
                }
                other => panic!("expected relative geometry policy, got {other:?}"),
            }
        }
        other => panic!("Expected ListElementsResponse, got: {other:?}"),
    }

    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::ListElementsRequest(
            crate::proto::ListElementsRequest {
                namespace_filter: Some("list-".to_string()),
                element_type: Some("zone".to_string()),
            },
        )),
    })
    .await
    .expect("send list-elements zone filter request");

    let zone_only = next_server_msg(&mut stream).await;
    match zone_only.payload {
        Some(ServerPayload::ListElementsResponse(resp)) => {
            assert_eq!(
                resp.elements.len(),
                1,
                "zone filter should return one element"
            );
            assert_eq!(resp.elements[0].element_type, "zone");
            assert_eq!(resp.elements[0].namespace, "list-zone");
            assert_eq!(
                bytes_to_scene_id(&resp.elements[0].element_id).expect("zone entry id must decode"),
                zone_id
            );
        }
        other => panic!("Expected ListElementsResponse, got: {other:?}"),
    }
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
            lease_id,
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

    // The batch will be rejected (no active tab in setup_test).
    // Regardless of rejection, MutationResult.batch_id MUST equal client_batch_id.
    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(result)) => {
            assert_eq!(
                result.batch_id, client_batch_id,
                "MutationResult.batch_id must echo the client-provided batch_id \
                     (regression for hud-wu32: batch_id was previously a fresh SceneId)"
            );
        }
        other => panic!("Expected MutationResult, got: {other:?}"),
    }
}

// ─── Deduplication tests (RFC 0005 §5.2) ─────────────────────────────────

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
