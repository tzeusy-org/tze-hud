use super::*;
use crate::test_pixels::pixel_at;

fn scroll_text(content: &str, bounds: Rect, overflow: TextOverflow) -> Node {
    leaf(NodeData::TextMarkdown(TextMarkdownNode {
        font_family: FontFamily::SystemMonospace,
        overflow,
        ..markdown_text(content, bounds, 14.0)
    }))
}

#[tokio::test]
async fn icon_texture_retries_missing_tokens_and_rasterizes_at_the_current_token_size() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(64, 64).await);
    let path = std::env::temp_dir().join(format!("tze_hud_scroll_icon_{}.svg", std::process::id()));
    std::fs::write(
        &path,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32">
<rect width="32" height="32" fill="{{token.color.primary}}"/>
</svg>"#,
    )
    .unwrap();
    let icon = path.to_str().unwrap();
    let id = ResourceId::of(icon.as_bytes());
    assert!(!compositor.ensure_icon_texture(icon));
    assert!(compositor.failed_icon_paths.contains(&id));
    compositor.set_token_map([("color.primary".into(), "#ff0000".into())].into());
    assert!(!compositor.failed_icon_paths.contains(&id));
    assert!(compositor.ensure_icon_texture(icon));
    assert_eq!(compositor.image_texture_cache[&id].width, 14);

    let mut tokens = compositor.token_map.clone();
    tokens.insert("size.status.icon".into(), "20".into());
    compositor.set_token_map(tokens);
    assert!(compositor.ensure_icon_texture(icon));
    assert_eq!(compositor.image_texture_cache[&id].width, 20);
    std::fs::remove_file(&path).unwrap();
}

#[tokio::test]
async fn scroll_viewports_and_truncation_cache_select_the_same_edge() {
    use crate::overflow::TruncationViewport::{HeadAnchored, TailAnchored};

    let (mut compositor, _surface) = require_gpu!(make_text_compositor(720, 360).await);
    let content = "FIRST\nbbbb\ncccc\ndddd\neeee\nffff\ngggg\nLAST";
    let mut observed = Vec::new();
    for (zone, overflow, opt_in, viewport) in [
        (false, TextOverflow::Ellipsis, Some(false), HeadAnchored),
        (false, TextOverflow::Ellipsis, Some(true), TailAnchored),
        (false, TextOverflow::Clip, Some(true), HeadAnchored),
        (true, TextOverflow::Ellipsis, None, HeadAnchored),
        (true, TextOverflow::Ellipsis, Some(true), TailAnchored),
        (true, TextOverflow::Ellipsis, Some(false), HeadAnchored),
        (true, TextOverflow::Clip, Some(true), TailAnchored),
    ] {
        let at_tail = opt_in == Some(true);
        let scene = if zone {
            make_stream_zone_scene(Some(overflow), opt_in, content)
        } else {
            let mut scene = SceneGraph::new(720.0, 360.0);
            let bounds = Rect::new(0.0, 0.0, 200.0, 40.0);
            let tile = scrollable_tile(&mut scene, "viewport-test", bounds, Some(200.0));
            scene
                .set_tile_root(tile, scroll_text(content, bounds, overflow))
                .unwrap();
            scene.set_tile_follow_tail_at_tail(tile, at_tail);
            scene
        };
        compositor.prime_markdown_cache(&scene);
        compositor.resize_reprime_last_at = None;
        compositor.prime_truncation_cache(&scene);
        let items = compositor.collect_text_items(&scene, 720.0, 360.0);
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!((item.overflow, item.viewport), (overflow, viewport));
        let key = crate::text::effective_truncation_key(item);
        if overflow == TextOverflow::Clip {
            assert!(key.is_none());
            continue;
        }

        let key = key.expect("ellipsis measures a truncated range");
        let cached = compositor
            .text_rasterizer
            .as_ref()
            .unwrap()
            .truncation_cache
            .get_by_key(&key)
            .unwrap();
        let (included, excluded) = if at_tail {
            ("LAST", "FIRST")
        } else {
            ("FIRST", "LAST")
        };
        assert!(
            cached.was_truncated
                && cached.text.contains(included)
                && !cached.text.contains(excluded),
            "zone {zone}, {viewport:?}: {cached:?}"
        );
        observed.push(key);
    }
    for (owner, first) in [("tile", 0), ("zone", 2)] {
        assert_ne!(
            observed[first],
            observed[first + 1],
            "{owner}: head and tail cache keys"
        );
    }
}

#[tokio::test]
async fn scrolling_moves_text_origin_without_moving_its_clip() {
    let (compositor, _) = require_gpu!(make_compositor_and_surface(720, 360).await);
    let tile_bounds = Rect::new(100.0, 50.0, 400.0, 200.0);
    let node_bounds = Rect::new(10.0, 20.0, 300.0, 24.0);
    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab = scene.create_tab("test", 0).unwrap();
    let lease = scene.grant_lease("scroll-test", 120_000);
    let tile = scene
        .create_tile(tab, "scroll-test", lease, tile_bounds, 1)
        .unwrap();
    let content = "scroll test line";
    scene
        .set_tile_root(tile, scroll_text(content, node_bounds, TextOverflow::Clip))
        .unwrap();

    let original = compositor.collect_text_items(&scene, 720.0, 360.0);
    let [original] = original.as_slice() else {
        panic!("one unscrolled text item")
    };
    assert_eq!(original.text.as_ref(), content);
    assert!(original.pixel_y >= tile_bounds.y + node_bounds.y);
    assert!(!compositor.scroll_smoothing_enabled);

    scene
        .register_tile_scroll_config(
            tile,
            TileScrollConfig {
                content_height: Some(600.0),
                ..TileScrollConfig::vertical()
            },
        )
        .unwrap();
    scene.set_tile_scroll_offset_local(tile, 0.0, 80.0).unwrap();
    assert_eq!(
        compositor.display_tile_scroll_offset(&scene, tile),
        (0.0, 80.0)
    );
    let scrolled = compositor.collect_text_items(&scene, 720.0, 360.0);
    let [scrolled] = scrolled.as_slice() else {
        panic!("one scrolled text item")
    };
    assert_eq!(scrolled.text.as_ref(), content);
    assert_eq!(scrolled.pixel_x, original.pixel_x);
    assert!((original.pixel_y - scrolled.pixel_y - 80.0).abs() < 0.01);
    assert_eq!(
        (scrolled.clip_pixel_y, scrolled.clip_bounds_height),
        (original.clip_pixel_y, original.clip_bounds_height)
    );
}

#[tokio::test]
async fn jump_to_latest_badge_gates_on_scroll_and_unread_count() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(480, 320).await);

    let mut scene = SceneGraph::new(480.0, 320.0);
    let bounds = Rect::new(0.0, 0.0, 400.0, 200.0);
    let tile_id = scrollable_tile(&mut scene, "badge-test", bounds, Some(800.0));
    let transcript = scroll_text(
        &"The viewport has more content than it can show.\n".repeat(25),
        bounds,
        TextOverflow::Clip,
    );
    scene.set_tile_root(tile_id, transcript).unwrap();
    compositor.set_token_map(token_overrides([("portal.transition.in_ms", "0")]));
    let tab_id = scene.tiles[&tile_id].tab_id;

    let indicator = super::token_colors::resolve_scroll_indicator_tokens(&HashMap::new());
    assert_eq!(indicator, tze_hud_input::ScrollIndicatorTokens::default());
    assert_eq!(
        indicator.width_px,
        tze_hud_scene::portal_defaults::number(
            tze_hud_scene::portal_defaults::SCROLL_INDICATOR_WIDTH_PX
        )
    );
    scene.set_tile_follow_tail_at_tail(tile_id, false);
    scene.set_tile_unread_count(tile_id, 3);
    let (_, items) = compositor.collect_scroll_affordances(&scene);
    let [item] = items.as_slice() else {
        panic!("expected one unread label, got {}", items.len());
    };
    assert_eq!(
        (item.text.as_ref(), item.alignment),
        ("3 unread", TextAlign::Center)
    );
    let clip = Rect::new(
        item.clip_pixel_x,
        item.clip_pixel_y,
        item.clip_bounds_width,
        item.clip_bounds_height,
    );
    assert!(clip.width <= 400.0 && clip.height <= 200.0 && item.pixel_y >= 100.0);

    let badge_interior = || {
        let left = clip.x as u32 + 2;
        let right = (clip.x + clip.width) as u32 - 2;
        let top = clip.y as u32 + 2;
        let bottom = (clip.y + clip.height) as u32 - 2;
        (top..bottom).flat_map(move |y| (left..right).map(move |x| (x, y)))
    };
    let capture = |c: &mut Compositor, s: &mut SceneGraph| {
        prime_text(c, s);
        render_pixels(c, s, &surface)
    };

    // Change both OUTPUT fill and glyphs below the opaque badge.
    let [red, blue] = [
        (String::new(), Rgba::new(1.0, 0.0, 0.0, 1.0)),
        (
            "WWWWWWWWWWWWWWWWWWWWWWWWWWWW\n".repeat(15),
            Rgba::new(0.0, 0.0, 1.0, 1.0),
        ),
    ]
    .map(|(content, background)| {
        let text = TextMarkdownNode {
            background: Some(background),
            font_family: FontFamily::SystemMonospace,
            ..markdown_text(content, bounds, 14.0)
        };
        scene
            .set_tile_root(tile_id, leaf(NodeData::TextMarkdown(text)))
            .unwrap();
        compositor.update_portal_tile_animations(&scene);
        compositor.portal_tile_anim_states.clear();
        capture(&mut compositor, &mut scene)
    });
    assert_ne!(pixel_at(&red, 480, 20, 20), pixel_at(&blue, 480, 20, 20));
    for (x, y) in badge_interior() {
        assert_eq!(
            pixel_at(&red, 480, x, y),
            pixel_at(&blue, 480, x, y),
            "the badge masks changing OUTPUT pixels at {x},{y}"
        );
    }

    let lease = scene.grant_lease("badge-cover", 120_000);
    let cover = scene
        .create_tile(tab_id, "badge-cover", lease, clip, 2)
        .unwrap();
    scene
        .set_tile_root(
            cover,
            solid_node(
                Rect::new(0.0, 0.0, clip.width, clip.height),
                Rgba::new(0.0, 1.0, 0.0, 1.0),
                None,
            ),
        )
        .unwrap();
    let capture_with_and_without_badge = |c: &mut Compositor, s: &mut SceneGraph| {
        assert!(!s.tile_follow_tail_at_tail(tile_id));
        let visible = capture(c, s);
        s.set_tile_follow_tail_at_tail(tile_id, true);
        let hidden = capture(c, s);
        s.set_tile_follow_tail_at_tail(tile_id, false);
        (visible, hidden)
    };
    let (covered, without_badge) = capture_with_and_without_badge(&mut compositor, &mut scene);
    for (x, y) in badge_interior() {
        assert_eq!(
            pixel_at(&covered, 480, x, y),
            pixel_at(&without_badge, 480, x, y),
            "the higher tile masks the badge at {x},{y}"
        );
    }
    let assert_regions = |c: &Compositor, scene: &mut SceneGraph, visible: bool, edge: f32| {
        let (rects, labels) = c.collect_scroll_affordances(scene);
        c.populate_zone_hit_regions(scene, 480.0, 320.0);
        let hits: Vec<_> = scene
            .overlay
            .zone_hit_regions
            .iter()
            .filter(|r| r.kind == (ZoneInteractionKind::JumpToLatest { tile_id }))
            .collect();
        assert_eq!(
            [!rects.is_empty(), !labels.is_empty(), !hits.is_empty()],
            [visible; 3]
        );
        assert!(labels.iter().all(|label| label.pixel_x == item.pixel_x
            && label.clip_pixel_x + label.clip_bounds_width <= edge));
        assert!(
            hits.iter()
                .all(|hit| hit.bounds.x + hit.bounds.width <= edge)
        );
    };
    assert_regions(&compositor, &mut scene, false, f32::INFINITY);
    for (dragged, visible) in [(tile_id, true), (cover, false)] {
        scene.set_drag_active(dragged);
        assert_regions(&compositor, &mut scene, visible, f32::INFINITY);
        scene.clear_drag_active(dragged);
    }
    scene.tiles.get_mut(&cover).unwrap().opacity = 0.0;
    assert_regions(&compositor, &mut scene, true, f32::INFINITY);
    scene.tiles.get_mut(&cover).unwrap().opacity = 1.0;
    scene.set_owner_hidden(tze_hud_scene::graph::visibility::SurfaceClasses {
        tiles: true,
        ..Default::default()
    });
    assert_regions(&compositor, &mut scene, true, f32::INFINITY);
    scene.set_owner_hidden(Default::default());
    let split = item.clip_pixel_x + item.clip_bounds_width / 2.0;
    scene.tiles.get_mut(&cover).unwrap().bounds.x = split;
    assert_regions(&compositor, &mut scene, true, split);
    let (partial, without_badge) = capture_with_and_without_badge(&mut compositor, &mut scene);
    for (x, y) in badge_interior() {
        let expected = if (x as f32) < split {
            &blue
        } else {
            &without_badge
        };
        assert_eq!(
            pixel_at(&partial, 480, x, y),
            pixel_at(expected, 480, x, y),
            "partially covered badge at {x},{y}"
        );
    }
    scene.delete_tile(cover, "badge-cover").unwrap();

    scene.set_tile_unread_count(tile_id, 0);
    let (rects, labels) = compositor.collect_scroll_affordances(&scene);
    assert!(
        !rects.is_empty() && labels.is_empty(),
        "plain tile keeps its jump control"
    );
    scene
        .set_portal_surface(
            tile_id,
            tze_hud_scene::types::PortalSurface {
                parts: vec![tze_hud_scene::types::PortalPart {
                    kind: tze_hud_scene::types::PortalPartKind::Transcript,
                    bounds,
                    node: scene.tiles[&tile_id].root_node,
                }],
                ..Default::default()
            },
            "badge-test",
        )
        .unwrap();
    assert_regions(&compositor, &mut scene, false, f32::INFINITY);

    for unread in [1, 2] {
        scene.set_tile_unread_count(tile_id, unread);
        let (_, labels) = compositor.collect_scroll_affordances(&scene);
        let [label] = labels.as_slice() else {
            panic!("one unread label")
        };
        assert_eq!(label.text.as_ref(), format!("{unread} unread"));
        let pixels = capture(&mut compositor, &mut scene);
        let (left, top) = (label.clip_pixel_x as u32 + 2, label.clip_pixel_y as u32 + 2);
        let right = (label.clip_pixel_x + label.clip_bounds_width) as u32 - 2;
        let bottom = (label.clip_pixel_y + label.clip_bounds_height) as u32 - 2;
        let fill = pixel_at(&pixels, 480, left, top);
        let glyphs = (top..bottom)
            .flat_map(|y| (left..right).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                pixel_at(&pixels, 480, x, y)[..3]
                    .iter()
                    .zip(fill)
                    .any(|(pixel, background)| pixel.saturating_sub(background) > 32)
            })
            .count();
        assert!(glyphs > 12, "{unread} unread paints readable glyphs");
    }

    scene.set_tile_unread_count(tile_id, 0);
    let (backscroll, at_tail) = capture_with_and_without_badge(&mut compositor, &mut scene);
    assert_eq!(backscroll, at_tail, "no empty badge paints on backscroll");

    scene.set_tile_unread_count(tile_id, 5);
    scene.set_tile_follow_tail_at_tail(tile_id, true);
    assert_regions(&compositor, &mut scene, false, f32::INFINITY);
}

#[tokio::test]
async fn scrolled_portal_output_clips_flat_and_rounded_geometry() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(220, 180).await);
    let mut scene = SceneGraph::new(220.0, 180.0);
    let viewport = Rect::new(90.0, 60.0, 80.0, 60.0);
    let owner = "portal-output-clip";
    let output = scrollable_tile(&mut scene, owner, viewport, Some(240.0));
    let frame = scene
        .create_tile(
            scene.tiles[&output].tab_id,
            owner,
            scene.tiles[&output].lease_id,
            Rect::new(40.0, 20.0, 160.0, 140.0),
            0,
        )
        .unwrap();
    let frame_color = Rgba::new(0.28, 0.34, 0.50, 1.0);
    scene
        .set_tile_root(
            frame,
            solid_node(Rect::new(0.0, 0.0, 160.0, 140.0), frame_color, None),
        )
        .unwrap();
    let root = solid_node(Rect::new(0.0, 0.0, 80.0, 60.0), Rgba::BLACK, None);
    let root_id = root.id;
    scene.set_tile_root(output, root).unwrap();
    let child = Rect::new(0.0, 92.0, 80.0, 80.0);
    let child_node = solid_node(child, Rgba::BLACK, None);
    let child_id = child_node.id;
    scene
        .add_node_to_tile(output, Some(root_id), child_node)
        .unwrap();
    let render = |compositor: &mut Compositor, scene: &mut SceneGraph| {
        prime_text(compositor, scene);
        compositor.render_frame_headless(scene, &surface);
    };
    // Commit the initial appearance so readback checks steady full opacity.
    render(&mut compositor, &mut scene);
    compositor.portal_tile_anim_states.clear();
    for scroll_y in [0.0, 48.0, 96.0] {
        scene
            .set_tile_scroll_offset_local(output, 0.0, scroll_y)
            .unwrap();
        render(&mut compositor, &mut scene);
        let pixels = surface.read_pixels(&compositor.device);
        let px = |x, y| pixel_at(&pixels, 220, x, y);
        let frame = px(55, 45);
        assert_eq!(px(110, 45), frame, "above viewport at {scroll_y}");
        assert_eq!(px(110, 135), frame, "below viewport at {scroll_y}");
        assert_ne!(px(110, 70), frame, "inside viewport at {scroll_y}");
    }
    scene
        .update_node_content_checked(
            output,
            child_id,
            NodeData::SolidColor(SolidColorNode {
                bounds: child,
                color: Rgba::BLACK,
                radius: Some(16.0),
            }),
            owner,
        )
        .unwrap();
    let commands = compositor.collect_tile_rounded_rect_cmds(&scene);
    let [rounded] = commands.as_slice() else {
        panic!("expected exactly one rounded child")
    };
    assert_eq!(
        Rect::new(rounded.x, rounded.y, rounded.width, rounded.height),
        Rect::new(90.0, 56.0, 80.0, 80.0)
    );
    assert_eq!(rounded.radius, 16.0);
    let clip = rounded.clip.unwrap();
    assert_eq!(Rect::new(clip.x, clip.y, clip.width, clip.height), viewport);
}

#[tokio::test]
async fn legacy_portal_part_geometry_keeps_chrome_fixed_during_scroll() {
    use PortalPartKind::{Composer, Divider, Frame, Header, Transcript};
    let (mut compositor, _) = require_gpu!(make_text_compositor(800, 520).await);
    let mut scene = SceneGraph::new(800.0, 520.0);
    let tile_bounds = Rect::new(80.0, 40.0, 600.0, 360.0);
    let tile = scrollable_tile(&mut scene, "legacy-portal", tile_bounds, Some(1_200.0));
    let frame = Rect::new(0.0, 0.0, 600.0, 360.0);
    let header = Rect::new(0.0, 0.0, 600.0, 48.0);
    let input = Rect::new(0.0, 48.0, 294.0, 312.0);
    let output = Rect::new(306.0, 48.0, 294.0, 312.0);
    let divider = Rect::new(294.0, 48.0, 12.0, 312.0);
    let root = solid_node(frame, Rgba::BLACK, Some(14.0));
    let root_id = root.id;
    scene.set_tile_root(tile, root).unwrap();
    for pane in [input, output] {
        scene
            .add_node_to_tile(
                tile,
                Some(root_id),
                solid_node(pane, Rgba::BLACK, Some(12.0)),
            )
            .unwrap();
    }
    let labels = ["Portal header", "INPUT", "OUTPUT", "output document"];
    for (index, bounds) in [
        Rect::new(12.0, 0.0, 576.0, 48.0),
        Rect::new(12.0, 48.0, 270.0, 24.0),
        Rect::new(318.0, 48.0, 270.0, 24.0),
        Rect::new(318.0, 76.0, 270.0, 272.0),
    ]
    .into_iter()
    .enumerate()
    {
        let node = leaf(NodeData::TextMarkdown(markdown_text(
            labels[index],
            bounds,
            16.0,
        )));
        scene.add_node_to_tile(tile, Some(root_id), node).unwrap();
    }

    // Resident gRPC publishes part geometry without stable materialized nodes.
    scene.overlay.portal_surfaces.insert(
        tile,
        PortalSurface {
            display_state: PortalDisplayState::Expanded,
            parts: [
                (Frame, frame),
                (Header, header),
                (Composer, input),
                (Transcript, output),
                (Divider, divider),
            ]
            .into_iter()
            .map(|(kind, bounds)| PortalPart {
                kind,
                bounds,
                node: None,
            })
            .collect(),
            ..Default::default()
        },
    );
    compositor.prime_markdown_cache(&scene);
    let rows = |scene: &SceneGraph| {
        let items = compositor.collect_text_items(scene, 800.0, 520.0);
        labels.map(|label| {
            items
                .iter()
                .find(|item| item.text.as_ref() == label)
                .unwrap()
                .pixel_y
        })
    };
    let [header_y, input_y, output_y, document_y] = rows(&scene);
    scene.set_tile_scroll_offset_local(tile, 0.0, 72.0).unwrap();
    let [
        scrolled_header,
        scrolled_input,
        scrolled_output,
        scrolled_document,
    ] = rows(&scene);
    assert_eq!(
        [scrolled_header, scrolled_input, scrolled_output],
        [header_y, input_y, output_y]
    );
    assert!((document_y - scrolled_document - 72.0).abs() < 0.5);

    let drawn: Vec<_> = compositor
        .collect_tile_rounded_rect_cmds(&scene)
        .into_iter()
        .map(|cmd| Rect::new(cmd.x, cmd.y, cmd.width, cmd.height))
        .collect();
    for part in [frame, input, output] {
        let bounds = Rect::new(
            tile_bounds.x + part.x,
            tile_bounds.y + part.y,
            part.width,
            part.height,
        );
        assert!(
            drawn.contains(&bounds),
            "legacy chrome must stay fixed at {bounds:?}"
        );
    }
}

#[tokio::test]
async fn expanded_portal_chrome_stays_fixed_while_document_content_scrolls_after_resize() {
    let (mut compositor, _surface) = require_gpu!(make_text_compositor(800, 520).await);

    let mut scene = SceneGraph::new(800.0, 520.0);
    // Use the operator's already-resolved post-resize geometry.
    let tile_bounds = Rect::new(80.0, 40.0, 600.0, 360.0);
    let tile_id = scrollable_tile(&mut scene, "expanded-portal", tile_bounds, Some(1_200.0));

    use tze_hud_input::{InputProcessor, ScrollEvent};
    use tze_hud_portal::{
        OutputKind, TranscriptUnit,
        render::{Size, render},
        tokens::PortalVisualTokens,
    };
    let mut view = empty_portal_view("scroll-anchor", "Reading anchor");
    view.unread_output_count = Some(3);
    let unit = |sequence, output_text, output_kind| TranscriptUnit {
        sequence,
        output_text,
        output_kind,
        expects_reply: false,
        appended_at_wall_us: 0,
    };
    let retained_lines = |first| {
        (first..first + 60)
            .map(|i| format!("Line {i:02} — stable reading marker.\n\n"))
            .collect()
    };
    view.visible_transcript
        .push(unit(1, retained_lines(1), OutputKind::Assistant));
    let replies = (1..=40)
        .map(|i| format!("Input {i:02} — retained reply.\n\n"))
        .collect();
    view.input_history
        .push(unit(100, replies, OutputKind::Viewer));
    let mut processor = InputProcessor::new();
    let mut reading = None;
    let smooth_time = std::time::Instant::now();
    let delay = std::time::Duration::from_millis;
    let viewport = Size {
        width: tile_bounds.width,
        height: tile_bounds.height,
    };
    let tokens = PortalVisualTokens::default();
    let chrome = |compositor: &Compositor, scene: &SceneGraph, items: &[crate::text::TextItem]| {
        let labels = ["Reading anchor", "INPUT", "OUTPUT"].map(|label| {
            let item = items
                .iter()
                .find(|item| item.text.starts_with(label))
                .unwrap();
            (
                item.pixel_x,
                item.pixel_y,
                item.clip_pixel_y,
                item.clip_bounds_height,
            )
        });
        let rounded: Vec<_> = compositor
            .collect_tile_rounded_rect_cmds(scene)
            .iter()
            .map(|cmd| Rect::new(cmd.x, cmd.y, cmd.width, cmd.height))
            .collect();
        for part in &scene.portal_surface(tile_id).unwrap().parts {
            let mut expected = part.bounds;
            expected.x += tile_bounds.x;
            expected.y += tile_bounds.y;
            assert!(rounded.contains(&expected), "missing chrome: {expected:?}");
        }
        (labels, rounded)
    };
    let mut fixed_chrome = None;
    for step in 0..4 {
        match step {
            1 => view
                .visible_transcript
                .push(unit(2, "NEW MARKER".into(), OutputKind::Assistant)),
            // The same-size retention window cannot be rebased from height delta.
            2 | 3 => view.visible_transcript[0].output_text = retained_lines(step * 10 + 1),
            _ => {}
        }
        let rendered = render(&view, &tokens, viewport, 3_000_000);
        scene.set_tile_unread_count(tile_id, rendered.unread);
        scene
            .set_portal_surface(tile_id, rendered.surface, "expanded-portal")
            .unwrap();
        scene
            .set_tile_root_tree(tile_id, rendered.root, rendered.children)
            .unwrap();
        compositor.resize_reprime_last_at = None; // A settled native commit, outside resize cadence.
        prime_text(&mut compositor, &scene);
        compositor.publish_portal_document_layouts(&mut scene);
        for (kind, factor) in [
            (PortalPartKind::Transcript, 4.0),
            (PortalPartKind::Composer, 2.0),
        ] {
            let document = &scene.overlay.portal_document_layouts[&(tile_id, kind)];
            assert!(
                document.height > document.viewport.height * factor,
                "{kind:?} at {step}"
            );
        }
        processor.reconcile_portal_document_layouts(&mut scene);
        if step == 0 {
            let offsets = |scene: &SceneGraph| {
                [
                    scene.tile_scroll_offset_local(tile_id).1,
                    scene.portal_input_scroll_offset(tile_id).1,
                ]
            };
            for (index, (kind, delta_y)) in [
                (PortalPartKind::Transcript, -130.0),
                (PortalPartKind::Composer, -70.0),
            ]
            .into_iter()
            .enumerate()
            {
                let part = scene
                    .portal_surface(tile_id)
                    .unwrap()
                    .parts
                    .iter()
                    .find(|part| part.kind == kind)
                    .unwrap()
                    .bounds;
                let wheel = ScrollEvent {
                    x: tile_bounds.x + part.x + part.width / 2.0,
                    y: tile_bounds.y + part.y + part.height / 2.0,
                    delta_x: 0.0,
                    delta_y,
                };
                let before = offsets(&scene);
                let dispatched = processor.process_scroll_event(&wheel, &mut scene);
                let after = offsets(&scene);
                assert_eq!(
                    dispatched.is_some(),
                    index == 0,
                    "only OUTPUT reports to the agent"
                );
                assert!(
                    after[index] < before[index],
                    "{kind:?} scrolls on its own wheel"
                );
                assert_eq!(after[1 - index], before[1 - index]);
            }
            assert_eq!(
                compositor.display_tile_scroll_offset(&scene, tile_id),
                scene.tile_scroll_offset_local(tile_id)
            );
        }
        if step == 3 {
            // Main-thread input can arrive after reconciliation but before the
            // compositor consumes the changed document coordinate origin.
            processor.process_keyboard_scroll(tile_id, -40.0, &mut scene);
        }
        // Exercise the native displayed coordinate, with a deterministic 1 ms
        // frame delta. Headless raw-target assertions cannot catch origin easing.
        compositor.scroll_smoothing_enabled = true;
        compositor.update_scroll_smoothing_at(&scene, smooth_time + delay(step));
        compositor.publish_displayed_scroll_offsets(&mut scene);
        let displayed = compositor.display_tile_scroll_offset(&scene, tile_id);
        assert_eq!(scene.effective_tile_scroll_offset_local(tile_id), displayed);
        if step == 0 {
            assert_eq!(
                displayed,
                scene.tile_scroll_offset_local(tile_id),
                "first frame starts settled"
            );
            assert_eq!(
                compositor.display_tile_scroll_offset(&scene, SceneId::new()),
                (0.0, 0.0),
                "without a smoother, the displayed offset falls back to raw"
            );
        }
        let layout = &scene.overlay.portal_document_layouts[&(tile_id, PortalPartKind::Transcript)];
        let marker_byte = layout.content.find("Line 55").unwrap();
        let marker_y = layout
            .rows
            .iter()
            .rev()
            .find(|(byte, _)| *byte <= marker_byte)
            .unwrap()
            .1
            - displayed.1;
        let input_offset = scene.portal_input_scroll_offset(tile_id);
        if let Some((previous_y, previous_input)) = reading {
            let scroll = if step == 3 {
                40.0 * super::easing::exp_smooth_factor(1.0, 90.0)
            } else {
                0.0
            };
            let delta: f32 = marker_y - previous_y - scroll;
            assert!(
                delta.abs() < 0.01,
                "only the intervening wheel eases at step {step}"
            );
            assert_eq!(
                input_offset, previous_input,
                "OUTPUT changes preserve INPUT"
            );
        }
        reading = Some((marker_y, input_offset));
        let (viewport, document_height) = (layout.viewport, layout.height);
        let output_left = tile_bounds.x + viewport.x;
        let (_, labels) = compositor.collect_scroll_affordances(&scene);
        let [badge] = labels.as_slice() else {
            panic!("native OUTPUT should have one unread badge at step {step}");
        };
        assert_eq!(badge.text.as_ref(), "3 unread", "step {step}");
        assert!(
            badge.pixel_x >= output_left,
            "badge is in OUTPUT at step {step}"
        );
        compositor.populate_zone_hit_regions(&mut scene, 800.0, 520.0);
        let jump = scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|region| region.kind == (ZoneInteractionKind::JumpToLatest { tile_id }))
            .expect("native OUTPUT should have a jump-to-latest hit target");
        assert!(
            jump.bounds.x >= output_left,
            "jump target is in OUTPUT at step {step}"
        );

        let epoch = scene.geometry_epoch;
        processor.reconcile_portal_document_layouts(&mut scene);
        assert_eq!(
            scene.geometry_epoch, epoch,
            "unchanged measured layout is quiescent"
        );
        let items = compositor.collect_text_items(&scene, 800.0, 520.0);
        let actual = chrome(&compositor, &scene, &items);
        assert_eq!(
            &actual,
            fixed_chrome.get_or_insert_with(|| actual.clone()),
            "pane chrome stays fixed through wheel, append and trim"
        );
        let item = items
            .iter()
            .find(|item| item.text.contains("Line 55"))
            .unwrap();
        let rasterizer = compositor.text_rasterizer.as_ref().unwrap();
        let display = crate::text::effective_truncation_key(item)
            .map(|key| {
                rasterizer
                    .truncation_cache
                    .get_by_key(&key)
                    .expect("settled native document truncation must be primed")
            })
            .map(|result| result.text.as_str())
            .unwrap_or(&item.text);
        assert!(
            display.contains("Line 55"),
            "a retained reading marker must survive append and the tail-to-scrollback transition; step={step}, display={display}"
        );
        assert_eq!(item.clip_bounds_height, viewport.height);
        assert!(
            item.bounds_height > document_height,
            "paint buffer must not be viewport bounded"
        );
    }
    // The native smoother is initialized and rebased above. A later ordinary
    // scroll must still animate rather than snapping with the layout correction.
    let displayed_before_wheel = compositor.display_tile_scroll_offset(&scene, tile_id).1;
    processor.process_keyboard_scroll(tile_id, -40.0, &mut scene);
    compositor.update_scroll_smoothing_at(&scene, smooth_time + delay(4));
    let displayed_after_wheel = compositor.display_tile_scroll_offset(&scene, tile_id).1;
    assert!(displayed_after_wheel < displayed_before_wheel);
    assert!(
        displayed_after_wheel > scene.tile_scroll_offset_local(tile_id).1,
        "ordinary wheel still eases; only measured origin changes snap"
    );
    compositor.scroll_smoothing_enabled = false;
    compositor.publish_displayed_scroll_offsets(&mut scene);
    assert_eq!(
        scene.effective_tile_scroll_offset_local(tile_id),
        scene.tile_scroll_offset_local(tile_id)
    );
    let output_reading = scene.tile_scroll_offset_local(tile_id);
    assert!(processor.reset_portal_input_scroll_to_tail(tile_id, &mut scene));
    assert_eq!(
        scene.tile_scroll_offset_local(tile_id),
        output_reading,
        "typing resets only INPUT history"
    );
    scene
        .overlay
        .portal_output_reads
        .entry(tile_id)
        .or_default()
        .latest = 5;
    assert!(processor.reset_tile_scroll_to_tail(tile_id, &mut scene));
    assert!(scene.tile_follow_tail_at_tail(tile_id));
    assert_eq!(scene.overlay.portal_output_reads[&tile_id].read_through, 5);
    // Identical shaped text may carry a newer output ordinal.
    scene
        .overlay
        .portal_output_reads
        .get_mut(&tile_id)
        .unwrap()
        .latest = 6;
    scene.set_tile_unread_count(tile_id, 1);
    processor.reconcile_portal_document_layouts(&mut scene);
    assert_eq!(scene.overlay.portal_output_reads[&tile_id].read_through, 6);
    processor.process_keyboard_scroll(tile_id, -40.0, &mut scene);
    assert_eq!(
        scene.tile_unread_count(tile_id),
        0,
        "read output stays read on backscroll"
    );
}

fn make_stream_zone_scene(
    overflow: Option<TextOverflow>,
    stream_tail_anchored: Option<bool>,
    content: &str,
) -> SceneGraph {
    let mut scene = SceneGraph::new(720.0, 360.0);
    scene.register_zone(ZoneDefinition {
        rendering_policy: RenderingPolicy {
            font_size_px: Some(14.0),
            font_family: Some(FontFamily::SystemMonospace),
            overflow,
            stream_tail_anchored,
            ..Default::default()
        },
        ..zone_fixture(
            "stream",
            relative(0.0, 0.0, 200.0 / 720.0, 40.0 / 360.0),
            ZoneMediaType::StreamText,
            LayerAttachment::Content,
        )
    });
    publish_zone(
        &mut scene,
        "stream",
        "test",
        ZoneContent::StreamText(content.to_owned()),
    );
    scene
}

#[test]
fn test_zone_stream_text_tail_anchored_opt_in_viewport() {
    use crate::overflow::TruncationViewport::{HeadAnchored, TailAnchored};
    let content = "Line A\nLine B\nLine C\nLine D\nLine E\nLine F\nLine G\nLine H";
    for (overflow, opt_in, viewport, keyed) in [
        (TextOverflow::Ellipsis, Some(true), TailAnchored, true),
        (TextOverflow::Ellipsis, None, HeadAnchored, true),
        (TextOverflow::Ellipsis, Some(false), HeadAnchored, true),
        (TextOverflow::Clip, Some(true), TailAnchored, false),
    ] {
        let policy = RenderingPolicy {
            font_size_px: Some(16.0),
            overflow: Some(overflow),
            stream_tail_anchored: opt_in,
            ..Default::default()
        };
        let item =
            Compositor::zone_stream_text_item(content, 0.0, 0.0, 720.0, 40.0, &policy, 1.0, 1.0);
        let case = format!("{overflow:?} {opt_in:?}");
        assert_eq!(
            item.overflow, overflow,
            "{case}: policy overflow propagates"
        );
        assert_eq!(item.viewport, viewport, "{case}");
        assert_eq!(
            crate::text::effective_truncation_key(&item).is_some(),
            keyed,
            "{case}: only Ellipsis truncates"
        );
    }
}
