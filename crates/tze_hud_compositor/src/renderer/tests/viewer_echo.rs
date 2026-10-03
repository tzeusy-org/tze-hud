use super::*;

/// hud-nx7yq.3: a runtime-authored viewer echo entry must render as a
/// kind-distinct text line above the composer strip on a raw-tile portal. This
/// is the compositor half of the "submitted text bubbles into the transcript"
/// fix — draw-list-level (no pixel readback) so it is deadlock-safe.
#[tokio::test]
async fn test_viewer_echo_renders_kind_distinct_line_above_composer() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Portal tile rooted at a composer-input HitRegion spanning the tile, so
    // there is room above the bottom input strip for history lines.
    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
            1,
        )
        .unwrap();
    let composer_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: composer_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                    interaction_id: "portal-composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    // No echoes yet → no viewer-echo text items.
    let before = compositor.collect_text_items(&scene, 400.0, 300.0);
    assert!(
        !before.iter().any(|t| t.text.contains("hello there")),
        "no viewer echo should render before any submission"
    );

    // Runtime authored a viewer reply (as append_raw_tile_viewer_echo does).
    compositor
        .viewer_echoes
        .append(tile_id, "hello there".to_owned(), 1);

    let after = compositor.collect_text_items(&scene, 400.0, 300.0);
    let echo = after
        .iter()
        .find(|t| t.text.contains("hello there"))
        .expect("viewer echo line must render after a submission");
    // hud-7ic89: the entry's timestamp (derived from submitted_at_wall_us=1,
    // i.e. 1 microsecond past UTC midnight) prefixes the message.
    assert_eq!(
        &*echo.text, "00:00  hello there",
        "viewer echo text is timestamp-prefixed"
    );

    // Kind-distinct: carries the token-driven viewer color (default accent blue),
    // not the near-white transcript text color.
    assert_eq!(
        echo.color,
        [0x8A, 0xB4, 0xF8, 0xFF],
        "viewer echo must use the kind-distinct viewer token color"
    );
    // Positioned above the bottom input strip (upper portion of the tile).
    assert!(
        echo.pixel_y < 300.0,
        "viewer echo line must sit within the tile above the composer strip"
    );
}

/// hud-hsc1t: a portal tile with ≥2 runtime-authored viewer echoes renders a
/// token-styled turn divider on the boundary between each adjacent pair of
/// entries, resolved from the shared `portal.divider.*` tokens (never hardcoded).
/// One entry → no interior divider. Draw-list-level (no readback).
#[tokio::test]
async fn test_viewer_echo_renders_turn_dividers_between_entries() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();

    // Divider token present (as `set_token_map` populates from
    // portal.divider.color / .thickness_px). Without it the pass is inert.
    compositor.markdown_tokens.separator_color = Some(Rgba::new(0.27, 0.32, 0.43, 1.0));
    compositor.markdown_tokens.separator_thickness_px = 2.0;

    let tile = |scene: &SceneGraph| -> Tile { scene.visible_tiles()[0].clone() };

    // A single echo entry yields no interior divider.
    compositor
        .viewer_echoes
        .append(tile_id, "first reply".to_owned(), 1);
    compositor.prime_viewer_echo_layout(&scene);
    assert!(
        compositor
            .collect_viewer_echo_divider_rects(&tile(&scene), &scene)
            .is_empty(),
        "one entry has no interior divider"
    );

    // Two more entries → three total → two interior dividers.
    compositor
        .viewer_echoes
        .append(tile_id, "second reply".to_owned(), 2);
    compositor
        .viewer_echoes
        .append(tile_id, "third reply".to_owned(), 3);
    compositor.prime_viewer_echo_layout(&scene);
    let rects = compositor.collect_viewer_echo_divider_rects(&tile(&scene), &scene);
    assert_eq!(
        rects.len(),
        2,
        "three echo entries render two interior turn dividers"
    );
    for rect in &rects {
        assert_eq!(
            rect.height, 2.0,
            "divider thickness comes from the portal.divider token, not a hardcode"
        );
        assert!(rect.width > 0.0, "divider spans the echo zone width");
    }
    // Dividers ascend (older boundary above newer boundary).
    assert!(
        rects[0].y < rects[1].y,
        "boundaries ordered oldest→newest top→bottom"
    );

    // Clearing the divider token disables the pass entirely.
    compositor.markdown_tokens.separator_color = None;
    assert!(
        compositor
            .collect_viewer_echo_divider_rects(&tile(&scene), &scene)
            .is_empty(),
        "no divider token ⇒ no separator geometry"
    );
}

/// hud-xgtuf: the viewer-echo stack must anchor to the TOP of the LIVE
/// (`visible_lines`-aware) composer box, so a growing multi-line draft never
/// grows into the echo history. As the composer box grows (1 → N lines) the echo
/// stack must ride upward and stay strictly above the box; shrinking back must
/// return it to the resting position. Draw-list-level (no readback).
#[tokio::test]
async fn test_viewer_echo_stack_tracks_live_composer_box() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
            1,
        )
        .unwrap();
    let composer_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: composer_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                    interaction_id: "portal-composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    compositor
        .viewer_echoes
        .append(tile_id, "the reply".to_owned(), 1);

    // Geometry the code derives internally, reconstructed here to assert against.
    let region = Rect::new(0.0, 0.0, 400.0, 300.0);
    let lhm = crate::markdown::MarkdownTokens::default().line_height_multiplier;
    let composer_font =
        super::token_colors::resolve_composer_overlay_tokens(&compositor.token_map).font_size_px;
    let echo_font =
        super::token_colors::resolve_viewer_echo_tokens(&compositor.token_map).font_size_px;
    let echo_line_h = (echo_font * lhm).max(1.0);

    let echo_y = |c: &Compositor, scene: &SceneGraph| -> f32 {
        c.collect_text_items(scene, 400.0, 300.0)
            .iter()
            .find(|t| t.text.contains("the reply"))
            .expect("viewer echo must render")
            .pixel_y
    };

    // Resting (single-line) box: echo sits strictly above the box top.
    compositor.composer_layout.visible_lines = 1.0;
    let y_rest = echo_y(&compositor, &scene);
    let box_top_1 = Compositor::composer_input_box(
        region,
        composer_font,
        lhm,
        1.0,
        ComposerVerticalAnchor::Bottom,
        6.0, // default content inset
    )
    .y;
    assert!(
        y_rest + echo_line_h <= box_top_1 + 0.5,
        "resting: echo bottom {} must be at/above the 1-line box top {box_top_1}",
        y_rest + echo_line_h
    );

    // Grow the draft to 4 lines: the echo must ride UP and stay above the taller box.
    compositor.composer_layout.visible_lines = 4.0;
    let y_grown = echo_y(&compositor, &scene);
    let box_top_4 = Compositor::composer_input_box(
        region,
        composer_font,
        lhm,
        4.0,
        ComposerVerticalAnchor::Bottom,
        6.0, // default content inset
    )
    .y;
    assert!(
        y_grown < y_rest,
        "echo must move up as the composer box grows (grown {y_grown} < resting {y_rest})"
    );
    assert!(
        y_grown + echo_line_h <= box_top_4 + 0.5,
        "grown: echo bottom {} must be at/above the 4-line box top {box_top_4} (no overlap)",
        y_grown + echo_line_h
    );

    // Shrink back to one line: the echo returns to its resting position.
    compositor.composer_layout.visible_lines = 1.0;
    let y_shrunk = echo_y(&compositor, &scene);
    assert!(
        (y_shrunk - y_rest).abs() < 0.5,
        "echo must return to the resting position on shrink ({y_shrunk} vs {y_rest})"
    );
}

/// hud-t8c3h: runtime-authored INPUT overlays must use the same viewer-local
/// portal font scale as adapter-authored OUTPUT text.  The live composer draft
/// and retained viewer-echo history own separate layout/measurement paths, so
/// cover both their shaped font sizes and their leading-derived geometry after a
/// whole-portal resize.
///
/// Draw-list-level only: this exercises the actual compositor collection + prime
/// paths without depending on pixel readback.
#[tokio::test]
async fn portal_resize_scales_runtime_authored_input_text_and_layout() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let (mut scene, tile_id) = viewer_echo_test_scene();
    let composer_id = scene.tiles[&tile_id]
        .root_node
        .expect("viewer-echo fixture must root the tile at its composer node");
    // Enough ordinary words to wrap at both scales, so this proves the prime
    // measurement path updates alongside the TextItems' draw-time font sizes.
    let draft = "resize-aware input text ".repeat(18);
    let draft_len = draft.len();
    compositor.local_composer = Some(LocalComposerState {
        text: draft.clone(),
        cursor_byte: draft_len,
        selection_anchor: draft_len,
        at_capacity: false,
        node_id: composer_id,
        placeholder: None,
    });
    compositor.viewer_echoes.append(tile_id, draft.clone(), 1);

    let composer_tokens = resolve_composer_overlay_tokens(&compositor.token_map);
    let echo_tokens = resolve_viewer_echo_tokens(&compositor.token_map);
    let snapshot = |compositor: &mut Compositor, scene: &SceneGraph| {
        compositor.prime_composer_scroll_offset(scene);
        compositor.prime_viewer_echo_layout(scene);
        let items = compositor.collect_text_items(scene, 400.0, 300.0);
        let composer = items
            .iter()
            .find(|item| item.text.as_ref() == draft)
            .expect("local composer draft must render");
        let echo = find_echo(&items);
        (
            composer.font_size_px,
            composer.bounds_height,
            composer.clip_pixel_y,
            echo.font_size_px,
            echo.bounds_height,
            echo.pixel_y,
            compositor.composer_layout.total_lines,
            compositor.viewer_echo_line_counts[&tile_id],
        )
    };

    let before = snapshot(&mut compositor, &scene);
    assert!(
        (before.0 - composer_tokens.font_size_px).abs() < 0.01,
        "unresized composer must use its base token font"
    );
    assert!(
        (before.3 - echo_tokens.font_size_px).abs() < 0.01,
        "unresized viewer history must use its base token font"
    );

    // `tile_font_scale` is the viewer-local whole-portal-resize contract used by
    // the generic OUTPUT collector. INPUT's compositor-owned overlays must follow
    // it without any adapter republish.
    const SCALE: f32 = 1.5;
    scene.set_tile_font_scale(tile_id, SCALE);
    let after = snapshot(&mut compositor, &scene);

    assert!(
        (after.0 - composer_tokens.font_size_px * SCALE).abs() < 0.01,
        "composer draw font must scale with the resized portal (got {})",
        after.0
    );
    assert!(
        (after.3 - echo_tokens.font_size_px * SCALE).abs() < 0.01,
        "viewer-history draw font must scale with the resized portal (got {})",
        after.3
    );
    assert!(
        after.1 > before.1 && after.6 > before.6,
        "composer leading/layout must be remeasured after resize: before={before:?}, after={after:?}"
    );
    assert!(
        after.4 > before.4 && after.7 > before.7,
        "viewer-history leading/wrap layout must be remeasured after resize: before={before:?}, after={after:?}"
    );
    assert!(
        (after.5 + after.4 - after.2).abs() < 0.5,
        "resized history must still bottom-align to the resized composer box: echo bottom={} box top={}",
        after.5 + after.4,
        after.2
    );
}

/// The first frame after a viewer-local resize must derive the composer fill,
/// viewer-echo dividers, and text from one freshly measured wrapped layout.
///
/// Regression for the ordering bug where `build_frame_vertices` emitted the
/// composer fill and echo dividers from the prior frame's layout, then the text
/// encode path reflowed the same draft later in the frame.  The test deliberately
/// leaves a six-line layout primed, applies a local resize scale that reflows the
/// unchanged draft, and inspects the actual staged first-frame vertices.
#[tokio::test]
async fn repro_resize_first_frame_keeps_composer_chrome_aligned_with_wrapped_text() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let (mut scene, tile_id) = viewer_echo_test_scene();
    let composer_id = scene.tiles[&tile_id]
        .root_node
        .expect("viewer-echo fixture must root the tile at its composer node");
    const SURFACE_H: f32 = 500.0;
    scene.tiles.get_mut(&tile_id).unwrap().bounds.height = SURFACE_H;
    match &mut scene.nodes.get_mut(&composer_id).unwrap().data {
        NodeData::HitRegion(region) => region.bounds.height = SURFACE_H,
        other => panic!("viewer-echo fixture root must be a composer HitRegion, got {other:?}"),
    }
    compositor
        .token_map
        .insert("portal.composer.max_lines".to_owned(), "8".to_owned());

    // At the base scale this draft occupies six wrapped rows.  Raising the
    // viewer-local scale reflows the same bytes into a taller visible box; no
    // adapter republish occurs between the two frames.
    let draft = "resize reflow ".repeat(21);
    let draft_len = draft.len();
    compositor.local_composer = Some(LocalComposerState {
        text: draft.clone(),
        cursor_byte: draft_len,
        selection_anchor: draft_len,
        at_capacity: false,
        node_id: composer_id,
        placeholder: None,
    });
    compositor
        .viewer_echoes
        .append(tile_id, "first reply".to_owned(), 1);
    compositor
        .viewer_echoes
        .append(tile_id, "second reply".to_owned(), 2);
    compositor.markdown_tokens.separator_color = Some(Rgba::WHITE);
    compositor.markdown_tokens.separator_thickness_px = 2.0;

    compositor.prime_composer_scroll_offset(&scene);
    compositor.prime_viewer_echo_layout(&scene);
    let old_visible_lines = compositor.composer_layout.visible_lines;
    assert_eq!(
        old_visible_lines, 6.0,
        "test setup: base layout must occupy six visible rows"
    );

    const RESIZED_SCALE: f32 = 1.5;
    scene.set_tile_font_scale(tile_id, RESIZED_SCALE);
    // `build_windowed_frame` is the actual first-frame staging path: it creates
    // flat chrome geometry before it prepares the later text pass.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    let frame = compositor.build_windowed_frame(&mut scene, 400, SURFACE_H as u32);

    // The text preparation at the tail of `build_windowed_frame` has resolved
    // the resized layout.  Capture its fresh geometry independently of the flat
    // vertex staging buffer so the assertion catches a one-frame mismatch.
    let fresh_layout = compositor.composer_layout;
    assert_eq!(
        fresh_layout.visible_lines, 8.0,
        "test setup: resized layout must reflow to eight visible rows"
    );
    let region = Rect::new(0.0, 0.0, 400.0, SURFACE_H);
    let composer_tokens = resolve_composer_overlay_tokens(&compositor.token_map);
    let line_height_multiplier = crate::markdown::MarkdownTokens::default().line_height_multiplier;
    let expected_box = Compositor::composer_input_box(
        region,
        composer_tokens.font_size_px * RESIZED_SCALE,
        line_height_multiplier,
        fresh_layout.visible_lines,
        composer_tokens.anchor,
        composer_tokens.content_inset_px,
    );

    // The staged composer fill is the exact flat-rect geometry presented in the
    // first frame.  Its token color is unique in this minimal scene.
    let composer_fill = [
        composer_tokens.bg_r,
        composer_tokens.bg_g,
        composer_tokens.bg_b,
        composer_tokens.bg_a,
    ];
    let fill_vertices = frame
        .flat_rect_vertices()
        .chunks_exact(6)
        .find(|rect| rect.iter().all(|vertex| vertex.color == composer_fill))
        .expect("first-frame composer fill must be staged");
    let fill_bounds = rect_vertex_bounds(fill_vertices, 400.0, SURFACE_H);
    assert_rect_close(
        fill_bounds,
        expected_box,
        "first-frame composer fill must use the resized wrapped layout",
    );

    // The later text preparation must anchor both the draft and input history to
    // that same input box, not merely update the fill on a subsequent frame.
    let items = compositor.collect_text_items(&scene, 400.0, SURFACE_H);
    let composer_text = items
        .iter()
        .find(|item| item.text.as_ref() == draft)
        .expect("resized composer text must render");
    let echo = find_echo(&items);
    assert!(
        (composer_text.clip_pixel_y - expected_box.y).abs() < 0.5,
        "resized composer text clip must start at the same box top: text={}, box={}",
        composer_text.clip_pixel_y,
        expected_box.y
    );
    assert!(
        (echo.pixel_y + echo.bounds_height - expected_box.y).abs() < 0.5,
        "resized viewer echo must bottom-align to the same box top: echo_bottom={}, box={}",
        echo.pixel_y + echo.bounds_height,
        expected_box.y
    );

    let tile = scene
        .tiles
        .get(&tile_id)
        .expect("fixture tile remains visible");
    let expected_divider = compositor
        .collect_viewer_echo_divider_rects(tile, &scene)
        .into_iter()
        .next()
        .expect("two echo entries must produce a divider");
    let sep_color = compositor
        .markdown_tokens
        .separator_color
        .expect("default portal divider color must be enabled");
    let expected_divider_color = compositor.gpu_color(sep_color);
    let divider_vertices = frame
        .flat_rect_vertices()
        .chunks_exact(6)
        .find(|rect| {
            rect.iter()
                .all(|vertex| vertex.color == expected_divider_color)
        })
        .expect("first-frame viewer-echo divider must be staged");
    let divider_bounds = rect_vertex_bounds(divider_vertices, 400.0, SURFACE_H);
    assert_rect_close(
        divider_bounds,
        expected_divider,
        "first-frame viewer-echo divider must use the resized composer anchor",
    );
}

fn rect_vertex_bounds(vertices: &[crate::pipeline::RectVertex], sw: f32, sh: f32) -> Rect {
    assert_eq!(vertices.len(), 6, "a flat rect must contain two triangles");
    let left = vertices
        .iter()
        .map(|vertex| (vertex.position[0] + 1.0) * sw * 0.5)
        .fold(f32::INFINITY, f32::min);
    let right = vertices
        .iter()
        .map(|vertex| (vertex.position[0] + 1.0) * sw * 0.5)
        .fold(f32::NEG_INFINITY, f32::max);
    let top = vertices
        .iter()
        .map(|vertex| (1.0 - vertex.position[1]) * sh * 0.5)
        .fold(f32::INFINITY, f32::min);
    let bottom = vertices
        .iter()
        .map(|vertex| (1.0 - vertex.position[1]) * sh * 0.5)
        .fold(f32::NEG_INFINITY, f32::max);
    Rect::new(left, top, right - left, bottom - top)
}

fn assert_rect_close(actual: Rect, expected: Rect, message: &str) {
    assert!(
        (actual.x - expected.x).abs() < 0.5
            && (actual.y - expected.y).abs() < 0.5
            && (actual.width - expected.width).abs() < 0.5
            && (actual.height - expected.height).abs() < 0.5,
        "{message}: actual={actual:?}, expected={expected:?}"
    );
}

// ── Viewer-echo wrap + newline rendering (hud-pncm3) ───────────────────────

/// Build a portal tile rooted at a composer-input HitRegion spanning the tile,
/// returning `(scene, tile_id)`. The composer region equals the tile, so there
/// is room above the composer box for viewer history.
fn viewer_echo_test_scene() -> (SceneGraph, SceneId) {
    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
            1,
        )
        .unwrap();
    let composer_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: composer_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                    interaction_id: "portal-composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    (scene, tile_id)
}

fn find_echo(items: &[crate::text::TextItem]) -> &crate::text::TextItem {
    items
        .iter()
        .find(|t| t.color == VIEWER_ECHO_COLOR)
        .expect("a viewer-echo text item must be present")
}

/// Geometry helpers matching what the code derives internally.
fn echo_geometry(compositor: &Compositor) -> (f32, f32) {
    let region = Rect::new(0.0, 0.0, 400.0, 300.0);
    let lhm = crate::markdown::MarkdownTokens::default().line_height_multiplier;
    let composer_font =
        super::token_colors::resolve_composer_overlay_tokens(&compositor.token_map).font_size_px;
    let echo_font =
        super::token_colors::resolve_viewer_echo_tokens(&compositor.token_map).font_size_px;
    let box_top = Compositor::composer_input_box(
        region,
        composer_font,
        lhm,
        1.0,
        ComposerVerticalAnchor::Bottom,
        6.0, // default content inset
    )
    .y;
    let echo_line_h = (echo_font * lhm).max(1.0);
    (box_top, echo_line_h)
}

/// hud-pncm3 (a): an entry with an embedded newline (Ctrl+Enter draft, #992)
/// renders as a multi-line block — the `\n` is preserved and the block is at
/// least two visual lines tall — with wrapping enabled (zone-width bounds).
#[tokio::test]
async fn test_viewer_echo_renders_embedded_newline_as_multiple_lines() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();
    compositor
        .viewer_echoes
        .append(tile_id, "line one\nline two".to_owned(), 1);

    compositor.prime_viewer_echo_layout(&scene);
    let items = compositor.collect_text_items(&scene, 400.0, 300.0);
    let echo = find_echo(&items);

    assert_eq!(
        &*echo.text, "00:00  line one\nline two",
        "the embedded newline must be preserved in the echo text (not stripped), \
         with the hud-7ic89 timestamp prefixing only the entry once (not per line)"
    );
    assert!(
        echo.bounds_width < 1000.0,
        "echo must wrap to the zone width (bounds_width {}), not a forced single line",
        echo.bounds_width
    );
    let (box_top, echo_line_h) = echo_geometry(&compositor);
    let block_height = box_top - echo.pixel_y;
    assert!(
        block_height >= 2.0 * echo_line_h - 0.5,
        "two logical lines must render a >=2-line block (height {block_height}, line {echo_line_h})"
    );
    // Bottom-aligned: the block sits directly above the composer box.
    assert!(
        (echo.pixel_y + echo.bounds_height - box_top).abs() < 0.5,
        "block bottom must align to the live composer box top"
    );
}

/// hud-7ic89: each retained viewer-echo entry's timestamp (derived from
/// `submitted_at_wall_us`) must reach the render path as a distinct, token-styled
/// `StyledRunItem` over the joined `TextItem` — muted color, smaller scale — not
/// silently dropped by the `.text`-only render helpers. Asserted at the
/// styled-runs/text-item layer (no pixel readback).
#[tokio::test]
async fn test_viewer_echo_timestamp_renders_as_styled_run() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();

    // Two entries with distinct, known submit times (12:00:00 and 13:30:45 UTC
    // day-seconds) so both the prefix text and its byte-range placement in the
    // `\n`-joined block are independently verifiable.
    let noon_us = (12 * 3600) as u64 * 1_000_000;
    let afternoon_us = (13 * 3600 + 30 * 60 + 45) as u64 * 1_000_000;
    compositor
        .viewer_echoes
        .append(tile_id, "first reply".to_owned(), noon_us);
    compositor
        .viewer_echoes
        .append(tile_id, "second reply".to_owned(), afternoon_us);
    compositor.prime_viewer_echo_layout(&scene);

    let tile = scene.visible_tiles()[0].clone();
    let tokens = super::token_colors::resolve_viewer_echo_tokens(&compositor.token_map);
    let items = compositor.collect_viewer_echo_text_items(&tile, &scene, 400.0, 300.0, &tokens);
    let echo = find_echo(&items);

    assert_eq!(
        &*echo.text, "12:00  first reply\n13:30  second reply",
        "joined block carries both entries' derived timestamp prefixes"
    );
    assert_eq!(
        echo.styled_runs.len(),
        2,
        "one timestamp styled-run per entry, got {:?}",
        echo.styled_runs
    );
    for run in echo.styled_runs.iter() {
        assert_eq!(
            run.color,
            Some(tokens.timestamp_color),
            "timestamp run must use the muted token color, not the message color"
        );
        assert_eq!(
            run.size_scale,
            Some(tokens.timestamp_font_scale),
            "timestamp run must apply the token-driven smaller scale"
        );
    }
    assert_eq!(
        &echo.text[echo.styled_runs[0].start_byte..echo.styled_runs[0].end_byte],
        "12:00  ",
        "first run's byte range must cover exactly the first entry's prefix"
    );
    assert_eq!(
        &echo.text[echo.styled_runs[1].start_byte..echo.styled_runs[1].end_byte],
        "13:30  ",
        "second run's byte range must cover exactly the second entry's prefix, \
         offset past the '\\n' joiner and the first entry's full display text"
    );
}

/// hud-7ic89: an entry with `submitted_at_wall_us == 0` (no timestamp captured —
/// e.g. a legacy append path) must render its text unchanged, with no styled run
/// and no panic — the backward-compatibility requirement.
#[tokio::test]
async fn test_viewer_echo_zero_timestamp_renders_without_prefix_or_run() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();

    compositor
        .viewer_echoes
        .append(tile_id, "no timestamp here".to_owned(), 0);
    compositor.prime_viewer_echo_layout(&scene);

    let tile = scene.visible_tiles()[0].clone();
    let tokens = super::token_colors::resolve_viewer_echo_tokens(&compositor.token_map);
    let items = compositor.collect_viewer_echo_text_items(&tile, &scene, 400.0, 300.0, &tokens);
    let echo = find_echo(&items);

    assert_eq!(&*echo.text, "no timestamp here");
    assert!(
        echo.styled_runs.is_empty(),
        "a zero-timestamp entry must not emit a timestamp styled run"
    );
}

/// hud-pncm3 (b): a single logical line wider than the zone wraps to multiple
/// visual lines (measured via the prime), rather than overflowing on one line.
#[tokio::test]
async fn test_viewer_echo_wraps_long_entry() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();
    // No newline; far wider than the ~388px zone at the echo font.
    compositor
        .viewer_echoes
        .append(tile_id, "wrap ".repeat(60).trim_end().to_owned(), 1);

    compositor.prime_viewer_echo_layout(&scene);
    let items = compositor.collect_text_items(&scene, 400.0, 300.0);
    let echo = find_echo(&items);

    let (box_top, echo_line_h) = echo_geometry(&compositor);
    let block_height = box_top - echo.pixel_y;
    assert!(
        block_height >= 2.0 * echo_line_h - 0.5,
        "a long entry must wrap to >=2 visual lines (block height {block_height})"
    );
    assert!(
        echo.bounds_width <= 400.0,
        "wrap width must be the zone width, not an unbounded single line"
    );
}

/// hud-pncm3 (c): a history taller than the band above the composer box stays
/// bounded — the scissor clips to the band and never intrudes into the box, and
/// the newest reply stays bottom-aligned to the box top.
#[tokio::test]
async fn test_viewer_echo_history_bounded_above_live_box() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();
    // Many wrapping entries → the joined block far exceeds the band height.
    for i in 0..8 {
        compositor
            .viewer_echoes
            .append(tile_id, format!("reply {i} ").repeat(20), i as u64);
    }

    compositor.prime_viewer_echo_layout(&scene);
    let items = compositor.collect_text_items(&scene, 400.0, 300.0);
    let echo = find_echo(&items);

    let region = Rect::new(0.0, 0.0, 400.0, 300.0);
    let (box_top, _) = echo_geometry(&compositor);
    let band_height = box_top - region.y;

    // The scissor is exactly the band above the box — it does not grow with the
    // history and never extends into the composer box.
    assert!(
        (echo.clip_pixel_y - region.y).abs() < 0.5,
        "clip top must be the region top"
    );
    assert!(
        (echo.clip_bounds_height - band_height).abs() < 0.5,
        "clip height must equal the band above the box (bounded), got {}",
        echo.clip_bounds_height
    );
    assert!(
        echo.clip_pixel_y + echo.clip_bounds_height <= box_top + 0.5,
        "the echo must never intrude into the live composer box"
    );
    // Newest reply stays pinned to the box top even when older lines clip.
    assert!(
        (echo.pixel_y + echo.bounds_height - box_top).abs() < 0.5,
        "block bottom (newest) must remain aligned to the box top under overflow"
    );
}
