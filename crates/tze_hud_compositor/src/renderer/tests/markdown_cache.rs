use super::*;
use crate::markdown::{
    MarkdownCache, MarkdownTokens, PARSE_MARKDOWN_SUBSET_CALLS, parse_markdown_subset,
};
use std::time::{Duration, Instant};

pub(super) fn markdown_node(
    content: impl Into<String>,
    bounds: Rect,
    font_size_px: f32,
    font_family: FontFamily,
    overflow: TextOverflow,
) -> Node {
    Node {
        id: SceneId::new(),
        children: Vec::new(),
        layout: Default::default(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.into(),
            bounds,
            font_size_px,
            font_family,
            color: Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow,
            color_runs: Box::default(),
        }),
    }
}

pub(super) async fn make_text_compositor(
    width: u32,
    height: u32,
) -> Option<(Compositor, HeadlessSurface)> {
    let gpu = make_compositor_and_surface(width, height).await;
    if std::env::var("TZE_HUD_REQUIRE_GPU").as_deref() == Ok("1") {
        assert!(gpu.is_some(), "this parent requires a real GPU constructor");
    }
    gpu.map(|(mut compositor, surface)| {
        compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
        (compositor, surface)
    })
}

pub(super) fn prime_text(compositor: &mut Compositor, scene: &SceneGraph) {
    compositor.prime_markdown_cache(scene);
    compositor.prime_truncation_cache(scene);
}

fn markdown_data(node: &Node) -> &TextMarkdownNode {
    let NodeData::TextMarkdown(node) = &node.data else {
        panic!("fixture must contain a Markdown node")
    };
    node
}

fn parse_calls() -> usize {
    PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get())
}

#[test]
fn markdown_cache_compute_key_is_deterministic_and_content_addressed() {
    let content = "# Hello\n\nThis is **bold** text.";
    let tokens = MarkdownTokens::default();
    let key = MarkdownCache::compute_key(content, &tokens);
    assert_eq!(key, MarkdownCache::compute_key(content, &tokens));
    assert_ne!(
        key,
        MarkdownCache::compute_key("Plain text with `code`.", &tokens)
    );

    let mut cache = MarkdownCache::new();
    let parsed = cache.prime(content, &tokens).clone();
    assert_eq!(cache.get_by_key(&key), Some(&parsed));
    assert_eq!(cache.get(content, &tokens), Some(&parsed));
}

#[test]
fn portal_markdown_node_ids_scopes_by_scroll_config() {
    let mut scene = SceneGraph::new(512.0, 256.0);
    let tab = scene.create_tab("test", 0).unwrap();
    let lease = scene.grant_lease("test", 60_000);
    let [portal, plain] = [(true, "# Portal"), (false, "# Plain")].map(|(scrollable, content)| {
        let x = if scrollable { 0.0 } else { 256.0 };
        let tile = scene
            .create_tile(tab, "test", lease, Rect::new(x, 0.0, 256.0, 256.0), 1)
            .unwrap();
        let node = markdown_node(
            content,
            Rect::new(0.0, 0.0, 200.0, 100.0),
            14.0,
            FontFamily::SystemSansSerif,
            TextOverflow::Clip,
        );
        let id = node.id;
        scene.set_tile_root(tile, node).unwrap();
        if scrollable {
            scene
                .register_tile_scroll_config(tile, TileScrollConfig::vertical())
                .unwrap();
        }
        id
    });
    let ids = super::portal_markdown_node_ids(&scene);
    assert!(ids.contains(&portal));
    assert!(!ids.contains(&plain));
}

fn assert_bold_item(item: &TextItem, text: &str, range: std::ops::Range<usize>) {
    assert_eq!(&*item.text, text);
    assert!(
        item.styled_runs.iter().any(|run| {
            run.start_byte <= range.start
                && run.end_byte >= range.end
                && run.weight.is_some_and(|weight| weight >= 700)
        }),
        "missing bold style on {text:?} at {range:?}"
    );
}

#[tokio::test]
async fn render_frame_headless_is_parse_free_after_commit_time_prime() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(64, 64).await);
    let node = markdown_node(
        "# Commit-time prime test\n\n**bold** and *italic*.",
        Rect::new(0.0, 0.0, 64.0, 64.0),
        12.0,
        FontFamily::SystemSansSerif,
        TextOverflow::Clip,
    );
    let root = node.id;
    let mut scene = scene_with_node(node);

    let assert_primed_frame = |compositor: &mut Compositor, scene: &mut SceneGraph| {
        let version = scene.version;
        assert_eq!(compositor.markdown_cache_scene_version, version);
        let before = parse_calls();
        compositor.render_frame_headless(scene, &surface);
        assert_eq!(parse_calls(), before);
        assert_eq!(compositor.markdown_cache_scene_version, version);
        assert_eq!(compositor.node_key_cache.len(), 1);
        assert!(compositor.node_key_cache.contains_key(&root));
    };
    prime_text(&mut compositor, &scene);
    assert_primed_frame(&mut compositor, &mut scene);
    assert_primed_frame(&mut compositor, &mut scene);

    let tile = *scene.tiles.keys().next().unwrap();
    let mut changed = markdown_data(&scene.nodes[&root]).clone();
    changed.content = "Updated **bold** content".into();
    let version = scene.version;
    scene
        .update_node_content_checked(tile, root, NodeData::TextMarkdown(changed), "test")
        .unwrap();
    assert!(scene.version > version);
    let before = parse_calls();
    prime_text(&mut compositor, &scene);
    assert!(parse_calls() > before, "new small commit enters the parser");

    let before = parse_calls();
    assert_primed_frame(&mut compositor, &mut scene);
    let items = compositor.collect_text_items(&scene, 64.0, 64.0);
    let [item] = items.as_slice() else {
        panic!("expected one updated Markdown item")
    };
    assert_bold_item(item, "Updated bold content", 8..12);
    assert_eq!(parse_calls(), before);
}

#[tokio::test]
async fn markdown_cache_miss_fallback_is_non_lossy() {
    let (compositor, _surface) = require_gpu!(make_text_compositor(256, 256).await);
    let content = "Hello **bold** world";
    let node = markdown_node(
        content,
        Rect::new(0.0, 0.0, 200.0, 50.0),
        12.0,
        FontFamily::SystemSansSerif,
        TextOverflow::Clip,
    );

    let tokens = &compositor.markdown_tokens_generic;
    let key = MarkdownCache::compute_key(content, tokens);
    let parsed = parse_markdown_subset(content, tokens);
    let item = TextItem::from_text_markdown_cached(markdown_data(&node), 0.0, 0.0, &parsed);
    assert_bold_item(&item, "Hello bold world", 6..10);

    // A cold collector consults the immutable snapshot twice. The fallback
    // supplies styled content on both misses and enters the parser only once.
    let scene = scene_with_node(node);
    let misses = compositor.markdown_cache_miss_count();
    for (attempt, parses) in [(1, 1), (2, 0)] {
        assert!(compositor.markdown_cache().get_by_key(&key).is_none());
        let before = parse_calls();
        let items = compositor.collect_text_items(&scene, 256.0, 256.0);
        let [item] = items.as_slice() else {
            panic!("expected one fallback Markdown item")
        };
        assert_bold_item(item, "Hello bold world", 6..10);
        assert_eq!(parse_calls() - before, parses);
        assert_eq!(compositor.markdown_cache_miss_count() - misses, attempt);
    }
    assert!(compositor.markdown_cache().get_by_key(&key).is_none());
}

const TRUNCATED_TEXT: &str = "The quick brown fox jumps over the lazy dog.";

fn truncation_node(content: impl Into<String>, y: f32) -> Node {
    markdown_node(
        content,
        Rect::new(0.0, y, 64.0, 16.0),
        12.0,
        FontFamily::SystemMonospace,
        TextOverflow::Ellipsis,
    )
}

#[tokio::test]
async fn truncation_primes_before_render_and_after_settled_resize() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(64, 64).await);
    let mut scene = scene_with_node(truncation_node(TRUNCATED_TEXT.repeat(4), 0.0));
    prime_text(&mut compositor, &scene);
    let primed = scene.version;
    assert_eq!(compositor.truncation_cache_scene_version, primed);
    compositor.render_frame_headless(&mut scene, &surface);
    assert_eq!(compositor.truncation_cache_scene_version, primed);

    // Move the previous prime inside and outside the cadence without sleep.
    scene.version += 1;
    let now = Instant::now();
    let hour = Duration::from_secs(3600);
    for (last_at, expected) in [
        (Some(now + hour), primed),
        (now.checked_sub(hour), scene.version),
    ] {
        compositor.resize_reprime_last_at = last_at;
        compositor.prime_truncation_cache(&scene);
        assert_eq!(compositor.truncation_cache_scene_version, expected);
    }

    // One rename wake must also prime its new caption when the portal prime defers.
    use crate::display::{DisplayLayout, DisplayRect, FrameTarget};
    use std::num::NonZeroU64;
    use tze_hud_scene::dock::{DockLayout, DockMetrics, DockProjection};
    use tze_hud_scene::scope::scoped_namespace;
    use tze_hud_scene::types::WidgetDefinition;

    compositor.init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let display = DisplayRect {
        name: "MAIN".into(),
        rect: Rect::new(0.0, 0.0, 64.0, 64.0),
        primary: true,
    };
    compositor.set_display_layout(DisplayLayout::new(
        vec![display.clone()],
        &Default::default(),
    ));
    scene.widget_registry.register_definition(WidgetDefinition {
        id: "fixture".into(),
        name: "Fixture".into(),
        description: String::new(),
        parameter_schema: Vec::new(),
        layers: Vec::new(),
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 1.0,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::LatestWins,
        max_publishers: WidgetDefinition::default_max_publishers(),
        ephemeral: false,
        hover_behavior: None,
    });
    scene.set_dock_projection(
        &["fixture".into()],
        Some(DockProjection {
            target: Some(display.name.clone()),
            layout: DockLayout::new(
                display.rect,
                1,
                DockMetrics {
                    row_height: 24.0,
                    caption_width: 20.0,
                    gap: 0.0,
                },
                [0.25],
            ),
            pixels_per_dp: 0.75,
            aspect_ratios: vec![("fixture".into(), 0.25)],
        }),
        NonZeroU64::new(60_000).unwrap(),
    );
    let namespace = scoped_namespace("agent", "reply");
    let lease = scene.grant_lease(&namespace, 60_000);
    scene
        .publish_to_dock("fixture", HashMap::new(), &namespace, Some(lease), 0, 0)
        .unwrap();
    compositor.resize_reprime_last_at = now.checked_sub(hour);
    compositor.prime_truncation_cache(&scene);
    let before_rename = compositor.truncation_cache_scene_version;
    assert_eq!(before_rename, scene.version);

    scene.renew_lease(lease, 60_000).unwrap();
    scene.set_session_label(&namespace, Some("A\u{0301}".repeat(10)));
    compositor.resize_reprime_last_at = Some(now + hour);
    let accepted = compositor
        .dock_draw_display(&scene, &FrameTarget::primary(64, 64))
        .unwrap();
    let captions = compositor.dock_caption_items(&scene, accepted);
    let [caption] = captions.as_slice() else {
        panic!("exactly one dock caption")
    };
    let key = crate::text::effective_truncation_key(caption).unwrap();
    assert!(
        compositor
            .text_rasterizer
            .as_ref()
            .unwrap()
            .truncation_cache
            .get_by_key(&key)
            .is_none()
    );

    compositor.prime_truncation_cache(&scene);
    assert_eq!(compositor.truncation_cache_scene_version, before_rename);
    let cached = compositor
        .text_rasterizer
        .as_ref()
        .unwrap()
        .truncation_cache
        .get_by_key(&key)
        .unwrap();
    assert!(cached.was_truncated && cached.text.ends_with('…'));
}

#[tokio::test]
async fn drag_move_position_only_skips_content_cache_reprimes_bench() {
    let (mut compositor, _surface) = require_gpu!(make_text_compositor(256, 256).await);
    let mut scene = scene_with_node(markdown_node(
        "The quick brown fox jumps over the lazy dog. ".repeat(400),
        Rect::new(0.0, 0.0, 256.0, 240.0),
        14.0,
        FontFamily::SystemMonospace,
        TextOverflow::Ellipsis,
    ));
    let tile = *scene.tiles.keys().next().unwrap();
    prime_text(&mut compositor, &scene);
    assert_eq!(compositor.markdown_cache_scene_version, scene.version);

    // Both arms move real tile bounds and call the production commit-time
    // primer. Only the pre-fix control also invalidates the content version.
    const FRAMES: usize = 60;
    for (advance_content_version, expected_reprimes) in [(true, FRAMES), (false, 0)] {
        let start = Instant::now();
        let mut reprimes = 0;
        for _ in 0..FRAMES {
            let bounds = &mut scene.tiles.get_mut(&tile).unwrap().bounds;
            bounds.x += 3.0;
            bounds.y -= 2.0;
            if advance_content_version {
                scene.version += 1;
            } else {
                scene.bump_geometry_epoch();
            }
            let before = compositor.markdown_cache_scene_version;
            compositor.prime_markdown_cache(&scene);
            reprimes += usize::from(compositor.markdown_cache_scene_version != before);
        }
        eprintln!(
            "{FRAMES} moves, content version bumped={advance_content_version}: {reprimes} Markdown re-primes in {:?}",
            start.elapsed()
        );
        assert_eq!(reprimes, expected_reprimes);
    }
}

#[tokio::test]
async fn load_font_bytes_new_font_resets_truncation_cache_scene_version() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(64, 64).await);
    let mut scene = scene_with_node(truncation_node(TRUNCATED_TEXT, 0.0));
    let tile = *scene.tiles.keys().next().unwrap();
    scene
        .add_node_to_tile(
            tile,
            scene.tiles[&tile].root_node,
            truncation_node(TRUNCATED_TEXT, 20.0),
        )
        .unwrap();

    let render = |compositor: &mut Compositor, scene: &mut SceneGraph, layouts| {
        compositor.render_frame_headless(scene, &surface);
        let work = compositor.take_work_counts().unwrap();
        assert_eq!((work.layout, work.raster, work.upload), (layouts, 0, 0));
        assert!(compositor.take_work_counts().is_none());
    };
    // Both ellipsis nodes share one truncation entry on a cold Markdown snapshot.
    assert!(compositor.markdown_cache().is_empty());
    compositor.prime_truncation_cache(&scene);
    assert_eq!(compositor.truncation_cache_scene_version, scene.version);
    assert_eq!(
        compositor.resize_reprime_content_bytes,
        2 * TRUNCATED_TEXT.len()
    );
    let rasterizer = compositor.text_rasterizer.as_ref().unwrap();
    assert_eq!(rasterizer.truncation_cache.len(), 1);
    assert!(compositor.markdown_cache().is_empty());

    compositor.prime_markdown_cache(&scene);
    render(&mut compositor, &mut scene, 2);
    render(&mut compositor, &mut scene, 0);

    let dummy_id = [0xAB; 32];
    let real_font = include_bytes!("../../../fonts/ibm-plex/IBMPlexMono-Regular.ttf");
    for (id, font) in [(dummy_id, b"OTTO".as_slice()), ([0xCD; 32], real_font)] {
        let loaded = compositor.load_font_bytes(id, font);
        if id != dummy_id {
            assert!(loaded);
        }
        assert_eq!(compositor.truncation_cache_scene_version, u64::MAX);
        compositor.prime_truncation_cache(&scene);
        assert_eq!(compositor.truncation_cache_scene_version, scene.version);
        render(&mut compositor, &mut scene, 2);
        render(&mut compositor, &mut scene, 0);

        if id == dummy_id {
            compositor.load_font_bytes(id, font);
            assert_eq!(compositor.truncation_cache_scene_version, scene.version);
            render(&mut compositor, &mut scene, 0);
        }
    }
}
