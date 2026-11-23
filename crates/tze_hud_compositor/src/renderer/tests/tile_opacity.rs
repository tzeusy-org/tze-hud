use super::*;

use crate::test_pixels::pixel_at;

async fn tile_test_gpu() -> Option<(Compositor, HeadlessSurface)> {
    let gpu = make_compositor_and_surface(256, 256).await;
    if std::env::var("TZE_HUD_REQUIRE_GPU").is_ok_and(|value| value.trim() == "1") {
        assert!(
            gpu.is_some(),
            "strict tile opacity tests require a GPU adapter"
        );
    }
    gpu
}

fn prime_text(compositor: &mut Compositor, scene: &SceneGraph) {
    compositor.prime_markdown_cache(scene);
    compositor.prime_truncation_cache(scene);
}

fn w41ef_portal_tile(scene: &mut SceneGraph, bounds: Rect, node_bounds: Rect) -> SceneId {
    let tile = b0x0m_tile_with_root(
        scene,
        bounds,
        SceneId::new(),
        NodeData::TextMarkdown(TextMarkdownNode {
            // The single glyph leaves the rest of the backdrop available for sampling.
            color: Rgba::new(0.9, 0.9, 0.9, 1.0),
            background: Some(Rgba::new(0.04, 0.05, 0.07, 1.0)),
            content: "x".into(),
            bounds: node_bounds,
            font_size_px: 14.0,
            font_family: FontFamily::SystemSansSerif,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    );
    scene
        .register_tile_scroll_config(tile, TileScrollConfig::vertical())
        .unwrap();
    tile
}

fn pin_fade(compositor: &mut Compositor, tile: SceneId, opacity: f32) {
    compositor.portal_tile_anim_states.insert(
        tile,
        ZoneAnimationState {
            transition_start: std::time::Instant::now(),
            duration_ms: 0,
            easing: super::easing::Easing::Linear,
            from_opacity: opacity,
            target_opacity: opacity,
        },
    );
}

fn upload_white_image(compositor: &mut Compositor, scene: &mut SceneGraph) -> ResourceId {
    let resource = ResourceId::of(b"tile opacity image");
    compositor.register_image_bytes(resource, std::sync::Arc::from(vec![255u8; 16]), 2, 2);
    assert!(compositor.ensure_image_texture(resource, 2, 2));
    scene.register_resource(resource);
    resource
}

fn image_data(resource_id: ResourceId, bounds: Rect) -> NodeData {
    NodeData::StaticImage(StaticImageNode {
        resource_id,
        width: 2,
        height: 2,
        decoded_bytes: 16,
        fit_mode: ImageFitMode::Fill,
        bounds,
    })
}

fn tile_pixel(frame: &crate::CapturedFrame, tile: Rect, (x, y): (u32, u32)) -> [u8; 4] {
    pixel_at(
        &frame.rgba,
        frame.width,
        tile.x as u32 + x,
        tile.y as u32 + y,
    )
}

fn set_composer_tints(
    scene: &mut SceneGraph,
    composer: SceneId,
    hover: Option<Rgba>,
    pressed: Option<Rgba>,
) {
    let NodeData::HitRegion(region) = &mut scene.nodes.get_mut(&composer).unwrap().data else {
        panic!("the generated composer must be a hit region");
    };
    region.local_style.hover_tint = hover;
    region.local_style.pressed_tint = pressed;
}

fn assert_tint(pixel: [u8; 4], dominant: usize, phase: &str) {
    for (channel, value) in pixel[..3].iter().enumerate() {
        if channel != dominant {
            assert!(
                pixel[dominant] > value.saturating_add(8),
                "{phase}: {pixel:?}"
            );
        }
    }
}

/// Live-overlay REPLACE and headless blending differ, so compare the
/// emitted flat and content backdrops directly at fractional opacity.
#[tokio::test]
async fn hud_w41ef_portal_content_background_scaled_by_tile_opacity() {
    let (mut compositor, _surface) = require_gpu!(tile_test_gpu().await);
    assert!(compositor.text_rasterizer.is_none());
    let mut scene = SceneGraph::new(256.0, 256.0);
    let bounds = Rect::new(0.0, 0.0, 120.0, 120.0);
    let tile_id = w41ef_portal_tile(&mut scene, bounds, bounds);
    scene.tiles.get_mut(&tile_id).unwrap().opacity = 0.5;
    let tile = &scene.tiles[&tile_id];
    for (level, alpha) in [
        (DegradationLevel::Nominal, 0.5),
        (DegradationLevel::Simplified, 1.0),
    ] {
        compositor.set_degradation_policy(CompositorDegradationPolicy {
            level,
            ..Default::default()
        });
        let flat = compositor.tile_background_color(tile, &scene).unwrap();
        let mut vertices = Vec::new();
        compositor.render_node(
            tile.root_node.unwrap(),
            tile,
            &scene,
            &mut vertices,
            &mut Vec::new(),
            120.0,
            120.0,
        );
        assert_eq!(
            (flat[3], vertices[0].color[3]),
            (alpha, alpha),
            "{level:?}: content and whole-tile backdrops must share alpha before text initializes"
        );
    }
}

fn assert_settled_drag_preserves_draw_alpha(
    compositor: &mut Compositor,
    scene: &mut SceneGraph,
    tile: SceneId,
) {
    pin_fade(compositor, tile, 1.0);
    let resource = upload_white_image(compositor, scene);
    let root = scene.tiles[&tile].root_node.unwrap();
    scene
        .add_node_to_tile(
            tile,
            Some(root),
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: image_data(resource, Rect::new(20.0, 20.0, 10.0, 10.0)),
            },
        )
        .unwrap();
    prime_text(compositor, scene);

    let version = scene.version;
    let mut baseline = None;
    for phase in ["settled", "activated", "moved", "released"] {
        match phase {
            "activated" => scene.set_drag_active(tile),
            "moved" => {
                let bounds = &mut scene.tiles.get_mut(&tile).unwrap().bounds;
                bounds.x += 5.0;
                bounds.y += 5.0;
                scene.bump_geometry_epoch();
            }
            "released" => scene.clear_drag_active(tile),
            _ => {}
        }
        let build = compositor.build_windowed_frame(scene, 256, 256);
        let flat: Vec<_> = build
            .flat_rect_vertices()
            .iter()
            .map(|vertex| vertex.color[3])
            .collect();
        let text: Vec<_> = compositor
            .collect_text_items(scene, 256.0, 256.0)
            .iter()
            .map(|item| item.opacity)
            .collect();
        assert!(
            !flat.is_empty() && !text.is_empty(),
            "{phase}: missing content"
        );
        let mut images = Vec::new();
        compositor.render_node(
            root,
            &scene.tiles[&tile],
            scene,
            &mut Vec::new(),
            &mut images,
            256.0,
            256.0,
        );
        let [image] = images.as_slice() else {
            panic!("{phase}: expected the uploaded image command");
        };
        let alphas = (flat, text, image.tint[3]);
        if let Some(expected) = &baseline {
            assert_eq!(&alphas, expected, "{phase}: draw alpha must stay settled");
        } else {
            baseline = Some(alphas);
        }
        assert_eq!(
            scene.version, version,
            "{phase}: drag must be geometry only"
        );
    }
}

/// Complement: at full tile opacity (an established portal, no fade), the grown
/// tile-backdrop-only region stays fully opaque — the desktop never shows through
/// after a resize-grow. This is the steady-state "not see-through" guarantee.
#[tokio::test]
async fn hud_w41ef_portal_backdrop_opaque_after_resize_grow_no_fade() {
    let (mut compositor, surface) = require_gpu!(tile_test_gpu().await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.overlay_mode = true;

    let mut scene = SceneGraph::new(256.0, 256.0);
    let tile_id = w41ef_portal_tile(
        &mut scene,
        Rect::new(10.0, 10.0, 100.0, 100.0),
        Rect::new(0.0, 0.0, 100.0, 100.0),
    );
    prime_text(&mut compositor, &scene);
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.portal_tile_anim_states.clear();

    let bounds = &mut scene.tiles.get_mut(&tile_id).unwrap().bounds;
    (bounds.width, bounds.height) = (200.0, 200.0);
    scene.version += 1;
    prime_text(&mut compositor, &scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let px = surface.read_pixels(&compositor.device);

    let a_grown = pixel_at(&px, 256, 180, 180)[3];
    assert!(
        a_grown > 250,
        "grown portal backdrop must stay opaque at full opacity (got alpha={a_grown})"
    );

    assert_settled_drag_preserves_draw_alpha(&mut compositor, &mut scene, tile_id);

    // The production expanded portal has rounded panes behind a transparent
    // transcript. Exercise the actual overlay REPLACE encoder across the
    // degradation transitions seen during native dragging, not headless blending.
    use tze_hud_portal::{
        PortalView, ProjectedPortalPresentation,
        render::{DEFAULT_COMPACT_SIZE, DEFAULT_PORTAL_SIZE, render},
        tokens::PortalVisualTokens,
    };
    let view = empty_portal_view("opacity", "Opacity");
    let rendered = render(
        &view,
        &PortalVisualTokens::default(),
        DEFAULT_PORTAL_SIZE,
        0,
    );
    let pane = |kind| {
        rendered
            .surface
            .parts
            .iter()
            .find(|part| part.kind == kind)
            .unwrap()
            .bounds
    };
    let output_pane = pane(PortalPartKind::Transcript);
    let header = pane(PortalPartKind::Header);
    let header_sample = ((header.width - 8.0) as u32, (header.height * 0.5) as u32);
    let transcript_bounds = rendered
        .children
        .iter()
        .find_map(|node| match &node.data {
            NodeData::TextMarkdown(text)
                if text.bounds.x >= output_pane.x && text.bounds.height > 100.0 =>
            {
                Some(text.bounds)
            }
            _ => None,
        })
        .expect("expanded transcript");
    let output_sample = (
        (transcript_bounds.x + transcript_bounds.width - 8.0) as u32,
        (transcript_bounds.y + transcript_bounds.height - 8.0) as u32,
    );
    let composer = rendered.composer.expect("generated interactive composer");
    let composer_bounds = composer.bounds;
    let input_sample = (
        (composer.bounds.x + composer.bounds.width * 0.8) as u32,
        (composer.bounds.y + composer.bounds.height * 0.4) as u32,
    );
    let (frame_w, frame_h) = (
        DEFAULT_PORTAL_SIZE.width as u32 + 80,
        DEFAULT_PORTAL_SIZE.height as u32 + 120,
    );
    let mut scene = SceneGraph::new(frame_w as f32, frame_h as f32);
    let tab = scene.create_tab("portal", 0).unwrap();
    let lease = scene.grant_lease("portal", 60_000);
    let bounds = Rect::new(
        10.0,
        10.0,
        DEFAULT_PORTAL_SIZE.width,
        DEFAULT_PORTAL_SIZE.height,
    );
    let tile_id = scene.create_tile(tab, "portal", lease, bounds, 1).unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    scene
        .set_tile_root_tree(tile_id, rendered.root, rendered.children)
        .unwrap();
    // The driver installs this interaction separately from the render tree.
    scene
        .set_tile_composer_interaction_checked(tile_id, composer, "portal")
        .unwrap();
    let composer_hit = scene.hit_test(10.0 + input_sample.0 as f32, 10.0 + input_sample.1 as f32);
    let composer_id = scene.overlay.tile_composer_nodes[&tile_id];
    prime_text(&mut compositor, &scene);
    compositor.build_windowed_frame(&mut scene, frame_w, frame_h);
    compositor.portal_tile_anim_states.clear(); // settled attachment, as above
    let target = crate::FrameTarget::primary(frame_w, frame_h);
    let capture = |compositor: &mut Compositor, scene: &mut SceneGraph| {
        let build = compositor.build_windowed_frame(scene, frame_w, frame_h);
        compositor
            .capture_windowed_frame(&build, &target, wgpu::TextureFormat::Rgba8UnormSrgb)
            .expect("real windowed overlay capture")
    };
    let capture_input = |compositor: &mut Compositor, scene: &mut SceneGraph, sample| {
        let frame = capture(compositor, scene);
        tile_pixel(&frame, scene.tiles[&tile_id].bounds, sample)
    };
    let set_level = |c: &mut Compositor, level| {
        c.set_degradation_policy(CompositorDegradationPolicy {
            level,
            ..Default::default()
        })
    };
    let set_hover = |scene: &mut SceneGraph, composer_id, hovered| {
        scene
            .hit_region_states
            .get_mut(&composer_id)
            .unwrap()
            .hovered = hovered;
    };
    let feedback = |compositor: &mut Compositor, scene: &mut SceneGraph, id, sample| {
        let idle = capture_input(compositor, scene, sample);
        set_hover(scene, id, true);
        let hovered = capture_input(compositor, scene, sample);
        scene.update_pressed_state(id, true);
        let pressed = capture_input(compositor, scene, sample);
        assert_eq!([hovered[3], pressed[3]], [idle[3]; 2]);
        set_hover(scene, id, false);
        scene.update_pressed_state(id, false);
        [idle, hovered, pressed]
    };
    let version = scene.version;
    let mut settled_output_alpha = None;
    for (phase, level) in [
        ("settled", DegradationLevel::Nominal),
        ("activated", DegradationLevel::Nominal),
        ("drag simplified", DegradationLevel::Simplified),
        ("moved simplified", DegradationLevel::Simplified),
        ("resized simplified", DegradationLevel::Simplified),
        ("drag recovered", DegradationLevel::Nominal),
        ("released", DegradationLevel::Nominal),
        ("released simplified", DegradationLevel::Simplified),
        ("released recovered", DegradationLevel::Nominal),
    ] {
        match phase {
            "activated" => {
                scene.set_drag_active(tile_id);
                scene
                    .update_hover_state(None, &composer_hit)
                    .expect("actual generated composer hit");
            }
            "moved simplified" => {
                let tile = scene.tiles.get_mut(&tile_id).unwrap();
                tile.bounds.x += 5.0;
                tile.bounds.y += 5.0;
                scene.bump_geometry_epoch();
            }
            "resized simplified" => {
                let bounds = &mut scene.tiles.get_mut(&tile_id).unwrap().bounds;
                bounds.width += 16.0;
                bounds.height += 16.0;
                scene.bump_geometry_epoch();
            }
            "released" => {
                scene.clear_drag_active(tile_id);
                let bounds = &mut scene.tiles.get_mut(&tile_id).unwrap().bounds;
                (bounds.width, bounds.height) =
                    (DEFAULT_PORTAL_SIZE.width, DEFAULT_PORTAL_SIZE.height);
                scene.bump_geometry_epoch();
            }
            _ => {}
        }
        set_level(&mut compositor, level);
        let frame = capture(&mut compositor, &mut scene);
        let bounds = scene.tiles[&tile_id].bounds;
        let [output, header, input] = [output_sample, header_sample, input_sample]
            .map(|sample| tile_pixel(&frame, bounds, sample));
        assert!(input[3] > 250, "{phase}: composer backdrop alpha {input:?}");
        assert!(
            input[..3].iter().any(|&channel| channel < 240),
            "{phase}: composer feedback must not replace the pane with white: {input:?}"
        );
        let settled = *settled_output_alpha.get_or_insert(output[3]);
        assert!(settled > 250, "default settled output must be opaque");
        assert_eq!(
            output[3], settled,
            "{phase}: output backdrop must survive degradation and drag"
        );
        assert!(header[3] > 250, "{phase}: header control must stay opaque");
        assert_eq!(
            scene.version, version,
            "drag and degradation leave content unchanged"
        );
    }
    let brightness = |pixel: [u8; 4]| pixel[..3].iter().map(|&v| u16::from(v)).sum::<u16>();
    let input_backdrop = scene
        .nodes
        .iter()
        .find_map(|(id, node)| match &node.data {
            NodeData::SolidColor(color) if color.bounds == composer_bounds => Some(*id),
            _ => None,
        })
        .expect("generated composer pane background");
    let NodeData::SolidColor(default_backdrop) = &scene.nodes[&input_backdrop].data else {
        unreachable!()
    };
    let default_backdrop_color = default_backdrop.color;
    use DegradationLevel::{Nominal, Simplified};
    let translucent_backdrop = Rgba::new(0.03, 0.07, 0.16, 0.35);
    let transparent = Some(Rgba::new(0.03, 0.07, 0.16, 0.0));
    let custom_tints = [Rgba::new(0.0, 1.0, 0.0, 0.8), Rgba::new(1.0, 0.0, 0.0, 0.8)];
    let clear_tints = [Rgba::new(1.0, 0.0, 0.0, 0.0), Rgba::new(0.0, 1.0, 0.0, 0.0)];
    for (level, pane_color) in [Nominal, Simplified].into_iter().flat_map(|level| {
        [None, transparent, Some(translucent_backdrop)]
            .into_iter()
            .map(move |pane_color| (level, pane_color))
    }) {
        let NodeData::SolidColor(fill) = &mut scene.nodes.get_mut(&input_backdrop).unwrap().data
        else {
            unreachable!()
        };
        fill.color = pane_color.unwrap_or(default_backdrop_color);
        let [hover, press] = pane_color.map_or([None; 2], |_| custom_tints.map(Some));
        set_composer_tints(&mut scene, composer_id, hover, press);
        scene.version += 1;
        prime_text(&mut compositor, &scene);
        set_level(&mut compositor, level);

        for opacity in [1.0, 0.4, 0.0] {
            scene.tiles.get_mut(&tile_id).unwrap().opacity = opacity;
            set_hover(&mut scene, composer_id, false);
            let [idle, hovered, pressed] =
                feedback(&mut compositor, &mut scene, composer_id, input_sample);
            if pane_color == transparent {
                assert_eq!([hovered, pressed], [idle; 2], "transparent pane");
            } else if opacity == 0.0 {
                assert_eq!([hovered, pressed], [[0; 4]; 2]);
            } else if pane_color == Some(translucent_backdrop) {
                assert_tint(hovered, 1, "custom green hover");
                assert_tint(pressed, 0, "custom red press");
            } else {
                assert!(brightness(hovered) > brightness(idle));
                assert!(brightness(pressed) < brightness(hovered));
            }
            scene.update_pressed_state(composer_id, true);
            assert_eq!(
                capture_input(&mut compositor, &mut scene, input_sample),
                pressed,
                "press overtakes hover at {level:?}, pane {pane_color:?}, opacity {opacity}"
            );
            scene.update_pressed_state(composer_id, false);
            assert_eq!(
                capture_input(&mut compositor, &mut scene, input_sample),
                idle,
                "leave restores the pane at {level:?}, pane {pane_color:?}, opacity {opacity}"
            );
        }
    }

    // Explicit zero-alpha overrides disable feedback on the translucent pane.
    let NodeData::SolidColor(fill) = &mut scene.nodes.get_mut(&input_backdrop).unwrap().data else {
        unreachable!()
    };
    fill.color = translucent_backdrop;
    scene.tiles.get_mut(&tile_id).unwrap().opacity = 1.0;
    scene.version += 1;
    prime_text(&mut compositor, &scene);
    let [hover, press] = clear_tints.map(Some);
    set_composer_tints(&mut scene, composer_id, hover, press);
    for level in [Nominal, Simplified] {
        set_level(&mut compositor, level);
        let [idle, hovered, pressed] =
            feedback(&mut compositor, &mut scene, composer_id, input_sample);
        assert_eq!([hovered, pressed], [idle; 2], "zero-alpha overrides");
    }
    set_composer_tints(&mut scene, composer_id, None, None);

    // A higher tile must still cover the lower composer's feedback in both
    // rounded and simplified rendering; no new final-chrome tint is involved.
    let bounds = scene.tiles[&tile_id].bounds;
    let upper = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(
                bounds.x + input_sample.0 as f32 - 4.0,
                bounds.y + input_sample.1 as f32 - 4.0,
                8.0,
                8.0,
            ),
            2,
        )
        .unwrap();
    scene
        .set_tile_root(
            upper,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
                    color: Rgba::new(0.0, 0.6, 0.1, 1.0),
                    radius: Some(1.0),
                }),
            },
        )
        .unwrap();
    prime_text(&mut compositor, &scene);
    for level in [DegradationLevel::Nominal, DegradationLevel::Simplified] {
        set_level(&mut compositor, level);
        set_hover(&mut scene, composer_id, false);
        let covered = capture_input(&mut compositor, &mut scene, input_sample);
        set_hover(&mut scene, composer_id, true);
        assert_eq!(
            capture_input(&mut compositor, &mut scene, input_sample),
            covered,
            "feedback stays below a higher tile"
        );
    }
    // The pointer leaves. Hover is pointer state: it survives a re-render of the
    // same composer node, so clear it before the collapsed render.
    set_hover(&mut scene, composer_id, false);

    // Exercise the native collapsed TextMarkdown backdrop at two viewport sizes.
    scene.delete_tile(upper, "portal").unwrap();
    let view = PortalView {
        presentation: ProjectedPortalPresentation::Collapsed,
        ..view
    };
    for size in [DEFAULT_PORTAL_SIZE, DEFAULT_COMPACT_SIZE] {
        let bounds = &mut scene.tiles.get_mut(&tile_id).unwrap().bounds;
        (bounds.width, bounds.height) = (size.width, size.height);
        scene.bump_geometry_epoch();
        let collapsed = render(&view, &PortalVisualTokens::default(), size, 0);
        let composer = collapsed.composer.unwrap();
        let pane = Rect::new(0.0, 0.0, size.width, size.height);
        assert_eq!(composer.bounds, pane);
        let NodeData::TextMarkdown(root) = &collapsed.root.data else {
            panic!("collapsed backdrop must be the production Markdown pane");
        };
        assert_eq!(root.bounds, pane);
        scene
            .set_tile_root_tree(tile_id, collapsed.root, collapsed.children)
            .unwrap();
        scene
            .set_tile_composer_interaction_checked(tile_id, composer, "portal")
            .unwrap();
        let composer_id = scene.overlay.tile_composer_nodes[&tile_id];
        prime_text(&mut compositor, &scene);
        let sample = ((size.width * 0.8) as u32, (size.height * 0.75) as u32);
        let edge = (size.width as u32 - 4, sample.1);
        let outside = (size.width as u32 + 3, sample.1);
        for level in [Nominal, Simplified, Nominal] {
            set_level(&mut compositor, level);
            let [idle, hovered, pressed] =
                feedback(&mut compositor, &mut scene, composer_id, sample);
            assert!(brightness(hovered) > brightness(idle), "collapsed hover");
            assert!(
                hovered[..3].iter().any(|&channel| channel < 240),
                "collapsed hover stays tinted"
            );
            assert!(brightness(pressed) < brightness(idle), "collapsed press");

            let base = capture(&mut compositor, &mut scene);
            let bounds = scene.tiles[&tile_id].bounds;
            assert_eq!(
                tile_pixel(&base, bounds, sample),
                idle,
                "collapsed leave restores backdrop"
            );
            let base_edge = tile_pixel(&base, bounds, edge);
            let base_outside = tile_pixel(&base, bounds, outside);
            set_hover(&mut scene, composer_id, true);
            let hovered = capture(&mut compositor, &mut scene);
            let edge_hover = tile_pixel(&hovered, bounds, edge);
            assert!(
                base_edge[3] > 250 && edge_hover[3] == base_edge[3],
                "collapsed edge coverage at {level:?}"
            );
            assert!(
                brightness(edge_hover) > brightness(base_edge),
                "hover reaches the pane edge"
            );
            assert_eq!(
                tile_pixel(&hovered, bounds, outside),
                base_outside,
                "hover stays in the pane"
            );
            set_hover(&mut scene, composer_id, false);
        }
    }
}

// ─── Every tile node fill type fades with the tile, not just the ──
// portal TextMarkdown background. Draw-list-level assertions
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

/// Non-text vertical-flow children resolve even before text is initialized.
#[tokio::test]
async fn hud_tfm3p_flow_offsets_resolve_without_a_text_rasterizer() {
    let (mut compositor, _surface) = require_gpu!(tile_test_gpu().await);
    assert!(compositor.text_rasterizer.is_none());
    let mut scene = SceneGraph::new(256.0, 256.0);
    let parent_id = SceneId::new();
    let solid = |y, height| {
        NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(1.0, 0.0, 0.0, 1.0),
            bounds: Rect::new(0.0, y, 200.0, height),
            radius: None,
        })
    };
    let tile_id = b0x0m_tile_with_root(
        &mut scene,
        Rect::new(0.0, 0.0, 200.0, 200.0),
        parent_id,
        solid(10.0, 0.0),
    );
    scene.nodes.get_mut(&parent_id).unwrap().layout = NodeLayout::VerticalFlow;
    let [child0_id, child1_id] = [40.0, 20.0].map(|height| {
        let id = SceneId::new();
        scene
            .add_node_to_tile(
                tile_id,
                Some(parent_id),
                Node {
                    layout: Default::default(),
                    id,
                    children: Vec::new(),
                    data: solid(999.0, height),
                },
            )
            .unwrap();
        id
    });
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

#[tokio::test]
async fn tile_opacity_scales_flat_and_rounded_solid_fill() {
    let (compositor, _surface) = require_gpu!(tile_test_gpu().await);
    let color = Rgba::new(0.2, 0.4, 0.6, 0.8);
    let bounds = Rect::new(0.0, 0.0, 120.0, 120.0);
    for radius in [None, Some(12.0)] {
        let mut scene = SceneGraph::new(256.0, 256.0);
        let root_id = SceneId::new();
        let tile_id = b0x0m_tile_with_root(
            &mut scene,
            bounds,
            root_id,
            NodeData::SolidColor(SolidColorNode {
                color,
                bounds,
                radius,
            }),
        );
        for opacity in [0.5, 1.0] {
            scene.tiles.get_mut(&tile_id).unwrap().opacity = opacity;
            let actual = if radius.is_some() {
                compositor.collect_tile_rounded_rect_cmds(&scene)[0].color[3]
            } else {
                let mut vertices = Vec::new();
                compositor.render_node(
                    root_id,
                    &scene.tiles[&tile_id],
                    &scene,
                    &mut vertices,
                    &mut Vec::new(),
                    120.0,
                    120.0,
                );
                vertices[0].color[3]
            };
            assert!(
                (actual - opacity * color.a).abs() < 1e-4,
                "{radius:?}, opacity {opacity}: {actual}"
            );
        }
    }
}

/// A `StaticImage` textured quad's tint alpha must be scaled by the FULL tile
/// fade — `tile_effective_opacity`, which includes the portal-transition
/// component — not just `effective_tile_opacity`.
#[tokio::test]
async fn hud_b0x0m_static_image_tint_scaled_by_tile_opacity() {
    let (mut compositor, _surface) = require_gpu!(tile_test_gpu().await);

    let mut scene = SceneGraph::new(256.0, 256.0);
    let resource_id = upload_white_image(&mut compositor, &mut scene);
    let root_id = SceneId::new();
    let bounds = Rect::new(0.0, 0.0, 120.0, 120.0);
    let tile_id =
        b0x0m_tile_with_root(&mut scene, bounds, root_id, image_data(resource_id, bounds));

    // Isolate the transient fade with durable tile opacity still at 1.
    pin_fade(&mut compositor, tile_id, 0.5);
    assert!(
        (compositor.portal_tile_anim_opacity(tile_id) - 0.5).abs() < 1e-4,
        "test setup: portal fade must be pinned at 0.5"
    );

    for (active, opacity, expected) in [
        (false, 1.0, 0.5),
        (false, 0.5, 0.25),
        (true, 1.0, 0.5),
        (true, 0.5, 0.25),
    ] {
        scene.tiles.get_mut(&tile_id).unwrap().opacity = opacity;
        if active {
            scene.set_drag_active(tile_id);
        }
        let mut images = Vec::new();
        compositor.render_node(
            root_id,
            &scene.tiles[&tile_id],
            &scene,
            &mut Vec::new(),
            &mut images,
            120.0,
            120.0,
        );
        let [image] = images.as_slice() else {
            panic!("cached StaticImage needs one textured command");
        };
        assert!(
            (image.tint[3] - expected).abs() < 1e-4,
            "image alpha with drag active={active} and tile opacity={opacity}"
        );
    }
}

/// Minimized tiles skip text work. A transient portal fade keeps the
/// text shaped at zero alpha so the first visible frame can reuse it.
#[tokio::test]
async fn tile_text_distinguishes_durable_opacity_from_transient_fade() {
    let (mut compositor, _surface) = require_gpu!(tile_test_gpu().await);
    let mut scene = SceneGraph::new(256.0, 256.0);
    let bounds = Rect::new(0.0, 0.0, 120.0, 120.0);
    let tile_id = w41ef_portal_tile(&mut scene, bounds, bounds);
    let root = scene.tiles[&tile_id].root_node.unwrap();
    let NodeData::TextMarkdown(text) = &mut scene.nodes.get_mut(&root).unwrap().data else {
        unreachable!()
    };
    text.content = "hello transcript".into();
    let transcript_alpha = |c: &Compositor, scene: &SceneGraph| {
        c.collect_text_items(scene, 256.0, 256.0)
            .into_iter()
            .find(|item| item.text.contains("hello"))
            .map(|item| item.opacity)
    };
    for opacity in [0.0, 0.5, 1.0] {
        scene.tiles.get_mut(&tile_id).unwrap().opacity = opacity;
        assert_eq!(
            transcript_alpha(&compositor, &scene),
            (opacity > 0.0).then_some(opacity),
            "durable tile opacity {opacity}"
        );
    }
    pin_fade(&mut compositor, tile_id, 0.0);
    assert_eq!(transcript_alpha(&compositor, &scene), Some(0.0));
}
