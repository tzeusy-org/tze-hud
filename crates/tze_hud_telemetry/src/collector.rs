//! Telemetry collector — aggregates frames into bounded session summaries.

use crate::record::{FrameTelemetry, SessionSummary};

/// Aggregates per-frame telemetry records into a session summary.
pub struct TelemetryCollector {
    summary: SessionSummary,
}

impl TelemetryCollector {
    /// Create a collector without a channel (for direct use).
    pub fn new() -> Self {
        Self {
            summary: SessionSummary::new(),
        }
    }

    /// Record a frame telemetry entry directly.
    pub fn record(&mut self, frame: FrameTelemetry) {
        self.summary.total_frames += 1;
        self.summary.frame_time.record(frame.frame_time_us);
        self.summary.record_frame_correctness(&frame);
    }

    /// Get the current session summary.
    pub fn summary(&self) -> &SessionSummary {
        &self.summary
    }
}

impl Default for TelemetryCollector {
    fn default() -> Self {
        Self::new()
    }
}
