use super::*;

// ─── ensure_icon_texture: token substitution unit tests ──────────────────

/// SVG icon with a `{{token.color.primary}}` placeholder loads successfully
/// when the token is present in the compositor's token map.
///
/// Acceptance criterion: compositor-level token substitution is applied
/// before SVG parsing so that token-driven icons render correctly.
#[tokio::test]
async fn test_ensure_icon_texture_resolves_token_placeholder() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(64, 64).await);

    // Write a minimal SVG with a token placeholder to a temp file.
    let svg_path = std::env::temp_dir().join("tze_hud_test_icon_token_ok.svg");
    std::fs::write(
        &svg_path,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32">
<rect width="32" height="32" fill="{{token.color.primary}}"/>
</svg>"#,
    )
    .expect("write test SVG");
    let path = svg_path.to_string_lossy().into_owned();

    // Token map with the required key — icon must load.
    compositor.set_token_map(
        [("color.primary".to_string(), "#ff0000".to_string())]
            .into_iter()
            .collect(),
    );
    let loaded = compositor.ensure_icon_texture(&path);
    assert!(
        loaded,
        "ensure_icon_texture must succeed when token is resolved"
    );

    // Texture should be in the cache.
    let resource_id = tze_hud_scene::ResourceId::of(path.as_bytes());
    assert!(
        compositor.image_texture_cache.contains_key(&resource_id),
        "resolved icon must be in image_texture_cache"
    );

    let _ = std::fs::remove_file(&svg_path);
}

/// SVG icon with an unresolved token placeholder causes `ensure_icon_texture`
/// to return `false` and negative-cache the failure.  After `set_token_map`
/// is called with the missing token, the negative cache is cleared and the
/// icon can be loaded on the next call.
#[tokio::test]
async fn test_ensure_icon_texture_unresolved_token_cleared_after_set_token_map() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(64, 64).await);

    let svg_path = std::env::temp_dir().join("tze_hud_test_icon_token_missing.svg");
    std::fs::write(
        &svg_path,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32">
<rect width="32" height="32" fill="{{token.color.primary}}"/>
</svg>"#,
    )
    .expect("write test SVG");
    let path = svg_path.to_string_lossy().into_owned();

    // Empty token map — token is missing, load must fail.
    compositor.set_token_map(std::collections::HashMap::new());
    let first = compositor.ensure_icon_texture(&path);
    assert!(
        !first,
        "ensure_icon_texture must return false on unresolved token"
    );

    let resource_id = tze_hud_scene::ResourceId::of(path.as_bytes());
    assert!(
        compositor.failed_icon_paths.contains(&resource_id),
        "failed path must be in negative cache after unresolved token"
    );

    // Now provide the missing token.  set_token_map must clear the negative
    // cache so the icon can be retried.
    compositor.set_token_map(
        [("color.primary".to_string(), "#00ff00".to_string())]
            .into_iter()
            .collect(),
    );
    assert!(
        !compositor.failed_icon_paths.contains(&resource_id),
        "negative cache must be cleared after set_token_map"
    );
    let second = compositor.ensure_icon_texture(&path);
    assert!(
        second,
        "ensure_icon_texture must succeed after token map is updated"
    );

    let _ = std::fs::remove_file(&svg_path);
}

// ─── Scroll-offset text rendering (hud-w5ih) ────────────────────────────

/// `collect_text_items` applies the tile scroll offset to `TextItem` pixel
/// positions so text glyphs track the scrolled content.
///
/// Without the fix, `collect_text_items_from_node` passed bare
/// `tile.bounds.x`/`tile.bounds.y` to `TextItem::from_text_markdown_node`,
/// leaving text anchored at its original position while the geometry quads
/// moved with the scroll. This test pins the contract: a text node at
/// `(node_x, node_y)` in a tile scrolled by `(scroll_x, scroll_y)` must
/// produce a `TextItem` whose `pixel_x`/`pixel_y` subtract the scroll offset.
///
/// Spec refs: hud-w5ih Bounded Transcript Viewport requirement; RFC 0013
/// Transcript Interaction Contract (local-first scroll).
#[tokio::test]
async fn test_collect_text_items_applies_tile_scroll_offset() {
    // This test needs no GPU — `collect_text_items` is a pure CPU path.
    // We construct a Compositor without a surface render call.
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);

    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("scroll-test", 120_000);

    // Place a tile at (100, 50).
    let tile_x = 100.0_f32;
    let tile_y = 50.0_f32;
    let tile_w = 400.0_f32;
    let tile_h = 200.0_f32;
    let tile_id = scene
        .create_tile(
            tab_id,
            "scroll-test",
            lease_id,
            Rect::new(tile_x, tile_y, tile_w, tile_h),
            1,
        )
        .unwrap();

    // Register a scroll config so scroll offsets are valid on this tile.
    scene
        .register_tile_scroll_config(
            tile_id,
            tze_hud_scene::types::TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(600.0),
            },
        )
        .unwrap();

    // Set a 80px vertical scroll offset (local-first, no agent roundtrip).
    let scroll_y = 80.0_f32;
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, scroll_y)
        .unwrap();

    // Place a TextMarkdown node at tile-local (10, 20).
    let node_x = 10.0_f32;
    let node_y = 20.0_f32;
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "scroll test line".to_string(),
            bounds: Rect::new(node_x, node_y, 300.0, 24.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();

    // Collect text items with scroll_y=80 applied — the path under test.
    let items_scrolled = compositor.collect_text_items(&scene, 720.0, 360.0);
    assert_eq!(
        items_scrolled.len(),
        1,
        "expected one TextItem for the scroll tile"
    );
    let scrolled_item = &items_scrolled[0];
    assert_eq!(
        &*scrolled_item.text, "scroll test line",
        "TextItem content must match the node"
    );

    // Now create an identical scene WITHOUT scroll offset so we can
    // compare pixel_y values directly. Both tiles have the same bounds and
    // the same node bounds, so margin_y is identical in both — subtracting
    // yields exactly scroll_y.
    //
    //   pixel_y_scrolled = (tile_y - scroll_y) + node_y + margin_y
    //   pixel_y_baseline =  tile_y             + node_y + margin_y
    //   diff             =  scroll_y  (margin cancels)
    let mut scene_baseline = SceneGraph::new(720.0, 360.0);
    let tab2 = scene_baseline.create_tab("test", 0).unwrap();
    let lease2 = scene_baseline.grant_lease("baseline", 120_000);
    let tile_id2 = scene_baseline
        .create_tile(
            tab2,
            "baseline",
            lease2,
            Rect::new(tile_x, tile_y, tile_w, tile_h),
            1,
        )
        .unwrap();
    // No scroll offset registered — offset defaults to (0, 0).
    let baseline_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "scroll test line".to_string(),
            bounds: Rect::new(node_x, node_y, 300.0, 24.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    scene_baseline
        .set_tile_root(tile_id2, baseline_node)
        .unwrap();
    let items_baseline = compositor.collect_text_items(&scene_baseline, 720.0, 360.0);
    assert_eq!(
        items_baseline.len(),
        1,
        "baseline must also produce one TextItem"
    );
    let baseline_item = &items_baseline[0];

    // Verify that the scrolled item is positioned exactly scroll_y pixels
    // above the baseline item (margin cancels between identical bounds).
    // Use 0.01 tolerance — f32 precision at values ~80.0 is ~9.5e-6, larger
    // than f32::EPSILON (1.19e-7), and intermediate margin arithmetic may
    // accumulate sub-ULP error. 0.01px is tight enough to catch wrong offset
    // but not fragile against f32 rounding. Matches existing renderer test
    // convention (see position comparisons at < 0.5 and color deltas at <
    // 0.01 throughout this module).
    let actual_shift = baseline_item.pixel_y - scrolled_item.pixel_y;
    assert!(
        (actual_shift - scroll_y).abs() < 0.01,
        "scroll shift ({actual_shift}) must equal scroll_y ({scroll_y}); \
             baseline_y={}, scrolled_y={}",
        baseline_item.pixel_y,
        scrolled_item.pixel_y
    );

    // Directional sanity: scrolled item must be above baseline.
    assert!(
        scrolled_item.pixel_y < baseline_item.pixel_y,
        "scrolled text ({}) must be above unscrolled text ({})",
        scrolled_item.pixel_y,
        baseline_item.pixel_y
    );
    assert!(
        (scrolled_item.clip_pixel_y - baseline_item.clip_pixel_y).abs() < 0.01,
        "scroll must not move the text clip rectangle with the glyph origin"
    );
    assert!(
        (scrolled_item.clip_bounds_height - baseline_item.clip_bounds_height).abs() < 0.01,
        "clip height must remain tied to the viewport, not the scrolled glyph origin"
    );
}

/// hud-g1ena.3: the jump-to-latest pill MAY carry the ambient unread count.
/// `collect_jump_to_latest_badge_item` yields a centered, clipped count
/// `TextItem` only when the pill would show (content overflows + scrolled away)
/// AND the tile carries a nonzero, non-redacted unread count; it returns `None`
/// at the tail or with nothing unread, so the badge appears and clears with the
/// pill (local-first, no adapter round trip).
#[tokio::test]
async fn jump_to_latest_badge_gates_on_scroll_and_unread_count() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(480, 320).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(480.0, 320.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("badge-test", 120_000);

    let tile_id = scene
        .create_tile(
            tab_id,
            "badge-test",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 200.0),
            1,
        )
        .unwrap();

    // Content overflows the 200px viewport → the pill (and badge) may show.
    scene
        .register_tile_scroll_config(
            tile_id,
            tze_hud_scene::types::TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(800.0),
            },
        )
        .unwrap();

    let jtl_tokens =
        super::token_colors::resolve_jump_to_latest_tokens(&std::collections::HashMap::new());
    let si_tokens =
        super::token_colors::resolve_scroll_indicator_tokens(&std::collections::HashMap::new());

    let badge = |c: &Compositor, s: &SceneGraph| {
        let tile = s.tiles.get(&tile_id).unwrap();
        c.collect_jump_to_latest_badge_item(tile, s, &jtl_tokens, &si_tokens)
    };

    // Scrolled away from the tail, with unread content → badge renders.
    scene.set_tile_follow_tail_at_tail(tile_id, false);
    scene.set_tile_unread_count(tile_id, 3);
    let item =
        badge(&compositor, &scene).expect("scrolled-away tile with unread must show a badge");
    assert_eq!(&*item.text, "3 unread", "badge must carry the unread count");
    assert_eq!(
        item.alignment,
        tze_hud_scene::types::TextAlign::Center,
        "count must be centered in the pill"
    );
    // Clip is confined to the pill (bottom-center of the tile), never the whole tile.
    assert!(
        item.clip_bounds_width <= 400.0 && item.clip_bounds_height <= 200.0,
        "badge clip must stay within the pill"
    );
    assert!(
        item.pixel_y >= 100.0,
        "pill (and badge) sits in the lower half of the tile, got y={}",
        item.pixel_y
    );

    // Nothing unread → plain pill, no badge (a presence engine renders nothing).
    scene.set_tile_unread_count(tile_id, 0);
    assert!(
        badge(&compositor, &scene).is_none(),
        "no badge when there is nothing unread"
    );

    // Back at the tail → the pill (and therefore the badge) is hidden, even with
    // a stale nonzero count still recorded.
    scene.set_tile_unread_count(tile_id, 5);
    scene.set_tile_follow_tail_at_tail(tile_id, true);
    assert!(
        badge(&compositor, &scene).is_none(),
        "no badge at the tail — it clears with the pill when the viewer returns"
    );
}

// ─── Smooth scroll / animated follow-tail (hud-bq0gl.10) ─────────────────

/// `display_tile_scroll_offset` snaps to the raw scene offset in headless mode
/// so deterministic golden tests are unaffected, and a freshly-observed tile in
/// windowed (smoothing-enabled) mode starts *settled* on its current offset
/// (no initial jump) once `update_scroll_smoothing` has run.
///
/// This pins the wiring contract for the smooth-scroll path: the scene's offset
/// remains the authoritative target (RFC 0013 §3.2 — user scroll authoritative),
/// and the smoother never introduces a jump on first sight. Easing dynamics are
/// covered exhaustively by the pure `easing::ScrollSmoother` unit tests.
#[tokio::test]
async fn display_tile_scroll_offset_snaps_headless_and_settles_windowed() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);

    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("smooth-scroll", 120_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "smooth-scroll",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 200.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            tile_id,
            tze_hud_scene::types::TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(800.0),
            },
        )
        .unwrap();
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 120.0)
        .unwrap();

    // Headless default: smoothing disabled → exact raw offset (snap).
    assert!(!compositor.scroll_smoothing_enabled);
    let (hx, hy) = compositor.display_tile_scroll_offset(&scene, tile_id);
    assert_eq!(
        (hx, hy),
        (0.0, 120.0),
        "headless must return the raw offset"
    );

    // Enable smoothing and advance once: the tile is observed for the first
    // time, so its smoother starts settled on the current target — no jump.
    compositor.scroll_smoothing_enabled = true;
    compositor.update_scroll_smoothing(&scene);
    let (wx, wy) = compositor.display_tile_scroll_offset(&scene, tile_id);
    assert!(
        (wx - 0.0).abs() < 1e-4 && (wy - 120.0).abs() < 1e-4,
        "freshly-observed tile must start settled on its offset (no jump); got ({wx}, {wy})"
    );

    // A non-scrollable / unknown tile has no smoother → falls back to raw.
    let unknown = SceneId::new();
    assert_eq!(
        compositor.display_tile_scroll_offset(&scene, unknown),
        (0.0, 0.0),
        "tiles without a smoother fall back to the raw scene offset"
    );
}

/// `publish_displayed_scroll_offsets` records exactly the offset the renderer
/// draws with (`display_tile_scroll_offset`) into the scene so the live
/// hit-test path agrees with the rendered rows during a smoothed scroll
/// (hud-3lynp). When smoothing is disabled (headless/snap) it clears any
/// published overrides so hit-testing falls back to the authoritative offset.
#[tokio::test]
async fn publish_displayed_scroll_offsets_mirrors_smoother_and_clears_headless() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);

    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("smooth-scroll", 120_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "smooth-scroll",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 200.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            tile_id,
            tze_hud_scene::types::TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(800.0),
            },
        )
        .unwrap();
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 120.0)
        .unwrap();

    // Windowed: advance the smoother, then publish. The published displayed
    // offset must equal display_tile_scroll_offset (the value the renderer drew)
    // and become the effective offset the hit-test path consults.
    compositor.scroll_smoothing_enabled = true;
    compositor.update_scroll_smoothing(&scene);
    let drawn = compositor.display_tile_scroll_offset(&scene, tile_id);
    compositor.publish_displayed_scroll_offsets(&mut scene);
    assert_eq!(
        scene.effective_tile_scroll_offset_local(tile_id),
        drawn,
        "hit-test path must see the same displayed offset the renderer drew with"
    );

    // Headless/snap: publishing clears the override so hit-testing falls back to
    // the authoritative offset (deterministic golden tests unaffected).
    compositor.scroll_smoothing_enabled = false;
    compositor.publish_displayed_scroll_offsets(&mut scene);
    assert_eq!(
        scene.effective_tile_scroll_offset_local(tile_id),
        (0.0, 120.0),
        "with smoothing disabled the effective offset falls back to authoritative"
    );
}

/// Headless readback regression for the text-stream portal output pane.
///
/// The live exemplar mounts the OUTPUT transcript body as a scrollable tile
/// inside a larger portal frame. Scrolled node geometry must be clipped to that
/// output tile viewport: root/background fills must not bleed above it, and
/// scrolled child fills must not bleed below it.
#[tokio::test]
async fn scrolled_portal_output_tile_clips_geometry_outside_viewport() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(220, 180).await);

    let mut scene = SceneGraph::new(220.0, 180.0);
    let tab_id = scene.create_tab("portal-output-clip", 0).unwrap();
    let lease_id = scene.grant_lease("portal-output-clip", 120_000);

    let frame_id = scene
        .create_tile(
            tab_id,
            "portal-output-clip",
            lease_id,
            Rect::new(40.0, 20.0, 160.0, 140.0),
            1,
        )
        .unwrap();
    scene
        .set_tile_root(
            frame_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.28, 0.34, 0.50, 1.0),
                    radius: None,
                    bounds: Rect::new(0.0, 0.0, 160.0, 140.0),
                }),
            },
        )
        .unwrap();

    let output_id = scene
        .create_tile(
            tab_id,
            "portal-output-clip",
            lease_id,
            Rect::new(90.0, 60.0, 80.0, 60.0),
            2,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            output_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(240.0),
            },
        )
        .unwrap();

    let root_id = SceneId::new();
    scene
        .set_tile_root(
            output_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                    radius: None,
                    bounds: Rect::new(0.0, 0.0, 80.0, 60.0),
                }),
            },
        )
        .unwrap();
    scene
        .add_node_to_tile(
            output_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                    radius: None,
                    bounds: Rect::new(0.0, 92.0, 80.0, 80.0),
                }),
            },
        )
        .unwrap();

    // Settle the §6.3 portal fade-in before probing. A freshly-created scrollable
    // tile begins a fade-in animation, and since hud-b0x0m every node fill (incl.
    // this SolidColor pane) honours the tile fade — so at t=0 the pane fill renders
    // translucent and composites toward the frame behind it. This test is about
    // geometry clipping, not the transition, so warm one frame to register the
    // appear, then clear the animation state so the probes below observe the
    // steady-state (fully opaque) fills.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.portal_tile_anim_states.clear();

    for scroll_y in [0.0_f32, 48.0, 96.0] {
        scene
            .set_tile_scroll_offset_local(output_id, 0.0, scroll_y)
            .unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        compositor.render_frame_headless(&mut scene, &surface);

        let pixels = surface.read_pixels(&compositor.device);
        let frame_control = crate::test_pixels::pixel_at(&pixels, 220, 55, 45);
        let above_output = crate::test_pixels::pixel_at(&pixels, 220, 110, 45);
        let below_output = crate::test_pixels::pixel_at(&pixels, 220, 110, 135);
        let inside_output = crate::test_pixels::pixel_at(&pixels, 220, 110, 70);

        assert_eq!(
            above_output, frame_control,
            "scroll_y={scroll_y}: output root fill leaked above the output viewport; \
             above={above_output:?}, frame={frame_control:?}"
        );
        assert_eq!(
            below_output, frame_control,
            "scroll_y={scroll_y}: scrolled output content leaked below the output viewport; \
             below={below_output:?}, frame={frame_control:?}"
        );
        assert_ne!(
            inside_output, frame_control,
            "scroll_y={scroll_y}: output viewport should still render its black pane fill"
        );
    }
}

#[tokio::test]
async fn scrolled_rounded_solid_preserves_original_shape_with_viewport_clip() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(220, 180).await);

    let mut scene = SceneGraph::new(220.0, 180.0);
    let tab_id = scene.create_tab("portal-rounded-clip", 0).unwrap();
    let lease_id = scene.grant_lease("portal-rounded-clip", 120_000);
    let output_id = scene
        .create_tile(
            tab_id,
            "portal-rounded-clip",
            lease_id,
            Rect::new(90.0, 60.0, 80.0, 60.0),
            2,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            output_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(240.0),
            },
        )
        .unwrap();

    let root_id = SceneId::new();
    scene
        .set_tile_root(
            output_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                    radius: None,
                    bounds: Rect::new(0.0, 0.0, 80.0, 60.0),
                }),
            },
        )
        .unwrap();
    scene
        .add_node_to_tile(
            output_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                    radius: Some(16.0),
                    bounds: Rect::new(0.0, 92.0, 80.0, 80.0),
                }),
            },
        )
        .unwrap();

    scene
        .set_tile_scroll_offset_local(output_id, 0.0, 96.0)
        .unwrap();

    let cmds = compositor.collect_tile_rounded_rect_cmds(&scene);
    assert_eq!(cmds.len(), 1, "expected exactly one rounded child command");
    let cmd = &cmds[0];
    assert_eq!(cmd.x, 90.0);
    assert_eq!(cmd.y, 56.0);
    assert_eq!(cmd.width, 80.0);
    assert_eq!(cmd.height, 80.0);
    assert_eq!(cmd.radius, 16.0);

    let clip = cmd
        .clip
        .expect("scrolled rounded child must carry a viewport clip");
    assert_eq!(clip.x, 90.0);
    assert_eq!(clip.y, 60.0);
    assert_eq!(clip.width, 80.0);
    assert_eq!(clip.height, 60.0);
}

/// A first-class expanded portal is one movable/resizable surface: wheel scroll
/// translates its document rows, never its frame, pane fills, header, or INPUT /
/// OUTPUT labels. This reproduces the live post-resize failure where the
/// `PortalSurface` carries geometry-only parts because its regenerated inline
/// subtree has no stable node IDs (hud-yrcev).
///
/// The draw-list assertions exercise the rounded frame/pane path; the text-item
/// assertions exercise the glyph path. Keeping both in one post-resize scene
/// prevents the two passes from silently applying different scroll policies.
#[tokio::test]
async fn expanded_portal_chrome_stays_fixed_while_document_content_scrolls_after_resize() {
    use tze_hud_scene::types::{
        FontFamily, PortalDisplayState, PortalPart, PortalPartKind, PortalSurface, SolidColorNode,
        TextAlign, TextMarkdownNode,
    };

    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(800, 520).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(800.0, 520.0);
    let tab = scene.create_tab("expanded-portal", 0).unwrap();
    let lease = scene.grant_lease("expanded-portal", 120_000);
    // Deliberately not the attach-time size: this is the operator's resized
    // portal. Every declared part is already re-resolved to this whole-unit
    // geometry before the wheel scroll happens.
    let tile_bounds = Rect::new(80.0, 40.0, 600.0, 360.0);
    let tile_id = scene
        .create_tile(tab, "expanded-portal", lease, tile_bounds, 1)
        .unwrap();
    scene
        .register_tile_scroll_config(
            tile_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(1_200.0),
            },
        )
        .unwrap();

    let header_h = 48.0;
    let divider_w = 12.0;
    let pane_w = (tile_bounds.width - divider_w) * 0.5;
    let input_pane = Rect::new(0.0, header_h, pane_w, tile_bounds.height - header_h);
    let output_pane = Rect::new(
        pane_w + divider_w,
        header_h,
        pane_w,
        tile_bounds.height - header_h,
    );
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.12, 0.12, 0.14, 1.0),
                    bounds: Rect::new(0.0, 0.0, tile_bounds.width, tile_bounds.height),
                    radius: Some(14.0),
                }),
            },
        )
        .unwrap();

    let add_child = |scene: &mut SceneGraph, data: NodeData| {
        scene
            .add_node_to_tile(
                tile_id,
                Some(root_id),
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data,
                },
            )
            .unwrap();
    };
    add_child(
        &mut scene,
        NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.16, 0.16, 0.18, 1.0),
            bounds: input_pane,
            radius: Some(12.0),
        }),
    );
    add_child(
        &mut scene,
        NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.03, 0.03, 0.04, 1.0),
            bounds: output_pane,
            radius: Some(12.0),
        }),
    );

    let markdown = |content: &str, bounds: Rect| {
        NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_owned(),
            bounds,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(0.9, 0.9, 0.9, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        })
    };
    add_child(
        &mut scene,
        markdown("Portal header", Rect::new(12.0, 0.0, 576.0, header_h)),
    );
    add_child(
        &mut scene,
        markdown(
            "INPUT",
            Rect::new(12.0, input_pane.y, input_pane.width - 24.0, 24.0),
        ),
    );
    add_child(
        &mut scene,
        markdown(
            "OUTPUT",
            Rect::new(
                output_pane.x + 12.0,
                output_pane.y,
                output_pane.width - 24.0,
                24.0,
            ),
        ),
    );
    add_child(
        &mut scene,
        markdown(
            "output document",
            Rect::new(
                output_pane.x + 12.0,
                output_pane.y + 28.0,
                output_pane.width - 24.0,
                output_pane.height - 40.0,
            ),
        ),
    );

    // This matches ResidentGrpcPortalAdapter::portal_surface_proto: it gives
    // first-class part geometry but no stable node IDs, because each publish
    // regenerates the inline subtree.
    scene.overlay.portal_surfaces.insert(
        tile_id,
        PortalSurface {
            display_state: PortalDisplayState::Expanded,
            parts: vec![
                PortalPart {
                    kind: PortalPartKind::Frame,
                    bounds: Rect::new(0.0, 0.0, tile_bounds.width, tile_bounds.height),
                    node: None,
                },
                PortalPart {
                    kind: PortalPartKind::Header,
                    bounds: Rect::new(0.0, 0.0, tile_bounds.width, header_h),
                    node: None,
                },
                PortalPart {
                    kind: PortalPartKind::Composer,
                    bounds: input_pane,
                    node: None,
                },
                PortalPart {
                    kind: PortalPartKind::Transcript,
                    bounds: output_pane,
                    node: None,
                },
                PortalPart {
                    kind: PortalPartKind::Divider,
                    bounds: Rect::new(pane_w, header_h, divider_w, tile_bounds.height - header_h),
                    node: None,
                },
            ],
            ..Default::default()
        },
    );
    compositor.prime_markdown_cache(&scene);

    let item_y = |items: &[crate::text::TextItem], content: &str| {
        items
            .iter()
            .find(|item| item.text.as_ref() == content)
            .unwrap_or_else(|| panic!("expected {content:?} TextItem"))
            .pixel_y
    };
    let before = compositor.collect_text_items(&scene, 800.0, 520.0);
    let header_before = item_y(&before, "Portal header");
    let input_label_before = item_y(&before, "INPUT");
    let output_label_before = item_y(&before, "OUTPUT");
    let document_before = item_y(&before, "output document");

    let scroll_y = 72.0;
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, scroll_y)
        .unwrap();
    let cmds = compositor.collect_tile_rounded_rect_cmds(&scene);
    let frame = cmds
        .iter()
        .find(|cmd| cmd.width == tile_bounds.width && cmd.height == tile_bounds.height)
        .expect("rounded frame draw command");
    assert_eq!(frame.x, tile_bounds.x, "frame x must remain tile-anchored");
    assert_eq!(
        frame.y, tile_bounds.y,
        "resized rounded frame must not translate with document scroll"
    );
    let panes: Vec<_> = cmds
        .iter()
        .filter(|cmd| cmd.width == pane_w && cmd.height == input_pane.height)
        .collect();
    assert_eq!(
        panes.len(),
        2,
        "both resized rounded pane backdrops must reach the draw list"
    );
    for pane in panes {
        assert_eq!(
            pane.y,
            tile_bounds.y + header_h,
            "pane backdrop must remain whole-portal chrome under scroll"
        );
    }

    let after = compositor.collect_text_items(&scene, 800.0, 520.0);
    assert_eq!(
        item_y(&after, "Portal header"),
        header_before,
        "header text must remain fixed with the frame"
    );
    assert_eq!(
        item_y(&after, "INPUT"),
        input_label_before,
        "INPUT label must remain chrome"
    );
    assert_eq!(
        item_y(&after, "OUTPUT"),
        output_label_before,
        "OUTPUT label must remain chrome"
    );
    assert!(
        (document_before - item_y(&after, "output document") - scroll_y).abs() < 0.5,
        "document content must translate by the display scroll offset"
    );
}

/// `collect_text_items` does NOT shift text for tiles with zero scroll offset.
///
/// Regression guard: ensuring the fix is additive (non-scrolled tiles
/// are unaffected).
#[tokio::test]
async fn test_collect_text_items_zero_scroll_unchanged() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);

    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("no-scroll-test", 120_000);

    let tile_x = 50.0_f32;
    let tile_y = 30.0_f32;
    let tile_id = scene
        .create_tile(
            tab_id,
            "no-scroll-test",
            lease_id,
            Rect::new(tile_x, tile_y, 200.0, 100.0),
            1,
        )
        .unwrap();

    let node_x = 5.0_f32;
    let node_y = 10.0_f32;
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "no-scroll baseline".to_string(),
            bounds: Rect::new(node_x, node_y, 180.0, 20.0),
            font_size_px: 12.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();
    // No scroll offset registered or set — tile_scroll_offset_local returns (0, 0).

    let items = compositor.collect_text_items(&scene, 720.0, 360.0);
    assert_eq!(items.len(), 1, "one TextItem for the non-scrolled tile");

    let item = &items[0];
    // pixel_y must be >= tile_y + node_y (the raw sum before margin).
    // The margin is positive, so pixel_y >= tile_y + node_y always holds.
    assert!(
        item.pixel_y >= tile_y + node_y,
        "non-scrolled text pixel_y ({}) must be >= tile_y ({tile_y}) + node_y ({node_y})",
        item.pixel_y
    );
}

/// At-tail Ellipsis tiles: `collect_text_items` must produce `TailAnchored`
/// viewport, and the truncation cache must hit (not miss) after priming.
///
/// **Bug context (hud-lu50e):** `prime_truncation_cache` primed
/// `TailAnchored` entries for at-tail tiles, but `collect_text_items_from_node`
/// always built items with the constructor-default `HeadAnchored` viewport.
/// `prepare_text_items` keyed the cache lookup on `item.viewport`, so the
/// per-frame key was `HeadAnchored` while the primed entry was `TailAnchored` —
/// causing a cache miss on every frame and the inline fallback always
/// running head-anchored truncation (showing oldest lines, not newest).
///
/// This test asserts:
/// 1. For a tile with `at_tail = true` and `TextOverflow::Ellipsis`, the
///    resulting `TextItem` has `viewport == TailAnchored`.
/// 2. For the same tile with `at_tail = false` (scrolled-back), the
///    `TextItem` retains `HeadAnchored`.
/// 3. Non-Ellipsis (Clip) nodes are unaffected by `at_tail`.
/// 4. After priming the truncation cache (`prime_truncation_cache`) with
///    the at-tail scene, the TailAnchored key is present in the cache,
///    confirming the per-frame item and the primed entry are aligned.
#[tokio::test]
async fn test_collect_text_items_at_tail_ellipsis_uses_tail_anchored_viewport() {
    // collect_text_items is a CPU path; init_text_renderer + prime_truncation_cache
    // require the GPU text rasterizer.
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Content with multiple distinct lines so head vs. tail truncation
    // would show different text.
    let content = "Line A\nLine B\nLine C\nLine D\nLine E\nLine F\nLine G\nLine H";

    let tile_w = 200.0_f32;
    // Narrow height so only a subset of lines fits — forces truncation.
    let tile_h = 40.0_f32;

    // ── Scene with at_tail = true ─────────────────────────────────────────
    let mut scene_at_tail = SceneGraph::new(720.0, 360.0);
    let tab_at = scene_at_tail.create_tab("test", 0).unwrap();
    let lease_at = scene_at_tail.grant_lease("at-tail-test", 120_000);
    let tile_at = scene_at_tail
        .create_tile(
            tab_at,
            "at-tail-test",
            lease_at,
            Rect::new(0.0, 0.0, tile_w, tile_h),
            1,
        )
        .unwrap();
    let node_ellipsis = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_owned(),
            bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    scene_at_tail.set_tile_root(tile_at, node_ellipsis).unwrap();
    // Mark tile as at-tail (follow-tail active, user has not scrolled back).
    scene_at_tail.set_tile_follow_tail_at_tail(tile_at, true);

    compositor.prime_markdown_cache(&scene_at_tail);

    // 1. per-frame viewport must be TailAnchored.
    let items_at_tail = compositor.collect_text_items(&scene_at_tail, 720.0, 360.0);
    assert_eq!(
        items_at_tail.len(),
        1,
        "expected one TextItem for the at-tail tile"
    );
    let at_tail_item = &items_at_tail[0];
    assert_eq!(
        at_tail_item.overflow,
        TextOverflow::Ellipsis,
        "item must carry Ellipsis overflow"
    );
    assert_eq!(
        at_tail_item.viewport,
        crate::overflow::TruncationViewport::TailAnchored,
        "at-tail Ellipsis tile must produce TailAnchored viewport \
             so the per-frame key matches the primed cache entry (hud-lu50e)"
    );

    // 2. Prime the truncation cache and verify TailAnchored key is present.
    //    `prime_truncation_cache` primes TailAnchored for at-tail tiles.
    //    If the per-frame item.viewport also matches TailAnchored, the
    //    cache lookup in `prepare_text_items` will hit — confirming alignment.
    compositor.prime_truncation_cache(&scene_at_tail);
    // Access the rasterizer's truncation cache to confirm the TailAnchored
    // entry was primed.  `prime_truncation_cache` calls
    // `rasterizer.prime_truncation_cache(items)` which stores entries keyed
    // on viewport_mode=1 (TailAnchored).  The cache must be non-empty after
    // priming an Ellipsis tile.
    let cache_len_after_prime = compositor
        .text_rasterizer
        .as_ref()
        .expect("text rasterizer must be initialised")
        .truncation_cache
        .len();
    assert!(
        cache_len_after_prime > 0,
        "truncation cache must be non-empty after priming an Ellipsis at-tail tile"
    );

    // ── Scene with at_tail = false (scrolled back) ────────────────────────
    let mut scene_head = SceneGraph::new(720.0, 360.0);
    let tab_head = scene_head.create_tab("test", 0).unwrap();
    let lease_head = scene_head.grant_lease("head-test", 120_000);
    let tile_head = scene_head
        .create_tile(
            tab_head,
            "head-test",
            lease_head,
            Rect::new(0.0, 0.0, tile_w, tile_h),
            1,
        )
        .unwrap();
    let node_ellipsis_head = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_owned(),
            bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    scene_head
        .set_tile_root(tile_head, node_ellipsis_head)
        .unwrap();
    // at_tail = false: tile is scrolled back (or no follow-tail active).
    // tile_follow_tail_at_tail defaults to false when not set.

    compositor.prime_markdown_cache(&scene_head);

    let items_head = compositor.collect_text_items(&scene_head, 720.0, 360.0);
    assert_eq!(
        items_head.len(),
        1,
        "expected one TextItem for the head tile"
    );
    assert_eq!(
        items_head[0].viewport,
        crate::overflow::TruncationViewport::HeadAnchored,
        "non-at-tail Ellipsis tile must retain HeadAnchored viewport"
    );

    // ── Clip overflow is unaffected by at_tail ────────────────────────────
    let mut scene_clip = SceneGraph::new(720.0, 360.0);
    let tab_clip = scene_clip.create_tab("test", 0).unwrap();
    let lease_clip = scene_clip.grant_lease("clip-test", 120_000);
    let tile_clip = scene_clip
        .create_tile(
            tab_clip,
            "clip-test",
            lease_clip,
            Rect::new(0.0, 0.0, tile_w, tile_h),
            1,
        )
        .unwrap();
    let node_clip = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_owned(),
            bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    scene_clip.set_tile_root(tile_clip, node_clip).unwrap();
    scene_clip.set_tile_follow_tail_at_tail(tile_clip, true);

    compositor.prime_markdown_cache(&scene_clip);

    let items_clip = compositor.collect_text_items(&scene_clip, 720.0, 360.0);
    assert_eq!(
        items_clip.len(),
        1,
        "expected one TextItem for the clip tile"
    );
    assert_eq!(
        items_clip[0].viewport,
        crate::overflow::TruncationViewport::HeadAnchored,
        "Clip overflow at-tail tile must remain HeadAnchored (at_tail only overrides Ellipsis)"
    );
}

// ── Zone StreamText tail-anchored truncation (hud-gxz0x) ──────────────────

/// Helper: register a `LatestWins` StreamText zone covering the whole display
/// with the given `overflow` / `stream_tail_anchored` policy, publish `content`,
/// and return the scene.  The zone uses a monospace font and small geometry so
/// multi-line content overflows and is forced to truncate.
fn make_stream_zone_scene(
    overflow: Option<TextOverflow>,
    stream_tail_anchored: Option<bool>,
    content: &str,
) -> SceneGraph {
    let mut scene = SceneGraph::new(720.0, 360.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "stream".to_owned(),
        description: "streaming zone".to_owned(),
        // Relative geometry: narrow + short so several lines overflow.
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 200.0 / 720.0,
            height_pct: 40.0 / 360.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(14.0),
            font_family: Some(FontFamily::SystemMonospace),
            overflow,
            stream_tail_anchored,
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
            "stream",
            ZoneContent::StreamText(content.to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();
    scene
}

/// A streaming zone that opts into `stream_tail_anchored` produces a
/// `TailAnchored` `TextItem` so the newest content (tail) is shown; the default
/// (None) stays `HeadAnchored`, and `Clip` overflow is unaffected.
///
/// Exercises the CPU `collect_text_items` path; the `Compositor` itself needs a
/// GPU device to construct, so the test is GPU-gated like its tile sibling
/// `test_collect_text_items_at_tail_ellipsis_uses_tail_anchored_viewport`.
#[tokio::test]
async fn test_zone_stream_text_tail_anchored_opt_in_viewport() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);

    let content = "Line A\nLine B\nLine C\nLine D\nLine E\nLine F\nLine G\nLine H";

    // Opt-in: Ellipsis overflow + stream_tail_anchored = Some(true) → TailAnchored.
    let scene_tail = make_stream_zone_scene(Some(TextOverflow::Ellipsis), Some(true), content);
    let items_tail = compositor.collect_text_items(&scene_tail, 720.0, 360.0);
    assert_eq!(items_tail.len(), 1, "expected one StreamText TextItem");
    assert_eq!(
        items_tail[0].overflow,
        TextOverflow::Ellipsis,
        "policy overflow must propagate to the item"
    );
    assert_eq!(
        items_tail[0].viewport,
        crate::overflow::TruncationViewport::TailAnchored,
        "stream_tail_anchored = Some(true) must produce TailAnchored viewport \
         so the streaming zone shows the newest content (hud-gxz0x)"
    );

    // Default: Ellipsis overflow, stream_tail_anchored = None → HeadAnchored.
    let scene_head = make_stream_zone_scene(Some(TextOverflow::Ellipsis), None, content);
    let items_head = compositor.collect_text_items(&scene_head, 720.0, 360.0);
    assert_eq!(items_head.len(), 1);
    assert_eq!(
        items_head[0].viewport,
        crate::overflow::TruncationViewport::HeadAnchored,
        "default (no opt-in) zone StreamText must remain HeadAnchored \
         (no regression for existing zone users)"
    );

    // Explicit Some(false) is also head-anchored.
    let scene_false = make_stream_zone_scene(Some(TextOverflow::Ellipsis), Some(false), content);
    let items_false = compositor.collect_text_items(&scene_false, 720.0, 360.0);
    assert_eq!(
        items_false[0].viewport,
        crate::overflow::TruncationViewport::HeadAnchored,
        "stream_tail_anchored = Some(false) must remain HeadAnchored"
    );

    // Clip overflow ignores the opt-in (no truncation, head always shown).
    let scene_clip = make_stream_zone_scene(Some(TextOverflow::Clip), Some(true), content);
    let items_clip = compositor.collect_text_items(&scene_clip, 720.0, 360.0);
    assert_eq!(
        items_clip[0].viewport,
        crate::overflow::TruncationViewport::TailAnchored,
        "viewport field still reflects the opt-in even for Clip; truncation key \
         gating (effective_truncation_key) is what makes it inert for Clip"
    );
    // For Clip overflow, effective_truncation_key returns None → no truncation.
    assert!(
        crate::text::effective_truncation_key(&items_clip[0]).is_none(),
        "Clip overflow must not produce a truncation key (anchoring is inert)"
    );
}

/// End-to-end: priming the truncation cache for a tail-anchored streaming zone
/// stores a truncation that shows the **newest** content (the tail), while the
/// head-anchored default stores the **oldest**.  Proves the zone StreamText
/// frame path resolves through the shared tail-anchored helpers.
///
/// GPU-gated: `prime_truncation_cache` requires the text rasterizer.
#[tokio::test]
async fn test_zone_stream_text_tail_anchored_primes_newest_content() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Distinct first/last lines so head vs tail truncation differ observably.
    let content = "FIRST\nbbbb\ncccc\ndddd\neeee\nffff\ngggg\nLAST";

    // ── Tail-anchored zone: cache must show the newest line (LAST) ────────
    let scene_tail = make_stream_zone_scene(Some(TextOverflow::Ellipsis), Some(true), content);
    compositor.prime_markdown_cache(&scene_tail);
    compositor.prime_truncation_cache(&scene_tail);

    let items_tail = compositor.collect_text_items(&scene_tail, 720.0, 360.0);
    assert_eq!(items_tail.len(), 1);
    let key_tail = crate::text::effective_truncation_key(&items_tail[0])
        .expect("Ellipsis item must yield a truncation key");
    let cached_tail = compositor
        .text_rasterizer
        .as_ref()
        .expect("rasterizer initialised")
        .truncation_cache
        .get_by_key(&key_tail)
        .expect("tail-anchored zone StreamText must be primed (hud-gxz0x)");
    assert!(
        cached_tail.was_truncated,
        "content must overflow the zone and be truncated"
    );
    assert!(
        cached_tail.text.contains("LAST"),
        "tail-anchored truncation must show the newest content (LAST); got {:?}",
        cached_tail.text
    );
    assert!(
        !cached_tail.text.contains("FIRST"),
        "tail-anchored truncation must NOT pin the oldest content (FIRST); got {:?}",
        cached_tail.text
    );

    // ── Head-anchored default: cache must show the oldest line (FIRST) ────
    let mut compositor2 = require_gpu!(make_compositor_and_surface(720, 360).await).0;
    compositor2.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let scene_head = make_stream_zone_scene(Some(TextOverflow::Ellipsis), None, content);
    compositor2.prime_markdown_cache(&scene_head);
    compositor2.prime_truncation_cache(&scene_head);

    let items_head = compositor2.collect_text_items(&scene_head, 720.0, 360.0);
    let key_head = crate::text::effective_truncation_key(&items_head[0])
        .expect("Ellipsis item must yield a truncation key");
    let cached_head = compositor2
        .text_rasterizer
        .as_ref()
        .expect("rasterizer initialised")
        .truncation_cache
        .get_by_key(&key_head)
        .expect("head-anchored zone StreamText must be primed");
    assert!(
        cached_head.text.contains("FIRST"),
        "head-anchored truncation must show the oldest content (FIRST); got {:?}",
        cached_head.text
    );
}
