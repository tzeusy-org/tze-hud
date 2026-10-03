//! Timed content: batches held until `present_at`, tiles swept at `expires_at`.
//!
//! Arrival time is not presentation time (invariant 1). A batch whose
//! `present_at` is in the future waits in [`SceneGraph::scheduled_batches`]
//! and is applied by [`SceneGraph::apply_due_batches`] once due. A batch with
//! `expires_at` stamps that deadline on the tiles it creates or targets, and
//! [`SceneGraph::drain_expired_tiles`] removes them once it passes. Both run
//! in the compositor's per-frame commit stage, and
//! [`SceneGraph::next_timed_content_wall_us`] gives the scheduler one exact
//! wake for the earliest deadline.

use super::SceneGraph;
use crate::mutation::{MutationBatch, MutationResult, SceneMutation};
use crate::types::SceneId;

/// A batch held until its `present_at` wall-clock time.
#[derive(Clone, Debug)]
pub struct ScheduledBatch {
    pub present_at_wall_us: u64,
    pub batch: MutationBatch,
}

impl SceneGraph {
    /// Hold `batch` until `present_at_wall_us`, then apply it on the first
    /// commit at or after that time. Validation happens at apply time.
    pub fn schedule_batch(&mut self, present_at_wall_us: u64, batch: MutationBatch) {
        self.scheduled_batches.push(ScheduledBatch {
            present_at_wall_us,
            batch,
        });
    }

    /// Apply every scheduled batch whose `present_at` has arrived, oldest
    /// deadline first. Returns each batch's result so callers can report
    /// late rejections.
    pub fn apply_due_batches(&mut self) -> Vec<MutationResult> {
        let now_us = self.clock.now_us();
        if !self
            .scheduled_batches
            .iter()
            .any(|s| s.present_at_wall_us <= now_us)
        {
            return Vec::new();
        }
        let (mut due, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut self.scheduled_batches)
            .into_iter()
            .partition(|s| s.present_at_wall_us <= now_us);
        self.scheduled_batches = pending;
        due.sort_by_key(|s| s.present_at_wall_us);
        due.into_iter()
            .map(|s| self.apply_batch(&s.batch))
            .collect()
    }

    /// Stamp the batch's `expires_at` on the tiles it created or targeted.
    /// Called after a batch commits.
    pub(crate) fn stamp_batch_expiry(&mut self, batch: &MutationBatch, created_ids: &[SceneId]) {
        let Some(expires_at) = batch
            .timing_hints
            .as_ref()
            .and_then(|h| h.expires_at_wall_us)
            .filter(|t| t.0 > 0)
        else {
            return;
        };
        let targeted = batch.mutations.iter().filter_map(target_tile_id);
        for tile_id in created_ids.iter().copied().chain(targeted) {
            if let Some(tile) = self.tiles.get_mut(&tile_id) {
                tile.expires_at = Some(expires_at.0);
            }
        }
    }

    /// Remove tiles whose `expires_at` has passed. Returns the removed ids.
    pub fn drain_expired_tiles(&mut self) -> Vec<SceneId> {
        let now_us = self.clock.now_us();
        let expired: Vec<SceneId> = self
            .tiles
            .values()
            .filter(|t| t.expires_at.is_some_and(|at| at <= now_us))
            .map(|t| t.id)
            .collect();
        for tile_id in &expired {
            self.remove_tile_and_nodes(*tile_id);
        }
        if !expired.is_empty() {
            self.version += 1;
        }
        expired
    }

    /// Earliest wall-clock deadline among scheduled batches and tile expiries.
    pub fn next_timed_content_wall_us(&self) -> Option<u64> {
        let scheduled = self
            .scheduled_batches
            .iter()
            .map(|s| s.present_at_wall_us)
            .min();
        let tile_expiry = self.tiles.values().filter_map(|t| t.expires_at).min();
        scheduled.into_iter().chain(tile_expiry).min()
    }
}

/// The existing tile a mutation targets, if any (creation is handled via
/// the batch's created ids; deletion needs no expiry).
fn target_tile_id(mutation: &SceneMutation) -> Option<SceneId> {
    match mutation {
        SceneMutation::UpdateTileBounds { tile_id, .. }
        | SceneMutation::UpdateTileOpacity { tile_id, .. }
        | SceneMutation::UpdateTileInputMode { tile_id, .. }
        | SceneMutation::SetTileRoot { tile_id, .. }
        | SceneMutation::AddNode { tile_id, .. }
        | SceneMutation::UpdateNodeContent { tile_id, .. } => Some(*tile_id),
        _ => None,
    }
}
