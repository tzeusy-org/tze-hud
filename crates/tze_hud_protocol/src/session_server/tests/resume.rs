use super::*;

/// Drop the real transport only after its session-specific witness is registered.
async fn disconnect_after_cleanup_registration(
    tx: tokio::sync::mpsc::Sender<ClientMessage>,
    stream: tonic::Streaming<ServerMessage>,
    cleanup: tokio::sync::oneshot::Receiver<()>,
) {
    drop(tx);
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .expect("real session cleanup did not complete")
        .expect("cleanup witness dropped before normal cleanup completed");
}

#[tokio::test]
async fn test_resume_with_token() {
    let (mut client, _server, state) = setup_test_with_state().await;

    // Start initial session to get a resume token and its actual scene identity.
    let (tx, init_messages, stream) = handshake(&mut client, "resumable", "test-key").await;
    let (resume_token, session_id) = match &init_messages[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => (
            e.resume_token.clone(),
            bytes_to_scene_id(&e.session_id).unwrap(),
        ),
        _ => panic!("Expected SessionEstablished"),
    };
    let cleanup = state.lock().await.sessions.observe_cleanup(&session_id);
    disconnect_after_cleanup_registration(tx, stream, cleanup).await;

    // Now resume with the token after observed complete cleanup.

    let (resume_tx, resume_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let resume_stream = tokio_stream::wrappers::ReceiverStream::new(resume_rx);

    resume_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionResume(SessionResume {
                agent_id: "resumable".to_string(),
                resume_token,
                last_seen_server_sequence: 2,
                pre_shared_key: "test-key".to_string(),
                auth_credential: None,
            })),
        })
        .await
        .unwrap();

    let mut response_stream = client.session(resume_stream).await.unwrap().into_inner();

    // Resume ordering is result, coherent snapshot, then current degradation
    // state before any live transition.
    let msg1 = response_stream.next().await.unwrap().unwrap();
    match &msg1.payload {
        Some(ServerPayload::SessionResumeResult(result)) => {
            assert!(result.accepted, "expected resume to be accepted");
            assert!(!result.new_session_token.is_empty());
            // version = major * 1000 + minor; runtime max = v1.1 = 1001
            assert_eq!(
                result.negotiated_protocol_version,
                crate::auth::RUNTIME_MAX_VERSION
            );
        }
        other => panic!("Expected SessionResumeResult on resume, got: {other:?}"),
    }

    let msg2 = response_stream.next().await.unwrap().unwrap();
    match &msg2.payload {
        Some(ServerPayload::SceneSnapshot(_)) => {}
        other => panic!("Expected SceneSnapshot on resume, got: {other:?}"),
    }

    let msg3 = response_stream.next().await.unwrap().unwrap();
    match &msg3.payload {
        Some(ServerPayload::DegradationNotice(notice)) => {
            assert_eq!(notice.level, DegradationLevel::Normal as i32);
        }
        other => panic!("Expected current DegradationNotice on resume, got: {other:?}"),
    }
}

// ─── Reconnect and resume ────────────────────────────────────────────────────

/// Helper: perform a full handshake and return the resume token.
///
/// Drops the sender and response stream, waits for server-side cleanup,
/// then returns the resume token for use in subsequent resume attempts.
async fn handshake_and_disconnect(
    client: &mut HudSessionClient<tonic::transport::Channel>,
    agent_id: &str,
    psk: &str,
    state: &Arc<Mutex<SharedState>>,
) -> Vec<u8> {
    let (tx, init_messages, stream) = handshake(client, agent_id, psk).await;
    let (resume_token, session_id) = match &init_messages[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => (
            e.resume_token.clone(),
            bytes_to_scene_id(&e.session_id).unwrap(),
        ),
        _ => panic!("Expected SessionEstablished"),
    };
    let cleanup = state.lock().await.sessions.observe_cleanup(&session_id);
    disconnect_after_cleanup_registration(tx, stream, cleanup).await;
    resume_token
}

/// Scenario: Reconnect within grace period succeeds with
/// `SessionResumeResult(accepted=true)`.
#[tokio::test]
async fn test_reconnect_within_grace_accepted() {
    let (mut client, _server, state) = setup_test_with_state().await;
    let resume_token =
        handshake_and_disconnect(&mut client, "resume-ok-agent", "test-key", &state).await;

    let (resume_tx, resume_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let resume_stream = tokio_stream::wrappers::ReceiverStream::new(resume_rx);

    resume_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionResume(SessionResume {
                agent_id: "resume-ok-agent".to_string(),
                resume_token: resume_token.clone(),
                last_seen_server_sequence: 2,
                pre_shared_key: "test-key".to_string(),
                auth_credential: None,
            })),
        })
        .await
        .unwrap();

    let mut response_stream = client.session(resume_stream).await.unwrap().into_inner();

    let msg1 = response_stream.next().await.unwrap().unwrap();
    match &msg1.payload {
        Some(ServerPayload::SessionResumeResult(result)) => {
            assert!(result.accepted, "expected resume to be accepted");
            assert!(
                !result.new_session_token.is_empty(),
                "new token must be issued"
            );
            assert_ne!(
                result.new_session_token, resume_token,
                "new token must differ from old token"
            );
            assert_eq!(
                result.negotiated_protocol_version,
                crate::auth::RUNTIME_MAX_VERSION
            );
        }
        other => panic!("Expected SessionResumeResult, got: {other:?}"),
    }

    // Full SceneSnapshot must follow SessionResumeResult.
    let msg2 = response_stream.next().await.unwrap().unwrap();
    match &msg2.payload {
        Some(ServerPayload::SceneSnapshot(_)) => {}
        other => panic!("Expected SceneSnapshot after resume, got: {other:?}"),
    }
}

/// Scenario: New session token is issued on resume; old token
/// is single-use and consumed.
#[tokio::test]
async fn test_resume_token_single_use() {
    let (mut client, _server, state) = setup_test_with_state().await;
    let resume_token =
        handshake_and_disconnect(&mut client, "single-use-agent", "test-key", &state).await;

    // First resume: should succeed and consume the token.
    let (tx1, rx1) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let s1 = tokio_stream::wrappers::ReceiverStream::new(rx1);
    tx1.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionResume(SessionResume {
            agent_id: "single-use-agent".to_string(),
            resume_token: resume_token.clone(),
            last_seen_server_sequence: 2,
            pre_shared_key: "test-key".to_string(),
            auth_credential: None,
        })),
    })
    .await
    .unwrap();

    let mut r1 = client.session(s1).await.unwrap().into_inner();
    let first_resume = r1.next().await.unwrap().unwrap();
    match &first_resume.payload {
        Some(ServerPayload::SessionResumeResult(result)) => {
            assert!(result.accepted, "first resume must succeed");
        }
        other => panic!("Expected SessionResumeResult, got: {other:?}"),
    }
    // Resume responses expose no scene session ID. Resolve the actual registered
    // transport through the test-only identity binding, never a count predicate.
    let cleanup = {
        let mut st = state.lock().await;
        let registry_id = st
            .sessions
            .session_for_namespace("single-use-agent")
            .expect("accepted resume must register its transport")
            .session_id
            .clone();
        st.sessions.observe_registered_cleanup(&registry_id)
    };
    disconnect_after_cleanup_registration(tx1, r1, cleanup).await;

    // Second resume attempt with the same original token: must fail.
    let (tx2, rx2) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let s2 = tokio_stream::wrappers::ReceiverStream::new(rx2);
    tx2.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionResume(SessionResume {
            agent_id: "single-use-agent".to_string(),
            resume_token: resume_token.clone(),
            last_seen_server_sequence: 2,
            pre_shared_key: "test-key".to_string(),
            auth_credential: None,
        })),
    })
    .await
    .unwrap();

    let mut r2 = client.session(s2).await.unwrap().into_inner();
    let second_resume = r2.next().await.unwrap().unwrap();
    match &second_resume.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "SESSION_GRACE_EXPIRED",
                "second use of same token must fail with SESSION_GRACE_EXPIRED, got: {}",
                err.code
            );
        }
        other => panic!("Expected SessionError(SESSION_GRACE_EXPIRED), got: {other:?}"),
    }
}

/// Manual directory changes apply to fresh/resumed authentication, while an
/// established stream keeps its captured permissions until reconnect.
#[tokio::test]
async fn test_resume_auth_required() {
    use tze_hud_scene::config::{AgentDirectory, hash_psk};

    let directory = |key: Option<&str>, reduced: bool| {
        let mut paired = AgentDirectory::default();
        if let Some(key) = key {
            let permission = if reduced {
                "publish_zone:reload-control"
            } else {
                "*"
            };
            paired.insert(
                "auth-check-agent",
                hash_psk(key),
                vec![permission.to_string()],
            );
        }
        paired.insert("control", hash_psk("control-key"), vec!["*".to_string()]);
        paired
    };

    for (change, current_key) in [
        ("deleted", None),
        ("rekeyed", Some("new-key")),
        ("allow-reduced", Some("old-key")),
    ] {
        let mut scene = SceneGraph::new(800.0, 600.0);
        scene.create_tab("main", 0).unwrap();
        scene.zone_registry = ZoneRegistry::with_defaults();
        let mut control_zone = scene.zone_registry.get_by_name("subtitle").unwrap().clone();
        control_zone.id = SceneId::new();
        control_zone.name = "reload-control".to_string();
        scene.zone_registry.register(control_zone);
        let agents = directory(Some("old-key"), false).shared();
        let mut service = HudSessionImpl::new(scene, "unused-dev-key");
        service.agents = Arc::clone(&agents);
        let state = Arc::clone(&service.state);
        let (mut client, server) = setup_widget_test_with_service(service).await;

        let (old_tx, _, mut old_stream) =
            handshake(&mut client, "auth-check-agent", "old-key").await;
        let before = request(
            &old_tx,
            &mut old_stream,
            2,
            ClientPayload::Publish(subtitle_publish("before reload", 0, 0, 0)),
        )
        .await;
        assert!(before.ok, "{change}: {before:?}");
        let resume_token =
            handshake_and_disconnect(&mut client, "auth-check-agent", "old-key", &state).await;

        // Invalid auth must not consume the token, even before any hand edit.
        let (wrong_tx, wrong_rx) = tokio::sync::mpsc::channel(64);
        wrong_tx
            .send(ClientMessage {
                sequence: 1,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ClientPayload::SessionResume(SessionResume {
                    agent_id: "auth-check-agent".to_string(),
                    resume_token: resume_token.clone(),
                    last_seen_server_sequence: 2,
                    auth_credential: Some(crate::auth::psk_credential("wrong-key".to_string())),
                    ..Default::default()
                })),
            })
            .await
            .unwrap();
        let mut wrong_stream = client
            .session(tokio_stream::wrappers::ReceiverStream::new(wrong_rx))
            .await
            .unwrap()
            .into_inner();
        match next_server_msg(&mut wrong_stream).await.payload {
            Some(ServerPayload::SessionError(err)) => assert_eq!(err.code, "AUTH_FAILED"),
            other => panic!("{change}: expected wrong-key AUTH_FAILED, got {other:?}"),
        }

        agents.store(Arc::new(directory(current_key, true)));
        let after = request(
            &old_tx,
            &mut old_stream,
            3,
            ClientPayload::Publish(subtitle_publish("established after reload", 0, 0, 0)),
        )
        .await;
        assert!(
            after.ok,
            "{change}: established permissions changed: {after:?}"
        );

        let mut attempts = vec![(false, "old-key"), (true, "old-key")];
        if current_key == Some("new-key") {
            attempts.extend([(false, "new-key"), (true, "new-key")]);
        }
        for (resume, key) in attempts {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let credential = Some(crate::auth::psk_credential(key.to_string()));
            let payload = if resume {
                ClientPayload::SessionResume(SessionResume {
                    agent_id: "auth-check-agent".to_string(),
                    resume_token: resume_token.clone(),
                    last_seen_server_sequence: 2,
                    auth_credential: credential,
                    ..Default::default()
                })
            } else {
                ClientPayload::SessionInit(SessionInit {
                    agent_id: "auth-check-agent".to_string(),
                    initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
                    min_protocol_version: 1000,
                    max_protocol_version: 1001,
                    auth_credential: credential,
                    ..Default::default()
                })
            };
            tx.send(ClientMessage {
                sequence: 1,
                timestamp_wall_us: now_wall_us(),
                payload: Some(payload),
            })
            .await
            .unwrap();
            let mut stream = client
                .session(tokio_stream::wrappers::ReceiverStream::new(rx))
                .await
                .unwrap()
                .into_inner();
            let first = next_server_msg(&mut stream).await;
            if current_key != Some(key) {
                match first.payload {
                    Some(ServerPayload::SessionError(err)) => assert_eq!(
                        err.code, "AUTH_FAILED",
                        "{change}: resume={resume} stale key"
                    ),
                    other => panic!("{change}: stale key accepted: {other:?}"),
                }
                continue;
            }
            match first.payload {
                Some(ServerPayload::SessionEstablished(established)) if !resume => {
                    assert_eq!(established.namespace, "auth-check-agent");
                    assert!(
                        !established
                            .active_subscriptions
                            .iter()
                            .any(|s| s == "SCENE_TOPOLOGY")
                    );
                }
                Some(ServerPayload::SessionResumeResult(result)) if resume => {
                    assert!(result.accepted, "{change}: {result:?}");
                    assert!(!result.new_session_token.is_empty());
                    assert_ne!(result.new_session_token, resume_token);
                    assert_eq!(
                        result.negotiated_protocol_version,
                        crate::auth::RUNTIME_MAX_VERSION
                    );
                    assert!(
                        result
                            .active_subscriptions
                            .iter()
                            .any(|s| s == "SCENE_TOPOLOGY")
                    );
                }
                other => panic!("{change}: resume={resume} handshake failed: {other:?}"),
            }
            // Drain the existing initial state through its terminal notice;
            // the assertions below exercise the real active session boundary.
            loop {
                if matches!(
                    next_server_msg(&mut stream).await.payload,
                    Some(ServerPayload::DegradationNotice(_))
                ) {
                    break;
                }
            }
            let denied = request(
                &tx,
                &mut stream,
                2,
                ClientPayload::Publish(subtitle_publish("new permissions", 0, 0, 0)),
            )
            .await;
            assert!(!denied.ok, "{change}: resume={resume} retained old allow");
            assert_eq!(denied.code, "NOT_ALLOWED");
            let mut retained = subtitle_publish("retained permission", 0, 0, 0);
            retained.surface = "zone:reload-control".to_string();
            let allowed = request(&tx, &mut stream, 3, ClientPayload::Publish(retained)).await;
            assert!(allowed.ok, "{change}: resume={resume}: {allowed:?}");
        }

        let (control_tx, _, mut control_stream) =
            handshake(&mut client, "control", "control-key").await;
        let unaffected = request(
            &control_tx,
            &mut control_stream,
            2,
            ClientPayload::Publish(subtitle_publish("unaffected agent", 0, 0, 0)),
        )
        .await;
        assert!(unaffected.ok, "{change}: unaffected agent: {unaffected:?}");
        server.abort();
    }
}

/// Scenario: Bogus token (as if runtime restarted and all tokens
/// cleared) is rejected with `SESSION_GRACE_EXPIRED`.
#[tokio::test]
async fn test_bogus_token_rejected_with_grace_expired() {
    let (mut client, _server) = setup_test().await;

    let bogus_token = uuid::Uuid::now_v7().as_bytes().to_vec();

    let (resume_tx, resume_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let resume_stream = tokio_stream::wrappers::ReceiverStream::new(resume_rx);

    resume_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionResume(SessionResume {
                agent_id: "restart-agent".to_string(),
                resume_token: bogus_token,
                last_seen_server_sequence: 0,
                pre_shared_key: "test-key".to_string(),
                auth_credential: None,
            })),
        })
        .await
        .unwrap();

    let mut response_stream = client.session(resume_stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "SESSION_GRACE_EXPIRED",
                "unknown token must produce SESSION_GRACE_EXPIRED, got: {}",
                err.code
            );
            assert!(
                !err.hint.is_empty(),
                "hint should direct client to SessionInit"
            );
        }
        other => panic!("Expected SessionError(SESSION_GRACE_EXPIRED), got: {other:?}"),
    }
}

/// Scenario: SessionResumeResult carries complete subscription state.
/// Agents must use the confirmed subscription state, not assume the pre-disconnect set.
#[tokio::test]
async fn test_resume_result_carries_subscription_state() {
    let (mut client, _server, state) = setup_test_with_state().await;

    // Establish a session that requested a specific subscription.
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "sub-resume-agent".to_string(),
            // Include required capabilities for both subscriptions (canonical names)
            initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string(), "INPUT_EVENTS".to_string()],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let established_msg = response_stream.next().await.unwrap().unwrap();
    let (resume_token, session_id) = match &established_msg.payload {
        Some(ServerPayload::SessionEstablished(e)) => (
            e.resume_token.clone(),
            bytes_to_scene_id(&e.session_id).unwrap(),
        ),
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    };
    let cleanup = state.lock().await.sessions.observe_cleanup(&session_id);
    disconnect_after_cleanup_registration(tx, response_stream, cleanup).await;

    // Now resume.
    let (rtx, rrx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let rstream = tokio_stream::wrappers::ReceiverStream::new(rrx);

    rtx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionResume(SessionResume {
            agent_id: "sub-resume-agent".to_string(),
            resume_token,
            last_seen_server_sequence: 2,
            pre_shared_key: "test-key".to_string(),
            auth_credential: None,
        })),
    })
    .await
    .unwrap();

    let mut rs = client.session(rstream).await.unwrap().into_inner();
    let resume_result_msg = rs.next().await.unwrap().unwrap();
    match &resume_result_msg.payload {
        Some(ServerPayload::SessionResumeResult(result)) => {
            assert!(result.accepted);
            // Capabilities must be restored.
            // Subscriptions must be restored.
            assert!(
                result
                    .active_subscriptions
                    .contains(&"SCENE_TOPOLOGY".to_string()),
                "SCENE_TOPOLOGY subscription must be present in resume result"
            );
            assert!(
                result
                    .active_subscriptions
                    .contains(&"INPUT_EVENTS".to_string()),
                "INPUT_EVENTS subscription must be present in resume result"
            );
        }
        other => panic!("Expected SessionResumeResult, got: {other:?}"),
    }
}
