//! A pointer press in progress keeps an answer-window card past its
//! deadline until the press ends (docs/invariants.md §1): its own release
//! answers; any other ending expires the card at once.

use super::*;
use tze_hud_scene::graph::NOTIFICATION_PRESS_HOLD_MAX_US;

const ZONE: &str = "notification-area";

/// The Approve / Reject / Wait / Details card, using a 1 s answer window
/// from the owner (the marker MCP and gRPC admission set). Returns the
/// harness, its clock and the deadline.
fn answer_window_card() -> (HeadlessEventLoopHarness, Arc<tze_hud_scene::TestClock>, u64) {
    let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
    let mut harness = approve_reject_notification_harness_on(clock.clone());
    let deadline = {
        let mut scene = harness.scene();
        let deadline = scene.now_wall_us() + 1_000_000;
        let record = &mut scene.zone_registry.active_publishes.get_mut(ZONE).unwrap()[0];
        record.expires_at_wall_us = Some(deadline);
        record.answer_window = true;
        deadline
    };
    harness.tick();
    (harness, clock, deadline)
}

fn pointer(harness: &mut HeadlessEventLoopHarness, at: (f32, f32), kind: PointerEventKind) {
    (harness.app.state.cursor_x, harness.app.state.cursor_y) = at;
    harness.app.enqueue_pointer_event(kind);
    harness.drain();
    harness.tick();
}

/// Press Approve with 100 ms left and keep it down 600 ms, past the deadline.
fn press_across_deadline(
    harness: &mut HeadlessEventLoopHarness,
    clock: &tze_hud_scene::TestClock,
) -> (f32, f32) {
    let approve = harness.notification_action_center("approve").unwrap();
    clock.advance(900);
    harness.tick();
    pointer(harness, approve, PointerEventKind::Down);
    clock.advance(600);
    harness.tick();
    assert_eq!(
        harness.zone_publication_count(ZONE),
        1,
        "the press holds the card"
    );
    approve
}

#[test]
fn press_held_across_answer_window_deadline_answers_on_release() {
    // R13.1 S1: released on the button it began on.
    let (mut harness, clock, deadline) = answer_window_card();
    let approve = press_across_deadline(&mut harness, &clock);
    // Idle: the only wake is the bound, not a per-frame poll.
    assert_eq!(
        harness.next_frame_wall_us(),
        Some(deadline + NOTIFICATION_PRESS_HOLD_MAX_US)
    );
    pointer(&mut harness, approve, PointerEventKind::Up);
    assert_eq!(take_action_ids(&harness), ["approve"]);
    assert!(harness.scene().overlay.notification_press.is_none());
    clock.advance(1_000);
    harness.tick();
    assert_eq!(
        harness.zone_publication_count(ZONE),
        0,
        "the answered exit ends"
    );

    // R13.1 S2: released elsewhere, cancelled or interrupted by safe mode:
    // the card expires at once and nothing is queued.
    for ending in ["off", "cancel", "safe-mode"] {
        let (mut harness, clock, _) = answer_window_card();
        let approve = press_across_deadline(&mut harness, &clock);
        match ending {
            "off" => pointer(&mut harness, (1.0, 1.0), PointerEventKind::Up),
            "cancel" => {
                harness.app.cancel_chrome_feedback();
                harness.tick();
            }
            _ => {
                harness
                    .app
                    .state
                    .safe_mode_atomic
                    .store(true, std::sync::atomic::Ordering::Release);
                harness.tick();
            }
        }
        assert_eq!(harness.zone_publication_count(ZONE), 0, "{ending}");
        pointer(&mut harness, approve, PointerEventKind::Up);
        assert!(take_action_ids(&harness).is_empty(), "{ending}");
    }

    // R13.1 S4: still down 5 s after the deadline: it expires then, and the
    // release does nothing.
    let (mut harness, clock, _) = answer_window_card();
    let approve = press_across_deadline(&mut harness, &clock);
    // 500 ms past the deadline already.
    clock.advance(NOTIFICATION_PRESS_HOLD_MAX_US / 1_000 - 501);
    harness.tick();
    assert_eq!(harness.zone_publication_count(ZONE), 1);
    clock.advance(1);
    harness.tick();
    assert_eq!(harness.zone_publication_count(ZONE), 0);
    pointer(&mut harness, approve, PointerEventKind::Up);
    assert!(take_action_ids(&harness).is_empty());
}

#[test]
fn keys_and_presses_that_began_late_hold_nothing() {
    // R13.2 S1: keyboard focus on the card at the deadline holds nothing.
    let (mut harness, clock, _) = answer_window_card();
    assert!(harness.app.focus_newest_question(true).unwrap());
    clock.advance(1_000);
    harness.tick();
    assert_eq!(harness.zone_publication_count(ZONE), 0);

    // R13.2 S2: an agent expiry is never extended by a press.
    let (mut harness, clock, _) = answer_window_card();
    harness
        .scene()
        .zone_registry
        .active_publishes
        .get_mut(ZONE)
        .unwrap()[0]
        .answer_window = false;
    let approve = harness.notification_action_center("approve").unwrap();
    clock.advance(900);
    pointer(&mut harness, approve, PointerEventKind::Down);
    clock.advance(200);
    harness.tick();
    assert_eq!(harness.zone_publication_count(ZONE), 0);
    pointer(&mut harness, approve, PointerEventKind::Up);
    assert!(take_action_ids(&harness).is_empty());
}

#[test]
fn a_press_the_guard_dropped_holds_nothing() {
    // R13.2 S3: the card arrives under the resting pointer with 100 ms of
    // its window left; the guard drops the press, so the card expires.
    let (mut harness, clock, _) = answer_window_card();
    let approve = harness.notification_action_center("approve").unwrap();
    let card = harness.scene().zone_registry.active_for_zone(ZONE)[0].clone();
    harness.scene().clear_zone(ZONE).unwrap();
    (harness.app.state.cursor_x, harness.app.state.cursor_y) = approve;
    harness.tick();
    {
        let mut scene = harness.scene();
        let expiry = scene.now_wall_us() + 100_000;
        scene
            .publish_to_zone(ZONE, card.content, "notif-agent", None, None, None)
            .unwrap();
        let record = &mut scene.zone_registry.active_publishes.get_mut(ZONE).unwrap()[0];
        record.expires_at_wall_us = Some(expiry);
        record.answer_window = true;
    }
    harness.tick();
    assert!(harness.scene().overlay.notification_press_guard.is_some());
    pointer(&mut harness, approve, PointerEventKind::Down);
    assert!(harness.scene().overlay.notification_press.is_none());
    clock.advance(200);
    harness.tick();
    assert_eq!(harness.zone_publication_count(ZONE), 0);
    pointer(&mut harness, approve, PointerEventKind::Up);
    assert!(take_action_ids(&harness).is_empty());
}
