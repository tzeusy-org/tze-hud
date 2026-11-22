//! Fresh-card press guard: a card an agent puts under a resting pointer
//! ignores pointer presses for `notification.press_guard.ms`; deliberate
//! input (a pointer that moved there, the keyboard, the owner's own
//! re-layout) is never delayed.

use super::*;
use tze_hud_scene::{NotificationAction, ZoneContent};

const ZONE: &str = "notification-area";

type Clock = Arc<tze_hud_scene::TestClock>;

/// The Approve / Reject / Wait / Details card's content.
fn content(harness: &HeadlessEventLoopHarness) -> ZoneContent {
    harness.scene().zone_registry.active_for_zone(ZONE)[0]
        .content
        .clone()
}

fn publish(harness: &HeadlessEventLoopHarness, content: ZoneContent, ns: &str, key: Option<&str>) {
    harness
        .scene()
        .publish_to_zone(ZONE, content, ns, key.map(Into::into), None, None)
        .unwrap();
}

fn rest(harness: &mut HeadlessEventLoopHarness, at: (f32, f32)) {
    (harness.app.state.cursor_x, harness.app.state.cursor_y) = at;
}

fn press(harness: &mut HeadlessEventLoopHarness, at: (f32, f32)) {
    rest(harness, at);
    harness.app.enqueue_pointer_event(PointerEventKind::Down);
    harness.app.enqueue_pointer_event(PointerEventKind::Up);
    harness.drain();
}

fn guarded(harness: &HeadlessEventLoopHarness) -> bool {
    harness.scene().overlay.notification_press_guard.is_some()
}

fn interaction_at(harness: &HeadlessEventLoopHarness, (x, y): (f32, f32)) -> String {
    harness
        .scene()
        .overlay
        .zone_hit_regions
        .iter()
        .find(|r| r.bounds.contains_point(x, y))
        .unwrap()
        .interaction_id
        .clone()
}

/// The card arrives, keyed by `key`, while the pointer rests where its
/// Approve button draws. Returns the harness, its clock and that point.
fn arrives_under_pointer(key: Option<&str>) -> (HeadlessEventLoopHarness, Clock, (f32, f32)) {
    let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
    let mut harness = approve_reject_notification_harness_on(clock.clone());
    let approve = harness.notification_action_center("approve").unwrap();
    let card = content(&harness);
    harness.scene().clear_zone(ZONE).unwrap();
    rest(&mut harness, approve);
    harness.tick();
    publish(&harness, card, "notif-agent", key);
    harness.tick();
    (harness, clock, approve)
}

fn with_actions(
    mut card: ZoneContent,
    edit: impl FnOnce(&mut Vec<NotificationAction>),
) -> ZoneContent {
    let ZoneContent::Notification(payload) = &mut card else {
        unreachable!()
    };
    edit(&mut payload.actions);
    card
}

/// R1.1 S1/S6, AC3: an arriving card drops a press 150 ms later (no answer,
/// no hover, the arrow); one wake ends the guard under the still pointer,
/// bringing hover and the hand back, and the next press answers.
#[test]
fn a_card_arriving_under_a_resting_pointer_drops_the_press() {
    let (mut harness, clock, approve) = arrives_under_pointer(None);
    assert!(guarded(&harness));
    let id = interaction_at(&harness, approve);
    clock.advance(150);
    press(&mut harness, approve);
    assert!(take_action_ids(&harness).is_empty());
    assert_eq!(harness.zone_publication_count(ZONE), 1, "the card stays");
    {
        let scene = harness.scene();
        assert!(
            scene.overlay.notification_choices.is_empty(),
            "chosen stays null"
        );
        assert!(
            scene
                .overlay
                .chrome_states
                .get(&id)
                .is_none_or(|s| !s.hovered && !s.pressed),
            "no hover or pressed fill while guarded"
        );
    }
    assert!(
        !harness
            .app
            .state
            .pressable_regions
            .iter()
            .any(|r| r.contains(approve.0, approve.1))
    );
    assert!(
        harness.app.state.press_guard.until_us().is_some(),
        "one wake armed"
    );
    // The dropped press restarted the interval: 150 + 400 ms.
    clock.advance(399);
    harness.tick();
    assert!(guarded(&harness));
    clock.advance(1);
    harness.tick();
    assert!(!guarded(&harness));
    assert!(harness.app.state.press_guard.until_us().is_none());
    assert!(
        harness.scene().overlay.chrome_states[&id].hovered,
        "hover without motion"
    );
    assert!(
        harness
            .app
            .state
            .pressable_regions
            .iter()
            .any(|r| r.contains(approve.0, approve.1))
    );
    press(&mut harness, approve);
    assert_eq!(take_action_ids(&harness), ["approve"]);
}

/// R1.1 S2: the × and a url action are guarded the same way.
#[test]
fn close_and_link_presses_are_dropped_too() {
    for target in ["close", "link"] {
        let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
        let mut harness = approve_reject_notification_harness_on(clock.clone());
        let card = with_actions(content(&harness), |actions| {
            actions[1] = NotificationAction {
                label: "Docs".into(),
                callback_id: None,
                url: Some("https://example.com/docs".into()),
                role: None,
            };
        });
        harness.scene().clear_zone(ZONE).unwrap();
        publish(&harness, card.clone(), "notif-agent", None);
        harness.tick();
        let at = match target {
            "close" => dismiss_center(&harness),
            _ => harness
                .notification_link_center("https://example.com/docs")
                .unwrap(),
        };
        harness.scene().clear_zone(ZONE).unwrap();
        rest(&mut harness, at);
        harness.tick();
        publish(&harness, card, "notif-agent", None);
        harness.tick();
        clock.advance(150);
        let launches = harness.app.state.notification_link_opener.launches.clone();
        press(&mut harness, at);
        assert_eq!(harness.zone_publication_count(ZONE), 1, "{target}");
        assert_eq!(
            launches.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "{target}"
        );
        assert!(
            harness
                .scene()
                .take_pending_actions("notif-agent")
                .is_empty()
        );
    }
}

/// R1.1 S3: presses every 300 ms keep restarting the interval until 2 s after
/// the guard began; 400 ms without one also ends it.
#[test]
fn dropped_presses_restart_the_guard_up_to_five_intervals() {
    let (mut harness, clock, approve) = arrives_under_pointer(None);
    for _ in 0..6 {
        clock.advance(300);
        press(&mut harness, approve);
        assert!(take_action_ids(&harness).is_empty());
    }
    // 1800 ms in: the last press asked for 2200 but the bound is 2000.
    clock.advance(200);
    press(&mut harness, approve);
    assert_eq!(take_action_ids(&harness), ["approve"]);

    let (mut harness, clock, approve) = arrives_under_pointer(None);
    clock.advance(100);
    press(&mut harness, approve);
    clock.advance(400);
    press(&mut harness, approve);
    assert_eq!(
        take_action_ids(&harness),
        ["approve"],
        "400 ms without a press"
    );
}

/// R1.1 S4: another agent's publish or expiry shifts a card's button under
/// the resting pointer; that card is guarded.
#[test]
fn a_stack_shift_under_a_resting_pointer_is_guarded() {
    for cause in ["publish", "expiry"] {
        let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
        let mut harness = approve_reject_notification_harness_on(clock.clone());
        let card = content(&harness);
        let other = with_actions(card.clone(), |actions| actions.truncate(1));
        let alone = harness.notification_action_center("approve").unwrap();
        publish(&harness, other.clone(), "another-agent", None);
        harness.tick();
        let stacked = harness.notification_action_center("approve").unwrap();
        assert_ne!(alone, stacked, "the other card shifts this one");
        let before = if cause == "publish" {
            harness.scene().clear_zone(ZONE).unwrap();
            publish(&harness, card, "notif-agent", None);
            stacked
        } else {
            alone
        };
        rest(&mut harness, before);
        harness.tick();
        assert!(!guarded(&harness), "{cause}: nothing has moved yet");
        if cause == "publish" {
            publish(&harness, other, "another-agent", None);
        } else {
            let mut scene = harness.scene();
            for record in scene.zone_registry.active_publishes.get_mut(ZONE).unwrap() {
                if record.publisher_namespace == "another-agent" {
                    record.expires_at_wall_us = Some(0);
                }
            }
            scene.drain_expired_zone_publications();
        }
        harness.tick();
        assert!(guarded(&harness), "{cause}");
        clock.advance(150);
        press(&mut harness, before);
        assert!(take_action_ids(&harness).is_empty(), "{cause}");
    }
}

/// R1.1 S5 / R1.2 S4: a keyed replacement with different action labels is
/// guarded; one that changes only the body is not.
#[test]
fn keyed_replacement_is_guarded_only_when_its_actions_change() {
    let (mut harness, clock, approve) = arrives_under_pointer(Some("q"));
    clock.advance(500);
    harness.tick();
    assert!(!guarded(&harness));
    let body_only = {
        let mut card = content(&harness);
        let ZoneContent::Notification(payload) = &mut card else {
            unreachable!()
        };
        payload.text = "A new body".into();
        card
    };
    publish(&harness, body_only, "notif-agent", Some("q"));
    harness.tick();
    assert!(!guarded(&harness), "same labels, new body");
    clock.advance(100);
    press(&mut harness, approve);
    assert_eq!(take_action_ids(&harness), ["approve"]);

    let (mut harness, clock, approve) = arrives_under_pointer(Some("q"));
    clock.advance(500);
    harness.tick();
    let relabelled = with_actions(content(&harness), |actions| {
        actions[0].label = "Ship".into()
    });
    publish(&harness, relabelled, "notif-agent", Some("q"));
    harness.tick();
    assert!(guarded(&harness), "different labels");
    clock.advance(150);
    press(&mut harness, approve);
    assert!(take_action_ids(&harness).is_empty());
}

/// R1.2 S1/S5: a pointer that moves onto a 100 ms old card, or leaves a
/// guarded card and returns, presses at once.
#[test]
fn a_pointer_that_moved_there_is_never_guarded() {
    let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
    let mut harness = approve_reject_notification_harness_on(clock.clone());
    clock.advance(100);
    let approve = harness.notification_action_center("approve").unwrap();
    press(&mut harness, approve);
    assert_eq!(take_action_ids(&harness), ["approve"]);

    let (mut harness, _clock, approve) = arrives_under_pointer(None);
    assert!(guarded(&harness));
    rest(&mut harness, (1.0, 1.0));
    harness.app.enqueue_pointer_event(PointerEventKind::Move);
    assert!(!guarded(&harness), "leaving ends it");
    rest(&mut harness, approve);
    harness.app.enqueue_pointer_event(PointerEventKind::Move);
    harness.tick();
    assert!(!guarded(&harness), "returning starts none");
    press(&mut harness, approve);
    assert_eq!(take_action_ids(&harness), ["approve"]);
}

/// R1.2 S2: the answer chord with a digit, or Tab and Enter, answers a
/// guarded card at once.
#[test]
fn keyboard_answers_are_never_guarded() {
    for path in ["chord", "tab"] {
        let (mut harness, _clock, _) = arrives_under_pointer(None);
        assert!(guarded(&harness));
        if path == "chord" {
            assert!(harness.app.focus_newest_question(true).unwrap());
            harness.enqueue(key_down("Digit2", "2", 1));
            harness.enqueue(character("2", 2));
            harness.enqueue(key_up("Digit2", "2", 3));
            harness.drain();
            assert_eq!(take_action_ids(&harness), ["reject"], "{path}");
        } else {
            let id = interaction_at(
                &harness,
                harness.notification_action_center("approve").unwrap(),
            );
            harness.scene().focus_notification(Some(id));
            harness.enqueue(key_down("Enter", "Enter", 1));
            harness.enqueue(key_up("Enter", "Enter", 2));
            harness.drain();
            assert_eq!(take_action_ids(&harness), ["approve"], "{path}");
        }
    }
}

/// R1.2 S3: dismissing a card with × re-lays the stack out; the next card's ×
/// under the resting pointer presses 200 ms later.
#[test]
fn the_owners_own_relayout_starts_no_guard() {
    let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
    let mut harness = approve_reject_notification_harness_on(clock.clone());
    let card = content(&harness);
    publish(&harness, card, "another-agent", None);
    harness.tick();
    let close = dismiss_center(&harness);
    let first = interaction_at(&harness, close);
    press(&mut harness, close);
    harness.tick();
    assert_eq!(harness.zone_publication_count(ZONE), 1);
    let next = interaction_at(&harness, close);
    assert_ne!(first, next, "the next card's × moved under the pointer");
    assert!(!guarded(&harness));
    clock.advance(200);
    press(&mut harness, close);
    harness.tick();
    assert_eq!(harness.zone_publication_count(ZONE), 0);
}

/// R1.3 S1-S4: 0 turns it off; Windows animations off leave the interval;
/// a guarded card that leaves arms no wake; safe mode still refuses.
#[test]
fn edges_zero_reduced_motion_leaving_and_safe_mode() {
    // S1
    let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
    let mut harness = approve_reject_notification_harness_on(clock.clone());
    harness
        .app
        .state
        .global_tokens
        .insert("notification.press_guard.ms".into(), "0".into());
    let approve = harness.notification_action_center("approve").unwrap();
    let card = content(&harness);
    harness.scene().clear_zone(ZONE).unwrap();
    rest(&mut harness, approve);
    harness.tick();
    publish(&harness, card, "notif-agent", None);
    harness.tick();
    assert!(!guarded(&harness));
    press(&mut harness, approve);
    assert_eq!(take_action_ids(&harness), ["approve"], "0 turns it off");

    // S2: the OS motion layer zeroes motion, not the guard.
    let (mut harness, clock, approve) = arrives_under_pointer(None);
    let mut tokens = harness.app.state.global_tokens.clone();
    tze_hud_config::tokens::apply_os_motion_layer(&mut tokens, &Default::default(), false);
    harness.app.state.global_tokens = tokens;
    clock.advance(399);
    harness.tick();
    assert!(guarded(&harness), "still 400 ms");
    clock.advance(1);
    harness.tick();
    assert!(!guarded(&harness));
    let _ = approve;

    // S3
    let (mut harness, _clock, _) = arrives_under_pointer(None);
    assert!(harness.app.state.press_guard.until_us().is_some());
    harness.scene().clear_zone(ZONE).unwrap();
    harness.tick();
    assert!(!guarded(&harness));
    assert!(
        harness.app.state.press_guard.until_us().is_none(),
        "no wake"
    );

    // S4
    let (mut harness, clock, approve) = arrives_under_pointer(None);
    harness
        .app
        .state
        .safe_mode_atomic
        .store(true, std::sync::atomic::Ordering::SeqCst);
    clock.advance(1_000);
    harness.tick();
    press(&mut harness, approve);
    assert!(
        take_action_ids(&harness).is_empty(),
        "safe mode refuses as today"
    );
}

/// A guarded press on a destructive action starts no hold either.
#[test]
fn a_guarded_destructive_press_starts_no_hold() {
    let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
    let mut harness = approve_reject_notification_harness_on(clock.clone());
    let approve = harness.notification_action_center("approve").unwrap();
    let card = with_actions(content(&harness), |actions| {
        actions[0].role = Some(tze_hud_scene::ActionRole::Destructive);
    });
    harness.scene().clear_zone(ZONE).unwrap();
    rest(&mut harness, approve);
    harness.tick();
    publish(&harness, card, "notif-agent", None);
    harness.tick();
    assert!(guarded(&harness));
    clock.advance(150);
    harness.app.enqueue_pointer_event(PointerEventKind::Down);
    assert!(harness.scene().overlay.notification_hold.is_none());
    clock.advance(1_000);
    harness.tick();
    harness.app.enqueue_pointer_event(PointerEventKind::Up);
    harness.drain();
    assert!(take_action_ids(&harness).is_empty());
}
