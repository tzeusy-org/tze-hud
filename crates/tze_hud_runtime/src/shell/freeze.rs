//! # Freeze Semantics
//!
//! Implements the freeze protocol per system-shell/spec.md §Freeze Scene and
//! §Freeze Backpressure Signal (source: RFC 0007 §4.3).
//!
//! ## Overview
//!
//! The freeze action (`Ctrl+Shift+F`) pauses the scene: agent mutations are
//! queued rather than applied. On unfreeze, all queued mutations are applied in
//! submission order. Freeze is a human-override: no agent can prevent or cancel
//! it, and agents are never told the scene is frozen.
//!
//! ## Ownership
//!
//! The shell is the **sole** writer of freeze state. Only the shell state machine
//! writes `freeze_active`. No other subsystem (including policy arbitration) may
//! modify this field.
//!
//! ## Invariant
//!
//! `safe_mode = true` implies `freeze_active = false`. On safe mode activation:
//! freeze is cancelled, the freeze queue is discarded, and `freeze_active` is
//! set to `false` **before** any other safe mode entry steps execute.
//!
//! ## Queue overflow (traffic-class-aware)
//!
//! | Traffic class  | Overflow behaviour                                |
//! |---------------|---------------------------------------------------|
//! | Transactional | Never evicted; gRPC backpressure applied instead  |
//! | StateStream   | Coalesced (latest-wins) before eviction           |
//! | Ephemeral     | Dropped oldest-first                              |
//!
//! At 80% queue capacity the runtime sends `MUTATION_QUEUE_PRESSURE` via
//! `RuntimeError` in `MutationResult`. On non-transactional eviction the
//! runtime sends `MUTATION_DROPPED`.
//!
//! ## Auto-unfreeze
//!
//! The runtime auto-unfreezes after a configurable timeout (default 5 minutes).
//! Auto-unfreeze is indistinguishable from a manual unfreeze: queued mutations
//! are applied in submission order.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Default per-session mutation queue capacity while frozen (spec §Freeze Scene).
pub const DEFAULT_FREEZE_QUEUE_CAPACITY: usize = 1_000;

/// Queue pressure threshold: fire MUTATION_QUEUE_PRESSURE when the queue
/// reaches this fraction of capacity (spec §Freeze Backpressure Signal: 80%).
pub const QUEUE_PRESSURE_FRACTION: f32 = 0.80;

/// Default auto-unfreeze timeout (spec §Freeze Semantics: 5 minutes).
pub const DEFAULT_AUTO_UNFREEZE_MS: u64 = 5 * 60 * 1_000;

// ─── Traffic class ────────────────────────────────────────────────────────────

/// Traffic class for an **inbound** `MutationBatch`.
///
/// The spec (system-shell/spec.md §Freeze Scene) distinguishes three classes
/// for queue overflow:
///
/// - **Transactional** — never evicted; gRPC backpressure applied instead.
/// - **StateStream** — coalesced (latest-wins) before eviction.
/// - **Ephemeral** — dropped oldest-first.
///
/// For inbound mutations, the classification is determined by the mutation
/// operations contained in the batch:
///
/// - Any structural / identity-changing mutation (`CreateTile`, `DeleteTile`,
///   `CreateTab`, `SwitchActiveTab`, `SetPortalSurface`) → **Transactional**.
/// - Content / state mutations (`SetTileRoot`, `AddNode`, `UpdateTileBounds`,
///   `PublishToZone`, `ClearZone`, `UpdatePortalSurfaceState`) → **StateStream**.
/// - Ephemeral batches are not currently expressed at the `MutationBatch`
///   level; this variant is reserved for future use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationTrafficClass {
    /// Reliable, ordered, never evicted. gRPC backpressure on overflow.
    Transactional,
    /// Coalesced under pressure; intermediate states may be skipped.
    StateStream,
    /// Droppable under backpressure; oldest evicted first.
    Ephemeral,
}

// ─── Queued mutation entry ────────────────────────────────────────────────────

/// A single enqueued mutation batch together with its traffic class and a
/// stable coalesce key (used for StateStream coalescing).
#[derive(Clone, Debug)]
pub struct QueuedMutation {
    /// The request's `batch_id` bytes (from the proto `MutationBatch`),
    /// used for error correlation and identifying which response belongs to
    /// which request.
    pub batch_id: Vec<u8>,
    /// Preserved copy of `batch_id` for eviction/drop error reporting.
    /// Kept separate so in-place coalescing (which replaces the entry) can
    /// still reference the original batch identity.
    pub original_batch_id: Vec<u8>,
    /// Traffic class inferred at enqueue time.
    pub traffic_class: MutationTrafficClass,
    /// Coalesce key for StateStream mutations: `"<namespace>/<lease_id>"`.
    /// When two entries share the same coalesce key, the newer overwrites the
    /// older on enqueue (latest-wins, per spec).
    pub coalesce_key: Option<String>,
    /// Wall-clock time of original submission (UTC µs since epoch).
    pub submitted_at_wall_us: u64,
    /// Opaque payload: the serialised proto `MutationBatch` bytes.
    /// Callers encode before enqueue and decode on drain.
    pub payload: Vec<u8>,
}

// ─── Freeze queue ─────────────────────────────────────────────────────────────

/// Per-session bounded mutation queue for freeze semantics.
///
/// Manages queued mutations during freeze, including traffic-class-aware
/// overflow, backpressure signalling thresholds, and coalescing.
#[derive(Debug)]
pub struct FreezeQueue {
    /// Maximum number of entries. Default: [`DEFAULT_FREEZE_QUEUE_CAPACITY`].
    capacity: usize,
    /// Ordered list of queued mutations (submission order preserved).
    queue: VecDeque<QueuedMutation>,
}

impl FreezeQueue {
    /// Create a new freeze queue with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `capacity == 0`. A zero-capacity queue cannot
    /// accept any mutations and makes backpressure calculations meaningless.
    pub fn new(capacity: usize) -> Self {
        debug_assert!(capacity > 0, "FreezeQueue capacity must be > 0");
        Self {
            capacity: capacity.max(1),
            queue: VecDeque::with_capacity(capacity.min(256)),
        }
    }

    /// Create a new freeze queue with the default capacity (1000).
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_FREEZE_QUEUE_CAPACITY)
    }

    /// Current number of queued entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the queue is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Fraction of capacity used (0.0 = empty, 1.0 = full).
    #[inline]
    pub fn pressure(&self) -> f32 {
        self.queue.len() as f32 / self.capacity as f32
    }

    /// Whether the queue is at or above the pressure threshold (≥ 80%).
    #[inline]
    pub fn is_pressure_threshold_reached(&self) -> bool {
        self.pressure() >= QUEUE_PRESSURE_FRACTION
    }

    /// Whether the queue is full (at capacity).
    #[inline]
    pub fn is_full(&self) -> bool {
        self.queue.len() >= self.capacity
    }

    /// Attempt to enqueue a mutation.
    ///
    /// Returns an [`EnqueueResult`] indicating whether the mutation was
    /// queued, coalesced, required backpressure (Transactional + full), or
    /// evicted another entry (with eviction details for MUTATION_DROPPED).
    pub fn enqueue(&mut self, mutation: QueuedMutation) -> EnqueueResult {
        let was_at_pressure = self.is_pressure_threshold_reached();
        let is_full = self.is_full();

        match mutation.traffic_class {
            MutationTrafficClass::Transactional => {
                if is_full {
                    // Transactional mutations MUST NOT be evicted.
                    // Signal backpressure to the caller; the gRPC layer applies
                    // flow control (does not enqueue).
                    return EnqueueResult::BackpressureRequired {
                        batch_id: mutation.original_batch_id,
                        pressure: self.pressure(),
                    };
                }
                self.queue.push_back(mutation);
                if !was_at_pressure && self.is_pressure_threshold_reached() {
                    EnqueueResult::QueuedWithPressure
                } else {
                    EnqueueResult::Queued
                }
            }

            MutationTrafficClass::StateStream => {
                // Try coalescing first: if an existing entry with the same
                // coalesce key exists, replace it in-place (latest-wins).
                if let Some(ref key) = mutation.coalesce_key {
                    for entry in self.queue.iter_mut() {
                        if entry.traffic_class == MutationTrafficClass::StateStream
                            && entry.coalesce_key.as_deref() == Some(key.as_str())
                        {
                            // Replace payload in-place — position in queue is
                            // preserved so submission order is maintained for
                            // non-coalesced entries.
                            *entry = mutation;
                            return EnqueueResult::Coalesced;
                        }
                    }
                }

                if is_full {
                    // Evict the oldest non-Transactional entry.
                    if let Some(evicted) = self.evict_oldest_non_transactional() {
                        self.queue.push_back(mutation);
                        return EnqueueResult::EvictedEntry {
                            evicted_batch_id: evicted.original_batch_id,
                            evicted_class: evicted.traffic_class,
                        };
                    } else {
                        // Queue is full of transactional entries only — apply
                        // backpressure.
                        return EnqueueResult::BackpressureRequired {
                            batch_id: mutation.original_batch_id,
                            pressure: self.pressure(),
                        };
                    }
                }

                self.queue.push_back(mutation);
                if !was_at_pressure && self.is_pressure_threshold_reached() {
                    EnqueueResult::QueuedWithPressure
                } else {
                    EnqueueResult::Queued
                }
            }

            MutationTrafficClass::Ephemeral => {
                if is_full {
                    // Evict the oldest non-Transactional entry (which may be
                    // Ephemeral or StateStream, oldest first).
                    if let Some(evicted) = self.evict_oldest_non_transactional() {
                        self.queue.push_back(mutation);
                        return EnqueueResult::EvictedEntry {
                            evicted_batch_id: evicted.original_batch_id,
                            evicted_class: evicted.traffic_class,
                        };
                    } else {
                        // Only transactional entries remain; drop this one.
                        return EnqueueResult::Dropped {
                            batch_id: mutation.original_batch_id,
                        };
                    }
                }

                self.queue.push_back(mutation);
                if !was_at_pressure && self.is_pressure_threshold_reached() {
                    EnqueueResult::QueuedWithPressure
                } else {
                    EnqueueResult::Queued
                }
            }
        }
    }

    /// Evict the oldest non-Transactional entry from the queue.
    ///
    /// Returns the evicted entry, or `None` if all entries are Transactional.
    fn evict_oldest_non_transactional(&mut self) -> Option<QueuedMutation> {
        // Find the index of the first non-Transactional entry.
        let idx = self
            .queue
            .iter()
            .position(|e| e.traffic_class != MutationTrafficClass::Transactional)?;
        self.queue.remove(idx)
    }

    /// Drain the queue and return all entries in submission order.
    ///
    /// Called on unfreeze. The caller applies the returned batches in the
    /// returned order.
    pub fn drain(&mut self) -> Vec<QueuedMutation> {
        self.queue.drain(..).collect()
    }

    /// Discard the entire queue without applying any mutations.
    ///
    /// Called when safe mode cancels freeze.
    pub fn discard(&mut self) {
        self.queue.clear();
    }

    /// Queue capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Result of an [`FreezeQueue::enqueue`] call.
#[derive(Debug)]
pub enum EnqueueResult {
    /// Mutation queued successfully.
    Queued,
    /// Mutation queued; queue just crossed the pressure threshold (80%).
    /// Caller MUST send `MUTATION_QUEUE_PRESSURE` to the submitting agent.
    QueuedWithPressure,
    /// StateStream mutation coalesced with an existing entry (latest-wins).
    Coalesced,
    /// A non-transactional entry was evicted to make room.
    /// Caller MUST send `MUTATION_DROPPED` for the evicted batch.
    EvictedEntry {
        evicted_batch_id: Vec<u8>,
        evicted_class: MutationTrafficClass,
    },
    /// Mutation itself was dropped (Ephemeral; only transactional entries
    /// remain in queue).
    Dropped { batch_id: Vec<u8> },
    /// Queue is full and this entry is Transactional (or all slots are
    /// occupied by Transactional entries). Caller MUST apply gRPC backpressure.
    BackpressureRequired { batch_id: Vec<u8>, pressure: f32 },
}

// ─── Freeze state machine ─────────────────────────────────────────────────────

/// Runtime freeze state for a single HUD node.
///
/// The shell is the **sole** writer of this struct. The session server reads
/// `freeze_active` to decide whether to queue incoming mutations.
///
/// # Thread safety
///
/// `FreezeState` is stored inside `SharedState` which is already protected by
/// a `tokio::sync::Mutex`. No additional synchronisation is required.
#[derive(Debug)]
pub struct FreezeState {
    /// Whether the scene is currently frozen.
    pub freeze_active: bool,
    /// Wall-clock time at which freeze was activated (µs since epoch, or 0).
    pub freeze_started_wall_us: u64,
    /// Monotonic instant at which freeze was activated (or the epoch).
    freeze_started_mono: Option<Instant>,
    /// Auto-unfreeze timeout (default 5 minutes).
    pub auto_unfreeze_timeout: Duration,
}

impl Default for FreezeState {
    fn default() -> Self {
        Self::new(Duration::from_millis(DEFAULT_AUTO_UNFREEZE_MS))
    }
}

impl FreezeState {
    /// Create a new (inactive) freeze state with a custom auto-unfreeze timeout.
    pub fn new(auto_unfreeze_timeout: Duration) -> Self {
        Self {
            freeze_active: false,
            freeze_started_wall_us: 0,
            freeze_started_mono: None,
            auto_unfreeze_timeout,
        }
    }

    /// Activate freeze.
    ///
    /// No-op if already frozen.
    ///
    /// # Parameters
    /// - `now_wall_us` — current wall-clock time (UTC µs since epoch).
    /// - `now_mono` — current monotonic instant (for timeout tracking).
    pub fn activate(&mut self, now_wall_us: u64, now_mono: Instant) {
        if self.freeze_active {
            return;
        }
        self.freeze_active = true;
        self.freeze_started_wall_us = now_wall_us;
        self.freeze_started_mono = Some(now_mono);
    }

    /// Deactivate freeze (manual unfreeze or auto-unfreeze).
    ///
    /// No-op if not frozen.
    pub fn deactivate(&mut self) {
        self.freeze_active = false;
        self.freeze_started_wall_us = 0;
        self.freeze_started_mono = None;
    }

    /// Cancel freeze due to safe mode activation.
    ///
    /// Per the spec invariant: safe_mode=true implies freeze_active=false.
    /// This MUST be called before any other safe mode entry steps.
    pub fn cancel_for_safe_mode(&mut self) {
        // Deactivation is identical; the distinction is semantic (documented
        // in the invariant). freeze_queue is discarded by the caller.
        self.deactivate();
    }

    /// Returns `true` if the auto-unfreeze timeout has elapsed.
    ///
    /// Returns `false` if freeze is inactive or no mono timestamp is set.
    pub fn is_auto_unfreeze_due(&self, now_mono: Instant) -> bool {
        if !self.freeze_active {
            return false;
        }
        match self.freeze_started_mono {
            Some(started) => {
                now_mono.saturating_duration_since(started) >= self.auto_unfreeze_timeout
            }
            None => false,
        }
    }

    /// How long the scene has been frozen (or `Duration::ZERO` if not frozen).
    pub fn freeze_duration(&self, now_mono: Instant) -> Duration {
        match (self.freeze_active, self.freeze_started_mono) {
            (true, Some(started)) => now_mono.saturating_duration_since(started),
            _ => Duration::ZERO,
        }
    }
}

// ─── FreezeManager ────────────────────────────────────────────────────────────

/// High-level freeze manager: combines [`FreezeState`] with a per-session
/// [`FreezeQueue`] and provides the shell-facing API.
///
/// The session server calls [`FreezeManager::try_enqueue`] for every inbound
/// `MutationBatch` when freeze is active, and calls
/// [`FreezeManager::drain_queue`] on unfreeze to retrieve the batches to apply.
///
/// The shell calls [`FreezeManager::activate`] / [`FreezeManager::deactivate`]
/// to drive freeze transitions, and [`FreezeManager::tick`] each frame to check
/// for auto-unfreeze.
#[derive(Debug)]
pub struct FreezeManager {
    /// Freeze state machine.
    pub state: FreezeState,
    /// Bounded mutation queue for this session.
    pub queue: FreezeQueue,
}

impl FreezeManager {
    /// Create a new freeze manager with defaults.
    pub fn new() -> Self {
        Self::with_config(
            Duration::from_millis(DEFAULT_AUTO_UNFREEZE_MS),
            DEFAULT_FREEZE_QUEUE_CAPACITY,
        )
    }

    /// Create with custom timeout and queue capacity.
    pub fn with_config(auto_unfreeze_timeout: Duration, queue_capacity: usize) -> Self {
        Self {
            state: FreezeState::new(auto_unfreeze_timeout),
            queue: FreezeQueue::new(queue_capacity),
        }
    }

    /// Whether the scene is currently frozen.
    #[inline]
    pub fn is_frozen(&self) -> bool {
        self.state.freeze_active
    }

    /// Activate freeze (viewer pressed `Ctrl+Shift+F`).
    ///
    /// If safe mode is currently active, the freeze attempt is **ignored**
    /// (spec invariant: freeze ignored during safe mode).
    ///
    /// Returns `true` if freeze was activated, `false` if it was ignored.
    pub fn activate(
        &mut self,
        safe_mode_active: bool,
        now_wall_us: u64,
        now_mono: Instant,
    ) -> bool {
        if safe_mode_active {
            // Spec: "Freeze attempted during safe mode is ignored."
            return false;
        }
        self.state.activate(now_wall_us, now_mono);
        true
    }

    /// Deactivate freeze (manual unfreeze or auto-unfreeze timeout).
    ///
    /// Returns the drained queue of mutations to apply in submission order.
    pub fn deactivate(&mut self) -> Vec<QueuedMutation> {
        self.state.deactivate();
        self.queue.drain()
    }

    /// Cancel freeze due to safe mode activation.
    ///
    /// Cancels freeze and discards the queue **before** any other safe mode
    /// entry steps, preserving the spec invariant.
    pub fn cancel_for_safe_mode(&mut self) {
        self.state.cancel_for_safe_mode();
        self.queue.discard();
    }

    /// Tick: check for auto-unfreeze timeout.
    ///
    /// Returns `Some(Vec<QueuedMutation>)` if auto-unfreeze was triggered (the
    /// caller should apply the returned mutations), or `None` if nothing changed.
    pub fn tick(&mut self, now_mono: Instant) -> Option<Vec<QueuedMutation>> {
        if self.state.is_auto_unfreeze_due(now_mono) {
            Some(self.deactivate())
        } else {
            None
        }
    }

    /// Enqueue an inbound mutation batch during freeze.
    ///
    /// Returns an [`EnqueueResult`] that the session server uses to decide
    /// whether to send `MUTATION_QUEUE_PRESSURE` or `MUTATION_DROPPED`.
    pub fn try_enqueue(&mut self, mutation: QueuedMutation) -> EnqueueResult {
        self.queue.enqueue(mutation)
    }

    /// Pressure fraction of the queue (0.0 = empty, 1.0 = full).
    pub fn queue_pressure(&self) -> f32 {
        self.queue.pressure()
    }

    /// Drain all queued mutations in submission order (called on unfreeze).
    ///
    /// Unlike `deactivate`, this does NOT change freeze state. Use when you
    /// need the queue contents without state transition, e.g. for testing.
    pub fn drain_queue(&mut self) -> Vec<QueuedMutation> {
        self.queue.drain()
    }
}

impl Default for FreezeManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helper: infer traffic class from proto mutation variants ─────────────────

/// Classify a `MutationBatch` by its dominant traffic class.
///
/// The batch traffic class is the **maximum** urgency of any contained
/// mutation:
///
/// - Any Transactional mutation → entire batch is Transactional.
/// - Otherwise, any StateStream mutation → batch is StateStream.
/// - If no mutations or all are Ephemeral → Ephemeral.
///
/// This ensures a batch containing even one structural mutation is never
/// evicted (preserving the "transactional never dropped" guarantee).
///
/// # Mutation classification
///
/// The caller passes a slice of mutation kind strings. The recognised
/// transactional kinds are:
/// `"create_tile"`, `"delete_tile"`, `"create_tab"`, `"switch_active_tab"`,
/// `"set_portal_surface"`.
///
/// Everything else is classified as StateStream. This mirrors the authoritative
/// protocol-layer classifier
/// (`tze_hud_protocol::session_server::traffic::classify_inbound_batch`) for the
/// structural-vs-coalescible split: `set_portal_surface` declares the first-class
/// portal surface (identity + parts) and must never be evicted under freeze
/// pressure, while `update_portal_surface_state` is a coalescible lifecycle/display
/// patch (StateStream).
pub fn classify_mutation_batch(mutation_kinds: &[&str]) -> MutationTrafficClass {
    let mut highest = MutationTrafficClass::Ephemeral;
    for kind in mutation_kinds {
        match *kind {
            "create_tile" | "delete_tile" | "create_tab" | "switch_active_tab"
            | "set_portal_surface" => {
                // Transactional is the highest class — short-circuit.
                return MutationTrafficClass::Transactional;
            }
            "set_tile_root"
            | "add_node"
            | "update_tile_bounds"
            | "publish_to_zone"
            | "clear_zone"
            | "update_portal_surface_state" => {
                highest = MutationTrafficClass::StateStream;
            }
            _ => {
                // Unknown kinds default to StateStream (safe: won't get evicted
                // from a queue that still has room; won't cause data loss).
                if highest == MutationTrafficClass::Ephemeral {
                    highest = MutationTrafficClass::StateStream;
                }
            }
        }
    }
    highest
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helper ────────────────────────────────────────────────────────────────

    fn make_mutation(
        batch_id: &str,
        traffic_class: MutationTrafficClass,
        coalesce_key: Option<&str>,
    ) -> QueuedMutation {
        QueuedMutation {
            batch_id: batch_id.as_bytes().to_vec(),
            original_batch_id: batch_id.as_bytes().to_vec(),
            traffic_class,
            coalesce_key: coalesce_key.map(str::to_string),
            submitted_at_wall_us: 0,
            payload: batch_id.as_bytes().to_vec(),
        }
    }

    // ── FreezeQueue unit tests ─────────────────────────────────────────────────

    /// WHEN a mutation is submitted and queue is not full
    /// THEN it is queued successfully
    #[test]
    fn test_basic_enqueue() {
        let mut q = FreezeQueue::new(10);
        let m = make_mutation("b1", MutationTrafficClass::StateStream, None);
        assert!(matches!(q.enqueue(m), EnqueueResult::Queued));
        assert_eq!(q.len(), 1);
    }

    /// WHEN the queue reaches 80% capacity
    /// THEN the next enqueue returns QueuedWithPressure
    #[test]
    fn test_pressure_threshold() {
        let mut q = FreezeQueue::new(10);
        // Fill 7 entries without triggering pressure (70%)
        for i in 0..7 {
            let m = make_mutation(&format!("b{i}"), MutationTrafficClass::StateStream, None);
            assert!(matches!(q.enqueue(m), EnqueueResult::Queued));
        }
        // 8th entry crosses 80% threshold
        let m = make_mutation("b7", MutationTrafficClass::StateStream, None);
        assert!(matches!(q.enqueue(m), EnqueueResult::QueuedWithPressure));
        assert_eq!(q.len(), 8);
    }

    /// WHEN a transactional mutation is submitted and queue is full
    /// THEN backpressure is required (not evicted)
    #[test]
    fn test_transactional_never_evicted() {
        let mut q = FreezeQueue::new(3);
        // Fill queue
        for i in 0..3 {
            let m = make_mutation(&format!("ss{i}"), MutationTrafficClass::StateStream, None);
            q.enqueue(m);
        }
        assert!(q.is_full());

        let tx = make_mutation("tx", MutationTrafficClass::Transactional, None);
        let r = q.enqueue(tx);
        assert!(matches!(r, EnqueueResult::BackpressureRequired { .. }));
        // Queue is unchanged
        assert_eq!(q.len(), 3);
    }

    /// WHEN a state-stream mutation with a matching coalesce key exists
    /// THEN the new entry replaces the old one (latest-wins)
    #[test]
    fn test_state_stream_coalescing() {
        let mut q = FreezeQueue::new(10);
        let m1 = make_mutation("b1", MutationTrafficClass::StateStream, Some("ns/lease1"));
        let m2 = make_mutation("b2", MutationTrafficClass::StateStream, Some("ns/lease1"));
        q.enqueue(m1);
        let r = q.enqueue(m2);
        assert!(matches!(r, EnqueueResult::Coalesced));
        // Still one entry, but with b2's payload
        assert_eq!(q.len(), 1);
        let drained = q.drain();
        assert_eq!(drained[0].original_batch_id, b"b2");
    }

    /// WHEN queue is full and a state-stream mutation is submitted
    /// THEN oldest non-transactional is evicted and MUTATION_DROPPED is reported
    #[test]
    fn test_state_stream_eviction() {
        let mut q = FreezeQueue::new(2);
        let m1 = make_mutation("b1", MutationTrafficClass::StateStream, None);
        let m2 = make_mutation("b2", MutationTrafficClass::StateStream, None);
        q.enqueue(m1);
        q.enqueue(m2);
        assert!(q.is_full());

        let m3 = make_mutation("b3", MutationTrafficClass::StateStream, None);
        let r = q.enqueue(m3);
        assert!(matches!(r, EnqueueResult::EvictedEntry { .. }));
        if let EnqueueResult::EvictedEntry {
            evicted_batch_id, ..
        } = r
        {
            assert_eq!(evicted_batch_id, b"b1");
        }
        assert_eq!(q.len(), 2);
    }

    /// WHEN queue is full of transactional entries and a state-stream mutation arrives
    /// THEN backpressure is required (no non-transactional to evict)
    #[test]
    fn test_state_stream_backpressure_when_only_transactional() {
        let mut q = FreezeQueue::new(2);
        for i in 0..2 {
            let m = make_mutation(&format!("tx{i}"), MutationTrafficClass::Transactional, None);
            q.enqueue(m);
        }
        assert!(q.is_full());

        let ss = make_mutation("ss", MutationTrafficClass::StateStream, None);
        let r = q.enqueue(ss);
        assert!(matches!(r, EnqueueResult::BackpressureRequired { .. }));
    }

    /// WHEN queue is full and an ephemeral mutation is submitted
    /// THEN it is dropped when only transactional entries remain
    #[test]
    fn test_ephemeral_dropped() {
        let mut q = FreezeQueue::new(2);
        for i in 0..2 {
            let m = make_mutation(&format!("tx{i}"), MutationTrafficClass::Transactional, None);
            q.enqueue(m);
        }
        let eph = make_mutation("eph", MutationTrafficClass::Ephemeral, None);
        let r = q.enqueue(eph);
        assert!(matches!(r, EnqueueResult::Dropped { .. }));
    }

    /// WHEN unfreeze is triggered
    /// THEN drain returns mutations in submission order
    #[test]
    fn test_drain_order() {
        let mut q = FreezeQueue::new(10);
        for i in 0..5u8 {
            let m = make_mutation(&format!("b{i}"), MutationTrafficClass::StateStream, None);
            q.enqueue(m);
        }
        let drained = q.drain();
        assert_eq!(drained.len(), 5);
        for (i, d) in drained.iter().enumerate() {
            assert_eq!(d.original_batch_id, format!("b{i}").as_bytes());
        }
        assert!(q.is_empty());
    }

    /// WHEN safe mode cancels freeze
    /// THEN queue is discarded
    #[test]
    fn test_discard_on_safe_mode() {
        let mut q = FreezeQueue::new(10);
        for i in 0..5 {
            let m = make_mutation(&format!("b{i}"), MutationTrafficClass::StateStream, None);
            q.enqueue(m);
        }
        q.discard();
        assert!(q.is_empty());
    }

    // ── FreezeState unit tests ─────────────────────────────────────────────────

    /// WHEN freeze is activated
    /// THEN freeze_active is true
    #[test]
    fn test_freeze_state_activate() {
        let mut s = FreezeState::default();
        assert!(!s.freeze_active);
        s.activate(1_000, Instant::now());
        assert!(s.freeze_active);
    }

    /// WHEN freeze is deactivated
    /// THEN freeze_active is false
    #[test]
    fn test_freeze_state_deactivate() {
        let mut s = FreezeState::default();
        s.activate(1_000, Instant::now());
        s.deactivate();
        assert!(!s.freeze_active);
    }

    /// WHEN auto-unfreeze timeout elapses
    /// THEN is_auto_unfreeze_due returns true
    #[test]
    fn test_auto_unfreeze_due() {
        let timeout = Duration::from_millis(100);
        let mut s = FreezeState::new(timeout);
        let start = Instant::now();
        s.activate(0, start);
        // Simulate time passing by using a later instant
        let later = start + Duration::from_millis(200);
        assert!(s.is_auto_unfreeze_due(later));
    }

    /// WHEN auto-unfreeze timeout has NOT elapsed
    /// THEN is_auto_unfreeze_due returns false
    #[test]
    fn test_auto_unfreeze_not_due() {
        let timeout = Duration::from_secs(300);
        let mut s = FreezeState::new(timeout);
        let start = Instant::now();
        s.activate(0, start);
        assert!(!s.is_auto_unfreeze_due(start));
    }

    /// WHEN freeze is cancelled for safe mode
    /// THEN freeze_active is false
    #[test]
    fn test_cancel_for_safe_mode() {
        let mut s = FreezeState::default();
        s.activate(1_000, Instant::now());
        assert!(s.freeze_active);
        s.cancel_for_safe_mode();
        assert!(!s.freeze_active);
    }

    // ── FreezeManager unit tests ───────────────────────────────────────────────

    /// WHEN safe mode is active and viewer presses Ctrl+Shift+F
    /// THEN freeze is ignored
    #[test]
    fn test_freeze_ignored_during_safe_mode() {
        let mut mgr = FreezeManager::new();
        let activated = mgr.activate(
            true, // safe_mode_active
            0,
            Instant::now(),
        );
        assert!(!activated);
        assert!(!mgr.is_frozen());
    }

    /// WHEN freeze is activated and a mutation is enqueued
    /// THEN the mutation is queued (not applied)
    #[test]
    fn test_freeze_queues_mutations() {
        let mut mgr = FreezeManager::new();
        mgr.activate(false, 0, Instant::now());
        assert!(mgr.is_frozen());

        let m = make_mutation("b1", MutationTrafficClass::StateStream, None);
        assert!(matches!(mgr.try_enqueue(m), EnqueueResult::Queued));
        assert_eq!(mgr.queue.len(), 1);
    }

    /// WHEN freeze is deactivated
    /// THEN queued mutations are returned in submission order
    #[test]
    fn test_unfreeze_applies_queued_mutations() {
        let mut mgr = FreezeManager::new();
        mgr.activate(false, 0, Instant::now());

        for i in 0..3u8 {
            let m = make_mutation(&format!("b{i}"), MutationTrafficClass::StateStream, None);
            mgr.try_enqueue(m);
        }

        let drained = mgr.deactivate();
        assert!(!mgr.is_frozen());
        assert_eq!(drained.len(), 3);
        for (i, d) in drained.iter().enumerate() {
            assert_eq!(d.original_batch_id, format!("b{i}").as_bytes());
        }
    }

    /// WHEN safe mode activates while frozen
    /// THEN freeze is cancelled and queue is discarded
    #[test]
    fn test_safe_mode_cancels_freeze() {
        let mut mgr = FreezeManager::new();
        mgr.activate(false, 0, Instant::now());

        let m = make_mutation("b1", MutationTrafficClass::StateStream, None);
        mgr.try_enqueue(m);
        assert_eq!(mgr.queue.len(), 1);

        mgr.cancel_for_safe_mode();
        assert!(!mgr.is_frozen());
        assert!(mgr.queue.is_empty());
    }

    /// WHEN auto-unfreeze timeout elapses
    /// THEN tick() returns the drained queue and deactivates freeze
    #[test]
    fn test_auto_unfreeze_via_tick() {
        let mut mgr =
            FreezeManager::with_config(Duration::from_millis(50), DEFAULT_FREEZE_QUEUE_CAPACITY);
        let start = Instant::now();
        mgr.activate(false, 0, start);

        let m = make_mutation("b1", MutationTrafficClass::StateStream, None);
        mgr.try_enqueue(m);

        // Before timeout: tick returns None
        assert!(mgr.tick(start).is_none());

        // After timeout: tick returns drained queue
        let later = start + Duration::from_millis(100);
        let result = mgr.tick(later);
        assert!(result.is_some());
        let drained = result.unwrap();
        assert_eq!(drained.len(), 1);
        assert!(!mgr.is_frozen());
    }

    // ── classify_mutation_batch unit tests ────────────────────────────────────

    #[test]
    fn test_classify_create_tile_is_transactional() {
        assert_eq!(
            classify_mutation_batch(&["create_tile"]),
            MutationTrafficClass::Transactional,
        );
    }

    #[test]
    fn test_classify_set_tile_root_is_state_stream() {
        assert_eq!(
            classify_mutation_batch(&["set_tile_root"]),
            MutationTrafficClass::StateStream,
        );
    }

    #[test]
    fn test_classify_mixed_batch_is_transactional() {
        // Any transactional mutation makes the whole batch transactional.
        assert_eq!(
            classify_mutation_batch(&["set_tile_root", "create_tile"]),
            MutationTrafficClass::Transactional,
        );
    }

    #[test]
    fn test_classify_empty_batch_is_ephemeral() {
        assert_eq!(
            classify_mutation_batch(&[]),
            MutationTrafficClass::Ephemeral,
        );
    }

    #[test]
    fn test_classify_publish_to_zone_is_state_stream() {
        assert_eq!(
            classify_mutation_batch(&["publish_to_zone"]),
            MutationTrafficClass::StateStream,
        );
    }

    #[test]
    fn test_classify_set_portal_surface_is_transactional() {
        // The first-class portal surface DECLARATION is structural (identity +
        // parts) and must never be evicted under freeze pressure — parity with
        // the authoritative protocol classifier (classify_inbound_batch).
        assert_eq!(
            classify_mutation_batch(&["set_portal_surface"]),
            MutationTrafficClass::Transactional,
        );
    }

    #[test]
    fn test_classify_update_portal_surface_state_is_state_stream() {
        // The lifecycle/display PATCH is a coalescible content update.
        assert_eq!(
            classify_mutation_batch(&["update_portal_surface_state"]),
            MutationTrafficClass::StateStream,
        );
    }

    #[test]
    fn test_all_transactional_kinds() {
        let kinds = [
            "create_tile",
            "delete_tile",
            "create_tab",
            "switch_active_tab",
            "set_portal_surface",
        ];
        for kind in &kinds {
            assert_eq!(
                classify_mutation_batch(&[kind]),
                MutationTrafficClass::Transactional,
                "expected Transactional for kind={kind}",
            );
        }
    }

    // ── Scenario-level tests (from spec) ──────────────────────────────────────

    /// Scenario: Freeze queues mutations (spec line 146)
    /// WHEN viewer presses Ctrl+Shift+F and agent submits mutations
    /// THEN mutations queued, tile content does not update
    #[test]
    fn scenario_freeze_queues_mutations() {
        let mut mgr = FreezeManager::new();
        mgr.activate(false, 0, Instant::now());
        let m = make_mutation("mut1", MutationTrafficClass::StateStream, Some("ns/l1"));
        let result = mgr.try_enqueue(m);
        // Queued — not applied
        assert!(matches!(
            result,
            EnqueueResult::Queued | EnqueueResult::QueuedWithPressure | EnqueueResult::Coalesced
        ));
        assert_eq!(mgr.queue.len(), 1);
    }

    /// Scenario: Transactional mutations never dropped (spec line 150)
    /// WHEN freeze queue is full and transactional mutation submitted
    /// THEN mutation not evicted; backpressure applied
    #[test]
    fn scenario_transactional_never_dropped() {
        let mut mgr = FreezeManager::with_config(Duration::from_secs(300), 3);
        mgr.activate(false, 0, Instant::now());

        // Fill with StateStream
        for i in 0..3 {
            let m = make_mutation(&format!("ss{i}"), MutationTrafficClass::StateStream, None);
            mgr.try_enqueue(m);
        }
        assert!(mgr.queue.is_full());

        // Transactional → backpressure, not eviction
        let tx = make_mutation("tx1", MutationTrafficClass::Transactional, None);
        let r = mgr.try_enqueue(tx);
        assert!(matches!(r, EnqueueResult::BackpressureRequired { .. }));
        // Queue unchanged
        assert_eq!(mgr.queue.len(), 3);
    }

    /// Scenario: Unfreeze applies queued mutations (spec line 154)
    /// WHEN viewer unfreezes scene
    /// THEN all queued mutations applied in submission order
    #[test]
    fn scenario_unfreeze_applies_in_order() {
        let mut mgr = FreezeManager::new();
        mgr.activate(false, 0, Instant::now());

        let ids: Vec<String> = (0..5).map(|i| format!("mut{i}")).collect();
        for id in &ids {
            let m = make_mutation(id, MutationTrafficClass::StateStream, None);
            mgr.try_enqueue(m);
        }

        let drained = mgr.deactivate();
        assert!(!mgr.is_frozen());
        assert_eq!(drained.len(), 5);
        for (i, d) in drained.iter().enumerate() {
            assert_eq!(d.original_batch_id, ids[i].as_bytes());
        }
    }

    /// Scenario: Queue pressure signal (spec line 163)
    /// WHEN per-session freeze queue reaches 80% capacity
    /// THEN caller is notified (QueuedWithPressure)
    #[test]
    fn scenario_queue_pressure_signal() {
        let mut mgr = FreezeManager::with_config(Duration::from_secs(300), 10);
        mgr.activate(false, 0, Instant::now());

        // 7 entries (70%)
        for i in 0..7 {
            let m = make_mutation(&format!("m{i}"), MutationTrafficClass::StateStream, None);
            mgr.try_enqueue(m);
        }
        // 8th crosses 80%
        let m = make_mutation("m7", MutationTrafficClass::StateStream, None);
        let r = mgr.try_enqueue(m);
        assert!(matches!(r, EnqueueResult::QueuedWithPressure));
    }

    /// Scenario: Mutation dropped signal (spec line 167)
    /// WHEN queue full and state-stream mutation evicted
    /// THEN EvictedEntry returned (caller sends MUTATION_DROPPED)
    #[test]
    fn scenario_mutation_dropped_signal() {
        let mut mgr = FreezeManager::with_config(Duration::from_secs(300), 2);
        mgr.activate(false, 0, Instant::now());

        let m1 = make_mutation("ss1", MutationTrafficClass::StateStream, None);
        let m2 = make_mutation("ss2", MutationTrafficClass::StateStream, None);
        mgr.try_enqueue(m1);
        mgr.try_enqueue(m2);

        let m3 = make_mutation("ss3", MutationTrafficClass::StateStream, None);
        let r = mgr.try_enqueue(m3);
        assert!(matches!(r, EnqueueResult::EvictedEntry { .. }));
    }

    /// Scenario: Freeze ignored during safe mode (spec line 137)
    #[test]
    fn scenario_freeze_ignored_during_safe_mode() {
        let mut mgr = FreezeManager::new();
        let activated = mgr.activate(true, 0, Instant::now());
        assert!(!activated);
        assert!(!mgr.is_frozen());
    }

    /// Scenario: Safe mode cancels freeze (spec lines 129-135)
    #[test]
    fn scenario_safe_mode_cancels_freeze() {
        let mut mgr = FreezeManager::new();
        mgr.activate(false, 0, Instant::now());
        assert!(mgr.is_frozen());

        // Enqueue some mutations
        for i in 0..3 {
            let m = make_mutation(&format!("m{i}"), MutationTrafficClass::StateStream, None);
            mgr.try_enqueue(m);
        }

        // Safe mode entry: cancel freeze BEFORE other safe mode steps
        mgr.cancel_for_safe_mode();

        // Invariant: freeze_active = false, queue discarded
        assert!(!mgr.is_frozen());
        assert!(mgr.queue.is_empty());
    }
}
