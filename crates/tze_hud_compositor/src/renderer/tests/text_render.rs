use super::*;

fn pixels_in_rect(pixels: &[u8], frame_width: usize, rect: Rect) -> impl Iterator<Item = &[u8]> {
    let left = rect.x as usize * 4;
    let right = (rect.x + rect.width) as usize * 4;
    pixels
        .chunks_exact(frame_width * 4)
        .skip(rect.y as usize)
        .take(rect.height as usize)
        .flat_map(move |row| row[left..right].chunks_exact(4))
}

fn bright(pixel: &[u8], threshold: u8) -> bool {
    pixel[..3].iter().all(|&channel| channel > threshold)
}

#[tokio::test]
async fn test_text_clip_overflow_stays_within_bounds() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(256, 256).await);
    let bounds = Rect::new(20.0, 16.0, 100.0, 40.0);
    let mut scene = scene_with_node(leaf(NodeData::TextMarkdown(TextMarkdownNode {
        background: Some(Rgba::BLACK),
        ..markdown_text("I\nI\nI\nI", bounds, 28.0)
    })));
    prime_text(&mut compositor, &scene);
    let pixels = render_pixels(&mut compositor, &mut scene, &surface);
    assert_eq!(pixels.len(), 256 * 256 * 4);
    let mut brightest_row = 0;
    for (y, row) in pixels.chunks_exact(256 * 4).enumerate() {
        let mut count = 0;
        for (x, pixel) in row.chunks_exact(4).enumerate() {
            if bright(pixel, 200) {
                assert!(
                    bounds.contains_point(x as f32, y as f32),
                    "glyph escapes clip at {x},{y}"
                );
                count += 1;
            }
        }
        brightest_row = brightest_row.max(count);
    }
    assert!(
        (1..40).contains(&brightest_row),
        "expected a narrow visible glyph in place of a placeholder bar, got {brightest_row} bright pixels"
    );

    let bounds = Rect::new(0.0, 0.0, 120.0, 40.0);
    let mut scene = scene_with_node(leaf(NodeData::TextMarkdown(TextMarkdownNode {
        background: Some(Rgba::new(0.1, 0.1, 0.1, 1.0)),
        overflow: TextOverflow::Ellipsis,
        ..markdown_text(
            "A very long line that definitely overflows the available width of this tile",
            bounds,
            14.0,
        )
    })));
    prime_text(&mut compositor, &scene);
    let pixels = render_pixels(&mut compositor, &mut scene, &surface);
    assert_eq!(pixels.len(), 256 * 256 * 4);
    assert!(pixels_in_rect(&pixels, 256, bounds).any(|pixel| bright(pixel, 200)));
}

#[tokio::test]
async fn test_zone_publications_render_visible_text_in_their_geometry() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(1280, 720).await);
    let cases = [
        (
            "subtitle",
            ZoneMediaType::StreamText,
            ZoneContent::StreamText("Hello Zone".into()),
            GeometryPolicy::EdgeAnchored {
                edge: DisplayEdge::Bottom,
                height_pct: 0.10,
                width_pct: 0.80,
                margin_px: 16.0,
            },
            22.0,
            Rect::new(150.0, 652.0, 980.0, 63.0),
        ),
        (
            "notification",
            ZoneMediaType::ShortTextWithIcon,
            ZoneContent::Notification(NotificationPayload {
                text: "Alert: system ready".into(),
                urgency: 1,
                ..Default::default()
            }),
            GeometryPolicy::EdgeAnchored {
                edge: DisplayEdge::Top,
                height_pct: 0.08,
                width_pct: 0.70,
                margin_px: 12.0,
            },
            20.0,
            Rect::new(200.0, 12.0, 880.0, 58.0),
        ),
        (
            "statusbar",
            ZoneMediaType::KeyValuePairs,
            ZoneContent::StatusBar(StatusBarPayload {
                entries: [
                    ("battery".into(), "95%".into()),
                    ("time".into(), "12:34".into()),
                ]
                .into(),
            }),
            GeometryPolicy::EdgeAnchored {
                edge: DisplayEdge::Top,
                height_pct: 0.05,
                width_pct: 0.80,
                margin_px: 8.0,
            },
            16.0,
            Rect::new(150.0, 10.0, 980.0, 32.0),
        ),
    ];
    for (name, media, content, geometry, font_size, sample) in cases {
        let mut scene = SceneGraph::new(1280.0, 720.0);
        scene.register_zone(ZoneDefinition {
            rendering_policy: RenderingPolicy {
                font_size_px: Some(font_size),
                backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
                ..Default::default()
            },
            ..zone_fixture(name, geometry, media, LayerAttachment::Content)
        });
        publish_zone(&mut scene, name, "test", content);
        if media == ZoneMediaType::KeyValuePairs {
            let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
            let texts: Vec<_> = items.iter().map(|item| &*item.text).collect();
            assert_eq!(texts, ["battery", "95%", "time", "12:34"]);
            assert!((8.0..72.0).contains(&items[0].pixel_y));
        }
        prime_text(&mut compositor, &scene);
        let pixels = render_pixels(&mut compositor, &mut scene, &surface);
        assert_eq!(pixels.len(), 1280 * 720 * 4);
        assert!(
            pixels_in_rect(&pixels, 1280, sample).any(|pixel| bright(pixel, 180)),
            "expected visible text in the {name} zone"
        );
    }
}

#[tokio::test]
async fn test_text_renderer_empty_scene_no_panic() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(64, 64).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(64.0, 64.0);
    prime_text(&mut compositor, &scene);
    assert_eq!(
        render_pixels(&mut compositor, &mut scene, &surface).len(),
        64 * 64 * 4
    );
}

/// Presence card labels can be shorter than one line of text.
#[tokio::test]
async fn ellipsis_labels_shorter_than_a_line_render_and_update() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(320, 112).await);
    let tile_bounds = Rect::new(0.0, 0.0, 320.0, 112.0);
    let mut scene = SceneGraph::new(tile_bounds.width, tile_bounds.height);
    let tab = scene.create_tab("card", 0).unwrap();
    let lease = scene.grant_lease("card", 60_000);
    let tile = scene
        .create_tile(tab, "card", lease, tile_bounds, 1)
        .unwrap();
    let root = solid_node(tile_bounds, Rgba::BLACK, None);
    let root_id = root.id;
    scene.set_tile_root(tile, root).unwrap();

    let label = |text: &str, bounds, size| {
        NodeData::TextMarkdown(TextMarkdownNode {
            overflow: TextOverflow::Ellipsis,
            ..markdown_text(text, bounds, size)
        })
    };
    let labels = [
        ("RESIDENT AGENT", Rect::new(96.0, 18.0, 152.0, 12.0), 11.0),
        ("**POC tile**", Rect::new(96.0, 34.0, 152.0, 26.0), 20.0),
        ("Connected", Rect::new(96.0, 68.0, 148.0, 18.0), 13.0),
    ];
    let [_, _, status] = labels.map(|(text, bounds, size)| {
        let node = leaf(label(text, bounds, size));
        let id = node.id;
        scene.add_node_to_tile(tile, Some(root_id), node).unwrap();
        id
    });
    prime_text(&mut compositor, &scene);
    let before = render_pixels(&mut compositor, &mut scene, &surface);
    for (text, bounds, _) in labels {
        let lit = pixels_in_rect(&before, 320, bounds)
            .filter(|pixel| pixel[0] > 128)
            .count();
        assert!(lit > 20, "{text} drew only {lit} lit pixels");
    }
    let (_, bounds, size) = labels[2];
    scene
        .update_node_content(tile, status, label("UPDATED", bounds, size))
        .unwrap();
    prime_text(&mut compositor, &scene);
    let after = render_pixels(&mut compositor, &mut scene, &surface);
    assert!(
        !pixels_in_rect(&before, 320, bounds)
            .map(|pixel| pixel[0])
            .eq(pixels_in_rect(&after, 320, bounds).map(|pixel| pixel[0]))
    );
}

/// A live font change rebuilds both text owners and preserves uploaded faces.
#[tokio::test]
async fn font_sans_change_rerasterizes_published_widget_text() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(256, 128).await);
    assert_eq!(
        compositor.resolved_fonts(),
        Some(&crate::fonts::ResolvedFonts::default())
    );
    let uploaded = [0x5A; 32];
    let face = include_bytes!("../../../fonts/ibm-plex/IBMPlexMono-Regular.ttf");
    assert!(compositor.load_font_bytes(uploaded, face));
    let faces = compositor
        .text_rasterizer
        .as_ref()
        .unwrap()
        .font_face_count();
    compositor.init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.widget_renderer_mut().unwrap().register_svg(
        "font-probe",
        "label.svg",
        br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 60"><text x="10" y="44" font-family="sans-serif" font-size="40" fill="#ffffff">Agag</text></svg>"##.to_vec(),
    );
    let mut scene = SceneGraph::new(256.0, 128.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    scene.widget_registry.register_definition(WidgetDefinition {
        id: "font-probe".into(),
        name: "Font probe".into(),
        description: "text-only widget".into(),
        parameter_schema: vec![],
        layers: vec![WidgetSvgLayer {
            svg_file: "label.svg".into(),
            bindings: vec![],
        }],
        default_geometry_policy: relative(0.0, 0.0, 1.0, 0.5),
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::LatestWins,
        max_publishers: WidgetDefinition::default_max_publishers(),
        ephemeral: false,
        hover_behavior: None,
    });
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "font-probe".into(),
        tab_id: tab,
        geometry_override: None,
        contention_override: None,
        instance_name: "probe".into(),
        current_params: HashMap::new(),
        auto_clear_ms: None,
    });
    scene
        .publish_to_widget("probe", HashMap::new(), "agent-a", None, 0, None)
        .unwrap();

    let draw = |compositor: &mut Compositor, scene: &mut SceneGraph| {
        prime_text(compositor, scene);
        render_pixels(compositor, scene, &surface)
    };
    let count = |c: &Compositor| c.widget_renderer().unwrap().raster_count("probe");
    let plex = draw(&mut compositor, &mut scene);
    assert_eq!(count(&compositor), 1);
    assert!(plex.chunks_exact(4).any(|pixel| pixel[0] > 128));
    assert_eq!(draw(&mut compositor, &mut scene), plex);
    assert_eq!(count(&compositor), 1);

    let tokens = |sans: &str| token_overrides([("font.sans", sans)]);
    compositor
        .text_rasterizer
        .as_mut()
        .unwrap()
        .shape_call_count = 12_345;
    compositor.set_token_map(tokens("IBM Plex Sans"));
    assert_eq!(
        compositor
            .text_rasterizer
            .as_ref()
            .unwrap()
            .shape_call_count,
        12_345
    );
    assert_eq!(draw(&mut compositor, &mut scene), plex);
    assert_eq!(count(&compositor), 1);

    compositor.set_token_map(tokens("DejaVu Serif"));
    let rasterizer = compositor.text_rasterizer.as_ref().unwrap();
    assert_eq!(rasterizer.shape_call_count, 0);
    assert!(rasterizer.has_font(&uploaded));
    assert_eq!(rasterizer.font_face_count(), faces);
    let resolved = compositor.resolved_fonts().unwrap();
    assert_eq!(resolved.sans, "DejaVu Serif");
    assert_eq!(resolved.mono, crate::fonts::DEFAULT_MONO_FAMILY);
    let serif = draw(&mut compositor, &mut scene);
    assert_eq!(count(&compositor), 2);
    assert_ne!(plex, serif, "widget still shows the old face");
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    assert_eq!(compositor.resolved_fonts().unwrap().sans, "DejaVu Serif");

    compositor.set_token_map(tokens("IBM Plex Sans"));
    let back = draw(&mut compositor, &mut scene);
    assert_eq!(count(&compositor), 3);
    assert_eq!(plex, back, "default face did not come back");
}
