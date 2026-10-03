use super::*;

// ─── hud-w41ef: portal tile backdrop fades as one unit (no see-through on a ──
// geometry change that exposes tile-backdrop-only regions) ───────────────────

/// Build a scrollable (portal-like) TextMarkdown tile with an OPAQUE background,
/// mirroring the resident portal node the projection driver publishes.
fn w41ef_portal_tile(scene: &mut SceneGraph, bounds: Rect, node_bounds: Rect) -> SceneId {
    let tab_id = scene.create_tab("t", 0).unwrap();
    let lease_id = scene.grant_lease("t", 60_000);
    let tile_id = scene.create_tile(tab_id, "t", lease_id, bounds, 1).unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            // Single short glyph: leaves the rest of the node body textless so a
            // pixel probe reads the backdrop alone (not a glyph drawn over it).
            content: "x".to_owned(),
            bounds: node_bounds,
            font_size_px: 14.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(0.9, 0.9, 0.9, 1.0),
            // Opaque backdrop (#0A0D11-ish), matching portal.transcript.background.
            background: Some(Rgba::new(0.04, 0.05, 0.07, 1.0)),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();
    tile_id
}

/// Regression for hud-w41ef: when a portal tile's opaque body is faded (§6.3
/// portal transition opacity, or any tile opacity < 1), the whole tile MUST fade
/// as one unit. Before the fix, the flat tile backdrop (`tile_background_color`)
/// and the tile text honoured the fade but the content-node background
/// (`tm.background` painted in `render_node`) did not — so a region covered only
/// by the flat backdrop (e.g. the newly-exposed area after a resize-grow, while
/// the content node still lags at its old smaller size) rendered see-through
/// while the content region stayed fully opaque. This asserts backdrop
/// uniformity: the tile-backdrop-only region and the content region paint at the
/// SAME alpha.
///
/// Asserted at the draw-list (vertex) level rather than by pixel readback: the
/// live overlay geometry pass uses the `clear_pipeline` (REPLACE, no blending),
/// but `render_frame_headless` always uses the blending pipeline, so a readback
/// composites the two overlapping backdrop quads instead of letting the last
/// write win — it cannot represent the live REPLACE alpha. The generated
/// backdrop colors, however, are blend-independent (hud-w41ef).
#[tokio::test]
async fn hud_w41ef_portal_content_background_scaled_by_tile_opacity() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    // No text renderer: `render_node` takes the fallback branch which still emits
    // the content-node background quad first. overlay_mode stays false so
    // `gpu_color` is identity and the emitted alpha is directly comparable.

    let mut scene = SceneGraph::new(256.0, 256.0);
    let bg = Rgba::new(0.04, 0.05, 0.07, 1.0); // opaque backdrop
    let tab_id = scene.create_tab("t", 0).unwrap();
    let lease_id = scene.grant_lease("t", 60_000);
    let tile_id = scene
        .create_tile(tab_id, "t", lease_id, Rect::new(0.0, 0.0, 120.0, 120.0), 1)
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: "x".to_owned(),
                    bounds: Rect::new(0.0, 0.0, 120.0, 120.0),
                    font_size_px: 14.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba::new(0.9, 0.9, 0.9, 1.0),
                    background: Some(bg),
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Ellipsis,
                    color_runs: Box::default(),
                }),
            },
        )
        .unwrap();

    // Half-fade the whole tile (deterministic stand-in for a §6.3 portal fade).
    scene.tiles.get_mut(&tile_id).unwrap().opacity = 0.5;
    let tile = scene.tiles.get(&tile_id).unwrap().clone();

    // Flat tile backdrop alpha (already opacity-scaled).
    let flat_bg = compositor
        .tile_background_color(&tile, &scene)
        .expect("markdown tile always has a flat backdrop");
    let flat_alpha = flat_bg[3];

    // Content-node backdrop quad, as emitted by render_node.
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    let mut cmds: Vec<super::draw_cmds::TexturedDrawCmd> = Vec::new();
    compositor.render_node(root_id, &tile, &scene, &mut verts, &mut cmds, 120.0, 120.0);
    let node_bg_alpha = verts
        .first()
        .expect("render_node must emit the content background quad first")
        .color[3];

    // Fix: the content-node background must be scaled by the tile opacity, exactly
    // like the flat backdrop, so the tile fades as one unit. Before the fix the
    // node background was painted at full alpha (bg.a = 1.0) while the flat
    // backdrop was 0.5 — the exact divergence that renders the tile-backdrop-only
    // region see-through relative to the content region on a resize/fade.
    assert!(
        (node_bg_alpha - 0.5 * bg.a).abs() < 1e-4,
        "content-node background alpha must be tile-opacity-scaled: got {node_bg_alpha}, \
         expected {} (bg.a {} × tile.opacity 0.5)",
        0.5 * bg.a,
        bg.a
    );
    assert!(
        (node_bg_alpha - flat_alpha).abs() < 1e-4,
        "content-node backdrop ({node_bg_alpha}) and flat tile backdrop ({flat_alpha}) \
         must paint at the SAME alpha so the tile fades uniformly (hud-w41ef)"
    );
}

/// Complement: at full tile opacity (an established portal, no fade), the grown
/// tile-backdrop-only region stays fully opaque — the desktop never shows through
/// after a resize-grow. This is the steady-state "not see-through" guarantee.
#[tokio::test]
async fn hud_w41ef_portal_backdrop_opaque_after_resize_grow_no_fade() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.overlay_mode = true;

    let mut scene = SceneGraph::new(256.0, 256.0);
    let tile_id = w41ef_portal_tile(
        &mut scene,
        Rect::new(10.0, 10.0, 100.0, 100.0),
        Rect::new(0.0, 0.0, 100.0, 100.0),
    );
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.portal_tile_anim_states.clear();

    if let Some(t) = scene.tiles.get_mut(&tile_id) {
        t.bounds.width = 200.0;
        t.bounds.height = 200.0;
        scene.version += 1;
    }
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let px = surface.read_pixels(&compositor.device);

    let a_grown = crate::surface::HeadlessSurface::pixel_at(&px, 256, 180, 180)[3];
    assert!(
        a_grown > 250,
        "grown portal backdrop must stay opaque at full opacity (got alpha={a_grown})"
    );
}

// ─── hud-b0x0m: every tile node fill type fades with the tile, not just the ──
// portal TextMarkdown background fixed in hud-w41ef. Draw-list-level assertions
// (not pixel readback): the live overlay geometry pass uses the REPLACE
// clear_pipeline while render_frame_headless always blends, so a readback cannot
// represent live overlay alpha — but the generated fill colors are
// blend-independent. overlay_mode stays false so `gpu_color` is identity and the
// emitted alpha is directly comparable to `color.a × tile_opacity`.

fn b0x0m_tile_with_root(
    scene: &mut SceneGraph,
    bounds: Rect,
    root_id: SceneId,
    data: NodeData,
) -> SceneId {
    let tab_id = scene.create_tab("t", 0).unwrap();
    let lease_id = scene.grant_lease("t", 60_000);
    let tile_id = scene.create_tile(tab_id, "t", lease_id, bounds, 1).unwrap();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data,
            },
        )
        .unwrap();
    tile_id
}

/// `prime_vertical_flow_layout` must resolve flow offsets even when
/// `init_text_renderer` has never been called (hud-tfm3p review of hud-9gopx).
///
/// hud-9gopx swapped this function from unconditionally building a fresh
/// `bundled_font_system()` to measuring against `self.text_rasterizer`'s own
/// `FontSystem` — gating the ENTIRE resolve on `self.text_rasterizer.is_some()`.
/// That silently dropped flow-stacking for every `VerticalFlow` child (not just
/// text — `SolidColor` / `StaticImage` / `HitRegion` children read
/// `tile_flow_offsets` too, per hud-pd9bp) whenever no rasterizer had been
/// initialized yet, even though non-text children never needed a `FontSystem` at
/// all — a real regression versus the pre-hud-9gopx baseline, which ran
/// regardless of rasterizer state. `make_compositor_and_surface` deliberately
/// does NOT call `init_text_renderer` (only production runtime setup and a
/// handful of text-specific tests do), so this fixture already exercises the
/// exact `text_rasterizer: None` state the regression required — no
/// GPU-specific machinery beyond the existing headless adapter is needed.
#[tokio::test]
async fn hud_tfm3p_flow_offsets_resolve_without_a_text_rasterizer() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    assert!(
        compositor.text_rasterizer.is_none(),
        "fixture precondition: this test only proves what it claims if no \
         rasterizer has been initialized"
    );

    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("t", 0).unwrap();
    let lease_id = scene.grant_lease("t", 60_000);
    let tile_id = scene
        .create_tile(tab_id, "t", lease_id, Rect::new(0.0, 0.0, 200.0, 200.0), 1)
        .unwrap();

    let parent_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: NodeLayout::VerticalFlow,
                id: parent_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 0.0),
                    bounds: Rect::new(0.0, 10.0, 200.0, 0.0),
                    radius: None,
                }),
            },
        )
        .unwrap();

    // Two non-text (SolidColor) children — flow resolution positions these
    // without ever needing a `FontSystem`, so they isolate the rasterizer-
    // presence regression from any font-measurement concern.
    let child0_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(parent_id),
            Node {
                layout: Default::default(),
                id: child0_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(1.0, 0.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 999.0, 200.0, 40.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    let child1_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(parent_id),
            Node {
                layout: Default::default(),
                id: child1_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 1.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 999.0, 200.0, 20.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    scene.nodes.get_mut(&parent_id).unwrap().children = vec![child0_id, child1_id];

    compositor.prime_vertical_flow_layout(&scene);

    assert_eq!(
        compositor.tile_flow_offsets.len(),
        2,
        "both flow children must resolve WITHOUT a text rasterizer: {:?}",
        compositor.tile_flow_offsets
    );
    let y0 = compositor.tile_flow_offsets[&child0_id];
    let y1 = compositor.tile_flow_offsets[&child1_id];
    assert!(
        (y0 - 10.0).abs() < 1e-3,
        "first child sits at the parent's own top: {y0}"
    );
    assert!(
        y1 >= y0 + 40.0,
        "second child must clear the first child's 40px height: y0={y0} y1={y1}"
    );
}

/// A non-rounded `SolidColor` node fill must be scaled by the whole-tile fade
/// (hud-b0x0m). Before the fix it was painted at `sc.color.a` regardless of tile
/// opacity — the same divergence hud-w41ef fixed for portal backgrounds.
#[tokio::test]
async fn hud_b0x0m_solid_color_node_fill_scaled_by_tile_opacity() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let color = Rgba::new(0.2, 0.4, 0.6, 0.8);
    let root_id = SceneId::new();
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tile_id = b0x0m_tile_with_root(
        &mut scene,
        Rect::new(0.0, 0.0, 120.0, 120.0),
        root_id,
        NodeData::SolidColor(SolidColorNode {
            color,
            bounds: Rect::new(0.0, 0.0, 120.0, 120.0),
            radius: None,
        }),
    );

    // Faded tile: fill alpha must track tile.opacity.
    scene.tiles.get_mut(&tile_id).unwrap().opacity = 0.5;
    let tile = scene.tiles.get(&tile_id).unwrap().clone();
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    let mut cmds: Vec<super::draw_cmds::TexturedDrawCmd> = Vec::new();
    compositor.render_node(root_id, &tile, &scene, &mut verts, &mut cmds, 120.0, 120.0);
    let faded = verts
        .first()
        .expect("SolidColor node must emit a fill quad")
        .color[3];
    assert!(
        (faded - 0.5 * color.a).abs() < 1e-4,
        "SolidColor fill alpha must be tile-opacity-scaled: got {faded}, expected {}",
        0.5 * color.a
    );

    // Full opacity: fill alpha unchanged (= color.a).
    scene.tiles.get_mut(&tile_id).unwrap().opacity = 1.0;
    let tile = scene.tiles.get(&tile_id).unwrap().clone();
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    let mut cmds: Vec<super::draw_cmds::TexturedDrawCmd> = Vec::new();
    compositor.render_node(root_id, &tile, &scene, &mut verts, &mut cmds, 120.0, 120.0);
    let full = verts.first().unwrap().color[3];
    assert!(
        (full - color.a).abs() < 1e-4,
        "at full opacity SolidColor fill alpha must be unchanged: got {full}, expected {}",
        color.a
    );
}

/// A rounded `SolidColor` node (painted via the SDF rounded-rect pass, not the
/// flat vertex pass) must also fade with the tile (hud-b0x0m).
#[tokio::test]
async fn hud_b0x0m_rounded_solid_color_scaled_by_tile_opacity() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let color = Rgba::new(0.3, 0.3, 0.35, 1.0);
    let root_id = SceneId::new();
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tile_id = b0x0m_tile_with_root(
        &mut scene,
        Rect::new(0.0, 0.0, 120.0, 120.0),
        root_id,
        NodeData::SolidColor(SolidColorNode {
            color,
            bounds: Rect::new(0.0, 0.0, 120.0, 120.0),
            radius: Some(12.0),
        }),
    );

    scene.tiles.get_mut(&tile_id).unwrap().opacity = 0.5;
    let cmds = compositor.collect_tile_rounded_rect_cmds(&scene);
    let faded = cmds
        .first()
        .expect("rounded SolidColor root must emit a rounded-rect cmd")
        .color[3];
    assert!(
        (faded - 0.5 * color.a).abs() < 1e-4,
        "rounded SolidColor alpha must be tile-opacity-scaled: got {faded}, expected {}",
        0.5 * color.a
    );

    scene.tiles.get_mut(&tile_id).unwrap().opacity = 1.0;
    let full = compositor
        .collect_tile_rounded_rect_cmds(&scene)
        .first()
        .unwrap()
        .color[3];
    assert!(
        (full - color.a).abs() < 1e-4,
        "at full opacity rounded SolidColor alpha must be unchanged: got {full}, expected {}",
        color.a
    );
}

/// A `StaticImage` textured quad's tint alpha must be scaled by the FULL tile
/// fade — `tile_effective_opacity`, which includes the §6.3 portal-transition
/// component — not just `effective_tile_opacity` (hud-b0x0m).
#[tokio::test]
async fn hud_b0x0m_static_image_tint_scaled_by_tile_opacity() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let resource_id = ResourceId::of(b"hud-b0x0m 2x2 image");
    // Register real RGBA bytes + upload the GPU texture so render_node takes the
    // textured (tint) path rather than the fallback placeholder.
    let rgba: std::sync::Arc<[u8]> = std::sync::Arc::from(vec![255u8; 2 * 2 * 4]);
    compositor.register_image_bytes(resource_id, rgba, 2, 2);
    assert!(
        compositor.ensure_image_texture(resource_id, 2, 2),
        "image texture must upload for the tint path"
    );

    let root_id = SceneId::new();
    let mut scene = SceneGraph::new(256.0, 256.0);
    scene.register_resource(resource_id);
    let tile_id = b0x0m_tile_with_root(
        &mut scene,
        Rect::new(0.0, 0.0, 120.0, 120.0),
        root_id,
        NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 2,
            height: 2,
            decoded_bytes: 2 * 2 * 4,
            fit_mode: ImageFitMode::Fill,
            bounds: Rect::new(0.0, 0.0, 120.0, 120.0),
        }),
    );

    // Drive the §6.3 portal-transition component specifically (NOT tile.opacity):
    // leave tile.opacity = 1.0 and pin a deterministic portal fade at 0.5. This is
    // the exact case the old code missed — it used `effective_tile_opacity`
    // (tile.opacity + drag = 1.0 here) and ignored the portal fade, so the image
    // stayed fully opaque while the faded tile backdrop/text went to 0.5.
    // `duration_ms: 0` makes `current_opacity_eased` return `target_opacity`
    // (time-independent), so the pinned 0.5 is deterministic.
    compositor.portal_tile_anim_states.insert(
        tile_id,
        super::draw_cmds::ZoneAnimationState {
            transition_start: std::time::Instant::now(),
            duration_ms: 0,
            from_opacity: 0.5,
            target_opacity: 0.5,
        },
    );
    assert!(
        (compositor.portal_tile_anim_opacity(tile_id) - 0.5).abs() < 1e-4,
        "test setup: portal fade must be pinned at 0.5"
    );

    let tile = scene.tiles.get(&tile_id).unwrap().clone();
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    let mut cmds: Vec<super::draw_cmds::TexturedDrawCmd> = Vec::new();
    compositor.render_node(root_id, &tile, &scene, &mut verts, &mut cmds, 120.0, 120.0);
    let tint_a = cmds
        .first()
        .expect("StaticImage with a cached texture must emit a textured draw cmd")
        .tint[3];
    assert!(
        (tint_a - 0.5).abs() < 1e-4,
        "StaticImage tint alpha must be tile-opacity-scaled: got {tint_a}, expected 0.5"
    );
}

// ─── hud-dat3x: tile text honours whole-tile opacity ─────────────────────────
// A tile whose opacity is driven to 0 (the exemplar minimize path calls
// `update_tile_opacity(0.0)`) hides its solid-color backdrop via the quad path,
// but text was collected at full opacity — leaving floating glyphs on screen.
// `collect_text_items` must now fold the same `tile_effective_opacity` the quad
// path uses into every text item: opacity 0 → ZERO items from that tile (nothing
// shaped/rasterized), a fractional opacity → items carrying the blended alpha.

/// Build a single scrollable (portal) markdown tile carrying `content` at the
/// given whole-tile `opacity`, and return `(scene, tile_id)`.
fn dat3x_markdown_tile_scene(content: &str, opacity: f32) -> (SceneGraph, SceneId) {
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("t", 0).unwrap();
    let lease_id = scene.grant_lease("t", 60_000);
    let tile_id = scene
        .create_tile(tab_id, "t", lease_id, Rect::new(0.0, 0.0, 120.0, 120.0), 1)
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: content.to_owned(),
                    bounds: Rect::new(0.0, 0.0, 120.0, 120.0),
                    font_size_px: 14.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba::new(0.9, 0.9, 0.9, 1.0),
                    background: Some(Rgba::new(0.04, 0.05, 0.07, 1.0)),
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Ellipsis,
                    color_runs: Box::default(),
                }),
            },
        )
        .unwrap();
    scene.tiles.get_mut(&tile_id).unwrap().opacity = opacity;
    (scene, tile_id)
}

/// A tile at opacity 0 must contribute NO text items — the minimize path hides
/// the backdrop AND the glyphs, together.
#[tokio::test]
async fn hud_dat3x_zero_tile_opacity_collects_no_text() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let (scene, _tile_id) = dat3x_markdown_tile_scene("hello transcript", 0.0);

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    assert!(
        items.iter().all(|t| !t.text.contains("hello")),
        "a tile at opacity 0 must yield no transcript text items, got {} item(s)",
        items.len()
    );
}

/// A tile at fractional opacity must blend its glyphs proportionally: the text
/// item carries the tile alpha (0.5), matching the backdrop fade.
#[tokio::test]
async fn hud_dat3x_fractional_tile_opacity_blends_text() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let (scene, _tile_id) = dat3x_markdown_tile_scene("hello transcript", 0.5);

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    let item = items
        .iter()
        .find(|t| t.text.contains("hello"))
        .expect("a tile at opacity 0.5 must still render its text");
    assert!(
        (item.opacity - 0.5).abs() < 1e-4,
        "tile opacity 0.5 must fold into the text item opacity: got {}",
        item.opacity
    );
}

/// Control: a fully-opaque tile renders its text at full opacity (no regression
/// to the steady-state path).
#[tokio::test]
async fn hud_dat3x_full_tile_opacity_renders_text_opaque() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let (scene, _tile_id) = dat3x_markdown_tile_scene("hello transcript", 1.0);

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    let item = items
        .iter()
        .find(|t| t.text.contains("hello"))
        .expect("a fully-opaque tile must render its text");
    assert!(
        (item.opacity - 1.0).abs() < 1e-4,
        "full tile opacity must leave text opacity at 1.0: got {}",
        item.opacity
    );
}

/// A portal fading IN (durable `tile.opacity == 1`, TRANSIENT §6.3 animation
/// opacity pinned at ~0) must STILL collect and shape its text — only the item
/// alpha rides the transient fade to ~0. The skip-shaping optimization gates on
/// the DURABLE scene-level `tile.opacity` only; gating it on the combined value
/// would defer the warm-up shape into the middle of the animation, forcing a
/// re-shape hitch when the tile crosses the visibility threshold (hud-991cj
/// steady-state reuse). This is the inverse of the durable-minimize skip.
#[tokio::test]
async fn hud_dat3x_transient_portal_fade_still_shapes_text() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let (scene, tile_id) = dat3x_markdown_tile_scene("hello transcript", 1.0);

    // Pin the §6.3 portal fade at ~0 while leaving tile.opacity = 1.0.
    // `duration_ms: 0` makes `current_opacity_eased` return `target_opacity`.
    compositor.portal_tile_anim_states.insert(
        tile_id,
        super::draw_cmds::ZoneAnimationState {
            transition_start: std::time::Instant::now(),
            duration_ms: 0,
            from_opacity: 0.0,
            target_opacity: 0.0,
        },
    );

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    let item = items
        .iter()
        .find(|t| t.text.contains("hello"))
        .expect("a fading-in portal (durable opacity 1) must STILL shape its text (warm-up)");
    assert!(
        item.opacity <= 1e-4,
        "transient fade must blend the item alpha to ~0: got {}",
        item.opacity
    );
}
