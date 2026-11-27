use super::*;

fn overlay_tile_scene(bounds: Rect) -> (SceneGraph, SceneId, SceneId) {
    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let tile_id = add_overlay_tile(&mut scene, tab_id, bounds);
    (scene, tab_id, tile_id)
}

fn add_overlay_tile(scene: &mut SceneGraph, tab_id: SceneId, bounds: Rect) -> SceneId {
    let lease_id = scene.grant_lease("agent", 60_000);
    scene
        .create_tile(tab_id, "agent", lease_id, bounds, 1)
        .unwrap()
}

fn set_overlay_hit_region(scene: &mut SceneGraph, tile_id: SceneId, hit: HitRegionNode) -> SceneId {
    let node_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: node_id,
                children: vec![],
                data: NodeData::HitRegion(hit),
            },
        )
        .unwrap();
    node_id
}

/// hud-2v8br: Tab focus draws a visible token-colored ring in the overlay's
/// chrome pass. Focus can name a node or its whole tile and belongs to a tab.
#[tokio::test]
async fn test_focused_hit_region_emits_focus_ring_in_overlay_mode() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.overlay_mode = true;
    let focus_vertices = |compositor: &Compositor, scene: &SceneGraph| {
        let mut vertices = Vec::new();
        compositor.append_focus_ring_vertices(scene, &mut vertices, 400.0, 300.0);
        vertices
    };

    let (mut scene, tab_id, tile_id) = overlay_tile_scene(Rect::new(50.0, 40.0, 300.0, 200.0));
    let node_id = set_overlay_hit_region(
        &mut scene,
        tile_id,
        HitRegionNode {
            bounds: Rect::new(20.0, 30.0, 120.0, 40.0),
            interaction_id: "portal-minimize".to_owned(),
            accepts_focus: true,
            accepts_pointer: true,
            ..Default::default()
        },
    );
    let before = focus_vertices(&compositor, &scene);
    assert!(
        before.is_empty(),
        "no focus owner must paint no ring, got {} verts",
        before.len()
    );
    compositor.focus_ring_owner = Some(crate::renderer::FocusRingOwner {
        tab_id,
        tile_id,
        node_id: Some(node_id),
    });
    let after = focus_vertices(&compositor, &scene);
    assert_eq!(
        after.len(),
        24,
        "focus ring must emit 4 edge quads (24 verts)"
    );
    let expected = compositor.gpu_color_raw(
        super::token_colors::parse_hex_color(tze_hud_scene::portal_defaults::FOCUS_RING_COLOR)
            .unwrap()
            .to_array(),
    );
    assert!(
        expected[3] > 0.0,
        "focus ring default alpha must be visible"
    );
    assert_overlay_vertex_color(&after, expected, "node focus ring");

    // A tile-level focus owner still gets a ring without a focusable node (#988).
    let (scene, tab_id, tile_id) = overlay_tile_scene(Rect::new(30.0, 20.0, 200.0, 150.0));
    compositor.focus_ring_owner = Some(crate::renderer::FocusRingOwner {
        tab_id,
        tile_id,
        node_id: None,
    });
    let vertices = focus_vertices(&compositor, &scene);
    assert_eq!(
        vertices.len(),
        24,
        "tile-level focus must emit a 4-edge ring (24 verts)"
    );
    assert_overlay_vertex_color(&vertices, expected, "tile focus ring");

    // A focusable control still gets a ring when it cannot accept composer input.
    let (mut scene, tab_id, tile_id) = overlay_tile_scene(Rect::new(0.0, 0.0, 400.0, 300.0));
    let node_id = set_overlay_hit_region(
        &mut scene,
        tile_id,
        HitRegionNode {
            bounds: Rect::new(10.0, 10.0, 80.0, 30.0),
            interaction_id: "plain-button".to_owned(),
            accepts_focus: true,
            accepts_pointer: true,
            accepts_composer_input: false,
            ..Default::default()
        },
    );
    compositor.focus_ring_owner = Some(crate::renderer::FocusRingOwner {
        tab_id,
        tile_id,
        node_id: Some(node_id),
    });
    assert_eq!(
        focus_vertices(&compositor, &scene).len(),
        24,
        "a composer-less focusable node must still get a ring"
    );

    assert_inactive_tab_focus_ring(&mut compositor);
}

/// A portal tile gets a token-colored dot-grid resize grip in overlay mode.
/// The hover tint belongs only to the tile at the pointer.
#[tokio::test]
async fn test_portal_tile_emits_resize_grip_in_overlay_mode() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.overlay_mode = true;
    let grip_vertices = |compositor: &Compositor, scene: &SceneGraph| {
        let mut vertices = Vec::new();
        compositor.append_resize_grip_vertices(scene, &mut vertices, 400.0, 300.0);
        vertices
    };

    let bounds = Rect::new(50.0, 40.0, 300.0, 200.0);
    let (mut scene, _, tile_id) = overlay_tile_scene(bounds);
    let before = grip_vertices(&compositor, &scene);
    assert!(
        before.is_empty(),
        "non-portal tile must paint no resize grip, got {} verts",
        before.len()
    );
    scene
        .register_tile_scroll_config(tile_id, tze_hud_scene::types::TileScrollConfig::vertical())
        .unwrap();
    let vertices = grip_vertices(&compositor, &scene);
    assert_eq!(
        vertices.len(),
        36,
        "resize grip must emit 6 dot quads (36 verts)"
    );
    let grip = crate::renderer::token_colors::resolve_resize_grip_tokens(&compositor.token_map);
    let resting = compositor.gpu_color_raw(grip.mark_color(false));
    assert!(
        resting[3] > 0.0,
        "resize grip default alpha must be visible"
    );
    assert_overlay_vertex_color(&vertices, resting, "portal resize grip");

    let (mut scene, _, tile_id) = overlay_tile_scene(bounds);
    scene
        .register_tile_scroll_config(tile_id, tze_hud_scene::types::TileScrollConfig::vertical())
        .unwrap();
    let hover = compositor.gpu_color_raw(grip.mark_color(true));
    assert!(
        resting
            .iter()
            .zip(hover.iter())
            .any(|(r, h)| (r - h).abs() > 1e-3),
        "resting and hover grip colors must differ for a meaningful swap"
    );

    for (hover_target, color, case) in [
        (None, resting, "no hovered tile"),
        (Some(tile_id), hover, "this hovered tile"),
        (Some(SceneId::new()), resting, "another hovered tile"),
    ] {
        compositor.resize_grip_hover = hover_target;
        assert_overlay_vertex_color(&grip_vertices(&compositor, &scene), color, case);
    }
}

/// an orphaned tile (agent disconnected, within grace) emits a
/// token-colored accent bar down its left edge; resuming the lease clears it.
/// Draw-list-level assertion (no pixel readback).
#[tokio::test]
async fn orphaned_tile_emits_disconnection_badge_draw_cmd() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.overlay_mode = true;

    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(50.0, 40.0, 300.0, 200.0),
            1,
        )
        .unwrap();

    let badge_verts = |compositor: &Compositor, scene: &SceneGraph| {
        let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
        compositor.append_disconnect_badge_vertices(scene, &mut verts, 400.0, 300.0);
        verts
    };

    assert!(
        badge_verts(&compositor, &scene).is_empty(),
        "a live tile must not show the disconnection badge"
    );

    scene.disconnect_lease(&lease_id, 1_000).unwrap();
    let verts = badge_verts(&compositor, &scene);
    assert_eq!(
        verts.len(),
        12,
        "orphaned tile must emit a bar and a label strip"
    );
    let tokens =
        crate::renderer::token_colors::resolve_disconnect_badge_tokens(&compositor.token_map);
    let ndc_x = |px: f32| px / 400.0 * 2.0 - 1.0;
    let ndc_y = |px: f32| 1.0 - px / 300.0 * 2.0;
    let extent = |quad: &[crate::pipeline::RectVertex]| {
        quad.iter().fold(
            (f32::MAX, f32::MIN, f32::MAX, f32::MIN),
            |(x0, x1, y0, y1), v| {
                (
                    x0.min(v.position[0]),
                    x1.max(v.position[0]),
                    y0.min(v.position[1]),
                    y1.max(v.position[1]),
                )
            },
        )
    };
    let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
    // Tile at x 50..350, y 40..240. The bar sits outside the left edge, so
    // agent content keeps its padding, and spans the full height.
    let (x0, x1, y0, y1) = extent(&verts[..6]);
    let bar_right = 50.0 - tokens.gap_px;
    assert!(
        close(x1, ndc_x(bar_right)),
        "bar ends a gap left of the tile"
    );
    assert!(
        close(x0, ndc_x(bar_right - tokens.width_px)),
        "bar is token-wide"
    );
    assert!(
        close(y1, ndc_y(40.0)) && close(y0, ndc_y(240.0)),
        "bar spans the tile height"
    );
    let expected = compositor.gpu_color_raw(tokens.color);
    assert!(expected[3] > 0.0, "badge default alpha must be visible");
    for v in &verts[..6] {
        assert!(
            v.color
                .iter()
                .zip(expected.iter())
                .all(|(a, b)| (a - b).abs() < 1e-3),
            "bar must carry the token-driven color"
        );
    }
    // The label strip runs along the tile's bottom edge, inside the tile.
    let (x0, x1, y0, y1) = extent(&verts[6..]);
    assert!(
        close(x0, ndc_x(50.0)) && close(x1, ndc_x(350.0)),
        "strip spans the tile width"
    );
    assert!(close(y0, ndc_y(240.0)), "strip sits on the bottom edge");
    assert!(
        close(y1, ndc_y(240.0 - tokens.label_height_px())),
        "strip fits one label line"
    );

    scene.reconnect_lease(&lease_id, 2_000).unwrap();
    assert!(
        badge_verts(&compositor, &scene).is_empty(),
        "resuming within grace must clear the badge"
    );
}

fn assert_overlay_vertex_color(
    vertices: &[crate::pipeline::RectVertex],
    expected: [f32; 4],
    case: &str,
) {
    assert!(!vertices.is_empty(), "{case}: expected colored vertices");
    for vertex in vertices {
        for (channel, (actual, want)) in vertex.color.iter().zip(expected.iter()).enumerate() {
            assert!(
                (actual - want).abs() < 1e-3,
                "{case}: color channel {channel} mismatch: {actual} vs {want}"
            );
        }
    }
}

/// the ring is per-tab — an owner on a non-active tab draws nothing.
fn assert_inactive_tab_focus_ring(compositor: &mut Compositor) {
    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let other_tab = scene.create_tab("agent2", 1).unwrap();
    let tile_id = add_overlay_tile(&mut scene, tab_id, Rect::new(0.0, 0.0, 200.0, 150.0));
    // Active tab is `tab_id`; claim focus on it but then switch active away.
    scene.switch_active_tab(other_tab).unwrap();

    compositor.focus_ring_owner = Some(crate::renderer::FocusRingOwner {
        tab_id,
        tile_id,
        node_id: None,
    });
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.append_focus_ring_vertices(&scene, &mut verts, 400.0, 300.0);
    assert!(
        verts.is_empty(),
        "an owner on a non-active tab must not draw a ring, got {} verts",
        verts.len()
    );
}

/// the runtime system card emits backdrop quads and text items
/// only while set; re-setting an identical card requests no repaint.
#[tokio::test]
async fn system_card_emits_draw_cmds_only_while_set() {
    use crate::renderer::{SystemCardKind, SystemCardModel};
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1_000, 500).await);
    let card = SystemCardModel {
        kind: SystemCardKind::Pairing,
        title: "Pair an agent".into(),
        lines: vec!["482913".into()],
    };
    let display_card = card.clone();

    assert!(compositor.system_card_vertices(800.0, 600.0).is_empty());
    assert!(
        !compositor.set_system_card(None),
        "no-op clear is not a change"
    );
    assert!(compositor.set_system_card(Some(card.clone())));
    assert!(
        !compositor.set_system_card(Some(card)),
        "identical card must not force a repaint"
    );
    assert_eq!(
        compositor.system_card_vertices(800.0, 600.0).len(),
        12,
        "backdrop + accent quads"
    );
    let texts = compositor.system_card_text_items(800.0, 600.0);
    assert_eq!(texts.len(), 2, "title + code line");
    assert_eq!(&*texts[1].text, "482913");

    assert!(compositor.set_system_card(None));
    assert!(compositor.system_card_vertices(800.0, 600.0).is_empty());
    assert!(compositor.system_card_text_items(800.0, 600.0).is_empty());

    // Render text without a card, then sample its backdrop away from glyphs.
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let first_root_id = SceneId::new();
    let mut scene = scene_with_node(Node {
        id: first_root_id,
        children: vec![],
        layout: Default::default(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "AB".into(),
            bounds: Rect::new(0.0, 0.0, 96.0, 92.0),
            font_size_px: 18.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    });
    let first_tile_id = *scene.tiles.keys().next().expect("text tile");
    let backdrop = crate::renderer::system_card::system_card_layout(
        &compositor.token_map,
        &display_card,
        1_000.0,
        500.0,
    )
    .rects[0];
    let sample_x = (backdrop.x + backdrop.w - 8.0).floor() as usize;
    let sample_y = (backdrop.y + backdrop.h - 8.0).floor() as usize;
    assert!(sample_x >= 100 && sample_y >= 100);
    assert!(sample_x + 3 < 1_000 && sample_y + 3 < 500);
    let card_pixels = |pixels: &[u8]| {
        let mut region = Vec::with_capacity(3 * 3 * 4);
        for y in sample_y..sample_y + 3 {
            for x in sample_x..sample_x + 3 {
                let offset = (y * 1_000 + x) * 4;
                region.extend_from_slice(&pixels[offset..offset + 4]);
            }
        }
        region
    };
    let change_text = |scene: &mut SceneGraph, content: &str| {
        let mut text = match &scene.nodes[&first_root_id].data {
            NodeData::TextMarkdown(text) => text.clone(),
            other => panic!("expected text root, got {other:?}"),
        };
        text.content = content.into();
        scene
            .update_node_content(first_tile_id, first_root_id, NodeData::TextMarkdown(text))
            .expect("controlled text mutation");
    };

    let assert_shapes = |work: tze_hud_telemetry::WorkCounts, expected_layout| {
        assert_eq!(
            (work.layout, work.raster, work.upload),
            (expected_layout, 0, 0),
            "{work:?}"
        );
    };
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let card_free_pixels = card_pixels(&surface.read_pixels(&compositor.device));
    assert_shapes(
        compositor.take_work_counts().expect("cold content frame"),
        1,
    );
    assert!(compositor.take_work_counts().is_none());
    compositor.render_frame_headless(&mut scene, &surface);
    assert_shapes(
        compositor.take_work_counts().expect("warm content frame"),
        0,
    );
    assert!(compositor.take_work_counts().is_none());
    assert_eq!(
        card_pixels(&surface.read_pixels(&compositor.device)),
        card_free_pixels
    );

    assert!(compositor.set_system_card(Some(display_card)));
    let overlay_shapes = compositor.system_card_text_items(1_000.0, 500.0).len() as u64;
    change_text(&mut scene, "BA");
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let visible_card_pixels = card_pixels(&surface.read_pixels(&compositor.device));
    assert_shapes(
        compositor
            .take_work_counts()
            .expect("content plus new overlay frame"),
        1 + overlay_shapes,
    );
    assert!(compositor.take_work_counts().is_none());
    assert_ne!(
        visible_card_pixels, card_free_pixels,
        "card backdrop is visible"
    );

    compositor.render_frame_headless(&mut scene, &surface);
    assert_shapes(
        compositor
            .take_work_counts()
            .expect("warm content plus overlay frame"),
        0,
    );
    assert!(compositor.take_work_counts().is_none());
    assert_eq!(
        card_pixels(&surface.read_pixels(&compositor.device)),
        visible_card_pixels
    );

    // The card stays visible when unrelated text changes again.
    change_text(&mut scene, "AB");
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    assert_eq!(
        card_pixels(&surface.read_pixels(&compositor.device)),
        visible_card_pixels,
        "card stays visible outside the changed tile"
    );

    assert_shapes(
        compositor
            .take_work_counts()
            .expect("content change with warm overlay frame"),
        1,
    );
    assert!(compositor.take_work_counts().is_none());

    assert!(compositor.set_system_card(None));
    compositor.render_frame_headless(&mut scene, &surface);
    assert_shapes(
        compositor.take_work_counts().expect("overlay clear frame"),
        0,
    );
    assert!(compositor.take_work_counts().is_none());
    assert_eq!(
        card_pixels(&surface.read_pixels(&compositor.device)),
        card_free_pixels,
        "full clean frame removes the card"
    );

    change_text(&mut scene, "BA");
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    assert_eq!(
        card_pixels(&surface.read_pixels(&compositor.device)),
        card_free_pixels,
        "a later text update must not restore stale card pixels"
    );
    assert_shapes(
        compositor
            .take_work_counts()
            .expect("later content-only frame"),
        1,
    );
    assert!(compositor.take_work_counts().is_none());
    compositor.render_frame_headless(&mut scene, &surface);
    assert_shapes(
        compositor
            .take_work_counts()
            .expect("later content-only warm frame"),
        0,
    );
    assert!(compositor.take_work_counts().is_none());
}
