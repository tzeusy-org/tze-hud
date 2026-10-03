//! Telemetry collector — gathers per-frame records and produces session summaries.

use crate::record::{FrameTelemetry, SessionSummary};
use std::sync::Arc;
use tze_hud_scene::clock::{Clock, SystemClock};

/// Aggregates per-frame telemetry records into a session summary.
pub struct TelemetryCollector {
    records: Vec<FrameTelemetry>,
    summary: SessionSummary,
}

impl TelemetryCollector {
    /// Create a collector without a channel (for direct use).
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
            summary: SessionSummary::new(),
        }
    }

    /// Record a frame telemetry entry directly.
    pub fn record(&mut self, frame: FrameTelemetry) {
        self.summary.total_frames += 1;
        self.summary.frame_time.record(frame.frame_time_us);
        self.summary.record_frame_correctness(&frame);
        self.records.push(frame);
    }

    /// Get the current session summary.
    pub fn summary(&self) -> &SessionSummary {
        &self.summary
    }

    /// Get a mutable reference to the session summary (for recording latencies).
    pub fn summary_mut(&mut self) -> &mut SessionSummary {
        &mut self.summary
    }

    /// Get all recorded frames.
    pub fn records(&self) -> &[FrameTelemetry] {
        &self.records
    }

    /// Emit session summary as JSON.
    pub fn emit_json(&self) -> Result<String, serde_json::Error> {
        self.summary.to_json()
    }
}

impl Default for TelemetryCollector {
    fn default() -> Self {
        Self::new()
    }
}

// ─── FrameRecorder ───────────────────────────────────────────────────────────

/// Stamps a `FrameTelemetry` record with `timestamp_us` drawn from an
/// injectable [`Clock`].
///
/// The compositor creates one `FrameRecorder` per session.  In production it
/// uses [`SystemClock`]; tests inject a [`tze_hud_scene::clock::TestClock`]
/// for fully deterministic timestamp assertions.
///
/// ```
/// use tze_hud_telemetry::collector::FrameRecorder;
/// use tze_hud_scene::clock::TestClock;
/// use std::sync::Arc;
///
/// // Clock at t=1000 ms → timestamp_us = 1000 * 1000 = 1_000_000 µs
/// let clock = TestClock::new(1_000);
/// let recorder = FrameRecorder::new_with_clock(Arc::new(clock));
/// let frame = recorder.begin_frame(1);
/// assert_eq!(frame.timestamp_us, 1_000_000);
/// ```
pub struct FrameRecorder {
    clock: Arc<dyn Clock>,
}

impl FrameRecorder {
    /// Create a `FrameRecorder` backed by the real system clock.
    pub fn new() -> Self {
        Self {
            clock: Arc::new(SystemClock::new()),
        }
    }

    /// Create a `FrameRecorder` with an injected clock (for testing).
    pub fn new_with_clock(clock: Arc<dyn Clock>) -> Self {
        Self { clock }
    }

    /// Begin a new frame: returns a [`FrameTelemetry`] pre-stamped with
    /// `timestamp_us` from the injected clock.
    ///
    /// The timestamp is expressed as microseconds (the clock provides
    /// milliseconds; we multiply by 1000 to match the `_us` convention used
    /// throughout the struct).
    pub fn begin_frame(&self, frame_number: u64) -> FrameTelemetry {
        let mut frame = FrameTelemetry::new(frame_number);
        // Clock provides milliseconds; FrameTelemetry uses microseconds.
        frame.timestamp_us = self.clock.now_millis().saturating_mul(1_000);
        frame
    }
}

impl Default for FrameRecorder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::clock::TestClock;

    #[test]
    fn test_frame_recorder_stamps_timestamp() {
        let clock = TestClock::new(2_500); // 2500 ms
        let recorder = FrameRecorder::new_with_clock(Arc::new(clock.clone()));

        let frame = recorder.begin_frame(42);
        assert_eq!(frame.frame_number, 42);
        // 2500 ms * 1000 = 2_500_000 µs
        assert_eq!(frame.timestamp_us, 2_500_000);

        clock.advance(100);
        let frame2 = recorder.begin_frame(43);
        assert_eq!(frame2.timestamp_us, 2_600_000);
    }

    #[test]
    fn test_frame_recorder_default_uses_system_clock() {
        let recorder = FrameRecorder::new();
        let frame = recorder.begin_frame(1);
        // System clock returns a nonzero epoch timestamp.
        assert!(frame.timestamp_us > 0);
    }
}
