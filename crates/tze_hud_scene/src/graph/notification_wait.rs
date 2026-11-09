//! Publication-specific ownership of one synchronous MCP question.
//!
//! Outcomes and notification debt are ordinary scene values, so a rejected
//! mutation batch rolls them back too. Owners flush readiness after scene unlock.

use super::{PendingAction, SceneGraph};
use crate::{SceneId, render_wake::RenderWakeNotifier};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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
    pub materialized: bool,
    pub outcome: Option<NotificationOutcome>,
    pub notified: bool,
    pub ready: RenderWakeNotifier,
}

impl SceneGraph {
    /// Arm under the same scene lock as admission, before any local activation.
    pub fn arm_notification_wait(
        &mut self,
        publication: SceneId,
        namespace: &str,
        active: Arc<AtomicBool>,
        ready: RenderWakeNotifier,
    ) {
        self.notification_waits
            .retain(|_, wait| wait.active.load(Ordering::Acquire));
        self.notification_waits.insert(
            publication,
            NotificationWait {
                namespace: namespace.to_owned(),
                active,
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

    pub(super) fn finish_notification_wait(
        &mut self,
        publication: SceneId,
        outcome: NotificationOutcome,
    ) {
        if let Some(wait) = self.notification_waits.get_mut(&publication)
            && wait.active.load(Ordering::Acquire)
            && wait.outcome.is_none()
        {
            wait.outcome = Some(outcome);
        }
    }

    /// The actual mutation/commit owner takes this debt before unlock and calls
    /// the returned notifiers afterwards. No callback runs in a rollback scope.
    pub fn take_notification_wait_notifiers(&mut self) -> Vec<RenderWakeNotifier> {
        self.notification_waits
            .retain(|_, wait| wait.active.load(Ordering::Acquire));
        self.notification_waits
            .values_mut()
            .filter_map(|wait| {
                if wait.outcome.is_some() && !wait.notified {
                    wait.notified = true;
                    Some(wait.ready.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub(super) fn notification_answer_is_claimed(&self, action: &PendingAction) -> bool {
        action
            .publication_id
            .and_then(|id| self.notification_waits.get(&id))
            .is_some_and(|wait| {
                wait.namespace == action.publisher_namespace && wait.active.load(Ordering::Acquire)
            })
    }

    pub fn notification_wait_answer(
        &self,
        publication: SceneId,
        namespace: &str,
    ) -> Option<&PendingAction> {
        self.pending_actions
            .get(namespace)?
            .iter()
            .find(|action| action.publication_id == Some(publication))
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
