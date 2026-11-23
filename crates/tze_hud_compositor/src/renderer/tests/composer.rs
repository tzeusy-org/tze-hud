use super::*;

/// The focused echo with its primed layout, as the frame draws it.
fn focused(compositor: &Compositor) -> (&LocalComposerState, image_cache::ComposerLayout) {
    let cs = compositor
        .local_composer
        .as_ref()
        .expect("focused composer");
    (cs, compositor.composer_layout)
}

#[test]
fn local_composer_state_handle_slot_semantics() {
    let handle = LocalComposerStateHandle::default();
    let mut current = None;
    assert!(!apply_composer_slot(&handle, &mut current));
    assert!(current.is_none());

    let node = SceneId::new();
    *handle.lock().unwrap() = Some(Some(composer_draft(node, "hello", 5)));
    assert!(apply_composer_slot(&handle, &mut current));
    assert!(handle.lock().unwrap().is_none());
    assert!(!apply_composer_slot(&handle, &mut current));
    let draft = current.as_ref().unwrap();
    assert_eq!((draft.text.as_str(), draft.cursor_byte), ("hello", 5));
    assert_eq!(draft.node_id, node);
    assert!(!draft.at_capacity);

    *handle.lock().unwrap() = Some(None);
    assert!(!apply_composer_slot(&handle, &mut current));
    assert!(current.is_none());
}

#[tokio::test]
async fn composer_echo_caret_and_kept_draft_share_the_input_strip() {
    use std::time::Instant;
    let (mut compositor, _) = require_gpu!(make_text_compositor(320, 200).await);
    let (scene, tile_id, hit_id) = composer_scene(
        (320.0, 200.0),
        Rect::new(20.0, 30.0, 200.0, 120.0),
        Rect::new(12.0, 16.0, 140.0, 72.0),
        Rgba::new(0.0, 0.0, 0.0, 0.0),
    );
    let tokens = resolve_composer_overlay_tokens(&HashMap::new());
    assert_eq!(tokens.placeholder_color, [0x6B, 0x76, 0x89, 0xFF]);
    let item = |c: &Compositor| {
        c.collect_composer_text_item(
            focused(c),
            &scene.tiles[&tile_id],
            &scene,
            320.0,
            200.0,
            &resolve_composer_overlay_tokens(&c.token_map),
        )
        .expect("focused composer must produce a text item")
    };

    compositor.local_composer = Some(composer_draft(hit_id, "hello", 5));
    let echo = item(&compositor);
    let strip_height = 16.0 * crate::text::LINE_HEIGHT_MULTIPLIER + 12.0;
    let strip_top = 46.0 + (72.0 - strip_height);
    assert_eq!(
        [
            echo.pixel_x,
            echo.pixel_y,
            echo.bounds_width,
            echo.bounds_height
        ],
        [38.0, strip_top + 6.0, 128.0, strip_height - 12.0]
    );
    assert_eq!(
        [echo.clip_pixel_y, echo.clip_bounds_height],
        [strip_top, strip_height]
    );

    let hint = "Type a message…";
    for (draft, placeholder, expected, dimmed) in [
        ("", Some(hint), hint, true),
        ("hi", Some(hint), "hi", false),
        ("", None, "", false),
    ] {
        compositor.local_composer = Some(LocalComposerState {
            placeholder: placeholder.map(str::to_owned),
            ..composer_draft(hit_id, draft, draft.len())
        });
        let echo = item(&compositor);
        assert_eq!(echo.text.as_ref(), expected);
        assert_eq!(echo.color == tokens.placeholder_color, dimmed);
        assert!(echo.styled_runs.is_empty());
    }

    compositor.set_token_map(token_overrides([(
        "portal.composer.preedit_underline_color",
        "#FF0000",
    )]));
    // The IME echo and underline use the current shaped strip until cancellation.
    let composition = ComposerPreedit {
        text: "かん".into(),
        cursor: None,
    };
    for (preedit, expected) in [(Some(composition), "abかん"), (None, "ab")] {
        *compositor.local_composer_state.lock().unwrap() = Some(Some(LocalComposerState {
            preedit,
            ..composer_draft(hit_id, "ab", 2)
        }));
        compositor.drain_local_composer_state();
        compositor.prime_composer_scroll_offset(&scene);
        assert_eq!(item(&compositor).text.as_ref(), expected);
        let mut vertices = Vec::new();
        compositor.append_composer_preedit_underline_vertices(&scene, &mut vertices, 320.0, 200.0);
        if expected == "ab" {
            assert!(vertices.is_empty(), "cancelling clears the underline");
            continue;
        }
        assert_eq!(vertices.len(), 6, "one underline quad");
        assert!(
            vertices
                .iter()
                .all(|vertex| vertex.color == [1.0, 0.0, 0.0, 1.0])
        );
        let bounds = rect_vertex_bounds(&vertices, 320.0, 200.0);
        let (caret, end) = compositor
            .text_rasterizer
            .as_mut()
            .unwrap()
            .measure_composer_caret(expected, 2, 16.0, crate::text::LINE_HEIGHT_MULTIPLIER);
        assert!(caret > 0.0 && (bounds.x - echo.pixel_x - caret).abs() < 0.5);
        assert!((bounds.x + bounds.width - echo.pixel_x - end).abs() < 0.5);
        let want_top = echo.pixel_y + (echo.bounds_height + 16.0) * 0.5;
        assert!((bounds.y - want_top).abs() < 1e-2 && (bounds.height - 1.5).abs() < 1e-3);
    }

    // Single-line hit testing uses the shaped row and its trailing byte sentinel.
    compositor
        .token_map
        .insert("portal.composer.max_lines".into(), "1".into());
    compositor.local_composer = Some(composer_draft(hit_id, "hi", 2));
    compositor.prime_composer_scroll_offset(&scene);
    let visual = compositor
        .composer_visual_layout
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    assert_eq!((visual.text_len, visual.lines.len()), (2, 1));
    let line = &visual.lines[0];
    assert_eq!((line.start_byte, line.end_byte), (0, 2));
    assert_eq!(line.glyph_x.last().map(|&(byte, _)| byte), Some(2));
    assert!(visual.input_box.is_none());
    assert_eq!(visual.x_at_cursor(0), 0.0);
    assert!(visual.x_at_cursor(2) > 0.0);

    let hidden_phase = || Instant::now() - (CARET_BLINK_HALF_PERIOD + CARET_BLINK_HALF_PERIOD / 2);
    let caret = |c: &Compositor| {
        let mut vertices = Vec::new();
        c.append_composer_caret_vertices(&scene, &mut vertices, 320.0, 200.0);
        vertices
    };
    compositor.local_composer = Some(composer_draft(hit_id, "", 0));
    compositor.prime_composer_scroll_offset(&scene);
    compositor.composer_caret_blink_start = Instant::now();
    let quad = caret(&compositor);
    assert_eq!(quad.len(), 6);
    let bounds = rect_vertex_bounds(&quad, 320.0, 200.0);
    assert!((bounds.x - 38.0).abs() < 0.1 && (bounds.y - strip_top - 6.0).abs() < 0.1);
    compositor.composer_caret_blink_start = hidden_phase();
    assert!(caret(&compositor).is_empty(), "blinking caret hides");
    compositor.local_composer = Some(composer_draft(hit_id, "hello world", 5));
    for started in [Instant::now(), hidden_phase()] {
        compositor.composer_caret_blink_start = started;
        assert_eq!(item(&compositor).text.as_ref(), "hello world");
    }
    let mut steady_tokens = compositor.token_map.clone();
    steady_tokens.insert(
        tze_hud_config::tokens::CARET_BLINK_MS_KEY.into(),
        "0".into(),
    );
    compositor.set_token_map(steady_tokens);
    compositor.composer_caret_blink_start = hidden_phase();
    assert_eq!(caret(&compositor).len(), 6, "steady caret stays drawn");

    compositor
        .token_map
        .insert("portal.composer.anchor".into(), "top".into());
    compositor.token_map.remove("portal.composer.max_lines");
    for draft in ["", "h"] {
        compositor.local_composer = Some(composer_draft(hit_id, draft, draft.len()));
        compositor.prime_composer_scroll_offset(&scene);
        let top = item(&compositor);
        assert_eq!((top.pixel_x, top.pixel_y), (38.0, 52.0));
    }

    // Kept drafts request one frame, draw without a caret and yield to the
    // echo without duplicating text when focus returns to the same composer.
    compositor.local_composer = None;
    compositor.set_token_map(HashMap::new());
    let draft_count = |compositor: &Compositor| {
        compositor
            .collect_text_items(&scene, 320.0, 200.0)
            .iter()
            .filter(|item| item.text.contains("half a reply"))
            .inspect(|item| assert_eq!(item.text.as_ref(), "half a reply"))
            .count()
    };
    *compositor.kept_composer_drafts_state.lock().unwrap() =
        Some(vec![(hit_id, "half a reply".into())]);
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(!compositor.drain_local_composer_and_needs_render());
    assert!(caret(&compositor).is_empty(), "a kept draft has no caret");
    assert_eq!(draft_count(&compositor), 1);
    let (_, kept_layout) = compositor.drawn_composers().next().unwrap();
    assert!(
        !kept_layout.wrap,
        "a kept draft occupies one unscrolled line"
    );
    compositor.local_composer = Some(composer_draft(hit_id, "half a reply", 4));
    assert_eq!(compositor.drawn_composers().count(), 1);
    assert_eq!(draft_count(&compositor), 1);
}

#[tokio::test]
async fn composer_selection_styled_run_uses_raw_byte_offsets() {
    let (mut compositor, _) = require_gpu!(make_text_compositor(320, 200).await);
    let (scene, tile_id, hit_id) = composer_scene(
        (320.0, 200.0),
        Rect::new(20.0, 30.0, 200.0, 120.0),
        Rect::new(12.0, 16.0, 140.0, 72.0),
        Rgba::BLACK,
    );
    let tokens = resolve_composer_overlay_tokens(&compositor.token_map);
    for (cursor, anchor, expected) in [
        (2, 4, Some((2, 4))),
        (4, 2, Some((2, 4))),
        (0, 5, Some((0, 5))),
        (5, 0, Some((0, 5))),
        (3, 3, None),
    ] {
        compositor.local_composer = Some(LocalComposerState {
            selection_anchor: anchor,
            ..composer_draft(hit_id, "hello", cursor)
        });
        let echo = compositor
            .collect_composer_text_item(
                focused(&compositor),
                &scene.tiles[&tile_id],
                &scene,
                320.0,
                200.0,
                &tokens,
            )
            .expect("focused composer must produce a text item");
        assert_eq!(echo.text.as_ref(), "hello");
        assert_eq!(echo.styled_runs.len(), usize::from(expected.is_some()));
        if let Some(range) = expected {
            let run = &echo.styled_runs[0];
            assert_eq!((run.start_byte, run.end_byte), range);
            assert_eq!(run.background_color, Some(tokens.selection_bg));
        }
    }
}

#[tokio::test]
async fn composer_glyphs_stay_inside_input_panes() {
    let (mut compositor, _) = require_gpu!(make_text_compositor(600, 400).await);
    for (surface_size, pane, draft, bottom_strip_minimum) in [
        (
            (600, 300),
            Rect::new(0.0, 0.0, 600.0, 300.0),
            "M".repeat(200),
            Some(1000),
        ),
        (
            (600, 400),
            Rect::new(0.0, 0.0, 400.0, 60.0),
            "word ".repeat(40),
            None,
        ),
        (
            (400, 160),
            Rect::new(20.0, 60.0, 120.0, 40.0),
            "M".repeat(60),
            None,
        ),
    ] {
        let (width, height) = surface_size;
        let surface = HeadlessSurface::new(&compositor.device, width, height);
        let size = (width as f32, height as f32);
        let (mut scene, _, hit) = composer_scene(
            size,
            Rect::new(0.0, 0.0, size.0, size.1),
            pane,
            Rgba::new(0.02, 0.02, 0.02, 1.0),
        );
        compositor.local_composer = Some(composer_draft(hit, &draft, draft.len()));
        prime_text(&mut compositor, &scene);
        let pixels = render_pixels(&mut compositor, &mut scene, &surface);
        let mut glyph_pixels = 0;
        let mut bottom_glyph_pixels = 0;
        for (index, _) in pixels
            .chunks_exact(4)
            .enumerate()
            .filter(|(_, pixel)| pixel[..3].iter().all(|channel| *channel > 160))
        {
            let x = (index % width as usize) as f32;
            let y = (index / width as usize) as f32;
            assert!(
                pane.contains_point(x, y),
                "draft escapes into neighboring UI at {x},{y}"
            );
            assert!(
                x <= pane.x + pane.width - 6.0,
                "draft crosses the right inset"
            );
            glyph_pixels += 1;
            bottom_glyph_pixels += usize::from(y >= pane.y + pane.height - 37.0);
        }
        assert!(glyph_pixels > 0, "visible draft in {pane:?}");
        if let Some(minimum) = bottom_strip_minimum {
            assert!(
                bottom_glyph_pixels > minimum,
                "visible wrapped draft in the bottom strip of {pane:?}"
            );
        }
    }
}

#[test]
fn caret_blink_phase_square_wave() {
    use std::time::Duration;
    let half = CARET_BLINK_HALF_PERIOD;
    for (elapsed, visible) in [
        (Duration::ZERO, true),
        (half / 2, true),
        (half - Duration::from_millis(1), true),
        (half, false),
        (half + half / 2, false),
        (half * 2, true),
        (half * 3, false),
    ] {
        assert_eq!(
            caret_visible_at(elapsed, Some(half)),
            visible,
            "elapsed {elapsed:?}"
        );
        assert!(caret_visible_at(elapsed, None), "blinking disabled");
    }
}

fn composer_scene(
    size: (f32, f32),
    tile_bounds: Rect,
    hit_bounds: Rect,
    background: Rgba,
) -> (SceneGraph, SceneId, SceneId) {
    let root = solid_node(
        Rect::new(0.0, 0.0, tile_bounds.width, tile_bounds.height),
        background,
        None,
    );
    let root_id = root.id;
    let (mut scene, tile) = scene_with_node_at(root, size, tile_bounds);
    let input = leaf(NodeData::HitRegion(HitRegionNode {
        bounds: hit_bounds,
        interaction_id: "composer".into(),
        accepts_focus: true,
        accepts_pointer: true,
        accepts_composer_input: true,
        ..Default::default()
    }));
    let hit = input.id;
    scene.add_node_to_tile(tile, Some(root_id), input).unwrap();
    (scene, tile, hit)
}

/// hud-ljhku.13: splicing a preedit for drawing clamps offsets to char
/// boundaries, defaults the caret to the composition end, suspends the
/// selection, and leaves a state without (or with an empty) preedit as is.
#[test]
fn preedit_splice_clamps_and_defaults() {
    let base = LocalComposerState {
        selection_anchor: 3,
        placeholder: Some("hint".into()),
        ..composer_draft(SceneId::new(), "é!", 1)
    };
    let drawn = base.clone().with_preedit_applied();
    assert_eq!((drawn.text.as_str(), drawn.preedit_range), ("é!", None));
    assert_eq!(drawn.selection_anchor, 3, "no preedit: selection kept");
    let empty = LocalComposerState {
        preedit: Some(ComposerPreedit {
            text: String::new(),
            cursor: None,
        }),
        ..base.clone()
    };
    assert_eq!(empty.with_preedit_applied().preedit_range, None);
    let composing = LocalComposerState {
        preedit: Some(ComposerPreedit {
            text: "日本".to_owned(),
            cursor: Some((4, 4)), // inside '日': snaps back to 3
        }),
        ..base.clone()
    };
    let drawn = composing.with_preedit_applied();
    assert_eq!(drawn.text, "日本é!");
    assert_eq!(drawn.preedit_range, Some(0..6));
    assert_eq!((drawn.cursor_byte, drawn.selection_anchor), (3, 3));
    assert!(drawn.preedit.is_none());
    let no_cursor = LocalComposerState {
        cursor_byte: 3,
        preedit: Some(ComposerPreedit {
            text: "x".to_owned(),
            cursor: None,
        }),
        ..base
    };
    let drawn = no_cursor.with_preedit_applied();
    assert_eq!((drawn.text.as_str(), drawn.cursor_byte), ("é!x", 4));
}

#[test]
fn horizontal_caret_follow_keeps_the_caret_visible_and_reveals_the_tail() {
    let margin = 6.0;
    for (caret, draft, width, inset, expected) in [
        (0.0, 50.0, 100.0, margin, 0.0),
        (50.0, 50.0, 100.0, margin, 0.0),
        (100.0, 100.0, 100.0, margin, 0.0),
        (150.0, 150.0, 100.0, margin, 56.0),
        (0.0, 500.0, 100.0, margin, 0.0),
        (300.0, 500.0, 100.0, margin, 206.0),
        (500.0, 500.0, 100.0, margin, 406.0),
        (120.0, 120.0, 100.0, margin, 26.0),
        (80.0, 80.0, 100.0, margin, 0.0),
        (50.0, 500.0, 0.0, margin, 0.0),
        (50.0, 500.0, -10.0, margin, 0.0),
        (100.0, 100.0, 8.0, 100.0, 96.0),
    ] {
        assert_eq!(
            composer_scroll_offset(caret, draft, width, inset),
            expected,
            "caret={caret}, draft={draft}, width={width}, margin={inset}",
        );
    }

    for draft in [0.0_f32, 50.0, 100.0, 250.0, 500.0, 1000.0] {
        let mut previous = f32::INFINITY;
        for step in (0..=25).rev() {
            let caret = draft * step as f32 / 25.0;
            let offset = composer_scroll_offset(caret, draft, 100.0, margin);
            assert!((0.0..=100.0).contains(&(caret - offset)));
            assert!((0.0..=(draft + margin - 100.0).max(0.0)).contains(&offset));
            assert!(offset <= previous, "moving left must reveal earlier text");
            previous = offset;
        }
        assert_eq!(previous, 0.0, "moving home reveals the start");
    }
}

const NX_LH_MULT: f32 = 1.4;

#[test]
fn wrapped_composer_grows_then_scrolls_within_its_available_rows() {
    for (total, capacity, expected) in [
        (0, 6, 1),
        (1, 6, 1),
        (3, 6, 3),
        (6, 6, 6),
        (10, 6, 6),
        (5, 1, 1),
    ] {
        assert_eq!(composer_visible_line_count(total, capacity), expected);
    }
    for (caret, total, capacity, expected) in [(3, 4, 6, 0), (0, 10, 6, 0), (9, 10, 6, 4)] {
        assert_eq!(
            composer_vertical_line_offset(caret, total, capacity),
            expected
        );
    }
    for (total, capacity) in [(1_usize, 6), (4, 6), (6, 6), (7, 6), (12, 5), (20, 6)] {
        let mut previous = usize::MAX;
        for caret in (0..total).rev() {
            let first = composer_vertical_line_offset(caret, total, capacity);
            assert!((first..first + capacity).contains(&caret));
            assert!(first <= total.saturating_sub(capacity));
            assert!(first <= previous, "moving up must reveal earlier rows");
            previous = first;
        }
        assert_eq!(previous, 0);
    }
}

/// `composer_input_box` grows UPWARD from the bottom edge as `visible_lines`
/// increases, and `visible_lines == 1` reproduces the single-line strip exactly.
#[test]
fn composer_input_box_grows_from_its_anchor_and_clamps_to_the_pane() {
    use ComposerVerticalAnchor::{Bottom, Top};
    let region = Rect::new(10.0, 100.0, 600.0, 300.0);
    for anchor in [Bottom, Top] {
        for rows in [1.0, 3.0, 40.0] {
            let height = (16.0 * NX_LH_MULT * rows + 12.0).min(region.height);
            let y = if anchor == Top {
                region.y
            } else {
                region.y + region.height - height
            };
            let actual =
                Compositor::composer_input_box(region, 16.0, NX_LH_MULT, rows, anchor, 6.0);
            assert_rect_close(
                actual,
                Rect::new(region.x, y, region.width, height),
                "input box anchor",
            );
        }
    }
    let short = Rect::new(0.0, 0.0, 400.0, 50.0);
    assert_eq!(
        Compositor::composer_input_box(short, 16.0, NX_LH_MULT, 20.0, Bottom, 6.0),
        short
    );
    assert_eq!(
        resolve_composer_overlay_tokens(&HashMap::new()).anchor,
        Bottom
    );
    for (token, expected) in [("top", Top), ("  TOP ", Top), ("sideways", Bottom)] {
        let map = token_overrides([("portal.composer.anchor", token)]);
        assert_eq!(resolve_composer_overlay_tokens(&map).anchor, expected);
    }
}

/// the non-composer tile-render spacing literals (code-panel backdrop
/// margins, glyphon-unavailable text fallback inset, unregistered-image
/// placeholder margin) resolve from their own `portal.spacing.*` tokens. Defaults
/// MUST equal the historical inline literals (8.0 / 4.0 / 2.0 / 4.0) so the
/// default profile is visually unchanged; overrides propagate; malformed values
/// fall back. These are compositor-local (the exemplar never renders these
/// surfaces), so — unlike `portal.spacing.content_inset_px` — they are resolved
/// here rather than through the config-crate handshake `PortalPartTokens`.
#[test]
fn tile_spacing_tokens_resolve_default_override_and_reject() {
    use crate::renderer::token_colors::resolve_tile_spacing_tokens;
    use std::collections::HashMap;

    let fields = |map: &HashMap<String, String>| {
        let t = resolve_tile_spacing_tokens(map);
        [
            t.transcript_fallback_inset_px,
            t.code_panel_margin_x_px,
            t.code_panel_pad_y_px,
            t.image_margin_px,
        ]
    };
    assert_eq!(fields(&HashMap::new()), [8.0, 4.0, 2.0, 4.0]);
    let overrides = [
        ("transcript_fallback_inset_px", "12"),
        ("code_panel_margin_x_px", "7"),
        ("code_panel_pad_y_px", "3.5"),
        ("image_margin_px", "9"),
    ]
    .into_iter()
    .map(|(key, value)| (format!("portal.spacing.{key}"), value.into()))
    .collect();
    assert_eq!(fields(&overrides), [12.0, 7.0, 3.5, 9.0]);
    let flush = token_overrides([("portal.spacing.code_panel_margin_x_px", "0")]);
    assert_eq!(fields(&flush), [8.0, 0.0, 2.0, 4.0]);
    for bad in ["not-a-number", "-4", "NaN", "inf", ""] {
        let map = token_overrides([("portal.spacing.image_margin_px", bad)]);
        assert_eq!(fields(&map), [8.0, 4.0, 2.0, 4.0], "{bad:?}");
    }
}

#[test]
fn composer_capacity_limits_wrapped_caret_to_the_visible_box() {
    let line_height = 16.0 * NX_LH_MULT;
    for (height, pitch, visible) in [
        (60.0, line_height, 2),
        (300.0, line_height, 12),
        (20.0, line_height, 1),
        (60.0, 0.0, 1),
    ] {
        assert_eq!(
            image_cache::composer_region_fit_lines(height, pitch, 6.0),
            visible
        );
    }
    // A six-row token must still keep a three-line draft inside a two-row pane.
    let capacity = 6.min(image_cache::composer_region_fit_lines(
        60.0,
        line_height,
        6.0,
    ));
    let (lines, caret) = (3, 2);
    let token_only = composer_vertical_line_offset(caret, lines, 6);
    assert!(caret - token_only >= capacity);
    let first = composer_vertical_line_offset(caret, lines, capacity);
    let visible = composer_visible_line_count(lines, capacity);
    assert_eq!((first, visible), (1, 2));
}

#[test]
fn composer_line_limit_defaults_rejects_zero_and_accepts_overrides() {
    assert_eq!(
        resolve_composer_overlay_tokens(&HashMap::new()).max_lines,
        6
    );
    for (value, lines) in [("3", 3), ("1", 1), ("0", 6)] {
        let tokens = token_overrides([("portal.composer.max_lines", value)]);
        assert_eq!(resolve_composer_overlay_tokens(&tokens).max_lines, lines);
    }
}

#[test]
fn composer_content_inset_token_drives_box_geometry() {
    let region = Rect::new(0.0, 0.0, 600.0, 1000.0);
    let font = 16.0;
    let multiplier = crate::markdown::MarkdownTokens::default().line_height_multiplier;
    for (value, expected) in [
        (None, 6.0),
        (Some("12"), 12.0),
        (Some("0"), 0.0),
        (Some("-4"), 6.0),
        (Some("NaN"), 6.0),
        (Some("inf"), 6.0),
        (Some("wat"), 6.0),
        (Some(""), 6.0),
    ] {
        let tokens = value
            .into_iter()
            .map(|value| ("portal.spacing.content_inset_px".into(), value.into()))
            .collect();
        let inset = resolve_composer_overlay_tokens(&tokens).content_inset_px;
        assert_eq!(inset, expected, "{value:?}");
        let field = Compositor::composer_input_box(
            region,
            font,
            multiplier,
            1.0,
            ComposerVerticalAnchor::Bottom,
            inset,
        );
        assert!((field.height - (font * multiplier + 2.0 * expected)).abs() < 0.01);
    }
}

/// The default `ComposerLayout` is the inert single-line profile, so a frame with
/// no active composer (or an unmeasured one) never wraps or scrolls — preserving
/// the single-line behavior exactly.
#[test]
fn multiline_default_layout_is_single_line() {
    let d = ComposerLayout::default();
    assert!(!d.wrap, "default profile is single-line");
    assert_eq!(d.h_scroll_px, 0.0);
    assert_eq!(d.vscroll_px, 0.0);
    assert_eq!(d.visible_lines, 1.0, "default box is one line tall");
    assert_eq!(d.total_lines, 1.0);
}

#[test]
fn separator_rects_span_the_break_rows() {
    use tile_render::transcript_separator_rects;
    let rects = transcript_separator_rects("A\n\nB\n\nC", &[2, 5], 100.0, 50.0, 300.0, 20.0, 2.0);
    assert_eq!(
        rects,
        [
            Rect::new(100.0, 79.0, 300.0, 2.0),
            Rect::new(100.0, 119.0, 300.0, 2.0)
        ]
    );
    for (breaks, width, thickness) in [
        (&[][..], 300.0, 1.0),
        (&[2][..], 0.0, 1.0),
        (&[2][..], 300.0, 0.0),
    ] {
        assert!(
            transcript_separator_rects("a\n\nb", breaks, 0.0, 0.0, width, 20.0, thickness)
                .is_empty()
        );
    }
}

#[tokio::test]
async fn transcript_divider_rects_track_tile_scroll_offset() {
    let (mut compositor, _surface) = require_gpu!(make_text_compositor(400, 400).await);
    assert!(
        compositor.text_rasterizer.is_some(),
        "test needs the separator draw path"
    );
    compositor.set_token_map(token_overrides([
        ("portal.divider.color", "#FF0000"),
        ("portal.divider.thickness_px", "2"),
    ]));
    let node = SceneId::new();
    let (mut scene, tile) = scene_with_node_at(
        Node {
            layout: Default::default(),
            id: node,
            children: vec![],
            data: NodeData::TextMarkdown(TextMarkdownNode {
                font_family: FontFamily::SystemMonospace,
                ..markdown_text(
                    "Entry A\n\n---\n\nEntry B",
                    Rect::new(0.0, 0.0, 300.0, 1200.0),
                    16.0,
                )
            }),
        },
        (400.0, 400.0),
        Rect::new(20.0, 30.0, 300.0, 300.0),
    );
    scene
        .register_tile_scroll_config(
            tile,
            TileScrollConfig {
                content_height: Some(1200.0),
                ..TileScrollConfig::vertical()
            },
        )
        .unwrap();
    compositor.prime_markdown_cache(&scene);
    let divider_top = |scene: &SceneGraph| {
        let mut vertices = Vec::new();
        compositor.render_node(
            node,
            &scene.tiles[&tile],
            scene,
            &mut vertices,
            &mut Vec::new(),
            400.0,
            400.0,
        );
        let top = vertices.iter().map(|v| v.position[1]).reduce(f32::max)?;
        Some((1.0 - top) * 200.0)
    };
    let base = divider_top(&scene).unwrap();
    let scroll = 12.0;
    scene
        .set_tile_scroll_offset_local(tile, 0.0, scroll)
        .unwrap();
    assert!((base - divider_top(&scene).unwrap() - scroll).abs() < 0.5);
    scene
        .set_tile_scroll_offset_local(tile, 0.0, 400.0)
        .unwrap();
    assert_eq!(
        divider_top(&scene),
        None,
        "the pane clips offscreen dividers"
    );
    scene.set_tile_scroll_offset_local(tile, 0.0, 0.0).unwrap();
    scene.set_tile_font_scale(tile, 1.5);
    assert!(
        divider_top(&scene).unwrap() > base + 5.0,
        "divider follows scaled text pitch"
    );
}

/// The first resized frame stages composer fill and text from one fresh wrapped layout.
#[tokio::test]
async fn repro_resize_first_frame_keeps_composer_chrome_aligned_with_wrapped_text() {
    let (mut compositor, _surface) = require_gpu!(make_text_compositor(400, 300).await);

    const SURFACE_H: f32 = 500.0;
    let region = Rect::new(0.0, 0.0, 400.0, SURFACE_H);
    let (mut scene, tile_id, composer_id) =
        composer_scene((400.0, SURFACE_H), region, region, Rgba::BLACK);
    compositor
        .token_map
        .insert("portal.composer.max_lines".to_owned(), "8".to_owned());

    let draft = "resize reflow ".repeat(21);
    let text = |compositor: &Compositor, scene: &SceneGraph| {
        compositor
            .collect_text_items(scene, 400.0, SURFACE_H)
            .into_iter()
            .find(|item| item.text.as_ref() == draft)
            .unwrap()
    };
    compositor.local_composer = Some(composer_draft(composer_id, &draft, draft.len()));
    compositor.prime_composer_scroll_offset(&scene);
    let old_layout = compositor.composer_layout;
    let old_text = text(&compositor, &scene);
    let tokens = resolve_composer_overlay_tokens(&compositor.token_map);
    assert_eq!(old_text.font_size_px, tokens.font_size_px);
    assert_eq!(old_layout.visible_lines, 6.0);

    const RESIZED_SCALE: f32 = 1.5;
    scene.set_tile_font_scale(tile_id, RESIZED_SCALE);
    prime_text(&mut compositor, &scene);
    let frame = compositor.build_windowed_frame(&mut scene, 400, SURFACE_H as u32);
    let fresh_layout = compositor.composer_layout;
    assert_eq!(fresh_layout.visible_lines, 8.0);
    assert!(fresh_layout.total_lines > old_layout.total_lines);
    let expected_box = Compositor::composer_input_box(
        region,
        tokens.font_size_px * RESIZED_SCALE,
        crate::markdown::MarkdownTokens::default().line_height_multiplier,
        fresh_layout.visible_lines,
        tokens.anchor,
        tokens.content_inset_px,
    );

    let fill = [tokens.bg_r, tokens.bg_g, tokens.bg_b, tokens.bg_a];
    let vertices = frame
        .flat_rect_vertices()
        .chunks_exact(6)
        .find(|rect| rect.iter().all(|v| v.color == fill))
        .expect("first-frame composer fill must be staged");
    assert_rect_close(
        rect_vertex_bounds(vertices, 400.0, SURFACE_H),
        expected_box,
        "first-frame composer fill uses the resized layout",
    );
    let new_text = text(&compositor, &scene);
    assert!((new_text.font_size_px - old_text.font_size_px * RESIZED_SCALE).abs() < 0.01);
    assert!(new_text.bounds_height > old_text.bounds_height);
    assert!((new_text.clip_pixel_y - expected_box.y).abs() < 0.5);
}

fn token_overrides<const N: usize>(pairs: [(&str, &str); N]) -> HashMap<String, String> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
}

fn composer_draft(node_id: SceneId, text: &str, cursor_byte: usize) -> LocalComposerState {
    LocalComposerState {
        text: text.into(),
        cursor_byte,
        selection_anchor: cursor_byte,
        at_capacity: false,
        node_id,
        placeholder: None,
        preedit: None,
        preedit_range: None,
    }
}

fn leaf(data: NodeData) -> Node {
    Node {
        id: SceneId::new(),
        children: Vec::new(),
        layout: Default::default(),
        data,
    }
}

fn solid_node(bounds: Rect, color: Rgba, radius: Option<f32>) -> Node {
    leaf(NodeData::SolidColor(SolidColorNode {
        bounds,
        color,
        radius,
    }))
}

fn markdown_text(content: impl Into<String>, bounds: Rect, font_size_px: f32) -> TextMarkdownNode {
    TextMarkdownNode {
        content: content.into(),
        bounds,
        font_size_px,
        font_family: FontFamily::SystemSansSerif,
        color: Rgba::WHITE,
        background: None,
        alignment: TextAlign::Start,
        overflow: TextOverflow::Clip,
        color_runs: Box::default(),
    }
}

fn scene_with_node_at(node: Node, size: (f32, f32), bounds: Rect) -> (SceneGraph, SceneId) {
    let mut scene = SceneGraph::new(size.0, size.1);
    let tab = scene.create_tab("test", 0).unwrap();
    let lease = scene.grant_lease("test", 60_000);
    let tile = scene.create_tile(tab, "test", lease, bounds, 1).unwrap();
    scene.set_tile_root(tile, node).unwrap();
    (scene, tile)
}

fn rect_vertex_bounds(vertices: &[crate::pipeline::RectVertex], sw: f32, sh: f32) -> Rect {
    assert_eq!(vertices.len(), 6);
    let [left, top, right, bottom] = vertices.iter().fold(
        [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ],
        |[left, top, right, bottom], v| {
            let x = (v.position[0] + 1.0) * sw * 0.5;
            let y = (1.0 - v.position[1]) * sh * 0.5;
            [left.min(x), top.min(y), right.max(x), bottom.max(y)]
        },
    );
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

async fn make_text_compositor(width: u32, height: u32) -> Option<(Compositor, HeadlessSurface)> {
    let (mut compositor, surface) = make_compositor_and_surface(width, height).await?;
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    Some((compositor, surface))
}

fn prime_text(compositor: &mut Compositor, scene: &SceneGraph) {
    compositor.prime_markdown_cache(scene);
    compositor.prime_truncation_cache(scene);
}

fn render_pixels(
    compositor: &mut Compositor,
    scene: &mut SceneGraph,
    surface: &HeadlessSurface,
) -> Vec<u8> {
    compositor.render_frame_headless(scene, surface);
    surface.read_pixels(&compositor.device)
}
