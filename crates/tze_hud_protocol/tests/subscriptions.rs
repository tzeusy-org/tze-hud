//! Subscription filtering integration tests.
//!
//! Tests category-to-capability mapping from session-protocol/spec.md lines 445-452,
//! mandatory subscription enforcement, and event batch variant filtering.
//!
//! These are integration-level tests that exercise the subscriptions module
//! from outside the crate (via the public API).
//!
//! Test count target: ≥6 tests.

use tze_hud_protocol::proto::input_envelope::Event as EnvEvent;
use tze_hud_protocol::proto::{
    CaptureReleasedEvent, EventBatch, FocusGainedEvent, FocusLostEvent, InputEnvelope,
    KeyDownEvent, PointerDownEvent, PointerMoveEvent, PointerUpEvent,
};
use tze_hud_protocol::subscriptions::{category, filter_event_batch, filter_subscriptions};

// ─── Category-to-capability mapping ─────────────────────────────────────────

/// SCENE_TOPOLOGY → read_scene_topology (spec lines 445-452).
#[test]
fn scene_topology_requires_read_scene_topology_capability() {
    // Without capability: denied
    let denied = filter_subscriptions(&[category::SCENE_TOPOLOGY.to_string()], &[]);
    assert!(
        denied
            .denied
            .contains(&category::SCENE_TOPOLOGY.to_string())
    );

    // With capability: active
    let caps = vec!["read_scene_topology".to_string()];
    let granted = filter_subscriptions(&[category::SCENE_TOPOLOGY.to_string()], &caps);
    assert!(
        granted
            .active
            .contains(&category::SCENE_TOPOLOGY.to_string())
    );
    assert!(granted.denied.is_empty());
}

/// INPUT_EVENTS and FOCUS_EVENTS → access_input_events.
#[test]
fn input_and_focus_events_require_access_input_events() {
    let categories = vec![
        category::INPUT_EVENTS.to_string(),
        category::FOCUS_EVENTS.to_string(),
    ];

    // Without capability: both denied
    let denied = filter_subscriptions(&categories, &[]);
    assert!(denied.denied.contains(&category::INPUT_EVENTS.to_string()));
    assert!(denied.denied.contains(&category::FOCUS_EVENTS.to_string()));

    // With access_input_events: both granted
    let caps = vec!["access_input_events".to_string()];
    let granted = filter_subscriptions(&categories, &caps);
    assert!(granted.active.contains(&category::INPUT_EVENTS.to_string()));
    assert!(granted.active.contains(&category::FOCUS_EVENTS.to_string()));
    assert!(granted.denied.is_empty());
}

/// TELEMETRY_FRAMES → read_telemetry.
#[test]
fn telemetry_frames_requires_read_telemetry() {
    let denied = filter_subscriptions(&[category::TELEMETRY_FRAMES.to_string()], &[]);
    assert!(
        denied
            .denied
            .contains(&category::TELEMETRY_FRAMES.to_string())
    );

    let caps = vec!["read_telemetry".to_string()];
    let granted = filter_subscriptions(&[category::TELEMETRY_FRAMES.to_string()], &caps);
    assert!(
        granted
            .active
            .contains(&category::TELEMETRY_FRAMES.to_string())
    );
}

/// DEGRADATION_NOTICES is always active — mandatory subscription.
#[test]
fn degradation_notices_mandatory_cannot_be_removed() {
    // Even when not requested and no capabilities
    let result = filter_subscriptions(&[], &[]);
    assert!(
        result
            .active
            .contains(&category::DEGRADATION_NOTICES.to_string()),
        "DEGRADATION_NOTICES must always be active (mandatory)"
    );
}

/// Unknown subscription category is denied.
#[test]
fn unknown_subscription_category_is_denied() {
    let result = filter_subscriptions(&["UNKNOWN_CATEGORY_XYZ".to_string()], &[]);
    assert!(
        result.denied.contains(&"UNKNOWN_CATEGORY_XYZ".to_string()),
        "unknown subscription categories must be denied"
    );
    assert!(!result.active.contains(&"UNKNOWN_CATEGORY_XYZ".to_string()));
}

// ─── EventBatch variant filtering ────────────────────────────────────────────

/// WHEN subscribed to INPUT_EVENTS only THEN focus variants are filtered out.
#[test]
fn input_subscription_only_filters_focus_variants() {
    let subs = vec![category::INPUT_EVENTS.to_string()];
    let batch = EventBatch {
        frame_number: 1,
        batch_ts_us: 0,
        events: vec![
            InputEnvelope {
                event: Some(EnvEvent::PointerDown(PointerDownEvent::default())),
            },
            InputEnvelope {
                event: Some(EnvEvent::FocusGained(FocusGainedEvent::default())),
            },
            InputEnvelope {
                event: Some(EnvEvent::KeyDown(KeyDownEvent::default())),
            },
        ],
    };
    let filtered = filter_event_batch(batch, &subs).expect("should not be empty");
    assert_eq!(filtered.events.len(), 2, "FocusGained must be removed");
    assert!(matches!(
        filtered.events[0].event,
        Some(EnvEvent::PointerDown(_))
    ));
    assert!(matches!(
        filtered.events[1].event,
        Some(EnvEvent::KeyDown(_))
    ));
}

/// WHEN subscribed to FOCUS_EVENTS only THEN input variants are filtered out.
#[test]
fn focus_subscription_only_filters_input_variants() {
    let subs = vec![category::FOCUS_EVENTS.to_string()];
    let batch = EventBatch {
        frame_number: 2,
        batch_ts_us: 0,
        events: vec![
            InputEnvelope {
                event: Some(EnvEvent::PointerMove(PointerMoveEvent::default())),
            },
            InputEnvelope {
                event: Some(EnvEvent::FocusLost(FocusLostEvent::default())),
            },
        ],
    };
    let filtered = filter_event_batch(batch, &subs).expect("should not be empty");
    assert_eq!(filtered.events.len(), 1);
    assert!(matches!(
        filtered.events[0].event,
        Some(EnvEvent::FocusLost(_))
    ));
}

/// WHEN subscribed to neither INPUT_EVENTS nor FOCUS_EVENTS THEN batch is dropped entirely.
#[test]
fn no_input_subscription_batch_not_delivered() {
    let subs = vec![category::SCENE_TOPOLOGY.to_string()];
    let batch = EventBatch {
        frame_number: 3,
        batch_ts_us: 0,
        events: vec![InputEnvelope {
            event: Some(EnvEvent::PointerDown(PointerDownEvent::default())),
        }],
    };
    assert!(
        filter_event_batch(batch, &subs).is_none(),
        "batch must not be delivered when agent lacks both input subscriptions"
    );
}

/// WHEN subscribed to both THEN all variants are delivered.
#[test]
fn both_input_and_focus_subscriptions_deliver_all_variants() {
    let subs = vec![
        category::INPUT_EVENTS.to_string(),
        category::FOCUS_EVENTS.to_string(),
    ];
    let batch = EventBatch {
        frame_number: 4,
        batch_ts_us: 0,
        events: vec![
            InputEnvelope {
                event: Some(EnvEvent::PointerDown(PointerDownEvent::default())),
            },
            InputEnvelope {
                event: Some(EnvEvent::FocusGained(FocusGainedEvent::default())),
            },
            InputEnvelope {
                event: Some(EnvEvent::KeyDown(KeyDownEvent::default())),
            },
            InputEnvelope {
                event: Some(EnvEvent::CaptureReleased(CaptureReleasedEvent::default())),
            },
        ],
    };
    let filtered = filter_event_batch(batch, &subs).expect("all events present");
    assert_eq!(filtered.events.len(), 4, "all 4 events must be delivered");
}

/// WHEN batch has only focus events and agent has only INPUT_EVENTS subscription THEN None.
#[test]
fn all_focus_events_filtered_returns_none_for_input_only_subscriber() {
    let subs = vec![category::INPUT_EVENTS.to_string()];
    let batch = EventBatch {
        frame_number: 5,
        batch_ts_us: 0,
        events: vec![
            InputEnvelope {
                event: Some(EnvEvent::FocusGained(FocusGainedEvent::default())),
            },
            InputEnvelope {
                event: Some(EnvEvent::FocusLost(FocusLostEvent::default())),
            },
        ],
    };
    assert!(
        filter_event_batch(batch, &subs).is_none(),
        "batch with only focus events must not be delivered to INPUT_EVENTS-only subscriber"
    );
}

/// Within-batch ordering is preserved after filtering.
#[test]
fn event_batch_filtering_preserves_order() {
    let subs = vec![category::INPUT_EVENTS.to_string()];
    let batch = EventBatch {
        frame_number: 10,
        batch_ts_us: 0,
        events: vec![
            InputEnvelope {
                event: Some(EnvEvent::PointerDown(PointerDownEvent {
                    button: 0,
                    ..Default::default()
                })),
            },
            InputEnvelope {
                event: Some(EnvEvent::FocusGained(FocusGainedEvent::default())),
            }, // filtered
            InputEnvelope {
                event: Some(EnvEvent::PointerUp(PointerUpEvent {
                    button: 0,
                    ..Default::default()
                })),
            },
            InputEnvelope {
                event: Some(EnvEvent::KeyDown(KeyDownEvent {
                    key_code: "Space".to_string(),
                    ..Default::default()
                })),
            },
        ],
    };
    let filtered = filter_event_batch(batch, &subs).expect("non-empty");
    // Ordering: PointerDown, PointerUp, KeyDown (FocusGained removed)
    assert_eq!(filtered.events.len(), 3);
    assert!(matches!(
        filtered.events[0].event,
        Some(EnvEvent::PointerDown(_))
    ));
    assert!(matches!(
        filtered.events[1].event,
        Some(EnvEvent::PointerUp(_))
    ));
    assert!(matches!(
        filtered.events[2].event,
        Some(EnvEvent::KeyDown(_))
    ));
}
