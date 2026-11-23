//! Notification stack trims (hud-ljhku.5 R5.2/R5.3): a full stack spares
//! unanswered questions on both admission planes, and a trimmed card's
//! publisher receives exactly one displaced receipt.

use super::*;
use crate::clock::TestClock;
use crate::types::{
    ContentionPolicy, GeometryPolicy, LayerAttachment, NotificationAction, NotificationPayload,
    RenderingPolicy, ZoneContent, ZoneDefinition, ZoneMediaType,
};
use std::sync::atomic::{AtomicBool, Ordering};

const ZONE: &str = "notification-area";

/// The production notification stack depth (types.rs default zone).
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

fn card(text: &str, actions: Vec<NotificationAction>) -> ZoneContent {
    ZoneContent::Notification(NotificationPayload {
        text: text.into(),
        icon: String::new(),
        urgency: 1,
        ttl_ms: None,
        title: String::new(),
        actions,
    })
}

fn status(text: &str) -> ZoneContent {
    card(text, Vec::new())
}

fn question(text: &str) -> ZoneContent {
    card(
        text,
        vec![NotificationAction {
            label: "Yes".into(),
            callback_id: Some(format!("{text}-yes")),
            url: None,
            role: None,
        }],
    )
}

fn link(text: &str) -> ZoneContent {
    card(
        text,
        vec![NotificationAction {
            label: "Open".into(),
            callback_id: None,
            url: Some("https://example.com/report".into()),
            role: None,
        }],
    )
}

#[derive(Clone, Copy, Debug)]
enum Plane {
    Mcp,
    Grpc,
}

fn publish(scene: &mut SceneGraph, plane: Plane, agent: &str, content: ZoneContent) {
    publish_keyed(scene, plane, agent, content, None);
}

fn publish_keyed(
    scene: &mut SceneGraph,
    plane: Plane,
    agent: &str,
    content: ZoneContent,
    key: Option<&str>,
) {
    let key = key.map(str::to_owned);
    match plane {
        Plane::Mcp => scene
            .publish_to_zone(ZONE, content, agent, key, None, None)
            .unwrap(),
        Plane::Grpc => {
            let publisher = crate::Publisher {
                namespace: agent.into(),
                lease_id: None,
                origin: None,
            };
            let publication = crate::Publication::Zone {
                zone_name: ZONE.into(),
                content,
                merge_key: key,
                expires_at_wall_us: None,
                content_classification: None,
                breakpoints: Vec::new(),
                held: true,
            };
            scene.publish(&publisher, &publication).unwrap();
        }
    }
}

fn shown(scene: &SceneGraph) -> Vec<String> {
    scene
        .zone_registry
        .active_for_zone(ZONE)
        .iter()
        .map(|r| match &r.content {
            ZoneContent::Notification(n) => n.text.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

fn id_of(scene: &SceneGraph, text: &str) -> SceneId {
    scene
        .zone_registry
        .active_for_zone(ZONE)
        .iter()
        .find(|r| matches!(&r.content, ZoneContent::Notification(n) if n.text == text))
        .expect("card shown")
        .publication_id
}

fn receipts(scene: &mut SceneGraph, agent: &str) -> Vec<(Option<String>, PendingActionKind)> {
    scene
        .take_pending_actions(agent)
        .into_iter()
        .map(|a| (a.key, a.kind))
        .collect()
}

#[test]
fn full_stack_trims_oldest_status_and_keeps_the_question_on_both_planes() {
    for plane in [Plane::Mcp, Plane::Grpc] {
        let (mut scene, _) = scene();
        publish(&mut scene, plane, "asker", question("q1"));
        for text in ["s1", "s2", "s3", "s4", "s5"] {
            publish(&mut scene, plane, "builder", status(text));
        }
        assert_eq!(
            shown(&scene),
            ["q1", "s2", "s3", "s4", "s5"],
            "{plane:?}: the oldest status card left, the older question stayed"
        );
    }
}

#[test]
fn all_question_stack_drops_the_oldest_question_on_both_planes() {
    for plane in [Plane::Mcp, Plane::Grpc] {
        let (mut scene, _) = scene();
        for text in ["q1", "q2", "q3", "q4", "q5", "q6"] {
            publish(&mut scene, plane, "asker", question(text));
        }
        assert_eq!(shown(&scene), ["q2", "q3", "q4", "q5", "q6"], "{plane:?}");
    }
}

#[test]
fn answered_question_no_longer_counts_as_a_question() {
    for plane in [Plane::Mcp, Plane::Grpc] {
        let (mut scene, _) = scene();
        for text in ["q1", "q2", "q3", "q4", "q5"] {
            publish(&mut scene, plane, "asker", question(text));
        }
        let answered = id_of(&scene, "q2");
        scene
            .overlay
            .notification_choices
            .insert(answered, "q2-yes".into());
        publish(&mut scene, plane, "asker", question("q6"));
        assert_eq!(
            shown(&scene),
            ["q1", "q3", "q4", "q5", "q6"],
            "{plane:?}: the answered card left before any unanswered question"
        );
        assert!(
            receipts(&mut scene, "asker").is_empty(),
            "{plane:?}: an answered card left by its answer, not unseen"
        );
    }
}

#[test]
fn url_only_cards_are_not_questions() {
    for plane in [Plane::Mcp, Plane::Grpc] {
        let (mut scene, _) = scene();
        publish(&mut scene, plane, "asker", question("q1"));
        publish(&mut scene, plane, "asker", link("u2"));
        for text in ["q3", "q4", "q5", "q6"] {
            publish(&mut scene, plane, "asker", question(text));
        }
        assert_eq!(shown(&scene), ["q1", "q3", "q4", "q5", "q6"], "{plane:?}");
    }
}

#[test]
fn a_status_card_arriving_at_a_stack_of_questions_is_the_one_trimmed() {
    let (mut scene, _) = scene();
    for text in ["q1", "q2", "q3", "q4", "q5"] {
        publish(&mut scene, Plane::Mcp, "asker", question(text));
    }
    publish(&mut scene, Plane::Mcp, "builder", status("late"));
    assert_eq!(shown(&scene), ["q1", "q2", "q3", "q4", "q5"]);
    assert_eq!(
        receipts(&mut scene, "builder"),
        [(None, PendingActionKind::Displaced)]
    );
}

#[test]
fn trimmed_card_queues_one_displaced_receipt_for_its_publisher_only() {
    for plane in [Plane::Mcp, Plane::Grpc] {
        let (mut scene, _) = scene();
        publish_keyed(&mut scene, plane, "builder", status("s1"), Some("build:1"));
        for text in ["s2", "s3", "s4", "s5"] {
            publish(&mut scene, plane, "other", status(text));
        }
        let victim = id_of(&scene, "s1");
        publish(&mut scene, plane, "other", status("s6"));

        let queued = scene.take_pending_actions("builder");
        assert_eq!(queued.len(), 1, "{plane:?}");
        assert_eq!(queued[0].kind, PendingActionKind::Displaced);
        assert_eq!(queued[0].key.as_deref(), Some("build:1"));
        assert_eq!(queued[0].zone_name, ZONE);
        assert_eq!(queued[0].publication_id, Some(victim));
        assert!(
            scene.take_pending_actions("other").is_empty(),
            "{plane:?}: the agent whose publish caused the trim receives nothing"
        );
        assert!(
            !scene.take_notification_wait_notifiers().is_empty(),
            "{plane:?}: the receipt owes its publisher an input wake"
        );
    }
}

#[test]
fn armed_wait_takes_the_displacement_and_no_receipt_is_queued() {
    let (mut scene, _) = scene();
    let lease = scene.grant_lease("asker", 60_000);
    let id = SceneId::new();
    let active = Arc::new(AtomicBool::new(true));
    let deadline = scene.now_monotonic_us() + 30_000_000;
    scene.arm_notification_wait(
        id,
        "asker",
        Arc::clone(&active),
        deadline,
        RenderWakeNotifier::default(),
    );
    scene
        .publish_notification_until_with_id(ZONE, question("q1"), "asker", lease, None, None, id)
        .unwrap();
    for text in ["q2", "q3", "q4", "q5", "q6"] {
        publish(&mut scene, Plane::Mcp, "other", question(text));
    }
    assert!(!shown(&scene).contains(&"q1".to_string()));
    assert_eq!(
        scene.notification_wait_outcome(id),
        Some(NotificationOutcome::Gone(NotificationGone::Displaced))
    );
    assert!(receipts(&mut scene, "asker").is_empty());
}

#[test]
fn a_finished_wait_does_not_swallow_the_receipt() {
    let (mut scene, _) = scene();
    let lease = scene.grant_lease("asker", 60_000);
    let id = SceneId::new();
    let active = Arc::new(AtomicBool::new(true));
    let deadline = scene.now_monotonic_us() + 30_000_000;
    scene.arm_notification_wait(
        id,
        "asker",
        Arc::clone(&active),
        deadline,
        RenderWakeNotifier::default(),
    );
    scene
        .publish_notification_until_with_id(ZONE, question("q1"), "asker", lease, None, None, id)
        .unwrap();
    // The call returned at its deadline; the card stays on screen.
    active.store(false, Ordering::Release);
    for text in ["q2", "q3", "q4", "q5", "q6"] {
        publish(&mut scene, Plane::Mcp, "other", question(text));
    }
    assert_eq!(
        receipts(&mut scene, "asker"),
        [(None, PendingActionKind::Displaced)]
    );
}

#[test]
fn expiry_clear_and_same_key_replacement_queue_no_receipt() {
    let (mut scene, clock) = scene();
    // Same-key replacement keeps the slot; nothing was trimmed.
    publish_keyed(&mut scene, Plane::Mcp, "a", status("v1"), Some("k"));
    publish_keyed(&mut scene, Plane::Mcp, "a", status("v2"), Some("k"));
    assert_eq!(shown(&scene), ["v2"]);
    // Clear removes the agent's own cards.
    scene.clear_zone_and_cancel_pending(ZONE, "a").unwrap();
    assert!(shown(&scene).is_empty());
    // Expiry sweeps a card at its deadline.
    let expires = scene.now_wall_us() + 1_000;
    scene
        .publish_to_zone(ZONE, status("brief"), "a", None, Some(expires), None)
        .unwrap();
    clock.advance(5);
    scene.drain_expired_zone_publications();
    assert!(shown(&scene).is_empty());
    assert!(receipts(&mut scene, "a").is_empty());
}

#[test]
fn an_expired_card_trimmed_before_its_sweep_queues_no_receipt() {
    let (mut scene, clock) = scene();
    let expires = scene.now_wall_us() + 1_000;
    scene
        .publish_to_zone(ZONE, status("stale"), "a", None, Some(expires), None)
        .unwrap();
    clock.advance(5);
    for text in ["s2", "s3", "s4", "s5", "s6"] {
        publish(&mut scene, Plane::Mcp, "b", status(text));
    }
    assert!(!shown(&scene).contains(&"stale".to_string()));
    assert!(receipts(&mut scene, "a").is_empty());
}

#[test]
fn a_publisher_that_never_drains_input_keeps_a_bounded_receipt_queue() {
    for plane in [Plane::Mcp, Plane::Grpc] {
        let (mut scene, _) = scene();
        // An accepted human event is never evicted to make room for receipts.
        let _ = scene.push_pending_action(PendingAction {
            publication_id: None,
            publisher_namespace: "ticker".into(),
            zone_name: ZONE.into(),
            kind: PendingActionKind::Callback("approve".into()),
            key: None,
        });
        let mut trimmed = Vec::new();
        for i in 0..60 {
            let ids = |scene: &SceneGraph| -> Vec<SceneId> {
                scene
                    .zone_registry
                    .active_for_zone(ZONE)
                    .iter()
                    .map(|r| r.publication_id)
                    .collect()
            };
            let before = ids(&scene);
            publish(&mut scene, plane, "ticker", status(&format!("tick {i}")));
            let after = ids(&scene);
            trimmed.extend(before.into_iter().filter(|id| !after.contains(id)));
        }
        let queued = scene.take_pending_actions("ticker");
        let displaced: Vec<_> = queued
            .iter()
            .filter(|a| a.kind == PendingActionKind::Displaced)
            .collect();
        assert_eq!(displaced.len(), DISPLACED_RECEIPT_CAP, "{plane:?}");
        assert!(
            queued
                .iter()
                .any(|a| a.kind == PendingActionKind::Callback("approve".into())),
            "{plane:?}"
        );
        // Only older receipts drop: the kept ones are the last trims, in order.
        let kept: Vec<_> = displaced
            .iter()
            .map(|a| a.publication_id.unwrap())
            .collect();
        assert_eq!(
            kept,
            trimmed[trimmed.len() - DISPLACED_RECEIPT_CAP..],
            "{plane:?}"
        );
    }
}

#[test]
fn a_newer_receipt_for_the_same_key_replaces_the_older_one() {
    let (mut scene, _) = scene();
    for round in 0..3 {
        publish_keyed(
            &mut scene,
            Plane::Mcp,
            "builder",
            status("build"),
            Some("build:1"),
        );
        for i in 0..5 {
            publish(
                &mut scene,
                Plane::Mcp,
                "other",
                status(&format!("r{round} s{i}")),
            );
        }
    }
    assert_eq!(
        receipts(&mut scene, "builder"),
        [(Some("build:1".into()), PendingActionKind::Displaced)]
    );
}

/// Hands out a queued display per placement and counts every call.
#[derive(Debug, Default)]
struct Placer {
    next: std::sync::Mutex<Vec<Option<String>>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl crate::QuestionPlacer for Placer {
    fn place(&self) -> Option<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.next.lock().unwrap().pop().flatten()
    }
}

fn placed(scene: &SceneGraph) -> Vec<(String, Option<String>)> {
    scene
        .zone_registry
        .active_for_zone(ZONE)
        .iter()
        .map(|r| {
            let ZoneContent::Notification(n) = &r.content else {
                unreachable!()
            };
            (n.text.clone(), r.display.clone())
        })
        .collect()
}

/// R12.1: a question is placed once when presented, on either plane and at
/// a delay's due time; a non-question (url-only included) never asks; a
/// same-key replacement keeps its display; and the stack still trims once
/// across displays (S8).
#[test]
fn questions_are_placed_once_when_presented() {
    let (mut scene, clock) = scene();
    let placer = Arc::new(Placer::default());
    scene.overlay.question_placer = Some(placer.clone());
    let calls = || placer.calls.load(Ordering::SeqCst);
    let queue = |d: &str| placer.next.lock().unwrap().push(Some(d.into()));

    queue("DISPLAY2");
    publish_keyed(&mut scene, Plane::Mcp, "a", question("q1"), Some("k"));
    queue("DISPLAY3");
    publish(&mut scene, Plane::Grpc, "a", question("q2"));
    publish(&mut scene, Plane::Mcp, "a", status("s"));
    publish(&mut scene, Plane::Mcp, "a", link("url"));
    assert_eq!(calls(), 2, "only questions sample");
    // S4: a same-key replacement keeps DISPLAY2 without sampling again,
    // whatever it now asks.
    publish_keyed(&mut scene, Plane::Mcp, "a", status("q1 again"), Some("k"));
    assert_eq!(calls(), 2);
    assert_eq!(
        placed(&scene),
        [
            ("q1 again".into(), Some("DISPLAY2".into())),
            ("q2".into(), Some("DISPLAY3".into())),
            ("s".into(), None),
            ("url".into(), None),
        ]
    );
    // S6: a delayed question samples when it comes due, not at the call.
    let publisher = crate::Publisher {
        namespace: "a".into(),
        lease_id: None,
        origin: None,
    };
    let publication = crate::Publication::Zone {
        zone_name: ZONE.into(),
        content: question("later"),
        merge_key: None,
        expires_at_wall_us: None,
        content_classification: None,
        breakpoints: Vec::new(),
        held: true,
    };
    scene.schedule_publication(clock.now_us() + 5_000, publisher, publication);
    scene.apply_due_publications();
    assert_eq!(calls(), 2, "not due yet");
    clock.advance(5);
    queue("DISPLAY2");
    scene.apply_due_publications();
    assert_eq!(calls(), 3);
    // S8: five cards over three displays; a sixth trims the oldest
    // non-question (the replaced card on DISPLAY2) once, as one stack.
    assert_eq!(placed(&scene).len(), 5);
    publish(&mut scene, Plane::Mcp, "a", status("sixth"));
    let texts: Vec<_> = placed(&scene).into_iter().map(|(t, _)| t).collect();
    assert_eq!(texts, ["q2", "s", "url", "later", "sixth"]);
}
