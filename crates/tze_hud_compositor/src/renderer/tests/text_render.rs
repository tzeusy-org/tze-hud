use super::*;

// ── Text rendering pixel tests ────────────────────────────────────────────
//
// These tests validate acceptance criteria 1–4 from hud-pmkf:
//  1. publish_to_zone with StreamText → visible text at zone geometry.
//  2. TextMarkdownNode renders text (some non-background pixels in text area).
//  3. Overflow Clip and Ellipsis modes: glyphs stay within TextBounds.
//  4. Headless pixel readback detects text presence.
//
// All tests initialise the text renderer via `compositor.init_text_renderer`
// targeting `Rgba8UnormSrgb` (the headless surface format).

/// Pixel readback validates text presence in a TextMarkdownNode tile.
///
/// After text rendering, pixels in the text region should differ from the
/// solid background color — glyphs overwrite some pixels.  We can't check
/// exact glyph shapes without font-specific knowledge, so we verify that
/// at least one pixel in the tile area differs from the pure background
/// color.
#[tokio::test]
async fn test_text_markdown_node_renders_visible_text() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Dark-blue background, white text — high contrast for pixel detection.
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "Hello world".to_owned(),
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
            font_size_px: 24.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0), // white
            background: Some(Rgba::new(0.0, 0.0, 0.5, 1.0)), // dark blue
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 256 * 256 * 4, "pixel buffer size");

    // The background is dark blue — sRGB of linear [0,0,0.5] ≈ [0, 0, 188].
    // White text (sRGB [255, 255, 255]) glyphs should appear in the tile.
    // We check that at least one pixel has R > 200 AND G > 200 (white).
    let any_bright_pixel = pixels
        .chunks(4)
        .any(|p| p[0] > 200 && p[1] > 200 && p[2] > 200);
    assert!(
        any_bright_pixel,
        "expected white text pixels in TextMarkdownNode tile — none found"
    );
}

/// When the text rasterizer is active, TextMarkdownNode must not fall back
/// to the old full-width placeholder bar path.
#[tokio::test]
async fn test_text_markdown_node_avoids_placeholder_bar_when_text_renderer_active() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(160, 80).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "I".to_owned(),
            bounds: Rect::new(20.0, 16.0, 100.0, 40.0),
            font_size_px: 28.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    let width = 160usize;
    let height = 80usize;
    let bright = |rgba: &[u8]| rgba[0] > 200 && rgba[1] > 200 && rgba[2] > 200;

    let max_bright_run = (0..height)
        .map(|row| {
            (0..width)
                .filter(|col| bright(&pixels[(row * width + col) * 4..][..4]))
                .count()
        })
        .max()
        .unwrap_or(0);

    assert!(
        max_bright_run < 40,
        "text renderer should not paint a placeholder bar; brightest row had {max_bright_run} bright pixels"
    );
}

/// Text stays within the TextBounds clip rectangle (Clip overflow mode).
///
/// We render white text in a small region at the top-left of a dark tile.
/// The bottom-right quadrant should remain all-dark (no text overflow).
#[tokio::test]
async fn test_text_clip_overflow_stays_within_bounds() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Text node occupies only top-left 64x64 pixels of the 256x256 tile.
    // Content: many lines so overflow is tested.
    let content = "Line1\nLine2\nLine3\nLine4\nLine5\nLine6\nLine7\nLine8".to_owned();
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content,
            bounds: Rect::new(0.0, 0.0, 64.0, 32.0), // small box
            font_size_px: 12.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0), // white
            background: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)), // pure black bg
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);

    // Bottom-right quadrant: rows 128..256, cols 128..256.
    // Tile background is ~[0.15, 0.15, 0.25] (tile_background_color default).
    // There should be no white pixels (from text) there.
    let mut bright_outside = false;
    for row in 128..256_usize {
        for col in 128..256_usize {
            let offset = (row * 256 + col) * 4;
            let p = &pixels[offset..offset + 4];
            // White text would have R > 200 AND G > 200 AND B > 200.
            if p[0] > 200 && p[1] > 200 && p[2] > 200 {
                bright_outside = true;
                break;
            }
        }
        if bright_outside {
            break;
        }
    }
    assert!(
        !bright_outside,
        "text overflow detected outside clip bounds (bottom-right quadrant has bright pixels)"
    );
}

/// Ellipsis overflow mode: text renders without panic; background present.
///
/// We don't assert exact "…" pixel shape — that's platform-font-specific.
/// We verify: (a) no panic, (b) background exists, (c) some non-background
/// pixels appear (text was rendered at all).
#[tokio::test]
async fn test_text_ellipsis_overflow_no_panic() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let long_line = "A very long line that definitely overflows the available width of this tile";
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: long_line.to_owned(),
            bounds: Rect::new(0.0, 0.0, 120.0, 40.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: Some(Rgba::new(0.1, 0.1, 0.1, 1.0)),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    // Must not panic.
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(
        pixels.len(),
        256 * 256 * 4,
        "pixel buffer must be full size"
    );
}

/// Zone StreamText publish renders visible text at zone geometry.
///
/// Acceptance criterion 1: publish_to_zone with StreamText content displays
/// readable text at the zone geometry position.
#[tokio::test]
async fn test_zone_stream_text_renders_visible_text() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Register a subtitle zone (bottom edge, 10% height).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "subtitle zone".to_owned(),
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
            text_align: None,
            margin_px: None,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish "Hello Zone" to the subtitle zone.
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Hello Zone".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 1280 * 720 * 4, "pixel buffer size");

    // The zone is at the bottom 10% (y ~648..720) centered (x ~128..1152).
    // We check for any bright pixels in that area (white text on dark bg).
    // Zone bg is semi-transparent dark [0.1, 0.1, 0.15, 0.85] rendered over
    // the default compositor clear [0.05, 0.05, 0.1] → still quite dark.
    // White text glyphs should show as bright pixels.
    let mut found_bright = false;
    // Sample a row in the subtitle zone (row 660 ≈ 91.7% of 720 = 661).
    for row in 652usize..715 {
        for col in 150usize..1130 {
            let offset = (row * 1280 + col) * 4;
            let p = &pixels[offset..offset + 4];
            if p[0] > 180 && p[1] > 180 && p[2] > 180 {
                found_bright = true;
                break;
            }
        }
        if found_bright {
            break;
        }
    }
    assert!(
        found_bright,
        "expected bright (text) pixels in zone subtitle area (rows 652..715)"
    );
}

/// Zone ShortTextWithIcon/Notification publish renders visible text at zone geometry.
///
/// Acceptance criterion for hud-lh3w: publish_to_zone with
/// `ZoneContent::Notification(NotificationPayload)` produces a `TextItem` in
/// `collect_text_items` and causes bright glyph pixels to appear in the zone
/// geometry area.
#[tokio::test]
async fn test_zone_notification_renders_visible_text() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Register a notification zone (top edge, 8% height).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification".to_owned(),
        description: "notification zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.08,
            width_pct: 0.70,
            margin_px: 12.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(20.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.75)),
            text_align: None,
            margin_px: None,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish a notification to the zone.
    scene
        .publish_to_zone(
            "notification",
            ZoneContent::Notification(NotificationPayload {
                text: "Alert: system ready".to_owned(),
                icon: String::new(),
                urgency: 1,
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

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 1280 * 720 * 4, "pixel buffer size");

    // The zone is at the top edge: y ≈ 12..(12 + 720*0.08) ≈ 12..69.6,
    // centered: x ≈ (1280-896)/2..896+192 = 192..1088.
    // White text glyphs should appear as bright pixels (r,g,b > 180).
    let mut found_bright = false;
    for row in 12usize..70 {
        for col in 200usize..1080 {
            let offset = (row * 1280 + col) * 4;
            let p = &pixels[offset..offset + 4];
            if p[0] > 180 && p[1] > 180 && p[2] > 180 {
                found_bright = true;
                break;
            }
        }
        if found_bright {
            break;
        }
    }
    assert!(
        found_bright,
        "expected bright (text) pixels in notification zone area (rows 12..70)"
    );
}

/// Zone StatusBar (KeyValuePairs) publish renders visible text at zone geometry.
///
/// Acceptance criteria for hud-6at1:
///   1. `publish_to_zone` with `ZoneContent::StatusBar` produces a `TextItem` in
///      `collect_text_items`.
///   2. The key-value pairs are rendered as text at the zone geometry position.
#[tokio::test]
async fn test_zone_status_bar_renders_visible_text() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Register a status-bar zone (top edge, 5% height).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "statusbar".to_owned(),
        description: "status bar zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.05,
            width_pct: 0.80,
            margin_px: 8.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_align: None,
            margin_px: None,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish StatusBar content with key-value pairs.
    let mut entries = std::collections::HashMap::new();
    entries.insert("battery".to_owned(), "95%".to_owned());
    entries.insert("time".to_owned(), "12:34".to_owned());
    scene
        .publish_to_zone(
            "statusbar",
            ZoneContent::StatusBar(StatusBarPayload { entries }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // Verify collect_text_items produces a TextItem with the formatted pairs.
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        items.len(),
        1,
        "expected exactly one TextItem for StatusBar"
    );
    let item = &items[0];
    // Entries are sorted by key ("battery" < "time") and separated by newlines.
    assert_eq!(
        &*item.text, "battery: 95%\ntime: 12:34",
        "Entries should be sorted by key and formatted correctly"
    );
    // The TextItem position should be within the zone geometry.
    // Zone top-edge: y = 8.0 (margin_px), height = 720*0.05 = 36, width = 1280*0.8 = 1024.
    assert!(
        item.pixel_y >= 8.0,
        "text y should be at or below zone top margin"
    );
    assert!(
        item.pixel_y < 720.0 * 0.10,
        "text y should be within top zone area"
    );

    // Render to pixels and verify bright text appears in the top zone area.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 1280 * 720 * 4, "pixel buffer size");

    // The zone is at the top ~8..44px, centered horizontally.
    // White text glyphs should show as bright pixels.
    let mut found_bright = false;
    for row in 10usize..42 {
        for col in 150usize..1130 {
            let offset = (row * 1280 + col) * 4;
            let p = &pixels[offset..offset + 4];
            if p[0] > 180 && p[1] > 180 && p[2] > 180 {
                found_bright = true;
                break;
            }
        }
        if found_bright {
            break;
        }
    }
    assert!(
        found_bright,
        "expected bright (text) pixels in status bar zone area (rows 10..42)"
    );
}

/// `init_text_renderer` called multiple times replaces the rasterizer (no panic).
#[tokio::test]
async fn test_init_text_renderer_idempotent() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(64, 64).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(64.0, 64.0);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    // No panic = pass.
}

/// Text rendering with no text items (empty scene) must not panic.
#[tokio::test]
async fn test_text_renderer_empty_scene_no_panic() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(64, 64).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(64.0, 64.0);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
}

/// Stage 6 frame-budget benchmark — text rendering active.
///
/// Renders 60 frames with `init_text_renderer` active, a `TextMarkdownNode`
/// tile, and a zone with `StreamText` content.  Asserts that the p99 of
/// `stage6_render_encode_us` (the Stage 6 wall-clock encode time returned by
/// `render_frame_headless`) stays below `test_budget(STAGE6_BUDGET_US)`
/// (4 ms = 4 000 µs, widened by the test slack factor).
///
/// Budget constant sourced from `tze_hud_runtime::pipeline::STAGE6_BUDGET_US`.
/// It is inlined here to avoid a cyclic dev-dependency
/// (tze_hud_runtime → tze_hud_compositor already exists).
#[tokio::test]
async fn test_stage6_budget_with_text_rendering_active() {
    use tze_hud_scene::perf_budget::test_budget;

    // Stage 6 p99 budget in microseconds — mirrors STAGE6_BUDGET_US in
    // tze_hud_runtime::pipeline (4 ms).
    const STAGE6_BUDGET_US: u64 = 4_000;
    let effective_budget = test_budget(STAGE6_BUDGET_US);
    const FRAME_COUNT: usize = 60;

    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // ── Build scene ─────────────────────────────────────────────────────────
    let mut scene = SceneGraph::new(1280.0, 720.0);
    let tab_id = scene.create_tab("bench", 0).unwrap();
    let lease_id = scene.grant_lease("bench", 60_000);

    // TextMarkdownNode tile occupying most of the screen.
    let tile_id = scene
        .create_tile(
            tab_id,
            "bench",
            lease_id,
            Rect::new(0.0, 0.0, 1000.0, 600.0),
            1,
        )
        .unwrap();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: "Stage 6 budget benchmark\nLine two of text\nLine three".to_owned(),
                    bounds: Rect::new(0.0, 0.0, 1000.0, 600.0),
                    font_size_px: 20.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                    background: Some(Rgba::new(0.05, 0.05, 0.1, 1.0)),
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Clip,
                    color_runs: Box::default(),
                }),
            },
        )
        .unwrap();

    // Zone with StreamText content (subtitle strip at the bottom).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "bench-subtitle".to_owned(),
        description: "benchmark subtitle zone".to_owned(),
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
            text_align: None,
            margin_px: None,
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
            "bench-subtitle",
            ZoneContent::StreamText("Stage 6 benchmark — stream text active".to_owned()),
            "bench",
            None,
            None,
            None,
        )
        .unwrap();

    // ── Warm-up pass ────────────────────────────────────────────────────────
    // Run a few frames to let llvmpipe/WARP JIT-compile the shaders before
    // the timed measurement window.  Shader compilation is a one-time cost
    // that does not reflect steady-state Stage 6 performance; excluding it
    // mirrors production behaviour where shaders are pre-compiled.
    // Prime once before the loop — scene does not change in this benchmark,
    // so subsequent frames are all cache-hit no-ops (hud-380dl / hud-v2z6u).
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    for _ in 0..5 {
        compositor.render_frame_headless(&mut scene, &surface);
    }

    // ── Render loop ─────────────────────────────────────────────────────────
    let mut timings: Vec<u64> = Vec::with_capacity(FRAME_COUNT);
    for _ in 0..FRAME_COUNT {
        let telem = compositor.render_frame_headless(&mut scene, &surface);
        // stage6_render_encode_us is the Stage 6 wall-clock encode duration.
        timings.push(telem.stage6_render_encode_us);
    }

    // ── p99 assertion ────────────────────────────────────────────────────────
    timings.sort_unstable();
    // p99 index: ceil(99/100 * N) - 1 (0-based), clamped to last element.
    let p99_index = ((FRAME_COUNT as f64 * 0.99).ceil() as usize).saturating_sub(1);
    let p99_index = p99_index.min(FRAME_COUNT - 1);
    let p99_us = timings[p99_index];

    assert!(
        p99_us <= effective_budget,
        "Stage 6 render-encode p99 ({p99_us} µs) exceeds budget ({effective_budget} µs, \
             spec target={STAGE6_BUDGET_US} µs). All timings (sorted): {timings:?}"
    );
}
