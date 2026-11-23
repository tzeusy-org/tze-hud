//! Owner answer intent: resolve a current rendered question on the main
//! thread, then take native focus after releasing every scene/state guard.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tze_hud_scene::{ZoneContent, ZoneInteractionKind};

use super::WinitApp;
use super::keyboard::keyboard_key_identity;
use super::lifecycle::{INTERACTION_LOCK_BUDGET, spin_acquire};
use crate::shell::control_panel::TrayVisit;

#[derive(Default)]
pub(super) struct AnswerKeyboardState {
    pub requested: Arc<AtomicBool>,
    /// A tray click: questions only, and a stale click opens the panel.
    pub tray_requested: Arc<Mutex<Option<TrayVisit>>>,
    pub focus: Option<AnswerFocus>,
    pub return_key: Option<String>,
    /// The key holding a destructive action; its release ends the hold.
    pub hold_key: Option<String>,
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
    /// A destructive action began a hold under this key.
    pub hold_key: Option<String>,
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
        hold_key: None,
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
    if super::notification_hold::needs_hold(scene, target, tokens) {
        let key = keyboard_key_identity(&raw.key_code, &raw.key);
        scene.begin_notification_hold(target.clone(), Some(key.clone()));
        outcome.hold_key = Some(key);
        return outcome;
    }
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

/// The shown, live portal on `tab_id` whose latest output expects a reply,
/// newest by that output's time.
fn newest_waiting_portal(
    scene: &tze_hud_scene::graph::SceneGraph,
    tab_id: tze_hud_scene::SceneId,
    driver: &crate::portal::PortalDriver,
) -> Option<tze_hud_scene::SceneId> {
    let handle = driver.portal_handle();
    let hub = handle.hub.lock().ok()?;
    let visible_bytes = hub.limits().visible_bytes;
    scene
        .tiles
        .iter()
        .filter(|(_, tile)| {
            tile.tab_id == tab_id
                && tile.namespace == crate::portal::PORTAL_DRIVER_NAMESPACE
                && scene.tile_shown(tile)
        })
        .filter_map(|(tile_id, _)| {
            let portal = hub.get(&driver.key_for_tile(*tile_id)?)?;
            let latest = portal.visible_transcript(visible_bytes).last()?;
            let waiting = portal.degraded_since_us.is_none() && latest.expects_reply;
            waiting.then_some((latest.at_us, *tile_id))
        })
        .max_by_key(|(at_us, tile_id)| (*at_us, tile_id.to_bytes_le()))
        .map(|(_, tile_id)| tile_id)
}

impl AnswerKeyboardState {
    /// The tray's answer handoff: the chord's main-thread path, marked as a
    /// tray visit.
    #[cfg(any(target_os = "windows", test, feature = "test-harness"))]
    pub(super) fn tray_hook(
        &self,
        wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    ) -> crate::shell::control_panel::AnswerHook {
        let requested = Arc::clone(&self.tray_requested);
        Arc::new(move |visit| {
            *requested.lock().unwrap_or_else(|e| e.into_inner()) = Some(visit);
            wake.notify();
        })
    }
}

impl WinitApp {
    pub(super) fn sync_answer_focus(&mut self) {
        let keyboard = &self.state.answer_keyboard;
        let chord = keyboard.requested.swap(false, Ordering::AcqRel);
        let tray = keyboard
            .tray_requested
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if !chord && tray.is_none() {
            return;
        }
        let paused = self.state.safe_mode_atomic.load(Ordering::Acquire);
        let panel_open = self
            .state
            .control_panel
            .as_ref()
            .is_some_and(|panel| panel.is_open());
        let found = if paused {
            Ok(false)
        } else {
            self.focus_newest_question(super::lifecycle::hud_owns_foreground() && !panel_open)
        };
        // Cards win; with none, only the chord falls through to a portal.
        let found = match found {
            Ok(false) if chord && tray.is_none() && !paused => self.focus_waiting_portal(),
            found => found,
        };
        match found {
            Ok(true) => {
                if let Some(visit) = tray {
                    // Only cards answer a tray click. The tray took the
                    // foreground; return to the app that was behind it.
                    super::lifecycle::redirect_focus_return(visit.origin);
                }
                if panel_open && let Some(panel) = &self.state.control_panel {
                    // This explicit owner handoff closes settings only after a
                    // question was found. Its close edge must not restore focus
                    // underneath the new card visit.
                    panel.send(crate::shell::control_panel::PanelEvent::Chord(
                        tze_hud_scene::config::ChordRole::Answer,
                    ));
                }
            }
            #[cfg(any(target_os = "windows", test, feature = "test-harness"))]
            Ok(false) => {
                // The question went between click and focus: a click is never dead.
                if let (Some(panel), Some(visit)) = (&self.state.control_panel, tray) {
                    use crate::shell::control_panel::{PanelEvent, TrayCommand};
                    panel.send(PanelEvent::Tray(TrayCommand::OpenPanel, visit));
                }
            }
            Err(()) => {
                let keyboard = &self.state.answer_keyboard;
                keyboard.requested.fetch_or(chord, Ordering::AcqRel);
                if let Some(visit) = tray {
                    keyboard
                        .tray_requested
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_or_insert(visit);
                }
                self.schedule_shared_scene_availability_wake();
            }
            #[cfg(not(any(target_os = "windows", test, feature = "test-harness")))]
            Ok(false) => {}
        }
    }

    /// With no question, the chord puts the caret in the composer of the
    /// newest shown, live portal whose latest output expects a reply, exactly
    /// as a click there would. No agent sees the move.
    fn focus_waiting_portal(&mut self) -> Result<bool, ()> {
        let (transition, bounds) = {
            let state = spin_acquire(&self.state.shared_state, INTERACTION_LOCK_BUDGET).ok_or(())?;
            let mut scene = spin_acquire(&state.scene, INTERACTION_LOCK_BUDGET).ok_or(())?;
            let Some(tab_id) = scene.active_tab else {
                return Ok(false);
            };
            let Some(tile_id) =
                newest_waiting_portal(&scene, tab_id, &self.state.portal_projection_driver)
            else {
                return Ok(false);
            };
            let tile = &scene.tiles[&tile_id];
            let bounds = tile.bounds;
            let Some(composer) = tze_hud_input::find_composer_node(tile.root_node, &scene.nodes)
            else {
                return Ok(false);
            };
            let transition = self.state.input_processor.recover_composer_focus(
                &mut self.state.focus_manager,
                &mut scene,
                tab_id,
                tile_id,
                composer,
            );
            (transition, bounds)
        };
        let composer_focus_lost = transition.lost.is_some();
        super::input_dispatch::dispatch_focus_event(&self.state.input_event_tx, transition);
        if composer_focus_lost {
            self.clear_local_composer_echo();
        }
        self.focus_native_window_at(
            bounds.x + bounds.width * 0.5,
            bounds.y + bounds.height * 0.5,
        );
        Ok(true)
    }

    /// The overlay window that shows scene point (x, y): `Some` index into
    /// the secondaries, `None` for the primary.
    pub(super) fn display_window_at(&self, x: f32, y: f32) -> Option<usize> {
        // The published layout lists the secondaries in window order.
        self.state
            .placement_layout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .displays()
            .iter()
            .filter(|display| !display.primary)
            .position(|display| display.rect.contains_point(x, y))
    }

    /// Take OS keyboard focus on the window that shows scene point (x, y).
    pub(super) fn focus_native_window_at(&mut self, x: f32, y: f32) {
        let secondary = self.display_window_at(x, y);
        let window = secondary
            .and_then(|index| self.state.secondaries.get(index))
            .map(|display| &display.window)
            .or(self.state.window.as_ref());
        if let Some(window) = window {
            super::lifecycle::focus_window_for_text_input(window);
        }
        self.state.keyboard_window = secondary;
        self.state
            .wake
            .mark_main_work_pending(crate::idle_efficiency::RuntimeWakeupSource::SceneChange);
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
            // Initial focus is the primary action (the first that is not
            // destructive), or the first action when all are destructive.
            let primary = scene
                .zone_registry
                .active_for_zone(&newest.zone_name)
                .iter()
                .find(|record| record.publication_id == newest.publication_id)
                .and_then(|record| match &record.content {
                    ZoneContent::Notification(payload) => payload.primary_action(),
                    _ => None,
                });
            let first = scene
                .overlay
                .zone_hit_regions
                .iter()
                .filter(|region| {
                    region.publication_id == newest.publication_id
                        && matches!(region.kind, ZoneInteractionKind::Action { .. })
                })
                .min_by_key(|region| match region.kind {
                    ZoneInteractionKind::Action { index } => (Some(index) != primary, index),
                    _ => (true, usize::MAX),
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
        self.focus_native_window_at(
            bounds.x + bounds.width * 0.5,
            bounds.y + bounds.height * 0.5,
        );
        Ok(true)
    }

    pub(super) fn return_focus_after_key(&mut self, raw: &tze_hud_input::RawKeyDownEvent) {
        let identity = keyboard_key_identity(&raw.key_code, &raw.key);
        self.state.notification_keydowns.insert(identity.clone());
        self.state.answer_keyboard.return_key = Some(identity);
    }
}
