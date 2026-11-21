//! Destructive actions fire only after a press held for
//! `notification.action.hold.ms`; the primary is the first safe action.

use super::*;
use std::sync::atomic::AtomicBool;
use tze_hud_scene::graph::NotificationOutcome;
use tze_hud_scene::{ActionRole, ZoneContent};

/// Approve / Reject / Wait / Details card on an injected clock, with the
/// actions at `destructive` marked destructive.
fn card(destructive: &[usize]) -> (HeadlessEventLoopHarness, Arc<tze_hud_scene::TestClock>) {
    let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
    let mut harness = approve_reject_notification_harness_on(clock.clone());
    {
        let mut scene = harness.scene();
        let ZoneContent::Notification(payload) = &mut scene
            .zone_registry
            .active_publishes
            .get_mut("notification-area")
            .unwrap()[0]
            .content
        else {
            unreachable!()
        };
        for &index in destructive {
            payload.actions[index].role = Some(ActionRole::Destructive);
        }
    }
    harness.tick();
    (harness, clock)
}

fn point(harness: &HeadlessEventLoopHarness, callback: &str) -> (f32, f32) {
    harness.notification_action_center(callback).unwrap()
}

fn pointer(harness: &mut HeadlessEventLoopHarness, at: (f32, f32), kind: PointerEventKind) {
    (harness.app.state.cursor_x, harness.app.state.cursor_y) = at;
    harness.app.enqueue_pointer_event(kind);
}

fn key(harness: &mut HeadlessEventLoopHarness, code: &str, down: bool, repeat: bool) {
    let name = match code {
        "Digit1" => "1",
        "Digit2" => "2",
        "Space" => " ",
        other => other,
    };
    if down {
        harness.enqueue(PendingKeyboardEvent::KeyDown(RawKeyDownEvent {
            key_code: code.into(),
            key: name.into(),
            modifiers: KeyboardModifiers::NONE,
            repeat,
            timestamp_mono_us: MonoUs(1),
        }));
    } else {
        harness.enqueue(key_up(code, name, 2));
    }
    harness.drain();
    harness.tick();
}

fn holding(harness: &HeadlessEventLoopHarness) -> bool {
    harness.scene().overlay.notification_hold.is_some()
}

fn focus_action(harness: &mut HeadlessEventLoopHarness, index: usize) {
    let mut scene = harness.scene();
    let id = scene
        .overlay
        .zone_hit_regions
        .iter()
        .find(|r| matches!(r.kind, ZoneInteractionKind::Action { index: i } if i == index))
        .unwrap()
        .interaction_id
        .clone();
    scene.focus_notification(Some(id));
}

fn arm_wait(harness: &HeadlessEventLoopHarness) -> SceneId {
    let mut scene = harness.scene();
    let publication = scene.zone_registry.active_for_zone("notification-area")[0].publication_id;
    scene.arm_notification_wait(
        publication,
        "notif-agent",
        Arc::new(AtomicBool::new(true)),
        u64::MAX,
        tze_hud_scene::render_wake::RenderWakeNotifier::new(|| {}),
    );
    publication
}

#[test]
fn pointer_hold_fires_once_at_the_token_duration() {
    let (mut harness, clock) = card(&[0]);
    let publication = arm_wait(&harness);
    let approve = point(&harness, "approve");
    // A tap, and repeated taps, fire nothing (R6.2 S3).
    for _ in 0..3 {
        harness.click(approve.0, approve.1);
        harness.tick();
        clock.advance(100);
        harness.tick();
    }
    assert!(take_action_ids(&harness).is_empty());
    // Released at 300 ms: nothing fires and the sweep drains (S2).
    pointer(&mut harness, approve, PointerEventKind::Down);
    assert!(holding(&harness), "a destructive press starts a hold");
    clock.advance(300);
    harness.tick();
    pointer(&mut harness, approve, PointerEventKind::Up);
    harness.tick();
    assert!(!holding(&harness));
    assert!(harness.scene().overlay.notification_hold_drain.is_some());
    clock.advance(1_000);
    harness.tick();
    assert!(take_action_ids(&harness).is_empty());
    // Held 600 ms: fires once without waiting for release (S1).
    pointer(&mut harness, approve, PointerEventKind::Down);
    clock.advance(599);
    harness.tick();
    assert!(take_action_ids(&harness).is_empty());
    clock.advance(1);
    harness.tick();
    {
        let scene = harness.scene();
        assert_eq!(
            scene.notification_wait_outcome(publication),
            Some(NotificationOutcome::Answered),
            "an armed wait returns the held action"
        );
        assert_eq!(
            scene
                .notification_wait_answer(publication, "notif-agent")
                .map(|a| a.kind.clone()),
            Some(tze_hud_scene::graph::PendingActionKind::Callback(
                "approve".into()
            ))
        );
    }
    harness.scene().release_notification_wait(publication);
    assert_eq!(take_action_ids(&harness), ["approve"]);
    pointer(&mut harness, approve, PointerEventKind::Up);
    clock.advance(1_000);
    harness.tick();
    assert!(take_action_ids(&harness).is_empty(), "fires exactly once");
}

#[test]
fn pointer_hold_ends_on_leave_and_capture_loss() {
    let (mut harness, clock) = card(&[0]);
    let approve = point(&harness, "approve");
    let reject = point(&harness, "reject");
    pointer(&mut harness, approve, PointerEventKind::Down);
    pointer(&mut harness, reject, PointerEventKind::Move);
    harness.tick();
    assert!(!holding(&harness), "leaving the button ends the hold");
    pointer(&mut harness, approve, PointerEventKind::Move);
    clock.advance(1_000);
    harness.tick();
    pointer(&mut harness, approve, PointerEventKind::Up);
    harness.tick();
    assert!(take_action_ids(&harness).is_empty());

    pointer(&mut harness, approve, PointerEventKind::Down);
    harness.app.cancel_chrome_feedback();
    harness.tick();
    assert!(!holding(&harness), "capture loss ends the hold");
    clock.advance(1_000);
    harness.tick();
    assert!(take_action_ids(&harness).is_empty());
}

#[test]
fn enter_space_and_digit_holds_fire_once_and_quick_presses_do_not() {
    for code in ["Enter", "Space", "Digit1"] {
        let (mut harness, clock) = card(&[0]);
        if code == "Digit1" {
            assert!(harness.app.focus_newest_question(true).unwrap());
        } else {
            focus_action(&mut harness, 0);
        }
        // Quick press (S4).
        key(&mut harness, code, true, false);
        assert!(holding(&harness), "{code} starts a hold");
        clock.advance(300);
        key(&mut harness, code, false, false);
        assert!(!holding(&harness));
        clock.advance(1_000);
        harness.tick();
        assert!(take_action_ids(&harness).is_empty(), "quick {code}");
        // Key repeat alone never starts or fires a hold.
        key(&mut harness, code, true, true);
        assert!(!holding(&harness), "{code} repeat");
        key(&mut harness, code, false, false);
        // Held for the token duration, with repeats: fires once.
        key(&mut harness, code, true, false);
        clock.advance(300);
        key(&mut harness, code, true, true);
        clock.advance(300);
        harness.tick();
        assert_eq!(take_action_ids(&harness), ["approve"], "held {code}");
        assert_eq!(
            harness.app.state.answer_keyboard.return_key.as_deref(),
            Some(code),
            "focus returns on the release"
        );
        key(&mut harness, code, false, false);
        assert!(take_action_ids(&harness).is_empty());
    }
}

#[test]
fn chord_focuses_the_primary_and_enter_takes_it() {
    // Plain card: chord then Enter takes the first action (R6.1 S1).
    let (mut harness, _) = card(&[]);
    assert!(harness.app.focus_newest_question(true).unwrap());
    key(&mut harness, "Enter", true, false);
    assert_eq!(take_action_ids(&harness), ["approve"]);
    assert!(harness.app.state.answer_keyboard.focus.is_none());
    key(&mut harness, "Enter", false, false);

    // Destructive first action: the second is the primary (S2).
    let (mut harness, _) = card(&[0]);
    assert!(harness.app.focus_newest_question(true).unwrap());
    key(&mut harness, "Enter", true, false);
    assert_eq!(take_action_ids(&harness), ["reject"]);

    // All destructive: the chord focuses the first; a quick Enter does nothing (S3).
    let (mut harness, clock) = card(&[0, 1, 2, 3]);
    assert!(harness.app.focus_newest_question(true).unwrap());
    let focused = harness.scene().overlay.notification_focus.clone().unwrap();
    assert!(focused.ends_with(":0"), "{focused}");
    key(&mut harness, "Enter", true, false);
    key(&mut harness, "Enter", false, false);
    clock.advance(1_000);
    harness.tick();
    assert!(take_action_ids(&harness).is_empty());
}

#[test]
fn interruptions_end_a_hold_with_no_callback() {
    type Interrupt = fn(&mut HeadlessEventLoopHarness);
    let cases: [(&str, Interrupt); 7] = [
        ("escape", |h| key(h, "Escape", true, false)),
        ("other key", |h| key(h, "ArrowRight", true, false)),
        ("focus loss", |h| h.app.handle_window_focus(None, false)),
        ("safe mode", |h| {
            h.app
                .state
                .safe_mode_atomic
                .store(true, std::sync::atomic::Ordering::Release)
        }),
        ("hide all", |h| {
            h.scene()
                .set_owner_hidden(tze_hud_scene::graph::visibility::SurfaceClasses {
                    zones: true,
                    ..Default::default()
                });
        }),
        ("cleared", |h| {
            h.scene().clear_zone("notification-area").unwrap();
        }),
        ("expired", |h| {
            let mut scene = h.scene();
            scene
                .zone_registry
                .active_publishes
                .get_mut("notification-area")
                .unwrap()[0]
                .expires_at_wall_us = Some(0);
            scene.drain_expired_zone_publications();
        }),
    ];
    for (name, interrupt) in cases {
        let (mut harness, clock) = card(&[0]);
        focus_action(&mut harness, 0);
        key(&mut harness, "Enter", true, false);
        assert!(holding(&harness), "{name}");
        clock.advance(300);
        interrupt(&mut harness);
        harness.tick();
        assert!(!holding(&harness), "{name} ends the hold");
        clock.advance(1_000);
        harness.tick();
        harness
            .app
            .state
            .safe_mode_atomic
            .store(false, std::sync::atomic::Ordering::Release);
        harness.tick();
        assert!(take_action_ids(&harness).is_empty(), "{name}");
        assert!(
            harness.app.state.answer_keyboard.hold_key.is_none(),
            "{name}"
        );
    }

    // A replacement card is never acted on by the key still held.
    let (mut harness, clock) = card(&[0]);
    focus_action(&mut harness, 0);
    key(&mut harness, "Enter", true, false);
    {
        let mut scene = harness.scene();
        let content = scene.zone_registry.active_for_zone("notification-area")[0]
            .content
            .clone();
        scene.clear_zone("notification-area").unwrap();
        scene
            .publish_to_zone(
                "notification-area",
                content,
                "notif-agent",
                None,
                None,
                None,
            )
            .unwrap();
    }
    harness.tick();
    assert!(!holding(&harness));
    clock.advance(1_000);
    harness.tick();
    assert!(take_action_ids(&harness).is_empty());
}

#[test]
fn hold_ms_zero_activates_on_an_ordinary_press() {
    let (mut harness, _) = card(&[0]);
    harness
        .app
        .state
        .global_tokens
        .insert("notification.action.hold.ms".into(), "0".into());
    let approve = point(&harness, "approve");
    harness.click(approve.0, approve.1);
    harness.tick();
    assert!(!holding(&harness));
    assert_eq!(take_action_ids(&harness), ["approve"]);
}
