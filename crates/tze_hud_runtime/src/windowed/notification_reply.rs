//! Local editing for one focused notification. Collapsed drafts stay with
//! their exact publications and never enter the publisher's input queue.

use std::collections::{HashMap, HashSet};

use tze_hud_input::{
    composer_draft::{ComposerDraft, EditOutcome},
    keyboard::{RawCharacterEvent, RawKeyDownEvent},
};
use tze_hud_scene::{
    SceneId, ZoneHitRegion, ZoneInteractionKind,
    graph::{SceneGraph, overlay::NotificationReplyView},
    types::NOTIFICATION_REPLY_MAX_BYTES,
};

#[derive(Default)]
pub(super) struct NotificationReplyState {
    pub(super) active: Option<ZoneHitRegion>,
    pub(super) keydowns: HashSet<String>,
    pub(super) ignore_character_at: Option<tze_hud_scene::MonoUs>,
    blur_pending: bool,
    refresh_view: bool,
    drafts: HashMap<SceneId, ReplyDraft>,
}

struct ReplyDraft {
    draft: ComposerDraft,
    hit_limit: bool,
}

enum ReplyKey {
    Edit,
    Submit,
    Collapse,
}

impl Default for ReplyDraft {
    fn default() -> Self {
        Self {
            draft: ComposerDraft::new(NOTIFICATION_REPLY_MAX_BYTES),
            hit_limit: false,
        }
    }
}

impl NotificationReplyState {
    fn activate(&mut self, region: ZoneHitRegion) {
        self.drafts.entry(region.publication_id).or_default();
        self.active = Some(region);
        self.blur_pending = false;
        self.refresh_view = true;
    }

    fn prune(&mut self, scene: &SceneGraph) {
        self.drafts.retain(|id, _| {
            !scene.overlay.notification_choices.contains_key(id)
                && scene
                    .zone_registry
                    .active_publishes
                    .values()
                    .flatten()
                    .any(|record| record.publication_id == *id)
        });
    }

    fn editor(&mut self, suspended: bool) -> Option<(&ZoneHitRegion, &mut ReplyDraft)> {
        let region = self.active.as_ref()?;
        let editor = self.drafts.get_mut(&region.publication_id)?;
        editor.draft.set_suspended(suspended);
        Some((region, editor))
    }
}

impl super::WinitApp {
    pub(super) fn open_notification_reply(&mut self, region: ZoneHitRegion) {
        let (x, y) = (
            region.bounds.x + region.bounds.width * 0.5,
            region.bounds.y + region.bounds.height * 0.5,
        );
        self.state.notification_reply.activate(region);
        self.sync_notification_reply();
        if self.state.notification_reply.active.is_some() {
            self.focus_native_window_at(x, y);
        }
    }

    pub(super) fn blur_notification_reply(&mut self) {
        if self.state.notification_reply.active.is_some() {
            self.state.notification_reply.blur_pending = true;
            self.sync_notification_reply();
        }
    }

    pub(super) fn sync_notification_reply(&mut self) {
        use super::lifecycle::{INTERACTION_LOCK_BUDGET, spin_acquire};
        let reply = &mut self.state.notification_reply;
        if reply.active.is_none() && reply.drafts.is_empty() {
            return;
        }
        let (restore, waits) = {
            let Some(state) = spin_acquire(&self.state.shared_state, INTERACTION_LOCK_BUDGET)
            else {
                return;
            };
            let Some(mut scene) = spin_acquire(&state.scene, INTERACTION_LOCK_BUDGET) else {
                return;
            };
            if reply.blur_pending {
                reply.blur_pending = false;
                reply.active = None;
                scene.set_notification_reply_view(None);
                scene.focus_notification(None);
            }
            reply.prune(&scene);
            let stale = reply.active.as_ref().is_some_and(|target| {
                scene
                    .overlay
                    .notification_reply
                    .as_ref()
                    .is_none_or(|view| view.publication_id != target.publication_id)
            });
            if stale {
                reply.active = None;
                self.state.answer_keyboard.focus = None;
            }
            let suspended = self
                .state
                .safe_mode_atomic
                .load(std::sync::atomic::Ordering::Acquire);
            let refresh = std::mem::take(&mut reply.refresh_view);
            if let Some((region, editor)) = reply.editor(suspended)
                && refresh
            {
                scene.set_notification_reply_view(Some(editor.view(region)));
            }
            (stale, scene.take_notification_wait_notifiers())
        };
        for wake in waits {
            wake.notify();
        }
        if restore {
            super::lifecycle::restore_foreground();
        }
    }

    pub(super) fn route_notification_reply_key(
        &mut self,
        raw: &RawKeyDownEvent,
    ) -> Result<bool, ()> {
        use super::keyboard::keyboard_key_identity;
        use super::lifecycle::{INTERACTION_LOCK_BUDGET, spin_acquire};
        let reply = &mut self.state.notification_reply;
        let suspended = self
            .state
            .safe_mode_atomic
            .load(std::sync::atomic::Ordering::Acquire);
        let Some((target, editor)) = reply.editor(suspended) else {
            return Ok(false);
        };
        let (exit, wake, wait_wakes) = {
            let state = spin_acquire(&self.state.shared_state, INTERACTION_LOCK_BUDGET).ok_or(())?;
            let mut scene = spin_acquire(&state.scene, INTERACTION_LOCK_BUDGET).ok_or(())?;
            let mut wake = None;
            let mut exit = false;
            if !suspended {
                match editor.key(raw) {
                    ReplyKey::Edit => {
                        scene.set_notification_reply_view(Some(editor.view(target)));
                    }
                    ReplyKey::Collapse => {
                        scene.set_notification_reply_view(None);
                        exit = true;
                    }
                    ReplyKey::Submit => {
                        let region = scene
                            .overlay
                            .zone_hit_regions
                            .iter()
                            .find(|live| {
                                live.interaction_id == target.interaction_id
                                    && live.publication_id == target.publication_id
                                    && live.publisher_namespace == target.publisher_namespace
                            })
                            .cloned();
                        if let Some(region) = region {
                            let exit_ms = super::lifecycle::notification_exit_ms(
                                &scene,
                                &region.zone_name,
                                &self.state.global_tokens,
                            );
                            wake = scene.submit_notification_reply(
                                &region,
                                editor.text().to_owned(),
                                exit_ms,
                            );
                            if wake.is_some() {
                                reply.drafts.remove(&region.publication_id);
                                exit = true;
                            }
                        }
                    }
                }
            }
            if exit {
                scene.focus_notification(None);
            }
            (exit, wake, scene.take_notification_wait_notifiers())
        };
        reply
            .keydowns
            .insert(keyboard_key_identity(&raw.key_code, &raw.key));
        if exit {
            reply.active = None;
            self.state.answer_keyboard.focus = None;
            self.state.answer_keyboard.invalidate_visit();
            self.return_focus_after_key(raw);
        }
        if let Some(wake) = wake {
            wake.notify();
        }
        for wake in wait_wakes {
            wake.notify();
        }
        Ok(true)
    }

    pub(super) fn route_notification_reply_text(
        &mut self,
        raw: &RawCharacterEvent,
    ) -> Result<bool, ()> {
        use super::lifecycle::{INTERACTION_LOCK_BUDGET, spin_acquire};
        let reply = &mut self.state.notification_reply;
        if reply.ignore_character_at == Some(raw.timestamp_mono_us) {
            reply.ignore_character_at = None;
            return Ok(true);
        }
        let suspended = self
            .state
            .safe_mode_atomic
            .load(std::sync::atomic::Ordering::Acquire);
        let Some((target, editor)) = reply.editor(suspended) else {
            return Ok(false);
        };
        if suspended {
            return Ok(true);
        }
        let waits = {
            let state = spin_acquire(&self.state.shared_state, INTERACTION_LOCK_BUDGET).ok_or(())?;
            let mut scene = spin_acquire(&state.scene, INTERACTION_LOCK_BUDGET).ok_or(())?;
            editor.insert(&raw.character);
            scene.set_notification_reply_view(Some(editor.view(target)));
            scene.take_notification_wait_notifiers()
        };
        for wake in waits {
            wake.notify();
        }
        Ok(true)
    }
}

impl ReplyDraft {
    fn view(&self, region: &ZoneHitRegion) -> NotificationReplyView {
        let ZoneInteractionKind::Action { index } = region.kind else {
            unreachable!("reply editor always has an exact action")
        };
        NotificationReplyView {
            publication_id: region.publication_id,
            action_index: index,
            text: self.draft.text().to_owned(),
            cursor_byte: self.draft.cursor(),
            selection_anchor: self.draft.selection_anchor(),
            at_capacity: self.hit_limit || self.draft.text().len() == self.draft.cap(),
        }
    }

    fn text(&self) -> &str {
        self.draft.text()
    }

    fn insert(&mut self, text: &str) {
        if text.chars().take(2).count() == 1
            && !text.contains(['\r', '\n', '\u{0085}', '\u{2028}', '\u{2029}'])
        {
            let edited = self.draft.insert(text);
            self.update_limit(edited);
        } else {
            // A paste or committed string remains one visual line. Keep words
            // separated when the source contained line breaks.
            let text = text
                .replace("\r\n", " ")
                .replace(['\r', '\n', '\u{0085}', '\u{2028}', '\u{2029}'], " ");
            let edited = self.draft.paste(&text);
            self.update_limit(edited);
        }
    }

    fn key(&mut self, raw: &RawKeyDownEvent) -> ReplyKey {
        let mods = raw.modifiers;
        let shortcut = (mods.ctrl || mods.meta) && !mods.alt;
        let plain = !mods.ctrl && !mods.meta && !mods.alt;
        let edited = match raw.key_code.as_str() {
            "Escape" if plain && !raw.repeat => {
                if !self.draft.has_selection() {
                    return ReplyKey::Collapse;
                }
                let cursor = self.draft.cursor();
                self.draft.set_pointer_selection(cursor, cursor)
            }
            "Enter" | "NumpadEnter" if plain && !mods.shift && !raw.repeat => {
                return ReplyKey::Submit;
            }
            "KeyA" if shortcut => self.draft.select_all(),
            "Backspace" if plain => self.draft.backspace(),
            "Delete" if plain => self.draft.delete_forward(),
            "ArrowLeft" if plain && mods.shift => self.draft.select_left(),
            "ArrowLeft" if plain => self.draft.move_left(),
            "ArrowRight" if plain && mods.shift => self.draft.select_right(),
            "ArrowRight" if plain => self.draft.move_right(),
            "Home" if plain && mods.shift => self.draft.select_to_start(),
            "Home" if plain => self.draft.move_to_start(),
            "End" if plain && mods.shift => self.draft.select_to_end(),
            "End" if plain => self.draft.move_to_end(),
            _ => EditOutcome::Unchanged,
        };
        self.update_limit(edited);
        ReplyKey::Edit
    }

    fn update_limit(&mut self, outcome: EditOutcome) {
        match outcome {
            EditOutcome::AtCapacity => self.hit_limit = true,
            EditOutcome::Mutated => self.hit_limit = false,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_input::keyboard::KeyboardModifiers;

    fn key(code: &str, modifiers: KeyboardModifiers, repeat: bool) -> RawKeyDownEvent {
        RawKeyDownEvent {
            key_code: code.into(),
            key: code.into(),
            modifiers,
            repeat,
            timestamp_mono_us: tze_hud_scene::MonoUs(0),
        }
    }

    #[test]
    fn escape_clears_selection_then_collapses_without_changing_text() {
        let mut reply = ReplyDraft::default();
        reply.insert("use the staging DB");
        let all = key(
            "KeyA",
            KeyboardModifiers {
                ctrl: true,
                ..Default::default()
            },
            false,
        );
        assert!(matches!(reply.key(&all), ReplyKey::Edit));
        assert!(reply.draft.has_selection());
        let escape = key("Escape", Default::default(), false);
        assert!(matches!(reply.key(&escape), ReplyKey::Edit));
        assert!(!reply.draft.has_selection());
        let repeated = key("Escape", Default::default(), true);
        assert!(matches!(reply.key(&repeated), ReplyKey::Edit));
        assert!(matches!(reply.key(&escape), ReplyKey::Collapse));
        assert_eq!(reply.text(), "use the staging DB");
    }

    #[test]
    fn paste_and_committed_text_share_the_single_line_utf8_cap() {
        let mut reply = ReplyDraft::default();
        reply.insert("東京\r\nstaging");
        assert_eq!(reply.text(), "東京 staging");
        reply.insert(&"é".repeat(200));
        assert!(reply.hit_limit);
        assert!(reply.text().len() <= NOTIFICATION_REPLY_MAX_BYTES);
        assert!(reply.text().len() >= NOTIFICATION_REPLY_MAX_BYTES - 1);
        assert!(reply.text().ends_with('é'));
    }
}
