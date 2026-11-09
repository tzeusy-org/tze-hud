//! Owner answer intent: resolve a current rendered question on the main
//! thread, then take native focus after releasing every scene/state guard.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tze_hud_scene::{ZoneContent, ZoneInteractionKind};

use super::WinitApp;
use super::keyboard::keyboard_key_identity;
use super::lifecycle::{INTERACTION_LOCK_BUDGET, spin_acquire};

#[derive(Default)]
pub(super) struct AnswerKeyboardState {
    pub requested: Arc<AtomicBool>,
    pub focus: Option<AnswerFocus>,
    pub return_key: Option<String>,
}

#[derive(Clone, Copy)]
pub(super) struct AnswerFocus {
    pub publication_id: tze_hud_scene::SceneId,
    pub nested: bool,
}

pub(super) struct AnswerKeyOutcome {
    pub activation: Option<tze_hud_scene::graph::NotificationActivation>,
    pub exit: bool,
    pub return_focus: bool,
}

/// A chord pins one publication for its entire keyboard interaction. Even
/// after replacement/expiry, keys cannot retarget a different question or
/// leak into the underlying composer; Escape can still leave that interaction.
pub(super) fn route_answer_key(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    focus: AnswerFocus,
    raw: &tze_hud_input::RawKeyDownEvent,
    tokens: &std::collections::HashMap<String, String>,
) -> AnswerKeyOutcome {
    let mut outcome = AnswerKeyOutcome {
        activation: None,
        exit: false,
        return_focus: false,
    };
    let modified = raw.modifiers.ctrl || raw.modifiers.alt || raw.modifiers.meta;
    if modified || raw.repeat {
        return outcome;
    }
    if super::keyboard::is_escape_key(&raw.key, &raw.key_code) {
        scene.focus_notification(None);
        outcome.exit = true;
        outcome.return_focus = !focus.nested;
        return outcome;
    }
    let mut controls: Vec<_> = scene
        .overlay
        .zone_hit_regions
        .iter()
        .filter(|region| {
            region.publication_id == focus.publication_id
                && matches!(region.kind, ZoneInteractionKind::Action { .. })
        })
        .cloned()
        .collect();
    controls.sort_by_key(|region| match region.kind {
        ZoneInteractionKind::Action { index } => index,
        _ => usize::MAX,
    });
    if controls.is_empty() {
        return outcome;
    }
    let current = controls.iter().position(|region| {
        scene.overlay.notification_focus.as_ref() == Some(&region.interaction_id)
    });
    let step = match raw.key_code.as_str() {
        "Tab" => Some(if raw.modifiers.shift { -1 } else { 1 }),
        "ArrowLeft" | "ArrowUp" => Some(-1),
        "ArrowRight" | "ArrowDown" => Some(1),
        _ => None,
    };
    if let Some(step) = step {
        let next =
            (current.unwrap_or(0) as isize + step).rem_euclid(controls.len() as isize) as usize;
        scene.focus_notification(Some(controls[next].interaction_id.clone()));
        return outcome;
    }
    let digit = if raw.modifiers.shift {
        None
    } else {
        match raw.key_code.as_str() {
            "Digit1" | "Numpad1" => Some(0),
            "Digit2" | "Numpad2" => Some(1),
            "Digit3" | "Numpad3" => Some(2),
            "Digit4" | "Numpad4" => Some(3),
            _ => None,
        }
    };
    let target = if let Some(digit) = digit {
        controls.iter().find(
            |region| matches!(region.kind, ZoneInteractionKind::Action { index } if index == digit),
        )
    } else if super::keyboard::is_activation_key(&raw.key, &raw.key_code) {
        current.map(|index| &controls[index])
    } else {
        None
    };
    let Some(target) = target else { return outcome };
    let duration = super::lifecycle::notification_exit_ms(scene, &target.zone_name, tokens);
    // A refused same-link repeat still finishes the owner's keyboard visit;
    // the existing per-publication/per-action limiter remains authoritative.
    let url = scene
        .zone_registry
        .active_for_zone(&target.zone_name)
        .iter()
        .any(|record| {
            record.publication_id == target.publication_id
                && matches!((&record.content, &target.kind),
                (ZoneContent::Notification(payload), ZoneInteractionKind::Action { index })
                    if payload.actions.get(*index).is_some_and(|action| action.url.is_some()))
        });
    outcome.activation = scene.activate_notification(target, duration);
    if outcome.activation.is_some() || url {
        scene.focus_notification(None);
        outcome.exit = true;
        outcome.return_focus = true;
    }
    outcome
}

impl WinitApp {
    pub(super) fn sync_answer_focus(&mut self) {
        if !self
            .state
            .answer_keyboard
            .requested
            .swap(false, Ordering::AcqRel)
        {
            return;
        }
        if self.state.safe_mode_atomic.load(Ordering::Acquire)
            || self
                .state
                .control_panel
                .as_ref()
                .is_some_and(|panel| panel.is_open())
        {
            return;
        }
        if self
            .focus_newest_question(super::lifecycle::hud_owns_foreground())
            .is_err()
        {
            self.state
                .answer_keyboard
                .requested
                .store(true, Ordering::Release);
            self.schedule_shared_scene_availability_wake();
        }
    }

    /// `already_foreground` comes from the OS, not a stale scene focus owner.
    /// The headless runtime parent also exercises both logical entry contexts.
    pub(super) fn focus_newest_question(&mut self, already_foreground: bool) -> Result<bool, ()> {
        let (bounds, focus) = {
            let state = spin_acquire(&self.state.shared_state, INTERACTION_LOCK_BUDGET).ok_or(())?;
            let mut scene = spin_acquire(&state.scene, INTERACTION_LOCK_BUDGET).ok_or(())?;
            if self.state.input_processor.chrome_feedback_cancel_pending {
                self.state.input_processor.clear_chrome_feedback(&mut scene);
            }
            scene.prune_notification_feedback();
            let now = scene.now_wall_us();
            let newest =
                scene
                    .overlay
                    .zone_hit_regions
                    .iter()
                    .filter(|region| {
                        let ZoneInteractionKind::Action { index } = region.kind else {
                            return false;
                        };
                        !scene.overlay.notification_choices.contains_key(&region.publication_id)
                    && scene.zone_registry.active_for_zone(&region.zone_name).iter().any(|record| {
                        record.publication_id == region.publication_id
                            && record.publisher_namespace == region.publisher_namespace
                            && record.expires_at_wall_us.is_none_or(|expiry| expiry > now)
                            && matches!(&record.content, ZoneContent::Notification(payload)
                                if payload.actions.get(index).is_some_and(|action|
                                    action.callback_id.is_some() && action.validate().is_ok()))
                    })
                    })
                    .max_by_key(|region| {
                        (
                            region.published_at_wall_us,
                            region.publication_id.to_bytes_le(),
                            &region.publisher_namespace,
                            &region.zone_name,
                        )
                    });
            let Some(newest) = newest else {
                return Ok(false);
            };
            let first = scene
                .overlay
                .zone_hit_regions
                .iter()
                .filter(|region| {
                    region.publication_id == newest.publication_id
                        && matches!(region.kind, ZoneInteractionKind::Action { .. })
                })
                .min_by_key(|region| match region.kind {
                    ZoneInteractionKind::Action { index } => index,
                    _ => usize::MAX,
                })
                .expect("eligible callback has a rendered action")
                .clone();
            let nested = already_foreground
                && scene.active_tab.is_some_and(|tab| {
                    self.state
                        .focus_manager
                        .current_owner(tab)
                        .tile_id()
                        .is_some()
                });
            let hints_changed = scene.overlay.notification_answer != Some(first.publication_id);
            scene.focus_notification(Some(first.interaction_id));
            scene.overlay.notification_answer = Some(first.publication_id);
            if hints_changed {
                scene.bump_geometry_epoch();
            }
            (
                first.bounds,
                AnswerFocus {
                    publication_id: first.publication_id,
                    nested,
                },
            )
        };
        // A fresh explicit chord owns a fresh keyboard interaction. Old
        // release debt can remain after the user switched apps while holding
        // a key; it must not suppress this new answer or its characters.
        self.state.notification_keydowns.clear();
        self.state.answer_keyboard.return_key = None;
        self.state.answer_keyboard.focus = Some(focus);
        let x = bounds.x + bounds.width * 0.5;
        let y = bounds.y + bounds.height * 0.5;
        let secondary = self.state.primary_monitor.as_ref().and_then(|primary| {
            self.state
                .secondaries
                .iter()
                .find(|display| display.spec.scene_rect(primary).contains_point(x, y))
        });
        let window = secondary
            .map(|display| &display.window)
            .or(self.state.window.as_ref());
        if let Some(window) = window {
            super::lifecycle::focus_window_for_text_input(window);
        }
        self.state
            .wake
            .mark_main_work_pending(crate::idle_efficiency::RuntimeWakeupSource::SceneChange);
        Ok(true)
    }

    pub(super) fn return_focus_after_key(&mut self, raw: &tze_hud_input::RawKeyDownEvent) {
        let identity = keyboard_key_identity(&raw.key_code, &raw.key);
        self.state.notification_keydowns.insert(identity.clone());
        self.state.answer_keyboard.return_key = Some(identity);
    }
}
