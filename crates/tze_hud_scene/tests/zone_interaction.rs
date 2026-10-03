//! # Zone Interaction Layer Tests — [hud-ltgk.4]
//!
//! Tests for zone-level input routing: dismiss affordance, action buttons,
//! `dismiss_notification()`, `ZoneHitRegion` hit-test routing, and
//! `ZoneInteraction` hit result variant.
//!
//! ## What is tested
//!
//! 1. `dismiss_notification()` removes a matching publication and increments version.
//! 2. `dismiss_notification()` returns `false` for unknown zone or non-existent pub.
//! 3. `dismiss_notification()` leaves other publications in the same zone untouched.
//! 4. `hit_test()` returns `ZoneInteraction` when `zone_hit_regions` contains a matching region.
//! 5. `hit_test()` returns `Passthrough` when no region matches (fallthrough).
//! 6. `HitResult::ZoneInteraction` carries correct zone_name, interaction_id, kind.
//! 7. `ZoneHitRegion` in `zone_hit_regions` does not survive `#[serde(skip)]` round-trip.
//! 8. `ZoneInteractionKind::Dismiss` vs `Action` variants round-trip via Clone.
//!
//! ## Layer
//!
//! Layer 0 — pure Rust, no GPU, no async.  All tests run in < 1 s.

use tze_hud_scene::{
    HitResult,
    graph::SceneGraph,
    types::{
        ContentionPolicy, GeometryPolicy, LayerAttachment, NotificationAction, NotificationPayload,
        Rect, RenderingPolicy, SceneId, ZoneContent, ZoneDefinition, ZoneHitRegion,
        ZoneInteractionKind, ZoneMediaType,
    },
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build a 1920×1080 scene with a single Stack zone "notif" and a single
/// notification publication from "agent-a" at `published_at_wall_us=100`.
///
/// Returns `(scene, published_at_wall_us, publisher_namespace)`.
fn setup_single_notification_scene() -> (SceneGraph, u64, &'static str) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab = scene.create_tab("Main", 0).unwrap();

    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Test notification zone".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    });

    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Test notification".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    // Retrieve the published_at_wall_us for the record we just published.
    let pubs = scene.zone_registry.active_for_zone("notif");
    let published_at = pubs[0].published_at_wall_us;
    (scene, published_at, "agent-a")
}

/// Insert a `ZoneHitRegion` for dismiss into an existing scene.
fn insert_dismiss_region(
    scene: &mut SceneGraph,
    zone_name: &str,
    published_at_wall_us: u64,
    publisher_namespace: &str,
    bounds: Rect,
    tab_order: u32,
) {
    let interaction_id =
        format!("zone:{zone_name}:dismiss:{published_at_wall_us}:{publisher_namespace}");
    scene.overlay.zone_hit_regions.push(ZoneHitRegion {
        zone_name: zone_name.to_string(),
        published_at_wall_us,
        publisher_namespace: publisher_namespace.to_string(),
        bounds,
        kind: ZoneInteractionKind::Dismiss,
        interaction_id,
        tab_order,
    });
}

// ─── dismiss_notification tests ───────────────────────────────────────────────

/// `dismiss_notification()` MUST return `true` and remove the matching publication.
#[test]
fn dismiss_notification_removes_matching_publication() {
    let (mut scene, published_at, ns) = setup_single_notification_scene();

    assert_eq!(
        scene.zone_registry.active_for_zone("notif").len(),
        1,
        "precondition: one publication present"
    );

    let removed = scene.dismiss_notification("notif", published_at, ns);

    assert!(
        removed,
        "dismiss_notification must return true for a matching publication"
    );
    assert_eq!(
        scene.zone_registry.active_for_zone("notif").len(),
        0,
        "publication must be removed after dismiss"
    );
}

/// `dismiss_notification()` MUST increment `version` when a publication is removed.
#[test]
fn dismiss_notification_increments_version() {
    let (mut scene, published_at, ns) = setup_single_notification_scene();

    let version_before = scene.version;
    scene.dismiss_notification("notif", published_at, ns);

    assert!(
        scene.version > version_before,
        "version must increase after dismiss: before={version_before}, after={}",
        scene.version
    );
}

/// `dismiss_notification()` MUST return `false` and not modify scene when
/// the zone does not exist.
#[test]
fn dismiss_notification_returns_false_for_unknown_zone() {
    let (mut scene, published_at, ns) = setup_single_notification_scene();

    let version_before = scene.version;
    let result = scene.dismiss_notification("nonexistent-zone", published_at, ns);

    assert!(!result, "must return false for unknown zone");
    assert_eq!(
        scene.version, version_before,
        "version must not change for unknown zone"
    );
}

/// `dismiss_notification()` MUST return `false` when the publication key does
/// not match any active publication (wrong `published_at_wall_us`).
#[test]
fn dismiss_notification_returns_false_for_wrong_pub_key() {
    let (mut scene, _published_at, ns) = setup_single_notification_scene();

    let result = scene.dismiss_notification("notif", 99999, ns);

    assert!(
        !result,
        "must return false when published_at does not match"
    );
    assert_eq!(
        scene.zone_registry.active_for_zone("notif").len(),
        1,
        "publication must still be present after failed dismiss"
    );
}

/// `dismiss_notification()` MUST leave other publications in the same zone
/// untouched when only the matching one is removed.
#[test]
fn dismiss_notification_leaves_other_publications_untouched() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab = scene.create_tab("Main", 0).unwrap();

    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Multi-publisher zone".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    });

    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "First".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Second".to_string(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-b",
            None,
            None,
            None,
        )
        .unwrap();

    let pubs = scene.zone_registry.active_for_zone("notif");
    assert_eq!(pubs.len(), 2, "precondition: two publications");

    // Find agent-a's published_at_wall_us.
    let agent_a_pub_at = pubs
        .iter()
        .find(|r| r.publisher_namespace == "agent-a")
        .map(|r| r.published_at_wall_us)
        .expect("agent-a publication must exist");

    let removed = scene.dismiss_notification("notif", agent_a_pub_at, "agent-a");

    assert!(
        removed,
        "dismiss must return true for agent-a's publication"
    );
    let remaining = scene.zone_registry.active_for_zone("notif");
    assert_eq!(
        remaining.len(),
        1,
        "one publication must remain after dismiss"
    );
    assert_eq!(
        remaining[0].publisher_namespace, "agent-b",
        "agent-b's publication must survive"
    );
}

// ─── ZoneHitRegion hit_test routing ──────────────────────────────────────────

/// `hit_test()` MUST return `ZoneInteraction` when the point falls within a
/// zone hit region and no tile claims the point.
#[test]
fn hit_test_returns_zone_interaction_for_dismiss_button() {
    let (mut scene, published_at, ns) = setup_single_notification_scene();

    // Place dismiss region in the top-right of a simulated zone slot:
    // zone at x=1440 (1920*0.75), width=460 (1920*0.24).
    // Dismiss button: 20×20 px at (1880, 0) i.e. the top-right corner.
    let dismiss_bounds = Rect::new(1880.0, 0.0, 20.0, 20.0);
    insert_dismiss_region(&mut scene, "notif", published_at, ns, dismiss_bounds, 0);

    // Hit inside the dismiss button.
    let result = scene.hit_test(1890.0, 10.0);

    match result {
        HitResult::ZoneInteraction {
            ref zone_name,
            published_at_wall_us,
            ref publisher_namespace,
            ref interaction_id,
            kind: ZoneInteractionKind::Dismiss,
        } => {
            assert_eq!(zone_name, "notif");
            assert_eq!(published_at_wall_us, published_at);
            assert_eq!(publisher_namespace, ns);
            assert!(
                interaction_id.contains("dismiss"),
                "interaction_id must contain 'dismiss', got: {interaction_id}"
            );
        }
        other => panic!("expected ZoneInteraction(Dismiss), got: {other:?}"),
    }
}

/// `hit_test()` MUST return `Passthrough` when the point is outside all
/// zone hit regions and no tile claims the point.
#[test]
fn hit_test_returns_passthrough_when_outside_zone_regions() {
    let (mut scene, published_at, ns) = setup_single_notification_scene();

    // Region in a corner far from the center.
    let dismiss_bounds = Rect::new(1880.0, 0.0, 20.0, 20.0);
    insert_dismiss_region(&mut scene, "notif", published_at, ns, dismiss_bounds, 0);

    // Hit well outside the dismiss button (center of screen).
    let result = scene.hit_test(960.0, 540.0);

    assert!(
        matches!(result, HitResult::Passthrough),
        "hit outside all regions must be Passthrough, got: {result:?}"
    );
}

/// `hit_test()` `ZoneInteraction` result MUST carry the correct `interaction_id`
/// following the scheme `zone:{zone_name}:dismiss:{published_at_wall_us}:{publisher_namespace}`.
#[test]
fn zone_interaction_result_has_correct_interaction_id_scheme() {
    let (mut scene, published_at, ns) = setup_single_notification_scene();

    let dismiss_bounds = Rect::new(100.0, 100.0, 20.0, 20.0);
    insert_dismiss_region(&mut scene, "notif", published_at, ns, dismiss_bounds, 0);

    let result = scene.hit_test(110.0, 110.0);

    if let HitResult::ZoneInteraction { interaction_id, .. } = result {
        let expected = format!("zone:notif:dismiss:{published_at}:{ns}");
        assert_eq!(
            interaction_id, expected,
            "interaction_id must follow zone:{{name}}:dismiss:{{pub_at}}:{{ns}} scheme"
        );
    } else {
        panic!("expected ZoneInteraction, got: {result:?}");
    }
}

/// `hit_test()` MUST return `ZoneInteraction` with `Action` kind when the
/// point falls within an action button hit region.
#[test]
fn hit_test_returns_zone_interaction_for_action_button() {
    let (mut scene, published_at, ns) = setup_single_notification_scene();

    let action_bounds = Rect::new(1440.0, 12.0, 100.0, 22.0);
    let action_id = format!("zone:notif:action:{published_at}:{ns}:confirm");
    scene.overlay.zone_hit_regions.push(ZoneHitRegion {
        zone_name: "notif".to_string(),
        published_at_wall_us: published_at,
        publisher_namespace: ns.to_string(),
        bounds: action_bounds,
        kind: ZoneInteractionKind::Action {
            callback_id: "confirm".to_string(),
        },
        interaction_id: action_id.clone(),
        tab_order: 1,
    });

    let result = scene.hit_test(1490.0, 20.0);

    match result {
        HitResult::ZoneInteraction {
            kind: ZoneInteractionKind::Action { ref callback_id },
            ref interaction_id,
            ..
        } => {
            assert_eq!(callback_id, "confirm");
            assert_eq!(interaction_id, &action_id);
        }
        other => panic!("expected ZoneInteraction(Action), got: {other:?}"),
    }
}

// ─── ZoneInteractionKind / ZoneHitRegion struct tests ─────────────────────────

/// `ZoneInteractionKind::Dismiss` and `Action` variants MUST round-trip via Clone.
#[test]
fn zone_interaction_kind_clones_correctly() {
    let dismiss = ZoneInteractionKind::Dismiss;
    let action = ZoneInteractionKind::Action {
        callback_id: "my-callback".to_string(),
    };

    assert_eq!(dismiss.clone(), ZoneInteractionKind::Dismiss);
    assert_eq!(
        action.clone(),
        ZoneInteractionKind::Action {
            callback_id: "my-callback".to_string()
        }
    );
}

/// `ZoneHitRegion` fields MUST be accessible and match what was inserted.
#[test]
fn zone_hit_region_fields_round_trip() {
    let region = ZoneHitRegion {
        zone_name: "my-zone".to_string(),
        published_at_wall_us: 42_000_000,
        publisher_namespace: "my-agent".to_string(),
        bounds: Rect::new(10.0, 20.0, 30.0, 40.0),
        kind: ZoneInteractionKind::Dismiss,
        interaction_id: "zone:my-zone:dismiss:42000000:my-agent".to_string(),
        tab_order: 7,
    };

    assert_eq!(region.zone_name, "my-zone");
    assert_eq!(region.published_at_wall_us, 42_000_000);
    assert_eq!(region.publisher_namespace, "my-agent");
    assert_eq!(region.bounds.x, 10.0);
    assert_eq!(region.bounds.y, 20.0);
    assert_eq!(region.bounds.width, 30.0);
    assert_eq!(region.bounds.height, 40.0);
    assert_eq!(region.tab_order, 7);
    assert_eq!(region.kind, ZoneInteractionKind::Dismiss);
}

// ─── NotificationAction struct tests ─────────────────────────────────────────

/// `NotificationAction` MUST store label and callback_id.
#[test]
fn notification_action_fields_accessible() {
    let action = NotificationAction {
        label: "Confirm".to_string(),
        callback_id: "confirm-action".to_string(),
    };

    assert_eq!(action.label, "Confirm");
    assert_eq!(action.callback_id, "confirm-action");
}

/// `NotificationPayload` with actions MUST serialize via serde (spot-check).
#[test]
fn notification_payload_with_actions_serializes() {
    let payload = NotificationPayload {
        text: "Do you confirm?".to_string(),
        icon: String::new(),
        urgency: 1,
        ttl_ms: None,
        title: String::new(),
        actions: vec![
            NotificationAction {
                label: "Yes".to_string(),
                callback_id: "yes".to_string(),
            },
            NotificationAction {
                label: "No".to_string(),
                callback_id: "no".to_string(),
            },
        ],
    };

    let json = serde_json::to_string(&payload).expect("serialization must succeed");
    let back: NotificationPayload =
        serde_json::from_str(&json).expect("deserialization must succeed");

    assert_eq!(back.text, "Do you confirm?");
    assert_eq!(back.actions.len(), 2);
    assert_eq!(back.actions[0].callback_id, "yes");
    assert_eq!(back.actions[1].label, "No");
}

/// `NotificationPayload` WITHOUT actions field in JSON MUST deserialize with
/// an empty `actions` vec (serde default).
#[test]
fn notification_payload_serde_default_actions() {
    let json = r#"{"text":"hello","icon":"","urgency":0,"ttl_ms":null}"#;
    let payload: NotificationPayload =
        serde_json::from_str(json).expect("legacy JSON must deserialize");
    assert!(
        payload.actions.is_empty(),
        "actions must default to empty Vec when absent in JSON"
    );
}

// ─── zone_hit_regions cleared via zone_registry ───────────────────────────────

/// `zone_hit_regions` MUST be cleared and remain empty after the scene is
/// serialized and deserialized (it is `#[serde(skip)]`).
#[test]
fn zone_hit_regions_are_skipped_in_serialization() {
    let (mut scene, published_at, ns) = setup_single_notification_scene();

    // Manually insert a region to simulate what compositor would write each frame.
    insert_dismiss_region(
        &mut scene,
        "notif",
        published_at,
        ns,
        Rect::new(0.0, 0.0, 20.0, 20.0),
        0,
    );

    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        1,
        "precondition: region present"
    );

    let json = serde_json::to_string(&scene).expect("serialization must succeed");
    let restored: SceneGraph = serde_json::from_str(&json).expect("deserialization must succeed");

    assert!(
        restored.overlay.zone_hit_regions.is_empty(),
        "zone_hit_regions must be empty after serde round-trip (field is #[serde(skip)])"
    );
}
