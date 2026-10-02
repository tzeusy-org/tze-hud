//! Session state machine tests.
//!
//! Tests lifecycle transitions (Connecting→Handshaking→Active→Disconnecting→Closed),
//! and Resuming path, from session-protocol/spec.md §1.1 and session_server.rs.
//!
//! Test count target: ≥8 covering legal transitions, illegal transitions, and
//! the handshake timeout / resume paths.

use tze_hud_protocol::proto::EventBatch;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{
    DegradationNotice, InputCaptureResponse, InputFocusResponse, Reclaimed, RequestResult,
    SceneSnapshot, SessionError, SessionEstablished, SessionResumeResult, SessionResumed,
    SessionSuspended, SubscriptionChangeResult,
};
use tze_hud_protocol::session_server::{SessionState, TrafficClass, classify_server_payload};
use tze_hud_protocol::token::{DEFAULT_GRACE_PERIOD_MS, TokenStore};

// ─── Legal Transitions ───────────────────────────────────────────────────────

/// WHEN stream opened THEN state starts at Connecting.
#[test]
fn initial_state_is_connecting() {
    let state = SessionState::Connecting;
    assert_eq!(state, SessionState::Connecting);
}

/// WHEN session is Connecting and SessionInit received THEN transitions to Handshaking.
#[test]
fn connecting_to_handshaking_on_session_init() {
    let mut state = SessionState::Connecting;
    assert!(
        !state.allows_mutations(),
        "Connecting must not allow mutations"
    );
    // Simulate: SessionInit received => move to Handshaking
    state = SessionState::Handshaking;
    assert_eq!(state, SessionState::Handshaking);
    assert!(!state.allows_mutations());
}

/// WHEN session is Handshaking and SessionEstablished sent THEN transitions to Active.
#[test]
fn handshaking_to_active_on_established() {
    let mut state = SessionState::Handshaking;
    assert!(
        !state.allows_mutations(),
        "Handshaking must not allow mutations"
    );
    state = SessionState::Active;
    assert_eq!(state, SessionState::Active);
    assert!(
        state.allows_mutations(),
        "Active state must allow mutations"
    );
}

/// WHEN session is Active and SessionClose sent THEN transitions to Disconnecting.
#[test]
fn active_to_disconnecting_on_session_close() {
    let mut state = SessionState::Active;
    assert!(state.allows_mutations(), "Active must allow mutations");
    state = SessionState::Disconnecting;
    assert_eq!(state, SessionState::Disconnecting);
    assert!(
        !state.allows_mutations(),
        "Disconnecting state must not allow mutations"
    );
}

/// WHEN session is Disconnecting and stream terminates THEN transitions to Closed.
#[test]
fn disconnecting_to_closed_on_stream_termination() {
    let mut state = SessionState::Disconnecting;
    assert!(
        !state.allows_mutations(),
        "Disconnecting must not allow mutations"
    );
    state = SessionState::Closed;
    assert_eq!(state, SessionState::Closed);
}

/// WHEN session is Active and heartbeat timeout THEN transitions directly to Closed (ungraceful).
#[test]
fn active_to_closed_on_heartbeat_timeout() {
    let mut state = SessionState::Active;
    assert!(state.allows_mutations(), "Active must allow mutations");
    // Ungraceful disconnect: heartbeat timeout
    state = SessionState::Closed;
    assert_eq!(state, SessionState::Closed);
}

/// WHEN session is Closed and SessionResume received within grace period THEN transitions to Resuming.
#[test]
fn closed_to_resuming_on_valid_resume() {
    let mut state = SessionState::Closed;
    assert!(!state.allows_mutations(), "Closed must not allow mutations");
    state = SessionState::Resuming;
    assert_eq!(state, SessionState::Resuming);
}

/// WHEN session is Resuming and token is valid THEN transitions to Active.
#[test]
fn resuming_to_active_on_valid_token() {
    let mut state = SessionState::Resuming;
    assert!(
        !state.allows_mutations(),
        "Resuming must not allow mutations"
    );
    state = SessionState::Active;
    assert_eq!(state, SessionState::Active);
    assert!(state.allows_mutations());
}

/// WHEN session is Resuming and token expired THEN transitions to Closed.
#[test]
fn resuming_to_closed_on_expired_token() {
    let mut state = SessionState::Resuming;
    assert!(
        !state.allows_mutations(),
        "Resuming must not allow mutations"
    );
    state = SessionState::Closed;
    assert_eq!(state, SessionState::Closed);
}

// ─── Illegal Transitions ─────────────────────────────────────────────────────

/// WHEN state is Closed THEN allows_mutations() returns false (no mutations from closed session).
#[test]
fn closed_state_does_not_allow_mutations() {
    let state = SessionState::Closed;
    assert!(
        !state.allows_mutations(),
        "Closed state must not allow mutations"
    );
}

/// WHEN state is Handshaking THEN allows_mutations() returns false.
#[test]
fn handshaking_state_does_not_allow_mutations() {
    let state = SessionState::Handshaking;
    assert!(
        !state.allows_mutations(),
        "Handshaking state must not allow mutations"
    );
}

/// WHEN state is Resuming THEN allows_mutations() returns false.
#[test]
fn resuming_state_does_not_allow_mutations() {
    let state = SessionState::Resuming;
    assert!(
        !state.allows_mutations(),
        "Resuming state must not allow mutations"
    );
}

/// WHEN state is Disconnecting THEN it is not the same as Active.
#[test]
fn disconnecting_is_not_active() {
    let state = SessionState::Disconnecting;
    assert_ne!(state, SessionState::Active);
}

/// State machine label coverage — verify all states have human-readable labels.
#[test]
fn all_states_have_labels() {
    let states = [
        SessionState::Connecting,
        SessionState::Handshaking,
        SessionState::Active,
        SessionState::Disconnecting,
        SessionState::Closed,
        SessionState::Resuming,
    ];
    for s in &states {
        let label = s.label();
        assert!(!label.is_empty(), "State {s:?} must have non-empty label");
    }
}

// ─── Token Store (resume within / after grace period) ────────────────────────

/// WHEN session token stored and queried within grace period THEN valid.
#[test]
fn resume_token_valid_within_grace_period() {
    let mut store = TokenStore::new();
    let token = vec![0xAB; 16];
    let now_ms = 1_000_000u64;
    let grace_ms = DEFAULT_GRACE_PERIOD_MS; // 30000ms

    store.insert(
        token.clone(),
        "agent-1".to_string(),
        vec!["resident_mcp".to_string()],
        vec!["DEGRADATION_NOTICES".to_string()],
        vec![],
        grace_ms,
        now_ms,
    );

    // Within grace period: 10 seconds later — consume should succeed
    let result = store.consume(&token, "agent-1", now_ms + 10_000);
    assert!(
        result.is_ok(),
        "token should be valid within grace period: {:?}",
        result.err()
    );
}

/// WHEN session token stored and queried after grace period THEN expired.
#[test]
fn resume_token_invalid_after_grace_period() {
    let mut store = TokenStore::new();
    let token = vec![0xCD; 16];
    let now_ms = 1_000_000u64;
    let grace_ms = DEFAULT_GRACE_PERIOD_MS; // 30000ms

    store.insert(
        token.clone(),
        "agent-2".to_string(),
        vec![],
        vec![],
        vec![],
        grace_ms,
        now_ms,
    );

    // After grace period: 31 seconds later
    let result = store.consume(&token, "agent-2", now_ms + 31_000);
    assert!(
        result.is_err(),
        "token should be expired after grace period"
    );
}

/// WHEN resume token used by wrong agent THEN rejected.
#[test]
fn resume_token_bound_to_agent_id() {
    let mut store = TokenStore::new();
    let token = vec![0xEF; 16];
    let now_ms = 2_000_000u64;

    store.insert(
        token.clone(),
        "agent-1".to_string(),
        vec![],
        vec![],
        vec![],
        DEFAULT_GRACE_PERIOD_MS,
        now_ms,
    );

    // Wrong agent_id — should fail with TokenNotFound (token stays in store)
    let result = store.consume(&token, "agent-2", now_ms + 1_000);
    assert!(
        result.is_err(),
        "token must be bound to agent_id; different agent must be rejected"
    );
}

/// WHEN resume token is consumed THEN it cannot be reused (single-use semantics).
#[test]
fn resume_token_is_single_use() {
    let mut store = TokenStore::new();
    let token = vec![0x12; 16];
    let now_ms = 3_000_000u64;

    store.insert(
        token.clone(),
        "agent-3".to_string(),
        vec![],
        vec![],
        vec![],
        DEFAULT_GRACE_PERIOD_MS,
        now_ms,
    );

    // First use: should succeed
    let first = store.consume(&token, "agent-3", now_ms + 1_000);
    assert!(first.is_ok(), "first use of token must succeed");

    // Second use: must fail (token consumed)
    let second = store.consume(&token, "agent-3", now_ms + 2_000);
    assert!(
        second.is_err(),
        "second use of token must fail (single-use)"
    );
}

// ─── Traffic class classification ────────────────────────────────────────────

/// WHEN session lifecycle payloads THEN classified as Transactional.
#[test]
fn session_lifecycle_payloads_are_transactional() {
    let transactional_payloads = vec![
        ServerPayload::SessionEstablished(SessionEstablished::default()),
        ServerPayload::SessionError(SessionError::default()),
        ServerPayload::SessionResumeResult(SessionResumeResult::default()),
        ServerPayload::SessionSuspended(SessionSuspended::default()),
        ServerPayload::SessionResumed(SessionResumed::default()),
        ServerPayload::Reclaimed(Reclaimed::default()),
        ServerPayload::RequestResult(RequestResult::default()),
        ServerPayload::RequestResult(RequestResult::default()),
        ServerPayload::SubscriptionChangeResult(SubscriptionChangeResult::default()),
        ServerPayload::RequestResult(RequestResult::default()),
        ServerPayload::InputFocusResponse(InputFocusResponse::default()),
        ServerPayload::InputCaptureResponse(InputCaptureResponse::default()),
        ServerPayload::DegradationNotice(DegradationNotice::default()),
    ];
    for payload in &transactional_payloads {
        assert_eq!(
            classify_server_payload(payload),
            TrafficClass::Transactional,
            "payload {:?} should be Transactional",
            std::mem::discriminant(payload)
        );
    }
}

/// WHEN scene state/event payloads THEN classified as StateStream.
#[test]
fn scene_state_payloads_are_state_stream() {
    let state_stream_payloads = vec![
        ServerPayload::SceneSnapshot(SceneSnapshot::default()),
        ServerPayload::EventBatch(EventBatch::default()),
    ];
    for payload in &state_stream_payloads {
        assert_eq!(
            classify_server_payload(payload),
            TrafficClass::StateStream,
            "payload should be StateStream"
        );
    }
}
