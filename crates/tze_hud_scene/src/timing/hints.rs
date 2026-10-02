//! TimingHints struct and MessageClass enum.
//!
//! # Spec alignment
//!
//! Implements `timing-model/spec.md §Requirement: Timing Fields on Payloads`
//! (lines 72-83) and `§Requirement: Message Class Typed Enum` (lines 369-380).
//!
//! All meaningful realtime payloads carry `TimingHints`. The `schedule` field
//! is a `oneof`-style enum: setting more than one scheduling variant is
//! rejected with `RELATIVE_SCHEDULE_CONFLICT` during validation.
//!
//! ## Scope boundary
//!
//! `after_us`, `frames_from_now`, and `next_frame` are **relative scheduling
//! primitives** (sub-bead #4 / rig-wu3q). They are defined here for the wire
//! contract (and mutual-exclusion validation) but conversion to absolute
//! `present_at_wall_us` is out of scope for this sub-bead.

use serde::{Deserialize, Serialize};

use crate::timing::{DurationUs, WallUs};

// ─── MessageClass ─────────────────────────────────────────────────────────────

/// Four message classes with distinct delivery semantics.
///
/// From spec §Requirement: Message Class Typed Enum (lines 369-380).
///
/// | Class | Semantics |
/// |---|---|
/// | `Transactional` | Reliable, ordered, acked, never coalesced |
/// | `StateStream` | Reliable, ordered, coalesced per `coalesce_key` |
/// | `EphemeralRealtime` | Low-latency, droppable, latest-wins |
/// | `ClockedMediaCue` | Scheduled against media/display clock (post-v1) |
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MessageClass {
    /// Reliable, ordered, acked. Never coalesced or dropped.
    #[default]
    Transactional,
    /// Reliable, ordered. Coalesced per `coalesce_key` within a frame window.
    StateStream,
    /// Low-latency, droppable, latest-wins. Compatible with `DROP_IF_LATE`.
    EphemeralRealtime,
    /// Scheduled against media/display clock. Defined in proto; not actively
    /// processed in v1 (post-v1 feature).
    ClockedMediaCue,
}

// ─── DeliveryPolicy ──────────────────────────────────────────────────────────

/// Delivery policy hint attached to a payload.
///
/// Controls what happens when a mutation arrives after its frame's Stage 3
/// intake window has closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeliveryPolicy {
    /// Defer to the next frame (default for all classes).
    #[default]
    DeferToNextFrame,
    /// Drop if it arrives late.
    ///
    /// **Only valid with `MessageClass::EphemeralRealtime`.**  Setting this on
    /// any other class MUST be rejected (`INVALID_DELIVERY_POLICY`).
    DropIfLate,
}

// ─── Schedule (oneof) ────────────────────────────────────────────────────────

/// Mutually exclusive schedule variants (proto `oneof schedule`).
///
/// Only one variant may be set per message. Setting more than one MUST be
/// rejected with `RELATIVE_SCHEDULE_CONFLICT`.
///
/// # v1 scope
///
/// `AfterUs`, `FramesFromNow`, and `NextFrame` are wire-level convenience
/// sugar for relative scheduling (rig-wu3q). Their conversion to
/// `present_at_wall_us` happens at Stage 3 intake and is not implemented in
/// this crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Schedule {
    /// Apply no earlier than this UTC wall-clock time (microseconds since
    /// Unix epoch). Zero means "not set" / immediate.
    PresentAt(WallUs),

    /// Apply N microseconds from the compositor's monotonic clock at intake.
    ///
    /// Relative scheduling primitive — converted to `PresentAt` at Stage 3.
    AfterUs(DurationUs),

    /// Apply at frame `current_frame + N`.
    ///
    /// Relative scheduling primitive — converted to `PresentAt` at Stage 3.
    FramesFromNow(u32),

    /// Sugar for `FramesFromNow(1)`. MUST NOT be applied in the current frame.
    ///
    /// Relative scheduling primitive — converted to `PresentAt` at Stage 3.
    NextFrame,
}

impl Schedule {
    /// Return the `PresentAt` wall-clock value if this variant is `PresentAt`,
    /// otherwise `None`.
    pub fn as_present_at(self) -> Option<WallUs> {
        match self {
            Schedule::PresentAt(t) => Some(t),
            _ => None,
        }
    }

    /// Returns `true` if this variant is a relative scheduling primitive
    /// (not `PresentAt`).
    pub fn is_relative(self) -> bool {
        !matches!(self, Schedule::PresentAt(_))
    }
}

// ─── TimingHints ─────────────────────────────────────────────────────────────

/// Timing metadata attached to every realtime payload.
///
/// # Spec alignment
///
/// From `timing-model/spec.md §Requirement: Timing Fields on Payloads`
/// (lines 72-83).
///
/// All fields are optional (zero/`None` = "not set"). The compositor MUST
/// interpret and enforce whichever fields are set.
///
/// # Mutual exclusion
///
/// The `schedule` field is a `oneof`-style enum. If the caller provides a
/// `Schedule::PresentAt` alongside any relative primitive, validation MUST
/// reject it with `RELATIVE_SCHEDULE_CONFLICT`.
///
/// # Timestamp precedence
///
/// Per spec §Requirement: Timestamp Precedence (lines 85-92): node-level
/// hints override tile-level, tile-level overrides batch-level.  The
/// [`TimingHints::resolve_present_at`] helper implements this.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingHints {
    /// Mutually exclusive schedule (present_at or one relative primitive).
    ///
    /// `None` is equivalent to `Schedule::PresentAt(WallUs::NOT_SET)` — apply
    /// at the earliest available frame.
    pub schedule: Option<Schedule>,

    /// Optional expiry time. Zero = no expiry.
    ///
    /// MUST satisfy `expires_at_wall_us > present_at_wall_us` when both are
    /// non-zero; otherwise rejected with `TIMESTAMP_EXPIRY_BEFORE_PRESENT`.
    pub expires_at_wall_us: WallUs,

    /// Creation timestamp (UTC µs since epoch).  Informational; never drives
    /// scheduling decisions.
    pub created_at_wall_us: WallUs,

    /// Monotonic per-source ordering sequence. Zero = unspecified.
    pub sequence: u64,

    /// Shedding priority under load. Higher value = higher shedding priority
    /// (dropped first). Zero = unspecified / default.
    pub priority: u32,

    /// State-stream deduplication key. Empty = no coalescing.
    pub coalesce_key: Option<String>,

    /// Message class governing delivery semantics.
    pub message_class: MessageClass,

    /// Delivery policy for late arrivals.
    pub delivery_policy: DeliveryPolicy,
}

impl TimingHints {
    /// Create a `TimingHints` with all defaults (immediate / no expiry).
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve the effective `present_at_wall_us` using three-level precedence.
    ///
    /// Precedence (highest to lowest): `self` → `parent` → `batch`.
    ///
    /// Returns the first non-zero `PresentAt` schedule value found, or
    /// `WallUs::NOT_SET` (zero) if all levels use relative primitives or have
    /// no schedule set.
    ///
    /// # Note
    ///
    /// Relative scheduling primitives at any level are ignored here; callers
    /// must convert relative schedules to `PresentAt` before invoking this.
    pub fn resolve_present_at(
        item: Option<&TimingHints>,
        parent: Option<&TimingHints>,
        batch: Option<&TimingHints>,
    ) -> WallUs {
        for hints in [item, parent, batch].into_iter().flatten() {
            if let Some(Schedule::PresentAt(t)) = hints.schedule {
                if t.is_set() {
                    return t;
                }
            }
        }
        WallUs::NOT_SET
    }

    /// Check that at most one `Schedule` variant is set.
    ///
    /// This is a helper used inside the `schedule` field. Since `Schedule` is
    /// an enum (not multiple optional fields), this invariant is trivially
    /// maintained by the Rust type system. However, when deserializing from
    /// wire format where the original proto `oneof` allows setting multiple
    /// fields, callers must check for conflicts during decode.
    pub fn has_relative_schedule(&self) -> bool {
        matches!(
            self.schedule,
            Some(Schedule::AfterUs(_))
                | Some(Schedule::FramesFromNow(_))
                | Some(Schedule::NextFrame)
        )
    }

    /// Returns the `present_at_wall_us` if the schedule is `PresentAt`,
    /// otherwise `WallUs::NOT_SET`.
    pub fn present_at_wall_us(&self) -> WallUs {
        match self.schedule {
            Some(Schedule::PresentAt(t)) => t,
            _ => WallUs::NOT_SET,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── MessageClass ──

    #[test]
    fn message_class_default_is_transactional() {
        let h = TimingHints::default();
        assert_eq!(h.message_class, MessageClass::Transactional);
    }

    #[test]
    fn message_class_variants_distinct() {
        assert_ne!(MessageClass::Transactional, MessageClass::StateStream);
        assert_ne!(
            MessageClass::EphemeralRealtime,
            MessageClass::ClockedMediaCue
        );
    }

    // ── Schedule oneof ──

    #[test]
    fn schedule_present_at_variant() {
        let t = WallUs(1_000_000);
        let s = Schedule::PresentAt(t);
        assert_eq!(s.as_present_at(), Some(t));
        assert!(!s.is_relative());
    }

    #[test]
    fn schedule_after_us_is_relative() {
        let s = Schedule::AfterUs(DurationUs(500_000));
        assert!(s.is_relative());
        assert_eq!(s.as_present_at(), None);
    }

    #[test]
    fn schedule_frames_from_now_is_relative() {
        let s = Schedule::FramesFromNow(3);
        assert!(s.is_relative());
    }

    #[test]
    fn schedule_next_frame_is_relative() {
        let s = Schedule::NextFrame;
        assert!(s.is_relative());
    }

    // ── TimingHints ──

    #[test]
    fn default_hints_are_immediate_no_expiry() {
        let h = TimingHints::new();
        assert_eq!(h.schedule, None);
        assert_eq!(h.expires_at_wall_us, WallUs::NOT_SET);
        assert_eq!(h.delivery_policy, DeliveryPolicy::DeferToNextFrame);
    }

    #[test]
    fn present_at_wall_us_extracts_from_schedule() {
        let mut h = TimingHints::new();
        h.schedule = Some(Schedule::PresentAt(WallUs(5_000_000)));
        assert_eq!(h.present_at_wall_us(), WallUs(5_000_000));
    }

    #[test]
    fn present_at_wall_us_returns_not_set_for_relative() {
        let mut h = TimingHints::new();
        h.schedule = Some(Schedule::AfterUs(DurationUs(1_000)));
        assert_eq!(h.present_at_wall_us(), WallUs::NOT_SET);
    }

    #[test]
    fn has_relative_schedule_detects_after_us() {
        let mut h = TimingHints::new();
        h.schedule = Some(Schedule::AfterUs(DurationUs(0)));
        assert!(h.has_relative_schedule());
    }

    #[test]
    fn has_relative_schedule_false_for_present_at() {
        let mut h = TimingHints::new();
        h.schedule = Some(Schedule::PresentAt(WallUs(1)));
        assert!(!h.has_relative_schedule());
    }

    // ── Timestamp precedence ──

    /// WHEN a node has its own present_at THEN node-level wins.
    #[test]
    fn resolve_present_at_node_wins_over_parent() {
        let mut node = TimingHints::new();
        node.schedule = Some(Schedule::PresentAt(WallUs(1_000)));
        let mut parent = TimingHints::new();
        parent.schedule = Some(Schedule::PresentAt(WallUs(2_000)));
        let result = TimingHints::resolve_present_at(Some(&node), Some(&parent), None);
        assert_eq!(result, WallUs(1_000));
    }

    /// WHEN node has no schedule, parent's schedule applies.
    #[test]
    fn resolve_present_at_parent_wins_when_node_unset() {
        let node = TimingHints::new();
        let mut parent = TimingHints::new();
        parent.schedule = Some(Schedule::PresentAt(WallUs(2_000)));
        let result = TimingHints::resolve_present_at(Some(&node), Some(&parent), None);
        assert_eq!(result, WallUs(2_000));
    }

    /// WHEN only batch schedule is set, batch applies.
    #[test]
    fn resolve_present_at_batch_fallback() {
        let mut batch = TimingHints::new();
        batch.schedule = Some(Schedule::PresentAt(WallUs(9_000)));
        let result = TimingHints::resolve_present_at(None, None, Some(&batch));
        assert_eq!(result, WallUs(9_000));
    }

    /// WHEN all are None/zero, NOT_SET is returned.
    #[test]
    fn resolve_present_at_all_unset_returns_not_set() {
        let result = TimingHints::resolve_present_at(None, None, None);
        assert_eq!(result, WallUs::NOT_SET);
    }

    // ── DeliveryPolicy ──

    #[test]
    fn delivery_policy_default_is_defer() {
        let h = TimingHints::new();
        assert_eq!(h.delivery_policy, DeliveryPolicy::DeferToNextFrame);
    }
}
