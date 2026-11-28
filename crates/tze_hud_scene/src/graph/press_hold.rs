//! A card that uses the owner's answer window is not removed while a pointer
//! press that began on one of its controls before the deadline is still down
//! (docs/invariants.md §1). The runtime begins and ends the press; the scene
//! decides what it holds, so expiry, activation, feedback pruning and the
//! next-expiry wake all agree.

use super::SceneGraph;
use super::overlay::NotificationPress;
use crate::types::{ZoneHitRegion, ZoneInteractionKind, ZonePublishRecord};

/// How long past its deadline a press can hold a card. Covers a lost
/// button-up; not a token.
pub const NOTIFICATION_PRESS_HOLD_MAX_US: u64 = 5_000_000;

impl SceneGraph {
    /// A primary pointer press on `region` (an action or ×) was accepted.
    /// Replaces any earlier press.
    pub fn begin_notification_press(&mut self, region: &ZoneHitRegion) {
        self.end_notification_press();
        if !matches!(
            region.kind,
            ZoneInteractionKind::Dismiss | ZoneInteractionKind::Action { .. }
        ) {
            return;
        }
        self.overlay.notification_press = Some(NotificationPress {
            region: region.clone(),
            began_wall_us: self.now_wall_us(),
        });
        // The compositor stops a TTL fade on a card the press can hold.
        if self.press_held_record().is_some() {
            self.bump_geometry_epoch();
        }
    }

    /// The press ended (release, cancel or a completed hold). A past-due
    /// card it held expires now. Returns whether the scene changed.
    pub fn end_notification_press(&mut self) -> bool {
        let held = self.press_held_record().is_some();
        if self.overlay.notification_press.take().is_none() {
            return false;
        }
        if held {
            self.drain_expired_zone_publications();
            self.bump_geometry_epoch();
        }
        held
    }

    /// Whether the press in progress keeps `record` past its deadline: the
    /// card uses the answer window, is unanswered and visible, the press
    /// began on it before the deadline, and the bound has not passed.
    pub fn notification_press_holds(&self, record: &ZonePublishRecord) -> bool {
        let (Some(press), Some(expiry)) = (
            self.overlay.notification_press.as_ref(),
            record.expires_at_wall_us,
        ) else {
            return false;
        };
        record.answer_window
            && press.region.publication_id == record.publication_id
            && press.region.zone_name == record.zone_name
            && press.region.publisher_namespace == record.publisher_namespace
            && press.began_wall_us < expiry
            && self.now_wall_us() < expiry.saturating_add(NOTIFICATION_PRESS_HOLD_MAX_US)
            && !self
                .overlay
                .notification_choices
                .contains_key(&record.publication_id)
            && !self.owner_hidden().zones
    }

    /// Whether `record` is still showing at `now`: before its effective
    /// expiry (an open reply field has none), or held by the press in
    /// progress.
    pub(crate) fn publication_live(&self, record: &ZonePublishRecord, now: u64) -> bool {
        self.zone_publication_expiry(record)
            .is_none_or(|expiry| expiry > now)
            || self.notification_press_holds(record)
    }

    /// When `record` is next due to leave: its effective expiry, or the press
    /// bound while the press holds it.
    pub(crate) fn publication_due_wall_us(&self, record: &ZonePublishRecord) -> Option<u64> {
        self.zone_publication_expiry(record).map(|expiry| {
            if self.notification_press_holds(record) {
                expiry.saturating_add(NOTIFICATION_PRESS_HOLD_MAX_US)
            } else {
                expiry
            }
        })
    }

    fn press_held_record(&self) -> Option<&ZonePublishRecord> {
        let press = self.overlay.notification_press.as_ref()?;
        self.zone_registry
            .active_for_zone(&press.region.zone_name)
            .iter()
            .find(|record| self.notification_press_holds(record))
    }
}
