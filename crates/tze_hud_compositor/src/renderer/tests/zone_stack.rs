use super::*;

// ── Multi-publication rendering: Stack and MergeByKey policies ──────────

/// Stack zone with two notifications: render_zone_content must emit a
/// separate backdrop quad for each publication, stacked vertically.
/// With max_depth=4 and zone height=400px, each slot is 100px tall.
/// Two publications → two quads; the second quad starts at y+100.
#[tokio::test]
async fn test_stack_zone_renders_separate_backdrop_per_publication() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone: top-right, 200×400 px via Relative.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "stack zone for multi-pub test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,    // 320 px at 1280 wide
            height_pct: 0.5556, // ~400 px at 720 tall (≈400/720)
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            backdrop_opacity: Some(0.9),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 4 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish two separate notifications.
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "First notification".to_owned(),
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
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Second notification".to_owned(),
                icon: String::new(),
                urgency: 1,
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

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // Two publications → two backdrop quads (6 verts each) + border quads.
    // Each Notification slot emits:
    //   1 backdrop quad (6) + 4 border quads (24) + 4 dismiss-button border quads (24) = 54.
    // Total: 2 × 54 = 108 vertices.
    // We assert at least 12 (2 backdrops) and a multiple of 6.
    assert!(
        vertices.len() >= 12,
        "Stack zone with 2 publications must emit at least 12 vertices (2 backdrop quads), got {}",
        vertices.len()
    );
    assert_eq!(
        vertices.len() % 6,
        0,
        "vertex count must be a multiple of 6 (each quad = 6 vertices), got {}",
        vertices.len()
    );

    // The first backdrop quad's top-left y should be 0 (zone starts at y_pct=0.0 → y=0).
    // The second backdrop quad's top-left y should be ~slot_h after the first slot.
    // zone_h = 720 * 0.5556 ≈ 400; slot_h is content-sized per stack_slot_height.
    // Vertices are in NDC; we check the first and Nth vertex y values differ.
    // rect_vertices emits 6 verts per quad in positions [x,y] NDC.
    // Each notification slot emits:
    //   6  backdrop quad vertices
    //   24 border quads (4 quads × 6 verts each)
    //   24 dismiss-button border quads (4 quads × 6 verts each)
    //   = 54 vertices per slot.
    // The second backdrop quad therefore starts at vertex index 54.
    let first_quad_y = vertices[0].position[1];
    // Find second backdrop by skipping first slot (54 vertices: 6 backdrop + 24 border + 24 dismiss border).
    let second_quad_idx = 54; // 6 backdrop + 4 border quads + 4 dismiss-button border quads (6 each)
    if vertices.len() > second_quad_idx {
        let second_quad_y = vertices[second_quad_idx].position[1];
        assert!(
            (first_quad_y - second_quad_y).abs() > 0.01,
            "second Stack slot must start at a different y than the first; got first={first_quad_y:.4}, second={second_quad_y:.4}"
        );
    }
}

/// Stack zone: collect_text_items must produce a separate TextItem for
/// each publication, with each item positioned in its own vertical slot.
#[tokio::test]
async fn test_stack_zone_collect_text_items_per_publication() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "stack zone text items test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5556,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 4 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Alpha alert".to_owned(),
                icon: String::new(),
                urgency: 0,
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
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Beta alert".to_owned(),
                icon: String::new(),
                urgency: 1,
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
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Gamma alert".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-c",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // Three publications in a Stack zone must produce three TextItems.
    assert_eq!(
        items.len(),
        3,
        "Stack zone with 3 publications must produce 3 TextItems, got {}",
        items.len()
    );

    // Items should be ordered newest-first (slot 0 = newest at top of zone).
    assert!(
        items[0].text.contains("Gamma"),
        "first TextItem should be the newest publication (Gamma), got: {}",
        items[0].text
    );
    assert!(
        items[1].text.contains("Beta"),
        "second TextItem should be Beta, got: {}",
        items[1].text
    );
    assert!(
        items[2].text.contains("Alpha"),
        "third TextItem should be the oldest publication (Alpha), got: {}",
        items[2].text
    );

    // Each item should occupy a different vertical slot; slot 0 is at top
    // (lowest y), slot 1 below it, slot 2 below that.
    assert!(
        items[1].pixel_y > items[0].pixel_y,
        "slot 1 y ({}) must be below slot 0 y ({})",
        items[1].pixel_y,
        items[0].pixel_y
    );
    assert!(
        items[2].pixel_y > items[1].pixel_y,
        "slot 2 y ({}) must be below slot 1 y ({})",
        items[2].pixel_y,
        items[1].pixel_y
    );
}

// ── Slot layout: content-sized slots, newest-first ───────────────────────

/// 5 stacked notifications must each appear at a distinct y-position.
/// slot_height = font_size_px(16) + 18 = 34 px.
/// Zone is tall enough to accommodate all 5 slots.
/// Verifies newest-first ordering: slot 0 = newest at zone top.
#[tokio::test]
async fn test_stack_slot_layout_five_notifications_distinct_y() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone: 300px wide × 300px tall — enough for 5 × 34px slots (170px).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "slot layout test zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,    // 320 px at 1280 wide
            height_pct: 0.4167, // ~300 px at 720 tall
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 5,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish 5 notifications (oldest to newest: "N1" .. "N5").
    for i in 1..=5 {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: format!("N{i}"),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                &format!("agent-{i}"),
                None,
                None,
                None,
            )
            .unwrap();
    }

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 5, "5 notifications must produce 5 TextItems");

    // With font_size_px=16 → slot_h = 34.  margin_v defaults to 8.
    // pixel_y for slot i = zone_y + i*slot_h + margin_v.
    // Check all 5 y-values are strictly increasing (slot 0 = newest = top).
    let ys: Vec<f32> = items.iter().map(|it| it.pixel_y).collect();
    for w in ys.windows(2) {
        assert!(
            w[1] > w[0],
            "slots must have strictly increasing y; got consecutive y={:.2} then {:.2}",
            w[0],
            w[1]
        );
    }

    // Newest notification is "N5" — must be slot 0 (lowest y).
    assert!(
        items[0].text.contains("N5"),
        "slot 0 must be the newest notification (N5), got: {}",
        items[0].text
    );
    // Oldest is "N1" — must be slot 4 (highest y).
    assert!(
        items[4].text.contains("N1"),
        "slot 4 must be the oldest notification (N1), got: {}",
        items[4].text
    );
}

/// When a 6th notification is published to a Stack zone with max_depth=5,
/// the oldest notification must be evicted.  After eviction, only 5 items
/// remain and the evicted notification is absent.
#[tokio::test]
async fn test_stack_slot_sixth_notification_evicts_oldest() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "eviction test zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 6,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish 6 notifications; "oldest" is "Oldest" (first published).
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Oldest".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-0",
            None,
            None,
            None,
        )
        .unwrap();
    for i in 1..=5 {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: format!("N{i}"),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                &format!("agent-{i}"),
                None,
                None,
                None,
            )
            .unwrap();
    }

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // Only 5 items after eviction (max_depth=5).
    assert_eq!(
        items.len(),
        5,
        "after 6th publish with max_depth=5, must have 5 TextItems; got {}",
        items.len()
    );

    // "Oldest" must have been evicted — not present in any item.
    let has_oldest = items.iter().any(|it| it.text.contains("Oldest"));
    assert!(
        !has_oldest,
        "oldest notification must be evicted after 6th publish"
    );

    // "N5" (newest) must be present as slot 0.
    assert!(
        items[0].text.contains("N5"),
        "newest notification (N5) must be slot 0 after eviction, got: {}",
        items[0].text
    );
}

/// Stack notifications clip at zone boundary: slots whose top-left y is at or
/// beyond zone_bottom are fully clipped and produce no TextItem. Partial slots
/// (y < zone_bottom but y+slot_h > zone_bottom) are emitted with clamped height.
#[tokio::test]
async fn test_stack_slot_clips_at_zone_boundary() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone height: 72px at 720 tall → height_pct = 72/720 = 0.1.
    // font_size_px=16, line_height = 16*1.4 = 22.4, margin_v=8, SLOT_BASELINE_GAP=4
    // → slot_h = 22.4 + 2*8 + 4 = 42.4px.
    // slot 0 at y=0  (fits: 0+42.4=42.4 ≤ 72 → emitted).
    // slot 1 at y=42.4 (fits: 42.4+42.4=84.8 > 72 but y < 72 → emitted).
    // slot 2 at y=84.8 → y ≥ zone_bottom(72) → loop breaks.
    // Exactly 2 items (slots 0, 1) are emitted.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "clipping test zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.1, // 72 px at 720 tall
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 5,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish 4 notifications; only the 2 newest should be visible (newest-first).
    for i in 1..=4 {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: format!("M{i}"),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                &format!("agent-{i}"),
                None,
                None,
                None,
            )
            .unwrap();
    }

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // slot_h = line_height(16*1.4) + 2*margin_v(8) + SLOT_BASELINE_GAP(4) = 42.4px.
    // slot 0 at y=0:    0 < 72 → emitted.
    // slot 1 at y=42.4: 42.4 < 72 → emitted.
    // slot 2 at y=84.8: 84.8 ≥ 72 → loop breaks.
    // Exactly 2 items (slots 0, 1) are emitted.
    assert_eq!(
        items.len(),
        2,
        "with 72px zone and 42.4px slots, exactly 2 items should be emitted; got {}",
        items.len()
    );

    // The newest notification (M4) must be in slot 0.
    assert!(
        !items.is_empty() && items[0].text.contains("M4"),
        "newest notification (M4) must be slot 0 (top of zone), got: {}",
        if items.is_empty() {
            "empty"
        } else {
            &items[0].text
        }
    );
}

/// ZoneSlotLayout::iter_visible: slots whose top-left y is at or beyond
/// zone_bottom are excluded; partial slots are emitted with clamped height.
///
/// This is a GPU-free unit test pinning the shared slot-geometry computation
/// introduced in hud-qlerb. Geometry correctness is guaranteed by
/// test_stack_slot_clips_at_zone_boundary (integration) and this test (unit).
#[test]
fn test_zone_slot_layout_iter_visible_clips_and_clamps() {
    // Build a ZoneSlotLayout directly with known values.
    // Three equal-height slots of 30px each (offsets: 0, 30, 60).
    // Zone origin zy = 10.0; effective_h = 70.0 → zone_bottom = 80.0.
    //
    // slot 0: slot_y = 10+0  = 10  < 80 → emitted (effective_slot_h = min(30, 70) = 30)
    // slot 1: slot_y = 10+30 = 40  < 80 → emitted (effective_slot_h = min(30, 40) = 30)
    // slot 2: slot_y = 10+60 = 70  < 80 → emitted (effective_slot_h = min(30, 10) = 10)
    // slot 3 would be at 100 ≥ 80 → excluded (not in this layout, but cull verified)
    let layout = ZoneSlotLayout {
        ordered_indices: vec![0, 1, 2],
        slot_heights: vec![30.0, 30.0, 30.0],
        slot_offsets: vec![0.0, 30.0, 60.0],
        effective_h: 70.0,
    };

    let zy = 10.0_f32;
    let visible: Vec<(usize, f32, f32)> = layout.iter_visible(zy).collect();

    assert_eq!(
        visible.len(),
        3,
        "all 3 slots start before zone_bottom → all emitted"
    );

    // slot 0: full slot
    assert_eq!(visible[0], (0, 10.0, 30.0), "slot 0: full height");
    // slot 1: full slot
    assert_eq!(visible[1], (1, 40.0, 30.0), "slot 1: full height");
    // slot 2: clamped to remaining zone height (80 - 70 = 10)
    assert!(
        (visible[2].0 == 2)
            && (visible[2].1 - 70.0).abs() < 0.01
            && (visible[2].2 - 10.0).abs() < 0.01,
        "slot 2: clamped to 10px; got {:?}",
        visible[2]
    );

    // Verify that a slot starting exactly at zone_bottom is excluded.
    let layout_tight = ZoneSlotLayout {
        ordered_indices: vec![0, 1],
        slot_heights: vec![30.0, 30.0],
        slot_offsets: vec![0.0, 30.0],
        effective_h: 30.0, // zone_bottom = zy + 30 = 40
    };
    let tight: Vec<(usize, f32, f32)> = layout_tight.iter_visible(10.0).collect();
    // slot 0: slot_y = 10 < 40 → emitted; slot 1: slot_y = 40 ≥ 40 → excluded
    assert_eq!(
        tight.len(),
        1,
        "slot starting at zone_bottom must be excluded"
    );
    assert_eq!(tight[0].0, 0, "only slot 0 is emitted");
}

/// MergeByKey zone: collect_text_items must merge ALL StatusBar publications'
/// entries and produce a single TextItem containing all unique keys.
#[tokio::test]
async fn test_merge_by_key_zone_merges_all_status_bar_entries() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_owned(),
        description: "merge-by-key zone test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.04,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 32 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Agent A publishes "cpu" and "mem" keys.
    let mut entries_a = std::collections::HashMap::new();
    entries_a.insert("cpu".to_owned(), "45%".to_owned());
    entries_a.insert("mem".to_owned(), "8.2 GB".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload { entries: entries_a }),
            "agent-a",
            Some("cpu-mem".to_owned()),
            None,
            None,
        )
        .unwrap();

    // Agent B publishes a "net" key.
    let mut entries_b = std::collections::HashMap::new();
    entries_b.insert("net".to_owned(), "1.2 MB/s".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload { entries: entries_b }),
            "agent-b",
            Some("net".to_owned()),
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // MergeByKey must produce exactly ONE TextItem containing all merged entries.
    assert_eq!(
        items.len(),
        1,
        "MergeByKey zone must produce a single merged TextItem, got {}",
        items.len()
    );

    let text = &items[0].text;
    assert!(
        text.contains("cpu"),
        "merged text must include 'cpu' key; got: {text}"
    );
    assert!(
        text.contains("mem"),
        "merged text must include 'mem' key; got: {text}"
    );
    assert!(
        text.contains("net"),
        "merged text must include 'net' key; got: {text}"
    );
    assert!(
        text.contains("45%"),
        "merged text must include cpu value '45%'; got: {text}"
    );
    assert!(
        text.contains("1.2 MB/s"),
        "merged text must include net value '1.2 MB/s'; got: {text}"
    );
}

/// MergeByKey zone: when a key appears in multiple publications, the latest
/// value wins (last-write-wins per key semantics).
#[tokio::test]
async fn test_merge_by_key_latest_value_wins_for_duplicate_keys() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_owned(),
        description: "merge-by-key duplicate key test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.04,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 32 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // First publish: cpu = "10%"
    let mut entries_old = std::collections::HashMap::new();
    entries_old.insert("cpu".to_owned(), "10%".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload {
                entries: entries_old,
            }),
            "agent-a",
            Some("cpu".to_owned()),
            None,
            None,
        )
        .unwrap();

    // Second publish: same key "cpu" with updated value "90%"
    let mut entries_new = std::collections::HashMap::new();
    entries_new.insert("cpu".to_owned(), "90%".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload {
                entries: entries_new,
            }),
            "agent-a",
            Some("cpu".to_owned()),
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    assert_eq!(items.len(), 1, "must produce one merged TextItem");
    let text = &items[0].text;

    // The latest value "90%" must appear; "10%" must not.
    assert!(
        text.contains("90%"),
        "merged text must show latest cpu value '90%'; got: {text}"
    );
    assert!(
        !text.contains("10%"),
        "merged text must not show stale cpu value '10%'; got: {text}"
    );
}

// ── StatusBar icon layout tests [hud-x2v1.2] ─────────────────────────────

/// `key_icon_map` empty → single merged TextItem (backward-compatible).
///
/// When `key_icon_map` is empty, the existing single-TextItem newline-joined
/// behavior must be preserved unchanged.
#[tokio::test]
async fn test_status_bar_empty_key_icon_map_produces_single_text_item() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_owned(),
        description: "icon layout: empty map regression".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.04,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            text_color: Some(Rgba::WHITE),
            // key_icon_map defaults to empty HashMap via serde(default).
            ..Default::default()
        },
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 16 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    let mut entries = std::collections::HashMap::new();
    entries.insert("cpu".to_owned(), "45%".to_owned());
    entries.insert("mem".to_owned(), "8 GB".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload { entries }),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // Must still produce exactly ONE TextItem (no icon layout).
    assert_eq!(
        items.len(),
        1,
        "empty key_icon_map must produce one merged TextItem; got {}",
        items.len()
    );
    let text = &items[0].text;
    assert!(text.contains("cpu"), "text must contain 'cpu'");
    assert!(text.contains("mem"), "text must contain 'mem'");
    // Entries joined with newline (alphabetically sorted).
    assert!(
        text.contains('\n'),
        "multiple entries must be newline-separated; got: {text}"
    );
}

/// `key_icon_map` non-empty → per-entry TextItems; icon-mapped entries have
/// text `pixel_x` inset by `ICON_SIZE_PX + ICON_TEXT_GAP_PX`.
///
/// We don't use real SVG files here since tests can't rely on specific
/// filesystem paths.  Instead we verify the TextItem layout (pixel_x
/// position) produced by `status_bar_icon_text_items` directly — the icon
/// draw command path is exercised separately.
#[tokio::test]
async fn test_status_bar_key_icon_map_produces_per_entry_text_items() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut key_icon_map = std::collections::HashMap::new();
    // Map only "cpu" to an icon path; "mem" has no mapping.
    key_icon_map.insert("cpu".to_owned(), "/nonexistent/cpu.svg".to_owned());

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_owned(),
        description: "icon layout: per-entry TextItems".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            text_color: Some(Rgba::WHITE),
            font_size_px: Some(16.0),
            key_icon_map,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 16 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    let mut entries = std::collections::HashMap::new();
    entries.insert("cpu".to_owned(), "45%".to_owned());
    entries.insert("mem".to_owned(), "8 GB".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload { entries }),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // Must produce one TextItem per entry (2 entries → 2 TextItems).
    assert_eq!(
        items.len(),
        2,
        "non-empty key_icon_map must produce per-entry TextItems; got {}",
        items.len()
    );

    // Entries are sorted by key: "cpu" (row 0) then "mem" (row 1).
    // "cpu" has an SVG icon mapping, so no prefix.
    // "mem" has no SVG icon mapping, so gets emoji prefix "💾".
    let cpu_item = items.iter().find(|i| i.text.starts_with("cpu:"));
    let mem_item = items.iter().find(|i| i.text.starts_with("💾 mem:"));

    assert!(cpu_item.is_some(), "expected a TextItem for 'cpu'");
    assert!(
        mem_item.is_some(),
        "expected a TextItem for 'mem' with emoji prefix"
    );

    let cpu_item = cpu_item.unwrap();
    let mem_item = mem_item.unwrap();

    // "cpu" is icon-mapped: pixel_x must be inset by ICON_SIZE_PX + ICON_TEXT_GAP_PX
    // relative to "mem" (which has no icon).
    // Both items use from_zone_policy with x = zx + icon_inset, so:
    //   cpu.pixel_x = zx + ICON_SIZE_PX + ICON_TEXT_GAP_PX + margin_h
    //   mem.pixel_x = zx + 0 + margin_h
    // Difference should be exactly ICON_SIZE_PX + ICON_TEXT_GAP_PX (30.0).
    let icon_inset = ICON_SIZE_PX + ICON_TEXT_GAP_PX; // 24.0 + 6.0 = 30.0
    let diff = cpu_item.pixel_x - mem_item.pixel_x;
    assert!(
        (diff - icon_inset).abs() < 0.5,
        "cpu pixel_x must be inset by {icon_inset} px relative to mem; diff={diff}"
    );

    // "mem" has no icon: bounds_width must be wider than "cpu" by icon_inset.
    let width_diff = mem_item.bounds_width - cpu_item.bounds_width;
    assert!(
        (width_diff - icon_inset).abs() < 0.5,
        "mem bounds_width must be {icon_inset} px wider than cpu; diff={width_diff}"
    );
}

/// LatestWins zone still renders only one TextItem even when multiple
/// publications are present (regression guard).
#[tokio::test]
async fn test_latest_wins_zone_renders_only_latest_publication() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "latest-wins regression guard".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // LatestWins should only keep the last publication.
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Old content".to_owned()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    // With LatestWins policy the scene graph may have already replaced it,
    // but we publish again to ensure only one ends up active.
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("New content".to_owned()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // LatestWins must produce exactly one TextItem.
    assert_eq!(
        items.len(),
        1,
        "LatestWins zone must produce exactly 1 TextItem; got {}",
        items.len()
    );
    // The item should contain the latest content.
    assert!(
        items[0].text.contains("New content"),
        "LatestWins must render latest publish; got: {}",
        items[0].text
    );
}
