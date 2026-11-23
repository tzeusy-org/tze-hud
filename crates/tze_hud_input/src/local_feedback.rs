//! Local feedback rollback logic.
//!
//! Implements the doctrinal core: "Local feedback first. Touch/interaction
//! acknowledgement happens locally and instantly; remote semantics follow."
//! The feedback visuals themselves are token-driven in the compositor.
//!
//! # Rollback semantics
//!
//! - Agent **explicitly rejects** an interaction → 100ms reverse animation.
//! - Agent **silent / slow** (>50ms, no response) → pressed state remains true
//!   until the interaction ends naturally (PointerUp).
//! - Only explicit rejection triggers rollback, not latency or silence.

use std::time::{Duration, Instant};
use tze_hud_scene::SceneId;

// ─── RollbackAnimation ───────────────────────────────────────────────────────

/// Duration of the rollback reverse animation per Local Feedback Rollback.
pub const ROLLBACK_ANIMATION_MS: u64 = 100;

/// State of a pending rollback animation for a single node.
///
/// Rollback is triggered only on **explicit agent rejection**, not on silence.
/// The compositor is responsible for driving the animation; the input crate
/// produces the `rollback=true` flag in `LocalStateUpdate` and records the
/// rollback entry here for bookkeeping (e.g. to prevent further presses during
/// animation).
#[derive(Clone, Debug)]
pub struct RollbackAnimation {
    /// The node undergoing rollback.
    pub node_id: SceneId,
    /// When the rollback started.
    pub started_at: Instant,
    /// Duration of the animation.
    pub duration: Duration,
}

impl RollbackAnimation {
    /// Start a rollback animation for the given node.
    pub fn start(node_id: SceneId) -> Self {
        Self {
            node_id,
            started_at: Instant::now(),
            duration: Duration::from_millis(ROLLBACK_ANIMATION_MS),
        }
    }

    /// Returns true if the animation is still in progress.
    pub fn is_active(&self) -> bool {
        self.started_at.elapsed() < self.duration
    }

    /// Returns the animation progress in [0.0, 1.0] (1.0 = complete).
    pub fn progress(&self) -> f32 {
        let elapsed = self.started_at.elapsed().as_secs_f32();
        let total = self.duration.as_secs_f32();
        (elapsed / total).min(1.0)
    }
}

// ─── RollbackTracker ─────────────────────────────────────────────────────────

/// Tracks active rollback animations across all nodes.
///
/// Owned by `InputProcessor`. When an agent rejection arrives, the processor
/// calls `begin_rollback(node_id)` which marks the animation as started and
/// returns a `LocalStateUpdate` with `rollback=true` to be included in the
/// next `SceneLocalPatch`.
///
/// Expired animations are pruned lazily on `begin_rollback` only (not on `is_rolling_back`,
/// which takes `&self` and cannot mutate).
#[derive(Default)]
pub struct RollbackTracker {
    active: Vec<RollbackAnimation>,
}

impl RollbackTracker {
    pub fn new() -> Self {
        Self { active: Vec::new() }
    }

    /// Begin a rollback animation for the given node.
    ///
    /// Replaces any existing animation for the same node (idempotent).
    pub fn begin_rollback(&mut self, node_id: SceneId) {
        self.prune_expired();
        self.active.retain(|a| a.node_id != node_id);
        self.active.push(RollbackAnimation::start(node_id));
    }

    /// Returns true if the given node is currently in a rollback animation.
    pub fn is_rolling_back(&self, node_id: SceneId) -> bool {
        self.active
            .iter()
            .any(|a| a.node_id == node_id && a.is_active())
    }

    /// Remove all completed animations.
    fn prune_expired(&mut self) {
        self.active.retain(|a| a.is_active());
    }

    /// Returns all active rollback animations (e.g. for debug rendering).
    pub fn active_animations(&self) -> &[RollbackAnimation] {
        &self.active
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rollback_animation_progress() {
        let node_id = SceneId::new();
        let anim = RollbackAnimation::start(node_id);
        assert!(anim.is_active());
        // Progress at t=0 should be near 0
        let p = anim.progress();
        assert!(
            (0.0..=0.1).contains(&p),
            "initial progress should be near 0, got {p}"
        );
    }

    #[test]
    fn test_rollback_tracker_begin_and_query() {
        let node_id = SceneId::new();
        let mut tracker = RollbackTracker::new();
        assert!(!tracker.is_rolling_back(node_id));

        tracker.begin_rollback(node_id);
        assert!(tracker.is_rolling_back(node_id));
    }

    #[test]
    fn test_rollback_tracker_begin_replaces_existing() {
        let node_id = SceneId::new();
        let mut tracker = RollbackTracker::new();
        tracker.begin_rollback(node_id);
        tracker.begin_rollback(node_id); // Should not double-add
        assert_eq!(tracker.active_animations().len(), 1);
    }

    #[test]
    fn test_rollback_animation_duration_is_100ms() {
        let node_id = SceneId::new();
        let anim = RollbackAnimation::start(node_id);
        assert_eq!(anim.duration.as_millis(), ROLLBACK_ANIMATION_MS as u128);
    }
}
