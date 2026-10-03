use super::*;

#[tokio::test]
async fn test_resume_with_token() {
    let (mut client, _server) = setup_test().await;

    // Start initial session to get a resume token
    let (tx, init_messages, _stream) = handshake(&mut client, "resumable", "test-key").await;
    drop(tx); // Close the first stream
    drop(_stream);

    // Wait a bit for cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Now resume with the token
    let resume_token = match &init_messages[0].payload {
        Some(ServerPayload::SessionEstablished(established)) => established.resume_token.clone(),
        _ => panic!("Expected SessionEstablished"),
    };

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
) -> Vec<u8> {
    let (tx, init_messages, stream) = handshake(client, agent_id, psk).await;
    let resume_token = match &init_messages[0].payload {
        Some(ServerPayload::SessionEstablished(e)) => e.resume_token.clone(),
        _ => panic!("Expected SessionEstablished"),
    };
    drop(tx);
    drop(stream);
    // Allow server task to process EOF and register the resume token.
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    resume_token
}

/// Scenario: Reconnect within grace period succeeds with
/// `SessionResumeResult(accepted=true)`.
#[tokio::test]
async fn test_reconnect_within_grace_accepted() {
    let (mut client, _server) = setup_test().await;
    let resume_token = handshake_and_disconnect(&mut client, "resume-ok-agent", "test-key").await;

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
    let (mut client, _server) = setup_test().await;
    let resume_token = handshake_and_disconnect(&mut client, "single-use-agent", "test-key").await;

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
    drop(tx1);
    drop(r1);
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

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

/// Scenario: Re-authentication required on resume.
/// Invalid credentials result in `SessionError(AUTH_FAILED)`.
#[tokio::test]
async fn test_resume_auth_required() {
    let (mut client, _server) = setup_test().await;
    let resume_token = handshake_and_disconnect(&mut client, "auth-check-agent", "test-key").await;

    let (resume_tx, resume_rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let resume_stream = tokio_stream::wrappers::ReceiverStream::new(resume_rx);

    // Use wrong PSK on resume — must be rejected with AUTH_FAILED.
    resume_tx
        .send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionResume(SessionResume {
                agent_id: "auth-check-agent".to_string(),
                resume_token: resume_token.clone(),
                last_seen_server_sequence: 2,
                pre_shared_key: "wrong-key".to_string(),
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
                err.code, "AUTH_FAILED",
                "expected AUTH_FAILED, got: {}",
                err.code
            );
        }
        other => panic!("Expected SessionError(AUTH_FAILED), got: {other:?}"),
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
    let (mut client, _server) = setup_test().await;

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
    let resume_token = match &established_msg.payload {
        Some(ServerPayload::SessionEstablished(e)) => e.resume_token.clone(),
        other => panic!("Expected SessionEstablished, got: {other:?}"),
    };

    drop(tx);
    drop(response_stream);
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

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
