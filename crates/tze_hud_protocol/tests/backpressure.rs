//! Traffic-class tests for outbound server messages.
//!
//! - Transactional messages are never dropped.
//! - Ephemeral realtime messages drop the oldest on overflow (latest wins).
//! - State-stream messages coalesce (latest state per key wins).

use tze_hud_protocol::proto::EventBatch;
use tze_hud_protocol::proto::session::{
    Heartbeat, LeaseResponse, MutationResult, SceneSnapshot, SessionEstablished,
    server_message::Payload as ServerPayload,
};
use tze_hud_protocol::session_server::{TrafficClass, classify_server_payload};

/// Transactional messages are never dropped (classified as Transactional).
#[test]
fn transactional_messages_never_dropped() {
    let payloads = vec![
        ServerPayload::SessionEstablished(SessionEstablished::default()),
        ServerPayload::MutationResult(MutationResult::default()),
        ServerPayload::LeaseResponse(LeaseResponse::default()),
    ];
    for payload in &payloads {
        assert_eq!(
            classify_server_payload(payload),
            TrafficClass::Transactional,
            "transactional payload must never be classified as droppable"
        );
    }
}

/// Ephemeral messages are droppable (classified as Ephemeral).
#[test]
fn heartbeat_is_ephemeral_and_droppable() {
    let payload = ServerPayload::Heartbeat(Heartbeat {
        timestamp_mono_us: 12345,
    });
    assert_eq!(
        classify_server_payload(&payload),
        TrafficClass::Ephemeral,
        "Heartbeat must be Ephemeral — oldest dropped under backpressure, latest-wins"
    );
}

/// State-stream messages are coalesced under pressure.
#[test]
fn state_stream_messages_are_coalesced_class() {
    let payloads = vec![
        ServerPayload::SceneSnapshot(SceneSnapshot::default()),
        ServerPayload::EventBatch(EventBatch::default()),
    ];
    for payload in &payloads {
        assert_eq!(
            classify_server_payload(payload),
            TrafficClass::StateStream,
            "scene/event payloads must be StateStream (coalesced under pressure)"
        );
    }
}

/// Transactional and Ephemeral are distinct classes.
#[test]
fn transactional_not_droppable_different_from_ephemeral() {
    let tc = classify_server_payload(&ServerPayload::MutationResult(MutationResult::default()));
    let te = classify_server_payload(&ServerPayload::Heartbeat(Heartbeat::default()));
    assert_ne!(tc, te);
    assert_eq!(tc, TrafficClass::Transactional);
    assert_eq!(te, TrafficClass::Ephemeral);
}
