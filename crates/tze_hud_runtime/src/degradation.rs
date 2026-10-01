//! Frame-time degradation controller: one fallback, with hysteresis.
//!
//! When the p95 frame time exceeds the entry threshold over a short window
//! (14ms over 10 frames at 60fps), the runtime switches to
//! [`DegradationLevel::Simplified`]. When p95 stays below the recovery
//! threshold (12ms over 30 frames), or the scene is quiescent for that long,
//! it switches back to [`DegradationLevel::Normal`]. Thresholds scale with the
//! effective display cadence.
//!
//! `DegradationController` is single-threaded; only the frame loop calls it.

use std::collections::VecDeque;

use tze_hud_compositor::CompositorDegradationPolicy;
use tze_hud_protocol::proto::session::{
    DegradationLevel as ProtocolDegradationLevel, DegradationNotice,
};
use tze_hud_protocol::session::RuntimeDegradationLevel;
use tze_hud_telemetry::{DegradationDirection, DegradationEvent};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Number of frames in the trigger rolling window (≈166ms at 60fps).
const TRIGGER_WINDOW: usize = 10;

/// Number of frames in the recovery rolling window (≈500ms at 60fps).
const RECOVERY_WINDOW: usize = 30;

/// Trigger threshold: frame_time_p95 must exceed this to advance a level (µs).
const TRIGGER_THRESHOLD_US: u64 = 14_000; // 14ms

/// Recovery threshold: frame_time_p95 must be below this to recover a level (µs).
const RECOVERY_THRESHOLD_US: u64 = 12_000; // 12ms

/// Immutable cadence-derived thresholds and elapsed windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DegradationEnvelope {
    pub effective_fps: u32,
    pub period_us: u64,
    pub entry_threshold_us: u64,
    pub recovery_threshold_us: u64,
    pub entry_duration_us: u64,
    pub recovery_duration_us: u64,
    pub entry_min_samples: usize,
    pub recovery_min_samples: usize,
}

impl DegradationEnvelope {
    /// Derive the frozen runtime envelope from the validated effective cadence.
    pub fn from_effective_fps(effective_fps: u32) -> Option<Self> {
        if effective_fps == 0 {
            return None;
        }
        let period_us = 1_000_000_u64 / u64::from(effective_fps);
        if period_us == 0 {
            return None;
        }
        let ceil_ratio = |numerator: u64, denominator: u64| {
            period_us
                .checked_mul(numerator)
                .map(|value| value.div_ceil(denominator))
        };
        Some(Self {
            effective_fps,
            period_us,
            entry_threshold_us: ceil_ratio(21, 25)?.min(TRIGGER_THRESHOLD_US),
            recovery_threshold_us: ceil_ratio(18, 25)?.min(RECOVERY_THRESHOLD_US),
            entry_duration_us: period_us.checked_mul(TRIGGER_WINDOW as u64)?,
            recovery_duration_us: period_us.checked_mul(RECOVERY_WINDOW as u64)?,
            entry_min_samples: TRIGGER_WINDOW,
            recovery_min_samples: RECOVERY_WINDOW,
        })
    }
}

/// Resolve the immutable startup cadence from the configured target and a
/// monitor refresh reported in millihertz. Unknown refresh leaves the target
/// unchanged; a known refresh caps it. Millihertz is rounded to the nearest
/// whole presentation cadence because the runtime envelope is integer-Hz.
pub(crate) fn effective_degradation_fps(
    target_fps: u32,
    monitor_refresh_millihz: Option<u32>,
) -> u32 {
    let target_fps = target_fps.max(1);
    monitor_refresh_millihz.map_or(target_fps, |refresh_millihz| {
        let refresh_fps = refresh_millihz.saturating_add(500) / 1_000;
        target_fps.min(refresh_fps.max(1))
    })
}

// ─── Degradation Level ────────────────────────────────────────────────────────

/// Runtime degradation level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DegradationLevel {
    /// Full quality rendering.
    Normal = 0,
    /// Simplified rendering: large textures downscaled, translucency drawn
    /// opaque, animations snapped. Tiles are never hidden.
    Simplified = 1,
}

impl DegradationLevel {
    /// Whether this level is Normal.
    pub fn is_normal(self) -> bool {
        self == Self::Normal
    }

    /// Numeric representation for telemetry.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl std::fmt::Display for DegradationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => write!(f, "Normal"),
            Self::Simplified => write!(f, "Simplified"),
        }
    }
}

// ─── Configuration ────────────────────────────────────────────────────────────

/// Configurable parameters for simplified rendering.
#[derive(Clone, Debug)]
pub struct DegradationConfig {
    /// Textures with linear dimensions exceeding this value (pixels)
    /// are scaled down by `texture_scale_factor`.
    /// Default: 512.
    pub texture_quality_threshold_px: u32,

    /// Scale factor applied to large textures (as a fraction of 1.0).
    /// Default: 0.5 (50% reduction).
    pub texture_scale_factor: f32,
}

impl Default for DegradationConfig {
    fn default() -> Self {
        Self {
            texture_quality_threshold_px: 512,
            texture_scale_factor: 0.5,
        }
    }
}

// ─── Degradation Controller ───────────────────────────────────────────────────

/// Rolling-window degradation state machine.
///
/// Call [`DegradationController::record_frame`] once per frame after the frame
/// completes. The controller evaluates trigger and recovery conditions and
/// advances or recovers the degradation level as needed.
///
/// Query [`DegradationController::level`] to determine what restrictions should
/// be applied to the current frame.
pub struct DegradationController {
    /// Current degradation level.
    level: DegradationLevel,

    /// Ring buffer of recent frame times (µs), capacity = RECOVERY_WINDOW.
    ///
    /// We keep the longer window (30 frames) because it subsumes the shorter
    /// (10 frames). The p95 over the last N entries gives the rolling window.
    frame_times: VecDeque<(u64, u64)>,

    /// Configuration.
    config: DegradationConfig,

    /// Monotonically increasing frame counter for telemetry.
    frame_number: u64,

    /// Frozen startup thresholds and elapsed windows.
    envelope: DegradationEnvelope,

    /// Deterministic clock used by the compatibility `record_frame` API.
    virtual_now_us: u64,

    /// First instant at which the scheduler proved there was no render deadline.
    quiescent_since_us: Option<u64>,
}

impl DegradationController {
    /// Create a new controller starting at Normal.
    pub fn new(config: DegradationConfig) -> Self {
        Self::with_envelope(
            config,
            DegradationEnvelope::from_effective_fps(60).expect("60 Hz envelope is valid"),
        )
    }

    /// Create a controller with a cadence envelope frozen by startup resolution.
    pub fn with_envelope(config: DegradationConfig, envelope: DegradationEnvelope) -> Self {
        Self {
            level: DegradationLevel::Normal,
            frame_times: VecDeque::with_capacity(RECOVERY_WINDOW),
            config,
            frame_number: 0,
            envelope,
            virtual_now_us: 0,
            quiescent_since_us: None,
        }
    }

    /// Create a controller with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(DegradationConfig::default())
    }

    /// The current degradation level.
    pub fn level(&self) -> DegradationLevel {
        self.level
    }

    /// The current configuration.
    pub fn config(&self) -> &DegradationConfig {
        &self.config
    }

    pub fn envelope(&self) -> DegradationEnvelope {
        self.envelope
    }

    /// Record a completed frame's time (in microseconds) and evaluate
    /// trigger / recovery conditions.
    ///
    /// Returns `Some(DegradationEvent)` if the level changed this frame, or
    /// `None` if the level is unchanged.
    ///
    /// This method MUST be called exactly once after every frame completes,
    /// so that the rolling windows advance correctly.
    ///
    /// ## Window semantics
    ///
    /// The controller keeps one ring buffer of recent frame times (capacity =
    /// RECOVERY_WINDOW = 30). After any level change the buffer is cleared so
    /// the recovery window only counts frames observed after the change.
    ///
    /// Trigger evaluation uses a true rolling 10-frame window: checked every
    /// frame once at least 10 samples exist. Recovery evaluation uses a true
    /// rolling 30-frame window: checked every frame once at least 30 samples
    /// exist, matching the spec ("30-frame rolling window").
    pub fn record_frame(&mut self, frame_time_us: u64) -> Option<DegradationEvent> {
        self.virtual_now_us = self.virtual_now_us.saturating_add(self.envelope.period_us);
        self.record_frame_at(frame_time_us, self.virtual_now_us)
    }

    /// Record a successful active frame at an injected monotonic completion time.
    pub fn record_frame_at(
        &mut self,
        frame_time_us: u64,
        completed_at_us: u64,
    ) -> Option<DegradationEvent> {
        self.frame_number += 1;
        self.virtual_now_us = self.virtual_now_us.max(completed_at_us);
        self.quiescent_since_us = None;

        self.frame_times.push_back((completed_at_us, frame_time_us));
        let oldest = completed_at_us.saturating_sub(self.envelope.recovery_duration_us);
        while self.frame_times.front().is_some_and(|(at, _)| *at < oldest) {
            self.frame_times.pop_front();
        }

        let old_level = self.level;

        // ── Trigger: switch to Simplified ─────────────────────────────────────
        let entry_samples = samples_for_window(
            &self.frame_times,
            completed_at_us,
            self.envelope.entry_duration_us,
        );
        if self.level == DegradationLevel::Normal
            && entry_samples.len() >= self.envelope.entry_min_samples
            && window_coverage_us(
                &self.frame_times,
                completed_at_us,
                self.envelope.entry_duration_us,
                self.envelope.period_us,
            ) >= self.envelope.entry_duration_us
        {
            let p95_trigger = p95(&entry_samples);
            if p95_trigger > self.envelope.entry_threshold_us {
                self.level = DegradationLevel::Simplified;
                self.frame_times.clear();
                return Some(DegradationEvent {
                    frame_number: self.frame_number,
                    previous_level: old_level.as_u8(),
                    new_level: self.level.as_u8(),
                    frame_time_p95_us: p95_trigger,
                    direction: DegradationDirection::Advance,
                    sample_count: entry_samples.len() as u32,
                    window_duration_us: self.envelope.entry_duration_us,
                    effective_cadence_hz: self.envelope.effective_fps,
                    entry_threshold_us: self.envelope.entry_threshold_us,
                    recovery_threshold_us: self.envelope.recovery_threshold_us,
                    recovery_source: tze_hud_telemetry::DegradationRecoverySource::ActiveFrames,
                });
            }
        }

        // ── Recovery: switch back to Normal ───────────────────────────────────
        let recovery_samples = samples_for_window(
            &self.frame_times,
            completed_at_us,
            self.envelope.recovery_duration_us,
        );
        if self.level > DegradationLevel::Normal
            && recovery_samples.len() >= self.envelope.recovery_min_samples
            && window_coverage_us(
                &self.frame_times,
                completed_at_us,
                self.envelope.recovery_duration_us,
                self.envelope.period_us,
            ) >= self.envelope.recovery_duration_us
        {
            let p95_recovery = p95(&recovery_samples);

            if p95_recovery < self.envelope.recovery_threshold_us {
                self.level = DegradationLevel::Normal;
                self.frame_times.clear();
                return Some(DegradationEvent {
                    frame_number: self.frame_number,
                    previous_level: old_level.as_u8(),
                    new_level: self.level.as_u8(),
                    frame_time_p95_us: p95_recovery,
                    direction: DegradationDirection::Recover,
                    sample_count: recovery_samples.len() as u32,
                    window_duration_us: self.envelope.recovery_duration_us,
                    effective_cadence_hz: self.envelope.effective_fps,
                    entry_threshold_us: self.envelope.entry_threshold_us,
                    recovery_threshold_us: self.envelope.recovery_threshold_us,
                    recovery_source: tze_hud_telemetry::DegradationRecoverySource::ActiveFrames,
                });
            }
        }

        None
    }

    /// Report a scheduler tick whose canonical predicate proved true quiescence.
    pub fn record_quiescent_at(&mut self, now_us: u64) -> Option<DegradationEvent> {
        self.virtual_now_us = self.virtual_now_us.max(now_us);
        if self.level == DegradationLevel::Normal {
            self.quiescent_since_us = Some(now_us);
            return None;
        }
        let since = *self.quiescent_since_us.get_or_insert(now_us);
        if now_us.saturating_sub(since) < self.envelope.recovery_duration_us {
            return None;
        }
        let old_level = self.level;
        self.level = DegradationLevel::Normal;
        self.quiescent_since_us = Some(now_us);
        self.frame_times.clear();
        Some(DegradationEvent {
            frame_number: self.frame_number,
            previous_level: old_level.as_u8(),
            new_level: self.level.as_u8(),
            frame_time_p95_us: 0,
            direction: DegradationDirection::Recover,
            sample_count: 0,
            window_duration_us: self.envelope.recovery_duration_us,
            effective_cadence_hz: self.envelope.effective_fps,
            entry_threshold_us: self.envelope.entry_threshold_us,
            recovery_threshold_us: self.envelope.recovery_threshold_us,
            recovery_source: tze_hud_telemetry::DegradationRecoverySource::Quiescent,
        })
    }

    /// Next monotonic instant at which quiescent recovery can advance one
    /// level. `None` means either normal operation or quiescence has not yet
    /// been observed by the scheduler.
    pub fn next_quiescent_recovery_at_us(&self) -> Option<u64> {
        (self.level != DegradationLevel::Normal)
            .then(|| {
                self.quiescent_since_us
                    .map(|since| since.saturating_add(self.envelope.recovery_duration_us))
            })
            .flatten()
    }

    /// Number of consecutive frames evaluated so far (for testing / telemetry).
    pub fn frame_number(&self) -> u64 {
        self.frame_number
    }

    /// Build the compositor policy for the frame being built.
    pub fn compositor_policy(&self) -> CompositorDegradationPolicy {
        let level = match self.level {
            DegradationLevel::Normal => tze_hud_scene::DegradationLevel::Nominal,
            DegradationLevel::Simplified => tze_hud_scene::DegradationLevel::Simplified,
        };
        CompositorDegradationPolicy {
            level,
            texture_quality_threshold_px: self.config.texture_quality_threshold_px,
            texture_scale_factor: self.config.texture_scale_factor,
        }
    }

    /// Runtime-to-wire mapping. Simplified maps to `RENDERING_SIMPLIFIED`.
    pub fn protocol_level(&self) -> (RuntimeDegradationLevel, ProtocolDegradationLevel) {
        match self.level {
            DegradationLevel::Normal => (
                RuntimeDegradationLevel::Normal,
                ProtocolDegradationLevel::Normal,
            ),
            DegradationLevel::Simplified => (
                RuntimeDegradationLevel::RenderingSimplified,
                ProtocolDegradationLevel::RenderingSimplified,
            ),
        }
    }

    pub fn protocol_notice(&self, timestamp_wall_us: u64) -> DegradationNotice {
        let (_, level) = self.protocol_level();
        DegradationNotice {
            level: level as i32,
            reason: format!("runtime degradation level changed to {}", self.level),
            affected_capabilities: Vec::new(),
            timestamp_wall_us,
        }
    }
}

// ─── p95 helper ───────────────────────────────────────────────────────────────

/// Compute the p95 of the last `n` values in a ring buffer (VecDeque).
///
/// Panics if `n > deque.len()` — callers must guard with `len() >= n`.
///
/// Uses the nearest-rank method (consistent with [`LatencyBucket::percentile`]).
#[cfg(test)]
fn p95_of_last_n(deque: &VecDeque<(u64, u64)>, n: usize) -> u64 {
    debug_assert!(deque.len() >= n, "caller must ensure len() >= n");
    let samples: Vec<u64> = deque
        .iter()
        .rev()
        .take(n)
        .map(|(_, value)| *value)
        .collect();
    p95(&samples)
}

fn samples_for_window(deque: &VecDeque<(u64, u64)>, now_us: u64, duration_us: u64) -> Vec<u64> {
    let oldest = now_us.saturating_sub(duration_us);
    deque
        .iter()
        .filter(|(at, _)| *at >= oldest && *at <= now_us)
        .map(|(_, value)| *value)
        .collect()
}

fn window_coverage_us(
    deque: &VecDeque<(u64, u64)>,
    now_us: u64,
    duration_us: u64,
    period_us: u64,
) -> u64 {
    let oldest = now_us.saturating_sub(duration_us);
    deque
        .iter()
        .find(|(at, _)| *at >= oldest && *at <= now_us)
        .map_or(0, |(first_at, _)| {
            now_us.saturating_sub(*first_at).saturating_add(period_us)
        })
}

fn p95(samples: &[u64]) -> u64 {
    debug_assert!(!samples.is_empty());
    let mut samples = samples.to_vec();
    samples.sort_unstable();
    let rank = (95 * samples.len()).div_ceil(100);
    let idx = rank.saturating_sub(1).min(samples.len() - 1);
    samples[idx]
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn controller() -> DegradationController {
        DegradationController::with_defaults()
    }

    #[test]
    fn cadence_envelope_preserves_60hz_calibration_and_tightens_faster_periods() {
        let sixty = DegradationEnvelope::from_effective_fps(60).expect("valid cadence");
        assert_eq!(sixty.entry_threshold_us, 14_000);
        assert_eq!(sixty.recovery_threshold_us, 12_000);
        assert_eq!(sixty.entry_min_samples, 10);
        assert_eq!(sixty.recovery_min_samples, 30);

        let faster = DegradationEnvelope::from_effective_fps(75).expect("valid cadence");
        assert!(faster.entry_threshold_us < sixty.entry_threshold_us);
        assert!(faster.recovery_threshold_us < sixty.recovery_threshold_us);
    }

    #[test]
    fn effective_cadence_is_capped_only_when_monitor_refresh_is_known() {
        assert_eq!(effective_degradation_fps(120, None), 120);
        assert_eq!(effective_degradation_fps(120, Some(60_000)), 60);
        assert_eq!(effective_degradation_fps(120, Some(59_940)), 60);
        assert_eq!(effective_degradation_fps(30, Some(60_000)), 30);
    }

    #[test]
    fn elapsed_window_blocks_burst_samples_and_quiescence_recovers_without_frames() {
        let envelope = DegradationEnvelope::from_effective_fps(60).expect("valid cadence");
        let mut ctrl = DegradationController::with_envelope(DegradationConfig::default(), envelope);
        for i in 0..10 {
            assert!(ctrl.record_frame_at(20_000, 1_000 + i).is_none());
        }
        assert_eq!(ctrl.level(), DegradationLevel::Normal);

        let mut now = 0;
        for _ in 0..10 {
            now += envelope.period_us;
            let _ = ctrl.record_frame_at(20_000, now);
        }
        assert_eq!(ctrl.level(), DegradationLevel::Simplified);
        assert!(ctrl.record_quiescent_at(now).is_none());
        assert_eq!(
            ctrl.next_quiescent_recovery_at_us(),
            Some(now + envelope.recovery_duration_us),
            "quiescent recovery must expose its monotonic wake deadline"
        );
        assert!(
            ctrl.record_quiescent_at(now + envelope.recovery_duration_us - 1)
                .is_none()
        );
        let recovered = ctrl
            .record_quiescent_at(now + envelope.recovery_duration_us)
            .expect("one quiescent recovery step");
        assert_eq!(recovered.new_level, DegradationLevel::Normal.as_u8());
        assert_eq!(ctrl.next_quiescent_recovery_at_us(), None);
    }

    #[test]
    fn production_degradation_sustained_payload_emits_machine_readable_deadline_evidence() {
        let started = std::time::Instant::now();
        let envelope = DegradationEnvelope::from_effective_fps(60).expect("valid cadence");
        let mut ctrl = DegradationController::with_envelope(DegradationConfig::default(), envelope);
        let mut selected_at_us = 0;
        let mut transition = None;

        for sample_index in 1..=envelope.entry_min_samples {
            selected_at_us = sample_index as u64 * envelope.period_us;
            transition = ctrl.record_frame_at(envelope.entry_threshold_us + 1, selected_at_us);
        }

        let transition = transition.expect("sustained over-budget load must select Simplified");
        assert!(
            selected_at_us <= envelope.entry_duration_us,
            "transition must be selected within the cadence-derived deadline"
        );
        assert_eq!(transition.previous_level, DegradationLevel::Normal.as_u8());
        assert_eq!(transition.new_level, DegradationLevel::Simplified.as_u8());

        println!(
            "{}",
            serde_json::json!({
                "artifact": "production_degradation_sustained_payload",
                "status": "pass",
                "effective_cadence_hz": envelope.effective_fps,
                "entry_threshold_us": envelope.entry_threshold_us,
                "entry_deadline_us": envelope.entry_duration_us,
                "selected_at_us": selected_at_us,
                "sample_count": transition.sample_count,
                "from_level": transition.previous_level,
                "to_level": transition.new_level,
                "validation_wall_time_us": started.elapsed().as_micros() as u64,
            })
        );
    }

    /// Push `n` frames of `frame_time_us` through the controller without
    /// caring about transition events.
    fn push_frames(ctrl: &mut DegradationController, frame_time_us: u64, n: usize) {
        for _ in 0..n {
            ctrl.record_frame(frame_time_us);
        }
    }

    // ── Level invariants ──────────────────────────────────────────────────────

    #[test]
    fn test_starts_at_normal() {
        let ctrl = controller();
        assert_eq!(ctrl.level(), DegradationLevel::Normal);
    }

    // ── Trigger: sustained overbudget → advance ───────────────────────────────

    #[test]
    fn test_trigger_advances_level_after_10_frames_over_14ms() {
        let mut ctrl = controller();
        // 10 frames all at 20ms — p95 = 20ms > 14ms → must advance.
        let event = push_and_get_last_event(&mut ctrl, 20_000, TRIGGER_WINDOW);
        assert!(event.is_some(), "Expected a degradation advance event");
        let ev = event.unwrap();
        assert_eq!(ev.previous_level, 0); // Normal
        assert_eq!(ev.new_level, 1); // Simplified
        assert_eq!(ev.direction, DegradationDirection::Advance);
        assert_eq!(ctrl.level(), DegradationLevel::Simplified);
    }

    #[test]
    fn test_trigger_requires_full_10_frame_window() {
        let mut ctrl = controller();
        // Only 9 frames — must NOT trigger yet.
        for _ in 0..(TRIGGER_WINDOW - 1) {
            let ev = ctrl.record_frame(20_000);
            assert!(
                ev.is_none(),
                "Should not trigger before full 10-frame window"
            );
        }
        assert_eq!(ctrl.level(), DegradationLevel::Normal);
        // 10th frame — now should trigger.
        let ev = ctrl.record_frame(20_000);
        assert!(ev.is_some());
        assert_eq!(ctrl.level(), DegradationLevel::Simplified);
    }

    // ── Transient spike tolerance ─────────────────────────────────────────────

    #[test]
    fn test_partial_window_does_not_trigger_degradation() {
        // Spec: trigger requires p95 > 14ms over the 10-frame ROLLING WINDOW.
        // Before accumulating 10 frames, the system MUST NOT trigger,
        // regardless of how large individual frames are.
        let mut ctrl = controller();
        // Push 9 frames well above the threshold — but no trigger yet (window not full).
        for i in 0..9 {
            let ev = ctrl.record_frame(50_000);
            assert!(
                ev.is_none(),
                "Frame {i}: must not trigger before 10-frame window is full"
            );
        }
        assert_eq!(ctrl.level(), DegradationLevel::Normal);
    }

    #[test]
    fn test_10th_frame_first_possible_trigger() {
        let mut ctrl = controller();
        push_frames(&mut ctrl, 50_000, 9);
        // The 10th frame is the FIRST point at which the trigger can fire.
        let ev = ctrl.record_frame(50_000);
        assert!(
            ev.is_some(),
            "10th frame above threshold should trigger degradation"
        );
        assert_eq!(ctrl.level(), DegradationLevel::Simplified);
    }

    #[test]
    fn test_p95_boundary_does_not_trigger_at_exactly_14ms() {
        let mut ctrl = controller();
        // All 10 frames at exactly 14ms — p95 = 14ms, NOT > 14ms. No trigger.
        push_frames(&mut ctrl, TRIGGER_THRESHOLD_US, TRIGGER_WINDOW);
        assert_eq!(ctrl.level(), DegradationLevel::Normal);
    }

    // ── Hysteresis / recovery ─────────────────────────────────────────────────

    #[test]
    fn test_recovery_requires_30_frames_under_12ms() {
        let mut ctrl = controller();
        // Force to Simplified by triggering.
        push_frames(&mut ctrl, 20_000, TRIGGER_WINDOW);
        assert_eq!(ctrl.level(), DegradationLevel::Simplified);

        // 29 clean frames — must NOT recover yet.
        for i in 0..(RECOVERY_WINDOW - 1) {
            let ev = ctrl.record_frame(5_000);
            assert!(
                ev.is_none(),
                "Frame {i}: should not recover before 30 frames"
            );
        }
        assert_eq!(ctrl.level(), DegradationLevel::Simplified);

        // 30th clean frame — should trigger recovery.
        let ev = ctrl.record_frame(5_000);
        assert!(ev.is_some(), "Should recover after 30 clean frames");
        let ev = ev.unwrap();
        assert_eq!(ev.previous_level, 1); // Simplified
        assert_eq!(ev.new_level, 0); // Normal
        assert_eq!(ev.direction, DegradationDirection::Recover);
        assert_eq!(ctrl.level(), DegradationLevel::Normal);
    }

    #[test]
    fn test_recovery_threshold_exactly_12ms_does_not_recover() {
        let mut ctrl = controller();
        // Force to Simplified.
        push_frames(&mut ctrl, 20_000, TRIGGER_WINDOW);
        assert_eq!(ctrl.level(), DegradationLevel::Simplified);

        // 30 frames at exactly 12ms — p95 = 12ms, NOT < 12ms. Must not recover.
        push_frames(&mut ctrl, RECOVERY_THRESHOLD_US, RECOVERY_WINDOW);
        assert_eq!(ctrl.level(), DegradationLevel::Simplified);
    }

    #[test]
    fn simplified_maps_to_compositor_and_protocol_simplified() {
        let mut ctrl = controller();
        assert_eq!(
            ctrl.compositor_policy().level,
            tze_hud_scene::DegradationLevel::Nominal
        );
        assert_eq!(ctrl.protocol_level().1, ProtocolDegradationLevel::Normal);

        push_frames(&mut ctrl, 20_000, TRIGGER_WINDOW);
        assert_eq!(
            ctrl.compositor_policy().level,
            tze_hud_scene::DegradationLevel::Simplified
        );
        assert_eq!(
            ctrl.protocol_level().1,
            ProtocolDegradationLevel::RenderingSimplified
        );
        assert!(ctrl.protocol_notice(1).affected_capabilities.is_empty());
    }

    #[test]
    fn sustained_overload_does_not_escalate_past_simplified() {
        let mut ctrl = controller();
        push_frames(&mut ctrl, 20_000, TRIGGER_WINDOW * 5);
        assert_eq!(ctrl.level(), DegradationLevel::Simplified);
    }

    // ── Telemetry events ──────────────────────────────────────────────────────

    #[test]
    fn test_advance_event_has_correct_fields() {
        let mut ctrl = controller();
        let ev = push_and_get_last_event(&mut ctrl, 20_000, TRIGGER_WINDOW).unwrap();
        assert_eq!(ev.previous_level, 0);
        assert_eq!(ev.new_level, 1);
        assert_eq!(ev.direction, DegradationDirection::Advance);
        assert!(
            ev.frame_time_p95_us > TRIGGER_THRESHOLD_US,
            "p95 should exceed trigger threshold"
        );
    }

    #[test]
    fn test_recover_event_has_correct_fields() {
        let mut ctrl = controller();
        push_frames(&mut ctrl, 20_000, TRIGGER_WINDOW); // advance to Simplified
        let ev = push_and_get_last_event(&mut ctrl, 5_000, RECOVERY_WINDOW).unwrap();
        assert_eq!(ev.previous_level, 1);
        assert_eq!(ev.new_level, 0);
        assert_eq!(ev.direction, DegradationDirection::Recover);
        assert!(
            ev.frame_time_p95_us < RECOVERY_THRESHOLD_US,
            "p95 should be below recovery threshold"
        );
    }

    // ── p95 helper ────────────────────────────────────────────────────────────

    #[test]
    fn test_p95_helper_correctness() {
        let mut deque: VecDeque<(u64, u64)> = VecDeque::new();
        for i in 1..=10u64 {
            deque.push_back((i, i * 1000));
        }
        // Values: [1000, 2000, ..., 10000]
        // p95 nearest-rank: ceil(0.95*10) = 10 → index 9 → 10000
        assert_eq!(p95_of_last_n(&deque, 10), 10_000);
        // p95 of last 5: [6000, 7000, 8000, 9000, 10000]
        // ceil(0.95*5) = 5 → index 4 → 10000
        assert_eq!(p95_of_last_n(&deque, 5), 10_000);
    }

    // ── Helper ────────────────────────────────────────────────────────────────

    /// Push n frames and return the last non-None event (or None if no events).
    fn push_and_get_last_event(
        ctrl: &mut DegradationController,
        frame_time_us: u64,
        n: usize,
    ) -> Option<DegradationEvent> {
        let mut last = None;
        for _ in 0..n {
            if let Some(ev) = ctrl.record_frame(frame_time_us) {
                last = Some(ev);
            }
        }
        last
    }
}
