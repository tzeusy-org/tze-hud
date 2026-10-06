//! Subscription category management for the session protocol (RFC 0005 §7).
//!
//! # Overview
//!
//! Subscription categories control which event types are delivered to an agent.
//! Some categories are always active (mandatory); others require specific granted
//! capabilities. The runtime validates subscriptions at session init.
//!
//! # Mandatory subscriptions
//!
//! `DEGRADATION_NOTICES` is always active regardless of what the agent requests
//! or what capabilities it has.
//!
//! # Capability requirements
//!
//! | Category              | Required capability             |
//! |-----------------------|---------------------------------|
//! | SCENE_TOPOLOGY        | read_scene_topology             |
//! | INPUT_EVENTS          | access_input_events             |
//! | FOCUS_EVENTS          | access_input_events             |
//! | DEGRADATION_NOTICES   | (mandatory — no requirement)    |
//! | TELEMETRY_FRAMES      | read_telemetry                  |
//!
//! # EventBatch variant filtering
//!
//! Within a delivered [`EventBatch`], variants are filtered per subscription:
//! - `FOCUS_EVENTS` subscription: `FocusGainedEvent`, `FocusLostEvent`,
//!   `CaptureReleasedEvent`.
//! - `INPUT_EVENTS` subscription: all other pointer, key, scroll,
//!   command, and character events.
//!
//! Per-variant filtering preserves within-batch ordering (RFC 0004 §8.4).
//! An agent subscribed only to `input_events` receives pointer/key events but
//! NOT focus events. An agent not subscribed to either receives no events
//! from the batch (the batch is not delivered at all).

use crate::proto::{EventBatch, InputEnvelope, input_envelope};

/// Well-known subscription category names (RFC 0005 §7.1, RFC 0010 §1.2).
pub mod category {
    /// Scene topology events: tile creation/deletion/update. Requires `read_scene_topology`.
    pub const SCENE_TOPOLOGY: &str = "SCENE_TOPOLOGY";
    /// Pointer, key, scroll, command, and character input events.
    /// Requires `access_input_events`.
    pub const INPUT_EVENTS: &str = "INPUT_EVENTS";
    /// Focus gain/loss and capture release events.
    /// Requires `access_input_events`.
    pub const FOCUS_EVENTS: &str = "FOCUS_EVENTS";
    /// Runtime degradation level changes. Always active; not filterable.
    pub const DEGRADATION_NOTICES: &str = "DEGRADATION_NOTICES";
    /// Compositor performance telemetry. Requires `read_telemetry`.
    pub const TELEMETRY_FRAMES: &str = "TELEMETRY_FRAMES";

    /// Mandatory subscriptions — always active, cannot be removed.
    pub const MANDATORY: &[&str] = &[DEGRADATION_NOTICES];
}

/// Return the capability required to subscribe to `category`, or `None` if
/// the category is mandatory (no capability required).
///
/// `DEGRADATION_NOTICES` returns `None` because it is always active and cannot
/// be filtered out.
fn required_capability(cat: &str) -> Option<&'static str> {
    match cat {
        category::SCENE_TOPOLOGY => Some("read_scene_topology"),
        category::INPUT_EVENTS => Some("access_input_events"),
        category::FOCUS_EVENTS => Some("access_input_events"),
        category::DEGRADATION_NOTICES => None, // mandatory
        category::TELEMETRY_FRAMES => Some("read_telemetry"),
        _ => Some("__unknown__"), // Unknown category: always denied
    }
}

/// Returns `true` if `category` is mandatory (always active, cannot be filtered).
pub fn is_mandatory(category: &str) -> bool {
    category == category::DEGRADATION_NOTICES
}

/// Returns `true` if the agent has the capability required for `category`.
///
/// Unknown categories are unconditionally denied regardless of the agent's
/// capabilities, preventing a malicious agent from activating unknown
/// subscription categories by requesting a synthetic `"__unknown__"` capability.
fn has_required_capability(category: &str, capabilities: &[String]) -> bool {
    match required_capability(category) {
        None => true,                 // mandatory — no capability check
        Some("__unknown__") => false, // unknown category — unconditionally denied
        Some(req) => capabilities.iter().any(|c| c == req || c == "*"),
    }
}

/// Result of filtering a set of requested subscription categories.
pub struct SubscriptionFilterResult {
    /// Categories that are active after filtering (includes mandatory categories).
    pub active: Vec<String>,
    /// Categories that were requested but denied due to missing capability.
    pub denied: Vec<String>,
}

/// Filter `requested` subscription categories against `granted_capabilities`.
///
/// The mandatory category (`DEGRADATION_NOTICES`) is included
/// in `active` unconditionally, even if not requested and regardless of
/// capabilities. Requested categories that require capabilities the agent
/// doesn't have are placed in `denied`. Unknown categories are denied.
///
/// # Arguments
///
/// - `requested`: The categories the agent requested (e.g. from `SessionInit.initial_subscriptions`).
/// - `granted_capabilities`: The capabilities already granted to the agent.
///
/// # Returns
///
/// A [`SubscriptionFilterResult`] with the active and denied sets.
pub fn filter_subscriptions(
    requested: &[String],
    granted_capabilities: &[String],
) -> SubscriptionFilterResult {
    let mut active: Vec<String> = Vec::new();
    let mut denied: Vec<String> = Vec::new();

    // Evaluate each requested category
    for cat in requested {
        if is_mandatory(cat.as_str()) {
            // Mandatory categories: always active (also added unconditionally below,
            // but handle here to avoid double-insertion from requested list)
            if !active.contains(cat) {
                active.push(cat.clone());
            }
        } else if has_required_capability(cat.as_str(), granted_capabilities) {
            if !active.contains(cat) {
                active.push(cat.clone());
            }
        } else {
            denied.push(cat.clone());
        }
    }

    // Ensure mandatory subscriptions are always present
    for &mandatory in category::MANDATORY {
        let s = mandatory.to_string();
        if !active.contains(&s) {
            active.push(s);
        }
    }

    SubscriptionFilterResult { active, denied }
}

// ─── EventBatch variant filtering ────────────────────────────────────────────

/// Returns `true` if the given [`InputEnvelope`] variant is a focus event
/// (must be filtered by the `FOCUS_EVENTS` subscription).
///
/// Focus variants per RFC 0005 §7.1:
/// - `FocusGainedEvent`
/// - `FocusLostEvent`
/// - `CaptureReleasedEvent`
pub fn is_focus_variant(envelope: &InputEnvelope) -> bool {
    matches!(
        &envelope.event,
        Some(input_envelope::Event::FocusGained(_))
            | Some(input_envelope::Event::FocusLost(_))
            | Some(input_envelope::Event::CaptureReleased(_))
    )
}

/// Returns `true` if the given [`InputEnvelope`] variant is an input event
/// (must be filtered by the `INPUT_EVENTS` subscription).
///
/// All non-focus variants are considered input events:
/// pointer, touch, key, scroll, command, character events.
pub fn is_input_variant(envelope: &InputEnvelope) -> bool {
    // Any envelope variant that is not a focus variant is an input variant.
    // Envelopes with no event field set are dropped silently.
    envelope.event.is_some() && !is_focus_variant(envelope)
}

/// Filter an [`EventBatch`] based on the agent's active subscriptions.
///
/// Per RFC 0005 §7.1:
/// - Focus variants are included only if the agent has `FOCUS_EVENTS` active.
/// - Input variants are included only if the agent has `INPUT_EVENTS` active.
/// - Within-batch ordering is preserved (RFC 0004 §8.4).
///
/// Returns `Some(filtered_batch)` if any events remain after filtering, or
/// `None` if all events were removed (the batch should not be sent at all).
///
/// # Arguments
///
/// - `batch`: The full event batch assembled by the compositor.
/// - `active_subscriptions`: The agent's current active subscription set.
pub fn filter_event_batch(
    batch: EventBatch,
    active_subscriptions: &[String],
) -> Option<EventBatch> {
    let has_input = active_subscriptions
        .iter()
        .any(|s| s == category::INPUT_EVENTS);
    let has_focus = active_subscriptions
        .iter()
        .any(|s| s == category::FOCUS_EVENTS);

    if !has_input && !has_focus {
        // Agent is not subscribed to any input category; do not deliver
        return None;
    }

    let filtered_events: Vec<InputEnvelope> = batch
        .events
        .into_iter()
        .filter(|env| {
            if is_focus_variant(env) {
                has_focus
            } else if is_input_variant(env) {
                has_input
            } else {
                // Unknown/empty variant — drop
                false
            }
        })
        .collect();

    if filtered_events.is_empty() {
        None
    } else {
        Some(EventBatch {
            frame_number: batch.frame_number,
            batch_ts_us: batch.batch_ts_us,
            events: filtered_events,
        })
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── filter_subscriptions tests ──────────────────────────────────────────

    #[test]
    fn test_mandatory_always_active_even_if_not_requested() {
        let result = filter_subscriptions(&[], &[]);
        assert!(
            result
                .active
                .contains(&category::DEGRADATION_NOTICES.to_string()),
            "DEGRADATION_NOTICES must always be active"
        );
        assert!(result.denied.is_empty());
    }

    #[test]
    fn test_scene_topology_granted_with_capability() {
        let caps = vec!["read_scene_topology".to_string()];
        let result = filter_subscriptions(&["SCENE_TOPOLOGY".to_string()], &caps);
        assert!(result.active.contains(&"SCENE_TOPOLOGY".to_string()));
        assert!(result.denied.is_empty());
    }

    #[test]
    fn test_scene_topology_denied_without_capability() {
        let result = filter_subscriptions(&["SCENE_TOPOLOGY".to_string()], &[]);
        assert!(!result.active.contains(&"SCENE_TOPOLOGY".to_string()));
        assert!(result.denied.contains(&"SCENE_TOPOLOGY".to_string()));
    }

    #[test]
    fn test_input_events_granted_with_capability() {
        let caps = vec!["access_input_events".to_string()];
        let result = filter_subscriptions(&["INPUT_EVENTS".to_string()], &caps);
        assert!(result.active.contains(&"INPUT_EVENTS".to_string()));
        assert!(result.denied.is_empty());
    }

    #[test]
    fn test_focus_events_granted_with_access_input_events_capability() {
        let caps = vec!["access_input_events".to_string()];
        let result = filter_subscriptions(&["FOCUS_EVENTS".to_string()], &caps);
        assert!(result.active.contains(&"FOCUS_EVENTS".to_string()));
        assert!(result.denied.is_empty());
    }

    #[test]
    fn test_input_events_denied_without_capability() {
        // WHEN agent requests INPUT_EVENTS without access_input_events capability
        // THEN subscription denied, listed in denied_subscriptions (spec lines 455-457)
        let result = filter_subscriptions(&["INPUT_EVENTS".to_string()], &[]);
        assert!(!result.active.contains(&"INPUT_EVENTS".to_string()));
        assert!(result.denied.contains(&"INPUT_EVENTS".to_string()));
    }

    #[test]
    fn test_telemetry_frames_granted_with_read_telemetry() {
        let caps = vec!["read_telemetry".to_string()];
        let result = filter_subscriptions(&["TELEMETRY_FRAMES".to_string()], &caps);
        assert!(result.active.contains(&"TELEMETRY_FRAMES".to_string()));
        assert!(result.denied.is_empty());
    }

    #[test]
    fn test_telemetry_frames_denied_without_capability() {
        let result = filter_subscriptions(&["TELEMETRY_FRAMES".to_string()], &[]);
        assert!(!result.active.contains(&"TELEMETRY_FRAMES".to_string()));
        assert!(result.denied.contains(&"TELEMETRY_FRAMES".to_string()));
    }

    // ─── EventBatch variant filtering tests ──────────────────────────────────

    #[test]
    fn test_focus_events_filtered_when_no_focus_subscription() {
        // WHEN agent subscribed to input_events but not focus_events
        // THEN agent receives pointer/key but NOT focus events (spec lines 481-483)
        let subs = vec![category::INPUT_EVENTS.to_string()];

        let batch = EventBatch {
            frame_number: 1,
            batch_ts_us: 0,
            events: vec![
                // pointer down — input variant
                InputEnvelope {
                    event: Some(input_envelope::Event::PointerDown(
                        crate::proto::PointerDownEvent {
                            tile_id: vec![0u8; 16],
                            ..Default::default()
                        },
                    )),
                },
                // focus gained — focus variant
                InputEnvelope {
                    event: Some(input_envelope::Event::FocusGained(
                        crate::proto::FocusGainedEvent {
                            tile_id: vec![0u8; 16],
                            ..Default::default()
                        },
                    )),
                },
                // key down — input variant
                InputEnvelope {
                    event: Some(input_envelope::Event::KeyDown(crate::proto::KeyDownEvent {
                        tile_id: vec![0u8; 16],
                        ..Default::default()
                    })),
                },
            ],
        };

        let filtered = filter_event_batch(batch, &subs).expect("batch should not be empty");
        assert_eq!(
            filtered.events.len(),
            2,
            "focus event should be filtered out"
        );
        // Verify ordering preserved: pointer_down, key_down
        assert!(matches!(
            &filtered.events[0].event,
            Some(input_envelope::Event::PointerDown(_))
        ));
        assert!(matches!(
            &filtered.events[1].event,
            Some(input_envelope::Event::KeyDown(_))
        ));
    }

    #[test]
    fn test_input_events_filtered_when_no_input_subscription() {
        let subs = vec![category::FOCUS_EVENTS.to_string()];

        let batch = EventBatch {
            frame_number: 1,
            batch_ts_us: 0,
            events: vec![
                InputEnvelope {
                    event: Some(input_envelope::Event::PointerDown(
                        crate::proto::PointerDownEvent::default(),
                    )),
                },
                InputEnvelope {
                    event: Some(input_envelope::Event::FocusGained(
                        crate::proto::FocusGainedEvent::default(),
                    )),
                },
            ],
        };

        let filtered = filter_event_batch(batch, &subs).expect("batch should not be empty");
        assert_eq!(
            filtered.events.len(),
            1,
            "pointer event should be filtered out"
        );
        assert!(matches!(
            &filtered.events[0].event,
            Some(input_envelope::Event::FocusGained(_))
        ));
    }

    #[test]
    fn test_both_subscriptions_delivers_all_variants() {
        let subs = vec![
            category::INPUT_EVENTS.to_string(),
            category::FOCUS_EVENTS.to_string(),
        ];

        let batch = EventBatch {
            frame_number: 1,
            batch_ts_us: 0,
            events: vec![
                InputEnvelope {
                    event: Some(input_envelope::Event::PointerDown(
                        crate::proto::PointerDownEvent::default(),
                    )),
                },
                InputEnvelope {
                    event: Some(input_envelope::Event::FocusGained(
                        crate::proto::FocusGainedEvent::default(),
                    )),
                },
            ],
        };

        let filtered = filter_event_batch(batch, &subs).expect("batch should not be empty");
        assert_eq!(filtered.events.len(), 2);
    }

    #[test]
    fn test_no_subscription_batch_dropped() {
        // Agent subscribed to neither input_events nor focus_events — batch not delivered
        let subs = vec![category::SCENE_TOPOLOGY.to_string()];

        let batch = EventBatch {
            frame_number: 1,
            batch_ts_us: 0,
            events: vec![InputEnvelope {
                event: Some(input_envelope::Event::PointerDown(
                    crate::proto::PointerDownEvent::default(),
                )),
            }],
        };

        assert!(filter_event_batch(batch, &subs).is_none());
    }

    #[test]
    fn test_empty_batch_after_filtering_returns_none() {
        // Agent subscribed only to input_events, batch has only focus events
        let subs = vec![category::INPUT_EVENTS.to_string()];

        let batch = EventBatch {
            frame_number: 1,
            batch_ts_us: 0,
            events: vec![InputEnvelope {
                event: Some(input_envelope::Event::FocusGained(
                    crate::proto::FocusGainedEvent::default(),
                )),
            }],
        };

        assert!(
            filter_event_batch(batch, &subs).is_none(),
            "empty filtered batch should not be delivered"
        );
    }

    #[test]
    fn test_capture_released_is_focus_variant() {
        let env = InputEnvelope {
            event: Some(input_envelope::Event::CaptureReleased(
                crate::proto::CaptureReleasedEvent::default(),
            )),
        };
        assert!(is_focus_variant(&env));
    }

    #[test]
    fn test_pointer_move_is_input_variant() {
        let env = InputEnvelope {
            event: Some(input_envelope::Event::PointerMove(
                crate::proto::PointerMoveEvent::default(),
            )),
        };
        assert!(is_input_variant(&env));
        assert!(!is_focus_variant(&env));
    }
}
