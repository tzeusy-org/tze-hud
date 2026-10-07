//! Motion-polish primitives for the portal tile render path (hud-bq0gl.10).
//!
//! This module hosts the **pure, deterministic** building blocks for compositor
//! animation: easing curves, scalar interpolation, frame-rate-
//! independent scroll smoothing, and a streaming-reveal fade ramp. The runtime
//! never sits in the frame loop (RFC 0013 §3 — *arrival time ≠ presentation
//! time*): adapters set targets (scroll offset, content), and these primitives
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
/// remains authoritative (RFC 0013 §3.2): the target is never altered here, only
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
    pub(crate) fn new(x: f32, y: f32) -> Self {
        Self {
            displayed_x: x,
            displayed_y: y,
            tau_ms: SCROLL_SMOOTH_TAU_MS,
            snap_epsilon: SCROLL_SNAP_EPSILON_PX,
        }
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
    /// it and produce no further motion. The idle render gate (hud-ilivg) uses
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

    const EPS: f32 = 1e-5;

    // ── Easing curves ───────────────────────────────────────────────────────

    #[test]
    fn easing_fixed_endpoints() {
        for e in [
            Easing::Linear,
            Easing::EaseInOut,
            Easing::EaseOutQuad,
            Easing::EaseInQuad,
        ] {
            assert!((e.apply(0.0) - 0.0).abs() < EPS, "{e:?} f(0) != 0");
            assert!((e.apply(1.0) - 1.0).abs() < EPS, "{e:?} f(1) != 1");
        }
    }

    #[test]
    fn easing_clamps_out_of_range() {
        for e in [
            Easing::Linear,
            Easing::EaseInOut,
            Easing::EaseOutQuad,
            Easing::EaseInQuad,
        ] {
            assert!(
                (e.apply(-5.0) - 0.0).abs() < EPS,
                "{e:?} below-0 not clamped"
            );
            assert!(
                (e.apply(5.0) - 1.0).abs() < EPS,
                "{e:?} above-1 not clamped"
            );
        }
    }

    #[test]
    fn ease_in_out_is_symmetric_half() {
        // Smoothstep is symmetric about t=0.5 → f(0.5) == 0.5.
        assert!((Easing::EaseInOut.apply(0.5) - 0.5).abs() < EPS);
        // Symmetry: f(t) + f(1-t) == 1.
        for &t in &[0.1f32, 0.25, 0.4] {
            let s = Easing::EaseInOut.apply(t) + Easing::EaseInOut.apply(1.0 - t);
            assert!((s - 1.0).abs() < EPS, "asymmetric at t={t}");
        }
    }

    #[test]
    fn ease_out_quad_decelerates() {
        // Ease-out is ahead of linear in the first half (fast start).
        assert!(Easing::EaseOutQuad.apply(0.25) > 0.25);
        assert!((Easing::EaseOutQuad.apply(0.5) - 0.75).abs() < EPS);
        assert!((Easing::EaseInQuad.apply(0.25) - 0.0625).abs() < EPS);
        assert!((Easing::EaseInQuad.apply(0.5) - 0.25).abs() < EPS);
        let profile = ZoneMotion::from_token_map(&std::collections::HashMap::from([
            ("motion.enter.easing".into(), "decelerate".into()),
            ("motion.exit.easing".into(), "accelerate".into()),
            ("motion.exit.ms".into(), "240".into()),
        ]));
        assert_eq!(profile.enter, Easing::EaseOutQuad);
        assert_eq!(profile.exit, Easing::EaseInQuad);
        assert_eq!(profile.exit_ms, 240);
        let fallback = ZoneMotion::from_token_map(&std::collections::HashMap::from([
            ("motion.enter.easing".into(), "invalid".into()),
            ("motion.exit.easing".into(), "invalid".into()),
            ("motion.exit.ms".into(), "-10".into()),
        ]));
        assert_eq!(fallback.enter, Easing::EaseOutQuad);
        assert_eq!(fallback.exit, Easing::EaseInQuad);
        assert_eq!(fallback.exit_ms, 120);
        let linear = ZoneMotion::from_token_map(&std::collections::HashMap::from([
            ("motion.enter.easing".into(), "linear".into()),
            ("motion.exit.easing".into(), "standard".into()),
        ]));
        assert_eq!(linear.enter, Easing::Linear);
        assert_eq!(linear.exit, Easing::EaseInOut);
    }

    #[test]
    fn easing_is_monotonic_non_decreasing() {
        for e in [
            Easing::Linear,
            Easing::EaseInOut,
            Easing::EaseOutQuad,
            Easing::EaseInQuad,
        ] {
            let mut prev = e.apply(0.0);
            for i in 1..=100 {
                let v = e.apply(i as f32 / 100.0);
                assert!(
                    v + EPS >= prev,
                    "{e:?} not monotonic at i={i}: {v} < {prev}"
                );
                prev = v;
            }
        }
    }

    // ── Interpolation ───────────────────────────────────────────────────────

    #[test]
    fn lerp_endpoints_and_mid() {
        assert!((lerp(10.0, 20.0, 0.0) - 10.0).abs() < EPS);
        assert!((lerp(10.0, 20.0, 1.0) - 20.0).abs() < EPS);
        assert!((lerp(10.0, 20.0, 0.5) - 15.0).abs() < EPS);
    }

    // ── Exponential smoothing factor ────────────────────────────────────────

    #[test]
    fn smooth_factor_endpoints() {
        assert!((exp_smooth_factor(0.0, 90.0) - 0.0).abs() < EPS);
        assert!(exp_smooth_factor(10_000.0, 90.0) > 0.99);
        // dt == tau → 1 - 1/e.
        let expected = 1.0 - std::f32::consts::E.recip();
        assert!((exp_smooth_factor(90.0, 90.0) - expected).abs() < 1e-4);
    }

    #[test]
    fn smooth_factor_monotonic_in_dt() {
        let mut prev = exp_smooth_factor(0.0, 90.0);
        for i in 1..=200 {
            let v = exp_smooth_factor(i as f32, 90.0);
            assert!(v + EPS >= prev, "not monotonic at dt={i}");
            prev = v;
        }
    }

    #[test]
    fn smooth_factor_non_positive_tau_snaps() {
        assert!((exp_smooth_factor(5.0, 0.0) - 1.0).abs() < EPS);
        assert!((exp_smooth_factor(5.0, -10.0) - 1.0).abs() < EPS);
    }

    // ── ScrollSmoother ──────────────────────────────────────────────────────

    #[test]
    fn scroll_smoother_starts_settled() {
        let s = ScrollSmoother::new(12.0, 34.0);
        assert_eq!(s.displayed(), (12.0, 34.0));
        // Freshly constructed on its offset → settled there (idle render gate).
        assert!(s.is_settled(12.0, 34.0));
    }

    #[test]
    fn scroll_smoother_is_settled_tracks_inflight_state() {
        // Settled on the construction offset; not settled once the target moves
        // far away; settles again once the displayed offset converges onto it.
        // This is the predicate the idle render gate (hud-ilivg) uses to tell a
        // still-catching-up smoother (keep rendering) from a finished one (idle).
        let mut s = ScrollSmoother::new(0.0, 0.0);
        assert!(s.is_settled(0.0, 0.0));

        // Target jumps 500px away: catch-up in flight, not yet settled.
        s.advance(0.0, 500.0, 16.6);
        assert!(
            !s.is_settled(0.0, 500.0),
            "a mid-flight smoother must not report settled"
        );

        // Drive it to the target; once within snap_epsilon it reports settled.
        let mut frames = 0;
        while !s.is_settled(0.0, 500.0) {
            s.advance(0.0, 500.0, 16.6);
            frames += 1;
            assert!(frames < 240, "did not settle within 240 frames");
        }
        assert!(s.is_settled(0.0, 500.0));
    }

    #[test]
    fn scroll_smoother_converges_to_target() {
        let mut s = ScrollSmoother::new(0.0, 0.0);
        // 16.6ms frames toward a target 500px away — must converge within ~2s.
        let mut frames = 0;
        loop {
            let (_, y) = s.advance(0.0, 500.0, 16.6);
            frames += 1;
            if (y - 500.0).abs() < EPS {
                break;
            }
            assert!(frames < 240, "did not converge within 240 frames");
        }
        assert!((s.displayed().1 - 500.0).abs() < EPS);
    }

    #[test]
    fn scroll_smoother_monotonic_approach() {
        let mut s = ScrollSmoother::new(0.0, 0.0);
        let mut prev = 0.0;
        for _ in 0..30 {
            let (_, y) = s.advance(0.0, 100.0, 16.6);
            assert!(y + EPS >= prev, "overshoot/regress: {y} < {prev}");
            assert!(y <= 100.0 + EPS, "overshoot past target: {y}");
            prev = y;
        }
    }

    #[test]
    fn scroll_smoother_frame_rate_independent() {
        // One 100ms step vs many small steps summing to 100ms land close.
        let mut coarse = ScrollSmoother::new(0.0, 0.0);
        coarse.advance(0.0, 100.0, 100.0);

        let mut fine = ScrollSmoother::new(0.0, 0.0);
        for _ in 0..100 {
            fine.advance(0.0, 100.0, 1.0);
        }
        assert!(
            (coarse.displayed().1 - fine.displayed().1).abs() < 1.0,
            "coarse {} vs fine {}",
            coarse.displayed().1,
            fine.displayed().1
        );
    }

    #[test]
    fn scroll_smoother_snaps_within_epsilon() {
        let mut s = ScrollSmoother::new(99.9, 0.0);
        let (x, _) = s.advance(100.0, 0.0, 16.6);
        assert_eq!(x, 100.0, "should snap exactly when within epsilon");
    }

    // ── StreamFadeRamp ──────────────────────────────────────────────────────

    #[test]
    fn stream_fade_ramps_zero_to_one() {
        let ramp = StreamFadeRamp::new(Easing::Linear);
        assert!((ramp.alpha(0, 10) - 0.0).abs() < EPS);
        assert!((ramp.alpha(5, 10) - 0.5).abs() < EPS);
        assert!((ramp.alpha(10, 10) - 1.0).abs() < EPS);
    }

    #[test]
    fn stream_fade_eased_is_ahead_of_linear_midway() {
        // EaseOutQuad (default) reveals faster early than linear.
        let ramp = StreamFadeRamp::default();
        assert!(ramp.alpha(5, 10) > 0.5);
    }

    #[test]
    fn stream_fade_zero_window_is_full() {
        let ramp = StreamFadeRamp::default();
        assert!((ramp.alpha(0, 0) - 1.0).abs() < EPS);
    }

    #[test]
    fn stream_fade_saturates_after_window() {
        let ramp = StreamFadeRamp::new(Easing::Linear);
        assert!((ramp.alpha(20, 10) - 1.0).abs() < EPS);
    }
}
