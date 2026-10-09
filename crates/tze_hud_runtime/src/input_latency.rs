//! Private input timing shared by the windowed and headless runtime owners.
//!
//! Local acknowledgement needs only an input. Response measurements additionally
//! require a newly applied, explicitly associated batch and its containing submit.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tze_hud_scene::SceneId;

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingInputLatencySample {
    input_started_at: Instant,
    local_ack_us: u64,
    response: Option<(SceneId, Instant)>,
}

pub(crate) type PendingInputLatencySamples = Arc<Mutex<VecDeque<PendingInputLatencySample>>>;

pub(crate) fn record_pending_input_latency(
    pending: &PendingInputLatencySamples,
    input_started_at: Instant,
    local_ack_us: u64,
) {
    if let Ok(mut samples) = pending.lock() {
        samples.push_back(PendingInputLatencySample {
            input_started_at,
            local_ack_us: local_ack_us.max(1),
            response: None,
        });
    }
}

pub(crate) fn record_committed_input_response(
    pending: &PendingInputLatencySamples,
    input_started_at: Instant,
    local_ack_us: u64,
    batch_id: SceneId,
    scene_commit_at: Instant,
) {
    if scene_commit_at < input_started_at {
        return;
    }
    if let Ok(mut samples) = pending.lock() {
        // A repeated acknowledgement for an outstanding batch cannot re-arm it.
        if samples
            .iter()
            .any(|sample| sample.response.is_some_and(|(id, _)| id == batch_id))
        {
            return;
        }
        samples.push_back(PendingInputLatencySample {
            input_started_at,
            local_ack_us: local_ack_us.max(1),
            response: Some((batch_id, scene_commit_at)),
        });
    }
}

pub(crate) fn drain_pending_input_latency(
    pending: &PendingInputLatencySamples,
    submitted_batch_ids: &[SceneId],
    frame_submitted_at: Option<Instant>,
) -> Option<(u64, u64, u64)> {
    let submitted_at = frame_submitted_at?;
    let mut samples = pending.lock().ok()?;
    let mut drained = false;
    let (mut local_ack_us, mut scene_commit_us, mut next_submit_us) = (0, 0, 0);
    samples.retain(|sample| {
        if let Some((batch_id, committed_at)) = sample.response {
            if !submitted_batch_ids.contains(&batch_id) || submitted_at < committed_at {
                return true;
            }
            scene_commit_us = scene_commit_us.max(
                committed_at
                    .duration_since(sample.input_started_at)
                    .as_micros() as u64,
            );
            next_submit_us = next_submit_us.max(
                submitted_at
                    .duration_since(sample.input_started_at)
                    .as_micros() as u64,
            );
        }
        local_ack_us = local_ack_us.max(sample.local_ack_us);
        drained = true;
        false
    });
    drained.then_some((local_ack_us, scene_commit_us, next_submit_us))
}
