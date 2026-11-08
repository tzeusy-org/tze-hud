//! Process-lifetime counters for quiescent-runtime efficiency evidence.
//!
//! Counters are monotonic. A measurement takes a snapshot after settling and
//! subtracts it from a second snapshot after the observation interval, keeping
//! startup/render work outside the idle result without resetting live counters.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeWakeupSource {
    FrameReady,
    SceneChange,
    AnimationDeadline,
    ComposerDeadline,
    TtlDeadline,
    Resize,
    Readback,
    OperatorCapture,
    Shutdown,
}

impl RuntimeWakeupSource {
    const COUNT: usize = 9;

    const fn index(self) -> usize {
        match self {
            Self::FrameReady => 0,
            Self::SceneChange => 1,
            Self::AnimationDeadline => 2,
            Self::ComposerDeadline => 3,
            Self::TtlDeadline => 4,
            Self::Resize => 5,
            Self::Readback => 6,
            Self::OperatorCapture => 7,
            Self::Shutdown => 8,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::FrameReady => "frame_ready",
            Self::SceneChange => "scene_change",
            Self::AnimationDeadline => "animation_deadline",
            Self::ComposerDeadline => "composer_deadline",
            Self::TtlDeadline => "ttl_deadline",
            Self::Resize => "resize",
            Self::Readback => "readback",
            Self::OperatorCapture => "operator_capture",
            Self::Shutdown => "shutdown",
        }
    }

    const ALL: [Self; Self::COUNT] = [
        Self::FrameReady,
        Self::SceneChange,
        Self::AnimationDeadline,
        Self::ComposerDeadline,
        Self::TtlDeadline,
        Self::Resize,
        Self::Readback,
        Self::OperatorCapture,
        Self::Shutdown,
    ];
}

#[derive(Debug, Default)]
pub struct IdleEfficiencyCounters {
    main_sources: [AtomicU64; RuntimeWakeupSource::COUNT],
    compositor_sources: [AtomicU64; RuntimeWakeupSource::COUNT],
    gpu_queue_submissions: AtomicU64,
    surface_acquisitions: AtomicU64,
    presents: AtomicU64,
    /// Microseconds after process start of the last present (0 = none yet).
    last_present_us: AtomicU64,
    excluded_sampler_wakeups: AtomicU64,
    excluded_operating_system_wakeups: AtomicU64,
}

impl IdleEfficiencyCounters {
    pub fn record_main_wakeup(&self, source: RuntimeWakeupSource) {
        self.main_sources[source.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_compositor_wakeup(&self, source: RuntimeWakeupSource) {
        self.compositor_sources[source.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_gpu_submission(&self) {
        self.gpu_queue_submissions.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_surface_acquisition(&self) {
        self.surface_acquisitions.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_present(&self) {
        self.presents.fetch_add(1, Ordering::Relaxed);
        let at = crate::operator::status::process_start()
            .elapsed()
            .as_micros();
        self.last_present_us.store(
            u64::try_from(at).unwrap_or(u64::MAX).max(1),
            Ordering::Relaxed,
        );
    }

    /// Milliseconds since the last present, or since process start before the
    /// first one. A value that keeps growing means the HUD has settled.
    pub fn idle_since_ms(&self) -> u64 {
        let now = crate::operator::status::process_start()
            .elapsed()
            .as_micros();
        self.idle_since_ms_at(u64::try_from(now).unwrap_or(u64::MAX))
    }

    /// `idle_since_ms` at `now_us` since process start.
    fn idle_since_ms_at(&self, now_us: u64) -> u64 {
        now_us.saturating_sub(self.last_present_us.load(Ordering::Relaxed)) / 1_000
    }

    pub fn record_excluded_sampler_wakeup(&self) {
        self.excluded_sampler_wakeups
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_excluded_operating_system_wakeup(&self) {
        self.excluded_operating_system_wakeups
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> IdleEfficiencySnapshot {
        let mut sources = BTreeMap::new();
        let mut main_loop_wakeups = 0_u64;
        let mut compositor_loop_wakeups = 0_u64;
        for source in RuntimeWakeupSource::ALL {
            let main = self.main_sources[source.index()].load(Ordering::Relaxed);
            main_loop_wakeups = main_loop_wakeups.saturating_add(main);
            if main > 0 {
                sources.insert(format!("main.{}", source.label()), main);
            }
            let compositor = self.compositor_sources[source.index()].load(Ordering::Relaxed);
            compositor_loop_wakeups = compositor_loop_wakeups.saturating_add(compositor);
            if compositor > 0 {
                sources.insert(format!("compositor.{}", source.label()), compositor);
            }
        }
        IdleEfficiencySnapshot {
            main_loop_wakeups,
            compositor_loop_wakeups,
            gpu_queue_submissions: self.gpu_queue_submissions.load(Ordering::Relaxed),
            surface_acquisitions: self.surface_acquisitions.load(Ordering::Relaxed),
            presents: self.presents.load(Ordering::Relaxed),
            excluded_sampler_wakeups: self.excluded_sampler_wakeups.load(Ordering::Relaxed),
            excluded_operating_system_wakeups: self
                .excluded_operating_system_wakeups
                .load(Ordering::Relaxed),
            sources,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdleEfficiencySnapshot {
    pub main_loop_wakeups: u64,
    pub compositor_loop_wakeups: u64,
    pub gpu_queue_submissions: u64,
    pub surface_acquisitions: u64,
    pub presents: u64,
    pub excluded_sampler_wakeups: u64,
    pub excluded_operating_system_wakeups: u64,
    pub sources: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("idle efficiency counter {field} moved backwards: baseline={baseline}, current={current}")]
pub struct IdleEfficiencyDeltaError {
    pub field: String,
    pub baseline: u64,
    pub current: u64,
}

impl IdleEfficiencySnapshot {
    pub fn combined_runtime_wakeups(&self) -> u64 {
        self.main_loop_wakeups
            .saturating_add(self.compositor_loop_wakeups)
    }

    pub fn delta_since(&self, baseline: &Self) -> Result<Self, IdleEfficiencyDeltaError> {
        fn delta(
            field: &str,
            current: u64,
            baseline: u64,
        ) -> Result<u64, IdleEfficiencyDeltaError> {
            current
                .checked_sub(baseline)
                .ok_or_else(|| IdleEfficiencyDeltaError {
                    field: field.into(),
                    baseline,
                    current,
                })
        }

        let source_keys = self
            .sources
            .keys()
            .chain(baseline.sources.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut sources = BTreeMap::new();
        for key in source_keys {
            let value = delta(
                &format!("sources.{key}"),
                self.sources.get(&key).copied().unwrap_or(0),
                baseline.sources.get(&key).copied().unwrap_or(0),
            )?;
            if value > 0 {
                sources.insert(key, value);
            }
        }

        Ok(Self {
            main_loop_wakeups: delta(
                "main_loop_wakeups",
                self.main_loop_wakeups,
                baseline.main_loop_wakeups,
            )?,
            compositor_loop_wakeups: delta(
                "compositor_loop_wakeups",
                self.compositor_loop_wakeups,
                baseline.compositor_loop_wakeups,
            )?,
            gpu_queue_submissions: delta(
                "gpu_queue_submissions",
                self.gpu_queue_submissions,
                baseline.gpu_queue_submissions,
            )?,
            surface_acquisitions: delta(
                "surface_acquisitions",
                self.surface_acquisitions,
                baseline.surface_acquisitions,
            )?,
            presents: delta("presents", self.presents, baseline.presents)?,
            excluded_sampler_wakeups: delta(
                "excluded_sampler_wakeups",
                self.excluded_sampler_wakeups,
                baseline.excluded_sampler_wakeups,
            )?,
            excluded_operating_system_wakeups: delta(
                "excluded_operating_system_wakeups",
                self.excluded_operating_system_wakeups,
                baseline.excluded_operating_system_wakeups,
            )?,
            sources,
        })
    }
}

#[cfg(test)]
mod idle_since_tests {
    use super::*;

    #[test]
    fn idle_since_restarts_at_each_present() {
        let counters = IdleEfficiencyCounters::default();
        // Before any present the clock runs from process start.
        assert_eq!(counters.idle_since_ms_at(7_000), 7);
        counters.record_present();
        let first = counters.last_present_us.load(Ordering::Relaxed);
        assert!(first >= 1, "a present is recorded");
        assert_eq!(
            counters.idle_since_ms_at(first + 2_500_000),
            2_500,
            "grows while idle"
        );
        counters.record_present();
        let second = counters.last_present_us.load(Ordering::Relaxed);
        assert!(second >= first);
        assert_eq!(
            counters.idle_since_ms_at(second),
            0,
            "a present restarts the clock"
        );
    }
}
