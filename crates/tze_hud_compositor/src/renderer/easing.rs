//! Motion-polish primitives for the portal tile render path.
//!
//! This module hosts the **pure, deterministic** building blocks for compositor
//! animation: easing curves, scalar interpolation, frame-rate-
//! independent scroll smoothing, and a streaming-reveal fade ramp. Arrival time
//! differs from presentation time: adapters set targets (scroll offset, content),
//! and these primitives
//! drive the *presentation-time* interpolation the compositor owns.
//!
//! Everything here is split so that the time-independent math is testable
//! without sleeping. The stateful machine ([`ScrollSmoother`]) exposes a pure
//! `advance` core; tests exercise it directly for determinism.

/// Easing curve applied to a normalized progress value `t ∈ [0, 1]`.
///
/// Every variant satisfies `f(0) == 0` and `f(1) == 1` and is monotonic
/// non-decreasing on `[0, 1]`, so it is safe to drive any
/// `lerp(from, to, ease(t))` interpolation without overshoot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum Easing {
    /// Identity — `f(t) = t`. No acceleration; matches the legacy linear fades.
    Linear,
    /// Smoothstep `f(t) = 3t² − 2t³`: gentle acceleration then deceleration.
    /// The default for collapse/expand and tile fades — symmetric about `t=0.5`.
    #[default]
    EaseInOut,
    /// Quadratic ease-out `f(t) = 1 − (1 − t)²`: fast start, soft stop. Used for
    /// follow-tail catch-up so newly-arrived content settles gently.
    EaseOutQuad,
    /// Quadratic ease-in `f(t) = t²`, the existing `accelerate` motion token.
    EaseInQuad,
}

impl Easing {
    /// Map raw linear progress to eased progress.
    ///
    /// The input is clamped to `[0, 1]`; the output is in `[0, 1]`.
    #[inline]
    pub(crate) fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Easing::Linear => t,
            Easing::EaseInOut => t * t * (3.0 - 2.0 * t),
            Easing::EaseOutQuad => {
                let inv = 1.0 - t;
                1.0 - inv * inv
            }
            Easing::EaseInQuad => t * t,
        }
    }
}

/// Zone-only motion parsed once when the compositor receives its startup map.
/// In-flight states capture their curve; later maps affect only new transitions.
pub(super) struct ZoneMotion {
    pub(super) enter: Easing,
    pub(super) exit: Easing,
    pub(super) exit_ms: u32,
}

impl ZoneMotion {
    pub(super) fn from_token_map(tokens: &std::collections::HashMap<String, String>) -> Self {
        let curve = |key: &str| {
            let parse = |value: &str| match value {
                "linear" => Some(Easing::Linear),
                "standard" => Some(Easing::EaseInOut),
                "decelerate" => Some(Easing::EaseOutQuad),
                "accelerate" => Some(Easing::EaseInQuad),
                _ => None,
            };
            if let Some(value) = tokens.get(key) {
                if let Some(curve) = parse(value) {
                    return curve;
                }
                tracing::warn!(
                    token_key = key,
                    "invalid motion easing; using canonical default"
                );
            }
            let canonical = tze_hud_config::tokens::CANONICAL_TOKENS
                .iter()
                .find(|token| token.key == key)
                .expect("zone easing key must be canonical");
            parse(canonical.default_value).expect("canonical zone easing must be valid")
        };
        Self {
            enter: curve("motion.enter.easing"),
            exit: curve("motion.exit.easing"),
            exit_ms: tze_hud_config::tokens::resolve_motion_duration_ms(tokens, "motion.exit.ms"),
        }
    }
}

/// Linear interpolation between `a` and `b` by factor `t` (unclamped).
///
/// Callers that need clamping should pass an already-clamped/eased `t`
/// (e.g. [`Easing::apply`], which clamps).
#[inline]
pub(crate) fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Frame-rate-independent exponential smoothing factor.
///
/// Returns the fraction `α ∈ [0, 1]` of the remaining distance to a target that
/// should be closed after `dt_ms` have elapsed, given a time constant
/// `tau_ms`: `α = 1 − exp(−dt/τ)`.
///
/// Properties (asserted in tests):
/// - `dt_ms == 0` → `0.0` (no movement).
/// - `dt_ms → ∞`  → `1.0` (fully caught up).
/// - `dt_ms == tau_ms` → `1 − 1/e ≈ 0.632`.
/// - monotonic non-decreasing in `dt_ms`.
///
/// `tau_ms <= 0` degenerates to an instant snap (`1.0`) rather than dividing by
/// zero — a non-positive time constant means "no smoothing".
#[inline]
pub(crate) fn exp_smooth_factor(dt_ms: f32, tau_ms: f32) -> f32 {
    if tau_ms <= 0.0 || !dt_ms.is_finite() {
        return 1.0;
    }
    let dt = dt_ms.max(0.0);
    (1.0 - (-dt / tau_ms).exp()).clamp(0.0, 1.0)
}

/// Per-tile smoothed scroll offset (smooth scroll / animated follow-tail).
///
/// The adapter/input layer sets the authoritative scroll *target*; this smoother
/// eases the *displayed* offset toward it each frame using frame-rate-
/// independent exponential smoothing ([`exp_smooth_factor`]). User scroll
/// remains authoritative: the target is never altered here, only
/// the visual catch-up is animated.
///
/// `advance` is pure given `dt_ms`, so convergence and frame-rate independence
/// are unit-tested without a real clock.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollSmoother {
    displayed_x: f32,
    displayed_y: f32,
    tau_ms: f32,
    /// Snap distance (px): within this of the target, jump exactly onto it so
    /// motion terminates cleanly instead of asymptotically crawling.
    snap_epsilon: f32,
    origin_shift: (f64, f64),
}

/// Default smoothing time constant (ms). ~90 ms reads as a quick, deliberate
/// settle at 60 Hz without feeling laggy. Kept private; callers construct via
/// [`ScrollSmoother::new`].
pub(super) const SCROLL_SMOOTH_TAU_MS: f32 = 90.0;
/// Default snap distance (px) — sub-pixel, so the terminal state is exact.
pub(super) const SCROLL_SNAP_EPSILON_PX: f32 = 0.5;

impl ScrollSmoother {
    /// Create a smoother already settled at `(x, y)`.
    ///
    /// A freshly-observed tile starts *on* its current offset (no initial
    /// jump); only subsequent target changes animate.
    pub(crate) fn new(x: f32, y: f32, origin_shift: (f64, f64)) -> Self {
        Self {
            displayed_x: x,
            displayed_y: y,
            tau_ms: SCROLL_SMOOTH_TAU_MS,
            snap_epsilon: SCROLL_SNAP_EPSILON_PX,
            origin_shift,
        }
    }

    /// Translate displayed coordinates by unconsumed layout corrections. The
    /// cumulative shift coalesces corrections without swallowing wheel input
    /// received before this frame; ordinary advance still eases that target.
    pub(crate) fn rebase(&mut self, origin_shift: (f64, f64)) {
        self.displayed_x += (origin_shift.0 - self.origin_shift.0) as f32;
        self.displayed_y += (origin_shift.1 - self.origin_shift.1) as f32;
        self.origin_shift = origin_shift;
    }

    /// The currently displayed (smoothed) offset.
    #[inline]
    pub(crate) fn displayed(&self) -> (f32, f32) {
        (self.displayed_x, self.displayed_y)
    }

    /// Whether the displayed offset has settled onto `(target_x, target_y)`.
    ///
    /// "Settled" means both axes are within [`Self::snap_epsilon`] of the
    /// target, i.e. the next [`advance`](Self::advance) would snap exactly onto
    /// it and produce no further motion. The idle render gate uses
    /// this to tell a still-catching-up smoother (must keep rendering) apart
    /// from a settled one (safe to idle).
    #[inline]
    pub(crate) fn is_settled(&self, target_x: f32, target_y: f32) -> bool {
        (target_x - self.displayed_x).abs() <= self.snap_epsilon
            && (target_y - self.displayed_y).abs() <= self.snap_epsilon
    }

    /// Advance the displayed offset toward `(target_x, target_y)` by one frame
    /// of `dt_ms`, returning the new displayed offset.
    ///
    /// Pure given `dt_ms`: the same `(target, dt)` sequence always yields the
    /// same trajectory. Within [`Self::snap_epsilon`] of the target on an axis,
    /// that axis snaps exactly onto the target.
    #[inline]
    pub(crate) fn advance(&mut self, target_x: f32, target_y: f32, dt_ms: f32) -> (f32, f32) {
        let a = exp_smooth_factor(dt_ms, self.tau_ms);
        self.displayed_x = Self::step_axis(self.displayed_x, target_x, a, self.snap_epsilon);
        self.displayed_y = Self::step_axis(self.displayed_y, target_y, a, self.snap_epsilon);
        self.displayed()
    }

    #[inline]
    fn step_axis(displayed: f32, target: f32, alpha: f32, snap_epsilon: f32) -> f32 {
        if (target - displayed).abs() <= snap_epsilon {
            return target;
        }
        let next = lerp(displayed, target, alpha);
        if (target - next).abs() <= snap_epsilon {
            target
        } else {
            next
        }
    }
}

/// Streaming-reveal fade ramp: maps "how far into the current dwell window we
/// are" to an eased alpha in `[0, 1]`.
///
/// Where the zone streaming reveal hard-*snaps* each breakpoint segment into
/// view (`StreamRevealState`), this ramps the leading (just-revealed) segment's
/// opacity from `0 → 1` across its dwell frames so it fades in. It is the
/// portal-path streaming-fade primitive (deliverable #3); the dwell-frame
/// counter is supplied by the reveal state, keeping this pure and testable.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StreamFadeRamp {
    easing: Easing,
}

impl Default for StreamFadeRamp {
    fn default() -> Self {
        Self {
            easing: Easing::EaseOutQuad,
        }
    }
}

impl StreamFadeRamp {
    /// Create a ramp with the given easing.
    #[cfg(test)]
    pub(crate) fn new(easing: Easing) -> Self {
        Self { easing }
    }

    /// Eased alpha for the leading segment given the dwell progress.
    ///
    /// `frames_in_segment` is the number of frames the leading segment has been
    /// visible; `frames_per_segment` is the full dwell window. Returns `1.0`
    /// (fully revealed) once the dwell completes or when `frames_per_segment`
    /// is `0`.
    #[inline]
    pub(crate) fn alpha(&self, frames_in_segment: u32, frames_per_segment: u32) -> f32 {
        if frames_per_segment == 0 {
            return 1.0;
        }
        let t = frames_in_segment as f32 / frames_per_segment as f32;
        self.easing.apply(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Easing::{EaseInOut, EaseInQuad, EaseOutQuad, Linear};
    use std::collections::HashMap;

    const EPS: f32 = 1e-5;

    #[test]
    fn easing_curves_clamp_and_grow_without_overshoot() {
        for (curve, quarter, midpoint) in [
            (Linear, 0.25, 0.5),
            (EaseInOut, 0.15625, 0.5),
            (EaseOutQuad, 0.4375, 0.75),
            (EaseInQuad, 0.0625, 0.25),
        ] {
            for (t, expected) in [
                (-5.0, 0.0),
                (0.0, 0.0),
                (0.25, quarter),
                (0.5, midpoint),
                (1.0, 1.0),
                (5.0, 1.0),
            ] {
                assert!((curve.apply(t) - expected).abs() < EPS, "{curve:?} at {t}");
            }
            for i in 0..100 {
                let left = curve.apply(i as f32 / 100.0);
                let right = curve.apply((i + 1) as f32 / 100.0);
                assert!(left <= right + EPS, "{curve:?} not monotonic at {i}");
            }
        }
        for t in [0.1, 0.25, 0.4] {
            assert!((EaseInOut.apply(t) + EaseInOut.apply(1.0 - t) - 1.0).abs() < EPS);
        }
    }

    #[test]
    fn motion_tokens_parse_curves_and_fall_back_to_canonical_defaults() {
        for (enter, exit, ms, expected) in [
            (
                "decelerate",
                "accelerate",
                "240",
                (EaseOutQuad, EaseInQuad, 240),
            ),
            ("invalid", "invalid", "-10", (EaseOutQuad, EaseInQuad, 120)),
            ("linear", "standard", "120", (Linear, EaseInOut, 120)),
        ] {
            let motion = ZoneMotion::from_token_map(&HashMap::from([
                ("motion.enter.easing".into(), enter.into()),
                ("motion.exit.easing".into(), exit.into()),
                ("motion.exit.ms".into(), ms.into()),
            ]));
            assert_eq!((motion.enter, motion.exit, motion.exit_ms), expected);
        }
    }

    #[test]
    fn interpolation_and_smoothing_keep_endpoints() {
        for (t, expected) in [(0.0, 10.0), (0.5, 15.0), (1.0, 20.0)] {
            assert_eq!(lerp(10.0, 20.0, t), expected);
        }
        assert_eq!(exp_smooth_factor(0.0, 90.0), 0.0);
        assert!(exp_smooth_factor(10_000.0, 90.0) > 0.99);
        assert!((exp_smooth_factor(90.0, 90.0) - (1.0 - std::f32::consts::E.recip())).abs() < 1e-4);
        for tau in [0.0, -10.0] {
            assert_eq!(exp_smooth_factor(5.0, tau), 1.0);
        }
        for dt in 0..200 {
            assert!(exp_smooth_factor(dt as f32, 90.0) <= exp_smooth_factor((dt + 1) as f32, 90.0));
        }
    }

    #[test]
    fn scroll_smoother_rebases_once_then_settles_without_overshoot() {
        let mut scroll = ScrollSmoother::new(12.0, 34.0, (5.0, -700.0));
        assert_eq!(scroll.displayed(), (12.0, 34.0));
        scroll.rebase((5.0, -700.0));
        assert!(scroll.is_settled(12.0, 34.0));

        scroll.rebase((12.0, -730.0));
        assert_eq!(scroll.displayed(), (19.0, 4.0));
        scroll.rebase((12.0, -730.0));
        assert_eq!(scroll.displayed(), (19.0, 4.0));

        let (mut previous_x, mut previous_y) = scroll.advance(19.1, 500.0, 16.6);
        assert_eq!(previous_x, 19.1, "snap to the nearby target immediately");
        assert!(
            !scroll.is_settled(19.1, 500.0),
            "long-axis motion stays active"
        );
        for _ in 0..239 {
            let (x, y) = scroll.advance(19.1, 500.0, 16.6);
            assert!(previous_x <= x && x <= 19.1);
            assert!(previous_y <= y && y <= 500.0);
            (previous_x, previous_y) = (x, y);
            if (x, y) == (19.1, 500.0) {
                break;
            }
        }
        assert_eq!(scroll.displayed(), (19.1, 500.0));
        assert!(scroll.is_settled(19.1, 500.0));
    }

    #[test]
    fn scroll_smoothing_depends_on_elapsed_time() {
        let elapsed = |steps, ms| {
            let mut scroll = ScrollSmoother::new(0.0, 0.0, (0.0, 0.0));
            for _ in 0..steps {
                scroll.advance(0.0, 100.0, ms);
            }
            scroll.displayed().1
        };
        let coarse = elapsed(1, 100.0);
        let fine = elapsed(100, 1.0);
        assert!((coarse - fine).abs() < 1.0, "{coarse} vs {fine}");
    }

    #[test]
    fn stream_fade_respects_dwell_and_saturates() {
        let linear = StreamFadeRamp::new(Linear);
        for (frame, window, expected) in [
            (0, 10, 0.0),
            (5, 10, 0.5),
            (10, 10, 1.0),
            (20, 10, 1.0),
            (0, 0, 1.0),
        ] {
            assert_eq!(linear.alpha(frame, window), expected);
        }
        let eased = StreamFadeRamp::default();
        assert!(eased.alpha(5, 10) > linear.alpha(5, 10));
        assert_eq!(eased.alpha(0, 0), 1.0);
    }
}
