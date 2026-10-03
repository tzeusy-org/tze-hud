//! Telemetry data types.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// Per-frame telemetry record.
///
/// All stage timings are in microseconds (us). Stage names map to the
/// 8-stage frame pipeline defined in RFC 0002 §3.2:
///
/// | Stage | Thread     | Budget (p99) |
/// |-------|-----------|-------------|
/// | 1     | Main       | < 500us      |
/// | 2     | Main       | < 500us      |
/// | 3     | Compositor | < 1ms        |
/// | 4     | Compositor | < 1ms        |
/// | 5     | Compositor | < 1ms        |
/// | 6     | Compositor | < 4ms        |
/// | 7     | Compositor+Main | < 8ms   |
/// | 8     | Telemetry  | < 200us      |
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameTelemetry {
    /// Frame number (monotonically increasing).
    pub frame_number: u64,
    /// Timestamp of frame start (microseconds since the Unix epoch).
    ///
    /// Populated by the `FrameRecorder` using wall-clock time (`Clock::now_us()`).
    /// Not to be confused with a process-local monotonic offset.
    pub timestamp_us: u64,
    /// Total frame time in microseconds (Stage 1 start → Stage 7 end).
    pub frame_time_us: u64,

    /// Authoritative degradation trigger workload: active runtime/compositor
    /// work before Stage 3 through successful Stage 7 completion.
    #[serde(default)]
    pub degradation_work_time_us: u64,

    /// Runtime degradation policy actually applied while rendering this frame.
    #[serde(default)]
    pub degradation_level: u8,

    // ── Per-stage timings ────────────────────────────────────────────────────
    /// Stage 1 — Input Drain (main thread). p99 budget: 500us.
    /// Drain OS input events, attach hardware timestamps, enqueue InputEvent records.
    pub stage1_input_drain_us: u64,

    /// Stage 2 — Local Feedback (main thread). p99 budget: 500us.
    /// Hit-test against tile bounds snapshot (ArcSwap), update pressed/hovered flags.
    pub stage2_local_feedback_us: u64,

    /// Stage 3 — Mutation Intake (compositor thread). p99 budget: 1ms.
    /// Drain MutationBatch channel, apply agent envelope limits. Each batch is atomic.
    pub stage3_mutation_intake_us: u64,

    /// Stage 4 — Scene Commit (compositor thread). p99 budget: 1ms.
    /// Apply validated batches with all-or-nothing semantics; publish hit-test snapshot.
    pub stage4_scene_commit_us: u64,

    /// Stage 5 — Layout Resolve (compositor thread). p99 budget: 1ms.
    /// Incremental layout: recompute only changed tiles, z-order, compositing regions.
    pub stage5_layout_resolve_us: u64,

    /// Stage 6 — Render Encode (compositor thread). p99 budget: 4ms.
    /// Build wgpu CommandEncoder; issue draw calls. MUST NOT submit to GPU queue.
    pub stage6_render_encode_us: u64,

    /// Markdown cache prime cost (microseconds) for this frame.
    ///
    /// Records the wall-clock time spent in `prime_markdown_cache` at **commit
    /// time** (Stage 4, before the render stages execute).  Parsing is moved
    /// fully off the render thread by hud-380dl (Option A — commit-time prime):
    /// the runtime calls `prime_markdown_cache` at the end of Stage 4 and passes
    /// the measured cost here; `render_frame` / `render_frame_headless` are
    /// parse-free in steady state.
    ///
    /// Invariants:
    ///
    /// - 0 on frames where `scene.version` has not changed (no parse work done).
    /// - Non-zero on the first frame after new/changed `TextMarkdownNode` content
    ///   is committed; the value reflects the one-time parse cost.
    /// - The render-frame contribution is always 0 (render path is a no-op when
    ///   the cache was primed at Stage 4).
    ///
    /// LLM consumers and the benchmark harness can assert:
    ///
    /// 1. Unchanged-content frames carry `markdown_prime_us == 0`.
    /// 2. After a content commit, `markdown_prime_us > 0` on exactly the next
    ///    frame, then 0 on all subsequent unchanged frames.
    /// 3. The render path does not contribute to `markdown_prime_us`.
    #[serde(default)]
    pub markdown_prime_us: u64,

    /// Stage 7 — GPU Submit + Present (compositor+main thread). p99 budget: 8ms.
    /// Submit CommandBuffer; signal main thread; main thread calls surface.present().
    pub stage7_gpu_submit_us: u64,

    /// Stage 8 — Telemetry Emit (telemetry thread). p99 budget: 200us.
    /// Non-blocking channel send of TelemetryRecord to telemetry thread.
    pub stage8_telemetry_emit_us: u64,

    // ── Split input latency measurements ────────────────────────────────────
    //
    // These three fields carry the split latency measurements required by
    // validation-framework/spec.md §"Split Latency Budgets". Each records
    // the elapsed time from the triggering input event to a specific pipeline
    // boundary for the *current frame*. A value of 0 means no input event
    // occurred this frame for that measurement point.
    /// input_to_local_ack — time from input event arrival to Stage 2 completion
    /// (local visual feedback rendered). p99 budget: 4ms (4_000 µs).
    /// Populated by the input processor; 0 when no input event occurred this frame.
    pub input_to_local_ack_us: u64,

    /// input_to_scene_commit — time from input event arrival to Stage 4
    /// completion (agent mutation reflected in scene graph). p99 budget: 50ms.
    /// Populated when an agent commits a mutation in response to this frame's
    /// input; 0 when no agent response was committed this frame.
    pub input_to_scene_commit_us: u64,

    /// input_to_next_present — time from input event arrival to Stage 7
    /// completion (GPU present of the frame containing the agent response).
    /// p99 budget: 33ms (two frames at 60Hz). Populated when Stage 7 completes
    /// on a frame that carries a scene commit triggered by input; 0 otherwise.
    pub input_to_next_present_us: u64,

    // ── Scene counters ───────────────────────────────────────────────────────
    /// Number of visible tiles this frame.
    pub tile_count: u32,
    /// Number of nodes rendered this frame.
    pub node_count: u32,
    /// Number of active leases.
    pub active_leases: u32,
    /// Number of mutations applied this frame.
    pub mutations_applied: u32,
    /// Number of hit-region states updated this frame.
    pub hit_region_updates: u32,
    /// Number of tiles that had layout recomputed (incremental layout).
    pub tiles_layout_recomputed: u32,
    /// Number of telemetry overflow drops since process start (non-blocking telemetry channel).
    pub telemetry_overflow_count: u64,

    // ── Per-frame correctness fields ─────────────────────────────────────────
    //
    // RFC 0002 §3.2 Stage 8 requires per-frame invariant violation counts so
    // that LLM-driven debugging can detect scene corruption at the frame level,
    // not just at session boundary via SessionSummary counters.
    /// Number of scene-commit rejections this frame (Stage 4 batches where
    /// `applied == false`). Each rejected batch represents a scene mutation
    /// that failed validation — lease checks, budget checks, bounds checks,
    /// or post-mutation invariant checks (Stage 5 of the mutation pipeline).
    ///
    /// A non-zero value on any frame means at least one agent submitted an
    /// invalid mutation batch. The session-level aggregate is tracked in
    /// `SessionSummary::invariant_violations`.
    #[serde(default)]
    pub invariant_violations_this_frame: u32,

    /// Number of Layer 0 structural invariant check failures this frame.
    ///
    /// Layer 0 checks (tile-tab refs, tile-lease refs, bounds positivity,
    /// z-order uniqueness, etc.) are run by `assert_layer0_invariants` from
    /// `tze_hud_scene::test_scenes`. In production the compositor does not run
    /// the full Layer 0 suite every frame (it would be too expensive); this
    /// field is populated by test harnesses that inject a Layer 0 check pass
    /// into the telemetry pipeline.
    ///
    /// A non-zero value indicates a structural invariant failure that survived
    /// Stage 5 validation — this is a stronger signal than
    /// `invariant_violations_this_frame` and warrants immediate investigation.
    ///
    /// In production frames this field is 0 unless a Layer 0 check was
    /// explicitly requested (e.g., via a debug mode flag or test fixture).
    #[serde(default)]
    pub layer0_checks_failed_this_frame: u32,

    /// Cumulative frame-loop scene `try_lock` misses since compositor thread start.
    ///
    /// Incremented each time `compositor_scene.try_lock()` fails in the
    /// compositor frame loop (Stage 4).  A miss means the scene lock was held by
    /// a concurrent gRPC/MCP handler when the compositor attempted to acquire it;
    /// the frame is skipped without GPU work or telemetry emission.
    ///
    /// The value is a **running total** (not a per-frame delta): it equals the
    /// number of misses observed from compositor-thread start through the frame
    /// that emitted this record.  Consumers wanting per-interval rates should
    /// diff successive records.
    ///
    /// Zero-initialized; `#[serde(default)]` ensures old records deserialize
    /// without error.  The counter is a plain thread-local `u64` owned by the
    /// compositor thread — no atomics or cross-thread synchronization cost.
    #[serde(default)]
    pub scene_lock_miss_count: u64,

    /// Widget instances whose SVG was re-rasterized while preparing this frame.
    ///
    /// Empty on frames where no widget parameter changed: widget work is
    /// proportional to change, and an update re-rasterizes only the instance
    /// it targets. (The frame is still re-presented in full; there is no
    /// damage tracking.)
    #[serde(default)]
    pub widget_rasterized: Vec<String>,
}

impl FrameTelemetry {
    pub fn new(frame_number: u64) -> Self {
        Self {
            frame_number,
            timestamp_us: 0,
            frame_time_us: 0,
            degradation_work_time_us: 0,
            degradation_level: 0,
            stage1_input_drain_us: 0,
            stage2_local_feedback_us: 0,
            stage3_mutation_intake_us: 0,
            stage4_scene_commit_us: 0,
            stage5_layout_resolve_us: 0,
            stage6_render_encode_us: 0,
            stage7_gpu_submit_us: 0,
            stage8_telemetry_emit_us: 0,
            markdown_prime_us: 0,
            // Split input latency measurements
            input_to_local_ack_us: 0,
            input_to_scene_commit_us: 0,
            input_to_next_present_us: 0,
            tile_count: 0,
            node_count: 0,
            active_leases: 0,
            mutations_applied: 0,
            hit_region_updates: 0,
            tiles_layout_recomputed: 0,
            telemetry_overflow_count: 0,
            invariant_violations_this_frame: 0,
            layer0_checks_failed_this_frame: 0,
            scene_lock_miss_count: 0,
            widget_rasterized: Vec::new(),
        }
    }
}

/// Maximum number of samples retained by a [`LatencyBucket`].
///
/// `LatencyBucket` is a diagnostic/observability helper used in debug tracing
/// and session summaries. Without a cap the `samples` deque grows without bound
/// for the driver lifetime, making `percentile()` — which clones and sorts the
/// full window — progressively more expensive in long diagnostic sessions.
///
/// 1 024 samples was chosen because:
/// - At 60 fps a full ring takes ~17 s, covering several seconds of recent history.
/// - It provides excellent percentile resolution (p99 needs ≥ 100 samples; 1 024
///   gives stable p95/p99 estimates with room to spare).
/// - Memory footprint is bounded at 8 KiB per bucket (1 024 × 8 bytes).
/// - `percentile()` cost is O(N log N) on at most 1 024 elements, i.e. constant.
pub const LATENCY_BUCKET_WINDOW: usize = 1_024;

/// Latency measurement bucket with a bounded sliding window.
///
/// Samples are stored in a `VecDeque` capped at [`LATENCY_BUCKET_WINDOW`]
/// entries. When the window is full, `record()` evicts the oldest sample before
/// inserting the new one, so memory and `percentile()` cost stay constant
/// regardless of session length.
///
/// Percentiles reflect the **most recent** window of samples, which is the
/// correct semantic for long diagnostic sessions: stale measurements from the
/// start of a session should not drag down (or inflate) current percentiles.
///
/// Serialises as a JSON object `{"name": "…", "samples": […]}` — the `samples`
/// array is bounded to at most `LATENCY_BUCKET_WINDOW` elements.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LatencyBucket {
    pub name: String,
    /// Recent samples in insertion order (oldest → newest).
    ///
    /// The deque is capped at [`LATENCY_BUCKET_WINDOW`] entries. Direct
    /// read-only access is intentionally public for tests that need to inspect
    /// raw sample values; callers must not push to it directly — use
    /// [`LatencyBucket::record`] instead.
    pub samples: VecDeque<u64>, // microseconds
}

impl Default for LatencyBucket {
    fn default() -> Self {
        Self::new("")
    }
}

impl LatencyBucket {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            samples: VecDeque::with_capacity(LATENCY_BUCKET_WINDOW),
        }
    }

    /// Record a latency sample (in microseconds).
    ///
    /// If the window is full the oldest sample is evicted to make room.
    pub fn record(&mut self, us: u64) {
        if self.samples.len() == LATENCY_BUCKET_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(us);
    }

    /// Number of samples currently held (≤ `LATENCY_BUCKET_WINDOW`).
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Returns `true` if no samples have been recorded.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn percentile(&self, pct: f64) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        // Collect the bounded window into a Vec for sorting.  The cost is
        // O(N log N) on at most LATENCY_BUCKET_WINDOW elements — constant.
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        // Nearest-rank method: ceil(pct/100 * N) - 1, clamped to valid range
        let rank = ((pct / 100.0) * sorted.len() as f64).ceil() as usize;
        let idx = rank.saturating_sub(1).min(sorted.len() - 1);
        Some(sorted[idx])
    }

    pub fn p50(&self) -> Option<u64> {
        self.percentile(50.0)
    }

    pub fn p95(&self) -> Option<u64> {
        self.percentile(95.0)
    }

    pub fn p99(&self) -> Option<u64> {
        self.percentile(99.0)
    }

    /// Assert that the p99 value is under the given budget (in microseconds).
    ///
    /// Returns `Ok(p99_value)` on pass, `Err(message)` on failure or if there
    /// are no samples.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tze_hud_telemetry::LatencyBucket;
    /// let mut bucket = LatencyBucket::new("frame_time");
    /// for _ in 0..100 { bucket.record(5_000); }
    /// assert!(bucket.assert_p99_under(16_600).is_ok());
    /// ```
    pub fn assert_p99_under(&self, budget_us: u64) -> Result<u64, String> {
        match self.p99() {
            None => Err(format!(
                "budget assertion failed for '{}': no samples recorded",
                self.name
            )),
            Some(p99) if p99 > budget_us => Err(format!(
                "budget assertion failed for '{}': p99={p99}us exceeds budget={budget_us}us \
                 (over by {}us, {:.1}%)",
                self.name,
                p99 - budget_us,
                (p99 as f64 / budget_us as f64 - 1.0) * 100.0,
            )),
            Some(p99) => Ok(p99),
        }
    }
}

/// Telemetry event emitted when the degradation level changes.
///
/// Emitted on every level transition (both advance and recovery).
/// Consumers use this to track degradation history and tune thresholds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DegradationEvent {
    /// Frame number when the transition occurred.
    pub frame_number: u64,
    /// Previous degradation level (0 = Normal, 5 = Emergency).
    pub previous_level: u8,
    /// New degradation level after transition.
    pub new_level: u8,
    /// The rolling-window p95 frame time (µs) that triggered this transition.
    pub frame_time_p95_us: u64,
    /// Direction of the transition.
    pub direction: DegradationDirection,
    #[serde(default)]
    pub sample_count: u32,
    #[serde(default)]
    pub window_duration_us: u64,
    #[serde(default)]
    pub effective_cadence_hz: u32,
    #[serde(default)]
    pub entry_threshold_us: u64,
    #[serde(default)]
    pub recovery_threshold_us: u64,
    #[serde(default)]
    pub recovery_source: DegradationRecoverySource,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DegradationRecoverySource {
    #[default]
    ActiveFrames,
    Quiescent,
}

/// Direction of a degradation level transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DegradationDirection {
    /// Level worsened (frame_time_p95 > 14ms trigger threshold).
    Advance,
    /// Level improved (frame_time_p95 < 12ms sustained over 30 frames).
    Recover,
}

/// Per-session aggregated telemetry summary.
///
/// Covers all Layer-3 performance requirements:
/// - Per-session totals: total_frames, fps, elapsed_us
/// - Frame time percentiles (p50/p95/p99) via `frame_time`
/// - Full latency breakdown: input_to_local_ack, input_to_scene_commit, input_to_next_present
/// - Peak tracking: peak_frame_time_us, peak_tile_count
/// - Violation counters: lease_violations, budget_overruns, sync_drift_violations,
///   invariant_violations (session aggregate of per-frame `invariant_violations_this_frame`)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionSummary {
    /// Total frames rendered in this session.
    pub total_frames: u64,
    /// Total session duration in microseconds (set externally when session ends).
    #[serde(default)]
    pub elapsed_us: u64,
    /// Average FPS over the session (computed from total_frames / elapsed_us).
    /// Zero if elapsed_us == 0.
    #[serde(default)]
    pub fps: f64,
    /// Per-frame total time (Stage 1 start → Stage 7 end), microseconds.
    pub frame_time: LatencyBucket,
    /// input_to_local_ack — time from input event to Stage 2 completion.
    /// Spec: p99 < 4ms (4_000 µs). Purely local, no network.
    #[serde(default)]
    pub input_to_local_ack: LatencyBucket,
    /// input_to_scene_commit — time from input event to Stage 4 completion.
    /// Spec: p99 < 50ms (50_000 µs). Covers agent response round-trip.
    #[serde(default)]
    pub input_to_scene_commit: LatencyBucket,
    /// input_to_next_present — time from input event to Stage 7 completion
    /// (GPU present of frame containing agent response).
    /// Spec: p99 < 33ms (33_000 µs) at 60Hz (two frames).
    #[serde(default)]
    pub input_to_next_present: LatencyBucket,
    /// Hit-test latency.
    pub hit_test_latency: LatencyBucket,
    /// Mutation batch validation latency.
    pub validation_latency: LatencyBucket,
    /// Scene diff computation latency.
    pub diff_latency: LatencyBucket,
    /// Lease acquire latency.
    pub lease_acquire_latency: LatencyBucket,
    /// Agent connect latency.
    pub agent_connect_latency: LatencyBucket,
    /// Peak single-frame time observed (microseconds).
    #[serde(default)]
    pub peak_frame_time_us: u64,
    /// Peak tile count seen in any single frame.
    #[serde(default)]
    pub peak_tile_count: u32,
    /// Number of lease violations observed (zero is the pass threshold).
    #[serde(default)]
    pub lease_violations: u64,
    /// Number of budget overruns observed (zero is the pass threshold).
    #[serde(default)]
    pub budget_overruns: u64,
    /// Number of sync drift violations (drift > 500µs).
    #[serde(default)]
    pub sync_drift_violations: u64,
    /// Session aggregate of per-frame `invariant_violations_this_frame`.
    ///
    /// Counts the total number of scene-commit rejections (batches where
    /// `applied == false`) across all frames in this session. Accumulated by
    /// `record_frame_correctness`. Zero is the expected value for a healthy
    /// session; non-zero indicates agents submitted invalid mutation batches.
    #[serde(default)]
    pub invariant_violations: u64,

    /// Cumulative frame-loop scene `try_lock` misses for this session.
    ///
    /// Tracks how many times the compositor frame loop attempted
    /// `compositor_scene.try_lock()` and failed (contention — the scene lock
    /// was held by a concurrent gRPC or MCP handler).  Accumulated by
    /// `record_frame_correctness` from `FrameTelemetry::scene_lock_miss_count`.
    ///
    /// Because `scene_lock_miss_count` is a running total, this field holds the
    /// peak value seen across all recorded frames (i.e., the miss count at the
    /// last emitted frame), not a sum of per-frame deltas.
    ///
    /// Zero is the expected value under no contention; persistent non-zero
    /// values indicate lock contention between the compositor and scene
    /// mutation handlers, which is the primary signal for the deferred
    /// double-buffer evaluation described in hud-3qpgv.2's close reason.
    #[serde(default)]
    pub scene_lock_misses: u64,
}

impl SessionSummary {
    pub fn new() -> Self {
        Self {
            total_frames: 0,
            elapsed_us: 0,
            fps: 0.0,
            frame_time: LatencyBucket::new("frame_time"),
            input_to_local_ack: LatencyBucket::new("input_to_local_ack"),
            input_to_scene_commit: LatencyBucket::new("input_to_scene_commit"),
            input_to_next_present: LatencyBucket::new("input_to_next_present"),
            hit_test_latency: LatencyBucket::new("hit_test"),
            validation_latency: LatencyBucket::new("validation"),
            diff_latency: LatencyBucket::new("diff"),
            lease_acquire_latency: LatencyBucket::new("lease_acquire"),
            agent_connect_latency: LatencyBucket::new("agent_connect"),
            peak_frame_time_us: 0,
            peak_tile_count: 0,
            lease_violations: 0,
            budget_overruns: 0,
            sync_drift_violations: 0,
            invariant_violations: 0,
            scene_lock_misses: 0,
        }
    }

    /// Record a frame's telemetry into this summary.
    ///
    /// Updates total_frames, frame_time bucket, and peak_frame_time_us.
    pub fn record_frame(&mut self, frame_time_us: u64, tile_count: u32) {
        self.total_frames += 1;
        self.frame_time.record(frame_time_us);
        if frame_time_us > self.peak_frame_time_us {
            self.peak_frame_time_us = frame_time_us;
        }
        if tile_count > self.peak_tile_count {
            self.peak_tile_count = tile_count;
        }
    }

    /// Accumulate per-frame correctness counters into session totals.
    ///
    /// Call this after each frame (alongside or after `record_frame`) to
    /// keep `invariant_violations` in sync with per-frame telemetry.
    ///
    /// # Arguments
    ///
    /// * `frame` — the `FrameTelemetry` record for the frame just completed.
    ///
    /// # Example
    ///
    /// ```
    /// # use tze_hud_telemetry::{SessionSummary, FrameTelemetry};
    /// let mut summary = SessionSummary::new();
    /// let mut frame = FrameTelemetry::new(1);
    /// frame.invariant_violations_this_frame = 2;
    /// summary.record_frame(frame.frame_time_us, frame.tile_count);
    /// summary.record_frame_correctness(&frame);
    /// assert_eq!(summary.invariant_violations, 2);
    /// ```
    pub fn record_frame_correctness(&mut self, frame: &FrameTelemetry) {
        self.invariant_violations += frame.invariant_violations_this_frame as u64;
        // scene_lock_miss_count is a running total; take the max so the session
        // summary holds the latest (highest) value observed across all frames.
        if frame.scene_lock_miss_count > self.scene_lock_misses {
            self.scene_lock_misses = frame.scene_lock_miss_count;
        }
    }

    /// Finalize: compute FPS from total_frames and elapsed_us.
    ///
    /// Call this once the session ends and `elapsed_us` has been set.
    pub fn finalize(&mut self) {
        if self.elapsed_us > 0 {
            self.fps = self.total_frames as f64 / (self.elapsed_us as f64 / 1_000_000.0);
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

impl Default for SessionSummary {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_latency_bucket_percentiles() {
        let mut bucket = LatencyBucket::new("test");
        for i in 1..=100 {
            bucket.record(i);
        }
        assert_eq!(bucket.p50(), Some(50));
        assert_eq!(bucket.p95(), Some(95));
        assert_eq!(bucket.p99(), Some(99));
    }

    #[test]
    fn test_session_summary_serialization() {
        let mut summary = SessionSummary::new();
        summary.total_frames = 100;
        summary.frame_time.record(12000);
        summary.frame_time.record(14000);

        let json = summary.to_json().unwrap();
        assert!(json.contains("frame_time"));
        assert!(json.contains("12000"));
    }

    /// Verify that all three split latency buckets exist in SessionSummary and
    /// serialize to their canonical names.
    #[test]
    fn test_session_summary_has_three_split_latency_buckets() {
        let mut summary = SessionSummary::new();

        // Populate each bucket independently
        summary.input_to_local_ack.record(1_000); // 1ms
        summary.input_to_scene_commit.record(10_000); // 10ms
        summary.input_to_next_present.record(20_000); // 20ms

        // Budget assertions must pass for all three
        assert!(
            summary.input_to_local_ack.assert_p99_under(4_000).is_ok(),
            "input_to_local_ack p99 must be under 4ms budget"
        );
        assert!(
            summary
                .input_to_scene_commit
                .assert_p99_under(50_000)
                .is_ok(),
            "input_to_scene_commit p99 must be under 50ms budget"
        );
        assert!(
            summary
                .input_to_next_present
                .assert_p99_under(33_000)
                .is_ok(),
            "input_to_next_present p99 must be under 33ms budget"
        );

        // Serialized JSON must contain all three bucket names
        let json = summary.to_json().unwrap();
        assert!(
            json.contains("input_to_local_ack"),
            "JSON must contain input_to_local_ack"
        );
        assert!(
            json.contains("input_to_scene_commit"),
            "JSON must contain input_to_scene_commit"
        );
        assert!(
            json.contains("input_to_next_present"),
            "JSON must contain input_to_next_present"
        );
    }

    /// Verify that FrameTelemetry carries all three split latency fields.
    #[test]
    fn test_frame_telemetry_has_split_latency_fields() {
        let mut frame = FrameTelemetry::new(1);
        frame.input_to_local_ack_us = 500; // 0.5ms
        frame.input_to_scene_commit_us = 5_000; // 5ms
        frame.input_to_next_present_us = 15_000; // 15ms

        // Fields round-trip through the struct
        assert_eq!(frame.input_to_local_ack_us, 500);
        assert_eq!(frame.input_to_scene_commit_us, 5_000);
        assert_eq!(frame.input_to_next_present_us, 15_000);

        // Serialized JSON must contain all three field names
        let json = serde_json::to_string(&frame).unwrap();
        assert!(
            json.contains("input_to_local_ack_us"),
            "JSON must contain input_to_local_ack_us"
        );
        assert!(
            json.contains("input_to_scene_commit_us"),
            "JSON must contain input_to_scene_commit_us"
        );
        assert!(
            json.contains("input_to_next_present_us"),
            "JSON must contain input_to_next_present_us"
        );
    }

    #[test]
    fn test_session_summary_record_frame_updates_peaks() {
        let mut summary = SessionSummary::new();
        summary.record_frame(10_000, 5);
        summary.record_frame(20_000, 3);
        summary.record_frame(15_000, 8);

        assert_eq!(summary.total_frames, 3);
        assert_eq!(summary.peak_frame_time_us, 20_000);
        assert_eq!(summary.peak_tile_count, 8);
    }

    #[test]
    fn test_session_summary_finalize_computes_fps() {
        let mut summary = SessionSummary::new();
        summary.total_frames = 60;
        summary.elapsed_us = 1_000_000; // 1 second
        summary.finalize();
        assert!((summary.fps - 60.0).abs() < 0.001);
    }

    #[test]
    fn test_session_summary_finalize_zero_elapsed() {
        let mut summary = SessionSummary::new();
        summary.total_frames = 10;
        summary.elapsed_us = 0;
        summary.finalize();
        assert_eq!(summary.fps, 0.0);
    }

    #[test]
    fn test_session_summary_has_input_to_next_present() {
        let mut summary = SessionSummary::new();
        summary.input_to_next_present.record(25_000);
        assert_eq!(summary.input_to_next_present.p99(), Some(25_000));
        let json = summary.to_json().unwrap();
        assert!(json.contains("input_to_next_present"));
    }

    #[test]
    fn test_assert_p99_under_passes_when_within_budget() {
        let mut bucket = LatencyBucket::new("test");
        for _ in 0..100 {
            bucket.record(5_000);
        }
        assert!(bucket.assert_p99_under(16_600).is_ok());
    }

    #[test]
    fn test_assert_p99_under_fails_when_exceeds_budget() {
        let mut bucket = LatencyBucket::new("test");
        for _ in 0..100 {
            bucket.record(20_000); // 20ms — over budget
        }
        let result = bucket.assert_p99_under(16_600);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("20000us"),
            "error should contain actual: {msg}"
        );
        assert!(
            msg.contains("16600us"),
            "error should contain budget: {msg}"
        );
    }

    #[test]
    fn test_assert_p99_under_fails_with_no_samples() {
        let bucket = LatencyBucket::new("empty");
        let result = bucket.assert_p99_under(16_600);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no samples"));
    }

    // ── Per-frame correctness fields (RFC 0002 §3.2 Stage 8) ─────────────────

    /// Verify FrameTelemetry has per-frame invariant violation count field,
    /// initialized to zero by FrameTelemetry::new().
    #[test]
    fn test_frame_telemetry_has_invariant_violations_this_frame_field() {
        let frame = FrameTelemetry::new(1);
        assert_eq!(
            frame.invariant_violations_this_frame, 0,
            "invariant_violations_this_frame must be zero-initialized"
        );
    }

    /// Verify FrameTelemetry has per-frame Layer 0 check failure count field,
    /// initialized to zero by FrameTelemetry::new().
    #[test]
    fn test_frame_telemetry_has_layer0_checks_failed_this_frame_field() {
        let frame = FrameTelemetry::new(1);
        assert_eq!(
            frame.layer0_checks_failed_this_frame, 0,
            "layer0_checks_failed_this_frame must be zero-initialized"
        );
    }

    /// Verify per-frame correctness fields serialize to JSON with canonical names.
    #[test]
    fn test_frame_telemetry_correctness_fields_serialize_to_json() {
        let mut frame = FrameTelemetry::new(1);
        frame.invariant_violations_this_frame = 3;
        frame.layer0_checks_failed_this_frame = 1;

        let json = serde_json::to_string(&frame).unwrap();
        assert!(
            json.contains("invariant_violations_this_frame"),
            "JSON must contain invariant_violations_this_frame"
        );
        assert!(
            json.contains("layer0_checks_failed_this_frame"),
            "JSON must contain layer0_checks_failed_this_frame"
        );
        assert!(
            json.contains("\"invariant_violations_this_frame\":3"),
            "value must be 3"
        );
        assert!(
            json.contains("\"layer0_checks_failed_this_frame\":1"),
            "value must be 1"
        );
    }

    /// Verify record_frame_correctness accumulates invariant_violations into
    /// SessionSummary.invariant_violations.
    #[test]
    fn test_session_summary_record_frame_correctness_accumulates_violations() {
        let mut summary = SessionSummary::new();

        let mut frame1 = FrameTelemetry::new(1);
        frame1.invariant_violations_this_frame = 2;
        summary.record_frame(frame1.frame_time_us, frame1.tile_count);
        summary.record_frame_correctness(&frame1);

        let mut frame2 = FrameTelemetry::new(2);
        frame2.invariant_violations_this_frame = 0; // clean frame
        summary.record_frame(frame2.frame_time_us, frame2.tile_count);
        summary.record_frame_correctness(&frame2);

        let mut frame3 = FrameTelemetry::new(3);
        frame3.invariant_violations_this_frame = 1;
        summary.record_frame(frame3.frame_time_us, frame3.tile_count);
        summary.record_frame_correctness(&frame3);

        assert_eq!(
            summary.invariant_violations, 3,
            "session total should be sum of per-frame counts: 2+0+1=3"
        );
        assert_eq!(summary.total_frames, 3);
    }

    /// Verify SessionSummary.invariant_violations is zero-initialized
    /// and serializes with serde(default).
    #[test]
    fn test_session_summary_invariant_violations_zero_initialized() {
        let summary = SessionSummary::new();
        assert_eq!(summary.invariant_violations, 0);

        // Verify it appears in JSON
        let json = summary.to_json().unwrap();
        assert!(
            json.contains("invariant_violations"),
            "JSON must contain invariant_violations field"
        );
    }

    /// Verify that a frame with no violations produces zero counts.
    #[test]
    fn test_frame_telemetry_clean_frame_has_zero_violations() {
        let frame = FrameTelemetry::new(42);
        assert_eq!(frame.invariant_violations_this_frame, 0);
        assert_eq!(frame.layer0_checks_failed_this_frame, 0);
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"invariant_violations_this_frame\":0"));
        assert!(json.contains("\"layer0_checks_failed_this_frame\":0"));
    }

    #[test]
    fn degradation_workload_and_applied_level_are_machine_readable() {
        let mut frame = FrameTelemetry::new(42);
        assert_eq!(frame.degradation_work_time_us, 0);
        assert_eq!(frame.degradation_level, 0);
        frame.degradation_work_time_us = 8_750;
        frame.degradation_level = 3;

        let encoded = serde_json::to_string(&frame).unwrap();
        let decoded: FrameTelemetry = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.degradation_work_time_us, 8_750);
        assert_eq!(decoded.degradation_level, 3);
    }

    #[test]
    fn degradation_fields_default_when_reading_legacy_frame_json() {
        let mut legacy = serde_json::to_value(FrameTelemetry::new(42)).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("degradation_work_time_us");
        object.remove("degradation_level");

        let decoded: FrameTelemetry = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.degradation_work_time_us, 0);
        assert_eq!(decoded.degradation_level, 0);
    }

    // ── hud-gpqde: markdown_prime_us telemetry field ─────────────────────────

    /// FrameTelemetry.markdown_prime_us is zero-initialized by FrameTelemetry::new.
    ///
    /// This field records the wall-clock cost of `prime_markdown_cache` for
    /// each frame, making markdown parse cost visible in stage telemetry.
    #[test]
    fn test_frame_telemetry_markdown_prime_us_zero_initialized() {
        let frame = FrameTelemetry::new(1);
        assert_eq!(
            frame.markdown_prime_us, 0,
            "markdown_prime_us must be zero-initialized"
        );
    }

    /// markdown_prime_us serializes to its canonical JSON field name and
    /// round-trips through serde correctly.
    #[test]
    fn test_frame_telemetry_markdown_prime_us_serializes() {
        let mut frame = FrameTelemetry::new(1);
        frame.markdown_prime_us = 350; // 350 µs — a realistic commit-frame value

        let json = serde_json::to_string(&frame).unwrap();
        assert!(
            json.contains("markdown_prime_us"),
            "serialized JSON must contain markdown_prime_us field: {json}"
        );
        assert!(
            json.contains("\"markdown_prime_us\":350"),
            "serialized value must match: {json}"
        );

        // Round-trip through serde_json.
        let decoded: FrameTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.markdown_prime_us, 350,
            "markdown_prime_us must survive JSON round-trip"
        );
    }

    /// markdown_prime_us defaults to 0 when absent from JSON (serde(default)).
    ///
    /// This ensures backward-compat: telemetry records written before this
    /// field was added still deserialize without error.
    #[test]
    fn test_frame_telemetry_markdown_prime_us_defaults_on_missing_json() {
        // JSON without the markdown_prime_us field — simulates an older record.
        let json = r#"{"frame_number":1,"timestamp_us":0,"frame_time_us":0,
            "stage1_input_drain_us":0,"stage2_local_feedback_us":0,
            "stage3_mutation_intake_us":0,"stage4_scene_commit_us":0,
            "stage5_layout_resolve_us":0,"stage6_render_encode_us":0,
            "stage7_gpu_submit_us":0,"stage8_telemetry_emit_us":0,
            "input_to_local_ack_us":0,"input_to_scene_commit_us":0,
            "input_to_next_present_us":0,"tile_count":0,"node_count":0,
            "active_leases":0,"mutations_applied":0,"hit_region_updates":0,
            "tiles_layout_recomputed":0,"telemetry_overflow_count":0,
            "invariant_violations_this_frame":0,"layer0_checks_failed_this_frame":0}"#;

        let decoded: FrameTelemetry = serde_json::from_str(json).unwrap();
        assert_eq!(
            decoded.markdown_prime_us, 0,
            "markdown_prime_us must default to 0 when absent from JSON"
        );
    }

    // ── hud-3qpgv.2: scene_lock_miss_count telemetry ─────────────────────────

    /// FrameTelemetry.scene_lock_miss_count is zero-initialized by
    /// FrameTelemetry::new, matching the no-contention default.
    #[test]
    fn scene_lock_miss_count_zero_initialized() {
        let frame = FrameTelemetry::new(1);
        assert_eq!(
            frame.scene_lock_miss_count, 0,
            "scene_lock_miss_count must be zero-initialized"
        );
    }

    /// scene_lock_miss_count serializes to its canonical JSON field name and
    /// round-trips through serde without loss.
    #[test]
    fn scene_lock_miss_count_serializes_and_round_trips() {
        let mut frame = FrameTelemetry::new(7);
        frame.scene_lock_miss_count = 42;

        let json = serde_json::to_string(&frame).unwrap();
        assert!(
            json.contains("scene_lock_miss_count"),
            "JSON must contain scene_lock_miss_count: {json}"
        );
        assert!(
            json.contains("\"scene_lock_miss_count\":42"),
            "serialized value must be 42: {json}"
        );

        let decoded: FrameTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.scene_lock_miss_count, 42,
            "scene_lock_miss_count must survive JSON round-trip"
        );
    }

    /// scene_lock_miss_count defaults to 0 when absent from JSON.
    ///
    /// Old telemetry records (written before this field was added) must
    /// deserialize without error; the field defaults to zero via
    /// `#[serde(default)]`.
    #[test]
    fn scene_lock_miss_count_defaults_on_missing_json() {
        // JSON without scene_lock_miss_count — simulates a pre-hud-3qpgv.2 record.
        let json = r#"{"frame_number":1,"timestamp_us":0,"frame_time_us":0,
            "stage1_input_drain_us":0,"stage2_local_feedback_us":0,
            "stage3_mutation_intake_us":0,"stage4_scene_commit_us":0,
            "stage5_layout_resolve_us":0,"stage6_render_encode_us":0,
            "stage7_gpu_submit_us":0,"stage8_telemetry_emit_us":0,
            "input_to_local_ack_us":0,"input_to_scene_commit_us":0,
            "input_to_next_present_us":0,"tile_count":0,"node_count":0,
            "active_leases":0,"mutations_applied":0,"hit_region_updates":0,
            "tiles_layout_recomputed":0,"telemetry_overflow_count":0,
            "invariant_violations_this_frame":0,"layer0_checks_failed_this_frame":0,
            "markdown_prime_us":0}"#;

        let decoded: FrameTelemetry = serde_json::from_str(json).unwrap();
        assert_eq!(
            decoded.scene_lock_miss_count, 0,
            "scene_lock_miss_count must default to 0 when absent"
        );
    }

    /// record_frame_correctness propagates scene_lock_miss_count into
    /// SessionSummary.scene_lock_misses (takes the max, since it is a running
    /// total not a per-frame delta).
    #[test]
    fn session_summary_scene_lock_misses_tracks_running_total() {
        let mut summary = SessionSummary::new();
        assert_eq!(summary.scene_lock_misses, 0, "zero-initialized");

        // Frame 1: 3 misses accumulated so far.
        let mut frame1 = FrameTelemetry::new(1);
        frame1.scene_lock_miss_count = 3;
        summary.record_frame_correctness(&frame1);
        assert_eq!(
            summary.scene_lock_misses, 3,
            "scene_lock_misses should track the running total from frame 1"
        );

        // Frame 2: 5 misses total (2 more happened between frames 1 and 2).
        let mut frame2 = FrameTelemetry::new(2);
        frame2.scene_lock_miss_count = 5;
        summary.record_frame_correctness(&frame2);
        assert_eq!(
            summary.scene_lock_misses, 5,
            "scene_lock_misses should advance to 5 after frame 2"
        );

        // Frame 3: no new misses — same running total as frame 2.
        let mut frame3 = FrameTelemetry::new(3);
        frame3.scene_lock_miss_count = 5;
        summary.record_frame_correctness(&frame3);
        assert_eq!(
            summary.scene_lock_misses, 5,
            "scene_lock_misses must not decrease when the total stays flat"
        );
    }

    /// SessionSummary.scene_lock_misses serializes to JSON.
    #[test]
    fn session_summary_scene_lock_misses_serializes() {
        let mut summary = SessionSummary::new();
        summary.scene_lock_misses = 7;
        let json = summary.to_json().unwrap();
        assert!(
            json.contains("scene_lock_misses"),
            "JSON must contain scene_lock_misses: {json}"
        );
        // to_json uses pretty-print (spaces after colons); check for the value.
        let compact = serde_json::to_string(&summary).unwrap();
        assert!(
            compact.contains("\"scene_lock_misses\":7"),
            "compact serialized value must be 7: {compact}"
        );
    }

    // ── hud-1rxbs: LatencyBucket bounded sliding window ───────────────────────

    /// Samples beyond LATENCY_BUCKET_WINDOW capacity evict the oldest entry.
    ///
    /// After inserting WINDOW + 1 samples the deque must still hold exactly
    /// WINDOW entries, and the evicted sample (value 0) must no longer be
    /// present while the most recently inserted sample is at the back.
    #[test]
    fn latency_bucket_window_is_bounded() {
        let mut bucket = LatencyBucket::new("bounded");

        // Fill to capacity.
        for i in 0..LATENCY_BUCKET_WINDOW {
            bucket.record(i as u64);
        }
        assert_eq!(
            bucket.len(),
            LATENCY_BUCKET_WINDOW,
            "bucket must hold exactly LATENCY_BUCKET_WINDOW samples when full"
        );

        // One more sample must evict the oldest (value 0) and keep the window size.
        let overflow_value: u64 = LATENCY_BUCKET_WINDOW as u64;
        bucket.record(overflow_value);
        assert_eq!(
            bucket.len(),
            LATENCY_BUCKET_WINDOW,
            "len must stay at LATENCY_BUCKET_WINDOW after overflow"
        );
        // Oldest sample (0) must have been evicted.
        assert!(
            !bucket.samples.contains(&0),
            "evicted sample (0) must no longer be in the window"
        );
        // Newest sample must be at the back.
        assert_eq!(
            *bucket.samples.back().unwrap(),
            overflow_value,
            "most recently inserted sample must be at the back of the deque"
        );
    }

    /// percentile() on a full window returns a sensible value bounded by the
    /// window contents, not the full session history.
    ///
    /// Loads WINDOW samples of value 1 000 µs, then overflows with WINDOW
    /// samples of value 2 000 µs. After overflow the window holds only the
    /// 2 000 µs samples, so p50/p99 must both be 2 000 µs, not 1 000 µs.
    #[test]
    fn latency_bucket_percentile_reflects_recent_window() {
        let mut bucket = LatencyBucket::new("recent");

        // Phase 1: fill with old samples.
        for _ in 0..LATENCY_BUCKET_WINDOW {
            bucket.record(1_000);
        }
        assert_eq!(bucket.p50(), Some(1_000));

        // Phase 2: overwrite entire window with new samples.
        for _ in 0..LATENCY_BUCKET_WINDOW {
            bucket.record(2_000);
        }

        assert_eq!(
            bucket.len(),
            LATENCY_BUCKET_WINDOW,
            "len must remain bounded after second fill"
        );
        assert_eq!(
            bucket.p50(),
            Some(2_000),
            "p50 must reflect recent window, not stale samples"
        );
        assert_eq!(
            bucket.p99(),
            Some(2_000),
            "p99 must reflect recent window, not stale samples"
        );
    }

    /// An empty bucket's `len()` and `is_empty()` helpers behave correctly.
    #[test]
    fn latency_bucket_len_is_empty_helpers() {
        let mut bucket = LatencyBucket::new("helpers");
        assert!(bucket.is_empty());
        assert_eq!(bucket.len(), 0);

        bucket.record(500);
        assert!(!bucket.is_empty());
        assert_eq!(bucket.len(), 1);
    }

    /// LatencyBucket serialises and deserialises through JSON without data loss
    /// (for windows smaller than LATENCY_BUCKET_WINDOW).
    #[test]
    fn latency_bucket_json_round_trip() {
        let mut bucket = LatencyBucket::new("round_trip");
        bucket.record(100);
        bucket.record(200);
        bucket.record(300);

        let json = serde_json::to_string(&bucket).unwrap();
        assert!(json.contains("\"samples\":[100,200,300]"), "json: {json}");

        let decoded: LatencyBucket = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.name, "round_trip");
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded.samples[0], 100);
        assert_eq!(decoded.samples[2], 300);
    }
}
