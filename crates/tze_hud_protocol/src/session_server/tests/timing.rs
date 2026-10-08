use super::*;

// ─── TimingHints validation ──────────────────────────────────────────────────

/// Unit test for validate_timing_hints: TIMESTAMP_TOO_OLD.
#[test]
fn test_timing_hints_too_old() {
    let session_open = 200_000_000u64;
    let now = session_open;
    let present = session_open - 61_000_001;
    let mut hints = TimingHints {
        present_at_wall_us: present,
        expires_at_wall_us: 0,
    };
    let result = validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US, now);
    assert!(result.is_err());
    let (code, _) = result.unwrap_err();
    assert_eq!(code, "TIMESTAMP_TOO_OLD");

    let threshold = session_open - 60_000_000;
    hints.present_at_wall_us = threshold - 1;
    assert_eq!(
        validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US, now)
            .unwrap_err()
            .0,
        "TIMESTAMP_TOO_OLD"
    );
    for present in [threshold, threshold + 1] {
        hints.present_at_wall_us = present;
        assert!(
            validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US, now)
                .is_ok()
        );
    }
}

/// Unit test for validate_timing_hints: TIMESTAMP_TOO_FUTURE.
#[test]
fn test_timing_hints_too_future() {
    let session_open = 200_000_000u64;
    let now = session_open;
    let max_future = DEFAULT_MAX_FUTURE_SCHEDULE_US;
    let horizon = now + max_future;
    let mut hints = TimingHints {
        present_at_wall_us: horizon + 1,
        expires_at_wall_us: 0,
    };
    let result = validate_timing_hints(&hints, session_open, max_future, now);
    assert!(result.is_err());
    let (code, _) = result.unwrap_err();
    assert_eq!(code, "TIMESTAMP_TOO_FUTURE");
    for present in [horizon - 1, horizon] {
        hints.present_at_wall_us = present;
        assert!(validate_timing_hints(&hints, session_open, max_future, now).is_ok());
    }
}

/// Unit test for validate_timing_hints: TIMESTAMP_EXPIRY_BEFORE_PRESENT.
#[test]
fn test_timing_hints_expiry_before_present() {
    let now = 200_000_000u64;
    let session_open = now - 1_000_000;
    let present = now + 1_000_000;
    let mut hints = TimingHints {
        present_at_wall_us: present,
        expires_at_wall_us: present - 1,
    };
    let result = validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US, now);
    assert!(result.is_err());
    let (code, _) = result.unwrap_err();
    assert_eq!(code, "TIMESTAMP_EXPIRY_BEFORE_PRESENT");
    hints.expires_at_wall_us = present;
    assert_eq!(
        validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US, now)
            .unwrap_err()
            .0,
        "TIMESTAMP_EXPIRY_BEFORE_PRESENT"
    );
    hints.expires_at_wall_us = present + 1;
    assert!(
        validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US, now).is_ok()
    );
}

/// Unit test for validate_timing_hints: valid future scheduling (present_at in future).
#[test]
fn test_timing_hints_valid_future() {
    let now = 200_000_000u64;
    let session_open = now - 1_000_000;
    let present = now + 500_000;
    let expires = present + 2_000_000;
    let hints = TimingHints {
        present_at_wall_us: present,
        expires_at_wall_us: expires,
    };
    assert!(
        validate_timing_hints(&hints, session_open, DEFAULT_MAX_FUTURE_SCHEDULE_US, now).is_ok(),
        "Valid future TimingHints should not be rejected"
    );
    // Horizon addition remains saturating at the wall-clock representation limit.
    let hints = TimingHints {
        present_at_wall_us: u64::MAX,
        expires_at_wall_us: 0,
    };
    assert!(
        validate_timing_hints(
            &hints,
            u64::MAX,
            DEFAULT_MAX_FUTURE_SCHEDULE_US,
            u64::MAX - 1
        )
        .is_ok()
    );
}

/// Unit test for validate_timing_hints: zero fields bypass validation.
#[test]
fn test_timing_hints_zero_bypasses_validation() {
    let session_open = 200_000_000u64;
    let mut hints = TimingHints {
        present_at_wall_us: 0,
        expires_at_wall_us: 0,
    };
    assert!(
        validate_timing_hints(
            &hints,
            session_open,
            DEFAULT_MAX_FUTURE_SCHEDULE_US,
            session_open
        )
        .is_ok(),
        "Zero TimingHints should always be valid"
    );
    hints.expires_at_wall_us = 1;
    assert!(
        validate_timing_hints(
            &hints,
            session_open,
            DEFAULT_MAX_FUTURE_SCHEDULE_US,
            session_open
        )
        .is_ok()
    );
}

/// Integration test: MutationBatch with TIMESTAMP_TOO_OLD is rejected via stream.
#[tokio::test]
async fn test_mutation_timing_too_old_rejected() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "timing-old-agent", "test-key").await;

    // Get a lease
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

    // Send a mutation with present_at more than 60s before epoch 0 (which means
    // it's more than 60s before session open; session opened near now_wall_us(),
    // so session_open - 60s - 1 ≫ 0 for any real timestamp).
    //
    // Use present_at = 1 µs since epoch — guaranteed to be older than
    // session_open_at_wall_us - 60_000_000.
    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: Some(TimingHints {
                present_at_wall_us: 1, // far in the past
                expires_at_wall_us: 0,
            }),
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(err)) => {
            assert_eq!(err.code, "TIMESTAMP_TOO_OLD");
        }
        other => panic!("Expected RuntimeError(TIMESTAMP_TOO_OLD), got: {other:?}"),
    }

    drop(tx);
}

/// Integration test: MutationBatch with TIMESTAMP_EXPIRY_BEFORE_PRESENT is rejected.
#[tokio::test]
async fn test_mutation_timing_expiry_before_present_rejected() {
    let (mut client, _server) = setup_test().await;
    let (tx, _init_messages, mut stream) =
        handshake(&mut client, "timing-exp-agent", "test-key").await;

    // Get a lease
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

    let now = now_wall_us();
    let present = now + 500_000; // 500ms in future
    let expires = present - 1; // expires 1µs before present → invalid

    let batch_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.send(ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(ClientPayload::MutationBatch(MutationBatch {
            batch_id: batch_id.clone(),
            lease_id: lease_id.clone(),
            mutations: Vec::new(),
            timing: Some(TimingHints {
                present_at_wall_us: present,
                expires_at_wall_us: expires,
            }),
        })),
    })
    .await
    .unwrap();

    let result_msg = next_server_msg(&mut stream).await;
    match &result_msg.payload {
        Some(ServerPayload::RequestResult(err)) => {
            assert_eq!(err.code, "TIMESTAMP_EXPIRY_BEFORE_PRESENT");
        }
        other => {
            panic!("Expected RuntimeError(TIMESTAMP_EXPIRY_BEFORE_PRESENT), got: {other:?}")
        }
    }

    drop(tx);
}
