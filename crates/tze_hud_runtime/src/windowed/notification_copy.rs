//! Owner-local clipboard writes for notification actions.

use std::time::{Duration, Instant};

use tze_hud_scene::graph::overlay::NotificationCopyFeedback;
use tze_hud_scene::{SceneId, ZoneContent};

use super::WinitApp;
use crate::idle_efficiency::RuntimeWakeupSource;

#[derive(Clone, Copy)]
pub(super) struct CopyFeedback {
    result: NotificationCopyFeedback,
    pub(super) until: Instant,
}

impl WinitApp {
    pub(super) fn copy_notification_text(
        &mut self,
        publication_id: SceneId,
        index: usize,
        text: &str,
    ) {
        let succeeded = (self.state.notification_clipboard.write)(text).is_ok();
        let hold = self
            .state
            .global_tokens
            .get("notification.copy.feedback.ms")
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(1200);
        self.state.copy_feedback = Some(CopyFeedback {
            result: NotificationCopyFeedback {
                publication_id,
                index,
                succeeded,
            },
            until: Instant::now() + Duration::from_millis(hold.into()),
        });
        self.state
            .wake
            .mark_main_work_pending(RuntimeWakeupSource::SceneChange);
    }

    pub(super) fn settle_copy_feedback(&mut self, now: Instant) -> bool {
        let Some(feedback) = self.state.copy_feedback else {
            return false;
        };
        let Ok(shared) = self.state.shared_state.try_lock() else {
            self.schedule_shared_scene_availability_wake();
            return false;
        };
        let Ok(mut scene) = shared.scene.try_lock() else {
            drop(shared);
            self.schedule_shared_scene_availability_wake();
            return false;
        };
        let id = feedback.result.publication_id;
        let shown = now < feedback.until
            && scene.zones_shown()
            && !scene.overlay.notification_choices.contains_key(&id)
            && scene
                .zone_registry
                .active_publishes
                .values()
                .flatten()
                .any(|record| {
                    record.publication_id == id
                        && matches!(&record.content, ZoneContent::Notification(payload)
                            if payload.actions.get(feedback.result.index)
                                .is_some_and(|action| action.copy.is_some()))
                });
        if !shown {
            self.state.copy_feedback = None;
        }
        let result = shown.then_some(feedback.result);
        if scene.overlay.notification_copy_feedback == result {
            return false;
        }
        scene.overlay.notification_copy_feedback = result;
        scene.version += 1;
        true
    }
}
