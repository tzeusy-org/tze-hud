//! A reserved answer stays in the scene FIFO until its reply is encoded.

use crate::tools::InputPollLease;
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex, MutexGuard};
use tze_hud_scene::{SceneGraph, SceneId, render_wake::RenderWakeNotifier};

pub(crate) struct WaitClaim {
    pub scene: Arc<Mutex<SceneGraph>>,
    pub id: SceneId,
    pub namespace: String,
    pub active: Arc<AtomicBool>,
    pub ready: RenderWakeNotifier,
}

impl Drop for WaitClaim {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        if let Ok(mut scene) = self.scene.try_lock() {
            scene.release_notification_wait(self.id);
        }
        // A concurrent input wait must observe any now-unclaimed answer even
        // when cancellation happened while another owner held the scene lock.
        self.ready.notify();
    }
}

struct PreparedAnswer<'a> {
    // Drop the lock before the claim's post-unlock readiness callback.
    scene: MutexGuard<'a, SceneGraph>,
    claim: WaitClaim,
}

/// Only dispatch can commit a prepared answer, after successful JSON encoding.
/// Dropping this value before then releases ownership without consuming input.
pub(crate) struct PublishReply<'a> {
    pub value: Value,
    answer: Option<PreparedAnswer<'a>>,
    _activity: Option<InputPollLease>,
}

impl<'a> PublishReply<'a> {
    pub(crate) fn plain(value: Value) -> Self {
        Self {
            value,
            answer: None,
            _activity: None,
        }
    }

    pub(crate) fn answered(
        value: Value,
        scene: MutexGuard<'a, SceneGraph>,
        claim: WaitClaim,
        activity: Option<InputPollLease>,
    ) -> Self {
        Self {
            value,
            answer: Some(PreparedAnswer { scene, claim }),
            _activity: activity,
        }
    }

    pub(crate) fn encoded(mut self) {
        if let Some(answer) = self.answer.as_mut() {
            answer
                .scene
                .consume_notification_wait_answer(answer.claim.id, &answer.claim.namespace);
        }
    }
}
