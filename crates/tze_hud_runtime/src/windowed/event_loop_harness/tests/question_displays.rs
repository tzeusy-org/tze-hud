//! Questions on two displays (R12.5): the answer chord takes keys to the
//! newest question's window, and Tab visits cards in the one global stack
//! order, moving keys to the window of the display each card is on.

use super::*;
use tze_hud_compositor::DisplayRect;
use tze_hud_scene::{NotificationAction, NotificationPayload, Rect, ZoneContent};

const ZONE: &str = "notification-area";

/// Places every new question on one display.
#[derive(Debug)]
struct On(&'static str);

impl tze_hud_scene::QuestionPlacer for On {
    fn place(&self) -> Option<String> {
        Some(self.0.into())
    }
}

/// The Approve/Reject card on DISPLAY1 (the primary), then a newer
/// one-button question placed on DISPLAY2.
fn two_displays() -> (HeadlessEventLoopHarness, SceneId, SceneId) {
    let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
    let mut harness = approve_reject_notification_harness_on(clock.clone());
    let display = |name: &str, x: f32, primary| {
        (
            DisplayRect {
                name: name.into(),
                rect: Rect::new(x, 0.0, 1920.0, 1080.0),
                primary,
            },
            format!("{}-key", name.to_lowercase()),
        )
    };
    harness.set_display_topology(vec![
        display("DISPLAY1", 0.0, true),
        display("DISPLAY2", 1920.0, false),
    ]);
    let older = harness.scene().zone_registry.active_for_zone(ZONE)[0].publication_id;
    // Give the DISPLAY2 card a later publication timestamp.
    clock.advance(1);
    {
        let mut scene = harness.scene();
        scene.overlay.question_placer = Some(Arc::new(On("DISPLAY2")));
        scene
            .publish_to_zone(
                ZONE,
                ZoneContent::Notification(NotificationPayload {
                    text: "Deploy?".into(),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: vec![NotificationAction {
                        label: "Yes".into(),
                        callback_id: Some("yes".into()),
                        url: None,
                        role: None,
                    }],
                }),
                "notif-agent",
                Some("deploy".into()),
                None,
                None,
            )
            .unwrap();
    }
    assert!(harness.tick());
    let newer = harness
        .scene()
        .zone_registry
        .active_for_zone(ZONE)
        .iter()
        .find(|r| r.display.as_deref() == Some("DISPLAY2"))
        .unwrap()
        .publication_id;
    (harness, older, newer)
}

fn press(harness: &mut HeadlessEventLoopHarness, code: &str, key: &str, shift: bool) {
    let mut down = key_down(code, key, 1_000);
    if let PendingKeyboardEvent::KeyDown(raw) = &mut down {
        raw.modifiers.shift = shift;
    }
    harness.enqueue(down);
    harness.enqueue(key_up(code, key, 1_100));
    harness.drain();
}

/// The focused card control's publication and the window holding keys.
fn focused(harness: &HeadlessEventLoopHarness) -> (Option<SceneId>, Option<usize>) {
    let scene = harness.scene();
    let publication = scene.overlay.notification_focus.as_ref().and_then(|id| {
        scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|region| &region.interaction_id == id)
            .map(|region| region.publication_id)
    });
    (publication, harness.app.state.keyboard_window)
}

/// R12.5 S1: the chord focuses the newest question, on DISPLAY2, in
/// DISPLAY2's window.
#[test]
fn the_answer_chord_takes_keys_to_the_newest_questions_window() {
    let (mut harness, _, newer) = two_displays();
    {
        let scene = harness.scene();
        let card = scene
            .overlay
            .notification_geometry
            .iter()
            .find(|card| card.publication_id == newer)
            .unwrap();
        assert!(
            card.bounds.x >= 1920.0,
            "drawn on DISPLAY2: {:?}",
            card.bounds
        );
    }
    harness
        .app
        .state
        .answer_keyboard
        .requested
        .store(true, std::sync::atomic::Ordering::Release);
    harness.drain();
    assert_eq!(
        harness
            .app
            .state
            .answer_keyboard
            .focus
            .unwrap()
            .publication_id,
        newer
    );
    assert_eq!(focused(&harness), (Some(newer), Some(0)));
}

/// R12.5 S2: Tab walks the global stack order (newest question first),
/// moving keys between windows as it crosses displays; Shift+Tab reverses.
#[test]
fn tab_visits_cards_in_stack_order_across_windows() {
    let (mut harness, older, newer) = two_displays();
    let mut visits = Vec::new();
    // Newer: close + Yes; older: close + four actions.
    for _ in 0..7 {
        press(&mut harness, "Tab", "Tab", false);
        visits.push(focused(&harness));
    }
    assert_eq!(
        visits,
        [
            (Some(newer), Some(0)),
            (Some(newer), Some(0)),
            (Some(older), None),
            (Some(older), None),
            (Some(older), None),
            (Some(older), None),
            (Some(older), None),
        ]
    );
    // Back across the display boundary.
    for _ in 0..5 {
        press(&mut harness, "Tab", "Tab", true);
    }
    assert_eq!(focused(&harness), (Some(newer), Some(0)));
}
