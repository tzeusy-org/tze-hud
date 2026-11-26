use super::*;

#[test]
fn test_alert_banner_backdrop_spans_full_display_width() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry.get_by_name("alert-banner").unwrap();
    let (x, _, width, _) = Compositor::resolve_zone_geometry(&zone.geometry_policy, 1920.0, 1080.0);
    assert_eq!((x, width), (0.0, 1920.0));
}

fn make_alert_banner_scene() -> SceneGraph {
    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert-banner severity-stack test zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.05,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            // Non-default typography so the text path must honour the policy.
            font_size_px: Some(24.0),
            font_weight: Some(700),
            margin_horizontal: Some(8.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            backdrop_opacity: Some(1.0),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 16,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
}

fn publish_alert(scene: &mut SceneGraph, text: &str, urgency: u32, publisher: &str) {
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: text.to_owned(),
                urgency,
                ..Default::default()
            }),
            publisher,
            None,
            None,
            None,
        )
        .expect("alert-banner publish must succeed");
}

#[tokio::test]
async fn test_alert_banner_stacks_by_severity_then_recency() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let assert_rendered = |scene: &SceneGraph, expected: &[&str]| {
        let mut vertices = Vec::new();
        compositor.render_zone_content(scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
        assert_eq!(
            vertices.len(),
            6 * expected.len(),
            "backdrops for {expected:?}"
        );
        if expected.len() == 3 {
            // The third quad must start below the nominal 36 px zone height.
            let first_top = vertices[0].position[1];
            let third_top = vertices[12].position[1];
            assert!(third_top < first_top && third_top < 1.0 - 2.0 * 36.0 / 720.0);
        }

        let items = compositor.collect_text_items(scene, 1280.0, 720.0);
        let order: Vec<&str> = items.iter().map(|item| item.text.as_ref()).collect();
        assert_eq!(order, expected);
        assert!(items.windows(2).all(|w| w[0].pixel_y < w[1].pixel_y));
        for item in &items {
            // Outer inset + policy margin + severity mark column + gap.
            assert_eq!(item.pixel_x, 16.0 + 8.0 + 24.0 + 12.0);
            assert_eq!((item.font_size_px, item.font_weight), (24.0, 700));
        }
    };

    let mut replace_scene = make_alert_banner_scene();
    let zone = replace_scene
        .zone_registry
        .zones
        .get_mut("alert-banner")
        .unwrap();
    zone.geometry_policy = GeometryPolicy::EdgeAnchored {
        edge: DisplayEdge::Top,
        height_pct: 0.06,
        width_pct: 1.0,
        margin_px: 0.0,
    };
    zone.rendering_policy.backdrop = Some(Rgba::new(1.0, 0.0, 0.0, 1.0));
    zone.rendering_policy.backdrop_opacity = Some(0.9);
    zone.contention_policy = ContentionPolicy::Replace;
    zone.max_publishers = 1;
    assert_rendered(&replace_scene, &[]);

    type Case = (&'static [(&'static str, u32)], &'static [&'static str]);
    let cases: [Case; 7] = [
        (&[], &[]),
        (&[("Single banner", 2)], &["Single banner"]),
        (&[("Warning", 2), ("Critical", 3)], &["Critical", "Warning"]),
        (&[("Critical", 3), ("Warning", 2)], &["Critical", "Warning"]),
        (
            &[("Info", 0), ("Warning", 2), ("Critical", 3)],
            &["Critical", "Warning", "Info"],
        ),
        (
            &[("Banner A", 1), ("Banner B", 2), ("Banner C", 3)],
            &["Banner C", "Banner B", "Banner A"],
        ),
        (
            &[("Warning A", 2), ("Warning B", 2)],
            &["Warning B", "Warning A"],
        ),
    ];
    for (published, expected) in cases {
        let mut scene = make_alert_banner_scene();
        for (i, &(text, urgency)) in published.iter().enumerate() {
            publish_alert(&mut scene, text, urgency, &format!("agent-{i}"));
        }
        assert_rendered(&scene, expected);
    }
}

/// Tonal Glass uses the same inset for the backdrop, hit card and dismiss target.
#[tokio::test]
async fn test_alert_banner_is_an_inset_card_matching_its_hit_regions() {
    let (mut compositor, _surface) = require_gpu!(make_text_compositor(1280, 720).await);
    compositor.set_token_map(token_overrides([
        ("space.alert.margin", "16"),
        ("color.on_alert", "#000000"),
        ("color.on_alert.error", "#FFFFFF"),
        ("color.severity.warning", "#00FF00"),
    ]));
    let mut scene = make_alert_banner_scene();
    scene
        .zone_registry
        .zones
        .get_mut("alert-banner")
        .unwrap()
        .rendering_policy
        .backdrop_radius = Some(10.0);
    publish_alert(&mut scene, "Disk almost full", 2, "agent-a");
    publish_alert(&mut scene, "Service down", 3, "agent-b");
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let cards: Vec<Rect> = scene
        .overlay
        .notification_geometry
        .iter()
        .map(|g| g.bounds)
        .collect();
    let [top, next] = cards.as_slice() else {
        panic!("expected two inset cards");
    };
    for card in &cards {
        assert_eq!((card.x, card.width), (16.0, 1280.0 - 32.0), "side inset");
    }
    assert_eq!(top.y, 16.0, "top inset");
    assert_eq!(next.y, top.y + top.height + 16.0, "stack gap");
    let backdrops: Vec<_> = compositor
        .collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0)
        .chrome
        .into_iter()
        .filter(|c| c.radius == 10.0)
        .map(|c| Rect::new(c.x, c.y, c.width, c.height))
        .collect();
    assert_eq!(backdrops, cards, "drawn backdrops are the hit-tested cards");
    let dismiss = scene
        .overlay
        .zone_hit_regions
        .iter()
        .filter(|r| r.kind == ZoneInteractionKind::Dismiss);
    assert_eq!(dismiss.clone().count(), cards.len());
    for (g, region) in scene.overlay.notification_geometry.iter().zip(dismiss) {
        assert_eq!(region.bounds, g.close, "dismiss hit region is the drawn ×");
        assert!(g.close.x + g.close.width <= g.bounds.x + g.bounds.width);
    }

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let item = |text: &str| {
        items
            .iter()
            .find(|i| i.text.as_ref() == text)
            .unwrap_or_else(|| panic!("no text item {text:?}"))
    };
    let warning = item("Disk almost full");
    assert_eq!(warning.color[..3], [0, 0, 0]);
    assert_eq!(item("Service down").color[..3], [255, 255, 255]);
    assert!(warning.outline_color.is_none());
    let mark = items
        .iter()
        .find(|i| i.text.as_ref() == "!" && i.color[..3] == [0, 255, 0])
        .expect("caution severity mark glyph");
    assert!(mark.pixel_x < warning.pixel_x);

    // Frame priming and inset drawing must retain the same live-font action
    // descriptor. Include flush Classic geometry and custom outer insets.
    for margin in [0, 16, 32] {
        compositor.set_token_map(HashMap::from([(
            "space.alert.margin".to_owned(),
            margin.to_string(),
        )]));
        let mut scene = make_alert_banner_scene();
        scene
            .publish_to_zone(
                "alert-banner",
                ZoneContent::Notification(NotificationPayload {
                    text: "Choose a response".into(),
                    actions: ["Inspect the service", "Keep monitoring"]
                        .into_iter()
                        .map(|label| NotificationAction {
                            copy: None,
                            label: label.into(),
                            callback_id: Some(label.into()),
                            url: None,
                            role: None,
                        })
                        .collect(),
                    ..Default::default()
                }),
                "action-agent",
                None,
                None,
                None,
            )
            .unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        let _first = compositor.build_windowed_frame(&mut scene, 1280, 720);
        let shapes = compositor.notification_layouts.borrow().shapes;
        assert!(shapes > 0, "the action fixture must shape labels");
        let _second = compositor.build_windowed_frame(&mut scene, 1280, 720);
        assert_eq!(
            compositor.notification_layouts.borrow().shapes,
            shapes,
            "unchanged inset banner actions retain their primed layout: margin={margin}"
        );
    }
}
