use super::*;

// ── Portal OUTPUT reading decoration (hud-ljhku.2) ───────────────────────────

/// A real rendered portal: the selection highlight paints
/// `portal.transcript.selection_color` behind the selected text and a hovered
/// link is underlined in `portal.transcript.link_color`, both from the shaped
/// document the compositor published; clearing the decoration restores pixels.
#[tokio::test]
async fn selection_highlight_and_link_underline_use_reading_tokens() {
    use tze_hud_portal::{
        OutputKind, TranscriptUnit,
        render::{Size, render},
        tokens::PortalVisualTokens,
    };
    use tze_hud_scene::types::PortalPartKind;

    let (w, h) = (800_u32, 520_u32);
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(w, h).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut tokens = HashMap::new();
    tokens.insert(
        "portal.transcript.selection_color".to_string(),
        "#FF0000".to_string(),
    );
    tokens.insert(
        "portal.transcript.link_color".to_string(),
        "#00FF00".to_string(),
    );
    compositor.set_token_map(tokens);

    let mut scene = SceneGraph::new(w as f32, h as f32);
    let tab = scene.create_tab("reading", 0).unwrap();
    let lease = scene.grant_lease("reading", 120_000);
    let tile_bounds = Rect::new(40.0, 20.0, 720.0, 480.0);
    let tile_id = scene
        .create_tile(tab, "reading", lease, tile_bounds, 1)
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, tze_hud_scene::types::TileScrollConfig::vertical())
        .unwrap();
    let mut view = empty_portal_view("reading", "Reading");
    view.visible_transcript.push(TranscriptUnit {
        sequence: 1,
        output_text: "Open [the docs page](https://example.com/docs) now\n\nSelect this whole line"
            .into(),
        output_kind: OutputKind::Assistant,
        expects_reply: false,
        appended_at_wall_us: 0,
    });
    let rendered = render(
        &view,
        &PortalVisualTokens::default(),
        Size {
            width: tile_bounds.width,
            height: tile_bounds.height,
        },
        3_000_000,
    );
    scene
        .set_portal_surface(tile_id, rendered.surface, "reading")
        .unwrap();
    scene
        .set_tile_root_tree(tile_id, rendered.root, rendered.children)
        .unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.portal_tile_anim_states.clear();
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.publish_portal_document_layouts(&mut scene);
    let layout =
        scene.overlay.portal_document_layouts[&(tile_id, PortalPartKind::Transcript)].clone();
    assert_eq!(
        layout.reading.links.len(),
        1,
        "the HTTPS link reaches the layout"
    );
    assert!(
        !layout.reading.glyphs.is_empty(),
        "glyph extents are published"
    );
    let link = layout.reading.links[0].clone();
    assert_eq!(&layout.content[link.start..link.end], "the docs page");
    let sel_start = layout.content.find("Select").unwrap();
    let sel = (sel_start, sel_start + "Select this whole line".len());
    let scroll = scene.tile_scroll_offset_local(tile_id).1;
    let screen = |r: Rect| {
        Rect::new(
            tile_bounds.x + layout.viewport.x + r.x,
            tile_bounds.y + layout.viewport.y + r.y - scroll,
            r.width,
            r.height,
        )
    };
    let before = surface.read_pixels(&compositor.device);

    scene.set_portal_reading(
        tile_id,
        tze_hud_scene::PortalReadingDecor {
            selection: Some(sel),
            link: Some((link.start, link.end)),
            ..Default::default()
        },
    );
    compositor.render_frame_headless(&mut scene, &surface);
    let decorated = surface.read_pixels(&compositor.device);

    // Selection: the top pixel row of the selected row is glyph-free and red.
    let row = screen(layout.range_rects(sel.0, sel.1)[0]);
    let px = |buf: &[u8], x: f32, y: f32| crate::test_pixels::pixel_at(buf, w, x as u32, y as u32);
    let (sx, sy) = (row.x + row.width / 2.0, row.y + 1.0);
    let lit = px(&decorated, sx, sy);
    assert!(
        lit[0] > 200 && lit[1] < 60 && lit[2] < 60,
        "selection color: {lit:?}"
    );
    assert_ne!(px(&before, sx, sy), lit, "undecorated pixels differ");

    // Underline: the bottom pixel row of the link is the link color.
    let under = screen(layout.range_rects(link.start, link.end)[0]);
    let line = px(
        &decorated,
        under.x + under.width / 2.0,
        under.y + under.height - 0.5,
    );
    assert!(
        line[1] > 200 && line[0] < 60 && line[2] < 60,
        "link underline: {line:?}"
    );
    let outside = px(
        &decorated,
        under.x + under.width + 40.0,
        under.y + under.height - 0.5,
    );
    assert!(outside[1] < 200, "only the link is underlined: {outside:?}");

    scene.set_portal_reading(tile_id, Default::default());
    compositor.render_frame_headless(&mut scene, &surface);
    assert_eq!(
        surface.read_pixels(&compositor.device),
        before,
        "clearing restores the frame"
    );

    // Same visible text, new target (a keyed replacement): the shape
    // signature is unchanged, but the published link must carry the new href.
    view.visible_transcript[0].output_text =
        "Open [the docs page](https://example.org/other) now\n\nSelect this whole line".into();
    let rendered = render(
        &view,
        &PortalVisualTokens::default(),
        Size {
            width: tile_bounds.width,
            height: tile_bounds.height,
        },
        3_000_000,
    );
    scene
        .set_tile_root_tree(tile_id, rendered.root, rendered.children)
        .unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.publish_portal_document_layouts(&mut scene);
    let replaced = &scene.overlay.portal_document_layouts[&(tile_id, PortalPartKind::Transcript)];
    assert_eq!(replaced.signature, layout.signature, "same shaped text");
    assert_eq!(
        &*replaced.reading.links[0].href,
        "https://example.org/other"
    );
    assert_eq!(
        replaced.reading.links[0].host().as_deref(),
        Some("example.org")
    );
}
