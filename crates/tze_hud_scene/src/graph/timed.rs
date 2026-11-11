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
    pub(crate) publication_origin: Option<SceneId>,
}

impl ScheduledBatch {
    /// Preserve an already accepted widget publication until it can present.
    /// A mixed batch waits as a whole so its mutations remain atomic.
    fn waits_for_widget_resume(&self, widgets_paused: bool) -> bool {
        widgets_paused
            && self
                .batch
                .mutations
                .iter()
                .any(|mutation| matches!(mutation, SceneMutation::PublishToWidget { .. }))
    }
}

impl SceneGraph {
    /// Hold `batch` until `present_at_wall_us`, then apply it on the first
    /// commit at or after that time. Validation happens at apply time.
    pub fn schedule_batch(&mut self, present_at_wall_us: u64, batch: MutationBatch) {
        self.schedule_batch_for_origin(present_at_wall_us, batch, None);
    }

    /// The preclaim gRPC verb preserves its session ownership while held.
    pub fn schedule_batch_for_origin(
        &mut self,
        present_at_wall_us: u64,
        batch: MutationBatch,
        publication_origin: Option<SceneId>,
    ) {
        let publication_origin = publication_origin.filter(|_| batch.lease_id.is_none());
        self.scheduled_batches.push(ScheduledBatch {
            present_at_wall_us,
            batch,
            publication_origin,
        });
    }

    /// Drop `namespace`'s pending publishes to `zone` (they have not
    /// materialized, so `clear` has nothing else to remove). Other agents'
    /// batches, and the rest of a mixed batch, are untouched; an emptied batch
    /// disappears, taking its wake deadline with it. Returns how many
    /// publishes were cancelled.
    pub fn cancel_scheduled_zone_publishes(&mut self, zone: &str, namespace: &str) -> usize {
        let mut cancelled = 0;
        for s in &mut self.scheduled_batches {
            if s.batch.agent_namespace == namespace {
                let before = s.batch.mutations.len();
                s.batch.mutations.retain(|m| !is_publish_to(m, zone));
                cancelled += before - s.batch.mutations.len();
            }
        }
        self.scheduled_batches
            .retain(|s| !s.batch.mutations.is_empty());
        self.prune_publication_orphans();
        cancelled
    }

    pub(crate) fn cancel_scheduled_notification_key(
        &mut self,
        zone: &str,
        namespace: &str,
        key: &str,
    ) {
        for scheduled in &mut self.scheduled_batches {
            if scheduled.batch.agent_namespace == namespace {
                scheduled.batch.mutations.retain(|mutation| {
                    !matches!(mutation, SceneMutation::PublishToZone {
                        zone_name, content: crate::types::ZoneContent::Notification(_),
                        merge_key: Some(candidate), ..
                    } if zone_name == zone && candidate == key)
                });
            }
        }
        self.scheduled_batches
            .retain(|scheduled| !scheduled.batch.mutations.is_empty());
        self.prune_publication_orphans();
    }

    /// Cancel only this publisher's pending widget content, preserving peers
    /// and unrelated mutations in a mixed batch.
    pub fn cancel_scheduled_widget_publishes(&mut self, widget: &str, namespace: &str) -> usize {
        let mut cancelled = 0;
        for scheduled in &mut self.scheduled_batches {
            if scheduled.batch.agent_namespace != namespace {
                continue;
            }
            let before = scheduled.batch.mutations.len();
            scheduled.batch.mutations.retain(|mutation| {
                !matches!(mutation, SceneMutation::PublishToWidget { widget_name, .. } if widget_name == widget)
            });
            cancelled += before - scheduled.batch.mutations.len();
        }
        self.scheduled_batches
            .retain(|s| !s.batch.mutations.is_empty());
        self.prune_publication_orphans();
        cancelled
    }

    /// A held pending widget starts its new TTL at actual presentation.
    pub fn hold_scheduled_widget_publishes(
        &mut self,
        widget: &str,
        namespace: &str,
        ttl_us: Option<u64>,
    ) -> bool {
        let mut held = false;
        for scheduled in &mut self.scheduled_batches {
            if scheduled.batch.agent_namespace != namespace {
                continue;
            }
            for mutation in &mut scheduled.batch.mutations {
                if let SceneMutation::PublishToWidget {
                    widget_name,
                    ttl_us: pending_ttl,
                    expires_at_wall_us,
                    ..
                } = mutation
                    && widget_name == widget
                {
                    *pending_ttl = ttl_us;
                    *expires_at_wall_us = None;
                    held = true;
                }
            }
        }
        held
    }

    /// Nominal pending expiry for discovery; a late drain starts relative TTL
    /// afresh. Owner auto-clear applies to pending and active content alike.
    pub fn scheduled_widget_publication_expiries<'a>(
        &'a self,
        widget: &'a str,
        namespace: &'a str,
    ) -> impl Iterator<Item = Option<u64>> + 'a {
        let auto_clear_us = self
            .widget_registry
            .instances
            .get(widget)
            .and_then(|instance| instance.auto_clear_ms)
            .map(|ms| ms.saturating_mul(1_000));
        self.scheduled_batches
            .iter()
            .filter(move |scheduled| scheduled.batch.agent_namespace == namespace)
            .flat_map(move |scheduled| {
                scheduled
                    .batch
                    .mutations
                    .iter()
                    .filter_map(move |mutation| match mutation {
                        SceneMutation::PublishToWidget {
                            widget_name,
                            ttl_us,
                            expires_at_wall_us,
                            ..
                        } if widget_name == widget => {
                            let expiry = expires_at_wall_us.or_else(|| {
                                ttl_us.map(|ttl| scheduled.present_at_wall_us.saturating_add(ttl))
                            });
                            let cap = auto_clear_us
                                .map(|ttl| scheduled.present_at_wall_us.saturating_add(ttl));
                            Some(match (expiry, cap) {
                                (Some(a), Some(b)) => Some(a.min(b)),
                                (a, b) => a.or(b),
                            })
                        }
                        _ => None,
                    })
            })
    }

    /// Retime `namespace`'s pending publishes to `zone` as a hold would an
    /// active one, except that the expiry counts from presentation
    /// (`present_at + ttl_ms`; `None` holds until cleared). Returns whether
    /// any were pending.
    pub fn hold_scheduled_zone_publishes(
        &mut self,
        zone: &str,
        namespace: &str,
        ttl_us: Option<u64>,
    ) -> bool {
        let mut held = false;
        for s in &mut self.scheduled_batches {
            if s.batch.agent_namespace != namespace {
                continue;
            }
            for m in &mut s.batch.mutations {
                if let SceneMutation::PublishToZone {
                    zone_name,
                    expires_at_wall_us,
                    held: is_held,
                    ..
                } = m
                    && zone_name == zone
                {
                    *expires_at_wall_us = ttl_us.map(|t| s.present_at_wall_us.saturating_add(t));
                    *is_held = ttl_us.is_none();
                    held = true;
                }
            }
        }
        held
    }

    /// Expiry of each caller-owned pending publication, for holding discovery.
    pub fn scheduled_zone_publication_expiries<'a>(
        &'a self,
        zone: &'a str,
        namespace: &'a str,
    ) -> impl Iterator<Item = Option<u64>> + 'a {
        self.scheduled_batches
            .iter()
            .filter(move |s| s.batch.agent_namespace == namespace)
            .flat_map(|s| &s.batch.mutations)
            .filter_map(move |m| match m {
                SceneMutation::PublishToZone {
                    zone_name,
                    expires_at_wall_us,
                    ..
                } if zone_name == zone => Some(*expires_at_wall_us),
                _ => None,
            })
    }

    /// Apply every scheduled batch whose `present_at` has arrived, oldest
    /// deadline first. Returns each batch's result so callers can report
    /// late rejections.
    pub fn apply_due_batches(&mut self) -> Vec<MutationResult> {
        // Grace cancellation precedes materializing due content in every runtime.
        self.reap_orphaned_publications(self.clock.now_millis());
        let now_us = self.clock.now_us();
        let widgets_paused = self.widget_time_paused();
        let can_present_now = |scheduled: &ScheduledBatch| {
            scheduled.present_at_wall_us <= now_us
                && !scheduled.waits_for_widget_resume(widgets_paused)
        };
        if !self.scheduled_batches.iter().any(can_present_now) {
            return Vec::new();
        }
        let (mut due, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut self.scheduled_batches)
            .into_iter()
            .partition(can_present_now);
        self.scheduled_batches = pending;
        due.sort_by_key(|s| s.present_at_wall_us);
        // Due payloads stay outside the scene's per-batch rollback snapshots.
        // Defer orphan pruning until all due origins have had a chance to apply.
        let orphan_deadlines = std::mem::take(&mut self.publication_orphans);
        let results = due
            .into_iter()
            .map(|s| self.apply_batch_for_origin(&s.batch, s.publication_origin))
            .collect();
        self.publication_orphans = orphan_deadlines;
        self.prune_publication_orphans();
        results
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
        let widgets_paused = self.widget_time_paused();
        let scheduled = self
            .scheduled_batches
            .iter()
            .filter(|scheduled| !scheduled.waits_for_widget_resume(widgets_paused))
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

fn is_publish_to(mutation: &SceneMutation, zone: &str) -> bool {
    matches!(mutation, SceneMutation::PublishToZone { zone_name, .. } if zone_name == zone)
}
