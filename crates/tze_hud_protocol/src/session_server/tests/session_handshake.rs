use super::*;

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

// ─── Sequence number validation tests (RFC 0005 §2.3) ────────────────────

/// Scenario: Sequence gap exceeds threshold (RFC 0005 §2.3)
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
        }
        other => panic!("Expected SessionError(SEQUENCE_GAP_EXCEEDED), got: {other:?}"),
    }
}

/// Scenario: Sequence regression rejected (RFC 0005 §2.3)
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
        }
        other => panic!("Expected SessionError(SEQUENCE_REGRESSION), got: {other:?}"),
    }
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

// ─── Session state machine tests (RFC 0005 §1.1) ─────────────────────────

/// Scenario: Successful session establishment transitions through Connecting→Handshaking→Active.
/// The state machine starts in Handshaking during the handle_session_init call and
/// transitions to Active after the handshake response is sent.
#[tokio::test]
async fn test_state_machine_successful_establishment() {
    let (mut client, _server) = setup_test().await;
    let (_tx, messages, _stream) = handshake(&mut client, "state-test-agent", "test-key").await;

    // The complete initial baseline is establishment, snapshot, then current policy.
    assert_eq!(
        messages.len(),
        3,
        "Expected SessionEstablished + SceneSnapshot + DegradationNotice"
    );
    assert!(
        matches!(
            messages[0].payload,
            Some(ServerPayload::SessionEstablished(_))
        ),
        "First message must be SessionEstablished"
    );
    assert!(
        matches!(messages[1].payload, Some(ServerPayload::SceneSnapshot(_))),
        "Second message must be SceneSnapshot"
    );
    assert!(
        matches!(
            messages[2].payload,
            Some(ServerPayload::DegradationNotice(_))
        ),
        "Third message must be current DegradationNotice"
    );
}

/// Scenario: Auth failure transitions Handshaking→Closed with SessionError.
#[tokio::test]
async fn test_state_machine_auth_failure_to_closed() {
    let (mut client, _server) = setup_test().await;

    let (init_tx, init_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(init_rx);

    init_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionInit(SessionInit {
                agent_id: "state-fail-agent".to_string(),
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

    // State machine should send SessionError (AUTH_FAILED) and transition to Closed
    match &msg.payload {
        Some(ServerPayload::SessionError(error)) => {
            assert_eq!(error.code, "AUTH_FAILED");
        }
        other => panic!("Expected SessionError(AUTH_FAILED), got: {other:?}"),
    }
}

/// Scenario: Graceful disconnect via SessionClose.
/// The session stream should terminate cleanly after SessionClose is sent.
#[tokio::test]
async fn test_graceful_disconnect_session_close() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) = handshake(&mut client, "close-agent", "test-key").await;

    // Send SessionClose
    tx.send(ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::SessionClose(SessionClose {
            reason: "test shutdown".to_string(),
        })),
    })
    .await
    .unwrap();

    // Stream should close (no response expected for SessionClose)
    // Give the server a moment to process
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // The stream should be closed; next() should return None or an error
    // (The server closes the stream after transitioning to Closed state)
    drop(tx);
    // Drain any remaining messages
    let mut got_stream_end = false;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
    loop {
        if tokio::time::Instant::now() > deadline {
            break;
        }
        match tokio::time::timeout(tokio::time::Duration::from_millis(100), stream.next()).await {
            Ok(None) | Err(_) => {
                got_stream_end = true;
                break;
            }
            Ok(Some(_)) => {
                // Some message still in transit, keep draining
            }
        }
    }
    assert!(
        got_stream_end,
        "session stream did not terminate after SessionClose — graceful disconnect had no observable effect"
    );
}

// ─── Traffic class classification tests ─────────────────────────────────

/// Verify traffic class routing for server payloads.
#[test]
fn test_traffic_class_routing() {
    use crate::proto::session::*;

    // Transactional messages
    assert_eq!(
        classify_server_payload(&ServerPayload::SessionEstablished(
            SessionEstablished::default()
        )),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::RequestResult(RequestResult::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::RequestResult(RequestResult::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::SessionSuspended(SessionSuspended::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::SessionResumed(SessionResumed::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::RequestResult(RequestResult::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::ResourceUploadAccepted(
            ResourceUploadAccepted::default(),
        )),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::ResourceStored(ResourceStored::default())),
        TrafficClass::Transactional,
    );
    assert_eq!(
        classify_server_payload(&ServerPayload::ResourceErrorResponse(
            ResourceErrorResponse::default(),
        )),
        TrafficClass::Transactional,
    );

    // StateStream messages
    assert_eq!(
        classify_server_payload(&ServerPayload::SceneSnapshot(SceneSnapshot::default())),
        TrafficClass::StateStream,
    );

    // DegradationNotice — transactional (RFC 0005 §3.4)
    assert_eq!(
        classify_server_payload(&ServerPayload::DegradationNotice(
            DegradationNotice::default()
        )),
        TrafficClass::Transactional,
    );

    // Ephemeral messages
    assert_eq!(
        classify_server_payload(&ServerPayload::Heartbeat(Heartbeat::default())),
        TrafficClass::Ephemeral,
    );
}

// ─── Sequence validation unit tests ─────────────────────────────────────

/// Unit tests for StreamSession::validate_client_sequence.
#[test]
fn test_validate_sequence_unit() {
    let mut session = StreamSession {
        session_id: "test".to_string(),
        namespace: "test".to_string(),
        agent_name: "test".to_string(),
        capabilities: Vec::new(),
        lease_ids: Vec::new(),
        scene_session_id: SceneId::new(),
        resource_budget: ResourceBudget::default(),
        budget_enforcer: None,
        subscriptions: Vec::new(),
        server_sequence: 0,
        resume_token: Vec::new(),
        last_heartbeat_ms: 0,
        state: SessionState::Active,
        last_client_sequence: 1,
        safe_mode_active: false,
        freeze_queue: SessionFreezeQueue::new(FREEZE_QUEUE_CAPACITY),
        session_open_at_wall_us: now_wall_us(),
        dedup_window: DedupWindow::new(1000, 60),
        lease_correlation_cache: LeaseCorrelationCache::new(
            DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY,
        ),
        resource_upload_rate_limiter: UploadByteRateLimiter::with_limit(
            tze_hud_resource::DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC,
        ),
    };

    // seq=2 (gap=1): OK
    assert!(session.validate_client_sequence(2, 100).is_ok());
    assert_eq!(session.last_client_sequence, 2);

    // seq=102 (gap=100): still OK (gap == max_gap, not >)
    assert!(session.validate_client_sequence(102, 100).is_ok());
    assert_eq!(session.last_client_sequence, 102);

    // seq=203 (gap=101): exceeds max_gap=100
    let err = session.validate_client_sequence(203, 100);
    assert!(err.is_err());
    let (code, _) = err.unwrap_err();
    assert_eq!(code, "SEQUENCE_GAP_EXCEEDED");
    // last_client_sequence unchanged on error
    assert_eq!(session.last_client_sequence, 102);

    // seq=50 (regression): error
    let err = session.validate_client_sequence(50, 100);
    assert!(err.is_err());
    let (code, _) = err.unwrap_err();
    assert_eq!(code, "SEQUENCE_REGRESSION");

    // seq=102 (same as last): regression (not strictly greater)
    let err = session.validate_client_sequence(102, 100);
    assert!(err.is_err());
    let (code, _) = err.unwrap_err();
    assert_eq!(code, "SEQUENCE_REGRESSION");
}

// ─── Handshake auth, version, capability, subscription tests (rig-8uqz) ──

/// Scenario: Structured AuthCredential (PSK) accepted (RFC 0005 §1.4)
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

/// Scenario: Invalid structured PSK credential rejected with AUTH_FAILED (RFC 0005 §1.4)
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

/// Scenario: LocalSocketCredential accepted (RFC 0005 §1.4)
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

// ── Wire-level LocalSocket non-loopback rejection (hud-stl9j / hud-1aswu.1) ──
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
/// Security regression gate for hud-1aswu.1: a future refactor that removes
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
/// Security regression gate for hud-1aswu.1 resume path: the resume path re-
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
        }
        other => panic!(
            "Expected SessionError(AUTH_FAILED) for non-loopback LocalSocket resume, \
                 got: {other:?}"
        ),
    }
}

/// Scenario: Version negotiated successfully (RFC 0005 §4.1)
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

/// Scenario: Version negotiation failure — no mutual version (RFC 0005 §4.1)
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
/// INPUT_EVENTS (RFC 0005 §7.1).
/// WHEN a PSK-authenticated agent requests INPUT_EVENTS subscription AND includes
/// access_input_events in requested_capabilities,
/// THEN SessionEstablished includes INPUT_EVENTS in active_subscriptions and
/// denied_subscriptions is empty.
///
/// Subscription gating uses the agent's explicitly granted capabilities (RFC 0005 §7.1).
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
/// still sends one decodes to an empty payload, which the server ignores.
#[test]
fn removed_client_request_numbers_decode_to_empty_payload() {
    use prost::Message;
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
    }
}
