//! Event bus — four-stage pipeline for scene event classification,
//! policy filtering, and delivery fan-out (RFC 0010 §8.1).
//!
//! ## Four-Stage Pipeline
//!
//! ```text
//! (1) Emission      — event created by runtime, agent, or system
//! (2) Classification — assign InterruptionClass (< 5 µs)
//! (3) Policy Filtering — evaluate subscriptions, self-suppression, audit visibility
//! (4) Delivery fan-out — enqueue on each subscriber's outbound stream (< 100 µs per subscriber)
//! ```
//!
//! ## Audit Event Visibility
//!
//! Shell-state audit events (safe mode, freeze, override) are CRITICAL system
//! events. They are logged but NOT delivered to agents. Agents receive downstream
//! effects (LeaseSuspendedEvent, LeaseResumedEvent, DegradationLevelChanged)
//! but not raw override commands.
//!
//! ## Rate Cap
//!
//! Maximum aggregate event rate: 1000 events/second across all sources.
//! Above this, backpressure or shedding is applied.

pub mod coalesce;
pub mod dedup;
pub mod suppression;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::subscriptions::SubscriptionRegistry;
use coalesce::{CoalesceBuffer, coalesce_key_for};
use suppression::is_suppressed;

// ─── Interruption class ───────────────────────────────────────────────────────

/// Interruption classification per RFC 0010 §3.1.
///
/// Lower numeric value = higher urgency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum InterruptionClass {
    /// Highest urgency. Runtime-only — agents cannot emit CRITICAL events directly.
    Critical = 0,
    /// Urgent.
    High = 1,
    /// Standard.
    Normal = 2,
    /// Low urgency.
    Low = 3,
    /// Never interrupts.
    Silent = 4,
}

impl InterruptionClass {
    /// Return the more restrictive (higher numeric value) of two classes.
    pub fn ceiling(self, other: InterruptionClass) -> InterruptionClass {
        if self as u8 > other as u8 {
            self
        } else {
            other
        }
    }
}

// ─── Classified event ─────────────────────────────────────────────────────────

/// A scene event after Stage 2 classification.
#[derive(Clone, Debug)]
pub struct ClassifiedEvent {
    /// Dotted event type string (e.g., "scene.tile.created").
    pub event_type: String,
    /// Effective interruption class (after ceiling enforcement).
    pub class: InterruptionClass,
    /// Namespace of the agent (or MutationBatch) that caused this event.
    /// Empty for runtime-internal events with no agent cause.
    pub source_namespace: String,
    /// Optional entity ID used for coalescing (e.g., tile_id for TileUpdated).
    pub entity_id: Option<String>,
    /// Opaque event payload bytes (proto-serialized or otherwise).
    pub payload: Vec<u8>,
}

// ─── Audit event block list ───────────────────────────────────────────────────

/// Event types that must NEVER be delivered to agents (RFC 0010 §10.1, §10.2).
///
/// These shell-state audit events are CRITICAL system events. They are logged
/// but blocked from agent delivery. Agents receive downstream effects instead.
const BLOCKED_AUDIT_EVENTS: &[&str] = &[
    "system.safe_mode_entered",
    "system.safe_mode_exited",
    "system.freeze_entered",
    "system.freeze_exited",
    "system.dismiss",
    "system.mute",
    "system.override_command",
];

/// Returns `true` if the event type is a blocked audit event that must never
/// reach agents.
#[inline]
fn is_blocked_audit_event(event_type: &str) -> bool {
    BLOCKED_AUDIT_EVENTS.contains(&event_type)
}

// ─── Rate limiter ─────────────────────────────────────────────────────────────

/// Aggregate event rate cap state (spec line 296-302).
///
/// Tracks events in a sliding 1-second window to enforce the 1000 events/s cap.
#[derive(Debug)]
pub struct AggregateRateLimiter {
    /// Rolling count of events in the current window.
    count: u64,
    /// Start of the current 1-second window.
    window_start: Instant,
    /// Aggregate rate cap (events/second).
    cap: u64,
    /// Number of events shed so far in the current window (for telemetry).
    shed_count: AtomicU64,
}

/// Maximum aggregate event rate (spec line 296).
pub const AGGREGATE_RATE_CAP: u64 = 1_000;

impl AggregateRateLimiter {
    pub fn new() -> Self {
        Self {
            count: 0,
            window_start: Instant::now(),
            cap: AGGREGATE_RATE_CAP,
            shed_count: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub fn with_cap(cap: u64) -> Self {
        Self {
            count: 0,
            window_start: Instant::now(),
            cap,
            shed_count: AtomicU64::new(0),
        }
    }

    /// Check and record an event. Returns `true` if the event is within budget
    /// (should be processed), `false` if it should be shed (backpressure).
    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.window_start);
        if elapsed.as_secs() >= 1 {
            // New window
            self.count = 0;
            self.window_start = now;
        }
        self.count += 1;
        if self.count <= self.cap {
            true
        } else {
            self.shed_count.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// Number of events shed in the current window.
    pub fn shed_count(&self) -> u64 {
        self.shed_count.load(Ordering::Relaxed)
    }
}

impl Default for AggregateRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Per-subscriber delivery queue ────────────────────────────────────────────

/// Per-subscriber outbound event queue with coalescing support.
#[derive(Debug)]
pub struct SubscriberQueue {
    /// Coalesce buffer for events under backpressure.
    pub buffer: CoalesceBuffer<ClassifiedEvent>,
    /// True when the subscriber is under backpressure (buffer above threshold).
    pub backpressure: bool,
}

impl SubscriberQueue {
    pub fn new() -> Self {
        Self {
            buffer: CoalesceBuffer::new(),
            backpressure: false,
        }
    }

    /// Enqueue an event, applying coalescing only when under backpressure.
    ///
    /// When `under_backpressure` is `false`, events are appended in FIFO order.
    /// When `under_backpressure` is `true`, events with the same coalesce key
    /// are collapsed to the latest (spec line 220-232).
    pub fn enqueue(&mut self, event: ClassifiedEvent, under_backpressure: bool) {
        self.backpressure = under_backpressure;
        let key = coalesce_key_for(&event.event_type, event.entity_id.as_deref());
        self.buffer.push(key, event, under_backpressure);
    }

    /// Drain all queued events, returning them in delivery order.
    pub fn drain(&mut self) -> Vec<ClassifiedEvent> {
        self.buffer.drain()
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

impl Default for SubscriberQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Event bus ────────────────────────────────────────────────────────────────

/// The four-stage event bus.
///
/// Holds the subscription registry, per-subscriber queues, and the aggregate
/// rate limiter. One `EventBus` instance lives in the runtime and is accessed
/// from the compositor thread.
#[derive(Debug)]
pub struct EventBus {
    /// Subscription registry: who is subscribed to what.
    pub subscriptions: SubscriptionRegistry,
    /// Per-subscriber coalescing queues keyed by agent namespace.
    subscriber_queues: HashMap<String, SubscriberQueue>,
    /// Aggregate rate limiter.
    rate_limiter: AggregateRateLimiter,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            subscriptions: SubscriptionRegistry::new(),
            subscriber_queues: HashMap::new(),
            rate_limiter: AggregateRateLimiter::new(),
        }
    }

    /// Register a new agent. Initializes mandatory subscriptions and an
    /// empty subscriber queue.
    pub fn register_agent(&mut self, namespace: &str) {
        self.subscriptions.register(namespace);
        self.subscriber_queues
            .entry(namespace.to_string())
            .or_default();
    }

    /// Remove an agent on disconnect.
    pub fn remove_agent(&mut self, namespace: &str) {
        self.subscriptions.remove(namespace);
        self.subscriber_queues.remove(namespace);
    }

    // ── Stage 2: Classification ───────────────────────────────────────────────

    /// Classify an event (Stage 2): assign an effective InterruptionClass.
    ///
    /// Rules:
    /// - Agents cannot emit CRITICAL class; if requested, downgrade to HIGH.
    /// - Zone ceiling enforcement: apply `ceiling` against zone's ceiling class.
    ///
    /// This method is intentionally branch-minimal for the < 5 µs budget.
    #[inline]
    pub fn classify(
        declared_class: InterruptionClass,
        zone_ceiling: InterruptionClass,
        is_agent_event: bool,
    ) -> InterruptionClass {
        // Agents cannot emit CRITICAL events (spec §3.1: only runtime can emit CRITICAL).
        let effective = if is_agent_event && declared_class == InterruptionClass::Critical {
            InterruptionClass::High
        } else {
            declared_class
        };
        // Apply zone ceiling: more restrictive wins.
        effective.ceiling(zone_ceiling)
    }

    // ── Stage 3: Policy filtering ─────────────────────────────────────────────

    /// Stage 3 policy filter: determines if an event should proceed to delivery.
    ///
    /// Checks:
    /// 1. Aggregate rate cap.
    /// 2. Audit event block list.
    ///
    /// Returns `true` if the event should proceed to Stage 4.
    pub fn policy_filter(&mut self, event: &ClassifiedEvent) -> bool {
        // Rate cap check. Lease and degradation events bypass the rate cap
        // regardless of their InterruptionClass — the spec mandates they are
        // never dropped (spec lines 229-231, 264-265). CRITICAL-class events
        // also bypass since they represent urgent runtime conditions.
        if !self.rate_limiter.allow()
            && event.class != InterruptionClass::Critical
            && !event.event_type.starts_with("system.lease_")
            && !event.event_type.starts_with("system.degradation_")
        {
            return false;
        }

        // Blocked audit events are never delivered to agents.
        if is_blocked_audit_event(&event.event_type) {
            return false;
        }

        true
    }

    // ── Stage 4: Delivery fan-out ─────────────────────────────────────────────

    /// Stage 4: Fan out the event to all matching subscribers.
    ///
    /// For each registered agent:
    /// 1. Check subscription filter (via `SubscriptionRegistry`).
    /// 2. Apply self-event suppression.
    /// 3. Deduplicate dual-route events (ZoneOccupancyChanged).
    /// 4. Enqueue in the subscriber's coalesce buffer.
    ///
    /// Returns the list of namespaces that received the event.
    pub fn deliver(&mut self, event: ClassifiedEvent) -> Vec<String> {
        let mut delivered_to = Vec::new();

        // Collect namespaces upfront to avoid borrowing issues
        let namespaces: Vec<String> = self.subscriber_queues.keys().cloned().collect();

        for namespace in &namespaces {
            // Subscription filter (Stage 4 / RFC 0010 §7)
            let subs = match self.subscriptions.get(namespace) {
                Some(s) => s,
                None => continue,
            };

            if !subs.should_receive(&event.event_type) {
                continue;
            }

            // Self-event suppression (Stage 4 / RFC 0010 §14.2)
            if is_suppressed(&event.event_type, &event.source_namespace, namespace) {
                continue;
            }

            // Dual-route deduplication note: ZoneOccupancyChanged matches both
            // SCENE_TOPOLOGY and ZONE_EVENTS. Because this fan-out iterates
            // agents (not categories), each namespace appears at most once per
            // event — `should_receive` already collapses the multi-category match
            // to a single boolean. No per-loop DeliveryDedup is needed here;
            // a fresh DeliveryDedup instance created inside the loop would be a
            // no-op (always allows on first call). DeliveryDedup is useful in
            // category-based fan-out layouts; see dedup.rs for details.

            // Enqueue in the subscriber's coalesce buffer
            let queue = self.subscriber_queues.entry(namespace.clone()).or_default();
            let under_backpressure = queue.len() >= coalesce::COALESCE_BUFFER_CAPACITY / 2;
            queue.enqueue(event.clone(), under_backpressure);
            delivered_to.push(namespace.clone());
        }

        delivered_to
    }

    /// Full pipeline: emit → classify → policy filter → deliver.
    ///
    /// This is the main entry point for the event bus. Calls stages 2–4
    /// in order and returns the list of namespaces that received the event.
    ///
    /// `zone_ceiling` — the InterruptionClass ceiling of the zone this event
    /// originated from. Pass `InterruptionClass::Critical` for unconstrained.
    pub fn emit(
        &mut self,
        event_type: impl Into<String>,
        declared_class: InterruptionClass,
        zone_ceiling: InterruptionClass,
        source_namespace: impl Into<String>,
        entity_id: Option<String>,
        payload: Vec<u8>,
    ) -> Vec<String> {
        let event_type = event_type.into();
        let source_namespace = source_namespace.into();

        // Stage 2: Classification
        let is_agent_event = event_type.starts_with("agent.");
        let effective_class = Self::classify(declared_class, zone_ceiling, is_agent_event);

        let event = ClassifiedEvent {
            event_type,
            class: effective_class,
            source_namespace,
            entity_id,
            payload,
        };

        // Stage 3: Policy filtering
        if !self.policy_filter(&event) {
            return vec![];
        }

        // Stage 4: Delivery fan-out
        self.deliver(event)
    }

    /// Drain all queued events for a subscriber.
    ///
    /// Called by the session server when it wants to flush events to the wire.
    pub fn drain_subscriber(&mut self, namespace: &str) -> Vec<ClassifiedEvent> {
        if let Some(queue) = self.subscriber_queues.get_mut(namespace) {
            queue.drain()
        } else {
            vec![]
        }
    }

    /// Returns the number of events currently queued for a subscriber.
    pub fn pending_count(&self, namespace: &str) -> usize {
        self.subscriber_queues
            .get(namespace)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    /// Access the rate limiter for diagnostics/testing.
    pub fn rate_limiter(&self) -> &AggregateRateLimiter {
        &self.rate_limiter
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::subscriptions::{CATEGORY_DEGRADATION_NOTICES, CATEGORY_LEASE_CHANGES};
    use crate::subscriptions::{CATEGORY_SCENE_TOPOLOGY, CATEGORY_ZONE_EVENTS};

    fn make_bus_with_agents(agents: &[&str]) -> EventBus {
        let mut bus = EventBus::new();
        for agent in agents {
            bus.register_agent(agent);
        }
        bus
    }

    fn subscribe(bus: &mut EventBus, namespace: &str, categories: &[&str]) {
        let subscribe: Vec<(String, Option<String>)> =
            categories.iter().map(|c| (c.to_string(), None)).collect();
        bus.subscriptions
            .apply_change(namespace, &subscribe, &[], &[]);
    }

    // ── Stage 2: Classification ───────────────────────────────────────────────

    #[test]
    fn test_classification_agent_cannot_emit_critical() {
        let class = EventBus::classify(
            InterruptionClass::Critical,
            InterruptionClass::Critical, // no zone ceiling
            true,                        // is agent event
        );
        assert_eq!(class, InterruptionClass::High);
    }

    #[test]
    fn test_classification_zone_ceiling_applied() {
        let class = EventBus::classify(
            InterruptionClass::High,
            InterruptionClass::Normal, // zone ceiling is NORMAL
            true,
        );
        assert_eq!(class, InterruptionClass::Normal);
    }

    #[test]
    fn test_classification_runtime_can_emit_critical() {
        let class = EventBus::classify(
            InterruptionClass::Critical,
            InterruptionClass::Critical,
            false, // runtime event
        );
        assert_eq!(class, InterruptionClass::Critical);
    }

    #[test]
    fn test_ceiling_more_restrictive_wins() {
        // Agent declares NORMAL, zone ceiling is LOW → effective is LOW (more restrictive)
        let class = EventBus::classify(InterruptionClass::Normal, InterruptionClass::Low, false);
        assert_eq!(class, InterruptionClass::Low);
    }

    // ── Stage 3: Policy filtering ─────────────────────────────────────────────

    #[test]
    fn test_audit_event_blocked() {
        let mut bus = EventBus::new();
        let event = ClassifiedEvent {
            event_type: "system.safe_mode_entered".to_string(),
            class: InterruptionClass::Critical,
            source_namespace: String::new(),
            entity_id: None,
            payload: vec![],
        };
        assert!(!bus.policy_filter(&event));
    }

    #[test]
    fn test_freeze_event_blocked() {
        let mut bus = EventBus::new();
        let event = ClassifiedEvent {
            event_type: "system.freeze_entered".to_string(),
            class: InterruptionClass::Critical,
            source_namespace: String::new(),
            entity_id: None,
            payload: vec![],
        };
        assert!(!bus.policy_filter(&event));
    }

    #[test]
    fn test_normal_event_passes_policy() {
        let mut bus = EventBus::new();
        let event = ClassifiedEvent {
            event_type: "scene.tile.created".to_string(),
            class: InterruptionClass::Normal,
            source_namespace: "agent_a".to_string(),
            entity_id: None,
            payload: vec![],
        };
        assert!(bus.policy_filter(&event));
    }

    // ── Stage 4: Delivery (subscription filtering) ────────────────────────────

    #[test]
    fn test_unsubscribed_agent_does_not_receive_scene_event() {
        let mut bus = make_bus_with_agents(&["agent_a"]);
        // agent_a has no SCENE_TOPOLOGY subscription

        let recipients = bus.emit(
            "scene.tile.created",
            InterruptionClass::Normal,
            InterruptionClass::Critical,
            "agent_b", // caused by agent_b
            None,
            vec![],
        );
        assert!(!recipients.contains(&"agent_a".to_string()));
    }

    #[test]
    fn test_subscribed_agent_receives_scene_event() {
        let mut bus = make_bus_with_agents(&["agent_a"]);
        subscribe(&mut bus, "agent_a", &[CATEGORY_SCENE_TOPOLOGY]);

        let recipients = bus.emit(
            "scene.tile.created",
            InterruptionClass::Normal,
            InterruptionClass::Critical,
            "agent_b",
            None,
            vec![],
        );
        assert!(recipients.contains(&"agent_a".to_string()));
    }

    // ── Self-event suppression ────────────────────────────────────────────────

    #[test]
    fn test_self_event_not_delivered_to_source_agent() {
        let mut bus = make_bus_with_agents(&["agent_a", "agent_b"]);
        subscribe(&mut bus, "agent_a", &[CATEGORY_SCENE_TOPOLOGY]);
        subscribe(&mut bus, "agent_b", &[CATEGORY_SCENE_TOPOLOGY]);

        // agent_a causes TileCreated
        let recipients = bus.emit(
            "scene.tile.created",
            InterruptionClass::Normal,
            InterruptionClass::Critical,
            "agent_a", // source
            None,
            vec![],
        );
        // agent_b should receive it, agent_a should NOT
        assert!(recipients.contains(&"agent_b".to_string()));
        assert!(!recipients.contains(&"agent_a".to_string()));
    }

    #[test]
    fn test_lease_event_delivered_to_self_despite_suppression() {
        let mut bus = make_bus_with_agents(&["agent_a"]);
        // LEASE_CHANGES is always subscribed (mandatory)

        // agent_a's budget violation triggers lease revocation for agent_a
        let recipients = bus.emit(
            "system.lease_revoked",
            InterruptionClass::Critical,
            InterruptionClass::Critical,
            "agent_a", // source (own budget violation)
            None,
            vec![],
        );
        // agent_a MUST receive the lease event even though it's the source
        assert!(recipients.contains(&"agent_a".to_string()));
    }

    #[test]
    fn test_degradation_event_delivered_to_self() {
        let mut bus = make_bus_with_agents(&["agent_a"]);

        let recipients = bus.emit(
            "system.degradation_changed",
            InterruptionClass::High,
            InterruptionClass::Critical,
            "agent_a",
            None,
            vec![],
        );
        assert!(recipients.contains(&"agent_a".to_string()));
    }

    // ── ZoneOccupancyChanged deduplication ────────────────────────────────────

    #[test]
    fn test_zone_occupancy_delivered_once_with_both_subscriptions() {
        let mut bus = make_bus_with_agents(&["agent_a"]);
        subscribe(
            &mut bus,
            "agent_a",
            &[CATEGORY_SCENE_TOPOLOGY, CATEGORY_ZONE_EVENTS],
        );

        // ZoneOccupancyChanged matches both categories
        let recipients = bus.emit(
            "scene.zone.occupancy_changed",
            InterruptionClass::Normal,
            InterruptionClass::Critical,
            "agent_b",
            Some("main_zone".to_string()),
            vec![],
        );

        // agent_a should appear exactly once in recipients
        let count = recipients
            .iter()
            .filter(|r| r.as_str() == "agent_a")
            .count();
        assert_eq!(
            count, 1,
            "ZoneOccupancyChanged must be delivered exactly once"
        );

        // And there should be exactly one event in the queue
        let drained = bus.drain_subscriber("agent_a");
        assert_eq!(drained.len(), 1);
    }

    // ── Mandatory event delivery ──────────────────────────────────────────────

    #[test]
    fn test_mandatory_lease_events_delivered_without_explicit_subscription() {
        let mut bus = make_bus_with_agents(&["agent_a"]);
        // No explicit subscription — mandatory categories are always active

        let recipients = bus.emit(
            "system.lease_revoked",
            InterruptionClass::Critical,
            InterruptionClass::Critical,
            "",
            None,
            vec![],
        );
        assert!(recipients.contains(&"agent_a".to_string()));
    }

    #[test]
    fn test_mandatory_degradation_events_delivered() {
        let mut bus = make_bus_with_agents(&["agent_a"]);

        let recipients = bus.emit(
            "system.degradation_changed",
            InterruptionClass::High,
            InterruptionClass::Critical,
            "",
            None,
            vec![],
        );
        assert!(recipients.contains(&"agent_a".to_string()));
    }

    // ── Rate cap ──────────────────────────────────────────────────────────────

    #[test]
    fn test_aggregate_rate_cap_enforced() {
        let mut bus = make_bus_with_agents(&["agent_a"]);
        subscribe(&mut bus, "agent_a", &[CATEGORY_SCENE_TOPOLOGY]);

        // Override the rate limiter with a small cap for testing
        bus.rate_limiter = AggregateRateLimiter::with_cap(5);

        let mut delivered = 0u32;
        for _ in 0..10 {
            let r = bus.emit(
                "scene.tile.created",
                InterruptionClass::Normal,
                InterruptionClass::Critical,
                "agent_b",
                None,
                vec![],
            );
            if !r.is_empty() {
                delivered += 1;
            }
        }

        // At most 5 should be delivered (cap = 5)
        assert!(delivered <= 5, "Expected <= 5 delivered, got {delivered}");
        assert!(bus.rate_limiter.shed_count() >= 5);
    }

    #[test]
    fn test_lease_events_bypass_rate_cap() {
        let mut bus = make_bus_with_agents(&["agent_a"]);
        // Mandatory LEASE_CHANGES subscription is auto-added

        bus.rate_limiter = AggregateRateLimiter::with_cap(0); // reject everything

        // Even with cap=0, lease events must be delivered
        // NOTE: The rate limiter increments before the bypass check; lease events
        // bypass the shedding decision but still count in the window.
        let recipients = bus.emit(
            "system.lease_revoked",
            InterruptionClass::Critical,
            InterruptionClass::Critical,
            "",
            None,
            vec![],
        );
        assert!(recipients.contains(&"agent_a".to_string()));
    }

    // ── Coalescing under backpressure ─────────────────────────────────────────

    #[test]
    fn test_tile_updates_not_coalesced_below_backpressure_threshold() {
        // Coalescing only occurs when queue.len() >= COALESCE_BUFFER_CAPACITY / 2.
        // With 5 events in the queue (far below threshold=32), no coalescing happens.
        let mut bus = make_bus_with_agents(&["agent_a"]);
        subscribe(&mut bus, "agent_a", &[CATEGORY_SCENE_TOPOLOGY]);

        for i in 0..5u8 {
            bus.emit(
                "scene.tile.updated",
                InterruptionClass::Normal,
                InterruptionClass::Critical,
                "agent_b",
                Some("tile-1".to_string()),
                vec![i],
            );
        }

        let drained = bus.drain_subscriber("agent_a");
        // All 5 events retained (FIFO, no coalescing)
        assert_eq!(drained.len(), 5);
    }

    #[test]
    fn test_tile_updates_coalesced_under_backpressure() {
        let mut bus = make_bus_with_agents(&["agent_a"]);
        subscribe(&mut bus, "agent_a", &[CATEGORY_SCENE_TOPOLOGY]);

        // First, fill the queue past the backpressure threshold (COALESCE_BUFFER_CAPACITY / 2 = 32).
        // Use distinct tile IDs so they don't coalesce with each other.
        let threshold = coalesce::COALESCE_BUFFER_CAPACITY / 2; // 32
        for i in 0..threshold {
            bus.emit(
                "scene.tile.updated",
                InterruptionClass::Normal,
                InterruptionClass::Critical,
                "agent_b",
                Some(format!("seed-tile-{i}")),
                vec![0],
            );
        }
        // Queue is now at threshold; backpressure is active.

        // Emit 5 more TileUpdated events for the same tile — should coalesce to 1.
        for i in 0..5u8 {
            bus.emit(
                "scene.tile.updated",
                InterruptionClass::Normal,
                InterruptionClass::Critical,
                "agent_b",
                Some("tile-1".to_string()),
                vec![i],
            );
        }

        let drained = bus.drain_subscriber("agent_a");
        // The 5 TileUpdated events for "tile-1" are coalesced to 1 (latest-wins).
        let tile1_events: Vec<_> = drained
            .iter()
            .filter(|e| e.entity_id.as_deref() == Some("tile-1"))
            .collect();
        assert_eq!(
            tile1_events.len(),
            1,
            "TileUpdated for tile-1 must coalesce to 1 under backpressure"
        );
        assert_eq!(
            tile1_events[0].payload,
            vec![4],
            "Latest payload must be retained"
        );
    }

    #[test]
    fn test_lease_events_not_dropped_under_backpressure() {
        let mut bus = make_bus_with_agents(&["agent_a"]);

        // Pack the buffer full of coalesable events
        subscribe(&mut bus, "agent_a", &[CATEGORY_SCENE_TOPOLOGY]);
        for i in 0..coalesce::COALESCE_BUFFER_CAPACITY as u8 {
            bus.emit(
                "scene.tile.updated",
                InterruptionClass::Normal,
                InterruptionClass::Critical,
                "agent_b",
                Some(format!("tile-{i}")),
                vec![i],
            );
        }

        // Now push a lease event — must not be dropped
        bus.emit(
            "system.lease_revoked",
            InterruptionClass::Critical,
            InterruptionClass::Critical,
            "",
            None,
            vec![42],
        );

        let drained = bus.drain_subscriber("agent_a");
        let lease_events: Vec<_> = drained
            .iter()
            .filter(|e| e.event_type == "system.lease_revoked")
            .collect();
        assert_eq!(
            lease_events.len(),
            1,
            "Lease event must not be dropped under backpressure"
        );
    }

    // ── Four-stage pipeline ordering ─────────────────────────────────────────

    #[test]
    fn test_pipeline_stage_ordering_audit_blocked_before_delivery() {
        let mut bus = make_bus_with_agents(&["agent_a"]);
        // Even with mandatory subscription, audit events never reach agents
        let recipients = bus.emit(
            "system.safe_mode_entered",
            InterruptionClass::Critical,
            InterruptionClass::Critical,
            "",
            None,
            vec![],
        );
        assert!(
            recipients.is_empty(),
            "Audit events must never be delivered to agents"
        );
    }

    #[test]
    fn test_pipeline_drain_and_queue() {
        let mut bus = make_bus_with_agents(&["agent_a"]);
        subscribe(&mut bus, "agent_a", &[CATEGORY_SCENE_TOPOLOGY]);

        bus.emit(
            "scene.tile.created",
            InterruptionClass::Normal,
            InterruptionClass::Critical,
            "agent_b",
            None,
            vec![1],
        );
        bus.emit(
            "scene.tile.deleted",
            InterruptionClass::Normal,
            InterruptionClass::Critical,
            "agent_b",
            None,
            vec![2],
        );

        assert_eq!(bus.pending_count("agent_a"), 2);

        let drained = bus.drain_subscriber("agent_a");
        assert_eq!(drained.len(), 2);
        assert_eq!(bus.pending_count("agent_a"), 0);
    }
}
