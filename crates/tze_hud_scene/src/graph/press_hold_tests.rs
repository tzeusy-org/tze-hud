//! A press in progress keeps an answer-window card past its deadline
//! (docs/invariants.md §1): only until the press ends, at most 5 s, and only
//! for the press's own control. Explicit agent actions win.

use super::*;
use crate::clock::TestClock;
use crate::types::{
    ContentionPolicy, GeometryPolicy, LayerAttachment, NotificationAction, NotificationPayload,
    Rect, RenderingPolicy, ZoneContent, ZoneDefinition, ZoneHitRegion, ZoneInteractionKind,
    ZoneMediaType,
};
use std::sync::atomic::AtomicBool;

const ZONE: &str = "notification-area";
const AGENT: &str = "asker";
/// A short owner answer window.
const WINDOW_US: u64 = 1_000_000;
const EXIT_MS: u32 = 200;

fn scene() -> (SceneGraph, TestClock) {
    let clock = TestClock::new(1_000);
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: ZONE.into(),
        description: "notifications".into(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 16,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    (scene, clock)
}

fn card(text: &str, actions: &[&str]) -> ZoneContent {
    ZoneContent::Notification(NotificationPayload {
        progress: None,
        text: text.into(),
        icon: String::new(),
        urgency: 1,
        ttl_ms: None,
        title: String::new(),
        actions: actions
            .iter()
            .map(|label| NotificationAction {
                label: (*label).into(),
                callback_id: Some(label.to_lowercase()),
                url: None,
                ..Default::default()
            })
            .collect(),
    })
}

fn question(text: &str) -> ZoneContent {
    card(text, &["Ship", "Hold"])
}

/// MCP admission: `answer_window` when no `ttl_ms` was given.
fn ask(scene: &mut SceneGraph, content: ZoneContent, key: Option<&str>, answer_window: bool) {
    let lease = scene.grant_lease(AGENT, 600_000);
    let expiry = scene.now_wall_us() + WINDOW_US;
    scene
        .publish_notification_until_with_id(
            ZONE,
            content,
            AGENT,
            lease,
            key.map(str::to_owned),
            Some(expiry),
            SceneId::new(),
            answer_window,
        )
        .unwrap();
}

/// gRPC admission's batch.
fn grpc_publication(content: ZoneContent, expiry: u64, answer_window: bool) -> Publication {
    Publication::Zone {
        zone_name: ZONE.into(),
        content,
        merge_key: None,
        expires_at_wall_us: Some(expiry),
        content_classification: None,
        breakpoints: Vec::new(),
        held: false,
        answer_window,
    }
}

fn record<'a>(scene: &'a SceneGraph, text: &str) -> Option<&'a ZonePublishRecord> {
    scene
        .zone_registry
        .active_for_zone(ZONE)
        .iter()
        .find(|r| matches!(&r.content, ZoneContent::Notification(n) if n.text == text))
}

fn shown(scene: &SceneGraph, text: &str) -> bool {
    record(scene, text).is_some()
}

/// The control's hit region, as the compositor publishes it.
fn control(scene: &mut SceneGraph, text: &str, kind: ZoneInteractionKind) -> ZoneHitRegion {
    let r = record(scene, text).expect("card shown");
    let id = r.publication_id;
    let interaction_id = match kind {
        ZoneInteractionKind::Action { index } => format!("zone:{ZONE}:action:{id}:{AGENT}:{index}"),
        _ => format!("zone:{ZONE}:dismiss:{id}:{AGENT}"),
    };
    let region = ZoneHitRegion {
        publication_id: id,
        zone_name: ZONE.into(),
        published_at_wall_us: r.published_at_wall_us,
        publisher_namespace: AGENT.into(),
        bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
        kind,
        interaction_id,
        tab_order: 0,
    };
    scene.overlay.zone_hit_regions.push(region.clone());
    region
}

fn ship(scene: &mut SceneGraph, text: &str) -> ZoneHitRegion {
    control(scene, text, ZoneInteractionKind::Action { index: 0 })
}

fn answers(scene: &mut SceneGraph) -> Vec<PendingActionKind> {
    scene
        .take_pending_actions(AGENT)
        .into_iter()
        .map(|a| a.kind)
        .collect()
}

#[test]
fn answer_window_press_holds_card_until_release() {
    // R13.1 S1: pressed with 100 ms left, released on "Ship" 600 ms later.
    let (mut scene, clock) = scene();
    ask(&mut scene, question("q"), None, true);
    let deadline = record(&scene, "q").unwrap().expires_at_wall_us.unwrap();
    let button = ship(&mut scene, "q");
    clock.advance(900);
    scene.begin_notification_press(&button);
    clock.advance(600);
    assert_eq!(scene.drain_expired_zone_publications(), 0);
    assert!(shown(&scene, "q"), "the press holds the past-due card");
    // Its link, Go To and reveal feedback stay with it.
    let id = button.publication_id;
    scene.overlay.notification_link_attempts.insert((id, 0), 1);
    scene.overlay.notification_go_to.insert(
        id,
        crate::graph::overlay::NotificationGoTo {
            available: true,
            ..Default::default()
        },
    );
    scene.overlay.notification_reveal_regions.push(
        crate::graph::overlay::NotificationRevealRegion {
            target: crate::graph::overlay::NotificationRevealTarget {
                publication_id: id,
                field: crate::graph::overlay::NotificationRevealField::Body,
            },
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            focus_id: "body".into(),
        },
    );
    scene.prune_notification_feedback();
    assert!(
        scene
            .overlay
            .notification_link_attempts
            .contains_key(&(id, 0))
    );
    assert!(scene.overlay.notification_go_to.contains_key(&id));
    assert_eq!(scene.overlay.notification_reveal_regions.len(), 1);
    // One wake, at the bound, while it holds.
    assert_eq!(
        scene.next_publication_expiry_wall_us(),
        Some(deadline + NOTIFICATION_PRESS_HOLD_MAX_US)
    );
    assert!(
        scene
            .activate_pressed_notification(&button, EXIT_MS)
            .is_some()
    );
    assert!(
        !scene.end_notification_press(),
        "answered: nothing to expire"
    );
    assert_eq!(
        answers(&mut scene),
        [PendingActionKind::Callback("ship".into())]
    );
    // The answered exit runs, then the card leaves.
    assert!(shown(&scene, "q"));
    clock.advance(u64::from(EXIT_MS));
    scene.drain_expired_zone_publications();
    assert!(!shown(&scene, "q"));

    // R13.1 S3: × released on × is a human dismiss, as today.
    let (mut scene, clock) = self::scene();
    ask(&mut scene, question("q"), None, true);
    let close = control(&mut scene, "q", ZoneInteractionKind::Dismiss);
    clock.advance(900);
    scene.begin_notification_press(&close);
    clock.advance(600);
    scene.drain_expired_zone_publications();
    assert!(
        scene
            .activate_pressed_notification(&close, EXIT_MS)
            .is_some()
    );
    scene.end_notification_press();
    assert!(!shown(&scene, "q"));
    assert_eq!(answers(&mut scene), [PendingActionKind::Dismiss]);

    // R13.1 S5: a destructive hold that completes after the deadline is the
    // press's own activation.
    let (mut scene, clock) = self::scene();
    ask(&mut scene, question("q"), None, true);
    let button = ship(&mut scene, "q");
    clock.advance(900);
    scene.begin_notification_press(&button);
    scene.begin_notification_hold(button.clone(), None);
    clock.advance(800);
    scene.drain_expired_zone_publications();
    let hold = scene
        .take_completed_notification_hold(800)
        .expect("hold due");
    assert!(
        scene
            .activate_pressed_notification(&hold.region, EXIT_MS)
            .is_some()
    );
    assert_eq!(
        answers(&mut scene),
        [PendingActionKind::Callback("ship".into())]
    );
}

#[test]
fn press_hold_release_off_control_expires_card() {
    // R13.1 S2: released elsewhere (or cancelled): the card expires at once,
    // an armed wait sees gone:"expired", and nothing is queued.
    let (mut scene, clock) = scene();
    let lease = scene.grant_lease(AGENT, 600_000);
    let id = SceneId::new();
    let active = Arc::new(AtomicBool::new(true));
    let wait_deadline = scene.now_monotonic_us() + 30_000_000;
    scene.arm_notification_wait(
        id,
        AGENT,
        Arc::clone(&active),
        wait_deadline,
        crate::render_wake::RenderWakeNotifier::default(),
    );
    let expiry = scene.now_wall_us() + WINDOW_US;
    scene
        .publish_notification_until_with_id(
            ZONE,
            question("q"),
            AGENT,
            lease,
            None,
            Some(expiry),
            id,
            true,
        )
        .unwrap();
    let button = ship(&mut scene, "q");
    clock.advance(900);
    scene.begin_notification_press(&button);
    clock.advance(600);
    scene.drain_expired_zone_publications();
    assert_eq!(scene.notification_wait_outcome(id), None, "still held");
    assert!(scene.end_notification_press());
    assert!(!shown(&scene, "q"));
    assert_eq!(
        scene.notification_wait_outcome(id),
        Some(NotificationOutcome::Gone(NotificationGone::Expired))
    );
    assert!(
        scene
            .activate_pressed_notification(&button, EXIT_MS)
            .is_none()
    );
    assert!(answers(&mut scene).is_empty());

    // R13.1 S4: still down 5 s after the deadline: the card expires then and
    // the later release does nothing.
    let (mut scene, clock) = self::scene();
    ask(&mut scene, question("q"), None, true);
    let button = ship(&mut scene, "q");
    clock.advance(900);
    scene.begin_notification_press(&button);
    clock.advance(100 + NOTIFICATION_PRESS_HOLD_MAX_US / 1_000 - 1);
    scene.drain_expired_zone_publications();
    assert!(shown(&scene, "q"));
    clock.advance(1);
    scene.drain_expired_zone_publications();
    assert!(!shown(&scene, "q"));
    assert!(
        scene
            .activate_pressed_notification(&button, EXIT_MS)
            .is_none()
    );
    scene.end_notification_press();
    assert!(answers(&mut scene).is_empty());

    // R13.1 S5: a destructive hold released before completion expires the card.
    let (mut scene, clock) = self::scene();
    ask(&mut scene, question("q"), None, true);
    let button = ship(&mut scene, "q");
    clock.advance(900);
    scene.begin_notification_press(&button);
    scene.begin_notification_hold(button, None);
    clock.advance(300);
    scene.cancel_notification_hold(800, 0);
    scene.end_notification_press();
    assert!(!shown(&scene, "q"));
}

#[test]
fn only_the_pressed_control_and_an_owner_answer_window_hold_a_card() {
    // R13.2 S1: nothing pressed (hover, focus and keys never reach the
    // scene's press): the card expires on schedule.
    let (mut scene, clock) = scene();
    ask(&mut scene, question("q"), None, true);
    clock.advance(1_000);
    scene.drain_expired_zone_publications();
    assert!(!shown(&scene, "q"));

    // R13.2 S2: an agent expiry (ttl_ms, expires_at) or no actions.
    for (text, content, answer_window) in [
        ("explicit", question("explicit"), false),
        ("status", card("status", &[]), true),
    ] {
        let (mut scene, clock) = self::scene();
        ask(&mut scene, content, None, answer_window);
        assert!(!record(&scene, text).unwrap().answer_window);
        let close = control(&mut scene, text, ZoneInteractionKind::Dismiss);
        clock.advance(900);
        scene.begin_notification_press(&close);
        clock.advance(200);
        scene.drain_expired_zone_publications();
        assert!(!shown(&scene, text), "{text}: a press changes nothing");
    }

    // R13.2 S3: a press that began after the deadline holds nothing.
    let (mut scene, clock) = self::scene();
    ask(&mut scene, question("q"), None, true);
    let button = ship(&mut scene, "q");
    clock.advance(1_000);
    scene.begin_notification_press(&button);
    scene.drain_expired_zone_publications();
    assert!(!shown(&scene, "q"));

    // Past the deadline, another control of the held card is refused.
    let (mut scene, clock) = self::scene();
    ask(&mut scene, question("q"), None, true);
    let button = ship(&mut scene, "q");
    let other = control(&mut scene, "q", ZoneInteractionKind::Action { index: 1 });
    clock.advance(900);
    scene.begin_notification_press(&button);
    clock.advance(200);
    assert!(
        scene
            .activate_pressed_notification(&other, EXIT_MS)
            .is_none()
    );
    assert!(shown(&scene, "q"));
}

#[test]
fn only_the_pointer_press_itself_answers_a_press_held_card() {
    // Past the deadline the pressed control is reachable only by that
    // press's own release or completed hold, never by the keyboard (an
    // answer chord digit or Enter) or a plain reply-field open.
    let (mut scene, clock) = scene();
    ask(&mut scene, reply_question("q"), None, true);
    let button = ship(&mut scene, "q");
    clock.advance(900);
    scene.begin_notification_press(&button);
    clock.advance(200);
    assert_eq!(scene.drain_expired_zone_publications(), 0);
    assert!(scene.activate_notification(&button, EXIT_MS).is_none());
    assert!(answers(&mut scene).is_empty());
    assert!(shown(&scene, "q"), "still held for the press's own release");

    let (mut scene, clock) = self::scene();
    ask(&mut scene, reply_question("q"), None, true);
    let other = control(&mut scene, "q", ZoneInteractionKind::Action { index: 1 });
    clock.advance(900);
    scene.begin_notification_press(&other);
    clock.advance(200);
    assert!(
        !scene.set_notification_reply_view(Some(reply_view(other.publication_id, ""))),
        "a non-press open of a past-deadline card is refused"
    );
    assert!(scene.overlay.notification_reply.is_none());
    assert!(scene.activate_notification(&other, EXIT_MS).is_none());
    scene.end_notification_press();
    assert!(!shown(&scene, "q"));
}

#[test]
fn hud_hold_retimes_a_press_held_card() {
    // R13.3 S2: the new expiry applies and the exception no longer does.
    let (mut scene, clock) = scene();
    ask(&mut scene, question("q"), None, true);
    let button = ship(&mut scene, "q");
    clock.advance(900);
    scene.begin_notification_press(&button);
    clock.advance(200);
    scene.drain_expired_zone_publications();
    assert!(scene.hold_zone_publications(ZONE, AGENT, Some(10_000_000)));
    let held = record(&scene, "q").unwrap();
    assert!(!held.answer_window);
    assert_eq!(
        held.expires_at_wall_us,
        Some(scene.now_wall_us() + 10_000_000)
    );
    // The release follows the ordinary rules: released elsewhere, it stays.
    assert!(!scene.end_notification_press());
    assert!(shown(&scene, "q"));
}

#[test]
fn replace_clear_and_trim_win_over_a_press_hold() {
    // R13.3 S1: each removes the card at once; the release activates nothing.
    for how in ["replace", "clear", "trim"] {
        let (mut scene, clock) = scene();
        ask(&mut scene, question("q"), Some("k"), true);
        let button = ship(&mut scene, "q");
        clock.advance(900);
        scene.begin_notification_press(&button);
        clock.advance(200);
        scene.drain_expired_zone_publications();
        assert!(shown(&scene, "q"));
        match how {
            "replace" => ask(&mut scene, question("q2"), Some("k"), true),
            "clear" => scene.clear_zone_and_cancel_pending(ZONE, AGENT).unwrap(),
            _ => {
                for text in ["s1", "s2", "s3", "s4", "s5"] {
                    scene
                        .publish_to_zone(ZONE, card(text, &[]), "other", None, None, None)
                        .unwrap();
                }
            }
        }
        assert!(!shown(&scene, "q"), "{how}");
        assert!(
            scene
                .activate_pressed_notification(&button, EXIT_MS)
                .is_none(),
            "{how}"
        );
        scene.end_notification_press();
        assert!(
            !answers(&mut scene)
                .iter()
                .any(|kind| matches!(kind, PendingActionKind::Callback(_))),
            "{how}"
        );
    }
}

#[test]
fn answer_window_marker_follows_admission_and_hud_hold() {
    // Both planes set it, a delayed publish keeps it, and it is runtime-only.
    let (mut scene, clock) = scene();
    ask(&mut scene, question("mcp"), None, true);
    assert!(record(&scene, "mcp").unwrap().answer_window);
    let expiry = scene.now_wall_us() + WINDOW_US;
    assert!(
        scene
            .publish(
                &Publisher::namespace(AGENT),
                &grpc_publication(question("grpc"), expiry, true)
            )
            .is_ok()
    );
    assert!(record(&scene, "grpc").unwrap().answer_window);
    let present_at = scene.now_wall_us() + 500_000;
    scene.schedule_publication(
        present_at,
        Publisher::namespace(AGENT),
        grpc_publication(question("delayed"), present_at + WINDOW_US, true),
    );
    clock.advance(500);
    scene.apply_due_publications();
    assert!(record(&scene, "delayed").unwrap().answer_window);
    let json = serde_json::to_string(record(&scene, "delayed").unwrap()).unwrap();
    assert!(!json.contains("answer_window"), "{json}");

    // hud_hold clears it on shown and still-scheduled publishes.
    let present_at = scene.now_wall_us() + 500_000;
    scene.schedule_publication(
        present_at,
        Publisher::namespace(AGENT),
        grpc_publication(question("pending"), present_at + WINDOW_US, true),
    );
    assert!(scene.hold_zone_publications(ZONE, AGENT, Some(WINDOW_US)));
    assert!(!record(&scene, "mcp").unwrap().answer_window);
    clock.advance(500);
    scene.apply_due_publications();
    assert!(!record(&scene, "pending").unwrap().answer_window);
}

#[test]
fn a_press_schedules_no_wake_until_it_holds_a_card() {
    // Idle cost: before the deadline the next wake is the deadline itself;
    // a press on an explicit-expiry card adds nothing.
    let (mut scene, clock) = scene();
    ask(&mut scene, question("q"), None, true);
    let deadline = record(&scene, "q").unwrap().expires_at_wall_us.unwrap();
    assert_eq!(scene.next_publication_expiry_wall_us(), Some(deadline));
    let button = ship(&mut scene, "q");
    clock.advance(900);
    scene.begin_notification_press(&button);
    // Holding: one wake at the bound.
    clock.advance(100);
    assert_eq!(
        scene.next_publication_expiry_wall_us(),
        Some(deadline + NOTIFICATION_PRESS_HOLD_MAX_US)
    );
    // Hidden by the owner: the press holds nothing.
    scene.set_owner_hidden(SurfaceClasses {
        zones: true,
        ..Default::default()
    });
    assert_eq!(scene.next_publication_expiry_wall_us(), Some(deadline));
    scene.drain_expired_zone_publications();
    assert!(!shown(&scene, "q"));
}

/// A card with one callback action ("Ship") and one reply action ("Other…").
fn reply_question(text: &str) -> ZoneContent {
    let ZoneContent::Notification(mut payload) = card(text, &["Ship"]) else {
        unreachable!()
    };
    payload.actions.push(NotificationAction {
        label: "Other…".into(),
        reply: Some("other".into()),
        ..Default::default()
    });
    ZoneContent::Notification(payload)
}

fn reply_view(id: SceneId, text: &str) -> crate::graph::overlay::NotificationReplyView {
    crate::graph::overlay::NotificationReplyView {
        publication_id: id,
        action_index: 1,
        text: text.into(),
        cursor_byte: text.len(),
        selection_anchor: text.len(),
        at_capacity: false,
    }
}

#[test]
fn press_opened_reply_field_keeps_a_past_deadline_card_until_closed() {
    // "Other…" pressed with 100 ms left and released after the deadline: the
    // press's own activation opens the field, and the field keeps the card,
    // empty or not, until it closes.
    let (mut scene, clock) = scene();
    ask(&mut scene, reply_question("q"), None, true);
    let other = control(&mut scene, "q", ZoneInteractionKind::Action { index: 1 });
    clock.advance(900);
    scene.begin_notification_press(&other);
    clock.advance(200);
    assert_eq!(scene.drain_expired_zone_publications(), 0);
    assert!(matches!(
        scene.activate_pressed_notification(&other, EXIT_MS),
        Some(crate::graph::NotificationActivation::Reply(_))
    ));
    assert!(
        scene.overlay.notification_reply.is_some(),
        "the field opens"
    );
    scene.end_notification_press();
    scene.drain_expired_zone_publications();
    assert!(shown(&scene, "q"), "the open field keeps the card");
    assert_eq!(scene.next_publication_expiry_wall_us(), None);
    // Typing redraws keep it; Esc closes the field and the card expires.
    assert!(scene.set_notification_reply_view(Some(reply_view(other.publication_id, "a"))));
    assert!(scene.set_notification_reply_view(Some(reply_view(other.publication_id, ""))));
    assert!(shown(&scene, "q"));
    assert!(scene.set_notification_reply_view(None));
    assert!(!shown(&scene, "q"), "Esc: the normal expiry applies");

    // Submitting from that field answers once, then the card leaves by the
    // answered exit.
    let (mut scene, clock) = self::scene();
    ask(&mut scene, reply_question("q"), None, true);
    let other = control(&mut scene, "q", ZoneInteractionKind::Action { index: 1 });
    clock.advance(900);
    scene.begin_notification_press(&other);
    clock.advance(200);
    assert!(
        scene
            .activate_pressed_notification(&other, EXIT_MS)
            .is_some()
    );
    scene.end_notification_press();
    assert!(
        scene
            .submit_notification_reply(&other, "staging".into(), EXIT_MS)
            .is_some()
    );
    assert_eq!(
        answers(&mut scene),
        [PendingActionKind::Reply {
            callback_id: "other".into(),
            text: "staging".into()
        }]
    );
    clock.advance(u64::from(EXIT_MS));
    scene.drain_expired_zone_publications();
    assert!(!shown(&scene, "q"));
}

#[test]
fn reply_and_press_holds_overlap_and_either_can_end_first() {
    for reply_ends_first in [true, false] {
        let (mut scene, clock) = scene();
        ask(&mut scene, reply_question("q"), None, true);
        let deadline = record(&scene, "q").unwrap().expires_at_wall_us.unwrap();
        let ship = ship(&mut scene, "q");
        let other = control(&mut scene, "q", ZoneInteractionKind::Action { index: 1 });
        // A nonempty field is open, then a press begins on "Ship" with
        // 100 ms left and stays down past the deadline.
        assert!(
            scene.set_notification_reply_view(Some(reply_view(other.publication_id, "staging")))
        );
        clock.advance(900);
        scene.begin_notification_press(&ship);
        clock.advance(200);
        assert_eq!(scene.drain_expired_zone_publications(), 0);
        assert!(shown(&scene, "q"));
        assert_eq!(
            scene.next_publication_expiry_wall_us(),
            None,
            "the field holds it"
        );
        if reply_ends_first {
            assert!(scene.set_notification_reply_view(None));
            assert!(shown(&scene, "q"), "the press still holds it");
            assert_eq!(
                scene.next_publication_expiry_wall_us(),
                Some(deadline + NOTIFICATION_PRESS_HOLD_MAX_US)
            );
            // Only a press on another control holds it: no reply is accepted.
            assert!(
                scene
                    .submit_notification_reply(&other, "staging".into(), EXIT_MS)
                    .is_none()
            );
            assert!(scene.end_notification_press());
            assert!(!shown(&scene, "q"));
        } else {
            scene.end_notification_press();
            scene.drain_expired_zone_publications();
            assert!(shown(&scene, "q"), "the field still holds it");
            assert_eq!(scene.next_publication_expiry_wall_us(), None);
            assert!(scene.set_notification_reply_view(None));
            assert!(!shown(&scene, "q"));
        }
        assert!(answers(&mut scene).is_empty());
    }
}
