//! Per-frame event batch assembly (spec.md §8.3–8.4 / RFC 0004 §8.3, §8.4).
//!
//! # Design
//!
//! `EventBatchAssembler` collects `(agent_namespace, InputEnvelope)` pairs
//! emitted by the dispatch pipeline (Stages 1+2) and assembles them into
//! per-agent `EventBatch` values at frame-end.
//!
//! ## Invariants (from spec)
//! - Multiple events for the same agent in a single frame → one `EventBatch`.
//! - Events within a batch are sorted by `timestamp_mono_us` ascending.
//! - `EventBatch.frame_number` = compositor frame counter.
//! - `EventBatch.batch_ts_wall_us` = wall-clock UTC µs at batch assembly time.
//! - Events are coalesced (via `FrameCoalescer`) before sorting.
//!
//! ## Latency
//! Enqueueing one event is O(1). `assemble_frame` is O(E log E) over all events
//! in the frame (dominated by the sort). The target is < 2ms from Stage 2
//! completion to enqueue (spec.md line 356-358).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use tze_hud_scene::WallUs;

use crate::coalescing::FrameCoalescer;
use crate::envelope::InputEnvelope;

/// A ready-to-deliver batch of events for one agent in one compositor frame.
///
/// Mirrors the `EventBatch` protobuf message from `events.proto`.
#[derive(Debug, Clone)]
pub struct EventBatch {
    /// Agent namespace this batch is destined for.
    pub namespace: String,
    /// Compositor frame number (monotonically increasing).
    pub frame_number: u64,
    /// Batch assembly timestamp: wall-clock UTC microseconds (RFC 0003 §1.1).
    pub batch_ts_wall_us: WallUs,
    /// Events sorted by `timestamp_mono_us` ascending.
    pub events: Vec<InputEnvelope>,
}

/// Assembles per-frame event batches for all agents.
///
/// Typical per-frame lifecycle:
/// ```ignore
/// // Called by the dispatch pipeline for each produced event:
/// assembler.push("agent-ns", envelope);
///
/// // Called once at frame end:
/// let batches = assembler.assemble_frame(frame_number);
/// // `batches` is empty if no events arrived this frame.
/// ```
pub struct EventBatchAssembler {
    /// Per-agent accumulator: agent_namespace → FrameCoalescer.
    pending: HashMap<String, FrameCoalescer>,
}

impl EventBatchAssembler {
    /// Create a new assembler with no pending events.
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    /// Enqueue one event for the given agent.
    ///
    /// This is the hot path — called after every Stage-2 hit-test result.
    /// O(1) amortized. Avoids a `String` allocation on the common path when
    /// the namespace is already present in the pending map.
    pub fn push(&mut self, namespace: &str, envelope: InputEnvelope) {
        // Fast path: agent already has a pending coalescer this frame.
        if let Some(coalescer) = self.pending.get_mut(namespace) {
            coalescer.push(envelope);
            return;
        }
        // Slow path: first event for this agent in this frame.
        self.pending
            .entry(namespace.to_owned())
            .or_default()
            .push(envelope);
    }

    /// Assemble and return all pending batches, clearing the internal state.
    ///
    /// Called once per compositor frame, after all Stage-2 events have been
    /// pushed. Returns one `EventBatch` per agent that received at least one
    /// event this frame.
    ///
    /// Events within each batch are sorted by `timestamp_mono_us` ascending
    /// (spec.md line 331).
    pub fn assemble_frame(&mut self, frame_number: u64) -> Vec<EventBatch> {
        if self.pending.is_empty() {
            return Vec::new();
        }

        let batch_ts_wall_us = wall_clock_us();
        let pending = std::mem::take(&mut self.pending);

        pending
            .into_iter()
            .map(|(namespace, coalescer)| {
                let mut events = coalescer.into_events();
                // Sort ascending by hardware timestamp (spec.md line 331).
                events.sort_unstable_by_key(|e| e.timestamp_mono_us());
                EventBatch {
                    namespace,
                    frame_number,
                    batch_ts_wall_us,
                    events,
                }
            })
            .collect()
    }

    /// Returns `true` if there are pending events for any agent.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

impl Default for EventBatchAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns current wall-clock time as UTC microseconds since Unix epoch.
///
/// Guaranteed non-zero: `0` is the "not set" sentinel (timing-model/spec.md
/// §Zero-value semantics). On clock error (pre-epoch), returns `WallUs(1)`.
fn wall_clock_us() -> WallUs {
    let us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_micros()).ok())
        .unwrap_or(1);
    WallUs(if us == 0 { 1 } else { us })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{InputEnvelope, PointerDownData, PointerMoveData, PointerUpData};
    use tze_hud_scene::{MonoUs, SceneId};

    fn null_id() -> SceneId {
        SceneId::null()
    }

    fn make_move(ts: u64, x: f32, y: f32) -> InputEnvelope {
        InputEnvelope::PointerMove(PointerMoveData {
            tile_id: null_id(),
            node_id: null_id(),
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(ts),
            device_id: String::new(),
            local_x: x,
            local_y: y,
            display_x: x,
            display_y: y,
        })
    }

    fn make_down(ts: u64) -> InputEnvelope {
        InputEnvelope::PointerDown(PointerDownData {
            tile_id: null_id(),
            node_id: null_id(),
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(ts),
            device_id: String::new(),
            local_x: 0.0,
            local_y: 0.0,
            display_x: 0.0,
            display_y: 0.0,
            button: 0,
        })
    }

    fn make_up(ts: u64) -> InputEnvelope {
        InputEnvelope::PointerUp(PointerUpData {
            tile_id: null_id(),
            node_id: null_id(),
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(ts),
            device_id: String::new(),
            local_x: 0.0,
            local_y: 0.0,
            display_x: 0.0,
            display_y: 0.0,
            button: 0,
        })
    }

    // ─── Acceptance scenario: two events same agent same frame → one batch ──

    #[test]
    fn test_two_events_same_agent_same_frame_one_batch() {
        let mut assembler = EventBatchAssembler::new();
        // Two pointer events for agent "alpha" within the same frame
        assembler.push("alpha", make_down(1000));
        assembler.push("alpha", make_up(2000));

        let batches = assembler.assemble_frame(42);
        assert_eq!(
            batches.len(),
            1,
            "should produce exactly one batch for agent alpha"
        );
        let batch = &batches[0];
        assert_eq!(batch.namespace, "alpha");
        assert_eq!(batch.frame_number, 42);
        assert_eq!(batch.events.len(), 2, "both events should be in the batch");
        // Events ordered by timestamp ascending
        assert_eq!(batch.events[0].timestamp_mono_us(), MonoUs(1000));
        assert_eq!(batch.events[1].timestamp_mono_us(), MonoUs(2000));
    }

    // ─── Events sorted by timestamp_mono_us ascending ───────────────────────

    #[test]
    fn test_events_sorted_by_timestamp_ascending() {
        let mut assembler = EventBatchAssembler::new();
        // Push out-of-order (down at 3000, then up at 1000)
        assembler.push("alpha", make_down(3000));
        assembler.push("alpha", make_up(1000));

        let batches = assembler.assemble_frame(1);
        let batch = &batches[0];
        assert_eq!(batch.events[0].timestamp_mono_us(), MonoUs(1000));
        assert_eq!(batch.events[1].timestamp_mono_us(), MonoUs(3000));
    }

    // ─── Two agents → two separate batches ──────────────────────────────────

    #[test]
    fn test_two_agents_produce_two_batches() {
        let mut assembler = EventBatchAssembler::new();
        assembler.push("alpha", make_down(1000));
        assembler.push("beta", make_down(2000));

        let batches = assembler.assemble_frame(7);
        assert_eq!(batches.len(), 2);
        let ns: Vec<&str> = batches.iter().map(|b| b.namespace.as_str()).collect();
        assert!(ns.contains(&"alpha"));
        assert!(ns.contains(&"beta"));
    }

    // ─── Frame cleared after assemble ───────────────────────────────────────

    #[test]
    fn test_assemble_clears_pending_state() {
        let mut assembler = EventBatchAssembler::new();
        assembler.push("alpha", make_down(1000));
        let _b = assembler.assemble_frame(1);

        // Next frame: no new events pushed
        let batches = assembler.assemble_frame(2);
        assert!(
            batches.is_empty(),
            "assembler must be cleared after assemble_frame"
        );
    }

    // ─── Empty frame produces no batches ────────────────────────────────────

    #[test]
    fn test_no_events_no_batches() {
        let mut assembler = EventBatchAssembler::new();
        let batches = assembler.assemble_frame(99);
        assert!(batches.is_empty());
    }

    // ─── PointerMove coalescing: 10 moves → 1 in batch ──────────────────────

    #[test]
    fn test_pointer_move_coalesced_in_batch() {
        let mut assembler = EventBatchAssembler::new();
        // Ten move events for the same node
        for i in 0..10u64 {
            assembler.push("alpha", make_move(i * 100, i as f32, i as f32));
        }

        let batches = assembler.assemble_frame(5);
        let batch = &batches[0];
        let moves: Vec<_> = batch
            .events
            .iter()
            .filter(|e| matches!(e, InputEnvelope::PointerMove(_)))
            .collect();
        assert_eq!(
            moves.len(),
            1,
            "10 PointerMoves for same node should coalesce to 1"
        );
        // The surviving move should be the last one (x=9, y=9)
        if let InputEnvelope::PointerMove(d) = &moves[0] {
            assert!(
                (d.local_x - 9.0).abs() < 0.001,
                "latest position should be retained"
            );
        }
    }

    // ─── Latency: assemble_frame < 2ms (p99) budget assertion ───────────────

    #[test]
    fn test_assemble_frame_under_2ms() {
        use std::time::Instant;
        use tze_hud_scene::perf_budget::{budgets, test_budget};

        let mut assembler = EventBatchAssembler::new();
        // Simulate a dense frame: 50 agents × 8 events each
        for agent_idx in 0..50 {
            let ns = format!("agent_{agent_idx}");
            for event_idx in 0..8u64 {
                let ts = agent_idx as u64 * 1000 + event_idx * 100;
                assembler.push(&ns, make_down(ts));
            }
        }

        let start = Instant::now();
        let batches = assembler.assemble_frame(1);
        let elapsed_us = start.elapsed().as_micros() as u64;

        assert_eq!(batches.len(), 50);

        // 2ms = 2000µs budget, widened by the test slack factor
        let dispatch_budget = test_budget(budgets::EVENT_DISPATCH_BUDGET_US);
        assert!(
            elapsed_us < dispatch_budget,
            "assemble_frame took {elapsed_us}µs, calibrated budget is {dispatch_budget}µs",
        );
    }
}
