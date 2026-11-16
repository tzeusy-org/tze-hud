//! Local feedback rendering style and rollback logic.
//!
//! Implements the doctrinal core: "Local feedback first. Touch/interaction
//! acknowledgement happens locally and instantly; remote semantics follow."
//!
//! # Default feedback styles
//!
//! | State   | Default visual                            |
//! |---------|-------------------------------------------|
//! | pressed | Multiply color by 0.85 (darkening)        |
//! | hovered | Add 0.1 white overlay                     |
//! | focused | 2px focus ring at node bounds             |
//!
//! These defaults are overridable per `HitRegionNode` via `LocalFeedbackStyle`.
//!
//! # Rollback semantics
//!
//! - Agent **explicitly rejects** an interaction → 100ms reverse animation.
//! - Agent **silent / slow** (>50ms, no response) → pressed state remains true
//!   until the interaction ends naturally (PointerUp).
//! - Only explicit rejection triggers rollback, not latency or silence.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tze_hud_scene::{Rgba, SceneId};

// ─── LocalFeedbackStyle ──────────────────────────────────────────────────────

/// Per-node customization of local feedback visuals.
///
/// All fields are optional; unset fields fall back to the default rendering
/// described in the module doc. Stored on `HitRegionNode` as `local_style`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalFeedbackStyle {
    /// Tint applied when the node is hovered.
    ///
    /// Default: `Rgba(1.0, 1.0, 1.0, 0.1)` — a 10% white overlay.
    /// The compositor blends this over the node's rendered content.
    pub hover_tint: Option<Rgba>,

    /// Tint applied when the node is pressed.
    ///
    /// Default: `None` (use the darkening multiplier instead, see `press_darken`).
    /// When set, the compositor applies this as an alpha-blended overlay instead
    /// of the 0.85 multiply.
    pub press_tint: Option<Rgba>,

    /// Multiply factor applied to the node color when pressed.
    ///
    /// Default: `0.85` (slight darkening). Ignored when `press_tint` is set.
    pub press_darken: Option<f32>,

    /// Color of the focus ring. Default: accent color (compositor-defined).
    pub focus_ring_color: Option<Rgba>,

    /// Width of the focus ring in pixels. Default: 2.0px.
    pub focus_ring_width_px: Option<f32>,
}

impl LocalFeedbackStyle {
    /// Construct a style with all defaults (all fields `None`).
    pub fn default_style() -> Self {
        Self {
            hover_tint: None,
            press_tint: None,
            press_darken: None,
            focus_ring_color: None,
            focus_ring_width_px: None,
        }
    }

    /// Returns true if no custom values are set (pure defaults).
    pub fn is_default(&self) -> bool {
        self.hover_tint.is_none()
            && self.press_tint.is_none()
            && self.press_darken.is_none()
            && self.focus_ring_color.is_none()
            && self.focus_ring_width_px.is_none()
    }
}

impl Default for LocalFeedbackStyle {
    fn default() -> Self {
        Self::default_style()
    }
}

// ─── LocalFeedbackDefaults ───────────────────────────────────────────────────

/// Compositor-visible resolved feedback parameters for a single node.
///
/// The input pipeline produces `ResolvedFeedbackStyle` by merging node-level
/// `LocalFeedbackStyle` with the system defaults. The compositor reads this to
/// know exactly what to render — no further style resolution needed.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedFeedbackStyle {
    /// Tint overlay applied when hovered.
    pub hover_tint: Rgba,
    /// Tint applied when pressed (takes priority over `press_darken`).
    pub press_tint: Option<Rgba>,
    /// Multiply factor for darkening on press (used when `press_tint` is None).
    pub press_darken: f32,
    /// Focus ring color.
    pub focus_ring_color: Rgba,
    /// Focus ring width in pixels.
    pub focus_ring_width_px: f32,
}

/// System-wide defaults for local feedback.
pub const DEFAULT_HOVER_TINT: Rgba = Rgba {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 0.1,
};
pub const DEFAULT_PRESS_DARKEN: f32 = 0.85;
pub const DEFAULT_FOCUS_RING_COLOR: Rgba = Rgba {
    r: 0.2,
    g: 0.5,
    b: 1.0,
    a: 1.0,
};
pub const DEFAULT_FOCUS_RING_WIDTH_PX: f32 = 2.0;

impl ResolvedFeedbackStyle {
    /// Resolve a `LocalFeedbackStyle` against system defaults.
    pub fn resolve(style: &LocalFeedbackStyle) -> Self {
        Self {
            hover_tint: style.hover_tint.unwrap_or(DEFAULT_HOVER_TINT),
            press_tint: style.press_tint,
            press_darken: style.press_darken.unwrap_or(DEFAULT_PRESS_DARKEN),
            focus_ring_color: style.focus_ring_color.unwrap_or(DEFAULT_FOCUS_RING_COLOR),
            focus_ring_width_px: style
                .focus_ring_width_px
                .unwrap_or(DEFAULT_FOCUS_RING_WIDTH_PX),
        }
    }

    /// Returns the pure system defaults (no per-node overrides).
    pub fn defaults() -> Self {
        Self::resolve(&LocalFeedbackStyle::default_style())
    }
}

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
    fn test_default_resolved_style_matches_spec() {
        let resolved = ResolvedFeedbackStyle::defaults();
        // Spec: hovered = 0.1 white overlay
        assert!((resolved.hover_tint.r - 1.0).abs() < f32::EPSILON);
        assert!((resolved.hover_tint.a - 0.1).abs() < f32::EPSILON);
        // Spec: pressed = multiply by 0.85
        assert!((resolved.press_darken - 0.85).abs() < f32::EPSILON);
        assert!(resolved.press_tint.is_none());
        // Spec: focused = 2px focus ring
        assert!((resolved.focus_ring_width_px - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_custom_press_tint_overrides_default_darken() {
        let style = LocalFeedbackStyle {
            press_tint: Some(Rgba::new(0.0, 0.0, 1.0, 0.3)),
            ..LocalFeedbackStyle::default_style()
        };
        let resolved = ResolvedFeedbackStyle::resolve(&style);
        // Custom tint should be set
        assert!(resolved.press_tint.is_some());
        let tint = resolved.press_tint.unwrap();
        assert!((tint.b - 1.0).abs() < f32::EPSILON);
        assert!((tint.a - 0.3).abs() < f32::EPSILON);
        // press_darken still present but compositor should prefer press_tint
        assert!((resolved.press_darken - 0.85).abs() < f32::EPSILON);
    }

    #[test]
    fn test_custom_focus_ring_overrides_default() {
        let style = LocalFeedbackStyle {
            focus_ring_color: Some(Rgba::new(1.0, 0.0, 0.0, 1.0)),
            focus_ring_width_px: Some(4.0),
            ..LocalFeedbackStyle::default_style()
        };
        let resolved = ResolvedFeedbackStyle::resolve(&style);
        assert!((resolved.focus_ring_color.r - 1.0).abs() < f32::EPSILON);
        assert!((resolved.focus_ring_width_px - 4.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_local_feedback_style_is_default() {
        let style = LocalFeedbackStyle::default_style();
        assert!(style.is_default());
        let custom = LocalFeedbackStyle {
            hover_tint: Some(Rgba::new(0.5, 0.5, 0.5, 0.2)),
            ..LocalFeedbackStyle::default_style()
        };
        assert!(!custom.is_default());
    }

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
