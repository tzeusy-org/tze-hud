//! Publication-specific ownership of one synchronous MCP question.
//!
//! Outcomes and notification debt are ordinary scene values, so a rejected
//! mutation batch rolls them back too. Owners flush readiness after scene unlock.

use super::{PendingAction, PendingActionKind, SceneGraph};
use crate::types::{ZoneContent, ZonePublishRecord};
use crate::{SceneId, render_wake::RenderWakeNotifier};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// An unexpired, unanswered card with a valid callback action. Counted once
/// per card; URL-only cards are not questions. `choices` holds answered cards.
pub(crate) fn is_unanswered_question(
    record: &ZonePublishRecord,
    choices: &HashMap<SceneId, String>,
    now_wall_us: u64,
) -> bool {
    record
        .expires_at_wall_us
        .is_none_or(|end| end > now_wall_us)
        && !choices.contains_key(&record.publication_id)
        && matches!(&record.content, ZoneContent::Notification(payload)
            if payload.actions.iter().any(|action|
                action.callback_id.is_some() && action.validate().is_ok()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationGone {
    Expired,
    Replaced,
    Displaced,
    Cleared,
}

impl NotificationGone {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Replaced => "replaced",
            Self::Displaced => "displaced",
            Self::Cleared => "cleared",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationOutcome {
    Answered,
    Gone(NotificationGone),
}

#[derive(Clone, Debug)]
pub(super) struct NotificationWait {
    pub namespace: String,
    pub active: Arc<AtomicBool>,
    pub deadline_mono_us: u64,
    pub materialized: bool,
    pub outcome: Option<NotificationOutcome>,
    pub notified: bool,
    pub ready: RenderWakeNotifier,
}

/// Displaced receipts one publisher can have waiting; older ones drop first.
pub const DISPLACED_RECEIPT_CAP: usize = 5;

impl SceneGraph {
    /// Arm under the same scene lock as admission, before any local activation.
    pub fn arm_notification_wait(
        &mut self,
        publication: SceneId,
        namespace: &str,
        active: Arc<AtomicBool>,
        deadline_mono_us: u64,
        ready: RenderWakeNotifier,
    ) {
        self.notification_waits
            .retain(|_, wait| wait.active.load(Ordering::Acquire));
        self.notification_waits.insert(
            publication,
            NotificationWait {
                namespace: namespace.to_owned(),
                active,
                deadline_mono_us,
                materialized: false,
                outcome: None,
                notified: false,
                ready,
            },
        );
    }

    pub fn notification_wait_outcome(&self, publication: SceneId) -> Option<NotificationOutcome> {
        self.notification_waits
            .get(&publication)
            .and_then(|wait| wait.outcome)
    }

    pub fn release_notification_wait(&mut self, publication: SceneId) {
        self.notification_waits.remove(&publication);
    }

    /// Whether `record` is a question still waiting for the human.
    pub fn is_unanswered_question(&self, record: &ZonePublishRecord) -> bool {
        is_unanswered_question(
            record,
            &self.overlay.notification_choices,
            self.now_wall_us(),
        )
    }

    /// Tell a trimmed card's publisher it left the stack unseen. An armed
    /// question wait that already took the displacement owns the news; an
    /// answered or expired card left for that reason, not by trimming.
    pub(super) fn queue_displaced_receipt(&mut self, victim: ZonePublishRecord, now_wall_us: u64) {
        let ZoneContent::Notification(_) = &victim.content else {
            return;
        };
        let id = victim.publication_id;
        let claimed_by_wait = self.notification_waits.get(&id).is_some_and(|wait| {
            wait.active.load(Ordering::Acquire)
                && wait.outcome == Some(NotificationOutcome::Gone(NotificationGone::Displaced))
        });
        if claimed_by_wait
            || self.overlay.notification_choices.contains_key(&id)
            || victim
                .expires_at_wall_us
                .is_some_and(|end| end <= now_wall_us)
        {
            return;
        }
        let key = victim.merge_key.filter(|key| !key.is_empty());
        // Receipts are news, not accepted human events: a publisher that never
        // drains hud_input (gRPC-only, one-shot notifiers) must not grow the
        // queue at its publish rate. Keep the newest receipt per key and at
        // most DISPLACED_RECEIPT_CAP in all, dropping only older receipts.
        if let Some(queue) = self.pending_actions.get_mut(&victim.publisher_namespace) {
            let is_receipt = |a: &PendingAction| a.kind == PendingActionKind::Displaced;
            if key.is_some() {
                queue.retain(|a| !(is_receipt(a) && a.key == key));
            }
            while queue.iter().filter(|a| is_receipt(a)).count() >= DISPLACED_RECEIPT_CAP {
                let oldest = queue.iter().position(is_receipt).expect("counted above");
                queue.remove(oldest);
            }
        }
        let wake = self.push_pending_action(PendingAction {
            publication_id: Some(id),
            publisher_namespace: victim.publisher_namespace,
            zone_name: victim.zone_name,
            kind: PendingActionKind::Displaced,
            key,
        });
        self.pending_input_wakes.push(wake);
    }

    pub(super) fn finish_notification_wait(
        &mut self,
        publication: SceneId,
        outcome: NotificationOutcome,
    ) {
        let now = self.now_monotonic_us();
        if let Some(wait) = self.notification_waits.get_mut(&publication)
            && wait.active.load(Ordering::Acquire)
            && now < wait.deadline_mono_us
            && wait.outcome.is_none()
        {
            wait.outcome = Some(outcome);
        }
    }

    /// The actual mutation/commit owner takes this debt before unlock and calls
    /// the returned notifiers afterwards. No callback runs in a rollback scope.
    /// Displaced receipts queued since the last take ride the same debt.
    pub fn take_notification_wait_notifiers(&mut self) -> Vec<RenderWakeNotifier> {
        self.notification_waits
            .retain(|_, wait| wait.active.load(Ordering::Acquire));
        let mut wakes = std::mem::take(&mut self.pending_input_wakes);
        wakes.extend(self.notification_waits.values_mut().filter_map(|wait| {
            if wait.outcome.is_some() && !wait.notified {
                wait.notified = true;
                Some(wait.ready.clone())
            } else {
                None
            }
        }));
        wakes
    }

    pub(super) fn notification_answer_is_claimed(&self, action: &PendingAction) -> bool {
        action
            .publication_id
            .and_then(|id| self.notification_waits.get(&id))
            .is_some_and(|wait| {
                wait.namespace == action.publisher_namespace
                    && wait.active.load(Ordering::Acquire)
                    // Early answers remain private until encoding/cancellation.
                    // New post-deadline input is ordinary even before repoll.
                    && (wait.outcome == Some(NotificationOutcome::Answered)
                        || self.now_monotonic_us() < wait.deadline_mono_us)
            })
    }

    pub fn notification_wait_answer(
        &self,
        publication: SceneId,
        namespace: &str,
    ) -> Option<&PendingAction> {
        self.pending_actions.get(namespace)?.iter().find(|action| {
            action.publication_id == Some(publication)
                && action.kind != PendingActionKind::Displaced
        })
    }

    /// Called only after successful reply encoding, while the preparation lock
    /// still protects the exact answer. Cancellation never calls this method.
    pub fn consume_notification_wait_answer(&mut self, publication: SceneId, namespace: &str) {
        if let Some(actions) = self.pending_actions.get_mut(namespace) {
            actions.retain(|action| action.publication_id != Some(publication));
        }
        self.release_notification_wait(publication);
    }

    /// Other removal owners (lease expiry, unpair, unregister) mean cleared.
    /// Pending delayed admissions are ended explicitly by their scheduler.
    pub(super) fn prune_notification_waits(&mut self) {
        let ended: Vec<_> = self
            .notification_waits
            .iter()
            .filter_map(|(id, wait)| {
                (wait.materialized
                    && wait.outcome.is_none()
                    && !self
                        .zone_registry
                        .active_publishes
                        .values()
                        .flatten()
                        .any(|r| r.publication_id == *id))
                .then_some(*id)
            })
            .collect();
        for id in ended {
            self.finish_notification_wait(id, NotificationOutcome::Gone(NotificationGone::Cleared));
        }
    }
}
