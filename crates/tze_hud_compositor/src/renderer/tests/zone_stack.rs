use super::*;

// ── Multi-publication rendering: Stack and MergeByKey policies ──────────

/// Stack publishes emit separate backdrops and text in their own vertical slots.
#[tokio::test]
async fn stack_draws_separate_backdrops_and_text_in_vertical_slots() {
    let (width, height) = (1280.0, 720.0);
    let (compositor, _surface) =
        require_gpu!(make_compositor_and_surface(width as u32, height as u32).await);
    let zone = "notification-area";
    let stack_scene = |cards: &[(&str, &str, u32)], backdrop_opacity| {
        let mut scene = SceneGraph::new(width, height);
        scene.register_zone(ZoneDefinition {
            rendering_policy: RenderingPolicy {
                backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
                backdrop_opacity,
                text_color: Some(Rgba::WHITE),
                ..Default::default()
            },
            contention_policy: ContentionPolicy::Stack { max_depth: 4 },
            max_publishers: 4,
            ..zone_fixture(
                zone,
                relative(0.75, 0.0, 0.25, 0.5556),
                ZoneMediaType::ShortTextWithIcon,
                LayerAttachment::Chrome,
            )
        });
        for &(publisher, text, urgency) in cards {
            publish_zone(
                &mut scene,
                zone,
                publisher,
                ZoneContent::Notification(NotificationPayload {
                    text: text.into(),
                    urgency,
                    ..Default::default()
                }),
            );
        }
        scene
    };

    let backdrop_scene = stack_scene(
        &[
            ("agent-a", "First notification", 1),
            ("agent-b", "Second notification", 1),
        ],
        Some(0.9),
    );
    let mut vertices = Vec::new();
    compositor.render_zone_content(
        &backdrop_scene,
        &mut vertices,
        &mut Vec::new(),
        width,
        height,
        None,
    );
    assert_eq!(vertices.len(), 12, "one six-vertex backdrop per card");
    // NDC y increases upward. Compare both edges of each emitted quad.
    let y_edges = |quad: &[crate::pipeline::RectVertex]| {
        let ys = quad.iter().map(|vertex| vertex.position[1]);
        (
            ys.clone().fold(f32::NEG_INFINITY, f32::max),
            ys.fold(f32::INFINITY, f32::min),
        )
    };
    let (first_top, first_bottom) = y_edges(&vertices[..6]);
    let (second_top, second_bottom) = y_edges(&vertices[6..]);
    assert!(first_top - second_top > 0.01, "distinct backdrop origins");
    assert!(
        first_top > first_bottom && first_bottom >= second_top && second_top > second_bottom,
        "two non-overlapping backdrop quads: {first_top}..{first_bottom}, {second_top}..{second_bottom}"
    );

    let text_scene = stack_scene(
        &[
            ("agent-a", "Alpha alert", 0),
            ("agent-b", "Beta alert", 1),
            ("agent-c", "Gamma alert", 2),
        ],
        None,
    );
    let items = compositor.collect_text_items(&text_scene, width, height);
    assert_eq!(items.len(), 3, "one text item per card");
    for (item, label) in items.iter().zip(["Gamma", "Beta", "Alpha"]) {
        assert!(
            item.text.contains(label),
            "expected {label}, got {}",
            item.text
        );
    }
    for pair in items.windows(2) {
        assert!(
            pair[0].pixel_y < pair[1].pixel_y,
            "newest text is above older text"
        );
    }
}

// ── Slot layout: content-sized slots, newest-first ───────────────────────

/// Stack text stays in distinct newest-first slots through keyed replacement
/// and the oldest card falls out when a sixth publication exceeds the depth.
#[tokio::test]
async fn test_stack_slot_layout_five_notifications_distinct_y() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let zone = "notification-area";
    let stack_scene = |description: &str, height_pct, max_publishers| {
        let mut scene = SceneGraph::new(1280.0, 720.0);
        scene.register_zone(ZoneDefinition {
            description: description.into(),
            rendering_policy: RenderingPolicy {
                font_size_px: Some(16.0),
                backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
                text_color: Some(Rgba::WHITE),
                ..Default::default()
            },
            contention_policy: ContentionPolicy::Stack { max_depth: 5 },
            max_publishers,
            ..zone_fixture(
                zone,
                relative(0.75, 0.0, 0.25, height_pct),
                ZoneMediaType::ShortTextWithIcon,
                LayerAttachment::Chrome,
            )
        });
        scene
    };
    let publish_numbered_cards = |scene: &mut SceneGraph| {
        for i in 1..=5 {
            publish_zone(
                scene,
                zone,
                &format!("agent-{i}"),
                ZoneContent::Notification(NotificationPayload {
                    text: format!("N{i}"),
                    urgency: 1,
                    ..Default::default()
                }),
            );
        }
    };

    let mut scene = stack_scene("slot layout test zone", 0.4167, 5);
    publish_numbered_cards(&mut scene);

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 5, "5 notifications must produce 5 TextItems");
    for pair in items.windows(2) {
        assert!(
            pair[1].pixel_y > pair[0].pixel_y,
            "slots must have strictly increasing y; got consecutive y={:.2} then {:.2}",
            pair[0].pixel_y,
            pair[1].pixel_y
        );
    }
    assert!(
        items[0].text.contains("N5"),
        "slot 0 must be the newest notification (N5), got: {}",
        items[0].text
    );
    assert!(
        items[4].text.contains("N1"),
        "slot 4 must be the oldest notification (N1), got: {}",
        items[4].text
    );

    // Give two different publishers the same key, then replace only the second.
    let old = scene.zone_registry.active_for_zone(zone).to_vec();
    scene.clear_zone(zone).unwrap();
    for (index, record) in old.into_iter().enumerate() {
        scene
            .publish_to_zone(
                zone,
                record.content,
                &record.publisher_namespace,
                matches!(index, 0 | 2).then(|| "slot".into()),
                None,
                None,
            )
            .unwrap();
    }
    let before = scene.zone_registry.active_for_zone(zone).to_vec();
    let before_items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let mut content = before[2].content.clone();
    if let ZoneContent::Notification(n) = &mut content {
        n.text = "R3".into();
    }
    scene
        .publish_to_zone(
            zone,
            content,
            &before[2].publisher_namespace,
            Some("slot".into()),
            None,
            None,
        )
        .unwrap();
    let after = scene.zone_registry.active_for_zone(zone);
    assert_eq!(after.len(), 5);
    for index in [0, 1, 3, 4] {
        assert_eq!(after[index], before[index]);
    }
    assert_ne!(after[2].publication_id, before[2].publication_id);
    let after_items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        before_items.iter().map(|i| i.pixel_y).collect::<Vec<_>>(),
        after_items.iter().map(|i| i.pixel_y).collect::<Vec<_>>()
    );
    assert!(after_items.iter().any(|i| i.text.contains("R3")));

    let mut eviction_scene = stack_scene("eviction test zone", 0.5, 6);
    publish_zone(
        &mut eviction_scene,
        zone,
        "agent-0",
        ZoneContent::Notification(NotificationPayload {
            text: "Oldest".into(),
            urgency: 0,
            ..Default::default()
        }),
    );
    publish_numbered_cards(&mut eviction_scene);
    let items = compositor.collect_text_items(&eviction_scene, 1280.0, 720.0);
    assert_eq!(
        items.len(),
        5,
        "after 6th publish with max_depth=5, must have 5 TextItems; got {}",
        items.len()
    );
    assert!(
        !items.iter().any(|it| it.text.contains("Oldest")),
        "oldest notification must be evicted after 6th publish"
    );
    assert!(
        items[0].text.contains("N5"),
        "newest notification (N5) must be slot 0 after eviction, got: {}",
        items[0].text
    );
}

/// Stack notifications stop at the zone boundary: a card is shown whole or not
/// at all, so a slot that would extend past zone_bottom (and every slot after
/// it) produces no TextItem.
#[tokio::test]
async fn test_stack_slot_clips_at_zone_boundary() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone height: 72px at 720 tall → height_pct = 72/720 = 0.1.
    // font_size_px=16, line_height = 16*1.4 = 22.4, margin_v=8, SLOT_BASELINE_GAP=4
    // → slot_h = 22.4 + 2*8 + 4 = 42.4px.
    // slot 0 at y=0  (fits: 0+42.4=42.4 ≤ 72 → emitted).
    // slot 1 at y=50.4 (42.4 + 8 px stack gap; 50.4+42.4 > 72 → not shown).
    // Exactly 1 item (slot 0) is emitted.
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

    // Publish 4 notifications; only the newest fits (newest-first).
    for i in 1..=4 {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    progress: None,
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
    // slot 0 at y=0 ends at 42.4 ≤ 72 → emitted.
    // slot 1 at y=50.4 would end at 92.8 > 72 → not shown, nor anything after.
    assert_eq!(
        items.len(),
        1,
        "with 72px zone and 42.4px slots 8px apart, only the newest card fits; got {}",
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

    // At production height, retained questions must lead the visible stack.
    // The same scene predicate must stop prioritizing answered, URL-only and
    // expired cards; newest-first remains stable within each group.
    scene
        .zone_registry
        .zones
        .get_mut("notification-area")
        .unwrap()
        .geometry_policy = GeometryPolicy::Relative {
        x_pct: 0.75,
        y_pct: 0.0,
        width_pct: 0.25,
        height_pct: 0.3,
    };
    let callback = NotificationAction {
        copy: None,
        reply: None,
        label: "Answer".into(),
        callback_id: Some("answer".into()),
        url: None,
        role: None,
    };
    let records = scene
        .zone_registry
        .active_publishes
        .get_mut("notification-area")
        .unwrap();
    for record in &mut records[..2] {
        let ZoneContent::Notification(payload) = &mut record.content else {
            panic!("notification fixture")
        };
        payload.actions = vec![callback.clone()];
    }
    let (older, newer) = (records[0].publication_id, records[1].publication_id);
    let order = |scene: &SceneGraph| {
        compositor
            .zone_slot_layout(
                scene,
                "notification-area",
                scene.zone_registry.active_for_zone("notification-area"),
                &scene.zone_registry.zones["notification-area"].rendering_policy,
                216.0,
                320.0,
            )
            .ordered_indices
    };
    assert_eq!(order(&scene), vec![1, 0, 3, 2], "newest question first");
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items[0].text.as_ref(), "M2");
    assert!(items.iter().any(|item| item.text.as_ref() == "M1"));

    scene
        .overlay
        .notification_choices
        .insert(newer, "answer".into());
    assert_eq!(
        order(&scene),
        vec![0, 3, 2, 1],
        "answered card loses priority"
    );
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items[0].text.as_ref(), "M1");
    scene
        .overlay
        .notification_choices
        .insert(older, "answer".into());
    assert_eq!(order(&scene), vec![3, 2, 1, 0], "no unanswered questions");

    scene.overlay.notification_choices.remove(&older);
    let record = &mut scene
        .zone_registry
        .active_publishes
        .get_mut("notification-area")
        .unwrap()[0];
    let ZoneContent::Notification(payload) = &mut record.content else {
        panic!("notification fixture")
    };
    payload.actions = vec![NotificationAction {
        copy: None,
        reply: None,
        label: "Open".into(),
        callback_id: None,
        url: Some("https://example.com/".into()),
        role: None,
    }];
    assert_eq!(
        order(&scene),
        vec![3, 2, 1, 0],
        "URL-only is not a question"
    );
    let expired = scene.now_wall_us().saturating_sub(1);
    let record = &mut scene
        .zone_registry
        .active_publishes
        .get_mut("notification-area")
        .unwrap()[0];
    let ZoneContent::Notification(payload) = &mut record.content else {
        panic!("notification fixture")
    };
    payload.actions = vec![callback];
    record.expires_at_wall_us = Some(expired);
    assert_eq!(
        order(&scene),
        vec![3, 2, 1, 0],
        "expired question loses priority"
    );
}

/// ZoneSlotLayout::iter_visible: only slots that fit wholly inside the zone are
/// emitted, stopping at the first that doesn't; only a first slot taller than
/// the zone is clamped.
///
/// This GPU-free unit test pins the shared slot-geometry computation.
/// Geometry correctness is guarded by
/// test_stack_slot_clips_at_zone_boundary (integration) and this test (unit).
#[test]
fn test_zone_slot_layout_iter_visible_clips_and_clamps() {
    // Build a ZoneSlotLayout directly with known values.
    // Three equal-height slots of 30px each (offsets: 0, 30, 60).
    // Zone origin zy = 10.0; effective_h = 70.0 → zone_bottom = 80.0.
    //
    // slot 0: 10..40 fits → emitted
    // slot 1: 40..70 fits → emitted
    // slot 2: 70..100 passes zone_bottom 80 → not shown
    let layout = ZoneSlotLayout {
        ordered_indices: vec![0, 1, 2],
        slot_heights: vec![30.0, 30.0, 30.0],
        slot_offsets: vec![0.0, 30.0, 60.0],
        inset: 0.0,
        effective_h: 70.0,
    };

    let zone = |zy: f32| Rect::new(0.0, zy, 100.0, 1000.0);
    let rows = |layout: &ZoneSlotLayout, zy: f32| -> Vec<(usize, f32, f32)> {
        layout
            .iter_visible(zone(zy))
            .map(|(i, c)| (i, c.y, c.height))
            .collect()
    };
    let visible = rows(&layout, 10.0);

    assert_eq!(
        visible,
        vec![(0, 10.0, 30.0), (1, 40.0, 30.0)],
        "only the two slots that fit wholly are emitted, at full height"
    );

    // A slot ending exactly at the zone bottom fits.
    let exact = ZoneSlotLayout {
        ordered_indices: vec![0, 1],
        slot_heights: vec![30.0, 40.0],
        slot_offsets: vec![0.0, 30.0],
        inset: 0.0,
        effective_h: 70.0,
    };
    assert_eq!(rows(&exact, 10.0).len(), 2, "flush slot fits");

    // A first slot taller than the zone is clamped, never hidden.
    let tall = ZoneSlotLayout {
        ordered_indices: vec![0, 1],
        slot_heights: vec![100.0, 10.0],
        slot_offsets: vec![0.0, 100.0],
        inset: 0.0,
        effective_h: 70.0,
    };
    assert_eq!(
        rows(&tall, 10.0),
        vec![(0, 10.0, 70.0)],
        "an oversized newest card is clamped to the zone"
    );

    // Verify that a slot starting exactly at zone_bottom is excluded.
    let layout_tight = ZoneSlotLayout {
        ordered_indices: vec![0, 1],
        slot_heights: vec![30.0, 30.0],
        slot_offsets: vec![0.0, 30.0],
        inset: 0.0,
        effective_h: 30.0, // zone_bottom = zy + 30 = 40
    };
    let tight = rows(&layout_tight, 10.0);
    // slot 0: slot_y = 10 < 40 → emitted; slot 1: slot_y = 40 ≥ 40 → excluded
    assert_eq!(
        tight.len(),
        1,
        "slot starting at zone_bottom must be excluded"
    );
    assert_eq!(tight[0].0, 0, "only slot 0 is emitted");

    // An inset (alert banners) narrows every slot by it on both sides.
    let inset = ZoneSlotLayout {
        inset: 16.0,
        ..layout_tight
    };
    assert_eq!(
        inset
            .iter_visible(zone(10.0))
            .next()
            .map(|(_, c)| (c.x, c.width)),
        Some((16.0, 68.0)),
        "inset slots keep clear of the zone sides"
    );
}

/// A merged status bar orders unique keys, shows replacement values, and
/// reserves space only for mapped icons.
#[tokio::test]
async fn test_merge_by_key_zone_merges_entries_updates_and_positions_icons() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let make_zone = |description: &str, height_pct, max_keys, max_publishers| {
        let mut zone = zone_fixture(
            "status-bar",
            GeometryPolicy::EdgeAnchored {
                edge: DisplayEdge::Bottom,
                height_pct,
                width_pct: 1.0,
                margin_px: 0.0,
            },
            ZoneMediaType::KeyValuePairs,
            LayerAttachment::Chrome,
        );
        zone.description = description.into();
        zone.rendering_policy.backdrop = Some(Rgba::new(0.08, 0.08, 0.08, 1.0));
        zone.rendering_policy.text_color = Some(Rgba::WHITE);
        zone.contention_policy = ContentionPolicy::MergeByKey { max_keys };
        zone.max_publishers = max_publishers;
        zone
    };
    let publish =
        |scene: &mut SceneGraph, agent, merge_key: Option<&str>, entries: &[(&str, &str)]| {
            scene
                .publish_to_zone(
                    "status-bar",
                    ZoneContent::StatusBar(StatusBarPayload {
                        entries: entries
                            .iter()
                            .map(|&(key, value)| (key.into(), value.into()))
                            .collect(),
                    }),
                    agent,
                    merge_key.map(str::to_owned),
                    None,
                    None,
                )
                .unwrap();
        };

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(make_zone("merge-by-key zone test", 0.20, 32, 8));
    publish(
        &mut scene,
        "agent-a",
        Some("cpu-mem"),
        &[("cpu", "45%"), ("mem", "8.2 GB")],
    );
    publish(&mut scene, "agent-b", Some("net"), &[("net", "1.2 MB/s")]);
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let texts: Vec<_> = items.iter().map(|item| item.text.as_ref()).collect();
    assert_eq!(texts, ["cpu", "45%", "mem", "8.2 GB", "net", "1.2 MB/s"]);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(make_zone("merge-by-key duplicate key test", 0.04, 32, 8));
    for value in ["10%", "90%"] {
        publish(&mut scene, "agent-a", Some("cpu"), &[("cpu", value)]);
    }
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 2, "one merged key/value pair");
    let text = &items[1].text;
    assert!(
        text.contains("90%"),
        "merged text must show latest cpu value '90%'; got: {text}"
    );
    assert!(
        !text.contains("10%"),
        "merged text must not show stale cpu value '10%'; got: {text}"
    );

    let mut scene = SceneGraph::new(1280.0, 720.0);
    let mut zone = make_zone("icon layout: per-entry TextItems", 0.10, 16, 4);
    zone.rendering_policy.font_size_px = Some(16.0);
    // The glyph asset is irrelevant to text layout. Only the "cpu" key has an icon.
    zone.rendering_policy.key_icon_map =
        HashMap::from([("cpu".into(), "/nonexistent/cpu.svg".into())]);
    scene.register_zone(zone);
    publish(
        &mut scene,
        "agent",
        None,
        &[("cpu", "45%"), ("mem", "8 GB")],
    );
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    assert_eq!(items.len(), 4, "one key/value pair per entry");
    assert_eq!(&*items[0].text, "cpu");
    assert_eq!(&*items[2].text, "mem", "unmapped keys do not invent emoji");
    let zone = &scene.zone_registry.zones["status-bar"];
    let records = &scene.zone_registry.active_publishes["status-bar"];
    let card = compositor
        .status_bar_card(
            "status-bar",
            records,
            zone.contention_policy,
            &zone.rendering_policy,
            Rect::new(0.0, 648.0, 1280.0, 72.0),
        )
        .unwrap();
    assert_eq!(
        card.entries[0].icon.as_deref(),
        Some("/nonexistent/cpu.svg")
    );
    assert!(card.entries[1].icon.is_none());
    assert!(
        (items[0].pixel_x - card.rect.x - card.entries[0].x - 20.0).abs() < 0.01,
        "mapped icon uses14dp plus6dp gap"
    );
    assert_eq!(items[2].pixel_x, card.rect.x + card.entries[1].x);
}

/// LatestWins zone still renders only one TextItem even when multiple
/// publications are present (regression guard).
#[tokio::test]
async fn test_latest_wins_zone_renders_only_latest_publication() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(1280, 720).await);
    let stream_zone = |name, description: &str, edge| ZoneDefinition {
        description: description.into(),
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        ..zone_fixture(
            name,
            GeometryPolicy::EdgeAnchored {
                edge,
                height_pct: 0.10,
                width_pct: 0.80,
                margin_px: 16.0,
            },
            ZoneMediaType::StreamText,
            LayerAttachment::Content,
        )
    };
    let publish = |scene: &mut SceneGraph, zone, text: &str| {
        publish_zone(scene, zone, "agent", ZoneContent::StreamText(text.into()));
    };

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(stream_zone(
        "subtitle",
        "latest-wins regression guard",
        DisplayEdge::Bottom,
    ));
    publish(&mut scene, "subtitle", "Old content");
    publish(&mut scene, "subtitle", "New content");
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        items.len(),
        1,
        "LatestWins zone must produce exactly 1 TextItem; got {}",
        items.len()
    );
    assert!(
        items[0].text.contains("New content"),
        "LatestWins must render latest publish; got: {}",
        items[0].text
    );

    // Distinct text keeps the two zones' shaping-cache controls independent.
    scene.register_zone(stream_zone(
        "secondary",
        "unchanged shaping control",
        DisplayEdge::Top,
    ));
    publish(&mut scene, "secondary", "Secondary control");

    for (replacement, expected_layout, phase) in [
        (None, 2, "warm/unchanged"),
        (None, 0, "warm/unchanged"),
        (Some("Changed content"), 1, "changed/unchanged"),
        (None, 0, "changed/unchanged"),
    ] {
        if let Some(text) = replacement {
            publish(&mut scene, "subtitle", text);
        }
        compositor.prime_markdown_cache(&scene);
        compositor.render_frame_headless(&mut scene, &surface);
        let work = compositor.take_work_counts().expect("completed zone frame");
        assert_eq!(
            (work.layout, work.raster, work.upload),
            (expected_layout, 0, 0),
            "{phase} zone work: {work:?}"
        );
    }
    assert!(
        compositor.take_work_counts().is_none(),
        "single-frame drain"
    );
}
