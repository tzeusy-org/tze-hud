use super::*;

// ── RenderingPolicy-driven zone rendering tests ─────────────

/// Subtitle with outline: when outline_color + outline_width are set in
/// RenderingPolicy, collect_text_items produces a TextItem with non-None
/// outline fields.
#[tokio::test]
async fn test_zone_subtitle_with_outline_text_item() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "subtitle zone with outline".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(22.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            outline_color: Some(Rgba::BLACK),
            outline_width: Some(2.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Test outline text".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 1, "expected one TextItem");
    let item = &items[0];
    assert!(
        item.outline_color.is_some(),
        "outline_color should be set from RenderingPolicy"
    );
    assert!(
        item.outline_width.is_some(),
        "outline_width should be set from RenderingPolicy"
    );
    assert_eq!(
        item.outline_width.unwrap(),
        2.0,
        "outline_width should match policy"
    );
    // Text color should be white (from text_color).
    assert_eq!(
        item.color[0], 255,
        "text fill color R should be white (255)"
    );
}

/// Shipped caption styling, wrapping, truncation, retained work and drawn/drag
/// geometry agree across themes and monitor/UI scales.
#[tokio::test]
async fn test_zone_subtitle_pill_layout_and_streaming() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    for theme in ["tonal-glass", "classic", "blueprint"] {
        for scale in [1.0, 1.5] {
            let mut tokens =
                tze_hud_config::themes::resolve_config_tokens(&std::collections::HashMap::from([
                    ("theme".into(), theme.into()),
                ]));
            tze_hud_config::tokens::set_px_scale(&mut tokens, scale);
            compositor.set_token_map(tokens.clone());
            let mut scene = SceneGraph::new(1280.0, 720.0);
            scene.zone_registry = ZoneRegistry::with_defaults();
            let zone = scene.zone_registry.zones.get_mut("subtitle").unwrap();
            zone.rendering_policy = tze_hud_config::build_effective_policy(
                "subtitle",
                &RenderingPolicy::default(),
                &tokens,
            );
            let policy = zone.rendering_policy.clone();
            let zone = zone.clone();
            let geom = scene.effective_zone_geometry(&zone);
            let (x, y, w, h) = compositor.zone_geometry("subtitle", &geom, 1280.0, 720.0);
            let zone_rect = Rect::new(x, y, w, h);
            let long =
                "FIRST paragraph with wrapping and punctuation — café. ".repeat(30) + " LAST";
            for text in ["A short caption", long.as_str()] {
                scene
                    .publish_to_zone(
                        "subtitle",
                        ZoneContent::StreamText(text.into()),
                        "test",
                        None,
                        None,
                        None,
                    )
                    .unwrap();
                compositor.prime_markdown_cache(&scene);
                // Model the settled commit after the existing resize debounce.
                compositor.resize_reprime_last_at = None;
                compositor.prime_truncation_cache(&scene);
                let pill = compositor
                    .subtitle_layout("subtitle", text, &policy, zone_rect)
                    .unwrap();
                let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
                assert_eq!(items.len(), 1);
                let item = &items[0];
                assert_eq!(item.outline_width.is_some(), theme == "classic");
                let physical = tze_hud_config::tokens::px_scale(&tokens);
                assert!(
                    (item.font_size_px
                        - tokens["type.caption.display.size"].parse::<f32>().unwrap() * physical)
                        .abs()
                        < 0.001
                );
                assert!(
                    (item.font_size_px * item.line_height_multiplier
                        - tokens["type.caption.display.line_height"]
                            .parse::<f32>()
                            .unwrap()
                            * physical)
                        .abs()
                        < 0.001
                );
                assert!(pill.rect.width <= zone_rect.width && pill.rect.height <= zone_rect.height);
                assert!(item.pixel_x >= pill.rect.x && item.pixel_y >= pill.rect.y);
                assert!(item.pixel_x + item.bounds_width <= pill.rect.x + pill.rect.width + 0.001);
                assert!(
                    item.pixel_y + item.bounds_height <= pill.rect.y + pill.rect.height + 0.001
                );
                if theme != "classic" {
                    assert!(pill.rect.width <= 760.0 * physical);
                }
                let key = crate::text::effective_truncation_key(item).unwrap();
                let truncation = compositor
                    .text_rasterizer
                    .as_ref()
                    .unwrap()
                    .truncation_cache
                    .get_by_key(&key)
                    .expect("priming uses the exact pill text box");
                assert_eq!(truncation.was_truncated, text == long);
                let commands = compositor.collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0);
                let fills = commands.content.iter().collect::<Vec<_>>();
                assert_eq!(fills.len(), usize::from(theme != "classic"));
                if let Some(cmd) = fills.first() {
                    assert_eq!(Rect::new(cmd.x, cmd.y, cmd.width, cmd.height), pill.rect);
                    assert!(cmd.border.is_some());
                }
                let first = compositor.build_windowed_frame(&mut scene, 1280, 720);
                drop(first);
                let shapes = compositor.subtitle_layouts.borrow().shapes;
                let second = compositor.build_windowed_frame(&mut scene, 1280, 720);
                drop(second);
                assert_eq!(
                    compositor.subtitle_layouts.borrow().shapes,
                    shapes,
                    "unchanged frames reuse measured captions"
                );
                let handles = compositor.collect_drag_handle_entries(&scene, 1280.0, 720.0);
                if let Some(handle) = handles
                    .iter()
                    .find(|h| h.element_id == scene.zone_registry.zones["subtitle"].id)
                {
                    assert_eq!(handle.element_bounds, pill.rect);
                }
            }
            scene.clear_zone("subtitle").unwrap();
            compositor.prime_subtitle_layout(&scene, 1280.0, 720.0);
            assert!(
                compositor
                    .collect_text_items(&scene, 1280.0, 720.0)
                    .is_empty()
            );
            assert!(
                compositor
                    .collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0)
                    .content
                    .is_empty()
            );
        }
    }

    let tokens =
        tze_hud_config::themes::resolve_config_tokens(&std::collections::HashMap::from([(
            "theme".into(),
            "tonal-glass".into(),
        )]));
    compositor.set_token_map(tokens.clone());
    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.zone_registry = ZoneRegistry::with_defaults();
    scene
        .zone_registry
        .zones
        .get_mut("subtitle")
        .unwrap()
        .rendering_policy =
        tze_hud_config::build_effective_policy("subtitle", &RenderingPolicy::default(), &tokens);

    // Changing the profile threshold must invalidate the pill as well as the
    // truncation cache. The long middle line is outside both visible windows.
    let text = [
        "H\n".repeat(12),
        "wide middle ".repeat(100),
        "\n".into(),
        ["TAIL"; 12].join("\n"),
    ]
    .concat();
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText(text.clone()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();
    compositor.set_max_truncation_input_bytes(usize::MAX);
    compositor.prime_subtitle_layout(&scene, 1280.0, 720.0);
    let full_width = compositor.collect_text_items(&scene, 1280.0, 720.0)[0].bounds_width;
    compositor.set_max_truncation_input_bytes(128);
    // Recreating the actual renderer must keep the configured measurement limit.
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.prime_subtitle_layout(&scene, 1280.0, 720.0);
    let head_width = compositor.collect_text_items(&scene, 1280.0, 720.0)[0].bounds_width;
    assert!(head_width < full_width);
    scene
        .zone_registry
        .zones
        .get_mut("subtitle")
        .unwrap()
        .rendering_policy
        .stream_tail_anchored = Some(true);
    compositor.prime_subtitle_layout(&scene, 1280.0, 720.0);
    let tail_width = compositor.collect_text_items(&scene, 1280.0, 720.0)[0].bounds_width;
    assert!(
        head_width < tail_width && tail_width < full_width,
        "anchoring selects the measured viewport"
    );
    let shapes = compositor.subtitle_layouts.borrow().shapes;
    compositor.set_max_truncation_input_bytes(128);
    compositor.prime_subtitle_layout(&scene, 1280.0, 720.0);
    assert_eq!(compositor.subtitle_layouts.borrow().shapes, shapes);

    compositor.set_max_truncation_input_bytes(4096);
    let line = "caption words with punctuation, cafe and line boundaries.\n";
    let budget = std::time::Duration::from_micros(tze_hud_scene::perf_budget::test_budget(50_000));
    for tail in [false, true] {
        scene
            .zone_registry
            .zones
            .get_mut("subtitle")
            .unwrap()
            .rendering_policy
            .stream_tail_anchored = Some(tail);
        for bytes in [65_536_usize, 262_144] {
            for revision in 0..3 {
                let text = format!(
                    "HEAD revision {revision}\n{}TAIL revision {revision}",
                    line.repeat(bytes.div_ceil(line.len()))
                );
                scene
                    .publish_to_zone(
                        "subtitle",
                        ZoneContent::StreamText(text),
                        "test",
                        None,
                        None,
                        None,
                    )
                    .unwrap();
                compositor.prime_markdown_cache(&scene);
                compositor.resize_reprime_last_at = None;
                let start = std::time::Instant::now();
                compositor.prime_truncation_cache(&scene);
                let elapsed = start.elapsed();
                assert!(
                    elapsed < budget,
                    "changed {bytes}-byte subtitle (tail={tail}) prime {elapsed:?} exceeds {budget:?}"
                );
                let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
                let item = &items[0];
                let key = crate::text::effective_truncation_key(item).unwrap();
                let result = compositor
                    .text_rasterizer
                    .as_ref()
                    .unwrap()
                    .truncation_cache
                    .get_by_key(&key)
                    .unwrap();
                assert!(result.was_truncated);
                assert!(
                    result.text.contains(if tail {
                        "TAIL revision"
                    } else {
                        "HEAD revision"
                    }),
                    "anchored visible text: {:?}",
                    result.text
                );
                assert!(!result.text.contains(if tail {
                    "HEAD revision"
                } else {
                    "TAIL revision"
                }));
                drop(compositor.build_windowed_frame(&mut scene, 1280, 720));
                let shapes = compositor.subtitle_layouts.borrow().shapes;
                drop(compositor.build_windowed_frame(&mut scene, 1280, 720));
                assert_eq!(
                    compositor.subtitle_layouts.borrow().shapes,
                    shapes,
                    "unchanged large publications reuse layout"
                );
            }
        }
    }
}

/// Render-branch regression guard: a portal carrying a *zero-length
/// lifecycle sentinel* color run must still take the cached/styled markdown
/// render path, NOT the lossy `from_text_markdown_node` / `strip_markdown_v1`
/// path.
///
/// Background: `lifecycle_marker_color_runs` (resident_grpc.rs) emits a
/// zero-length `TextColorRun` ([start..start], no pixel coverage) on every
/// permitted viewer of a normal active/attached portal.  The render branch used
/// to gate on `color_runs.is_empty()`, so this sentinel flipped *every* normal
/// portal onto the lossy/uncached path (losing markdown styling AND the
/// commit-time markdown cache) while painting no accent pixels.  The fix gates
/// on `markdown_node_has_pixel_runs` instead.
///
/// This asserts at the COMPOSITOR RENDER-BRANCH level (the existing tests only
/// covered node construction, which is why the regression slipped through):
/// `collect_text_items` must produce a `TextItem` with populated `styled_runs`
/// (the cached path's signature; the lossy node path always leaves
/// `styled_runs` empty).
#[tokio::test]
async fn test_lifecycle_sentinel_keeps_cached_markdown_render_path() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Markdown content that yields styled spans (heading + bold) — the cached
    // path emits StyledRunItems for these; the lossy strip path emits none.
    let content = "# Portal\n**attached** and ready".to_owned();

    let make_node = |runs: Box<[tze_hud_scene::types::TextColorRun]>| Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.clone(),
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: runs,
        }),
    };

    // Zero-length lifecycle sentinel: start_byte == end_byte (no pixel coverage),
    // exactly as lifecycle_marker_color_runs emits for a normal active portal.
    let sentinel = tze_hud_scene::types::TextColorRun {
        start_byte: 0,
        end_byte: 0,
        color: Rgba::new(0.2, 0.8, 0.4, 1.0),
    };
    let scene = scene_with_node(make_node(Box::from([sentinel])));
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    assert_eq!(
        items.len(),
        1,
        "expected exactly one TextItem for the portal node"
    );
    let item = &items[0];
    assert!(
        !item.styled_runs.is_empty(),
        "portal with a zero-length lifecycle sentinel must take the cached/styled \
         markdown path (styled_runs populated), not the lossy strip path"
    );
    assert!(
        item.color_runs.is_empty(),
        "the zero-length sentinel carries no pixel coverage and must be dropped \
         (no ColorRunItems) on the cached path"
    );

    // Control: a genuine *pixel-bearing* color run (start < end) must still force
    // the legacy from_text_markdown_node path (styled_runs empty), proving the
    // fix narrowed the gate to pixel runs without disabling the legacy path.
    let pixel_run = tze_hud_scene::types::TextColorRun {
        start_byte: 0,
        end_byte: 4,
        color: Rgba::new(0.9, 0.1, 0.1, 1.0),
    };
    let scene_pixel = scene_with_node(make_node(Box::from([pixel_run])));
    compositor.prime_markdown_cache(&scene_pixel);
    compositor.prime_truncation_cache(&scene_pixel);
    let pixel_items = compositor.collect_text_items(&scene_pixel, 256.0, 256.0);
    assert_eq!(
        pixel_items.len(),
        1,
        "expected one TextItem for the pixel-run node"
    );
    assert!(
        pixel_items[0].styled_runs.is_empty(),
        "a pixel-bearing color run must still take the legacy raw-content path \
         (styled_runs empty), preserving its raw byte offsets"
    );
    assert!(
        !pixel_items[0].color_runs.is_empty(),
        "the pixel-bearing color run must be preserved as a ColorRunItem"
    );
}

/// Notification action buttons are drawn at exactly the bounds the pointer
/// hit regions register, as `shape.s` SDF controls: the first filled with
/// `color.primary.container`, the rest transparent, with `state.*` layers
/// when `chrome_states` says so and the focus ring when focused; an idle
/// transparent button draws nothing. Labels sit inside each button. Two
/// notifications (the older one titled, so slot heights differ) exercise the
/// shared slot layout. Draw-command level (no pixel readback).
#[tokio::test]
async fn notification_actions_layout_matches_hit_regions() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 1080).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let adversarial = "A full-width label still clips within one button. ".repeat(128);
    for (labels, stacked) in [
        (["Go", "No", "OK", "Do"], false),
        ([adversarial.as_str(), "Hold", "Skip", "Open"], true),
        (
            [
                "Ship after the entire staging validation has finished",
                "Hold",
                "Skip",
                "Open",
            ],
            true,
        ),
        (
            [
                "Sélectionner après la vérification complète",
                "Überprüfen",
                "العربية",
                "Ouvrir",
            ],
            true,
        ),
    ] {
        let mut tokens = HashMap::from([
            ("runtime.px_scale".into(), "1.5".into()),
            ("notification.action.link_mark".into(), "↗︎".into()),
            ("font.sans".into(), "IBM Plex Mono".into()),
            ("color.primary.container".into(), "#336699".into()),
            ("space.m".into(), "10".into()),
            ("space.xs".into(), "6".into()),
        ]);
        tokens = tze_hud_config::themes::resolve_config_tokens(&tokens);
        compositor.set_token_map(tokens);
        let mut scene = SceneGraph::new(1280.0, 1080.0);
        scene.create_tab("Main", 0).unwrap();
        let mut zone = ZoneRegistry::with_defaults()
            .zones
            .remove("notification-area")
            .unwrap();
        // The mock's 368 dp card at 1.5x: 552 px.
        zone.geometry_policy = GeometryPolicy::Relative {
            x_pct: 0.5,
            y_pct: 0.0,
            width_pct: 552.0 / 1280.0,
            height_pct: 0.95,
        };
        zone.rendering_policy.margin_horizontal = Some(18.0);
        zone.rendering_policy.font_family = Some(FontFamily::SystemMonospace);
        scene.register_zone(zone);
        for ns in ["following", "newest"] {
            scene
                .publish_to_zone(
                    "notification-area",
                    ZoneContent::Notification(NotificationPayload {
                        text: format!("Body for {ns}"),
                        title: "Deploy".into(),
                        actions: labels
                            .iter()
                            .enumerate()
                            .map(|(i, label)| NotificationAction {
                                copy: None,
                                label: (*label).into(),
                                callback_id: (i % 2 == 0).then(|| i.to_string()),
                                url: (i % 2 == 1).then(|| format!("https://example.com/{i}")),
                                role: None,
                            })
                            .collect(),
                        ..Default::default()
                    }),
                    ns,
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        compositor.prime_notification_layouts(&scene, 1280.0, 1080.0);
        compositor.populate_zone_hit_regions(&mut scene, 1280.0, 1080.0);
        let record = &scene.zone_registry.active_for_zone("notification-area")[1];
        let policy = &scene.zone_registry.zones["notification-area"].rendering_policy;
        let descriptor =
            compositor.notification_action_layout("notification-area", record, policy, 552.0);
        let publication_id = record.publication_id;
        let ZoneContent::Notification(payload) = record.content.clone() else {
            unreachable!()
        };
        assert_eq!(descriptor.stacked, stacked);
        let shapes = compositor.notification_layouts.borrow().shapes;
        let regions = scene.overlay.zone_hit_regions.clone();
        assert_eq!(
            regions
                .iter()
                .filter(|r| matches!(r.kind, ZoneInteractionKind::Action { .. }))
                .count(),
            8
        );
        let action_regions: Vec<_> = regions
            .iter()
            .filter(|r| {
                r.publisher_namespace == "newest"
                    && matches!(r.kind, ZoneInteractionKind::Action { .. })
            })
            .collect();
        scene.set_chrome_hovered(&action_regions[0].interaction_id, true);
        scene.set_chrome_pressed(&action_regions[1].interaction_id, true);
        scene.focus_notification(Some(action_regions[2].interaction_id.clone()));
        let commands = compositor
            .collect_all_rounded_rect_cmds(&scene, 1280.0, 1080.0)
            .chrome;
        let items = compositor.collect_text_items(&scene, 1280.0, 1080.0);
        let style = crate::renderer::notification_style::NotificationStyle::resolve(
            compositor.zone_tokens("notification-area"),
            &scene.zone_registry.zones["notification-area"].rendering_policy,
        );
        for region in &action_regions {
            let ZoneInteractionKind::Action { index } = &region.kind else {
                unreachable!()
            };
            let index = *index;
            let bounds = region.bounds;
            let state = scene
                .overlay
                .chrome_states
                .get(&region.interaction_id)
                .copied()
                .unwrap_or_default();
            let fill = style.control_fill(state, style.action_fill(&payload, index));
            let drawn: Vec<_> = commands
                .iter()
                .filter(|c| {
                    (c.x, c.y, c.width, c.height)
                        == (bounds.x, bounds.y, bounds.width, bounds.height)
                })
                .collect();
            match (index, fill) {
                (3, None) => assert!(drawn.is_empty(), "an idle transparent action draws nothing"),
                (2, None) => {
                    assert_eq!(drawn.len(), 1);
                    assert_eq!(drawn[0].color, [0.0; 4], "focused: ring only");
                    assert_eq!(drawn[0].border.as_ref().unwrap().width, style.focus_width);
                }
                (_, Some(fill)) => {
                    assert_eq!(drawn.len(), 1);
                    assert_eq!(drawn[0].color, compositor.gpu_color(fill));
                    assert_eq!(drawn[0].radius, style.control_radius);
                }
                other => panic!("unexpected action state {other:?}"),
            }
            let item = items
                .iter()
                .find(|item| &*item.text == labels[index])
                .unwrap();
            assert_eq!(
                item.pixel_x,
                bounds.x + descriptor.padding + descriptor.hint_width
            );
            let mark = descriptor.link_marks[index].as_ref();
            let mark_width = mark.map_or(0.0, |(_, width)| *width);
            assert_eq!(
                item.bounds_width,
                bounds.width - descriptor.padding * 2.0 - descriptor.hint_width - mark_width
            );
            assert!(region.interaction_id.ends_with(&format!(":{index}")));
            if let Some((mark, width)) = mark {
                let glyph = items
                    .iter()
                    .find(|i| {
                        &*i.text == mark
                            && i.pixel_x == bounds.x + bounds.width - descriptor.padding - width
                            && i.pixel_y == item.pixel_y
                    })
                    .unwrap();
                assert_eq!(glyph.bounds_width, *width);
                assert!((glyph.pixel_x - (item.pixel_x + item.bounds_width)).abs() < 0.001);
                assert_eq!(glyph.font_size_px, item.font_size_px);
                assert_eq!(glyph.color, item.color);
            }
            assert!(
                item.pixel_y >= bounds.y
                    && item.pixel_y + item.bounds_height <= bounds.y + bounds.height
            );
            assert!(
                items
                    .iter()
                    .filter(|i| &*i.text == "Body for newest")
                    .all(|i| i.pixel_y + i.bounds_height <= action_regions[0].bounds.y)
            );
            if stacked {
                assert_eq!(bounds.width, 552.0 - 2.0 * style.padding_h);
            }
        }
        for pair in action_regions.windows(2) {
            if stacked {
                assert!(pair[0].bounds.y + pair[0].bounds.height < pair[1].bounds.y);
            } else {
                assert_eq!(pair[0].bounds.y, pair[1].bounds.y);
            }
        }
        let dismiss: Vec<_> = regions
            .iter()
            .filter(|r| matches!(r.kind, ZoneInteractionKind::Dismiss))
            .collect();
        assert!(
            dismiss[0].bounds.y < dismiss[1].bounds.y,
            "following card moves below the entire chosen layout"
        );
        // An unchanged frame uses retained measurements and identical hit geometry.
        compositor.prime_notification_layouts(&scene, 1280.0, 1080.0);
        compositor.populate_zone_hit_regions(&mut scene, 1280.0, 1080.0);
        assert_eq!(compositor.notification_layouts.borrow().shapes, shapes);
        assert_eq!(scene.overlay.zone_hit_regions, regions);
        assert!(
            !items
                .iter()
                .any(|item| ["1", "2", "3", "4"].contains(&item.text.as_ref()))
        );
        scene.overlay.notification_answer = Some(publication_id);
        let with_hints = compositor.collect_text_items(&scene, 1280.0, 1080.0);
        for (index, action_label) in labels.iter().enumerate() {
            let label = |items: &[crate::text::TextItem]| {
                items
                    .iter()
                    .find(|item| &*item.text == *action_label)
                    .map(|item| {
                        (
                            item.pixel_x,
                            item.pixel_y,
                            item.bounds_width,
                            item.bounds_height,
                        )
                    })
            };
            assert_eq!(
                label(&items),
                label(&with_hints),
                "hint visibility cannot move or rewrap labels"
            );
            assert_eq!(
                with_hints
                    .iter()
                    .filter(|item| *item.text == (index + 1).to_string())
                    .count(),
                1
            );
        }
        compositor.populate_zone_hit_regions(&mut scene, 1280.0, 1080.0);
        assert_eq!(scene.overlay.zone_hit_regions, regions);
        assert_eq!(compositor.notification_layouts.borrow().shapes, shapes);
        scene.focus_notification(None);
        let after = compositor.collect_text_items(&scene, 1280.0, 1080.0);
        assert!(
            !after
                .iter()
                .any(|item| ["1", "2", "3", "4"].contains(&item.text.as_ref()))
        );
        // A pressed url action says what happened in its label's exact box;
        // failure takes `color.error`. Nothing else moves.
        scene.overlay.notification_answer = None;
        let geometry = |item: &crate::text::TextItem| {
            (
                item.pixel_x,
                item.pixel_y,
                item.bounds_width,
                item.bounds_height,
            )
        };
        let idle = after.iter().find(|item| &*item.text == labels[1]).unwrap();
        let error =
            super::super::token_colors::parse_hex_color(&compositor.token_map["color.error"])
                .unwrap();
        for state in [
            tze_hud_scene::LinkFeedbackState::Opening,
            tze_hud_scene::LinkFeedbackState::Opened,
            tze_hud_scene::LinkFeedbackState::Failed,
        ] {
            scene.set_link_feedback(Some((
                tze_hud_scene::LinkFeedbackTarget::CardAction {
                    publication_id,
                    index: 1,
                },
                state,
            )));
            let shown = compositor.collect_text_items(&scene, 1280.0, 1080.0);
            let item = shown
                .iter()
                .find(|item| &*item.text == state.label())
                .unwrap();
            assert_eq!(
                geometry(item),
                geometry(idle),
                "{state:?} keeps the label's box"
            );
            if state == tze_hud_scene::LinkFeedbackState::Failed {
                assert_eq!(item.color[..3], crate::text::rgba_to_srgb_u8(error)[..3]);
            } else {
                assert_eq!(item.color, idle.color);
            }
            assert_eq!(
                shown.iter().filter(|item| &*item.text == labels[1]).count(),
                1,
                "the other card's label is untouched"
            );
            compositor.populate_zone_hit_regions(&mut scene, 1280.0, 1080.0);
            assert_eq!(scene.overlay.zone_hit_regions, regions);
        }
        scene.set_link_feedback(None);
    }
}

/// a notification with actions reserves its button row in the
/// slot height, so stacked one- and two-line notifications draw their text
/// strictly above their buttons; a notification without actions keeps its
/// text-only height.
#[tokio::test]
async fn notification_action_row_is_reserved_below_the_text() {
    check_notification_action_row(None).await;
}

/// The same draw/hit/reserve agreement at an explicit horizontal zone margin
/// (20 px, unlike the canonical 16 dp vertical padding).
#[tokio::test]
async fn notification_action_row_follows_a_themed_inset() {
    check_notification_action_row(Some(20.0)).await;
}

async fn check_notification_action_row(margin: Option<f32>) {
    use crate::renderer::notification_style::DISMISS_GLYPH;
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let policy = RenderingPolicy {
        font_size_px: Some(16.0),
        backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
        margin_horizontal: margin,
        ..Default::default()
    };
    // Every card metric is token-derived.
    let style = NotificationStyle::resolve(&HashMap::new(), &policy);
    let card_inset = style.padding_h;
    assert_eq!(
        card_inset,
        margin.unwrap_or(16.0),
        "policy margin, else space.l"
    );
    let action = |label: &str| tze_hud_scene::types::NotificationAction {
        copy: None,
        label: label.to_owned(),
        callback_id: Some(label.to_lowercase()),
        url: None,
        role: None,
    };
    let payload = |title: &str, text: &str, labels: &[&str]| NotificationPayload {
        progress: None,
        text: text.to_owned(),
        icon: String::new(),
        urgency: 1,
        ttl_ms: None,
        title: title.to_owned(),
        actions: labels.iter().map(|l| action(l)).collect(),
    };

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.create_tab("Main", 0).unwrap();
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.6,
            y_pct: 0.0,
            width_pct: 0.35,
            height_pct: 0.8,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: policy.clone(),
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    // The owner's Windows case (title + one-line body), a body-only card, and
    // a card without actions, stacked.
    for (ns, p) in [
        (
            "a",
            payload(
                "Click test 2",
                "Hover each button, then click Approve once.",
                &["Approve", "Reject"],
            ),
        ),
        ("b", payload("", "Body only", &["Open"])),
        ("c", payload("Plain", "No actions here", &[])),
    ] {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(p),
                ns,
                None,
                None,
                None,
            )
            .unwrap();
    }

    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let text_bottom = |text: &str| {
        let i = items
            .iter()
            .find(|i| &*i.text == text)
            .unwrap_or_else(|| panic!("no text item {text:?}"));
        i.pixel_y + i.bounds_height
    };
    let button_top = |callback: &str| {
        scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|r| {
                scene
                    .notification_action(r)
                    .is_some_and(|a| a.callback_id.as_deref() == Some(callback))
            })
            .unwrap_or_else(|| panic!("no hit region for {callback}"))
            .bounds
            .y
    };
    for (text, callback) in [
        ("Hover each button, then click Approve once.", "approve"),
        ("Hover each button, then click Approve once.", "reject"),
        ("Click test 2", "approve"),
        ("Body only", "open"),
    ] {
        assert!(
            text_bottom(text) <= button_top(callback),
            "{text:?} bounds end at {} but the {callback} button starts at {}",
            text_bottom(text),
            button_top(callback)
        );
    }

    // Owner feedback on Windows: buttons must sit inside the card (inset from
    // its sides and bottom), and stacked cards must not touch.
    let publishes = scene
        .zone_registry
        .active_for_zone("notification-area")
        .to_vec();
    // Zone: x = 0.6 * 1280, y = 0, w = 0.35 * 1280, h = 0.8 * 720.
    let (zx, zy, zw, zh) = (768.0_f32, 0.0_f32, 448.0_f32, 576.0_f32);
    let layout =
        compositor.zone_slot_layout(&scene, "notification-area", &publishes, &policy, zh, zw);
    let cards: Vec<(usize, f32, f32)> = layout
        .iter_visible(Rect::new(zx, zy, zw, zh))
        .map(|(i, c)| (i, c.y, c.height))
        .collect();
    assert_eq!(cards.len(), 3, "all three cards are visible");
    for pair in cards.windows(2) {
        let (_, top_y, top_h) = pair[0];
        let (_, next_y, _) = pair[1];
        assert_eq!(
            next_y - (top_y + top_h),
            style.stack_gap,
            "consecutive cards are separated by the stack gap"
        );
    }
    let action_regions: Vec<_> = scene
        .overlay
        .zone_hit_regions
        .iter()
        .filter(|r| {
            matches!(
                r.kind,
                tze_hud_scene::types::ZoneInteractionKind::Action { .. }
            )
        })
        .collect();
    assert_eq!(action_regions.len(), 3, "approve, reject and open");
    for region in action_regions {
        let b = region.bounds;
        let (_, card_y, card_h) = *cards
            .iter()
            .find(|(_, y, h)| b.y >= *y && b.y < y + h)
            .expect("each button lies in a card");
        let inset = card_inset;
        assert!(
            b.x >= zx + inset - 0.01
                && b.x + b.width <= zx + zw - inset + 0.01
                && b.y + b.height <= card_y + card_h - style.padding_v + 0.01,
            "button {b:?} must sit inside card y={card_y} h={card_h} minus the {inset} px inset"
        );
    }

    // The dismiss × sits inside each card's top-right padding (so the rounded
    // corner never clips it) and clear of the card's text.
    let dismiss_regions: Vec<_> = scene
        .overlay
        .zone_hit_regions
        .iter()
        .filter(|r| matches!(r.kind, tze_hud_scene::types::ZoneInteractionKind::Dismiss))
        .map(|r| r.bounds)
        .collect();
    assert_eq!(dismiss_regions.len(), 3, "one × per card");
    let inset = card_inset;
    for b in &dismiss_regions {
        let (_, card_y, card_h) = *cards
            .iter()
            .find(|(_, y, h)| b.y >= *y && b.y < y + h)
            .expect("each × lies in a card");
        assert!(
            b.x + b.width <= zx + zw - inset + 0.01
                && b.y >= card_y + style.padding_v - 0.01
                && b.y + b.height <= card_y + card_h - style.padding_v + 0.01,
            "× {b:?} must sit inside card y={card_y} h={card_h} minus the {inset} px inset"
        );
    }
    let overlaps = |a: tze_hud_scene::types::Rect, x: f32, y: f32, w: f32, h: f32| {
        a.x < x + w && x < a.x + a.width && a.y < y + h && y < a.y + a.height
    };
    // Card text only: skip the × glyph itself.
    for item in items.iter().filter(|i| &*i.text != DISMISS_GLYPH) {
        for b in &dismiss_regions {
            assert!(
                !overlaps(
                    *b,
                    item.pixel_x,
                    item.pixel_y,
                    item.bounds_width,
                    item.bounds_height
                ),
                "× {b:?} overlaps text {:?} at ({}, {}, {}x{})",
                item.text,
                item.pixel_x,
                item.pixel_y,
                item.bounds_width,
                item.bounds_height
            );
        }
    }
    for labels in [
        ["A", "B", "C", "D"],
        [
            "Choose after every staging dependency is ready",
            "Hold",
            "Skip",
            "Open",
        ],
        [
            "Après la vérification complète de tous les services",
            "Später",
            "العربية",
            "Åpne",
        ],
    ] {
        scene.clear_zone("notification-area").unwrap();
        for (ns, title) in [("older", "Title"), ("newer", "")] {
            scene
                .publish_to_zone(
                    "notification-area",
                    ZoneContent::Notification(payload(title, ns, &labels)),
                    ns,
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
        let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
        for ns in ["older", "newer"] {
            let body = items.iter().find(|item| &*item.text == ns).unwrap();
            let top = scene
                .overlay
                .zone_hit_regions
                .iter()
                .filter(|region| {
                    region.publisher_namespace == ns
                        && matches!(region.kind, ZoneInteractionKind::Action { .. })
                })
                .map(|region| region.bounds.y)
                .fold(f32::INFINITY, f32::min);
            assert!(body.pixel_y + body.bounds_height <= top);
        }
    }
}

/// Pixel check: on a rounded Chrome-layer card the action button
/// reads as a button over the card fill rather than vanishing under it, and
/// hovering it brightens it with no agent round trip.
#[tokio::test]
async fn notification_action_button_is_visible_and_hover_brightens_it() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = bordered_notification_scene(Some(12.0));
    scene.zone_registry.active_publishes.clear();
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                progress: None,
                text: "Approve the deploy?".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: "Deploy".to_owned(),
                actions: vec![tze_hud_scene::types::NotificationAction {
                    copy: None,
                    label: "Approve".to_owned(),
                    callback_id: Some("approve".to_owned()),
                    url: None,
                    role: None,
                }],
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let button = scene
        .overlay
        .zone_hit_regions
        .iter()
        .find(|r| {
            matches!(
                r.kind,
                tze_hud_scene::types::ZoneInteractionKind::Action { .. }
            )
        })
        .cloned()
        .expect("the action has a hit region");

    let render = |compositor: &mut Compositor, scene: &mut SceneGraph| {
        compositor.prime_markdown_cache(scene);
        compositor.prime_truncation_cache(scene);
        compositor.render_frame_headless(scene, &surface);
        let pixels = surface.read_pixels(&compositor.device);
        let px = move |x: f32, y: f32| -> [u8; 4] {
            let i = (y as usize * 1280 + x as usize) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        // A label-free spot inside the button (left edge, past the outline),
        // and the card fill in the inset left of the button row.
        let b = button.bounds;
        (
            px(b.x + 3.0, b.y + b.height * 0.5),
            px(b.x - 5.0, b.y + b.height * 0.5),
        )
    };
    let luma = |p: [u8; 4]| p[0] as u32 + p[1] as u32 + p[2] as u32;

    let (idle, card) = render(&mut compositor, &mut scene);
    assert!(
        luma(idle) > luma(card) + 20,
        "the idle button must stand out from the card fill: button {idle:?}, card {card:?}"
    );

    scene.set_chrome_hovered(&button.interaction_id, true);
    let (hovered, _) = render(&mut compositor, &mut scene);
    assert!(
        luma(hovered) > luma(idle) + 20,
        "hover must brighten the button: idle {idle:?}, hovered {hovered:?}"
    );
}

/// The alert and notification fill roles resolve in the backdrop pass. With
/// text initialized, the opaque card also emits its body and dismiss label.
#[tokio::test]
async fn notification_backdrops_follow_zone_policy_and_urgency() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let make_scene = |zone: ZoneDefinition, text: &str, urgency| {
        let mut scene = SceneGraph::new(1280.0, 720.0);
        let name = zone.name.clone();
        scene.register_zone(zone);
        publish_zone(
            &mut scene,
            &name,
            "test",
            ZoneContent::Notification(NotificationPayload {
                text: text.into(),
                urgency,
                ..Default::default()
            }),
        );
        scene
    };
    let notification_zone = |description: &str, geometry, rendering_policy, max_publishers| {
        let mut zone = zone_fixture(
            "notification-area",
            geometry,
            ZoneMediaType::ShortTextWithIcon,
            LayerAttachment::Chrome,
        );
        zone.description = description.into();
        zone.rendering_policy = rendering_policy;
        zone.contention_policy = ContentionPolicy::Stack { max_depth: 8 };
        zone.max_publishers = max_publishers;
        zone
    };
    let backdrop_vertices = |compositor: &Compositor, scene: &SceneGraph| {
        let mut vertices = Vec::new();
        compositor.render_zone_content(scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
        vertices
    };

    let mut alert = zone_fixture(
        "alert-banner",
        GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.07,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        ZoneMediaType::ShortTextWithIcon,
        LayerAttachment::Chrome,
    );
    alert.description = "alert banner zone".into();
    alert.rendering_policy = RenderingPolicy {
        font_size_px: Some(20.0),
        backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
        backdrop_opacity: Some(1.0),
        text_color: Some(Rgba::WHITE),
        ..Default::default()
    };
    let scene = make_scene(alert, "Warning: disk space low", 2);
    let vertices = backdrop_vertices(&compositor, &scene);
    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for alert-banner urgency=2"
    );

    // The urgency-2 tone is the alert caution container (canonical: Tonal
    // Glass's surface.container.high; the severity mark carries caution).
    let style =
        NotificationStyle::for_zone("alert-banner", &HashMap::new(), &RenderingPolicy::default());
    let caution = parse_hex_color("#222833").unwrap();
    let fill = style.tone(2).fill;
    assert_eq!((fill.r, fill.g, fill.b), (caution.r, caution.g, caution.b));
    assert!(
        style.mark(2, false).is_some(),
        "urgency 2 shows the caution severity mark"
    );

    let card_geometry = relative(0.75, 0.02, 0.24, 0.30);
    let critical = notification_zone(
        "notification area - uses notification urgency tokens, not severity",
        card_geometry,
        RenderingPolicy {
            backdrop: Some(Rgba::new(0.05, 0.05, 0.05, 0.85)),
            backdrop_opacity: Some(0.85),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        16,
    );
    let scene = make_scene(critical, "System alert", 3);
    assert!(
        !is_alert_banner_zone("notification-area"),
        "notification-area must not be treated as alert-banner"
    );
    let vertices = backdrop_vertices(&compositor, &scene);
    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for notification-area urgency=3"
    );
    // The card's error container differs from the critical severity mark.
    let first = &vertices[0];
    assert!(
        first.color[0] < 0.7,
        "notification-area urgency=3 R must NOT be severity critical (~1.0), got {}",
        first.color[0]
    );

    let opacity = notification_zone(
        "notification area urgency opacity test",
        card_geometry,
        RenderingPolicy {
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.5)),
            backdrop_opacity: Some(0.5),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        4,
    );
    let mut scene = make_scene(opacity, "test", 1);
    let vertices = backdrop_vertices(&compositor, &scene);
    assert!(!vertices.is_empty(), "expected vertices");
    let first_alpha = vertices[0].color[3];
    assert!(
        (first_alpha - 0.5).abs() < 0.01,
        "explicit backdrop_opacity must win, got {first_alpha}"
    );
    // Without it, the card takes opacity.notification (canonical 0.92).
    scene
        .zone_registry
        .zones
        .get_mut("notification-area")
        .unwrap()
        .rendering_policy
        .backdrop_opacity = None;
    let vertices = backdrop_vertices(&compositor, &scene);
    assert!(
        (vertices[0].color[3] - 0.92).abs() < 0.01,
        "opacity.notification default, got {}",
        vertices[0].color[3]
    );

    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut opaque = notification_zone(
        "notification area zone",
        GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.08,
            width_pct: 0.70,
            margin_px: 12.0,
        },
        RenderingPolicy {
            font_size_px: Some(18.0),
            backdrop: Some(Rgba::BLACK),
            backdrop_opacity: Some(0.9),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        4,
    );
    opaque.auto_clear_ms = Some(5_000);
    let scene = make_scene(opaque, "Notification with opaque backdrop", 1);
    let vertices = backdrop_vertices(&compositor, &scene);
    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for notification with opaque backdrop"
    );
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        items.len(),
        2,
        "expected two TextItems: notification body + dismiss button"
    );
    assert!(
        items[0].color[0] > 200,
        "text color R should be near-white from policy.text_color"
    );
}

/// Alert-banner urgency=3 fills with the alert error container.
#[tokio::test]
async fn test_alert_banner_urgency3_maps_to_alert_error_container() {
    let style =
        NotificationStyle::for_zone("alert-banner", &HashMap::new(), &RenderingPolicy::default());
    let error = parse_hex_color("#5C1A17").unwrap();
    let fill = style.tone(3).fill;
    assert_eq!((fill.r, fill.g, fill.b), (error.r, error.g, error.b));
}

/// Alert-banner urgency=0 and 1 share the info tone (fill and mark).
#[tokio::test]
async fn test_alert_banner_urgency_low_maps_to_info() {
    let style =
        NotificationStyle::for_zone("alert-banner", &HashMap::new(), &RenderingPolicy::default());
    let (info0, info1) = (style.tone(0), style.tone(1));
    assert_eq!(info0.fill, info1.fill);
    assert_eq!(info0.mark, info1.mark);
    assert_eq!(severity_glyph(0), severity_glyph(1));
    assert_ne!(severity_glyph(1), severity_glyph(2));
}

// ── Notification urgency tone tests ──────────────────────────────────────

/// Urgency tones: 0 and 1 the notification containers
/// (equal by default), 2 the caution container, 3 (and anything above,
/// clamped) the error container, each under its own hairline role; a token
/// restyles the card (sRGB hex decoded to linear) at `opacity.notification`.
#[test]
fn test_notification_urgency_color_tokens() {
    let policy = RenderingPolicy::default();
    let style = NotificationStyle::resolve(&HashMap::new(), &policy);
    let rgb = |c: Rgba| (c.r, c.g, c.b);
    assert_eq!(
        rgb(style.tone(0).fill),
        rgb(style.tone(1).fill),
        "0 and 1 are neutral"
    );
    assert_ne!(rgb(style.tone(1).fill), rgb(style.tone(2).fill));
    assert_ne!(rgb(style.tone(2).fill), rgb(style.tone(3).fill));
    for urgency in [4, 100] {
        assert_eq!(
            rgb(style.tone(urgency).fill),
            rgb(style.tone(3).fill),
            "urgency={urgency} must clamp to critical"
        );
    }

    // (urgency, role token, hex, expected linear rgb)
    let overrides = [
        (
            0,
            "color.notification.container.low",
            "#00FFFF",
            [0.0, 1.0, 1.0],
        ),
        (2, "color.caution.container", "#FF0000", [1.0, 0.0, 0.0]),
        (
            3,
            "color.error.container",
            "#450612",
            [0.0595, 0.0018, 0.0060],
        ),
    ];
    for (urgency, key, hex, expected) in overrides {
        let tokens = HashMap::from([
            (key.to_string(), hex.to_string()),
            ("opacity.notification".to_string(), "0.5".to_string()),
        ]);
        let c = NotificationStyle::resolve(&tokens, &policy)
            .tone(urgency)
            .fill;
        for (got, want) in [c.r, c.g, c.b].into_iter().zip(expected) {
            assert!((got - want).abs() < 0.004, "{key} token {hex}: got {c:?}");
        }
        assert!(
            (c.a - 0.5).abs() < 1e-6,
            "opacity.notification applies: {c:?}"
        );
    }
}

/// A Stack `notification-area` scene with one urgency-0 notification, for the
/// border tests. `radius` sets `backdrop_radius` (None = flat backdrop).
fn bordered_notification_scene(radius: Option<f32>) -> SceneGraph {
    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area border test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.05, 0.05, 0.05, 1.0)),
            backdrop_radius: radius,
            font_size_px: Some(18.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                progress: None,
                text: "border test".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();
    scene
}

/// The notification card hairline is an SDF border, not flat quads. On a flat
/// backdrop (no radius) the flat pass emits only the backdrop and the SDF pass
/// adds a border-only card shape; on a rounded backdrop the hairline rides on
/// the backdrop's own SDF shape. An idle × draws no shape; a hovered one
/// takes the state layer at the shared dismiss bounds.
#[tokio::test]
async fn test_notification_card_border_is_an_sdf_border() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let flat = bordered_notification_scene(None);
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&flat, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert_eq!(vertices.len(), 6, "flat pass: backdrop quad only");

    let (x, y, w) = (960.0, 14.4, 307.2);
    let cmds = compositor
        .collect_all_rounded_rect_cmds(&flat, 1280.0, 720.0)
        .chrome;
    assert_eq!(cmds.len(), 1, "card hairline only (idle ×): {cmds:?}");
    let card = &cmds[0];
    assert_eq!(card.color, [0.0; 4], "border-only over the flat backdrop");
    assert_eq!(card.radius, 0.0);
    assert!((card.x - x).abs() < 0.01 && (card.y - y).abs() < 0.01);
    assert!((card.width - w).abs() < 0.01, "card spans the slot width");
    assert_eq!(
        card.border.map(|b| b.width),
        Some(1.0),
        "stroke.border.width dp"
    );
    let mut hovered = flat.clone();
    compositor.populate_zone_hit_regions(&mut hovered, 1280.0, 720.0);
    let id = hovered
        .overlay
        .zone_hit_regions
        .iter()
        .find(|r| matches!(r.kind, ZoneInteractionKind::Dismiss))
        .unwrap()
        .clone();
    hovered.set_chrome_hovered(&id.interaction_id, true);
    let cmds = compositor
        .collect_all_rounded_rect_cmds(&hovered, 1280.0, 720.0)
        .chrome;
    assert_eq!(cmds.len(), 2, "card + hovered ×: {cmds:?}");
    let dismiss = &cmds[1];
    assert!(dismiss.color[3] > 0.0, "hover state layer");
    assert!(dismiss.border.is_none(), "no outline unless focused");
    assert_eq!(
        (dismiss.x, dismiss.y, dismiss.width, dismiss.height),
        (id.bounds.x, id.bounds.y, id.bounds.width, id.bounds.height),
        "drawn × is the hit region"
    );
    assert!(
        (dismiss.x + dismiss.width - (x + w - 16.0)).abs() < 0.01,
        "dismiss inside the space.notification padding from the right edge"
    );

    let rounded = bordered_notification_scene(Some(12.0));
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(
        &rounded,
        &mut vertices,
        &mut Vec::new(),
        1280.0,
        720.0,
        None,
    );
    assert!(
        vertices.is_empty(),
        "rounded backdrop and borders all go to the SDF pass"
    );
    let cmds = compositor
        .collect_all_rounded_rect_cmds(&rounded, 1280.0, 720.0)
        .chrome;
    assert_eq!(cmds.len(), 1, "card (idle ×): {cmds:?}");
    assert_eq!(cmds[0].radius, 12.0);
    assert!(cmds[0].color[3] > 0.7, "card carries its fill");
    assert_eq!(cmds[0].border.map(|b| b.width), Some(1.0));

    // Overlapping same-layer zones share one stable paint order, independent
    // of registration and randomized map buckets. Keep the actual SDF/pixel
    // path and generated controls in this existing border parent.
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.set_token_map(HashMap::from([
        ("color.notification.outline".into(), "#FF0000".into()),
        (
            "color.notification.caution.outline".into(),
            "#00FF00".into(),
        ),
        ("color.notification.container.low".into(), "#0000FF".into()),
        ("opacity.notification".into(), "1".into()),
    ]));
    let mut failures = Vec::new();
    for reverse in [false, true] {
        let mut overlap = SceneGraph::new(1280.0, 720.0);
        let mut names = vec!["alert-banner".to_string()];
        names.extend((0..8).map(|i| format!("notification-area-{i:02}")));
        let expected_names = names[1..].to_vec();
        if reverse {
            names.reverse();
        }
        for name in names {
            let banner = name == "alert-banner";
            let index = name.rsplit('-').next().unwrap().parse::<u32>().unwrap_or(0);
            let mut zone =
                bordered_notification_scene(Some(8.0)).zone_registry.zones["notification-area"]
                    .clone();
            zone.name = name.clone();
            zone.geometry_policy = GeometryPolicy::Relative {
                x_pct: if banner {
                    0.70
                } else {
                    0.75 + index as f32 * 0.001
                },
                y_pct: if banner { 0.0 } else { 40.0 / 720.0 },
                width_pct: 0.30,
                height_pct: 0.20,
            };
            zone.accepted_media_types.push(ZoneMediaType::SolidColor);
            if banner {
                // A whole-zone rounded solid fill, not a notification Stack slot.
                zone.contention_policy = ContentionPolicy::LatestWins;
                zone.rendering_policy.backdrop_opacity = Some(1.0);
            }
            overlap.register_zone(zone);
            let content = if banner {
                ZoneContent::SolidColor(Rgba::new(1.0, 0.0, 0.0, 1.0))
            } else {
                ZoneContent::Notification(NotificationPayload {
                    text: name.clone(),
                    urgency: if index == 7 { 2 } else { 0 },
                    actions: vec![NotificationAction {
                        copy: None,
                        label: "Act".into(),
                        callback_id: Some("act".into()),
                        url: None,
                        role: None,
                    }],
                    ..Default::default()
                })
            };
            overlap
                .publish_to_zone(&name, content, "test", None, None, None)
                .unwrap();
        }
        let commands = compositor
            .collect_all_rounded_rect_cmds(&overlap, 1280.0, 720.0)
            .chrome;
        assert!(
            commands.iter().any(|c| c.x == 896.0
                && c.y == 0.0
                && c.width == 384.0
                && c.height == 144.0
                && c.color == [1.0, 0.0, 0.0, 1.0]),
            "the actual rounded banner fill must cover the sampled notification border"
        );
        let xs: Vec<_> = commands
            .iter()
            // Backdrops span the full 384px zone; action controls are inset.
            .filter(|c| (c.width - 384.0).abs() < 0.01)
            .map(|c| c.x)
            .collect();
        let expected_xs: Vec<_> = std::iter::once(0.70 * 1280.0)
            .chain((0..8).map(|i| (0.75 + i as f32 * 0.001) * 1280.0))
            .collect();
        if xs != expected_xs {
            failures.push(format!("SDF order reverse={reverse}: {xs:?}"));
        }
        let texts = compositor.collect_text_items(&overlap, 1280.0, 720.0);
        let bodies: Vec<_> = texts
            .iter()
            .filter(|t| t.text.starts_with("notification-area-"))
            .map(|t| t.text.to_string())
            .collect();
        if bodies != expected_names {
            failures.push(format!("text order reverse={reverse}: {bodies:?}"));
        }
        compositor.prime_markdown_cache(&overlap);
        compositor.prime_truncation_cache(&overlap);
        compositor.render_frame_headless(&mut overlap, &surface);
        let pixels = surface.read_pixels(&compositor.device);
        let offset = (40 * 1280 + 1144) * 4;
        let border = &pixels[offset..offset + 4];
        eprintln!(
            "[ZONE-ORDER-PIXEL] reverse={reverse} x1144row40={border:?} SDFbackdrop_xs={xs:?}"
        );
        if !border
            .iter()
            .zip([0u8, 255, 0, 255])
            .all(|(a, b)| a.abs_diff(b) <= 3)
        {
            failures.push(format!("overlap border reverse={reverse}: {border:?}"));
        }
        compositor.populate_zone_hit_regions(&mut overlap, 1280.0, 720.0);
        let mut keyboard: Vec<_> = overlap.overlay.zone_hit_regions.iter().collect();
        keyboard.sort_by_key(|r| r.tab_order);
        let keyboard_names: Vec<_> = keyboard
            .iter()
            .filter(|r| matches!(r.kind, ZoneInteractionKind::Action { .. }))
            .map(|r| r.zone_name.clone())
            .collect();
        assert_eq!(
            keyboard_names, expected_names,
            "keyboard order stays ascending"
        );
        let top_action = overlap
            .overlay
            .zone_hit_regions
            .iter()
            .find(|r| {
                r.zone_name == "notification-area-07"
                    && matches!(r.kind, ZoneInteractionKind::Action { .. })
            })
            .unwrap()
            .clone();
        let (x, y) = (
            top_action.bounds.x + top_action.bounds.width / 2.0,
            top_action.bounds.y + top_action.bounds.height / 2.0,
        );
        if !matches!(overlap.hit_test(x,y), HitResult::ZoneInteraction { zone_name, kind: ZoneInteractionKind::Action { index: 0 }, .. } if zone_name == "notification-area-07")
        {
            failures.push(format!(
                "pointer must pick later-painted zone reverse={reverse}: {:?}",
                overlap.hit_test(x, y)
            ));
        }
        // Preserve first-control precedence inside the winning zone, and the
        // existing runtime controls that are inserted before/after this block.
        let close = overlap
            .overlay
            .zone_hit_regions
            .iter_mut()
            .find(|r| {
                r.zone_name == "notification-area-07"
                    && matches!(r.kind, ZoneInteractionKind::Dismiss)
            })
            .unwrap();
        close.bounds = top_action.bounds;
        if !matches!(overlap.hit_test(x,y), HitResult::ZoneInteraction { zone_name, kind: ZoneInteractionKind::Dismiss, .. } if zone_name == "notification-area-07")
        {
            failures.push(format!("within-card priority reverse={reverse}"));
        }
        let mut runtime = top_action.clone();
        runtime.zone_name = "__chrome_jump_to_latest__".into();
        runtime.kind = ZoneInteractionKind::JumpToLatest {
            tile_id: SceneId::null(),
        };
        overlap.overlay.zone_hit_regions.push(runtime.clone());
        if !matches!(
            overlap.hit_test(x, y),
            HitResult::ZoneInteraction {
                kind: ZoneInteractionKind::Dismiss,
                ..
            }
        ) {
            failures.push("appended runtime control changed priority".into());
        }
        runtime.kind = ZoneInteractionKind::DismissTile {
            tile_id: SceneId::null(),
        };
        overlap.overlay.zone_hit_regions.insert(0, runtime);
        assert!(matches!(
            overlap.hit_test(x, y),
            HitResult::ZoneInteraction {
                kind: ZoneInteractionKind::DismissTile { .. },
                ..
            }
        ));
        overlap.set_owner_hidden(tze_hud_scene::graph::SurfaceClasses {
            zones: true,
            ..Default::default()
        });
        assert!(
            compositor
                .collect_all_rounded_rect_cmds(&overlap, 1280.0, 720.0)
                .chrome
                .is_empty()
        );
        assert!(
            compositor
                .collect_text_items(&overlap, 1280.0, 720.0)
                .is_empty()
        );
        compositor.populate_zone_hit_regions(&mut overlap, 1280.0, 720.0);
        assert!(overlap.overlay.zone_hit_regions.is_empty());
    }
    assert!(
        failures.is_empty(),
        "same-layer order failures: {failures:?}"
    );
}

/// Pixel check of the SDF border on a rounded card (fullscreen, opaque border
/// token): just inside the rounded corner is border colour, the middle is
/// fill, and the bounding-box corner outside the curve stays clear.
#[tokio::test]
async fn test_rounded_card_border_follows_the_corner() {
    let built = make_compositor_and_surface(1280, 720).await;
    if std::env::var("TZE_HUD_REQUIRE_GPU").as_deref() == Ok("1") {
        assert!(
            built.is_some(),
            "required hairline GPU constructor must succeed"
        );
    }
    let (mut compositor, surface) = require_gpu!(built);
    let mut tokens = HashMap::new();
    tokens.insert(
        "color.notification.outline".to_string(),
        "#00FF00".to_string(),
    );
    tokens.insert(
        "color.notification.container.low".to_string(),
        "#FF0000".to_string(),
    );
    compositor.set_token_map(tokens);
    let mut scene = bordered_notification_scene(Some(16.0));
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 1280 + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    };
    // Zone top-left (960, 14.4), radius 16. In the corner box, the pixel whose
    // centre is closest to 0.5 px inside the curve (mid-border, 1 px wide) on
    // the curved part of the edge (both offsets past 4 px) is border.
    let (x0, y0, r) = (960.0_f32, 14.4_f32, 16.0_f32);
    let sdf = |cx: f32, cy: f32| {
        let (qx, qy) = (x0 + r - cx, y0 + r - cy);
        (qx * qx + qy * qy).sqrt() - r
    };
    let border_px = (964..976)
        .flat_map(|x| (19..30).map(move |y| (x, y)))
        .min_by(|a, b| {
            let da = (sdf(a.0 as f32 + 0.5, a.1 as f32 + 0.5) + 0.5).abs();
            let db = (sdf(b.0 as f32 + 0.5, b.1 as f32 + 0.5) + 0.5).abs();
            da.total_cmp(&db)
        })
        .unwrap();
    let border = px(border_px.0, border_px.1);
    assert!(
        border[1] > 150 && border[1] > border[0] + 60,
        "corner pixel {border_px:?} inside the radius must be border green, got {border:?}"
    );
    let slot_h = compositor
        .collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0)
        .chrome[0]
        .height;
    let fill = px(960 + 184, (y0 + slot_h * 0.5) as usize);
    assert!(
        fill[0] > 150 && fill[1] < 100,
        "card middle must be fill red, got {fill:?}"
    );
    let outside = px(960, 14 + 1);
    let clear = px(10, 700);
    assert_eq!(outside, clear, "outside the curve stays clear");

    // The straight top edge is one device-pixel row at fractional y=14.4.
    // Read away from corner AA, text and dismiss chrome; the existing middle
    // fill oracle supplies the exact fill-only RGBA reference.
    let top_border = px(1144, 14);
    let below_border = px(1144, 15);
    let interior = px(1144, 16);
    eprintln!(
        "[SDF-HAIRLINE-RGBA] x1144 row14={top_border:?} row15={below_border:?} row16={interior:?} fill={fill:?} actual_gpu_readback=true"
    );
    assert!(
        below_border
            .iter()
            .zip(interior)
            .all(|(actual, expected)| actual.abs_diff(expected) <= 3),
        "row15 must match row16 fill only, not a second hairline row: {below_border:?}, row16 {interior:?}"
    );
    assert!(
        interior
            .iter()
            .zip(fill)
            .all(|(actual, expected)| actual.abs_diff(expected) <= 3),
        "row16 must retain fill RGBA: {interior:?}, fill {fill:?}"
    );
    assert!(
        top_border
            .iter()
            .zip([0, 255, 0, 255])
            .all(|(actual, expected)| actual.abs_diff(expected) <= 3),
        "row14 must cover the single green device-pixel hairline: {top_border:?}"
    );
}

/// An alert banner whose `color.alert.outline` and `color.severity.*` are
/// transparent (Classic's legacy strip) draws neither a hairline nor a
/// severity mark: only its flat backdrop quad.
#[tokio::test]
async fn test_alert_banner_transparent_roles_draw_no_border_or_mark() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.set_token_map(
        [
            "color.alert.outline",
            "color.severity.info",
            "color.severity.warning",
            "color.severity.critical",
        ]
        .into_iter()
        .map(|k| (k.to_owned(), "#00000000".to_owned()))
        .collect(),
    );

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert banner — no border".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.07,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(20.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            backdrop_opacity: Some(1.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                progress: None,
                text: "no border here".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // alert-banner: only 6 vertices (one backdrop quad) and no SDF border.
    assert_eq!(
        vertices.len(),
        6,
        "alert-banner must emit exactly 6 vertices (backdrop only), got {}",
        vertices.len()
    );
    let rr = compositor.collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0);
    assert!(
        rr.chrome.is_empty(),
        "alert-banner must emit no border: {:?}",
        rr.chrome
    );
}

/// The card hairline is its urgency tone's outline role: neutral cards
/// `color.notification.outline`, urgency 2
/// `color.notification.caution.outline`, urgency 3
/// `color.notification.error.outline`.
#[tokio::test]
async fn test_notification_area_border_uses_border_default_token() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    for (urgency, key) in [
        (0, "color.notification.outline"),
        (2, "color.notification.caution.outline"),
        (3, "color.notification.error.outline"),
    ] {
        // Install the role as pure cyan (#00FFFF).
        compositor.set_token_map(HashMap::from([(key.to_string(), "#00FFFF".to_string())]));
        let mut scene = bordered_notification_scene(None);
        if let ZoneContent::Notification(n) = &mut scene
            .zone_registry
            .active_publishes
            .get_mut("notification-area")
            .unwrap()[0]
            .content
        {
            n.urgency = urgency;
        }
        let cmds = compositor
            .collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0)
            .chrome;
        let border = cmds[0].border.expect("card border").color;
        assert!(
            border[0] < 0.1 && border[1] > 0.9 && border[2] > 0.9,
            "urgency {urgency} hairline must be the cyan {key}, got {border:?}"
        );
    }
}

// ── Token-resolved severity color tests ───────────────────────────────────

/// The alert roles override their canonical values (info covers urgency 0
/// and 1); an invalid value is ignored and the canonical value stays. A
/// transparent `color.severity.*` draws no mark.
#[test]
fn test_alert_role_tokens_override_or_fall_back() {
    // (urgencies, token, value, expected linear rgb within 0.1)
    let cases: [(&[u32], &str, &str, [f32; 3]); 4] = [
        (
            &[2],
            "color.alert.caution.container",
            "#00FF00",
            [0.0, 1.0, 0.0],
        ),
        (
            &[3],
            "color.alert.error.container",
            "#FF00FF",
            [1.0, 0.0, 1.0],
        ),
        (&[0, 1], "color.alert.container", "#FF0000", [1.0, 0.0, 0.0]),
        // Canonical #222833: linear ~(0.016, 0.021, 0.033).
        (
            &[2],
            "color.alert.caution.container",
            "not-a-color",
            [0.016, 0.021, 0.033],
        ),
    ];
    for (urgencies, key, value, expected) in cases {
        let tokens = HashMap::from([(key.to_string(), value.to_string())]);
        let style =
            NotificationStyle::for_zone("alert-banner", &tokens, &RenderingPolicy::default());
        for &urgency in urgencies {
            let c = style.tone(urgency).fill;
            for (got, want) in [c.r, c.g, c.b].into_iter().zip(expected) {
                assert!(
                    (got - want).abs() < 0.1,
                    "{key}={value} urgency={urgency}: got {c:?}"
                );
            }
        }
    }
    let hidden = HashMap::from([(
        "color.severity.warning".to_string(),
        "#00000000".to_string(),
    )]);
    let style = NotificationStyle::for_zone("alert-banner", &hidden, &RenderingPolicy::default());
    assert!(style.mark(2, false).is_none(), "transparent tint: no mark");
    assert!(style.mark(3, false).is_some());
    assert!(
        style.mark(3, true).is_none(),
        "an agent icon takes the column"
    );
    // Cards never draw a mark.
    assert!(
        NotificationStyle::resolve(&HashMap::new(), &RenderingPolicy::default())
            .mark(3, false)
            .is_none()
    );
}

/// Custom alert role tokens in [design_tokens] affect alert-banner backdrop colors.
///
/// This is the end-to-end integration test: `set_token_map` populates the
/// compositor, and `render_zone_content` uses the token-resolved color for the
/// alert-banner backdrop.
#[tokio::test]
async fn test_custom_alert_tokens_affect_alert_banner_backdrop() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    // Install a custom token map: override warning with pure green (#00FF00).
    let mut token_map = HashMap::new();
    token_map.insert(
        "color.alert.caution.container".to_string(),
        "#00FF00".to_string(),
    );
    compositor.set_token_map(token_map);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert banner zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.07,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(20.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            backdrop_opacity: Some(1.0),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish urgency=2 (warning) — should use custom green token.
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                progress: None,
                text: "Custom token warning".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // Collect vertices from render_zone_content.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // rect_vertices emits 6 vertices per quad; each vertex has a `color: [f32; 4]` field.
    // The backdrop should be green (R~0.0, G~1.0, B~0.0), not amber.
    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for alert-banner"
    );

    // Check first vertex color. RectVertex layout: [position: [f32; 2], color: [f32; 4]].
    let first = &vertices[0];
    assert!(
        first.color[1] > 0.9,
        "alert-banner backdrop G should be ~1.0 (custom green token), got {}",
        first.color[1]
    );
    assert!(
        first.color[0] < 0.1,
        "alert-banner backdrop R should be ~0.0 (custom green token), got {}",
        first.color[0]
    );
    assert!(
        first.color[2] < 0.1,
        "alert-banner backdrop B should be ~0.0 (custom green token), got {}",
        first.color[2]
    );
}

/// ZoneAnimationState fade-in reaches 1.0 after duration elapses.
#[test]
fn test_zone_animation_state_fade_in_completes() {
    // Use 0ms duration for instant completion.
    let state = ZoneAnimationState::fade_in(0);
    // Opacity at duration=0 should immediately be target (1.0).
    let opacity = state.current_opacity();
    assert_eq!(opacity, 1.0, "fade-in with 0ms should be 1.0 immediately");
    assert!(
        state.is_complete(),
        "0ms fade-in should be complete immediately"
    );
}

/// ZoneAnimationState fade-out starts at 1.0 and reaches 0.0 after duration.
#[test]
fn test_zone_animation_state_fade_out_completes() {
    let state = ZoneAnimationState::fade_out(0);
    let opacity = state.current_opacity();
    assert_eq!(opacity, 0.0, "fade-out with 0ms should be 0.0 immediately");
    assert!(
        state.is_complete(),
        "0ms fade-out should be complete immediately"
    );
}

/// ZoneAnimationState with non-zero duration: opacity is interpolated.
#[test]
fn test_zone_animation_state_interpolates() {
    let now = std::time::Instant::now();
    let state = ZoneAnimationState::fade_in_at(10_000, now);
    let opacity = state.current_opacity_at(now);
    assert!(
        (0.0..=0.1).contains(&opacity),
        "fade-in opacity shortly after start should be near 0, got {opacity}"
    );
    assert!(
        !state.is_complete_at(now),
        "10s fade-in should not be complete immediately"
    );
    let midpoint = now + std::time::Duration::from_millis(5_000);
    assert_eq!(state.current_opacity_at(midpoint), 0.5);
    let interrupted =
        ZoneAnimationState::fade_in_from_at(100, state.current_opacity_at(midpoint), midpoint);
    assert_eq!(interrupted.current_opacity_at(midpoint), 0.5);
    assert_eq!(
        interrupted.current_opacity_at(midpoint + std::time::Duration::from_millis(50)),
        0.75
    );
    let end = now + std::time::Duration::from_millis(10_000);
    assert_eq!(state.current_opacity_at(end), 1.0);
    assert!(state.is_complete_at(end));
}

/// backdrop_opacity overrides the backdrop color's alpha channel.
/// When backdrop_opacity=0.6 and backdrop.a=1.0, effective alpha=0.6.
#[tokio::test]
async fn test_backdrop_opacity_overrides_color_alpha() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // backdrop color has alpha=1.0 but backdrop_opacity=0.6 should override it.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "test backdrop opacity override".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)), // alpha=1.0
            backdrop_opacity: Some(0.6),                   // override to 0.6
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("opacity test".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // The backdrop rendered should use alpha=0.6 (backdrop_opacity), not 1.0.
    // We verify this by checking the vertex colors produced — the alpha channel
    // of the first rect vertex should reflect 0.6.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert!(!vertices.is_empty(), "expected backdrop vertices");
    // The RectVertex has color field [f32; 4]; alpha should be ~0.6.
    let alpha = vertices[0].color[3];
    assert!(
        (alpha - 0.6).abs() < 0.01,
        "backdrop alpha should be ~0.6 (backdrop_opacity override), got {alpha}"
    );
}

/// backdrop=None: no backdrop quad rendered even when backdrop_opacity is set.
#[tokio::test]
async fn test_no_backdrop_when_backdrop_is_none() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "no-backdrop test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: None,
            backdrop_opacity: Some(0.9), // ignored because backdrop is None
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("no backdrop".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // With backdrop=None, no rect vertices should be emitted.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert!(
        vertices.is_empty(),
        "no backdrop quad should be rendered when policy.backdrop is None"
    );
}

/// backdrop=None with Notification content: no backdrop or border quads rendered.
///
/// Even though Notification content in a non-alert-banner zone overrides the
/// backdrop color with urgency-tinted tokens, the override must respect the
/// policy.backdrop contract: when backdrop is None, nothing is emitted.
#[tokio::test]
async fn test_notification_no_backdrop_when_backdrop_is_none() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area with backdrop=None".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: None,
            backdrop_opacity: Some(0.9),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                progress: None,
                text: "no backdrop".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert!(
        vertices.is_empty(),
        "no backdrop or border quads should be rendered when policy.backdrop is None, got {} vertices",
        vertices.len()
    );

    // Notification borders use the SDF pass, separate from flat backdrop vertices.
    let rounded_rects = compositor.collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0);
    assert!(
        rounded_rects.background.is_empty()
            && rounded_rects.content.is_empty()
            && rounded_rects.chrome.is_empty(),
        "no rounded-rect commands should be rendered when policy.backdrop is None"
    );
}

/// text.rs: TextItem::from_zone_policy respects all RenderingPolicy fields.
#[test]
fn test_from_zone_policy_reads_all_policy_fields() {
    use crate::text::TextItem;

    let policy = RenderingPolicy {
        font_size_px: Some(28.0),
        text_color: Some(Rgba::new(1.0, 0.5, 0.0, 1.0)), // orange
        font_family: Some(FontFamily::SystemMonospace),
        text_align: Some(TextAlign::Center),
        outline_color: Some(Rgba::BLACK),
        outline_width: Some(1.5),
        margin_horizontal: Some(12.0),
        margin_vertical: Some(6.0),
        ..Default::default()
    };

    let item = TextItem::from_zone_policy("test", 0.0, 0.0, 400.0, 100.0, &policy, 1.0, 1.0);
    assert_eq!(item.font_size_px, 28.0);
    assert_eq!(item.font_family, FontFamily::SystemMonospace);
    assert_eq!(item.alignment, TextAlign::Center);
    assert!(item.outline_color.is_some(), "outline_color should be set");
    assert_eq!(item.outline_width.unwrap(), 1.5);
    // Margins: x+12, y+6
    assert_eq!(item.pixel_x, 12.0);
    assert_eq!(item.pixel_y, 6.0);
}
