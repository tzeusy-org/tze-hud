//! Schedule zone and widget publications for `present_at`.
//!
//! Arrival time is not presentation time (invariant 1). A publication whose
//! `present_at` is in the future waits in
//! [`SceneGraph::scheduled_publications`] and is applied by
//! [`SceneGraph::apply_due_publications`] in the frame commit stage once due.
//! [`SceneGraph::next_timed_content_wall_us`] gives the scheduler the earliest
//! eligible publication time.

use super::SceneGraph;
use super::publish::{Publication, Publisher};
use crate::types::SceneId;
use crate::validation::ValidationError;

/// A publication held until its `present_at` wall-clock time.
#[derive(Clone, Debug)]
pub struct ScheduledPublication {
    pub id: SceneId,
    pub present_at_wall_us: u64,
    pub publisher: Publisher,
    pub publication: Publication,
    pub notification_publication: Option<SceneId>,
}

impl ScheduledPublication {
    /// Preserve an already accepted widget publication until it can present.
    fn waits_for_widget_resume(&self, widgets_paused: bool) -> bool {
        widgets_paused && matches!(self.publication, Publication::Widget { .. })
    }

    /// Whether this is the agent's still-unleased resident-session content.
    pub(crate) fn has_origin(&self, origin: SceneId) -> bool {
        self.publisher.lease_id.is_none() && self.publisher.origin == Some(origin)
    }
}

/// The outcome of one due publication, reported so callers can log late
/// rejections.
#[derive(Clone, Debug)]
pub struct DuePublication {
    pub id: SceneId,
    pub result: Result<(), ValidationError>,
}

impl SceneGraph {
    /// Hold `publication` until `present_at_wall_us`, then apply it on the
    /// first commit at or after that time. Validation happens at apply time.
    /// A session origin is kept only for unleased content. Returns its id.
    pub fn schedule_publication(
        &mut self,
        present_at_wall_us: u64,
        mut publisher: Publisher,
        publication: Publication,
    ) -> SceneId {
        if publisher.lease_id.is_some() {
            publisher.origin = None;
        }
        let id = SceneId::new();
        self.scheduled_publications.push(ScheduledPublication {
            id,
            present_at_wall_us,
            publisher,
            publication,
            notification_publication: None,
        });
        id
    }

    /// Admit a resident raster before enqueue, then recheck its pairing when
    /// due through publish_with_id. Generic schedules retain their old policy.
    pub fn schedule_resident_raster(
        &mut self,
        at: u64,
        publisher: Publisher,
        publication: Publication,
    ) -> Result<SceneId, ValidationError> {
        let source = publisher
            .resident_raster
            .clone()
            .ok_or_else(Self::raster_unavailable)?;
        self.validate_resident_raster(&publication, &source)?;
        source
            .with_authorized(|| self.schedule_publication(at, publisher, publication))
            .ok_or_else(Self::raster_unavailable)
    }

    /// Reserve identity only for one notification publication. It never
    /// enters the resident protocol's origin identity.
    pub fn schedule_notification(
        &mut self,
        at: u64,
        publisher: Publisher,
        publication: Publication,
        notification_id: SceneId,
    ) -> SceneId {
        assert!(matches!(
            publication,
            Publication::Zone {
                content: crate::types::ZoneContent::Notification(_),
                ..
            }
        ));
        let id = SceneId::new();
        self.scheduled_publications.push(ScheduledPublication {
            id,
            present_at_wall_us: at,
            publisher: Publisher {
                origin: None,
                ..publisher
            },
            publication,
            notification_publication: Some(notification_id),
        });
        id
    }

    /// Remove the pending publications `cancel` selects, closing any
    /// notification wait they carried. Returns how many were removed.
    fn cancel_scheduled(&mut self, cancel: impl Fn(&ScheduledPublication) -> bool) -> usize {
        let before = self.scheduled_publications.len();
        let mut closed = Vec::new();
        self.scheduled_publications.retain(|scheduled| {
            let keep = !cancel(scheduled);
            if !keep {
                closed.extend(scheduled.notification_publication);
            }
            keep
        });
        for id in closed {
            self.finish_notification_wait(
                id,
                super::NotificationOutcome::Gone(super::NotificationGone::Cleared),
            );
        }
        self.prune_publication_orphans();
        before - self.scheduled_publications.len()
    }

    /// Drop `namespace`'s pending publishes to `zone` (they have not
    /// materialized, so `clear` has nothing else to remove). Other agents'
    /// publications are untouched. Returns how many publishes were cancelled.
    pub fn cancel_scheduled_zone_publishes(&mut self, zone: &str, namespace: &str) -> usize {
        self.cancel_scheduled(|s| {
            s.publisher.namespace == namespace && s.publication.zone_name() == Some(zone)
        })
    }

    pub(crate) fn cancel_scheduled_notification_key(
        &mut self,
        zone: &str,
        namespace: &str,
        key: &str,
    ) {
        self.cancel_scheduled(|s| {
            s.publisher.namespace == namespace
                && matches!(&s.publication, Publication::Zone {
                    zone_name, content: crate::types::ZoneContent::Notification(_),
                    merge_key: Some(candidate), ..
                } if zone_name == zone && candidate == key)
        });
    }

    /// Cancel only this publisher's pending widget content, preserving peers.
    pub fn cancel_scheduled_widget_publishes(&mut self, widget: &str, namespace: &str) -> usize {
        self.cancel_scheduled(|s| {
            s.publisher.namespace == namespace && s.publication.widget_name() == Some(widget)
        })
    }

    /// A held pending widget starts its new TTL at actual presentation.
    pub fn hold_scheduled_widget_publishes(
        &mut self,
        widget: &str,
        namespace: &str,
        ttl_us: Option<u64>,
    ) -> bool {
        let mut held = false;
        for scheduled in &mut self.scheduled_publications {
            if scheduled.publisher.namespace != namespace {
                continue;
            }
            if let Publication::Widget {
                widget_name,
                ttl_us: pending_ttl,
                expires_at_wall_us,
                ..
            } = &mut scheduled.publication
                && widget_name == widget
            {
                *pending_ttl = ttl_us;
                *expires_at_wall_us = None;
                held = true;
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
        self.scheduled_publications
            .iter()
            .filter(move |scheduled| scheduled.publisher.namespace == namespace)
            .filter_map(move |scheduled| match &scheduled.publication {
                Publication::Widget {
                    widget_name,
                    ttl_us,
                    expires_at_wall_us,
                    ..
                } if widget_name == widget => {
                    let expiry = expires_at_wall_us.or_else(|| {
                        ttl_us.map(|ttl| scheduled.present_at_wall_us.saturating_add(ttl))
                    });
                    let cap =
                        auto_clear_us.map(|ttl| scheduled.present_at_wall_us.saturating_add(ttl));
                    Some(match (expiry, cap) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    })
                }
                _ => None,
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
        for s in &mut self.scheduled_publications {
            if s.publisher.namespace != namespace {
                continue;
            }
            if let Publication::Zone {
                zone_name,
                expires_at_wall_us,
                held: is_held,
                ..
            } = &mut s.publication
                && zone_name == zone
            {
                *expires_at_wall_us = ttl_us.map(|t| s.present_at_wall_us.saturating_add(t));
                *is_held = ttl_us.is_none();
                held = true;
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
        self.scheduled_publications
            .iter()
            .filter(move |s| s.publisher.namespace == namespace)
            .filter_map(move |s| match &s.publication {
                Publication::Zone {
                    zone_name,
                    expires_at_wall_us,
                    ..
                } if zone_name == zone => Some(*expires_at_wall_us),
                _ => None,
            })
    }

    /// Apply every scheduled publication whose `present_at` has arrived,
    /// oldest deadline first. Returns each one's outcome so callers can
    /// report late rejections.
    pub fn apply_due_publications(&mut self) -> Vec<DuePublication> {
        // Grace cancellation precedes materializing due content in every runtime.
        self.reap_orphaned_publications(self.clock.now_millis());
        let now_us = self.clock.now_us();
        let widgets_paused = self.widget_time_paused();
        let can_present_now = |scheduled: &ScheduledPublication| {
            scheduled.present_at_wall_us <= now_us
                && !scheduled.waits_for_widget_resume(widgets_paused)
        };
        if !self.scheduled_publications.iter().any(can_present_now) {
            return Vec::new();
        }
        let (mut due, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut self.scheduled_publications)
            .into_iter()
            .partition(can_present_now);
        self.scheduled_publications = pending;
        due.sort_by_key(|s| s.present_at_wall_us);
        // Defer orphan pruning until all due origins have had a chance to apply.
        let orphan_deadlines = std::mem::take(&mut self.publication_orphans);
        let results = due
            .into_iter()
            .map(|s| {
                let result = self.publish_with_id(
                    s.id,
                    &s.publisher,
                    &s.publication,
                    s.notification_publication,
                );
                if result.is_err()
                    && let Some(id) = s.notification_publication
                {
                    self.finish_notification_wait(
                        id,
                        super::NotificationOutcome::Gone(super::NotificationGone::Cleared),
                    );
                }
                DuePublication { id: s.id, result }
            })
            .collect();
        self.publication_orphans = orphan_deadlines;
        self.prune_publication_orphans();
        results
    }

    /// Earliest wall-clock deadline among scheduled publications.
    pub fn next_timed_content_wall_us(&self) -> Option<u64> {
        let widgets_paused = self.widget_time_paused();
        self.scheduled_publications
            .iter()
            .filter(|scheduled| !scheduled.waits_for_widget_resume(widgets_paused))
            .map(|s| s.present_at_wall_us)
            .min()
    }
}
