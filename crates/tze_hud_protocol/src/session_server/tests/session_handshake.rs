use super::*;

#[tokio::test]
async fn test_initial_handshake_failures_carry_recovery_hints() {
    let service = HudSessionImpl::new(SceneGraph::new(800.0, 600.0), "test-key");
    let agents = tze_hud_scene::config::AgentDirectory::unrestricted("test-key");
    let budget = ResourceBudget::default();
    let ctx = HandshakeCtx {
        state: &service.state,
        agents: &agents,
        resource_budget: &budget,
        budget_enforcer: None,
        peer_ip: None,
    };
    for (read, code, message_fragment) in [
        (
            Ok(None),
            "HANDSHAKE_TIMEOUT",
            "Stream closed before handshake",
        ),
        (
            Err(Status::data_loss("inbound decode failed")),
            "HANDSHAKE_ERROR",
            "Error receiving handshake:",
        ),
        (
            Ok(Some(ClientMessage {
                payload: Some(ClientPayload::Heartbeat(Heartbeat::default())),
                ..Default::default()
            })),
            "INVALID_HANDSHAKE",
            "First message must be SessionInit or SessionResume",
        ),
        (
            Ok(Some(ClientMessage::default())),
            "INVALID_HANDSHAKE",
            "First message must be SessionInit or SessionResume",
        ),
    ] {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        assert!(handle_handshake_read(ctx, &tx, read).await.is_none());
        let message = rx.recv().await.unwrap().unwrap();
        assert_eq!(message.sequence, 1);
        let Some(ServerPayload::SessionError(error)) = message.payload else {
            panic!("expected a terminal SessionError for {code}");
        };
        assert_eq!(error.code, code);
        assert!(error.message.contains(message_fragment));
        assert!(error.hint.contains("new stream"));
        assert!(error.hint.contains("SessionInit"));
        if code == "INVALID_HANDSHAKE" {
            assert!(error.hint.contains("SessionResume"));
        }
        assert!(rx.try_recv().is_err(), "rejection sends only one error");
        assert_eq!(service.state.lock().await.sessions.session_count(), 0);
    }
}

#[tokio::test]
async fn test_session_registration_rejections_carry_recovery_hints() {
    struct RejectRegistration;
    impl MutationBudgetEnforcer for RejectRegistration {
        fn register_session(
            &self,
            _session_id: SceneId,
            _namespace: String,
            _budget: ResourceBudget,
            _resident: bool,
            _initial_usage: MutationBudgetUsage,
        ) -> MutationBudgetDecision {
            MutationBudgetDecision::Reject {
                error_code: "RESOURCE_EXHAUSTED",
                message: "session capacity exhausted".to_string(),
            }
        }

        fn remove_session(&self, _session_id: SceneId) {
            panic!("a rejected session was never registered");
        }

        fn reserve_mutation(
            &self,
            _session_id: SceneId,
            _delta_tiles: i32,
            _delta_texture_bytes: i64,
            _max_nodes_in_batch: u32,
        ) -> MutationBudgetDecision {
            panic!("a rejected session cannot mutate");
        }

        fn rollback_mutation(
            &self,
            _session_id: SceneId,
            _delta_tiles: i32,
            _delta_texture_bytes: i64,
        ) {
            panic!("a rejected session cannot mutate");
        }
    }

    let enforcer: SharedMutationBudgetEnforcer = Arc::new(RejectRegistration);
    let agents = tze_hud_scene::config::AgentDirectory::unrestricted("test-key");
    let budget = ResourceBudget::default();
    for resume in [false, true] {
        let clock = tze_hud_scene::TestClock::new(0);
        let scene = SceneGraph::new_with_clock(800.0, 600.0, Arc::new(clock));
        let service = HudSessionImpl::new(scene, "test-key");
        let token = vec![1; 16];
        let payload = if resume {
            service.state.lock().await.token_store.insert(
                token.clone(),
                "rejected-agent".to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                DEFAULT_GRACE_PERIOD_MS,
                0,
            );
            ClientPayload::SessionResume(SessionResume {
                agent_id: "rejected-agent".to_string(),
                resume_token: token.clone(),
                auth_credential: Some(crate::auth::psk_credential("test-key")),
                ..Default::default()
            })
        } else {
            ClientPayload::SessionInit(SessionInit {
                agent_id: "rejected-agent".to_string(),
                auth_credential: Some(crate::auth::psk_credential("test-key")),
                ..Default::default()
            })
        };
        let ctx = HandshakeCtx {
            state: &service.state,
            agents: &agents,
            resource_budget: &budget,
            budget_enforcer: Some(&enforcer),
            peer_ip: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        assert!(
            handle_handshake_read(
                ctx,
                &tx,
                Ok(Some(ClientMessage {
                    sequence: 1,
                    payload: Some(payload),
                    ..Default::default()
                })),
            )
            .await
            .is_none()
        );
        let message = rx.recv().await.unwrap().unwrap();
        assert_eq!(message.sequence, 1);
        let Some(ServerPayload::SessionError(error)) = message.payload else {
            panic!("expected registration refusal");
        };
        assert_eq!(error.code, "RESOURCE_EXHAUSTED");
        assert_eq!(error.message, "session capacity exhausted");
        assert!(error.hint.contains("capacity"));
        assert!(error.hint.contains("new stream with SessionInit"));
        assert!(rx.try_recv().is_err(), "rejection sends only one error");
        let mut state = service.state.lock().await;
        assert_eq!(state.sessions.session_count(), 0);
        if resume {
            assert!(error.hint.contains("token has been consumed"));
            assert!(matches!(
                state.token_store.consume(&token, "rejected-agent", 0),
                Err(crate::token::ResumeError::TokenNotFound)
            ));
        }
    }
}

#[tokio::test]
async fn test_handshake_init_established_and_snapshot() {
    let (mut client, _server) = setup_test().await;
    let (_tx, messages, _stream) = handshake(&mut client, "test-agent", "test-key").await;

    assert_eq!(messages.len(), 3);

    // First message: SessionEstablished
    match &messages[0].payload {
        Some(ServerPayload::SessionEstablished(established)) => {
            assert!(!established.session_id.is_empty());
            assert_eq!(established.namespace, "test-agent");
            assert!(!established.resume_token.is_empty());
            assert_eq!(
                established.heartbeat_interval_ms,
                DEFAULT_HEARTBEAT_INTERVAL_MS
            );
            // SCENE_TOPOLOGY is granted because agent has read_scene_topology capability
            assert!(
                established
                    .active_subscriptions
                    .contains(&"SCENE_TOPOLOGY".to_string()),
                "SCENE_TOPOLOGY should be active (agent has read_scene_topology)"
            );
            // Mandatory subscriptions always present
            assert!(
                established
                    .active_subscriptions
                    .contains(&"DEGRADATION_NOTICES".to_string()),
                "DEGRADATION_NOTICES must always be active"
            );
            // denied_subscriptions must be empty (all requested categories granted)
            assert!(
                established.denied_subscriptions.is_empty(),
                "no subscriptions should be denied"
            );
        }
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    }

    // Second message: SceneSnapshot
    match &messages[1].payload {
        Some(ServerPayload::SceneSnapshot(snapshot)) => {
            assert!(!snapshot.snapshot_json.is_empty());
        }
        other => panic!("Expected SceneSnapshot, got: {other:?}"),
    }

    match &messages[2].payload {
        Some(ServerPayload::DegradationNotice(notice)) => {
            assert_eq!(notice.level, DegradationLevel::Normal as i32);
        }
        other => panic!("Expected current DegradationNotice, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_handshake_auth_failure() {
    let (mut client, _server) = setup_test().await;

    let (_tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let (init_tx, init_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(init_rx);

    // Send SessionInit with wrong key
    init_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionInit(SessionInit {
                agent_id: "bad-agent".to_string(),
                initial_subscriptions: Vec::new(),
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(crate::auth::psk_credential("wrong-key".to_string())),
            })),
        })
        .await
        .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();

    match &msg.payload {
        Some(ServerPayload::SessionError(error)) => {
            assert_eq!(error.code, "AUTH_FAILED");
        }
        other => panic!("Expected SessionError, got: {other:?}"),
    }

    drop(_tx);
    drop(rx);
}

#[tokio::test]
async fn test_heartbeat_echo() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "heartbeater", "test-key").await;

    let mono_us = 12345678u64;
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: mono_us,
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::Heartbeat(hb)) => {
            assert_eq!(hb.timestamp_mono_us, mono_us);
        }
        other => panic!("Expected Heartbeat echo, got: {other:?}"),
    }
}

// ─── Sequence validation ─────────────────────────────────────────────────────

/// Scenario: Sequence gap exceeds threshold
/// WHEN client sends sequence 5 followed by 150 (gap > max_sequence_gap=100),
/// THEN runtime closes the stream with SEQUENCE_GAP_EXCEEDED.
#[tokio::test]
async fn test_sequence_gap_exceeded() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "seq-gap-agent", "test-key").await;

    // Handshake consumes sequence 1. Send a valid message at sequence 2.
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 100,
        })),
    })
    .await
    .unwrap();

    // Drain the heartbeat echo
    let _ = stream.next().await;

    // Now jump to sequence 5, then to 150 — gap of 145 > DEFAULT_MAX_SEQUENCE_GAP=100
    tx.send(ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 200,
        })),
    })
    .await
    .unwrap();
    let _ = stream.next().await; // drain heartbeat echo

    tx.send(ClientMessage {
        sequence: 150,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 300,
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "SEQUENCE_GAP_EXCEEDED",
                "Expected SEQUENCE_GAP_EXCEEDED, got: {}",
                err.code
            );
            assert!(err.hint.contains("new stream"));
            assert!(err.hint.contains("increasing sequence numbers"));
            assert!(err.hint.contains(&DEFAULT_MAX_SEQUENCE_GAP.to_string()));
        }
        other => panic!("Expected SessionError(SEQUENCE_GAP_EXCEEDED), got: {other:?}"),
    }
    assert!(
        stream.next().await.is_none(),
        "sequence error closes the stream"
    );
}

/// Scenario: Sequence regression rejected
/// WHEN client sends sequence 10 followed by sequence 8,
/// THEN runtime closes the stream with SEQUENCE_REGRESSION.
#[tokio::test]
async fn test_sequence_regression() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "seq-reg-agent", "test-key").await;

    // Send sequence 10
    tx.send(ClientMessage {
        sequence: 10,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 100,
        })),
    })
    .await
    .unwrap();
    let _ = stream.next().await; // drain heartbeat echo

    // Send sequence 8 — regression
    tx.send(ClientMessage {
        sequence: 8,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::Heartbeat(Heartbeat {
            timestamp_mono_us: 200,
        })),
    })
    .await
    .unwrap();

    let msg = stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "SEQUENCE_REGRESSION",
                "Expected SEQUENCE_REGRESSION, got: {}",
                err.code
            );
            assert!(err.hint.contains("new stream"));
            assert!(err.hint.contains("increasing sequence numbers"));
            assert!(err.hint.contains("starting at 2"));
        }
        other => panic!("Expected SessionError(SEQUENCE_REGRESSION), got: {other:?}"),
    }
    assert!(
        stream.next().await.is_none(),
        "sequence error closes the stream"
    );
}

/// Scenario: Monotonically increasing sequence numbers accepted.
/// WHEN agent sends sequences 1, 2, 3,
/// THEN all are processed without error.
#[tokio::test]
async fn test_sequence_monotonic_accepted() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "seq-ok-agent", "test-key").await;

    for seq in 2u64..=4 {
        tx.send(ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::Heartbeat(Heartbeat {
                timestamp_mono_us: seq * 1000,
            })),
        })
        .await
        .unwrap();

        let msg = stream.next().await.unwrap().unwrap();
        match &msg.payload {
            Some(ServerPayload::Heartbeat(hb)) => {
                assert_eq!(hb.timestamp_mono_us, seq * 1000);
            }
            other => panic!("Expected Heartbeat echo at seq {seq}, got: {other:?}"),
        }
    }
}

/// Scenario: Graceful disconnect via SessionClose.
/// The session stream should terminate cleanly after SessionClose is sent.
#[tokio::test]
async fn test_graceful_disconnect_session_close() {
    // One observation seam for both the real stream and the watchdog
    // counterfactual: only None establishes stream termination.
    async fn observe_stream_end(
        stream: &mut (impl tokio_stream::Stream<Item = Result<ServerMessage, Status>> + Unpin),
        watchdog: Duration,
    ) -> bool {
        tokio::time::timeout(watchdog, async {
            while let Some(message) = stream.next().await {
                message.expect("session stream failed instead of ending cleanly");
            }
        })
        .await
        .is_ok()
    }

    let mut pending = tokio_stream::pending::<Result<ServerMessage, Status>>();
    assert!(
        !observe_stream_end(&mut pending, Duration::ZERO).await,
        "watchdog timeout must not count as stream termination"
    );

    let (mut client, _server, state) = setup_test_with_state().await;
    let (tx, init_messages, mut stream) = handshake(&mut client, "close-agent", "test-key").await;
    let session_id = match &init_messages[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => bytes_to_scene_id(&e.session_id).unwrap(),
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    };
    let cleanup = state.lock().await.sessions.observe_cleanup(&session_id);

    // Register before the real close request; neither lock spans the await.
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionClose(SessionClose {
            reason: "test shutdown".to_string(),
        })),
    })
    .await
    .unwrap();

    drop(tx);
    let got_stream_end = observe_stream_end(&mut stream, Duration::from_millis(500)).await;
    assert!(
        got_stream_end,
        "session stream did not terminate after SessionClose — graceful disconnect had no observable effect"
    );
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .expect("real session cleanup did not complete")
        .expect("cleanup witness dropped before normal cleanup completed");
}

// ─── Handshake auth, version, capability, subscription ───────────────────────

/// Scenario: Structured AuthCredential (PSK) accepted
/// WHEN agent sends SessionInit with a valid PreSharedKeyCredential in auth_credential,
/// THEN runtime authenticates and proceeds to SessionEstablished.
#[tokio::test]
async fn test_auth_structured_psk_credential_accepted() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "psk-agent".to_string(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::proto::session::AuthCredential {
                credential: Some(
                    crate::proto::session::auth_credential::Credential::PreSharedKey(
                        crate::proto::session::PreSharedKeyCredential {
                            key: "test-key".to_string(),
                        },
                    ),
                ),
            }),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionEstablished(_)) => {}
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    }
}

/// Scenario: Invalid structured PSK credential rejected with AUTH_FAILED
/// WHEN agent sends SessionInit with a wrong PreSharedKeyCredential,
/// THEN runtime sends SessionError(AUTH_FAILED) and closes stream.
#[tokio::test]
async fn test_auth_structured_psk_credential_wrong_key() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "bad-psk-agent".to_string(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::proto::session::AuthCredential {
                credential: Some(
                    crate::proto::session::auth_credential::Credential::PreSharedKey(
                        crate::proto::session::PreSharedKeyCredential {
                            key: "wrong-key".to_string(),
                        },
                    ),
                ),
            }),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(err.code, "AUTH_FAILED");
        }
        other => panic!("Expected SessionError(AUTH_FAILED), got: {other:?}"),
    }
}

/// Scenario: LocalSocketCredential accepted
/// WHEN agent sends SessionInit with a valid LocalSocketCredential,
/// THEN runtime authenticates and proceeds to SessionEstablished.
#[tokio::test]
async fn test_auth_local_socket_credential_accepted() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "local-agent".to_string(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::proto::session::AuthCredential {
                credential: Some(
                    crate::proto::session::auth_credential::Credential::LocalSocket(
                        crate::proto::session::LocalSocketCredential {
                            socket_path: "/run/tze_hud.sock".to_string(),
                            pid_hint: "42".to_string(),
                        },
                    ),
                ),
            }),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionEstablished(_)) => {}
        other => panic!("Expected SessionEstablished with LocalSocket cred, got: {other:?}"),
    }
}

// ── Wire-level LocalSocket non-loopback rejection ──
//
// The gRPC integration tests above always connect from loopback (::1), so
// peer_ip is always loopback there.  These unit tests call handle_session_init
// and handle_session_resume directly — bypassing the TCP transport — so we can
// inject an arbitrary peer_ip and assert AUTH_FAILED on the non-loopback path.

fn local_socket_session_init(agent_id: &str) -> SessionInit {
    SessionInit {
        agent_id: agent_id.to_string(),
        initial_subscriptions: Vec::new(),
        resume_token: Vec::new(),
        min_protocol_version: 1000,
        max_protocol_version: 1001,
        auth_credential: Some(crate::proto::session::AuthCredential {
            credential: Some(
                crate::proto::session::auth_credential::Credential::LocalSocket(
                    crate::proto::session::LocalSocketCredential {
                        socket_path: "/run/tze_hud.sock".to_string(),
                        pid_hint: "42".to_string(),
                    },
                ),
            ),
        }),
    }
}

/// Scenario: LocalSocketCredential + non-loopback peer → wire AUTH_FAILED on init path.
///
/// GIVEN a SessionInit carrying a LocalSocketCredential,
/// WHEN handle_session_init is called with peer_ip = Some(10.0.0.5),
/// THEN the server message channel receives SessionError { code: "AUTH_FAILED" }
///      and handle_session_init returns None (session not established).
///
/// Security regression gate: a future refactor that removes
/// the loopback check would cause this test to fail instead of silently breaking.
#[tokio::test]
async fn test_handle_session_init_local_socket_non_loopback_auth_failed() {
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");
    let state = service.state.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(16);

    let init = local_socket_session_init("non-loopback-agent");
    let non_loopback_ip: std::net::IpAddr = "10.0.0.5".parse().unwrap();

    let agents = tze_hud_scene::config::AgentDirectory::unrestricted("test-key");
    let budget = ResourceBudget::default();
    let ctx = HandshakeCtx {
        state: &state,
        agents: &agents,
        resource_budget: &budget,
        budget_enforcer: None,
        peer_ip: Some(non_loopback_ip),
    };
    let session = handle_session_init(ctx, &tx, &init).await;

    assert!(
        session.is_none(),
        "handle_session_init must return None for non-loopback LocalSocket peer"
    );

    let server_msg = rx
        .recv()
        .await
        .expect("server must send a message on auth failure")
        .expect("message must not be a transport error");

    match server_msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "AUTH_FAILED",
                "non-loopback LocalSocket must produce AUTH_FAILED, got: {}",
                err.code
            );
            assert!(
                err.message.contains("not a loopback address"),
                "error message must mention loopback, got: {}",
                err.message
            );
            assert!(err.hint.contains("PreSharedKeyCredential"));
            assert!(err.hint.contains("paired PSK"));
        }
        other => panic!(
            "Expected SessionError(AUTH_FAILED) for non-loopback LocalSocket init, \
                 got: {other:?}"
        ),
    }
}

/// Scenario: LocalSocketCredential + non-loopback peer → wire AUTH_FAILED on resume path.
///
/// GIVEN a SessionResume carrying a LocalSocketCredential,
/// WHEN handle_session_resume is called with peer_ip = Some(10.0.0.5),
/// THEN the server message channel receives SessionError { code: "AUTH_FAILED" }
///      and handle_session_resume returns None (resume rejected before token check).
///
/// Security regression gate for the resume path: the resume path re-
/// authenticates independently; this test pins it.
#[tokio::test]
async fn test_handle_session_resume_local_socket_non_loopback_auth_failed() {
    let scene = SceneGraph::new(800.0, 600.0);
    let service = HudSessionImpl::new(scene, "test-key");
    let state = service.state.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(16);

    // A bogus resume token — auth is checked before the token, so this value is
    // irrelevant; the test asserts AUTH_FAILED fires before SESSION_GRACE_EXPIRED.
    let bogus_token = vec![0u8; 16];

    let resume = SessionResume {
        agent_id: "non-loopback-resume-agent".to_string(),
        resume_token: bogus_token,
        last_seen_server_sequence: 0,
        pre_shared_key: String::new(),
        auth_credential: Some(crate::proto::session::AuthCredential {
            credential: Some(
                crate::proto::session::auth_credential::Credential::LocalSocket(
                    crate::proto::session::LocalSocketCredential {
                        socket_path: "/run/tze_hud.sock".to_string(),
                        pid_hint: "99".to_string(),
                    },
                ),
            ),
        }),
    };

    let non_loopback_ip: std::net::IpAddr = "10.0.0.5".parse().unwrap();

    let agents = tze_hud_scene::config::AgentDirectory::unrestricted("test-key");
    let budget = ResourceBudget::default();
    let ctx = HandshakeCtx {
        state: &state,
        agents: &agents,
        resource_budget: &budget,
        budget_enforcer: None,
        peer_ip: Some(non_loopback_ip),
    };
    let session = handle_session_resume(ctx, &tx, &resume).await;

    assert!(
        session.is_none(),
        "handle_session_resume must return None for non-loopback LocalSocket peer"
    );

    let server_msg = rx
        .recv()
        .await
        .expect("server must send a message on auth failure")
        .expect("message must not be a transport error");

    match server_msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "AUTH_FAILED",
                "non-loopback LocalSocket resume must produce AUTH_FAILED, got: {}",
                err.code
            );
            assert!(err.hint.contains("PreSharedKeyCredential"));
            assert!(err.hint.contains("paired PSK"));
        }
        other => panic!(
            "Expected SessionError(AUTH_FAILED) for non-loopback LocalSocket resume, \
                 got: {other:?}"
        ),
    }
}

/// Scenario: Version negotiated successfully
/// WHEN agent declares min=1000, max=1001 and runtime supports 1000-1001,
/// THEN SessionEstablished contains negotiated_protocol_version=1001.
#[tokio::test]
async fn test_version_negotiation_success() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "version-agent".to_string(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionEstablished(established)) => {
            assert_eq!(
                established.negotiated_protocol_version, 1001,
                "Should pick highest mutual version (1001)"
            );
        }
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    }
}

/// Scenario: Version negotiation failure — no mutual version
/// WHEN agent declares min=2000, max=2001 and runtime only supports 1000-1001,
/// THEN runtime sends SessionError(code=UNSUPPORTED_PROTOCOL_VERSION) and closes stream.
#[tokio::test]
async fn test_version_negotiation_unsupported() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "old-agent".to_string(),
            initial_subscriptions: Vec::new(),
            resume_token: Vec::new(),
            min_protocol_version: 2000,
            max_protocol_version: 2001,
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionError(err)) => {
            assert_eq!(
                err.code, "UNSUPPORTED_PROTOCOL_VERSION",
                "Expected UNSUPPORTED_PROTOCOL_VERSION, got: {}",
                err.code
            );
            // Hint should include runtime's supported range
            assert!(
                !err.hint.is_empty(),
                "Hint should contain runtime version range"
            );
        }
        other => panic!("Expected SessionError(UNSUPPORTED_PROTOCOL_VERSION), got: {other:?}"),
    }
}

#[test]
fn test_capability_set_covers_wildcard_grants() {
    let caps = vec!["publish_zone:*".to_string(), "publish_widget:*".to_string()];
    assert!(capability_set_covers(&caps, "publish_zone:subtitle"));
    assert!(capability_set_covers(&caps, "publish_widget:gauge"));
    assert!(!capability_set_covers(&caps, "create_tiles"));
}

/// Scenario: PSK agent with access_input_events capability successfully subscribes to
/// INPUT_EVENTS.
/// WHEN a PSK-authenticated agent requests INPUT_EVENTS subscription AND includes
/// access_input_events in requested_capabilities,
/// THEN SessionEstablished includes INPUT_EVENTS in active_subscriptions and
/// denied_subscriptions is empty.
///
/// Subscription gating uses the agent's explicitly granted capabilities.
/// Agents must request the required capability to subscribe to gated categories.
#[tokio::test]
async fn test_psk_with_capability_allows_input_events_subscription() {
    let (mut client, _server) = setup_test().await;

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    // PSK agent requesting INPUT_EVENTS subscription WITH the required capability
    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "sub-test-agent".to_string(),
            initial_subscriptions: vec!["INPUT_EVENTS".to_string()],
            resume_token: Vec::new(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
        })),
    })
    .await
    .unwrap();

    let mut response_stream = client.session(stream).await.unwrap().into_inner();
    let msg = response_stream.next().await.unwrap().unwrap();
    match &msg.payload {
        Some(ServerPayload::SessionEstablished(established)) => {
            // Agent with access_input_events capability should have INPUT_EVENTS active
            assert!(
                established
                    .active_subscriptions
                    .contains(&"INPUT_EVENTS".to_string()),
                "Agent with access_input_events should have INPUT_EVENTS in active_subscriptions; \
                     active={:?}, denied={:?}",
                established.active_subscriptions,
                established.denied_subscriptions
            );
            assert!(
                established.denied_subscriptions.is_empty(),
                "Agent with required capability should have no denied subscriptions"
            );
        }
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    }
}

/// Removed client requests (subscription change, input focus/capture, widget
/// asset register, element listing) are reserved field numbers: a peer that
/// still sends one decodes to an empty payload and gets a correlated rejection.
#[tokio::test]
async fn removed_client_request_numbers_decode_to_empty_payload() {
    use prost::Message;
    let mut removed = Vec::new();
    for field in [24u64, 27, 28, 29, 34, 39] {
        // tag = field << 3 | length-delimited, then an empty message body.
        let mut wire = vec![0x08, 0x07];
        prost::encoding::encode_varint(field << 3 | 2, &mut wire);
        wire.push(0x00);
        let msg = ClientMessage::decode(wire.as_slice()).expect("unknown field is skipped");
        assert_eq!(msg.sequence, 7);
        assert!(
            msg.payload.is_none(),
            "field {field} must not map to a payload"
        );
        removed.push(msg);
    }

    let (mut client, _server, state) = setup_test_with_state().await;
    let (tx, initial, mut stream) = handshake(&mut client, "empty-payload-agent", "test-key").await;
    let mut server_sequence = initial.last().unwrap().sequence;
    let (scene_before, scheduled_before, leases_before) = {
        let state = state.lock().await;
        let scene = state.scene.lock().await;
        (
            serde_json::to_value(&*scene).unwrap(),
            scene.scheduled_batches.len(),
            state
                .sessions
                .session_for_namespace("empty-payload-agent")
                .unwrap()
                .lease_ids
                .clone(),
        )
    };

    let mut future_wire = vec![0x08, 0x07];
    prost::encoding::encode_varint(100 << 3 | 2, &mut future_wire);
    future_wire.push(0);
    let future = ClientMessage::decode(future_wire.as_slice()).unwrap();
    assert!(future.payload.is_none());
    let mut rejected = removed;
    rejected.push(ClientMessage::default());
    rejected.push(future);
    let mut client_sequence = 2;
    for mut request in rejected {
        request.sequence = client_sequence;
        tx.send(request).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("empty-payload rejection must not leave callers waiting")
            .expect("the stream remains open")
            .unwrap();
        server_sequence += 1;
        assert_eq!(response.sequence, server_sequence);
        let Some(ServerPayload::RequestResult(result)) = response.payload else {
            panic!("expected a correlated RequestResult");
        };
        assert_eq!(result.seq, client_sequence);
        assert!(!result.ok);
        assert_eq!(result.code, "INVALID_ARGUMENT");
        assert!(result.hint.contains("current session.proto schema"));
        assert!(result.hint.contains("recognized request"));
        client_sequence += 1;
    }

    // Legacy zero skips client sequence validation but is still echoed exactly.
    tx.send(ClientMessage::default()).await.unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("legacy zero gets a rejection")
        .expect("the stream remains open")
        .unwrap();
    server_sequence += 1;
    assert_eq!(response.sequence, server_sequence);
    let Some(ServerPayload::RequestResult(result)) = response.payload else {
        panic!("expected a legacy-zero RequestResult");
    };
    assert_eq!(result.seq, 0);
    assert!(!result.ok);
    assert_eq!(result.code, "INVALID_ARGUMENT");
    assert!(result.hint.contains("current session.proto schema"));

    // Unknown fields do not invalidate a recognized oneof or poison the stream.
    for (mono_us, append_unknown) in [(123u64, true), (456, false)] {
        let heartbeat = ClientMessage {
            sequence: client_sequence,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::Heartbeat(Heartbeat {
                timestamp_mono_us: mono_us,
            })),
        };
        let mut wire = heartbeat.encode_to_vec();
        if append_unknown {
            prost::encoding::encode_varint(100 << 3 | 2, &mut wire);
            wire.push(0);
        }
        let decoded = ClientMessage::decode(wire.as_slice()).unwrap();
        assert_eq!(decoded.payload, heartbeat.payload);
        tx.send(decoded).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("valid Heartbeat succeeds after rejection")
            .expect("the stream remains open")
            .unwrap();
        server_sequence += 1;
        assert_eq!(response.sequence, server_sequence);
        let Some(ServerPayload::Heartbeat(echo)) = response.payload else {
            panic!("expected a normal Heartbeat echo, with no extra rejection");
        };
        assert_eq!(echo.timestamp_mono_us, mono_us);
        client_sequence += 1;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), stream.next())
            .await
            .is_err(),
        "each rejected request produces exactly one result"
    );
    {
        let state = state.lock().await;
        let scene = state.scene.lock().await;
        assert_eq!(serde_json::to_value(&*scene).unwrap(), scene_before);
        assert_eq!(scene.scheduled_batches.len(), scheduled_before);
        assert_eq!(
            state
                .sessions
                .session_for_namespace("empty-payload-agent")
                .unwrap()
                .lease_ids,
            leases_before
        );
    }
}

/// Poll until the registry reports `expected` sessions (bounded, no fixed sleep).
async fn wait_for_session_count(state: &Arc<Mutex<SharedState>>, expected: usize) -> usize {
    for _ in 0..100 {
        let count = state.lock().await.sessions.session_count();
        if count == expected {
            return count;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
    }
    state.lock().await.sessions.session_count()
}

/// A session whose outbound stream is already gone when the handler tries to
/// send the initial DegradationNotice exits early. That exit must still
/// unregister the session (registry entry and its outbound sender), while a
/// healthy session keeps its entry.
#[tokio::test]
async fn degradation_notice_send_failure_unregisters_session() {
    let (mut client, server, state) = setup_test_with_state().await;

    // Healthy control session.
    let (_healthy_tx, _msgs, _healthy_stream) = handshake(&mut client, "healthy", "test-key").await;
    assert_eq!(wait_for_session_count(&state, 1).await, 1);
    assert!(
        state
            .lock()
            .await
            .sessions
            .session_for_namespace("healthy")
            .is_some_and(|s| s.server_message_tx.is_some())
    );

    // Hold the registry lock so the doomed handler parks mid-handshake; drop
    // its client connection meanwhile so every later send fails.
    let blocker = state.lock().await;
    let (doomed_tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(1);
    doomed_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionInit(SessionInit {
                agent_id: "doomed".to_string(),
                initial_subscriptions: Vec::new(),
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(crate::auth::psk_credential("test-key".to_string())),
            })),
        })
        .await
        .unwrap();
    let mut doomed_client = client.clone();
    let doomed_stream = doomed_client
        .session(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    // Let the handler read SessionInit and park on the registry lock, then
    // reset the stream (the response receiver is dropped server-side).
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    drop(doomed_stream);
    drop(doomed_tx);
    drop(doomed_client);
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    drop(blocker);

    // Only the healthy session remains registered.
    assert_eq!(wait_for_session_count(&state, 1).await, 1);
    let st = state.lock().await;
    assert!(st.sessions.session_for_namespace("doomed").is_none());
    assert!(st.sessions.session_for_namespace("healthy").is_some());
    drop(st);
    server.abort();
}
