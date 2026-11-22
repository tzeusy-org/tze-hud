//! Fresh-card press guard: a card an agent puts under a resting pointer
//! ignores pointer presses on its controls for `notification.press_guard.ms`
//! (and until its enter motion ends), so a click meant for the app beneath
//! cannot answer it. Keyboard answers never pass through here.
//!
//! A control "came to the pointer" when the pointer is over it now but was not
//! over that same control (card identity, action labels and control) at the
//! previous observation. A pointer that moved onto an unchanged control, a
//! same-key update that keeps its labels, and the re-layout after the owner's
//! own answer or × start no guard.

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::{Rect, SceneId, ZoneContent};

/// Restarts from dropped presses never stretch a guard past this many
/// intervals from when it began.
const MAX_INTERVALS: u64 = 5;

/// One card control as the pointer meets it.
#[derive(Clone, Debug, PartialEq)]
struct ControlKey {
    zone: String,
    publisher: String,
    /// The merge key, else the publication id: a keyed replacement is the
    /// same card.
    card: String,
    labels: Vec<String>,
    /// `None` is the ×.
    action: Option<usize>,
}

struct Control {
    key: ControlKey,
    rect: Rect,
    card_bounds: Rect,
    publication_id: SceneId,
}

#[derive(Clone, Copy, Debug)]
struct Guarded {
    publication_id: SceneId,
    began_us: u64,
    until_us: u64,
}

/// Runtime-owned guard state; the scene mirrors the guarded card for drawing
/// and `/admin/status`.
#[derive(Default)]
pub(super) struct PressGuard {
    seen: Vec<(ControlKey, Rect)>,
    guarded: Option<Guarded>,
}

/// The drawn card controls, or `None` while the last frame's geometry names a
/// card that has since left (a frame is due; observe that one instead).
fn controls(scene: &SceneGraph) -> Option<Vec<Control>> {
    let mut out = Vec::new();
    for card in &scene.overlay.notification_geometry {
        let record = scene
            .zone_registry
            .active_publishes
            .get(&card.zone_name)
            .and_then(|records| {
                records
                    .iter()
                    .find(|r| r.publication_id == card.publication_id)
            })?;
        let ZoneContent::Notification(payload) = &record.content else {
            continue;
        };
        let key = |action| ControlKey {
            zone: card.zone_name.clone(),
            publisher: record.publisher_namespace.clone(),
            card: record
                .merge_key
                .clone()
                .filter(|k| !k.is_empty())
                .unwrap_or_else(|| card.publication_id.to_string()),
            labels: payload.actions.iter().map(|a| a.label.clone()).collect(),
            action,
        };
        let rects = std::iter::once((None, card.close))
            .chain(card.actions.iter().enumerate().map(|(i, r)| (Some(i), *r)));
        for (action, rect) in rects {
            out.push(Control {
                key: key(action),
                rect,
                card_bounds: card.bounds,
                publication_id: card.publication_id,
            });
        }
    }
    Some(out)
}

impl PressGuard {
    /// Observe the drawn cards with the pointer at `pointer` (`None` when it
    /// left the window): end a guard that expired, whose card left or that
    /// the pointer moved off; start one when an agent-caused change put a
    /// control under the pointer. Mirrors the result into the scene and keeps
    /// the guarded card's controls free of hover and press feedback. Returns
    /// the control under the pointer when a guard ended there, so the caller
    /// can show its hover without pointer motion.
    pub(super) fn observe(
        &mut self,
        scene: &mut SceneGraph,
        pointer: Option<(f32, f32)>,
        tokens: &std::collections::HashMap<String, String>,
    ) -> Option<SceneId> {
        let now = scene.now_monotonic_us();
        let guard_ms = u64::from(tze_hud_config::tokens::notification_press_guard_ms(tokens));
        let Some(current) = controls(scene) else {
            self.mirror(scene);
            return None;
        };
        let mut ended = None;
        if let Some(g) = self.guarded {
            let on_card = pointer.is_some_and(|(x, y)| {
                current.iter().any(|c| {
                    c.publication_id == g.publication_id && c.card_bounds.contains_point(x, y)
                })
            });
            if !on_card || now >= g.until_us || guard_ms == 0 {
                self.guarded = None;
                if on_card {
                    ended = Some(g.publication_id);
                }
            }
        }
        // The owner's own answer or × re-lays the stack out once its card
        // has left; that re-layout starts no guard.
        let owner_relayout = scene
            .overlay
            .notification_owner_activated
            .is_some_and(|id| !current.iter().any(|c| c.publication_id == id));
        if owner_relayout {
            scene.overlay.notification_owner_activated = None;
        }
        if self.guarded.is_none()
            && guard_ms > 0
            && !owner_relayout
            && let Some((x, y)) = pointer
            && let Some(hit) = current.iter().find(|c| c.rect.contains_point(x, y))
            && !self
                .seen
                .iter()
                .any(|(key, rect)| *key == hit.key && rect.contains_point(x, y))
        {
            let enter_ms = u64::from(tze_hud_config::tokens::resolve_motion_duration_ms(
                tokens,
                "motion.enter.ms",
            ));
            self.guarded = Some(Guarded {
                publication_id: hit.publication_id,
                began_us: now,
                until_us: now.saturating_add(guard_ms.max(enter_ms) * 1_000),
            });
        }
        self.seen = current.into_iter().map(|c| (c.key, c.rect)).collect();
        self.mirror(scene);
        ended
    }

    /// Whether a press on `publication_id` must be dropped. A dropped press
    /// restarts the interval, bounded at [`MAX_INTERVALS`] from the start.
    pub(super) fn drop_press(
        &mut self,
        scene: &SceneGraph,
        publication_id: SceneId,
        tokens: &std::collections::HashMap<String, String>,
    ) -> bool {
        let Some(g) = self.guarded.as_mut() else {
            return false;
        };
        if g.publication_id != publication_id {
            return false;
        }
        let interval =
            u64::from(tze_hud_config::tokens::notification_press_guard_ms(tokens)) * 1_000;
        let bound = g.began_us.saturating_add(interval * MAX_INTERVALS);
        g.until_us = g
            .until_us
            .max(scene.now_monotonic_us().saturating_add(interval).min(bound));
        true
    }

    /// When the current guard ends (scene monotonic µs), for the wake.
    pub(super) fn until_us(&self) -> Option<u64> {
        self.guarded.map(|g| g.until_us)
    }

    fn mirror(&self, scene: &mut SceneGraph) {
        let id = self.guarded.map(|g| g.publication_id);
        if scene.overlay.notification_press_guard != id {
            scene.overlay.notification_press_guard = id;
            scene.bump_geometry_epoch();
        }
        if let Some(id) = id {
            let controls: Vec<String> = scene
                .overlay
                .zone_hit_regions
                .iter()
                .filter(|r| r.publication_id == id)
                .map(|r| r.interaction_id.clone())
                .collect();
            for control in controls {
                scene.set_chrome_hovered(&control, false);
                scene.set_chrome_pressed(&control, false);
            }
        }
    }
}

impl super::WinitApp {
    /// Observe the press guard at the current pointer: start one for a card
    /// that came under a resting pointer, end one that expired, then refresh
    /// the pressable regions and cursor. Returns whether the scene changed.
    pub(super) fn settle_press_guard(&mut self) -> bool {
        let changed = {
            let Ok(shared) = self.state.shared_state.try_lock() else {
                return false;
            };
            let Ok(mut scene) = shared.scene.try_lock() else {
                return false;
            };
            let before = (scene.geometry_epoch, scene.overlay.notification_press_guard);
            let pointer = (!self.state.cursor_left_window)
                .then_some((self.state.cursor_x, self.state.cursor_y));
            let ended =
                self.state
                    .press_guard
                    .observe(&mut scene, pointer, &self.state.global_tokens);
            show_hover_after_guard(
                &mut self.state.input_processor,
                &mut scene,
                (self.state.cursor_x, self.state.cursor_y),
                ended,
            );
            before != (scene.geometry_epoch, scene.overlay.notification_press_guard)
        };
        if changed {
            self.refresh_widget_hover_tracking();
            self.update_portal_cursor_icon();
        }
        changed
    }
}

/// A guard on `ended` just ended under the pointer at `(x, y)`: hover the
/// control it is over, without waiting for pointer motion.
pub(super) fn show_hover_after_guard(
    input: &mut tze_hud_input::InputProcessor,
    scene: &mut SceneGraph,
    (x, y): (f32, f32),
    ended: Option<SceneId>,
) {
    let Some(id) = ended else {
        return;
    };
    let control = scene
        .overlay
        .zone_hit_regions
        .iter()
        .find(|r| r.publication_id == id && r.bounds.contains_point(x, y))
        .map(|r| r.interaction_id.clone());
    if control.is_some() {
        input.hover_chrome(scene, control);
    }
}
