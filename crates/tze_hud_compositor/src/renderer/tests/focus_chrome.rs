use super::*;

/// hud-2v8br: a Tab-focused hit-region node MUST emit a visible focus ring in
/// overlay mode. Before this fix the ring was computed in `tze_hud_input`
/// (`compute_ring`) but never drawn — nothing in the compositor read the
/// `hit_region_states.focused` flag — so a keyboard-only viewer had no way to
/// see where Tab focus landed on the transparent overlay ("input doesn't work").
///
/// This is a draw-list-level assertion (no pixel readback) so it is safe to run
/// synchronously without the headless llvmpipe readback deadlock.
///
/// hud-k6yvb: the ring now emits from the CHROME-LAYER pass
/// (`append_focus_ring_vertices`, above all content, §416) driven by the
/// runtime-plumbed focus owner — not from `render_node`. The behavioral
/// guarantee is preserved and asserted here: a focused node produces a visible,
/// token-colored ring of four edge quads in overlay mode at the node's
/// display-space bounds; nothing renders when focus is absent.
#[tokio::test]
async fn test_focused_hit_region_emits_focus_ring_in_overlay_mode() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    // The live windowed path is always in overlay mode; the ring must show there.
    compositor.overlay_mode = true;

    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(50.0, 40.0, 300.0, 200.0),
            1,
        )
        .unwrap();

    let node_id = SceneId::new();
    let hit = HitRegionNode {
        bounds: Rect::new(20.0, 30.0, 120.0, 40.0),
        interaction_id: "portal-minimize".to_owned(),
        accepts_focus: true,
        accepts_pointer: true,
        ..Default::default()
    };
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

    // Baseline: no focus owner plumbed → the chrome ring pass paints nothing.
    {
        let mut before: Vec<crate::pipeline::RectVertex> = Vec::new();
        compositor.append_focus_ring_vertices(&scene, &mut before, 400.0, 300.0);
        assert!(
            before.is_empty(),
            "no focus owner must paint no ring, got {} verts",
            before.len()
        );
    }

    // Plumb node-level focus → a ring of four edge quads must appear.
    compositor.focus_ring_owner = Some(crate::renderer::FocusRingOwner {
        tab_id,
        tile_id,
        node_id: Some(node_id),
    });
    let mut after: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.append_focus_ring_vertices(&scene, &mut after, 400.0, 300.0);

    // 4 edge quads × 6 vertices each.
    assert_eq!(
        after.len(),
        24,
        "focus ring must emit 4 edge quads (24 verts), got {}",
        after.len()
    );

    // Every ring vertex must carry the token-driven focus-ring color with a
    // visible (non-zero) alpha — otherwise the ring would be invisible on the
    // transparent overlay.
    let expected = compositor.gpu_color_raw(tze_hud_input::DEFAULT_FOCUS_RING_COLOR.to_array());
    assert!(
        expected[3] > 0.0,
        "focus ring default alpha must be visible"
    );
    for v in &after {
        for (c, (actual, want)) in v.color.iter().zip(expected.iter()).enumerate() {
            assert!(
                (actual - want).abs() < 1e-3,
                "ring color channel {c} mismatch: {actual} vs {want}"
            );
        }
    }
}

/// vd-crude-resize-handle-grip: a portal (scrollable) tile gets a token-colored
/// dot-grid resize grip painted at its bottom-right corner; a non-portal tile
/// (no scroll config) gets nothing. Draw-list-level assertion (no pixel
/// readback), safe to run synchronously.
#[tokio::test]
async fn test_portal_tile_emits_resize_grip_in_overlay_mode() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    // The live windowed path is always in overlay mode; the grip must show there.
    compositor.overlay_mode = true;

    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(50.0, 40.0, 300.0, 200.0),
            1,
        )
        .unwrap();

    // Baseline: a non-portal tile (no scroll config) paints no grip.
    {
        let mut before: Vec<crate::pipeline::RectVertex> = Vec::new();
        compositor.append_resize_grip_vertices(&scene, &mut before, 400.0, 300.0);
        assert!(
            before.is_empty(),
            "non-portal tile must paint no resize grip, got {} verts",
            before.len()
        );
    }

    // Mark the tile a portal (scrollable) → the grip appears.
    scene
        .register_tile_scroll_config(tile_id, tze_hud_scene::types::TileScrollConfig::vertical())
        .unwrap();
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.append_resize_grip_vertices(&scene, &mut verts, 400.0, 300.0);

    // 6 dots (lower-right triangle of a 3×3 grid) × 6 vertices each.
    assert_eq!(
        verts.len(),
        36,
        "resize grip must emit 6 dot quads (36 verts), got {}",
        verts.len()
    );

    // Every grip vertex must carry the token-driven resting grip color with a
    // visible (non-zero) alpha — otherwise the grip would be invisible on the
    // transparent overlay.
    let grip = crate::renderer::token_colors::resolve_resize_grip_tokens(&compositor.token_map);
    let expected = compositor.gpu_color_raw(grip.mark_color(false));
    assert!(
        expected[3] > 0.0,
        "resize grip default alpha must be visible"
    );
    for v in &verts {
        for (c, (actual, want)) in v.color.iter().zip(expected.iter()).enumerate() {
            assert!(
                (actual - want).abs() < 1e-3,
                "grip color channel {c} mismatch: {actual} vs {want}"
            );
        }
    }
}

/// hud-wgiys: the resize grip swaps to `hover_color` for the tile named by the
/// runtime-plumbed `resize_grip_hover` slot, and stays resting for every other
/// tile. Draw-list-level assertion on the emitted vertex colors.
#[tokio::test]
async fn test_resize_grip_swaps_to_hover_color_for_hovered_tile() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.overlay_mode = true;

    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(50.0, 40.0, 300.0, 200.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, tze_hud_scene::types::TileScrollConfig::vertical())
        .unwrap();

    let grip = crate::renderer::token_colors::resolve_resize_grip_tokens(&compositor.token_map);
    let resting = compositor.gpu_color_raw(grip.mark_color(false));
    let hover = compositor.gpu_color_raw(grip.mark_color(true));
    // The tokens must actually differ, or the test could not tell the swap apart.
    assert!(
        resting
            .iter()
            .zip(hover.iter())
            .any(|(r, h)| (r - h).abs() > 1e-3),
        "resting and hover grip colors must differ for a meaningful swap"
    );

    let colors_of = |compositor: &Compositor, scene: &SceneGraph| {
        let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
        compositor.append_resize_grip_vertices(scene, &mut verts, 400.0, 300.0);
        verts
    };
    let all_match = |verts: &[crate::pipeline::RectVertex], want: [f32; 4]| {
        !verts.is_empty()
            && verts.iter().all(|v| {
                v.color
                    .iter()
                    .zip(want.iter())
                    .all(|(a, b)| (a - b).abs() < 1e-3)
            })
    };

    // No hover target → resting color.
    compositor.resize_grip_hover = None;
    assert!(
        all_match(&colors_of(&compositor, &scene), resting),
        "with no hover target the grip must render the resting color"
    );

    // Hover slot names this tile → hover color.
    compositor.resize_grip_hover = Some(tile_id);
    assert!(
        all_match(&colors_of(&compositor, &scene), hover),
        "hovering this tile's resize corner must swap the grip to hover_color"
    );

    // Hover slot names a different tile → this tile stays resting.
    compositor.resize_grip_hover = Some(SceneId::new());
    assert!(
        all_match(&colors_of(&compositor, &scene), resting),
        "a hover target on another tile must not light this tile's grip"
    );
}

/// hud-jm8nq.12: an orphaned tile (agent disconnected, within grace) emits a
/// token-colored disconnection badge quad; resuming the lease clears it.
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
    assert_eq!(verts.len(), 6, "orphaned tile must emit one badge quad");
    let tokens =
        crate::renderer::token_colors::resolve_disconnect_badge_tokens(&compositor.token_map);
    let expected = compositor.gpu_color_raw(tokens.color);
    assert!(expected[3] > 0.0, "badge default alpha must be visible");
    for v in &verts {
        assert!(
            v.color
                .iter()
                .zip(expected.iter())
                .all(|(a, b)| (a - b).abs() < 1e-3),
            "badge must carry the token-driven color"
        );
    }

    scene.reconnect_lease(&lease_id, 2_000).unwrap();
    assert!(
        badge_verts(&compositor, &scene).is_empty(),
        "resuming within grace must clear the badge"
    );
}

/// hud-k6yvb: a TILE-LEVEL focus owner (a non-passthrough tile with no focusable
/// nodes) must get a visible ring around the whole tile from the chrome pass —
/// the case #988 could not draw because tile-level focus has no scene state.
#[tokio::test]
async fn test_tile_level_focus_owner_emits_ring_in_overlay_mode() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.overlay_mode = true;

    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(30.0, 20.0, 200.0, 150.0),
            1,
        )
        .unwrap();

    compositor.focus_ring_owner = Some(crate::renderer::FocusRingOwner {
        tab_id,
        tile_id,
        node_id: None, // tile-level stop
    });
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.append_focus_ring_vertices(&scene, &mut verts, 400.0, 300.0);

    assert_eq!(
        verts.len(),
        24,
        "tile-level focus must emit a 4-edge ring (24 verts), got {}",
        verts.len()
    );
    let expected = compositor.gpu_color_raw(tze_hud_input::DEFAULT_FOCUS_RING_COLOR.to_array());
    for v in &verts {
        for (actual, want) in v.color.iter().zip(expected.iter()) {
            assert!(
                (actual - want).abs() < 1e-3,
                "ring must use the token color"
            );
        }
    }
}

/// hud-k6yvb: a focusable node in a COMPOSER-LESS tile still gets a ring (the
/// ring is independent of typing-recovery / composer presence).
#[tokio::test]
async fn test_composerless_node_focus_emits_ring() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.overlay_mode = true;

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
    // A plain focusable control — NOT a composer (accepts_composer_input = false).
    let node_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(10.0, 10.0, 80.0, 30.0),
                    interaction_id: "plain-button".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: false,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    compositor.focus_ring_owner = Some(crate::renderer::FocusRingOwner {
        tab_id,
        tile_id,
        node_id: Some(node_id),
    });
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.append_focus_ring_vertices(&scene, &mut verts, 400.0, 300.0);
    assert_eq!(
        verts.len(),
        24,
        "a composer-less focusable node must still get a ring, got {}",
        verts.len()
    );
}

/// hud-k6yvb: the ring is per-tab — an owner on a non-active tab draws nothing.
#[tokio::test]
async fn test_focus_ring_suppressed_on_non_active_tab() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.overlay_mode = true;

    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let other_tab = scene.create_tab("agent2", 1).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 150.0),
            1,
        )
        .unwrap();
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

/// hud-i2e10.4: the runtime system card emits backdrop quads and text items
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
    let retained_card = card.clone();

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

    // Seed the actual retained baseline without a card, then sample a small
    // backdrop region outside the changed tile and away from card glyphs.
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (mut scene, first_tile_id, first_root_id) = canonical_retained_scene();
    let backdrop = crate::renderer::system_card::system_card_layout(
        &compositor.token_map,
        &retained_card,
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
            other => panic!("expected canonical text root, got {other:?}"),
        };
        text.content = content.into();
        scene
            .update_node_content(first_tile_id, first_root_id, NodeData::TextMarkdown(text))
            .expect("controlled canonical text mutation");
    };

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let baseline_work = compositor.take_work_counts().expect("card-free baseline");
    assert!(baseline_work.full_frame, "{baseline_work:?}");
    assert_eq!(baseline_work.damage_px, 1_000 * 500);
    let card_free_pixels = card_pixels(&surface.read_pixels(&compositor.device));

    assert!(compositor.set_system_card(Some(retained_card)));
    change_text(&mut scene, "BA");
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let first_card_work = compositor.take_work_counts().expect("first card frame");
    assert!(first_card_work.full_frame, "{first_card_work:?}");
    assert_eq!(first_card_work.damage_px, 1_000 * 500);
    let visible_card_pixels = card_pixels(&surface.read_pixels(&compositor.device));
    assert_ne!(
        visible_card_pixels, card_free_pixels,
        "card backdrop is visible"
    );

    // A second eligible text change must still use the full card pass; the
    // preceding card frame must not have become a retained baseline.
    change_text(&mut scene, "AB");
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let second_card_work = compositor.take_work_counts().expect("second card frame");
    assert!(second_card_work.full_frame, "{second_card_work:?}");
    assert_eq!(second_card_work.damage_px, 1_000 * 500);
    assert_eq!(
        card_pixels(&surface.read_pixels(&compositor.device)),
        visible_card_pixels,
        "card stays visible outside the changed tile"
    );

    assert!(compositor.set_system_card(None));
    compositor.render_frame_headless(&mut scene, &surface);
    let clear_work = compositor.take_work_counts().expect("clean card removal");
    assert!(clear_work.full_frame, "{clear_work:?}");
    assert_eq!(clear_work.damage_px, 1_000 * 500);
    assert_eq!(
        card_pixels(&surface.read_pixels(&compositor.device)),
        card_free_pixels,
        "full clean frame removes the card"
    );

    change_text(&mut scene, "BA");
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let resumed_work = compositor
        .take_work_counts()
        .expect("resumed retained frame");
    assert!(!resumed_work.full_frame, "{resumed_work:?}");
    assert!(resumed_work.damage_px < 1_000 * 500, "{resumed_work:?}");
    assert_eq!(
        card_pixels(&surface.read_pixels(&compositor.device)),
        card_free_pixels,
        "retained update must not restore stale card pixels"
    );
}
