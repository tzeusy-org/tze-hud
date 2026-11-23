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
    use tze_hud_scene::{LinkFeedbackState, PortalReadingDecor, types::PortalPartKind};

    let (w, h) = (800_u32, 520_u32);
    let (mut compositor, surface) = require_gpu!(make_text_compositor(w, h).await);
    compositor.set_token_map(token_overrides([
        ("portal.transcript.selection_color", "#FF0000"),
        ("portal.transcript.link_color", "#00FF00"),
        ("color.error", "#FF00FF"),
    ]));

    let mut scene = SceneGraph::new(w as f32, h as f32);
    let tile_bounds = Rect::new(40.0, 20.0, 720.0, 480.0);
    let tile_id = scrollable_tile(&mut scene, "reading", tile_bounds, None);
    let mut view = empty_portal_view("reading", "Reading");
    view.visible_transcript.push(TranscriptUnit {
        sequence: 1,
        output_text: "Open [the docs page](https://example.com/docs) now\n\nSelect this whole line"
            .into(),
        output_kind: OutputKind::Assistant,
        expects_reply: false,
        appended_at_wall_us: 0,
    });
    let size = Size {
        width: tile_bounds.width,
        height: tile_bounds.height,
    };
    let tokens = PortalVisualTokens::default();
    let rendered = render(&view, &tokens, size, 3_000_000);
    scene
        .set_portal_surface(tile_id, rendered.surface, "reading")
        .unwrap();
    scene
        .set_tile_root_tree(tile_id, rendered.root, rendered.children)
        .unwrap();
    prime_text(&mut compositor, &scene);
    compositor.update_portal_tile_animations(&scene);
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
        PortalReadingDecor {
            selection: Some(sel),
            link: Some((link.start, link.end)),
            ..Default::default()
        },
    );
    let decorated = render_pixels(&mut compositor, &mut scene, &surface);

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

    // A pressed link's hint pill says what happened in place of the host;
    // failure takes `color.error`.
    let tile = scene.tiles[&tile_id].clone();
    let anchor = Rect::new(10.0, 10.0, 160.0, 20.0);
    let mut host_color = None;
    for (feedback, words) in [
        (None, "example.com"),
        (Some(LinkFeedbackState::Opening), "Opening…"),
        (Some(LinkFeedbackState::Opened), "Opened"),
        (Some(LinkFeedbackState::Failed), "Couldn't open"),
    ] {
        scene.set_portal_reading(
            tile_id,
            PortalReadingDecor {
                link: Some((link.start, link.end)),
                hint: Some(("example.com".into(), anchor)),
                hint_feedback: feedback,
                ..Default::default()
            },
        );
        let (mut rects, mut items) = (Vec::new(), Vec::new());
        compositor.collect_portal_reading_affordances(&tile, &scene, &mut rects, &mut items);
        let pill = items
            .iter()
            .find(|i| i.pixel_x == tile_bounds.x + anchor.x)
            .expect("hint pill label");
        assert_eq!(&*pill.text, words);
        let host = *host_color.get_or_insert(pill.color);
        if feedback == Some(LinkFeedbackState::Failed) {
            assert_eq!(pill.color[..3], [255, 0, 255], "color.error");
        } else {
            assert_eq!(pill.color, host);
        }
    }
    scene.set_portal_reading(tile_id, Default::default());

    // Same visible text, new target (a keyed replacement): the shape
    // signature is unchanged, but the published link must carry the new href.
    view.visible_transcript[0].output_text =
        "Open [the docs page](https://example.org/other) now\n\nSelect this whole line".into();
    let rendered = render(&view, &tokens, size, 3_000_000);
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
