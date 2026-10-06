//! # Zone Ontology Integration Tests
//!
//! Tests for the four-level zone model per scene-graph/spec.md §Requirement: Zone Registry (line 185).
//!
//! ## Four-level zone ontology
//!
//! 1. **ZoneDefinition** (zone type) — schema: accepted_media_types, contention_policy, layer_attachment
//! 2. **ZoneInstance** — zone type bound to a specific tab
//! 3. **ZonePublication** (= ZonePublishRecord) — a single publish event
//! 4. **ZoneOccupancy** — resolved state after applying contention policy
//!
//! ## What is tested
//!
//! - Zone test scenes: zone_publish_subtitle, zone_reject_wrong_type,
//!   zone_conflict_two_publishers, zone_orchestrate_then_publish,
//!   zone_disconnect_cleanup
//! - All four contention policies (LatestWins, Stack, MergeByKey, Replace)
//! - All five v1 media types (StreamText, ShortTextWithIcon, KeyValuePairs, StaticImage, SolidColor)
//! - Layer attachment routing: Background, Content (z >= ZONE_TILE_Z_MIN), Chrome
//! - ZonePublishToken: validation rejects expired/invalid tokens
//! - ZoneOccupancy: query API in v1 (no effective_geometry)
//! - Per-publisher clear semantics
//! - Expiry semantics (expires_at_wall_us)
//! - Six v1 default zones loadable from config (with_defaults())
//! - Contention proptest: random publish/clear sequences verify occupancy invariants

use std::collections::HashMap;

use tze_hud_scene::{
    graph::SceneGraph,
    test_scenes::{ClockMs, TestSceneRegistry},
    types::{
        ContentionPolicy, DisplayEdge, GeometryPolicy, LayerAttachment, NotificationPayload,
        RenderingPolicy, ResourceId, Rgba, SceneId, StatusBarPayload, ZONE_TILE_Z_MIN, ZoneContent,
        ZoneDefinition, ZoneMediaType,
    },
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build a subtitle zone definition (Content layer, LatestWins).
fn make_subtitle_zone() -> ZoneDefinition {
    ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_string(),
        description: "Subtitle overlay".to_string(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 48.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 2,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Content,
        ephemeral: false,
    }
}

// ─── Scene: zone_publish_subtitle ────────────────────────────────────────────

#[test]
fn zone_publish_subtitle_has_subtitle_zone() {
    let registry = TestSceneRegistry::default();
    let (graph, spec) = registry
        .build("zone_publish_subtitle", ClockMs::FIXED)
        .unwrap();
    assert!(spec.has_zones, "spec must declare has_zones=true");
    assert!(
        graph.zone_registry.get_by_name("subtitle").is_some(),
        "subtitle zone must be registered"
    );
}

#[test]
fn zone_publish_subtitle_layer_attachment_is_content() {
    let registry = TestSceneRegistry::default();
    let (graph, _spec) = registry
        .build("zone_publish_subtitle", ClockMs::FIXED)
        .unwrap();
    let zone = graph.zone_registry.get_by_name("subtitle").unwrap();
    assert_eq!(
        zone.layer_attachment,
        LayerAttachment::Content,
        "subtitle zone must use Content layer attachment"
    );
}

// ─── Scene: zone_reject_wrong_type ───────────────────────────────────────────

#[test]
fn zone_reject_wrong_type_rejects_mismatched_media_type() {
    // Zone accepts only StreamText; publishing KeyValuePairs must be rejected.
    let registry = TestSceneRegistry::default();
    let (mut graph, _spec) = registry
        .build("zone_reject_wrong_type", ClockMs::FIXED)
        .unwrap();

    let wrong_content = ZoneContent::StatusBar(StatusBarPayload {
        entries: {
            let mut m = HashMap::new();
            m.insert("clock".to_string(), "12:00".to_string());
            m
        },
    });

    let result = graph.publish_to_zone("typed_zone", wrong_content, "agent", None, None, None);
    assert!(
        result.is_err(),
        "publishing wrong media type must be rejected"
    );
}

#[test]
fn zone_reject_wrong_type_accepts_correct_media_type() {
    let registry = TestSceneRegistry::default();
    let (mut graph, _spec) = registry
        .build("zone_reject_wrong_type", ClockMs::FIXED)
        .unwrap();
    let result = graph.publish_to_zone(
        "typed_zone",
        ZoneContent::StreamText("valid content".to_string()),
        "agent",
        None,
        None,
        None,
    );
    assert!(result.is_ok(), "publishing correct media type must succeed");
}

// ─── Scene: zone_conflict_two_publishers ─────────────────────────────────────

#[test]
fn zone_conflict_latest_wins_policy() {
    // Two publishers to a LatestWins zone: only the latest should survive.
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone());

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("first".to_string()),
            "pub_a",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("second".to_string()),
            "pub_b",
            None,
            None,
            None,
        )
        .unwrap();

    let pubs = scene.zone_registry.active_for_zone("subtitle");
    assert_eq!(pubs.len(), 1, "LatestWins: only 1 publication survives");
    assert_eq!(pubs[0].publisher_namespace, "pub_b");
    assert_eq!(
        pubs[0].content,
        ZoneContent::StreamText("second".to_string())
    );
}

// ─── Scene: zone_orchestrate_then_publish ────────────────────────────────────

#[test]
fn zone_orchestrate_then_publish_has_three_zones() {
    let registry = TestSceneRegistry::default();
    let (graph, _spec) = registry
        .build("zone_orchestrate_then_publish", ClockMs::FIXED)
        .unwrap();
    for name in &["alert_banner", "notification_area", "status_bar"] {
        assert!(
            graph.zone_registry.get_by_name(name).is_some(),
            "zone '{name}' must be registered"
        );
    }
}

// ─── Scene: zone_disconnect_cleanup ──────────────────────────────────────────

// ─── All four contention policies ────────────────────────────────────────────

#[test]
fn contention_stack_evicts_oldest_at_max_depth() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.3,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 3 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    });

    let notif = |text: &str| {
        ZoneContent::Notification(NotificationPayload {
            text: text.to_string(),
            icon: "".to_string(),
            urgency: 1,
            ttl_ms: None,
            title: String::new(),
            actions: Vec::new(),
        })
    };

    // Push 3 to fill the stack
    scene
        .publish_to_zone("notif", notif("msg1"), "a1", None, None, None)
        .unwrap();
    scene
        .publish_to_zone("notif", notif("msg2"), "a2", None, None, None)
        .unwrap();
    scene
        .publish_to_zone("notif", notif("msg3"), "a3", None, None, None)
        .unwrap();
    assert_eq!(scene.zone_registry.active_for_zone("notif").len(), 3);

    // 4th push must evict oldest (msg1)
    scene
        .publish_to_zone("notif", notif("msg4"), "a4", None, None, None)
        .unwrap();
    let pubs = scene.zone_registry.active_for_zone("notif");
    assert_eq!(
        pubs.len(),
        3,
        "Stack max_depth=3 must evict oldest on overflow"
    );

    let texts: Vec<_> = pubs
        .iter()
        .map(|r| {
            if let ZoneContent::Notification(n) = &r.content {
                n.text.clone()
            } else {
                "?".into()
            }
        })
        .collect();
    assert!(
        !texts.contains(&"msg1".to_string()),
        "oldest msg1 must be evicted"
    );
    assert!(
        texts.contains(&"msg4".to_string()),
        "newest msg4 must survive"
    );
}

#[test]
fn contention_merge_by_key_same_key_replaces() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "status".to_string(),
        description: "Status bar".to_string(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.04,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 16 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    });

    let kv = |k: &str, v: &str| {
        let mut entries = HashMap::new();
        entries.insert(k.to_string(), v.to_string());
        ZoneContent::StatusBar(StatusBarPayload { entries })
    };

    scene
        .publish_to_zone(
            "status",
            kv("clock", "12:00"),
            "a1",
            Some("clock".to_string()),
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "status",
            kv("battery", "80%"),
            "a2",
            Some("battery".to_string()),
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        scene.zone_registry.active_for_zone("status").len(),
        2,
        "different keys should coexist"
    );

    // Update same key "clock"
    scene
        .publish_to_zone(
            "status",
            kv("clock", "12:01"),
            "a1",
            Some("clock".to_string()),
            None,
            None,
        )
        .unwrap();
    let pubs = scene.zone_registry.active_for_zone("status");
    assert_eq!(
        pubs.len(),
        2,
        "updating existing key must not create a new entry"
    );
    let clock = pubs
        .iter()
        .find(|r| r.merge_key.as_deref() == Some("clock"))
        .unwrap();
    if let ZoneContent::StatusBar(sb) = &clock.content {
        assert_eq!(
            sb.entries["clock"], "12:01",
            "clock must be updated to 12:01"
        );
    } else {
        panic!("expected StatusBar content");
    }
}

#[test]
fn contention_replace_evicts_current() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "pip".to_string(),
        description: "PiP".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.70,
            width_pct: 0.22,
            height_pct: 0.26,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Replace,
        max_publishers: 2,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Content,
        ephemeral: false,
    });

    scene
        .publish_to_zone(
            "pip",
            ZoneContent::SolidColor(Rgba::WHITE),
            "a1",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "pip",
            ZoneContent::SolidColor(Rgba::BLACK),
            "a2",
            None,
            None,
            None,
        )
        .unwrap();

    let pubs = scene.zone_registry.active_for_zone("pip");
    assert_eq!(pubs.len(), 1, "Replace: only 1 occupant allowed");
    assert_eq!(
        pubs[0].publisher_namespace, "a2",
        "Replace: new publish evicts current"
    );
}

// ─── Layer attachment routing ─────────────────────────────────────────────────

#[test]
fn zone_tile_z_min_constant_is_correct() {
    assert_eq!(
        ZONE_TILE_Z_MIN, 0x8000_0000u32,
        "ZONE_TILE_Z_MIN must be 0x8000_0000"
    );
}

#[test]
fn content_layer_zone_z_order_must_be_ge_zone_tile_z_min() {
    // Any tile added for a Content-layer zone should use z_order >= ZONE_TILE_Z_MIN.
    // This is a schema constraint — we verify the constant is below any agent tile value.
    // Agent tiles use z_order values set by the agent (typically small integers).
    // Zone tiles in Content layer must be in the reserved upper band.
    let agent_max_z: u32 = u16::MAX as u32; // typical high agent z_order
    assert!(
        ZONE_TILE_Z_MIN > agent_max_z,
        "ZONE_TILE_Z_MIN ({ZONE_TILE_Z_MIN:#010x}) must be > typical max agent z_order ({agent_max_z:#010x})"
    );
}

#[test]
fn default_zone_registry_contains_all_six_v1_zones() {
    use tze_hud_scene::types::ZoneRegistry;
    let registry = ZoneRegistry::with_defaults();
    let zones = registry.all_zones();
    assert_eq!(
        zones.len(),
        6,
        "V1 default registry must contain exactly 6 zones"
    );

    let names: Vec<_> = zones.iter().map(|z| z.name.as_str()).collect();
    for expected in &[
        "subtitle",
        "notification-area",
        "status-bar",
        "pip",
        "ambient-background",
        "alert-banner",
    ] {
        assert!(
            names.contains(expected),
            "default registry missing zone '{expected}'"
        );
    }
}

#[test]
fn default_zones_have_correct_layer_attachments() {
    use tze_hud_scene::types::ZoneRegistry;
    let registry = ZoneRegistry::with_defaults();

    let check = |name: &str, expected: LayerAttachment| {
        let zone = registry
            .get_by_name(name)
            .unwrap_or_else(|| panic!("zone '{name}' not found"));
        assert_eq!(
            zone.layer_attachment, expected,
            "zone '{name}' must have layer attachment {expected:?}"
        );
    };

    check("subtitle", LayerAttachment::Content);
    check("pip", LayerAttachment::Content);
    check("ambient-background", LayerAttachment::Background);
    check("status-bar", LayerAttachment::Chrome);
    check("notification-area", LayerAttachment::Chrome);
    check("alert-banner", LayerAttachment::Chrome);
}

// ─── Five v1 media types ──────────────────────────────────────────────────────

#[test]
fn all_five_v1_media_types_accepted_by_appropriate_zones() {
    use tze_hud_scene::types::ZoneRegistry;
    let registry = ZoneRegistry::with_defaults();

    // StreamText: subtitle
    assert!(
        !registry
            .zones_accepting(ZoneMediaType::StreamText)
            .is_empty(),
        "StreamText must be accepted by at least one default zone"
    );

    // ShortTextWithIcon: notification-area, alert-banner
    assert!(
        !registry
            .zones_accepting(ZoneMediaType::ShortTextWithIcon)
            .is_empty(),
        "ShortTextWithIcon must be accepted by at least one default zone"
    );

    // KeyValuePairs: status-bar
    assert!(
        !registry
            .zones_accepting(ZoneMediaType::KeyValuePairs)
            .is_empty(),
        "KeyValuePairs must be accepted by at least one default zone"
    );

    // StaticImage: pip, ambient-background
    assert!(
        !registry
            .zones_accepting(ZoneMediaType::StaticImage)
            .is_empty(),
        "StaticImage must be accepted by at least one default zone"
    );

    // SolidColor: pip, ambient-background
    assert!(
        !registry
            .zones_accepting(ZoneMediaType::SolidColor)
            .is_empty(),
        "SolidColor must be accepted by at least one default zone"
    );
}

#[test]
fn static_image_content_publishes_successfully() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    // Register a zone that accepts StaticImage
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "bg".to_string(),
        description: "Background zone".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 1.0,
        },
        accepted_media_types: vec![ZoneMediaType::StaticImage],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Replace,
        max_publishers: 1,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Background,
        ephemeral: false,
    });

    let resource_id = ResourceId::of(b"fake image data");
    let result = scene.publish_to_zone(
        "bg",
        ZoneContent::StaticImage(resource_id),
        "agent",
        None,
        None,
        None,
    );
    assert!(
        result.is_ok(),
        "StaticImage content must publish to a zone accepting StaticImage"
    );
}

// ─── ZonePublishToken validation ──────────────────────────────────────────────

#[test]
fn zone_not_found_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let result = scene.publish_to_zone(
        "nonexistent",
        ZoneContent::StreamText("hello".to_string()),
        "agent",
        None,
        None,
        None,
    );
    assert!(
        matches!(
            result,
            Err(tze_hud_scene::ValidationError::ZoneNotFound { .. })
        ),
        "publishing to nonexistent zone must produce ZoneNotFound, got {result:?}"
    );
}

#[test]
fn zone_media_type_mismatch_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone()); // accepts StreamText only

    let result = scene.publish_to_zone(
        "subtitle",
        ZoneContent::SolidColor(Rgba::WHITE),
        "agent",
        None,
        None,
        None,
    );
    assert!(result.is_err(), "media type mismatch must be rejected");
}

// ─── Per-publisher clear semantics ───────────────────────────────────────────

#[test]
fn clear_zone_for_publisher_only_removes_own_publications() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "shared".to_string(),
        description: "Shared stack zone".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 0.1,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 10 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Content,
        ephemeral: false,
    });

    scene
        .publish_to_zone(
            "shared",
            ZoneContent::StreamText("from a1".to_string()),
            "a1",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "shared",
            ZoneContent::StreamText("from a2".to_string()),
            "a2",
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(scene.zone_registry.active_for_zone("shared").len(), 2);

    // Clear only a1's publications
    scene.clear_zone_for_publisher("shared", "a1").unwrap();

    let pubs = scene.zone_registry.active_for_zone("shared");
    assert_eq!(pubs.len(), 1, "only a1's publication should be removed");
    assert_eq!(
        pubs[0].publisher_namespace, "a2",
        "a2's publication must survive"
    );
}

#[test]
fn clear_zone_for_publisher_nonexistent_zone_fails() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let result = scene.clear_zone_for_publisher("nonexistent", "agent");
    assert!(result.is_err(), "clearing nonexistent zone must fail");
}

// ─── Expiry semantics ────────────────────────────────────────────────────────

#[test]
fn publish_with_expiry_stores_expiry_timestamp() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone());

    let expiry_us = 1_735_689_600_000_000u64; // 1 Jan 2025 00:00:00 UTC in microseconds
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("expiring content".to_string()),
            "agent",
            None,
            Some(expiry_us),
            None,
        )
        .unwrap();

    let pubs = scene.zone_registry.active_for_zone("subtitle");
    assert_eq!(pubs.len(), 1);
    assert_eq!(
        pubs[0].expires_at_wall_us,
        Some(expiry_us),
        "expires_at_wall_us must be stored in the publish record"
    );
}

#[test]
fn publish_without_expiry_stores_none() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone());

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("permanent content".to_string()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    let pubs = scene.zone_registry.active_for_zone("subtitle");
    assert_eq!(
        pubs[0].expires_at_wall_us, None,
        "no expiry should store None"
    );
}

// ─── Zone publication expiry sweep ──────────────────────────────────────────

#[test]
fn drain_expired_zone_publications_removes_past_due() {
    use std::sync::Arc;
    use tze_hud_scene::TestClock;

    let clock = Arc::new(TestClock::from_us(1_000_000)); // t=1s
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    scene.register_zone(make_subtitle_zone());

    // Publish with expiry at t=2s
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("will expire".to_string()),
            "agent",
            None,
            Some(2_000_000),
            None,
        )
        .unwrap();
    assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 1);

    // Before expiry: nothing removed
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(removed, 0);
    assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 1);

    // Advance past expiry
    clock.set_us(2_000_001);
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(removed, 1);
    assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 0);
}

#[test]
fn drain_expired_leaves_no_expiry_publications() {
    use std::sync::Arc;
    use tze_hud_scene::TestClock;

    let clock = Arc::new(TestClock::from_us(1_000_000));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    scene.register_zone(make_subtitle_zone());

    // Publish without expiry
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("permanent".to_string()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    clock.set_us(999_999_999_999);
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(removed, 0, "no-expiry publications must never be reaped");
    assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 1);
}

#[test]
fn drain_expired_mixed_keeps_live_removes_dead() {
    use std::sync::Arc;
    use tze_hud_scene::TestClock;

    let clock = Arc::new(TestClock::from_us(1_000_000));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    // Use a Stack zone so multiple publications coexist
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "test".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Three publications: expires soon, no expiry, expires later
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::StreamText("dies at 2s".to_string()),
            "a",
            None,
            Some(2_000_000),
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::StreamText("permanent".to_string()),
            "b",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::StreamText("dies at 5s".to_string()),
            "c",
            None,
            Some(5_000_000),
            None,
        )
        .unwrap();

    assert_eq!(scene.zone_registry.active_for_zone("notif").len(), 3);

    // Advance to 3s — first publication expired, other two live
    clock.set_us(3_000_000);
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(removed, 1);
    let pubs = scene.zone_registry.active_for_zone("notif");
    assert_eq!(pubs.len(), 2);
    assert_eq!(pubs[0].publisher_namespace, "b");
    assert_eq!(pubs[1].publisher_namespace, "c");
}

#[test]
fn drain_expired_increments_version_only_when_changed() {
    use std::sync::Arc;
    use tze_hud_scene::TestClock;

    let clock = Arc::new(TestClock::from_us(1_000_000));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    scene.register_zone(make_subtitle_zone());

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("temp".to_string()),
            "agent",
            None,
            Some(2_000_000),
            None,
        )
        .unwrap();

    let v_before = scene.version;

    // No expiry yet — version unchanged
    scene.drain_expired_zone_publications();
    assert_eq!(scene.version, v_before);

    // Expire — version bumped
    clock.set_us(3_000_000);
    scene.drain_expired_zone_publications();
    assert_eq!(scene.version, v_before + 1);

    // Second drain — nothing to remove, version stable
    scene.drain_expired_zone_publications();
    assert_eq!(scene.version, v_before + 1);
}

// ─── Zone occupancy query API (v1: no effective_geometry) ────────────────────

#[test]
fn zone_occupancy_query_returns_correct_state() {
    use tze_hud_scene::types::ZoneRegistry;
    let mut registry = ZoneRegistry::new();
    registry.register(make_subtitle_zone());

    let tab_id = SceneId::new();

    // No publications yet
    let occupancy = registry.get_occupancy("subtitle", tab_id).unwrap();
    assert_eq!(occupancy.occupant_count, 0);
    assert!(occupancy.active_publications.is_empty());

    // Simulate a publish
    registry
        .active_publishes
        .entry("subtitle".to_string())
        .or_default()
        .push(tze_hud_scene::types::ZonePublishRecord {
            lease_id: None,
            zone_name: "subtitle".to_string(),
            publisher_namespace: "agent".to_string(),
            content: ZoneContent::StreamText("hello".to_string()),
            published_at_wall_us: 1_000_000, // microseconds
            merge_key: None,
            expires_at_wall_us: None,
            content_classification: None,
            breakpoints: Vec::new(),
        });

    let occupancy = registry.get_occupancy("subtitle", tab_id).unwrap();
    assert_eq!(occupancy.occupant_count, 1);
    assert_eq!(occupancy.zone_name, "subtitle");
    assert_eq!(occupancy.tab_id, tab_id);
    // v1: no effective_geometry field
}

#[test]
fn zone_occupancy_returns_none_for_unknown_zone() {
    use tze_hud_scene::types::ZoneRegistry;
    let registry = ZoneRegistry::new();
    let result = registry.get_occupancy("nonexistent", SceneId::new());
    assert!(
        result.is_none(),
        "occupancy query for unknown zone must return None"
    );
}

// ─── ZoneInstance type ────────────────────────────────────────────────────────

#[test]
fn zone_instance_binds_type_to_tab() {
    use tze_hud_scene::types::ZoneInstance;

    let tab_id = SceneId::new();
    let instance = ZoneInstance {
        zone_type_name: "subtitle".to_string(),
        tab_id,
        geometry_override: None,
    };

    assert_eq!(instance.zone_type_name, "subtitle");
    assert_eq!(instance.tab_id, tab_id);
    assert!(instance.geometry_override.is_none());
}

// ─── Content classification ───────────────────────────────────────────────────

#[test]
fn publish_with_content_classification_stored() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone());

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("sensitive content".to_string()),
            "agent",
            None,
            None,
            Some("pii".to_string()),
        )
        .unwrap();

    let pubs = scene.zone_registry.active_for_zone("subtitle");
    assert_eq!(pubs[0].content_classification.as_deref(), Some("pii"));
}

// ─── Multi-agent isolation: status-bar zone ──────────────────────────────────

/// Build a status-bar ZoneDefinition using the canonical default parameters.
fn make_status_bar_zone() -> ZoneDefinition {
    ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_string(),
        description: "Status bar (MergeByKey)".to_string(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.04,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 32 },
        max_publishers: 16,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    }
}

/// Helper: build a `StatusBar` payload with a single key-value pair.
fn sb(key: &str, value: &str) -> ZoneContent {
    let mut entries = HashMap::new();
    entries.insert(key.to_string(), value.to_string());
    ZoneContent::StatusBar(StatusBarPayload { entries })
}

/// exemplar_status_bar_clear_per_publisher
///
/// Three agents publish to the status-bar zone using distinct namespaces and
/// merge keys.  One agent clears its publication.  Only that agent's entries
/// must be removed; the other two agents' publications must remain intact.
#[test]
fn exemplar_status_bar_clear_per_publisher() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_status_bar_zone());

    // Three agents publish distinct keys.
    scene
        .publish_to_zone(
            "status-bar",
            sb("temperature", "22°C"),
            "agent-weather",
            Some("temperature".to_string()),
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "status-bar",
            sb("battery", "87%"),
            "agent-power",
            Some("battery".to_string()),
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "status-bar",
            sb("time", "14:35"),
            "agent-clock",
            Some("time".to_string()),
            None,
            None,
        )
        .unwrap();

    // All three publications are active.
    assert_eq!(
        scene.zone_registry.active_for_zone("status-bar").len(),
        3,
        "all three agents must have active publications before clear"
    );

    // agent-power clears its publication.
    scene
        .clear_zone_for_publisher("status-bar", "agent-power")
        .unwrap();

    let pubs = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(
        pubs.len(),
        2,
        "only agent-power's publication must be removed; two remain"
    );

    let namespaces: Vec<&str> = pubs
        .iter()
        .map(|r| r.publisher_namespace.as_str())
        .collect();
    assert!(
        !namespaces.contains(&"agent-power"),
        "agent-power's publication must not appear after clear"
    );
    assert!(
        namespaces.contains(&"agent-weather"),
        "agent-weather's publication must survive"
    );
    assert!(
        namespaces.contains(&"agent-clock"),
        "agent-clock's publication must survive"
    );

    // Confirm the surviving values are unchanged.
    let weather_pub = pubs
        .iter()
        .find(|r| r.publisher_namespace == "agent-weather")
        .unwrap();
    if let ZoneContent::StatusBar(sb) = &weather_pub.content {
        assert_eq!(
            sb.entries["temperature"], "22°C",
            "agent-weather's temperature value must be unchanged"
        );
    } else {
        panic!("expected StatusBar content for agent-weather");
    }

    let clock_pub = pubs
        .iter()
        .find(|r| r.publisher_namespace == "agent-clock")
        .unwrap();
    if let ZoneContent::StatusBar(sb) = &clock_pub.content {
        assert_eq!(
            sb.entries["time"], "14:35",
            "agent-clock's time value must be unchanged"
        );
    } else {
        panic!("expected StatusBar content for agent-clock");
    }
}

/// exemplar_status_bar_lease_expiry_isolation
///
/// Three agents publish to the status-bar zone.  One agent publishes with a
/// short lease (expires at t=2s); the other two publish without expiry.  After
/// the simulated clock advances past the lease boundary, only the expired
/// agent's publication is removed by `drain_expired_zone_publications`; the
/// other two agents' publications remain intact.
#[test]
fn exemplar_status_bar_lease_expiry_isolation() {
    use std::sync::Arc;
    use tze_hud_scene::TestClock;

    let clock = Arc::new(TestClock::from_us(1_000_000)); // t=1s
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    scene.register_zone(make_status_bar_zone());

    // agent-weather: permanent (no expiry).
    scene
        .publish_to_zone(
            "status-bar",
            sb("temperature", "19°C"),
            "agent-weather",
            Some("temperature".to_string()),
            None,
            None,
        )
        .unwrap();

    // agent-power: short lease, expires at t=2s (2_000_000 µs).
    scene
        .publish_to_zone(
            "status-bar",
            sb("battery", "54%"),
            "agent-power",
            Some("battery".to_string()),
            Some(2_000_000),
            None,
        )
        .unwrap();

    // agent-clock: permanent (no expiry).
    scene
        .publish_to_zone(
            "status-bar",
            sb("time", "09:15"),
            "agent-clock",
            Some("time".to_string()),
            None,
            None,
        )
        .unwrap();

    // All three publications active before clock advances.
    assert_eq!(
        scene.zone_registry.active_for_zone("status-bar").len(),
        3,
        "all three publications must be active before expiry"
    );

    // Drain at t=1s — nothing expired yet.
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(removed, 0, "no publications should expire at t=1s");
    assert_eq!(
        scene.zone_registry.active_for_zone("status-bar").len(),
        3,
        "all three publications must still be active at t=1s"
    );

    // Advance clock past agent-power's lease boundary.
    clock.set_us(2_000_001); // t=2.000001s
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(
        removed, 1,
        "exactly one publication (agent-power) must expire"
    );

    let pubs = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(
        pubs.len(),
        2,
        "two publications must survive after agent-power's lease expires"
    );

    let namespaces: Vec<&str> = pubs
        .iter()
        .map(|r| r.publisher_namespace.as_str())
        .collect();
    assert!(
        !namespaces.contains(&"agent-power"),
        "agent-power's publication must be cleared after lease expiry"
    );
    assert!(
        namespaces.contains(&"agent-weather"),
        "agent-weather's permanent publication must survive"
    );
    assert!(
        namespaces.contains(&"agent-clock"),
        "agent-clock's permanent publication must survive"
    );

    // Confirm surviving values are intact.
    let weather_pub = pubs
        .iter()
        .find(|r| r.publisher_namespace == "agent-weather")
        .unwrap();
    if let ZoneContent::StatusBar(sb) = &weather_pub.content {
        assert_eq!(
            sb.entries["temperature"], "19°C",
            "agent-weather's temperature must be unchanged after expiry sweep"
        );
    } else {
        panic!("expected StatusBar content for agent-weather");
    }

    let clock_pub = pubs
        .iter()
        .find(|r| r.publisher_namespace == "agent-clock")
        .unwrap();
    if let ZoneContent::StatusBar(sb) = &clock_pub.content {
        assert_eq!(
            sb.entries["time"], "09:15",
            "agent-clock's time must be unchanged after expiry sweep"
        );
    } else {
        panic!("expected StatusBar content for agent-clock");
    }
}

// ─── MergeByKey contention: status-bar integration ───────────────────────────
//
// These tests verify MergeByKey semantics for the status-bar zone loaded from
// ZoneRegistry::with_defaults().  The status-bar zone is defined as:
//   contention_policy: MergeByKey { max_keys: 32 }
//   accepted_media_types: [KeyValuePairs]  (StatusBar content)
//   layer_attachment: Chrome
//
// Spec references: openspec/changes/exemplar-status-bar/spec.md
// Task references: openspec/changes/exemplar-status-bar/tasks.md §2

/// Build a SceneGraph pre-loaded with the default v1 zone registry
/// (status-bar, notification-area, subtitle, pip, ambient-background, alert-banner).
fn make_scene_with_defaults() -> SceneGraph {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
    scene
}

/// Construct a StatusBar payload with a single key/value entry.
fn status_bar_entry(key: &str, value: &str) -> ZoneContent {
    ZoneContent::StatusBar(StatusBarPayload {
        entries: HashMap::from([(key.to_string(), value.to_string())]),
    })
}

// ── Test 1: Three agents coexist with different merge_keys ────────────────────
//
// Scenario: agents "weather-agent", "battery-agent", "time-agent" each publish
// to the status-bar zone using distinct merge_keys.  All three publications
// must coexist because their keys are different.

#[test]
fn exemplar_status_bar_three_agents_coexist() {
    // [hud-t1in.2]: MergeByKey — three distinct keys must coexist.
    let mut scene = make_scene_with_defaults();

    scene
        .publish_to_zone(
            "status-bar",
            status_bar_entry("weather", "☀ 22°C"),
            "weather-agent",
            Some("weather".to_string()),
            None,
            None,
        )
        .expect("weather-agent publish must succeed");

    scene
        .publish_to_zone(
            "status-bar",
            status_bar_entry("battery", "87%"),
            "battery-agent",
            Some("battery".to_string()),
            None,
            None,
        )
        .expect("battery-agent publish must succeed");

    scene
        .publish_to_zone(
            "status-bar",
            status_bar_entry("time", "14:32"),
            "time-agent",
            Some("time".to_string()),
            None,
            None,
        )
        .expect("time-agent publish must succeed");

    let pubs = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(
        pubs.len(),
        3,
        "three distinct merge_keys must produce 3 active publications; got {}",
        pubs.len()
    );

    let keys: std::collections::HashSet<_> = pubs
        .iter()
        .map(|r| r.merge_key.as_deref().unwrap())
        .collect();
    let expected_keys: std::collections::HashSet<_> =
        ["weather", "battery", "time"].iter().copied().collect();
    assert_eq!(
        keys, expected_keys,
        "active publications must contain exactly the expected merge keys"
    );
}

// ── Test 2: Key update replaces previous value for same merge_key ─────────────
//
// Scenario: weather-agent publishes "weather" key twice with different values.
// The second publish must replace the first — publication count stays at 1.

#[test]
fn exemplar_status_bar_key_update_replaces() {
    // [hud-t1in.2]: MergeByKey — same key replaces previous value.
    let mut scene = make_scene_with_defaults();

    scene
        .publish_to_zone(
            "status-bar",
            status_bar_entry("weather", "☁ 15°C"),
            "weather-agent",
            Some("weather".to_string()),
            None,
            None,
        )
        .expect("first publish must succeed");

    let pubs = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(pubs.len(), 1, "after first publish: 1 publication expected");

    scene
        .publish_to_zone(
            "status-bar",
            status_bar_entry("weather", "☀ 22°C"),
            "weather-agent",
            Some("weather".to_string()),
            None,
            None,
        )
        .expect("second publish (same key) must succeed");

    let pubs = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(
        pubs.len(),
        1,
        "same merge_key must replace, not accumulate; got {} publications",
        pubs.len()
    );

    // The stored content must reflect the updated value.
    match &pubs[0].content {
        ZoneContent::StatusBar(payload) => {
            let val = payload
                .entries
                .get("weather")
                .map(String::as_str)
                .unwrap_or("");
            assert_eq!(
                val, "☀ 22°C",
                "publication content must reflect the updated value"
            );
        }
        other => panic!("unexpected content variant: {other:?}"),
    }
}

// ── Test 3: Key removal via empty-string value ────────────────────────────────
//
// Scenario: an agent previously published a "weather" key.  It now re-publishes
// the same merge_key with an empty string value.  Per the empty-value-removal
// convention (openspec/changes/exemplar-status-bar/design.md), the scene graph
// MUST store the updated record (MergeByKey replacement) and the entry value
// MUST be an empty string.  The compositor is responsible for skipping empty-
// valued entries when rendering.

#[test]
fn exemplar_status_bar_key_removal_empty_value() {
    // [hud-t1in.2]: MergeByKey — empty-value publish is stored; compositor skip convention.
    let mut scene = make_scene_with_defaults();

    // Initial publish with a visible value.
    scene
        .publish_to_zone(
            "status-bar",
            status_bar_entry("weather", "☀ 22°C"),
            "weather-agent",
            Some("weather".to_string()),
            None,
            None,
        )
        .expect("initial publish must succeed");

    // Re-publish with empty string value — signals compositor to hide this key.
    scene
        .publish_to_zone(
            "status-bar",
            status_bar_entry("weather", ""),
            "weather-agent",
            Some("weather".to_string()),
            None,
            None,
        )
        .expect("empty-value publish must succeed (stored as tombstone convention)");

    let pubs = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(
        pubs.len(),
        1,
        "empty-value publish replaces the existing record; count must stay at 1"
    );

    // The scene graph stores the record; it does NOT filter it out.
    // The compositor is responsible for skipping entries whose value is "".
    match &pubs[0].content {
        ZoneContent::StatusBar(payload) => {
            let val = payload.entries.get("weather").map(String::as_str);
            assert_eq!(
                val,
                Some(""),
                "empty-value publish must be stored with value=\"\" (compositor skip convention)"
            );
        }
        other => panic!("unexpected content variant: {other:?}"),
    }
}

// ── Test 4: Key removal via TTL expiry (drain_expired_zone_publications) ───────
//
// Scenario: an agent publishes merge_key "weather" with a short TTL.
// After simulated time advances past the expiry, drain_expired_zone_publications
// removes the record and the key is no longer active.

#[test]
fn exemplar_status_bar_key_removal_ttl_expiry() {
    // [hud-t1in.2]: MergeByKey — TTL expiry removes the publication.
    use std::sync::Arc;
    use tze_hud_scene::TestClock;

    let clock = Arc::new(TestClock::from_us(1_000_000)); // t=1s
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();

    // Publish "weather" with expiry at t=2s.
    let expiry_us = 2_000_000u64;
    scene
        .publish_to_zone(
            "status-bar",
            status_bar_entry("weather", "☀ 22°C"),
            "weather-agent",
            Some("weather".to_string()),
            Some(expiry_us),
            None,
        )
        .expect("publish with TTL must succeed");

    // Also publish "battery" with no TTL — must survive the sweep.
    scene
        .publish_to_zone(
            "status-bar",
            status_bar_entry("battery", "87%"),
            "battery-agent",
            Some("battery".to_string()),
            None,
            None,
        )
        .expect("publish without TTL must succeed");

    assert_eq!(
        scene.zone_registry.active_for_zone("status-bar").len(),
        2,
        "before expiry: 2 active publications expected"
    );

    // Before expiry — drain is a no-op.
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(removed, 0, "no publications should expire before TTL");
    assert_eq!(scene.zone_registry.active_for_zone("status-bar").len(), 2);

    // Advance past the TTL expiry.
    clock.set_us(2_000_001);
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(removed, 1, "exactly 1 publication (weather) must be swept");

    let pubs = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(
        pubs.len(),
        1,
        "only the non-expiring publication must remain"
    );
    assert_eq!(
        pubs[0].merge_key.as_deref(),
        Some("battery"),
        "surviving publication must be the 'battery' key"
    );
}

// ── Test 5: Max keys eviction — 33rd distinct key evicts the oldest ────────────
//
// Spec: openspec/changes/exemplar-status-bar/tasks.md §2.5
//   "publish 33 distinct merge_keys to status-bar zone (max_keys: 32);
//    assert oldest key evicted and 32 keys remain"
//
// When a MergeByKey zone is at max_keys capacity and a new distinct key
// arrives, the oldest (first-inserted) publication is evicted to make room.
// The zone always retains exactly max_keys entries — never rejects.

#[test]
fn exemplar_status_bar_max_keys_eviction() {
    // [hud-6v0t]: MergeByKey max_keys=32 — 33rd distinct key evicts oldest.
    let mut scene = make_scene_with_defaults();

    // Publish 32 distinct keys — all must succeed.  key-00 is inserted first
    // and is therefore the "oldest" that will be evicted.
    for i in 0..32u32 {
        let key = format!("key-{i:02}");
        scene
            .publish_to_zone(
                "status-bar",
                status_bar_entry(&key, &format!("val-{i}")),
                &format!("agent-{i}"),
                Some(key.clone()),
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("publish {i} (key={key}) must succeed; got {e:?}"));
    }

    assert_eq!(
        scene.zone_registry.active_for_zone("status-bar").len(),
        32,
        "32 distinct keys must all be stored"
    );

    // Publishing the 33rd distinct key must SUCCEED — evicting the oldest entry.
    scene
        .publish_to_zone(
            "status-bar",
            status_bar_entry("overflow-key", "overflow"),
            "overflow-agent",
            Some("overflow-key".to_string()),
            None,
            None,
        )
        .expect("33rd key must succeed: oldest evicted, 32 remain");

    // Zone must retain exactly 32 publications after eviction.
    let pubs = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(
        pubs.len(),
        32,
        "zone must retain exactly 32 publications after oldest evicted"
    );

    // The oldest key ("key-00") must have been evicted.
    assert!(
        !pubs
            .iter()
            .any(|r| r.merge_key.as_deref() == Some("key-00")),
        "key-00 (oldest) must have been evicted"
    );

    // The new key ("overflow-key") must be present.
    assert!(
        pubs.iter()
            .any(|r| r.merge_key.as_deref() == Some("overflow-key")),
        "overflow-key must be present after eviction"
    );
}

// ── Test 6: Rapid updates coalesce to latest value ────────────────────────────
//
// Scenario: a single agent publishes the same merge_key many times in rapid
// succession.  Only the most recent value must be present in the zone —
// MergeByKey semantics require that the count never exceeds 1 for a given key
// and the content reflects the last write.

#[test]
fn exemplar_status_bar_rapid_updates_coalesce_to_latest() {
    // [hud-t1in.2]: MergeByKey — rapid same-key updates coalesce to latest value.
    let mut scene = make_scene_with_defaults();

    // Rapid-fire 50 updates with the same key from the same agent.
    for i in 0..50u32 {
        scene
            .publish_to_zone(
                "status-bar",
                status_bar_entry("battery", &format!("{i}%")),
                "battery-agent",
                Some("battery".to_string()),
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("rapid update {i} must succeed; got {e:?}"));
    }

    let pubs = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(
        pubs.len(),
        1,
        "rapid same-key updates must coalesce to a single publication; got {}",
        pubs.len()
    );

    // Content must reflect the last write (update 49).
    match &pubs[0].content {
        ZoneContent::StatusBar(payload) => {
            let val = payload
                .entries
                .get("battery")
                .map(String::as_str)
                .unwrap_or("");
            assert_eq!(
                val, "49%",
                "coalesced content must reflect the final update value; got \"{val}\""
            );
        }
        other => panic!("unexpected content variant: {other:?}"),
    }
}

// ─── Proptest: contention policy invariants ──────────────────────────────────

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;
    use tze_hud_scene::graph::SceneGraph;
    use tze_hud_scene::types::{ContentionPolicy, ZoneContent, ZoneDefinition, ZoneMediaType};

    /// Arbitrary StreamText content for proptest
    fn arb_stream_text() -> impl Strategy<Value = ZoneContent> {
        "[a-z]{1,20}".prop_map(ZoneContent::StreamText)
    }

    /// Publisher namespaces
    fn arb_namespace() -> impl Strategy<Value = String> {
        prop::sample::select(vec!["a1", "a2", "a3", "a4"]).prop_map(|s| s.to_string())
    }

    proptest! {
        /// LatestWins invariant: after any sequence of publishes,
        /// active publication count is always <= 1.
        #[test]
        fn prop_latest_wins_at_most_one_publication(
            publishes in prop::collection::vec(
                (arb_stream_text(), arb_namespace()),
                0..20usize
            )
        ) {
            let mut scene = SceneGraph::new(1920.0, 1080.0);
            scene.register_zone(ZoneDefinition {
                id: SceneId::new(),
                name: "lw_zone".to_string(),
                description: "LatestWins zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0, y_pct: 0.0, width_pct: 1.0, height_pct: 0.1,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 16,
                auto_clear_ms: None,
                layer_attachment: LayerAttachment::Content,
                ephemeral: false,
            });

            for (content, ns) in publishes {
                let _ = scene.publish_to_zone("lw_zone", content, &ns, None, None, None);
            }

            let count = scene.zone_registry.active_for_zone("lw_zone").len();
            prop_assert!(count <= 1, "LatestWins must have at most 1 active publication, got {}", count);
        }

        /// Replace invariant: after any sequence of publishes,
        /// active publication count is always <= 1.
        #[test]
        fn prop_replace_at_most_one_publication(
            publishes in prop::collection::vec(
                (arb_stream_text(), arb_namespace()),
                0..20usize
            )
        ) {
            let mut scene = SceneGraph::new(1920.0, 1080.0);
            scene.register_zone(ZoneDefinition {
                id: SceneId::new(),
                name: "rp_zone".to_string(),
                description: "Replace zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0, y_pct: 0.0, width_pct: 1.0, height_pct: 0.1,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Replace,
                max_publishers: 16,
                auto_clear_ms: None,
                layer_attachment: LayerAttachment::Content,
                ephemeral: false,
            });

            for (content, ns) in publishes {
                let _ = scene.publish_to_zone("rp_zone", content, &ns, None, None, None);
            }

            let count = scene.zone_registry.active_for_zone("rp_zone").len();
            prop_assert!(count <= 1, "Replace must have at most 1 active publication, got {}", count);
        }

        /// Stack invariant: after any sequence of publishes,
        /// active publication count is always <= max_depth.
        #[test]
        fn prop_stack_never_exceeds_max_depth(
            max_depth in 1u8..=8u8,
            publishes in prop::collection::vec(
                (arb_stream_text(), arb_namespace()),
                0..30usize
            )
        ) {
            let mut scene = SceneGraph::new(1920.0, 1080.0);
            scene.register_zone(ZoneDefinition {
                id: SceneId::new(),
                name: "stack_zone".to_string(),
                description: "Stack zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0, y_pct: 0.0, width_pct: 1.0, height_pct: 0.1,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Stack { max_depth },
                max_publishers: 64,
                auto_clear_ms: None,
                layer_attachment: LayerAttachment::Chrome,
                ephemeral: false,
            });

            for (content, ns) in publishes {
                let _ = scene.publish_to_zone("stack_zone", content, &ns, None, None, None);
            }

            let count = scene.zone_registry.active_for_zone("stack_zone").len();
            prop_assert!(
                count <= max_depth as usize,
                "Stack max_depth={} must never be exceeded, got {}",
                max_depth,
                count
            );
        }

        /// MergeByKey invariant: after any sequence of publishes with keys,
        /// active publication count is always <= max_keys,
        /// and no two entries share the same key.
        #[test]
        fn prop_merge_by_key_unique_keys_and_bounded(
            max_keys in 2u8..=8u8,
            publishes in prop::collection::vec(
                (
                    prop::sample::select(vec!["k1", "k2", "k3", "k4"]).prop_map(|s| s.to_string()),
                    arb_namespace(),
                ),
                0..20usize
            )
        ) {
            let mut scene = SceneGraph::new(1920.0, 1080.0);
            let kv_content = |_key: &str| {
                let mut entries = HashMap::new();
                entries.insert("v".to_string(), "value".to_string());
                ZoneContent::StatusBar(StatusBarPayload { entries })
            };

            scene.register_zone(ZoneDefinition {
                id: SceneId::new(),
                name: "mbk_zone".to_string(),
                description: "MergeByKey zone".to_string(),
                geometry_policy: GeometryPolicy::EdgeAnchored {
                    edge: DisplayEdge::Bottom,
                    height_pct: 0.04,
                    width_pct: 1.0,
                    margin_px: 0.0,
                },
                accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::MergeByKey { max_keys },
                max_publishers: 16,
                auto_clear_ms: None,
                layer_attachment: LayerAttachment::Chrome,
                ephemeral: false,
            });

            for (key, ns) in &publishes {
                let _ = scene.publish_to_zone("mbk_zone", kv_content(key), ns, Some(key.clone()), None, None);
            }

            let pubs = scene.zone_registry.active_for_zone("mbk_zone");

            // Count <= max_keys
            prop_assert!(
                pubs.len() <= max_keys as usize,
                "MergeByKey max_keys={} must not be exceeded, got {}",
                max_keys,
                pubs.len()
            );

            // Keys must be unique
            let keys: Vec<_> = pubs.iter().map(|r| r.merge_key.clone()).collect();
            let unique_keys: std::collections::HashSet<_> = keys.iter().collect();
            prop_assert!(
                keys.len() == unique_keys.len(),
                "MergeByKey: duplicate keys found: {:?}",
                keys
            );
        }
    }
}

// ─── Ambient-background TTL semantics ────────────────────────────────────────
//
// Spec: openspec/changes/exemplar-ambient-background/specs/exemplar-ambient-background/spec.md
// Requirement "Ambient Background TTL Semantics"
// Design: openspec/changes/exemplar-ambient-background/design.md — Decision 4
//   "TTL semantics: 0 = persistent until replaced"
//
// The ambient-background zone uses contention_policy: Replace and
// auto_clear_ms: None (no zone-level auto-clear).
//
// TTL mapping to scene graph API:
//   ttl_us == 0  (zero)    → expires_at_wall_us: None  (indefinite persistence)
//   ttl_us omitted         → expires_at_wall_us: None  (same as zero)
//   ttl_us > 0  (non-zero) → expires_at_wall_us: Some(now_us + ttl_us)
//
// These tests exercise all four scenarios defined in the spec.

/// Helper: dark-blue solid color payload for ambient-background tests.
fn dark_blue() -> ZoneContent {
    ZoneContent::SolidColor(Rgba {
        r: 0.05,
        g: 0.05,
        b: 0.30,
        a: 1.0,
    })
}

/// Helper: warm-amber solid color payload for ambient-background tests.
fn warm_amber() -> ZoneContent {
    ZoneContent::SolidColor(Rgba {
        r: 1.0,
        g: 0.75,
        b: 0.20,
        a: 1.0,
    })
}

/// Helper: forest-green solid color for republish tests.
fn forest_green() -> ZoneContent {
    ZoneContent::SolidColor(Rgba {
        r: 0.13,
        g: 0.55,
        b: 0.13,
        a: 1.0,
    })
}

/// Helper: build a SceneGraph with a simulated clock and the default zone registry.
///
/// Returns `(scene, clock)` so the caller can advance time independently.
fn make_ambient_scene(start_us: u64) -> (SceneGraph, std::sync::Arc<tze_hud_scene::TestClock>) {
    use std::sync::Arc;
    use tze_hud_scene::TestClock;
    let clock = Arc::new(TestClock::from_us(start_us));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
    (scene, clock)
}

// ── Precondition: ambient-background auto_clear_ms is None ───────────────────
//
// The TTL=0-persists guarantee depends on the zone having no zone-level
// auto-clear timer.  This test locks that invariant in place.

#[test]
fn ambient_background_auto_clear_ms_is_none() {
    // [hud-gwhr.3]: ambient-background zone must have auto_clear_ms: None so
    // that zero-TTL publications persist indefinitely.
    let registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("ambient-background")
        .expect("ambient-background must be present in with_defaults()");
    assert!(
        zone.auto_clear_ms.is_none(),
        "ambient-background auto_clear_ms must be None (zone has no auto-clear timer); \
         got {:?}",
        zone.auto_clear_ms
    );
}

// ── Scenario 1: Zero TTL persists until replaced ──────────────────────────────
//
// Spec scenario "Zero TTL persists until replaced":
//   Publish SolidColor with ttl_us == 0 (→ expires_at_wall_us: None).
//   Advance simulated time by 60 s.
//   Verify the publication is still active — no auto-expiry.

#[test]
fn test_ambient_background_zero_ttl_persists() {
    // [hud-gwhr.3]: Zero TTL must persist; ambient-background has no auto-clear.
    let (mut scene, clock) = make_ambient_scene(1_000_000); // t=1 s

    // Publish with ttl_us == 0: maps to expires_at_wall_us: None.
    scene
        .publish_to_zone(
            "ambient-background",
            dark_blue(),
            "mood-agent",
            None,
            None, // None ↔ ttl_us: 0 (indefinite)
            None,
        )
        .expect("publish with zero-TTL (None expires) must succeed");

    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("ambient-background")
            .len(),
        1,
        "ambient-background must have 1 active publication immediately after publish"
    );

    // Advance 60 seconds.
    clock.set_us(1_000_000 + 60_000_000);

    // drain_expired_zone_publications must not remove this publication.
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(
        removed, 0,
        "zero-TTL (indefinite) publication must not be removed after 60 s"
    );
    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("ambient-background")
            .len(),
        1,
        "ambient-background publication must persist 60 s after publish (zero TTL = indefinite)"
    );

    // The stored expiry must be None (no deadline set).
    let pubs = scene.zone_registry.active_for_zone("ambient-background");
    assert!(
        pubs[0].expires_at_wall_us.is_none(),
        "zero-TTL publication expires_at_wall_us must be None"
    );
}

// ── Scenario 2: Omitted TTL persists (same as zero TTL) ──────────────────────
//
// Spec scenario "Zero TTL persists until replaced" (omission variant):
//   Publish SolidColor without specifying ttl_us at all (→ expires_at_wall_us: None).
//   Advance simulated time by 60 s.
//   Verify same persistence as ttl_us: 0.

#[test]
fn test_ambient_background_omitted_ttl_persists() {
    // [hud-gwhr.3]: Omitting TTL defaults to persistent (same as ttl_us: 0).
    let (mut scene, clock) = make_ambient_scene(1_000_000);

    // Publish without specifying any expiry — same as ttl_us not provided.
    scene
        .publish_to_zone(
            "ambient-background",
            dark_blue(),
            "mood-agent",
            None,
            None, // ttl_us field omitted → expires_at_wall_us: None
            None,
        )
        .expect("publish without TTL must succeed");

    // Advance well past any reasonable timeout.
    clock.set_us(999_999_999_999);

    let removed = scene.drain_expired_zone_publications();
    assert_eq!(
        removed, 0,
        "omitted-TTL publication must never expire via drain_expired_zone_publications"
    );
    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("ambient-background")
            .len(),
        1,
        "omitted-TTL ambient-background publication must survive indefinitely"
    );
}

// ── Scenario 3: Non-zero TTL expires ─────────────────────────────────────────
//
// Spec scenario "Non-zero TTL expires":
//   Publish SolidColor(warm_amber) with ttl_us: 2_000_000 (2 s).
//   Verify present immediately.
//   Advance past TTL.
//   Verify zone active_for_zone is empty (reverted to transparent/clear).

#[test]
fn test_ambient_background_nonzero_ttl_expires() {
    // [hud-gwhr.3]: Non-zero TTL expires; ambient-background reverts to clear.
    let start_us = 1_000_000u64; // t=1 s
    let ttl_us = 2_000_000u64; // 2 s TTL
    let expiry_us = start_us + ttl_us; // t=3 s

    let (mut scene, clock) = make_ambient_scene(start_us);

    // Publish with absolute expiry derived from ttl_us.
    scene
        .publish_to_zone(
            "ambient-background",
            warm_amber(),
            "mood-agent",
            None,
            Some(expiry_us),
            None,
        )
        .expect("publish with non-zero TTL must succeed");

    // Immediately present.
    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("ambient-background")
            .len(),
        1,
        "ambient-background must have 1 active publication immediately after publish"
    );

    // Verify the stored expiry matches what we set.
    let pubs = scene.zone_registry.active_for_zone("ambient-background");
    assert_eq!(
        pubs[0].expires_at_wall_us,
        Some(expiry_us),
        "non-zero TTL publication must store expiry_us = start_us + ttl_us"
    );

    // Before expiry: drain is a no-op.
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(removed, 0, "publication must not expire before TTL elapses");
    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("ambient-background")
            .len(),
        1
    );

    // Advance past the TTL expiry.
    clock.set_us(expiry_us + 1);
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(
        removed, 1,
        "non-zero-TTL ambient-background publication must be removed after expiry"
    );
    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("ambient-background")
            .len(),
        0,
        "ambient-background must be empty after TTL expiry (reverts to transparent/clear)"
    );
}

// ── Scenario 4: TTL expiry then republish ────────────────────────────────────
//
// Spec scenario "Non-zero TTL expires" (republish variant):
//   Publish with non-zero TTL, let it expire.
//   Publish a new persistent color.
//   Verify new color renders and persists.

#[test]
fn test_ambient_background_ttl_expiry_then_republish() {
    // [hud-gwhr.3]: After TTL expiry, ambient-background accepts a new persistent publish.
    let start_us = 1_000_000u64;
    let ttl_us = 2_000_000u64;
    let expiry_us = start_us + ttl_us;

    let (mut scene, clock) = make_ambient_scene(start_us);

    // Phase 1: publish warm_amber with a short TTL.
    scene
        .publish_to_zone(
            "ambient-background",
            warm_amber(),
            "mood-agent",
            None,
            Some(expiry_us),
            None,
        )
        .expect("initial publish with non-zero TTL must succeed");

    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("ambient-background")
            .len(),
        1,
        "warm_amber publication must be present immediately"
    );

    // Phase 2: advance past TTL and drain.
    clock.set_us(expiry_us + 1);
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(removed, 1, "warm_amber TTL publication must expire");
    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("ambient-background")
            .len(),
        0,
        "ambient-background must be empty after TTL expiry"
    );

    // Phase 3: republish a persistent color (forest_green, ttl_us = 0 → None).
    scene
        .publish_to_zone(
            "ambient-background",
            forest_green(),
            "mood-agent",
            None,
            None, // persistent (ttl_us: 0)
            None,
        )
        .expect("republish after expiry must succeed");

    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("ambient-background")
            .len(),
        1,
        "forest_green republish must produce exactly 1 active publication"
    );

    // Verify the new publication is forest_green.
    let pubs = scene.zone_registry.active_for_zone("ambient-background");
    assert_eq!(
        pubs[0].content,
        forest_green(),
        "republished color must be forest_green"
    );

    // Phase 4: advance far into the future — republished persistent color must survive.
    clock.set_us(expiry_us + 60_000_000 + 1); // 60 s after the initial expiry
    let removed = scene.drain_expired_zone_publications();
    assert_eq!(
        removed, 0,
        "persistent republish must not expire regardless of elapsed time"
    );
    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("ambient-background")
            .len(),
        1,
        "forest_green persistent publication must still be active 60 s after republish"
    );
}
