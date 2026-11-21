//! Destructive notification actions fire only after a press held for
//! `notification.action.hold.ms`. The scene records the hold (and draws its
//! sweep); this turn ends a hold whose press is gone and fires a due one.

use super::WinitApp;
use super::lifecycle::notification_exit_ms;
use tze_hud_scene::graph::{NotificationActivation, SceneGraph};

/// (hold ms, drain ms) from the live tokens.
pub(super) fn hold_timing(tokens: &std::collections::HashMap<String, String>) -> (u32, u32) {
    (
        tze_hud_config::tokens::notification_action_hold_ms(tokens),
        tze_hud_config::tokens::resolve_motion_duration_ms(tokens, "motion.state.ms"),
    )
}

/// End the current hold, if any, with no callback.
pub(super) fn cancel_hold(
    scene: &mut SceneGraph,
    tokens: &std::collections::HashMap<String, String>,
) -> bool {
    let (hold_ms, drain_ms) = hold_timing(tokens);
    scene.cancel_notification_hold(hold_ms, drain_ms)
}

/// Whether a press on `region` must start a hold instead of activating it.
pub(super) fn needs_hold(
    scene: &SceneGraph,
    region: &tze_hud_scene::ZoneHitRegion,
    tokens: &std::collections::HashMap<String, String>,
) -> bool {
    hold_timing(tokens).0 > 0 && scene.notification_needs_hold(region)
}

impl WinitApp {
    /// Cancel a hold whose pointer press or key is no longer down (or that
    /// safe mode interrupts), then fire a hold that has lasted long enough.
    /// Returns whether the scene changed.
    pub(super) fn settle_notification_hold(&mut self) -> bool {
        let mut activation = None;
        let changed = {
            let Ok(shared) = self.state.shared_state.try_lock() else {
                return false;
            };
            let Ok(mut scene) = shared.scene.try_lock() else {
                return false;
            };
            if scene.overlay.notification_hold.is_none() {
                self.state.answer_keyboard.hold_key = None;
                return false;
            }
            let before = scene.geometry_epoch;
            scene.prune_notification_feedback();
            let tokens = &self.state.global_tokens;
            let safe_mode = self
                .state
                .safe_mode_atomic
                .load(std::sync::atomic::Ordering::Acquire);
            let held = scene
                .overlay
                .notification_hold
                .as_ref()
                .is_some_and(|hold| match &hold.key {
                    None => self.state.zone_press.as_deref() == Some(&hold.region.interaction_id),
                    Some(key) => self.state.answer_keyboard.hold_key.as_ref() == Some(key),
                });
            if safe_mode || !held {
                cancel_hold(&mut scene, tokens);
            }
            let (hold_ms, _) = hold_timing(tokens);
            if let Some(hold) = scene.take_completed_notification_hold(hold_ms) {
                let exit_ms = notification_exit_ms(&scene, &hold.region.zone_name, tokens);
                activation = scene.activate_notification(&hold.region, exit_ms);
                match hold.key {
                    // The release that follows must not press anything again.
                    None => self.state.zone_press = None,
                    Some(key) => {
                        if matches!(activation, Some(NotificationActivation::Callback(_))) {
                            scene.focus_notification(None);
                            self.state.answer_keyboard.focus = None;
                            self.state.notification_keydowns.insert(key.clone());
                            self.state.answer_keyboard.return_key = Some(key);
                        }
                    }
                }
            }
            if scene.overlay.notification_hold.is_none() {
                self.state.answer_keyboard.hold_key = None;
            }
            scene.geometry_epoch != before
        };
        if let Some(activation) = activation {
            self.deliver_notification_activation(activation);
        }
        changed
    }
}
