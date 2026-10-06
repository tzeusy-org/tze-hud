//! Telemetry collector — gathers per-frame records and produces session summaries.

use crate::record::{FrameTelemetry, SessionSummary};

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

    /// Get all recorded frames.
    pub fn records(&self) -> &[FrameTelemetry] {
        &self.records
    }
}

impl Default for TelemetryCollector {
    fn default() -> Self {
        Self::new()
    }
}
