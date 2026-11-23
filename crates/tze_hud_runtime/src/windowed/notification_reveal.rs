//! Owner-local hover and keyboard interaction for full notification text.

use std::time::{Duration, Instant};

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::graph::overlay::{
    NotificationRevealField, NotificationRevealPanel, NotificationRevealRegion,
    NotificationRevealRequest, NotificationRevealTarget,
};
use tze_hud_scene::types::{Rect, ZoneHitRegion, ZoneInteractionKind};

use super::WinitApp;

#[derive(Default)]
pub(super) struct RevealState {
    hover: Option<(NotificationRevealTarget, Instant)>,
    dismissed: Option<NotificationRevealTarget>,
    pending: Option<Instant>,
    active: bool,
}

impl RevealState {
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.pending
    }

    pub(super) fn dismiss(&mut self, scene: &mut SceneGraph) -> bool {
        let Some(request) = scene.overlay.notification_reveal.take() else {
            return false;
        };
        self.dismissed = Some(request.target);
        self.pending = None;
        self.active = false;
        scene.bump_geometry_epoch();
        true
    }
}

fn pointer_target(
    regions: &[NotificationRevealRegion],
    panel: Option<NotificationRevealPanel>,
    point: (f32, f32),
) -> Option<NotificationRevealTarget> {
    let (x, y) = point;
    if let Some(panel) = panel
        && panel.bounds.contains_point(x, y)
    {
        return Some(panel.target);
    }
    regions
        .iter()
        .rev()
        .find(|region| region.bounds.contains_point(x, y))
        .map(|region| region.target)
        .or_else(|| {
            panel
                .filter(|panel| panel.bridge.contains_point(x, y))
                .map(|panel| panel.target)
        })
}

pub(super) fn scroll_panel(scene: &mut SceneGraph, dy: f32) -> bool {
    let (Some(panel), Some(request)) = (
        scene.overlay.notification_reveal_panel,
        scene.overlay.notification_reveal.as_mut(),
    ) else {
        return false;
    };
    if panel.target != request.target {
        return false;
    }
    let scroll = (request.scroll + dy).clamp(0.0, panel.max_scroll);
    if scroll != request.scroll {
        request.scroll = scroll;
        scene.bump_geometry_epoch();
    }
    true
}

pub(super) struct NotificationFocusStop {
    pub(super) id: String,
    pub(super) bounds: Rect,
    pub(super) region: Option<ZoneHitRegion>,
}

pub(super) fn keyboard_stops(scene: &SceneGraph) -> Vec<NotificationFocusStop> {
    let mut controls: Vec<_> = scene
        .overlay
        .zone_hit_regions
        .iter()
        .filter(|region| {
            matches!(
                region.kind,
                ZoneInteractionKind::Dismiss
                    | ZoneInteractionKind::GoTo
                    | ZoneInteractionKind::Action { .. }
            )
        })
        .collect();
    controls.sort_by_key(|region| region.tab_order);
    let mut stops = Vec::new();
    for control in controls {
        if control.kind == ZoneInteractionKind::Dismiss {
            for text in &scene.overlay.notification_reveal_regions {
                if text.target.publication_id == control.publication_id
                    && matches!(
                        text.target.field,
                        NotificationRevealField::Title | NotificationRevealField::Body
                    )
                {
                    stops.push(NotificationFocusStop {
                        id: text.focus_id.clone(),
                        bounds: text.bounds,
                        region: None,
                    });
                }
            }
        }
        stops.push(NotificationFocusStop {
            id: control.interaction_id.clone(),
            bounds: control.bounds,
            region: Some(control.clone()),
        });
    }
    stops
}

impl WinitApp {
    fn reveal_pointer(&self) -> Option<(f32, f32)> {
        (!self.state.cursor_left_window).then_some((self.state.cursor_x, self.state.cursor_y))
    }

    pub(super) fn notification_reveal_under_cursor(&self) -> Option<NotificationRevealTarget> {
        let point = self.reveal_pointer()?;
        let snapshot = self.state.pipeline.hit_test_snapshot.load();
        pointer_target(
            &snapshot.notification_reveals,
            snapshot.notification_reveal_panel,
            point,
        )
    }

    pub(super) fn notification_reveal_panel_under_cursor(
        &self,
    ) -> Option<NotificationRevealTarget> {
        let (x, y) = self.reveal_pointer()?;
        self.state
            .pipeline
            .hit_test_snapshot
            .load()
            .notification_reveal_panel
            .filter(|panel| panel.bounds.contains_point(x, y))
            .map(|panel| panel.target)
    }

    pub(super) fn settle_notification_reveal(&mut self, now: Instant) -> bool {
        if !self.state.notification_reveal.active
            && self.state.notification_reveal.hover.is_none()
            && self
                .state
                .pipeline
                .hit_test_snapshot
                .load()
                .notification_reveals
                .is_empty()
        {
            return false;
        }
        let point = self.reveal_pointer();
        let enabled = !self
            .state
            .safe_mode_atomic
            .load(std::sync::atomic::Ordering::Acquire)
            && !self
                .state
                .control_panel
                .as_ref()
                .is_some_and(|panel| panel.is_open());
        let Ok(shared) = self.state.shared_state.try_lock() else {
            self.state.notification_reveal.pending = None;
            self.schedule_shared_scene_availability_wake();
            return false;
        };
        let Ok(mut scene) = shared.scene.try_lock() else {
            drop(shared);
            self.state.notification_reveal.pending = None;
            self.schedule_shared_scene_availability_wake();
            return false;
        };
        let regions = &scene.overlay.notification_reveal_regions;
        let current = scene.overlay.notification_reveal;
        let panel = scene
            .overlay
            .notification_reveal_panel
            .filter(|panel| current.is_some_and(|request| request.target == panel.target));
        let keyboard = enabled
            .then(|| {
                regions
                    .iter()
                    .find(|region| {
                        scene.overlay.notification_focus.as_ref() == Some(&region.focus_id)
                    })
                    .map(|region| region.target)
            })
            .flatten();
        let pointer = point
            .filter(|_| enabled && !self.state.left_button_down)
            .and_then(|point| pointer_target(regions, panel, point));
        let state = &mut self.state.notification_reveal;
        if state.hover.map(|(target, _)| target) != pointer {
            let delay = self
                .state
                .global_tokens
                .get("progress.hover.delay_ms")
                .and_then(|value| value.trim().parse::<u32>().ok())
                .unwrap_or(180);
            state.hover = pointer.map(|target| (target, now + Duration::from_millis(delay.into())));
        }
        if state.dismissed != keyboard && state.dismissed != pointer {
            state.dismissed = None;
        }
        let ready_pointer = state
            .hover
            .filter(|&(target, at)| now >= at && state.dismissed != Some(target))
            .map(|(target, _)| target);
        let keyboard = keyboard.filter(|target| state.dismissed != Some(*target));
        let target = keyboard.or(ready_pointer);
        state.pending = state
            .hover
            .filter(|&(target, at)| {
                keyboard.is_none() && state.dismissed != Some(target) && at > now
            })
            .map(|(_, at)| at);
        let next = target.map(|target| {
            current
                .filter(|request| request.target == target)
                .unwrap_or(NotificationRevealRequest {
                    target,
                    scroll: 0.0,
                })
        });
        state.active = next.is_some();
        if current == next {
            return false;
        }
        scene.overlay.notification_reveal = next;
        scene.bump_geometry_epoch();
        true
    }
}
