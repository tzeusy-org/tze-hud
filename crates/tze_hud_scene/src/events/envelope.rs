//! # SceneEvent Envelope
//!
//! The common envelope carried by every scene event, per
//! scene-events/spec.md §2.1, Requirement: SceneEvent Envelope Structure,
//! lines 20-32.
//!
//! Every event carries:
//! - `event_id`           — UUID v7 (time-ordered, 16 bytes)
//! - `event_type`         — namespaced dotted string (e.g. `scene.tile.created`)
//! - `interruption_class` — effective class after zone-ceiling enforcement
//! - `timestamp_wall_us`  — wall-clock microseconds (UTC)
//! - `timestamp_mono_us`  — monotonic microseconds (for ordering within session)
//! - `source_lease_id`    — the lease that produced this event; empty for system events
//! - `source_namespace`   — the agent namespace; empty for runtime events
//! - `sequence`           — monotonically increasing per-session counter for
//!   gap detection on reconnect
//! - `payload`            — typed event-specific data

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::taxonomy::SubscriptionCategory;

// ─── Interruption class ───────────────────────────────────────────────────────

/// Five-level interruption class for event attention management.
///
/// Lower numeric value = higher urgency. The runtime always applies the more
/// restrictive of the agent-declared class and the zone's ceiling class.
///
/// Spec: scene-events/spec.md lines 50-66, Requirement: Interruption Classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum InterruptionClass {
    /// Overrides everything. Bypasses quiet hours and attention budget.
    /// Only the runtime may emit CRITICAL events; agent-requested CRITICAL
    /// is downgraded to HIGH.
    Critical = 0,
    /// May override quiet hours (subject to `pass_through_class` config).
    /// Subject to attention budget. Maps to "Urgent" in privacy doctrine.
    High = 1,
    /// Standard. Filtered by attention budget.
    Normal = 2,
    /// Batched/deferred. Blocked (discarded) during quiet hours.
    /// Subtle indicators only. Maps to "Gentle" in privacy doctrine.
    Low = 3,
    /// Never interrupts. Always passes quiet hours. Zero interruption cost.
    Silent = 4,
}

impl InterruptionClass {
    /// Return the more restrictive (higher numeric value = lower urgency) of
    /// the two classes.
    ///
    /// Used to apply zone-ceiling enforcement: the effective class is
    /// `max(agent_declared, zone_ceiling)` since a higher numeric value means
    /// lower urgency.
    ///
    /// Spec: scene-events/spec.md line 57.
    pub fn ceiling(self, zone_ceiling: InterruptionClass) -> InterruptionClass {
        // Ord is derived in numeric order where Critical=0 < High=1 < ...
        // "More restrictive" means higher urgency cap → take the max.
        if self > zone_ceiling {
            self
        } else {
            zone_ceiling
        }
    }
}

// ─── Event source ─────────────────────────────────────────────────────────────

/// Identifies who produced a `SceneEvent`.
///
/// Spec: scene-events/spec.md lines 29-31.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSource {
    /// UUID bytes of the lease that produced this event.
    /// Empty (`Uuid::nil()`) for system events.
    pub lease_id: Uuid,
    /// Agent namespace string.
    /// Empty (`""`) for system/runtime events.
    pub namespace: String,
}

impl EventSource {
    /// Construct a source record for a runtime/system event.
    ///
    /// Both `lease_id` and `namespace` will be empty per spec line 31.
    pub fn system() -> Self {
        Self {
            lease_id: Uuid::nil(),
            namespace: String::new(),
        }
    }

    /// Construct a source record for an agent event.
    pub fn agent(lease_id: Uuid, namespace: impl Into<String>) -> Self {
        Self {
            lease_id,
            namespace: namespace.into(),
        }
    }

    /// Whether this represents a system (runtime-internal) event.
    pub fn is_system(&self) -> bool {
        self.lease_id.is_nil() && self.namespace.is_empty()
    }
}

// ─── Payload variants ─────────────────────────────────────────────────────────

/// Typed payload carried inside a `SceneEvent`.
///
/// The payload is a `oneof` of zone, tile, tab, agent, system, or sync_group
/// payloads per spec §2.1.  Only scene-level variants are present here;
/// the full set will be populated as beads #2–#4 land.
///
/// This crate owns the taxonomy and envelope; concrete payload types are
/// defined or re-exported as those implementation beads land.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EventPayload {
    // Scene topology events
    /// A tile was created.
    TileCreated { tile_id: Uuid, namespace: String },
    /// A tile was deleted.
    TileDeleted { tile_id: Uuid },
    /// A tile was updated (content or geometry changed).
    TileUpdated { tile_id: Uuid },
    /// A tab was created.
    TabCreated { tab_id: Uuid, name: String },
    /// The active tab changed.
    ActiveTabChanged { tab_id: Uuid },
    /// An agent joined the session.
    AgentJoined { namespace: String },

    // Zone events
    /// A zone's occupancy changed (dual-routed to both SceneTopology and ZoneEvents).
    ZoneOccupancyChanged { zone_id: Uuid, occupant_count: u32 },

    // Focus events
    /// Scene focus changed (tile or node level).
    FocusChanged {
        tile_id: Uuid,
        node_id: Option<Uuid>,
    },

    // System events
    /// The system degradation level changed.
    DegradationChanged { level: u32 },
    /// A lease was revoked.
    LeaseRevoked { lease_id: Uuid, namespace: String },
    /// A lease was granted.
    LeaseGranted { lease_id: Uuid, namespace: String },
    /// A lease was suspended.
    LeaseSuspended { lease_id: Uuid },
    /// A lease was resumed.
    LeaseResumed { lease_id: Uuid },

    // Agent events
    /// An agent-emitted event with arbitrary encoded payload (≤4 KB).
    AgentEvent {
        /// The bare name the agent supplied (e.g. `"doorbell.ring"`).
        bare_name: String,
        /// Encoded payload bytes (proto-encoded, ≤4 KB).
        payload_bytes: Vec<u8>,
    },

    // Placeholder for future variants
    /// Unstructured payload for test/prototype use only.
    Raw { data: Vec<u8> },
}

// ─── SceneEvent envelope ─────────────────────────────────────────────────────

/// The common envelope carried by every scene event.
///
/// Spec: scene-events/spec.md §2.1, lines 20-32.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneEvent {
    /// UUID v7 uniquely identifying this event instance.
    /// Time-ordered; monotonically increasing within the node clock.
    pub event_id: Uuid,

    /// Namespaced dotted event type string (e.g. `"scene.tile.created"`).
    ///
    /// Callers are responsible for ensuring this string conforms to the
    /// naming convention defined in `naming.rs` (see `validate_event_type`);
    /// `SceneEvent` itself does not validate the event_type on construction.
    pub event_type: String,

    /// Effective interruption class after zone-ceiling enforcement.
    pub interruption_class: InterruptionClass,

    /// Wall-clock time at event generation, in microseconds since Unix epoch.
    pub timestamp_wall_us: u64,

    /// Monotonic clock at event generation, in microseconds.
    /// Used for ordering within a session; not comparable across sessions.
    pub timestamp_mono_us: u64,

    /// Who produced this event.
    pub source: EventSource,

    /// Monotonically increasing sequence number within the agent's subscription
    /// session.  Gaps (e.g., 10, 11, 14) indicate dropped events and trigger a
    /// SceneSnapshot reconnect cycle.
    ///
    /// Spec: scene-events/spec.md lines 269-277.
    pub sequence: u64,

    /// The typed event data.
    pub payload: EventPayload,
}

impl SceneEvent {
    /// Returns the subscription category this event is routed to, if any.
    pub fn subscription_category(&self) -> Option<SubscriptionCategory> {
        SubscriptionCategory::for_event_type(&self.event_type)
    }

    /// Whether this is a system-generated event (no agent source).
    pub fn is_system_event(&self) -> bool {
        self.source.is_system()
    }
}

// ─── Builder ─────────────────────────────────────────────────────────────────

/// Construct a `SceneEvent` with a freshly-generated UUID v7 event_id.
///
/// The caller supplies the sequence number (from a per-session monotonic
/// counter), timestamps (wall and monotonic), and the typed payload.
pub struct SceneEventBuilder {
    event_type: String,
    interruption_class: InterruptionClass,
    timestamp_wall_us: u64,
    timestamp_mono_us: u64,
    source: EventSource,
    sequence: u64,
    payload: EventPayload,
}

impl SceneEventBuilder {
    /// Begin building a scene event.
    pub fn new(
        event_type: impl Into<String>,
        interruption_class: InterruptionClass,
        payload: EventPayload,
    ) -> Self {
        Self {
            event_type: event_type.into(),
            interruption_class,
            timestamp_wall_us: 0,
            timestamp_mono_us: 0,
            source: EventSource::system(),
            sequence: 0,
            payload,
        }
    }

    pub fn wall_us(mut self, us: u64) -> Self {
        self.timestamp_wall_us = us;
        self
    }

    pub fn mono_us(mut self, us: u64) -> Self {
        self.timestamp_mono_us = us;
        self
    }

    pub fn source(mut self, source: EventSource) -> Self {
        self.source = source;
        self
    }

    pub fn sequence(mut self, seq: u64) -> Self {
        self.sequence = seq;
        self
    }

    /// Finalize and produce a `SceneEvent` with a new UUID v7 `event_id`.
    pub fn build(self) -> SceneEvent {
        SceneEvent {
            event_id: Uuid::now_v7(),
            event_type: self.event_type,
            interruption_class: self.interruption_class,
            timestamp_wall_us: self.timestamp_wall_us,
            timestamp_mono_us: self.timestamp_mono_us,
            source: self.source,
            sequence: self.sequence,
            payload: self.payload,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── InterruptionClass ─────────────────────────────────────────────────────

    /// Verify ordering: Critical < High < Normal < Low < Silent (spec lines 50-66).
    #[test]
    fn interruption_class_ordering() {
        assert!(InterruptionClass::Critical < InterruptionClass::High);
        assert!(InterruptionClass::High < InterruptionClass::Normal);
        assert!(InterruptionClass::Normal < InterruptionClass::Low);
        assert!(InterruptionClass::Low < InterruptionClass::Silent);
    }

    /// WHEN an agent declares HIGH on a zone whose ceiling is NORMAL
    /// THEN the effective class MUST be NORMAL (spec line 57).
    #[test]
    fn zone_ceiling_enforcement() {
        let agent_declared = InterruptionClass::High;
        let zone_ceiling = InterruptionClass::Normal;
        let effective = agent_declared.ceiling(zone_ceiling);
        assert_eq!(effective, InterruptionClass::Normal);
    }

    #[test]
    fn zone_ceiling_no_downgrade_when_already_lower() {
        // Agent declares LOW (very unobtrusive), zone ceiling is HIGH — effective is LOW.
        let agent_declared = InterruptionClass::Low;
        let zone_ceiling = InterruptionClass::High;
        let effective = agent_declared.ceiling(zone_ceiling);
        assert_eq!(effective, InterruptionClass::Low);
    }

    // ── EventSource ───────────────────────────────────────────────────────────

    /// WHEN a system event is generated THEN source_lease_id MUST be empty and
    /// source_namespace MUST be empty (spec line 31).
    #[test]
    fn system_event_source_is_empty() {
        let src = EventSource::system();
        assert!(src.lease_id.is_nil(), "system event must have nil lease_id");
        assert!(
            src.namespace.is_empty(),
            "system event must have empty namespace"
        );
        assert!(src.is_system());
    }

    #[test]
    fn agent_event_source_is_not_system() {
        let lease = Uuid::now_v7();
        let src = EventSource::agent(lease, "doorbell_agent");
        assert!(!src.is_system());
        assert_eq!(src.namespace, "doorbell_agent");
        assert_eq!(src.lease_id, lease);
    }

    // ── SceneEvent envelope ───────────────────────────────────────────────────

    /// WHEN the runtime generates a TileCreated scene event THEN the SceneEvent
    /// envelope MUST contain a non-zero UUID v7 event_id, event_type
    /// "scene.tile.created", a valid InterruptionClass, both wall and monotonic
    /// timestamps, the source lease_id, the source agent namespace, and a
    /// monotonically increasing sequence number (spec line 27).
    #[test]
    fn tile_created_envelope_all_fields_populated() {
        let tile_id = Uuid::now_v7();
        let lease_id = Uuid::now_v7();

        let event = SceneEventBuilder::new(
            "scene.tile.created",
            InterruptionClass::Normal,
            EventPayload::TileCreated {
                tile_id,
                namespace: "myagent".to_string(),
            },
        )
        .wall_us(1_000_000)
        .mono_us(500_000)
        .source(EventSource::agent(lease_id, "myagent"))
        .sequence(1)
        .build();

        assert!(!event.event_id.is_nil(), "event_id must be non-zero");
        assert_eq!(event.event_type, "scene.tile.created");
        assert_eq!(event.interruption_class, InterruptionClass::Normal);
        assert_eq!(event.timestamp_wall_us, 1_000_000);
        assert_eq!(event.timestamp_mono_us, 500_000);
        assert_eq!(event.source.lease_id, lease_id);
        assert_eq!(event.source.namespace, "myagent");
        assert_eq!(event.sequence, 1);
    }

    /// WHEN a system event (e.g., degradation_changed) is generated THEN
    /// source_lease_id MUST be empty and source_namespace MUST be empty
    /// (spec line 31).
    #[test]
    fn system_event_has_empty_source_fields() {
        let event = SceneEventBuilder::new(
            "system.degradation_changed",
            InterruptionClass::Normal,
            EventPayload::DegradationChanged { level: 1 },
        )
        .wall_us(2_000_000)
        .mono_us(1_000_000)
        .sequence(5)
        .build(); // source defaults to EventSource::system()

        assert!(
            event.source.lease_id.is_nil(),
            "system event must have nil lease_id"
        );
        assert!(
            event.source.namespace.is_empty(),
            "system event must have empty namespace"
        );
        assert!(event.is_system_event());
    }

    /// UUID v7 event_ids are time-ordered (monotonically non-decreasing).
    #[test]
    fn event_ids_are_time_ordered() {
        let e1 = SceneEventBuilder::new(
            "scene.tile.created",
            InterruptionClass::Normal,
            EventPayload::TileCreated {
                tile_id: Uuid::now_v7(),
                namespace: "a".to_string(),
            },
        )
        .build();

        let e2 = SceneEventBuilder::new(
            "scene.tile.created",
            InterruptionClass::Normal,
            EventPayload::TileCreated {
                tile_id: Uuid::now_v7(),
                namespace: "a".to_string(),
            },
        )
        .build();

        assert!(
            e2.event_id >= e1.event_id,
            "UUID v7 event_ids must be time-ordered"
        );
    }

    /// Sequence numbers must be monotonically increasing per session (spec lines 269-277).
    #[test]
    fn sequence_numbers_monotonically_increasing() {
        let events: Vec<SceneEvent> = (1u64..=5)
            .map(|seq| {
                SceneEventBuilder::new(
                    "scene.tile.created",
                    InterruptionClass::Normal,
                    EventPayload::TileCreated {
                        tile_id: Uuid::now_v7(),
                        namespace: "a".to_string(),
                    },
                )
                .sequence(seq)
                .build()
            })
            .collect();

        for pair in events.windows(2) {
            assert!(
                pair[1].sequence > pair[0].sequence,
                "sequence numbers must be strictly increasing"
            );
        }
    }

    /// Gap detection helper — simulating spec line 276.
    #[test]
    fn gap_detection_in_sequence_numbers() {
        let sequences = [10u64, 11, 14]; // gap at 12-13
        let mut gaps = Vec::new();
        for pair in sequences.windows(2) {
            if pair[1] != pair[0] + 1 {
                gaps.push((pair[0], pair[1]));
            }
        }
        assert_eq!(gaps, vec![(11, 14)], "gap at 12-13 should be detected");
    }

    /// `subscription_category()` returns the correct routing for envelope event_type.
    #[test]
    fn subscription_category_routing() {
        let event = SceneEventBuilder::new(
            "scene.tile.created",
            InterruptionClass::Normal,
            EventPayload::TileCreated {
                tile_id: Uuid::now_v7(),
                namespace: "a".to_string(),
            },
        )
        .build();

        use super::super::taxonomy::SubscriptionCategory;
        assert_eq!(
            event.subscription_category(),
            Some(SubscriptionCategory::SceneTopology)
        );
    }
}
