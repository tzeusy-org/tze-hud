use super::*;

// ── hud-r3ax6: composer echo local render tests ───────────────────────────

/// `LocalComposerStateHandle` slot semantics:
///
/// - `None`             → no pending update; `local_composer` unchanged.
/// - `Some(None)`       → explicit deactivation; clears `local_composer`.
/// - `Some(Some(state))`→ new draft state; replaces `local_composer`.
///
/// Drives the real `apply_composer_slot` free function (production code path)
/// without requiring a GPU-backed `Compositor` instance.
#[test]
fn local_composer_state_handle_slot_semantics() {
    use tze_hud_scene::types::SceneId;

    let handle: LocalComposerStateHandle = std::sync::Arc::new(std::sync::Mutex::new(None));

    let node_id = SceneId::new();
    let mut local_composer: Option<LocalComposerState> = None;

    // 1. Slot = None → no change.
    apply_composer_slot(&handle, &mut local_composer);
    assert!(
        local_composer.is_none(),
        "None slot must leave local_composer unchanged"
    );

    // 2. Slot = Some(Some(state)) → activate.
    {
        let mut guard = handle.lock().unwrap();
        *guard = Some(Some(LocalComposerState {
            text: "hello".to_owned(),
            cursor_byte: 5,
            selection_anchor: 5, // no selection
            at_capacity: false,
            node_id,
            placeholder: None,
        }));
    }
    apply_composer_slot(&handle, &mut local_composer);
    let cs = local_composer
        .as_ref()
        .expect("local_composer must be set after Some(Some)");
    assert_eq!(cs.text, "hello", "text must match pushed draft");
    assert_eq!(cs.cursor_byte, 5, "cursor_byte must match pushed draft");
    assert!(!cs.at_capacity, "at_capacity must match pushed draft");
    assert_eq!(cs.node_id, node_id, "node_id must match pushed draft");

    // Slot is taken → drained to None.
    {
        let guard = handle.lock().unwrap();
        assert!(guard.is_none(), "slot must be cleared after drain");
    }

    // 3. Second drain with None slot → local_composer UNCHANGED (still active).
    apply_composer_slot(&handle, &mut local_composer);
    assert!(
        local_composer.is_some(),
        "None slot must not clear a previously set local_composer"
    );

    // 4. Slot = Some(None) → deactivate.
    {
        let mut guard = handle.lock().unwrap();
        *guard = Some(None);
    }
    apply_composer_slot(&handle, &mut local_composer);
    assert!(
        local_composer.is_none(),
        "Some(None) slot must clear local_composer (deactivation)"
    );
}

/// Local composer echo must render inside the focused composer HitRegion, not
/// as a generic strip at the bottom of the containing tile.
#[tokio::test]
async fn local_composer_text_item_uses_hit_region_bounds() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(320.0, 200.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(20.0, 30.0, 200.0, 120.0),
            1,
        )
        .unwrap();
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 0.0),
                    bounds: Rect::new(0.0, 0.0, 200.0, 120.0),
                    radius: None,
                }),
            },
        )
        .unwrap();

    let hit_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: hit_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(12.0, 16.0, 140.0, 72.0),
                    interaction_id: "composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    compositor.local_composer = Some(LocalComposerState {
        text: "hello".to_owned(),
        cursor_byte: 5,
        selection_anchor: 5,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });

    let tokens = resolve_composer_overlay_tokens(&std::collections::HashMap::new());
    let tile = scene.tiles.get(&tile_id).unwrap();
    let item = compositor
        .collect_composer_text_item(tile, &scene, 320.0, 200.0, &tokens)
        .expect("focused composer HitRegion must produce a text item");

    // The composer echo is confined to a single input-line strip pinned to the
    // BOTTOM of the composer region (hud-2zsbf): the full HitRegion can span the
    // whole portal (click-anywhere-to-focus), so the rendered draft must not
    // stretch across it. Strip height = font_line_height + 2*margin
    // = 16*1.4 + 12 = 34.4; strip_y = region.y + (region.height - strip_height)
    // = 46 + (72 - 34.4) = 83.6.
    let strip_height = 16.0 * crate::text::LINE_HEIGHT_MULTIPLIER + 12.0;
    let strip_y = 46.0 + (72.0 - strip_height);
    assert_eq!(
        item.pixel_x, 38.0,
        "text x must anchor to hit-region x + margin (horizontal unchanged)"
    );
    assert_eq!(
        item.pixel_y,
        strip_y + 6.0,
        "text y must anchor to the bottom input strip top + margin"
    );
    assert_eq!(
        item.clip_pixel_y, strip_y,
        "clip y must use the input-strip top (bottom of region), not the region top"
    );
    assert_eq!(
        item.bounds_width, 128.0,
        "text bounds width must be the hit-region width minus horizontal margins"
    );
    assert_eq!(
        item.bounds_height,
        strip_height - 12.0,
        "text bounds height must be one input line (strip height minus vertical margins)"
    );
    assert_eq!(
        item.clip_bounds_height, strip_height,
        "clip height must be one input-line strip, not the full region height"
    );
}

/// hud-evk0j: an EMPTY composer draft with a placeholder hint renders that hint
/// dimmed from `portal.composer.placeholder_color`, and the placeholder vanishes
/// the instant the draft is non-empty (or the composer carries no placeholder).
///
/// Asserts the three contract points: (1) empty draft + placeholder → the item
/// text is the placeholder, colored `placeholder_color`, with no caret/selection
/// styled runs; (2) a non-empty draft suppresses the placeholder even when one is
/// present (it is not treated as draft text and disappears on the first keystroke);
/// (3) an empty draft with NO placeholder is unchanged (never the placeholder
/// color).
#[tokio::test]
async fn composer_placeholder_renders_only_when_draft_empty() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(320.0, 200.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(20.0, 30.0, 200.0, 120.0),
            1,
        )
        .unwrap();
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 0.0),
                    bounds: Rect::new(0.0, 0.0, 200.0, 120.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    let hit_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: hit_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(12.0, 16.0, 140.0, 72.0),
                    interaction_id: "composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    let tokens = resolve_composer_overlay_tokens(&std::collections::HashMap::new());
    // Default placeholder color is the dimmed slate #6B7689 (config-crate default).
    assert_eq!(
        tokens.placeholder_color,
        [0x6B, 0x76, 0x89, 0xFF],
        "default placeholder color must resolve to the dimmed slate token default"
    );

    let placeholder = "Type a message…";

    // ── 1. Empty draft + placeholder → dimmed placeholder run, no caret. ──
    compositor.local_composer = Some(LocalComposerState {
        text: String::new(),
        cursor_byte: 0,
        selection_anchor: 0,
        at_capacity: false,
        node_id: hit_id,
        placeholder: Some(placeholder.to_owned()),
    });
    let tile = scene.tiles.get(&tile_id).unwrap();
    let item = compositor
        .collect_composer_text_item(tile, &scene, 320.0, 200.0, &tokens)
        .expect("empty focused composer with a placeholder must still produce a text item");
    assert_eq!(
        item.text.as_ref(),
        placeholder,
        "empty draft must render the placeholder string, not the caret glyph"
    );
    assert_eq!(
        item.color, tokens.placeholder_color,
        "placeholder text must be colored from portal.composer.placeholder_color"
    );
    assert!(
        item.styled_runs.is_empty(),
        "placeholder is a static hint: no caret or selection styled runs"
    );

    // ── 2. Non-empty draft suppresses the placeholder (disappears on typing). ──
    compositor.local_composer = Some(LocalComposerState {
        text: "hi".to_owned(),
        cursor_byte: 2,
        selection_anchor: 2,
        at_capacity: false,
        node_id: hit_id,
        placeholder: Some(placeholder.to_owned()),
    });
    let tile = scene.tiles.get(&tile_id).unwrap();
    let typed = compositor
        .collect_composer_text_item(tile, &scene, 320.0, 200.0, &tokens)
        .expect("non-empty composer must produce a text item");
    assert!(
        typed.text.contains("hi"),
        "non-empty draft must render the live draft text ({:?})",
        typed.text
    );
    assert_ne!(
        typed.text.as_ref(),
        placeholder,
        "a non-empty draft must NOT render the placeholder"
    );
    assert_ne!(
        typed.color, tokens.placeholder_color,
        "live draft text must use the composer text color, not the placeholder color"
    );

    // ── 3. Empty draft with NO placeholder is unchanged (never dimmed). ──
    compositor.local_composer = Some(LocalComposerState {
        text: String::new(),
        cursor_byte: 0,
        selection_anchor: 0,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });
    let tile = scene.tiles.get(&tile_id).unwrap();
    let no_hint = compositor
        .collect_composer_text_item(tile, &scene, 320.0, 200.0, &tokens)
        .expect("empty composer without a placeholder must still produce a text item");
    assert_ne!(
        no_hint.text.as_ref(),
        placeholder,
        "no placeholder configured → the placeholder string must never appear"
    );
    assert_ne!(
        no_hint.color, tokens.placeholder_color,
        "no placeholder configured → text must not use the placeholder color"
    );
}

/// Regression (hud-2zsbf + hud-n0x4u): mirror the resident portal — a FULL-TILE
/// composer HitRegion (as `resident_grpc::render_batch` publishes via
/// `local_bounds_for_state`) with a long unbreakable draft. The draft MUST NOT
/// "extend forever" horizontally past the region's right edge, and MUST be
/// bottom-anchored (caret line in the bottom input strip), not laid as a
/// full-width line across the portal TOP (the live hud-2zsbf P1).
///
/// Under break-anywhere wrap (hud-n0x4u) the 200-char token no longer stays one
/// clipped line — it wraps at the glyph level into a bottom-anchored multi-line
/// box so every character stays visible. This test therefore guards the
/// horizontal no-overflow + bottom-anchoring invariants, not a single-line
/// layout.
///
/// Transcript body is rendered dim so only the (bright) composer draft registers.
#[tokio::test]
async fn composer_echo_confined_to_bottom_strip_full_tile_hitregion() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(600, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(600.0, 300.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 600.0, 300.0),
            1,
        )
        .unwrap();
    let root_id = SceneId::new();
    // Portal body: an opaque dark backdrop (no transcript text) so the only
    // bright pixels are the composer echo glyphs.
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.02, 0.02, 0.02, 1.0),
                    bounds: Rect::new(0.0, 0.0, 600.0, 300.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    // Composer HitRegion == full tile (matches resident_grpc local_bounds_for_state).
    let hit_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: hit_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 600.0, 300.0),
                    interaction_id: "composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    let draft = "M".repeat(200);
    let draft_len = draft.len();
    compositor.local_composer = Some(LocalComposerState {
        text: draft,
        cursor_byte: draft_len,
        selection_anchor: draft_len,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    let is_bright = |o: usize| pixels[o] > 160 && pixels[o + 1] > 160 && pixels[o + 2] > 160;
    let (mut minx, mut miny, mut maxx, mut maxy) = (600usize, 300usize, 0usize, 0usize);
    let mut count = 0usize;
    for row in 0..300usize {
        for col in 0..600usize {
            if is_bright((row * 600 + col) * 4) {
                count += 1;
                minx = minx.min(col);
                maxx = maxx.max(col);
                miny = miny.min(row);
                maxy = maxy.max(row);
            }
        }
    }
    assert!(count > 0, "composer echo must render some glyphs");
    let _ = minx; // bbox min-x unused; the horizontal guard is on maxx

    // Input strip: line_height + 2*margin = 16*1.4 + 12 = 34.4, pinned to the
    // bottom of the 300px-tall region → strip_top ≈ 265.6.
    let strip_height = 16.0 * crate::text::LINE_HEIGHT_MULTIPLIER + 12.0;
    let strip_top = (300.0 - strip_height) as usize; // ≈ 265

    // The core hud-2zsbf P1 was HORIZONTAL: the draft "extended forever" as a
    // full-width unwrapped line spilling past the region's right edge. Under
    // break-anywhere wrap (hud-n0x4u) an unbreakable 200-char token is no longer
    // one over-long clipped line — it wraps at the glyph level into a bottom-
    // anchored multi-line box so every character stays visible — but the
    // horizontal clip must STILL hold: nothing past the region interior right
    // edge (600 - COMPOSER_TEXT_MARGIN(6) = 594).
    assert!(
        maxx <= 594,
        "composer draft overflowed horizontally to x={maxx} (region interior right = 594); \
         break-anywhere wrap must keep every line inside the box, never 'extend forever'"
    );

    // Bottom-anchored: the composer box is pinned to the BOTTOM of the portal and
    // grows UPWARD as the draft wraps, so the newest/caret line rides in the
    // bottom input strip. The live P1 laid the draft at the PORTAL TOP instead;
    // here the draft must reach down into the bottom strip.
    assert!(
        maxy >= strip_top,
        "composer draft is not bottom-anchored: maxy={maxy} never reaches the input \
         strip (strip_top≈{strip_top}); the caret line must ride at the bottom, \
         not float at the portal top"
    );

    // The bottom input strip carries the newest wrapped line's glyphs.
    let strip_bright = (strip_top.saturating_sub(2)..300usize)
        .flat_map(|r| (0..600usize).map(move |c| (r, c)))
        .filter(|(r, c)| is_bright((r * 600 + c) * 4))
        .count();
    assert!(
        strip_bright > 1000,
        "composer draft not rendered in the bottom input strip (strip_bright={strip_bright})"
    );
}

/// Repro (hud-nottc): a WRAPPED multi-line draft in a SHORT composer pane (the
/// exemplar's top input strip) must keep its glyphs — including the caret on the
/// last visual line — CONFINED to the composer region, not clipped away or laid
/// outside it. The live P1 was the blinking caret showing at the portal's
/// top-left when a long draft wrapped in a short input pane: the multi-line
/// growth/scroll used the `max_lines` token instead of what the pane fits, so the
/// caret line fell outside the box. This renders through the full headless GPU
/// pipeline and asserts the composer glyphs sit within the pane rect.
#[tokio::test]
async fn composer_wrapped_draft_stays_in_short_pane_headless() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(600, 400).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(600.0, 400.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 600.0, 400.0),
            1,
        )
        .unwrap();
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.02, 0.02, 0.02, 1.0),
                    bounds: Rect::new(0.0, 0.0, 600.0, 400.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    // SHORT composer input pane at the TOP-LEFT of the tile (exemplar-style,
    // ~2 text lines tall). local bounds are tile-relative.
    const PANE_H: f32 = 60.0;
    let hit_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: hit_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 400.0, PANE_H),
                    interaction_id: "composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    // A long draft with spaces so it word-wraps to many visual lines in the
    // 400px-wide pane; caret at the end (typing).
    let draft = "word ".repeat(40); // ~200 chars → wraps well past the 2-line pane
    let draft_len = draft.len();
    compositor.local_composer = Some(LocalComposerState {
        text: draft,
        cursor_byte: draft_len,
        selection_anchor: draft_len,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    let is_bright = |o: usize| pixels[o] > 160 && pixels[o + 1] > 160 && pixels[o + 2] > 160;
    let (mut miny, mut maxy, mut count) = (400usize, 0usize, 0usize);
    for row in 0..400usize {
        for col in 0..600usize {
            if is_bright((row * 600 + col) * 4) {
                count += 1;
                miny = miny.min(row);
                maxy = maxy.max(row);
            }
        }
    }
    // The caret + draft must render (not clipped entirely away).
    assert!(
        count > 0,
        "composer draft/caret must render some glyphs in the pane"
    );
    // All composer glyphs stay within the input pane rect (top-anchored, 60px).
    // A small tolerance covers glyph descenders / anti-aliasing at the edge.
    assert!(
        maxy <= (PANE_H as usize) + 3,
        "composer glyphs rendered below the input pane (maxy={maxy} > {PANE_H}); \
         a wrapped draft must stay within its box, not overflow"
    );
    assert!(
        miny <= PANE_H as usize,
        "composer glyphs must be inside the pane, got miny={miny}"
    );
}

/// Repro (hud-2zsbf): a composer draft wider than the box must be CLIPPED to the
/// composer interior — no glyph pixels may appear to the RIGHT of the box edge.
///
/// This renders through the full headless GPU pipeline (the same
/// `collect_composer_text_item` → glyphon `TextBounds` path the live overlay
/// uses) with an overflowing single-line draft and asserts the region to the
/// right of the composer interior stays background-dark.  The live P1 was that
/// the single unwrapped line "extends forever" past the box.
#[tokio::test]
async fn composer_draft_overflow_is_clipped_to_box_headless() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(400, 160).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(400.0, 160.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 160.0),
            1,
        )
        .unwrap();
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    // Opaque dark backdrop so overflow glyphs (bright) stand out.
                    color: Rgba::new(0.02, 0.02, 0.02, 1.0),
                    bounds: Rect::new(0.0, 0.0, 400.0, 160.0),
                    radius: None,
                }),
            },
        )
        .unwrap();

    // Composer HitRegion: a narrow input strip at local (20, 60) sized 120x40.
    // Region interior (clip) right edge = 20 + 120 - COMPOSER_TEXT_MARGIN(6) = 134.
    let hit_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: hit_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(20.0, 60.0, 120.0, 40.0),
                    interaction_id: "composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    // A draft far wider than the 120px strip, cursor at end (caret pinned right).
    let draft = "M".repeat(60);
    let draft_len = draft.len();
    compositor.local_composer = Some(LocalComposerState {
        text: draft,
        cursor_byte: draft_len,
        selection_anchor: draft_len,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    let px = |row: usize, col: usize| -> [u8; 4] {
        let o = (row * 400 + col) * 4;
        [pixels[o], pixels[o + 1], pixels[o + 2], pixels[o + 3]]
    };
    let is_bright = |p: [u8; 4]| p[0] > 160 && p[1] > 160 && p[2] > 160;

    // Sanity: SOME bright glyph pixel must appear INSIDE the box interior
    // (x in [26, 134)) across the composer text band (y in [60, 100)).
    let mut bright_inside = false;
    for row in 60..100usize {
        for col in 26..134usize {
            if is_bright(px(row, col)) {
                bright_inside = true;
                break;
            }
        }
        if bright_inside {
            break;
        }
    }
    assert!(
        bright_inside,
        "expected composer draft glyphs to render inside the box interior"
    );

    // Defect assertion: NO bright glyph pixel may appear to the RIGHT of the box
    // interior (x >= 140, a few px past the 134 clip edge to avoid AA fringe)
    // within the composer text band.
    let mut overflow_col: Option<usize> = None;
    'outer: for row in 60..100usize {
        for col in 140..400usize {
            if is_bright(px(row, col)) {
                overflow_col = Some(col);
                break 'outer;
            }
        }
    }
    assert!(
        overflow_col.is_none(),
        "composer draft overflowed the box: bright glyph pixel at col {overflow_col:?} \
         (clip interior right edge is x=134)"
    );
}

/// Pure blink-phase logic: `elapsed → caret-visible` must produce a square wave
/// with period `2 * CARET_BLINK_HALF_PERIOD`, solid in the first half-period so
/// the caret is solid immediately after a reset (keystroke / caret move).
#[test]
fn caret_blink_phase_square_wave() {
    use std::time::Duration;
    let half = CARET_BLINK_HALF_PERIOD;

    // Phase 0 (solid) — including exactly at reset.
    assert!(
        caret_visible_at(Duration::ZERO),
        "solid immediately after reset"
    );
    assert!(caret_visible_at(half / 2), "solid mid first half-period");
    assert!(
        caret_visible_at(half - Duration::from_millis(1)),
        "solid just before first toggle"
    );

    // Phase 1 (hidden).
    assert!(!caret_visible_at(half), "hidden at first toggle boundary");
    assert!(
        !caret_visible_at(half + half / 2),
        "hidden mid second half-period"
    );

    // Phase 2 (solid again) — wave repeats.
    assert!(
        caret_visible_at(half * 2),
        "solid again after one full period"
    );
    assert!(!caret_visible_at(half * 3), "hidden in fourth half-period");
}

/// Build a minimal scene (one tile, one composer HitRegion) matching the
/// geometry used by `composer_echo_confined_to_bottom_strip_full_tile_hitregion`
/// et al: tile at `(20, 30, 200, 120)`, HitRegion at `(12, 16, 140, 72)` — so
/// `region.x == 38` and `region.y == 46` with the default `content_inset_px`
/// (6.0), giving deterministic expected caret pixel coordinates across the
/// hud-hxhnt regression tests below.
fn composer_caret_test_scene() -> (SceneGraph, SceneId, SceneId) {
    let mut scene = SceneGraph::new(320.0, 200.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(20.0, 30.0, 200.0, 120.0),
            1,
        )
        .unwrap();
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 0.0),
                    bounds: Rect::new(0.0, 0.0, 200.0, 120.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    let hit_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: hit_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(12.0, 16.0, 140.0, 72.0),
                    interaction_id: "composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    (scene, tile_id, hit_id)
}

/// hud-hxhnt finding 2 (gate a): the rendered/measured draft text must NEVER
/// contain the `▌` (U+258C) caret glyph any more — the caret is a chrome-layer
/// quad now, not an inserted character — regardless of blink phase. This is the
/// core "no jitter" guarantee: if the glyph were still inserted, toggling it
/// would reflow every trailing character on each blink tick.
#[tokio::test]
async fn composer_draft_text_never_contains_caret_glyph() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id, hit_id) = composer_caret_test_scene();
    let tokens = resolve_composer_overlay_tokens(&std::collections::HashMap::new());
    let tile = scene.tiles.get(&tile_id).unwrap();

    compositor.local_composer = Some(LocalComposerState {
        text: "hello world".to_owned(),
        cursor_byte: 5,
        selection_anchor: 5,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });

    // Blink "on" phase (fresh compositor → elapsed == 0 → solid).
    let on = compositor
        .collect_composer_text_item(tile, &scene, 320.0, 200.0, &tokens)
        .expect("focused composer must produce a text item");
    assert!(
        !on.text.contains('▌'),
        "draft text must never contain the caret glyph (on phase), got {:?}",
        on.text
    );
    assert_eq!(
        on.text.as_ref(),
        "hello world",
        "draft text must be the raw draft verbatim (on phase)"
    );

    // Blink "off" phase (force elapsed past one half-period).
    compositor.composer_caret_blink_start = std::time::Instant::now()
        .checked_sub(CARET_BLINK_HALF_PERIOD)
        .expect("test clock must have enough uptime to rewind one half-period");
    let off = compositor
        .collect_composer_text_item(tile, &scene, 320.0, 200.0, &tokens)
        .expect("focused composer must produce a text item");
    assert!(
        !off.text.contains('▌'),
        "draft text must never contain the caret glyph (off phase), got {:?}",
        off.text
    );
    assert_eq!(
        off.text.as_ref(),
        "hello world",
        "draft text must be identical across blink phases — blink-invariant (hud-hxhnt)"
    );
    assert_eq!(
        on.text, off.text,
        "the draft TextItem must not change at all when the caret blinks off"
    );
}

/// hud-hxhnt finding 2 (gate b): the caret renders as a zero-width-relative
/// vertical QUAD at the shaped caret-x, emitted by
/// `append_composer_caret_vertices` — the same primitive/pass as the focus ring.
/// Uses an EMPTY draft so the expected caret x is deterministic (no font-metric
/// dependency): `region.x + content_inset_px` exactly, matching the geometry
/// `composer_echo_confined_to_bottom_strip_full_tile_hitregion` pins for this
/// same scene (region.x == 38, strip_y == 83.6, content_inset_px == 6.0).
#[tokio::test]
async fn composer_caret_quad_emitted_at_expected_position() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, _tile_id, hit_id) = composer_caret_test_scene();

    compositor.local_composer = Some(LocalComposerState {
        text: String::new(),
        cursor_byte: 0,
        selection_anchor: 0,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });
    // Pin the solid blink phase: GPU setup under load can outlast one half-period.
    compositor.composer_caret_blink_start = std::time::Instant::now();
    compositor.prime_composer_scroll_offset(&scene);
    // Pin the blink to the "on" phase; a slow host can otherwise let the
    // wall-clock phase flip between compositor creation and this call.
    compositor.composer_caret_blink_start = std::time::Instant::now();

    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.append_composer_caret_vertices(&scene, &mut verts, 320.0, 200.0);
    assert_eq!(
        verts.len(),
        6,
        "one caret quad must emit 6 vertices (2 triangles), got {}",
        verts.len()
    );

    // Expected pixel geometry (mirrors the sibling text-item test's constants):
    // strip_height = 16*LINE_HEIGHT_MULTIPLIER + 12; strip_y = 46 + (72 - strip_height).
    let strip_height = 16.0 * crate::text::LINE_HEIGHT_MULTIPLIER + 12.0;
    let strip_y = 46.0 + (72.0 - strip_height);
    let expected_x = 38.0; // region.x (20+12=32) + content_inset_px (6.0)
    let expected_y = strip_y + 6.0; // input_box.y + content_inset_px

    let expected_left = (expected_x / 320.0) * 2.0 - 1.0;
    let expected_top = 1.0 - (expected_y / 200.0) * 2.0;
    let min_x = verts
        .iter()
        .map(|v| v.position[0])
        .fold(f32::INFINITY, f32::min);
    let max_y_ndc = verts
        .iter()
        .map(|v| v.position[1])
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(
        (min_x - expected_left).abs() < 1e-3,
        "caret quad left edge NDC mismatch: got {min_x}, want {expected_left} (pixel x {expected_x})"
    );
    assert!(
        (max_y_ndc - expected_top).abs() < 1e-3,
        "caret quad top edge NDC mismatch: got {max_y_ndc}, want {expected_top} (pixel y {expected_y})"
    );
}

/// hud-hxhnt finding 2 (gate c): the selection highlight `StyledRunItem` byte
/// range is the RAW `[min(cursor, anchor), max(cursor, anchor))` range — no
/// +3-byte caret-glyph shift, since the caret is no longer inserted into the
/// display string.
#[tokio::test]
async fn composer_selection_styled_run_uses_raw_byte_offsets() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id, hit_id) = composer_caret_test_scene();
    let tokens = resolve_composer_overlay_tokens(&std::collections::HashMap::new());
    let tile = scene.tiles.get(&tile_id).unwrap();

    let check = |compositor: &mut Compositor,
                 cursor: usize,
                 anchor: usize,
                 want: (usize, usize)| {
        compositor.local_composer = Some(LocalComposerState {
            text: "hello".to_owned(),
            cursor_byte: cursor,
            selection_anchor: anchor,
            at_capacity: false,
            node_id: hit_id,
            placeholder: None,
        });
        let item = compositor
            .collect_composer_text_item(tile, &scene, 320.0, 200.0, &tokens)
            .expect("focused composer with a selection must produce a text item");
        assert_eq!(
            item.styled_runs.len(),
            1,
            "an active selection must emit exactly one styled run (cursor={cursor}, anchor={anchor})"
        );
        let run = &item.styled_runs[0];
        assert_eq!(
            (run.start_byte, run.end_byte),
            want,
            "selection byte range must be the RAW [min,max) range, no caret-glyph shift \
             (cursor={cursor}, anchor={anchor})"
        );
        assert_eq!(
            run.background_color,
            Some(tokens.selection_bg),
            "selection run must be colored from portal.composer.selection_color"
        );
    };

    // cursor < anchor.
    check(&mut compositor, 2, 4, (2, 4));
    // cursor > anchor.
    check(&mut compositor, 4, 2, (2, 4));
    // whole string selected, cursor at start.
    check(&mut compositor, 0, 5, (0, 5));
    // whole string selected, cursor at end.
    check(&mut compositor, 5, 0, (0, 5));

    // cursor == anchor → no selection → no styled run.
    compositor.local_composer = Some(LocalComposerState {
        text: "hello".to_owned(),
        cursor_byte: 3,
        selection_anchor: 3,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });
    let item = compositor
        .collect_composer_text_item(tile, &scene, 320.0, 200.0, &tokens)
        .expect("focused composer must produce a text item");
    assert!(
        item.styled_runs.is_empty(),
        "cursor == anchor must emit no selection styled run"
    );
}

/// hud-hxhnt finding 1 (gate d): the SINGLE-LINE composer profile must now also
/// publish a one-row `ComposerVisualLayout` (previously only the multi-line
/// profile did), so the runtime's pointer hit-test can use real glyph geometry
/// instead of a linear byte-fraction guess for single-line composers too.
#[tokio::test]
async fn single_line_composer_publishes_visual_layout() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, _tile_id, hit_id) = composer_caret_test_scene();

    compositor.local_composer = Some(LocalComposerState {
        text: "hi".to_owned(),
        cursor_byte: 2,
        selection_anchor: 2,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });
    // Default token map → max_lines defaults to > 1, so pin max_lines=1 to
    // force the single-line profile explicitly (rather than relying on the
    // default, which could drift independently of this test's intent).
    compositor
        .token_map
        .insert("portal.composer.max_lines".to_string(), "1".to_string());
    compositor.prime_composer_scroll_offset(&scene);

    let visual = compositor
        .composer_visual_layout
        .lock()
        .unwrap()
        .clone()
        .expect(
            "single-line profile must now publish a ComposerVisualLayout (hud-hxhnt finding 1)",
        );

    assert_eq!(visual.text_len, 2, "text_len must match the 2-byte draft");
    assert_eq!(
        visual.lines.len(),
        1,
        "single-line profile publishes exactly one row"
    );
    let line = &visual.lines[0];
    assert_eq!(line.start_byte, 0);
    assert_eq!(line.end_byte, 2);
    assert_eq!(
        line.glyph_x.last().map(|&(b, _)| b),
        Some(2),
        "the trailing sentinel must map the row end (text.len())"
    );
    assert!(
        visual.input_box.is_none(),
        "single-line profile has no rendered multi-row box — byte_at_point's \
         even-split-over-one-row fallback is exact, so input_box stays None"
    );
    assert_eq!(
        visual.x_at_cursor(0),
        0.0,
        "caret x at byte 0 must be the line origin"
    );
    assert!(
        visual.x_at_cursor(2) > 0.0,
        "caret x at the end of a non-empty draft must be past the origin"
    );
}

// ─── Horizontal caret-follow (hud-zlfi4) ─────────────────────────────────────
//
// `composer_scroll_offset` is the pure, GPU-free core of the composer's
// horizontal caret-follow: given a caret x and full draft width (both measured
// against the composer font by the renderer), it returns how far to scroll the
// draft left so the caret stays visible. These CPU-only tests pin the standard
// single-line chat-input semantics; the measurement + apply path is exercised
// by the live-verify pass.

/// Fixed keep-visible margin used across the caret-follow tests (mirrors the
/// composer's `text_margin`).
const FOLLOW_MARGIN: f32 = 6.0;

/// When the draft fits inside the visible window, the offset is always 0
/// (left-aligned) regardless of caret position.
#[test]
fn caret_follow_no_scroll_when_text_fits() {
    let window = 100.0;
    // Caret at start, middle, and end of a draft narrower than the window.
    for &caret_x in &[0.0_f32, 25.0, 50.0] {
        let off = composer_scroll_offset(caret_x, 50.0, window, FOLLOW_MARGIN);
        assert_eq!(
            off, 0.0,
            "fitting draft must never scroll (caret_x={caret_x})"
        );
    }
    // Exactly-fitting draft (content == window) still does not scroll.
    assert_eq!(
        composer_scroll_offset(100.0, 100.0, window, FOLLOW_MARGIN),
        0.0
    );
}

/// Typing past the box width advances the scroll offset so the caret's on-screen
/// x stays within the visible window (caret is at the draft end while typing).
#[test]
fn caret_follow_advances_when_typing_past_width() {
    let window = 100.0;
    // Draft has grown to 150px; caret sits at the end (typing).
    let off = composer_scroll_offset(150.0, 150.0, window, FOLLOW_MARGIN);
    assert!(off > 0.0, "overflowing draft must scroll, got {off}");
    // Caret on-screen position = caret_x - offset must be inside [margin, window-margin].
    let caret_on_screen = 150.0 - off;
    assert!(
        caret_on_screen >= FOLLOW_MARGIN - 0.5 && caret_on_screen <= window - FOLLOW_MARGIN + 0.5,
        "caret must stay within the keep-visible band, got {caret_on_screen}"
    );
    assert!(
        caret_on_screen <= window,
        "caret must not fall off the right edge"
    );
}

/// Home (caret_x == 0) resets the scroll offset to 0, revealing the draft start.
#[test]
fn caret_follow_home_resets_to_zero() {
    let window = 100.0;
    // Long draft (500px) but caret jumped to Home.
    let off = composer_scroll_offset(0.0, 500.0, window, FOLLOW_MARGIN);
    assert_eq!(off, 0.0, "Home must reset scroll to 0");
}

/// End (caret_x == content_width) reveals the tail: the caret sits at the right
/// keep-visible band and the offset is the maximum (no dead space past the end).
#[test]
fn caret_follow_end_shows_tail() {
    let window = 100.0;
    let content = 500.0;
    let off = composer_scroll_offset(content, content, window, FOLLOW_MARGIN);
    let max_scroll = content + FOLLOW_MARGIN - window;
    assert!(
        (off - max_scroll).abs() < 0.5,
        "End must scroll to the tail (max_scroll={max_scroll}), got {off}"
    );
    // Caret sits exactly at the right keep-visible band.
    let caret_on_screen = content - off;
    assert!(
        (caret_on_screen - (window - FOLLOW_MARGIN)).abs() < 0.5,
        "End caret must sit at window - margin, got {caret_on_screen}"
    );
}

/// A caret parked in the MIDDLE of a wide draft stays visible on screen.
#[test]
fn caret_follow_mid_text_stays_visible() {
    let window = 100.0;
    let content = 500.0;
    let off = composer_scroll_offset(300.0, content, window, FOLLOW_MARGIN);
    let caret_on_screen = 300.0 - off;
    assert!(
        caret_on_screen >= 0.0 && caret_on_screen <= window,
        "mid-text caret must remain within the window, got {caret_on_screen}"
    );
}

/// Sweeping the caret from End back toward Home keeps it visible at every step
/// and monotonically reveals earlier text (offset never increases as caret_x
/// decreases). Guards the spec scenario "moving the caret back toward the start
/// SHALL reveal the earlier text, keeping the caret visible throughout".
#[test]
fn caret_follow_moving_left_reveals_earlier_text() {
    let window = 100.0;
    let content = 500.0;
    let mut prev_off = f32::INFINITY;
    let mut caret_x = content;
    while caret_x >= 0.0 {
        let off = composer_scroll_offset(caret_x, content, window, FOLLOW_MARGIN);
        // Caret stays on-screen throughout.
        let caret_on_screen = caret_x - off;
        assert!(
            caret_on_screen >= -0.5 && caret_on_screen <= window + 0.5,
            "caret must stay visible while moving left (caret_x={caret_x}, on_screen={caret_on_screen})"
        );
        // Offset is monotonically non-increasing as the caret moves left.
        assert!(
            off <= prev_off + 0.001,
            "moving the caret left must not scroll further right (caret_x={caret_x}, off={off}, prev={prev_off})"
        );
        prev_off = off;
        caret_x -= 20.0;
    }
    // Fully at Home the earlier text is revealed (offset 0).
    assert_eq!(
        composer_scroll_offset(0.0, content, window, FOLLOW_MARGIN),
        0.0
    );
}

/// Deleting text (draft shrinks) scrolls back left with no dead space: the
/// offset is clamped so nothing past the draft-end + margin is ever revealed.
#[test]
fn caret_follow_delete_scrolls_back_no_dead_space() {
    let window = 100.0;
    // Draft was wide (offset was large); now the user deleted down to 120px with
    // the caret at the new end.
    let off = composer_scroll_offset(120.0, 120.0, window, FOLLOW_MARGIN);
    let max_scroll = 120.0 + FOLLOW_MARGIN - window;
    assert!(
        off <= max_scroll + 0.001,
        "offset must not exceed max_scroll after delete (got {off}, max {max_scroll})"
    );
    // Delete further so the draft now fits — offset snaps back to 0.
    assert_eq!(
        composer_scroll_offset(80.0, 80.0, window, FOLLOW_MARGIN),
        0.0,
        "once the draft fits again the window must left-align (no dead space)"
    );
}

/// The returned offset is always within `[0, max_scroll]` and finite for a
/// range of inputs — never negative, never NaN, never past the tail.
#[test]
fn caret_follow_offset_always_bounded() {
    let window = 100.0;
    for &content in &[0.0_f32, 50.0, 100.0, 250.0, 1000.0] {
        for step in 0..=10 {
            let caret_x = content * (step as f32) / 10.0;
            let off = composer_scroll_offset(caret_x, content, window, FOLLOW_MARGIN);
            assert!(off.is_finite(), "offset must be finite");
            assert!(off >= 0.0, "offset must be non-negative, got {off}");
            let max_scroll = (content + FOLLOW_MARGIN - window).max(0.0);
            assert!(
                off <= max_scroll + 0.001,
                "offset must not exceed max_scroll (off={off}, max={max_scroll}, content={content})"
            );
        }
    }
}

/// A degenerate (zero/negative) window never scrolls, and a margin wider than the
/// window is clamped so the target band cannot invert (narrow-box robustness).
#[test]
fn caret_follow_degenerate_inputs_are_safe() {
    // Zero-width window → no scroll, no panic.
    assert_eq!(composer_scroll_offset(50.0, 500.0, 0.0, FOLLOW_MARGIN), 0.0);
    assert_eq!(
        composer_scroll_offset(50.0, 500.0, -10.0, FOLLOW_MARGIN),
        0.0
    );

    // Very narrow box with an over-wide margin: margin is clamped to window/2, so
    // the caret still lands inside the window and the offset stays bounded.
    let window = 8.0;
    let content = 100.0;
    let off = composer_scroll_offset(content, content, window, /* margin */ 100.0);
    let caret_on_screen = content - off;
    assert!(
        caret_on_screen >= 0.0 && caret_on_screen <= window,
        "narrow-box caret must stay within the window, got {caret_on_screen}"
    );
}

// ─── Multi-line composer wrap / growth / vscroll (hud-nx7yq.1) ────────────────
//
// CPU-only tests over the pure layout core: how many lines the box shows, how far
// it scrolls vertically to keep the caret line visible, the upward-grown box
// geometry, and the max-lines token. The wrap measurement + GPU render are
// exercised by CI's pixel-readback lane (headless llvmpipe readback deadlocks
// under a synchronous local run).

/// Default sans-serif line-height multiplier used to size composer boxes.
const NX_LH_MULT: f32 = 1.4;

/// The box shows `min(total_lines, max_lines)` lines, but never fewer than one.
#[test]
fn multiline_visible_line_count_grows_then_caps() {
    assert_eq!(
        composer_visible_line_count(1, 6),
        1,
        "one line stays one line"
    );
    assert_eq!(
        composer_visible_line_count(3, 6),
        3,
        "grows with wrapped lines"
    );
    assert_eq!(composer_visible_line_count(6, 6), 6, "reaches the max");
    assert_eq!(composer_visible_line_count(10, 6), 6, "caps at the max");
    assert_eq!(
        composer_visible_line_count(0, 6),
        1,
        "empty draft still shows one line"
    );
    // max_lines == 1 is the single-line profile: always one visible line.
    assert_eq!(composer_visible_line_count(5, 1), 1);
}

/// Vertical scroll keeps the caret line visible: no scroll while the draft fits,
/// bottom-pin as it grows, and reveal-upward as the caret moves toward the top.
#[test]
fn multiline_vertical_offset_keeps_caret_line_visible() {
    let max = 6;
    // Fits within the window → never scrolls, regardless of caret line.
    for caret in 0..=3 {
        assert_eq!(
            composer_vertical_line_offset(caret, 4, max),
            0,
            "fits: no vscroll"
        );
    }
    // 10 lines, caret at the end (typing): bottom-pin shows lines 4..=9.
    let first = composer_vertical_line_offset(9, 10, max);
    assert_eq!(first, 4, "caret at last line pins to the bottom window");
    assert!(
        9 >= first && 9 < first + max,
        "caret line stays within the window"
    );
    // Caret jumped to the top → reveal the earliest lines.
    assert_eq!(
        composer_vertical_line_offset(0, 10, max),
        0,
        "top caret reveals line 0"
    );
    // Caret in the middle stays visible (bottom-pinned).
    let firstm = composer_vertical_line_offset(7, 10, max);
    assert!(
        7 >= firstm && 7 < firstm + max,
        "mid caret stays within the window"
    );
}

/// Moving the caret upward line-by-line never scrolls further down and keeps the
/// caret visible throughout (vertical analogue of the horizontal left-sweep test).
#[test]
fn multiline_vertical_offset_moving_up_reveals_earlier_lines() {
    let (total, max) = (12usize, 5usize);
    let mut prev = usize::MAX;
    for caret in (0..total).rev() {
        let first = composer_vertical_line_offset(caret, total, max);
        assert!(
            caret >= first && caret < first + max,
            "caret line {caret} must stay within window [{first},{})",
            first + max
        );
        assert!(
            first <= prev,
            "moving up must not scroll further down (caret={caret})"
        );
        prev = first;
    }
    assert_eq!(composer_vertical_line_offset(0, total, max), 0);
}

/// The offset never exceeds `total_lines - max_lines` (no dead space below the
/// last line) and is zero once the draft shrinks back within the window.
#[test]
fn multiline_vertical_offset_bounded_and_shrinks_back() {
    let max = 6;
    for total in [1usize, 6, 7, 20] {
        let max_first = total.saturating_sub(max);
        for caret in 0..total {
            let first = composer_vertical_line_offset(caret, total, max);
            assert!(
                first <= max_first,
                "offset {first} exceeds max_first {max_first}"
            );
        }
    }
    // Draft shrank back to fit → scroll resets to 0 (transcript reclaims space).
    assert_eq!(composer_vertical_line_offset(3, 4, max), 0);
}

/// `composer_input_box` grows UPWARD from the bottom edge as `visible_lines`
/// increases, and `visible_lines == 1` reproduces the single-line strip exactly.
#[test]
fn multiline_input_box_grows_upward_pinned_bottom() {
    let region = Rect::new(10.0, 100.0, 600.0, 300.0); // bottom edge at y=400
    let font = 16.0;
    let line_height = font * NX_LH_MULT;
    let margin = 6.0; // COMPOSER_TEXT_MARGIN

    let one = Compositor::composer_input_box(
        region,
        font,
        NX_LH_MULT,
        1.0,
        ComposerVerticalAnchor::Bottom,
        margin,
    );
    let expected_one_h = line_height + margin * 2.0;
    assert!(
        (one.height - expected_one_h).abs() < 0.01,
        "one-line height"
    );
    assert!(
        (one.y + one.height - (region.y + region.height)).abs() < 0.01,
        "one-line box is pinned to the region bottom"
    );

    let three = Compositor::composer_input_box(
        region,
        font,
        NX_LH_MULT,
        3.0,
        ComposerVerticalAnchor::Bottom,
        margin,
    );
    let expected_three_h = line_height * 3.0 + margin * 2.0;
    assert!(
        (three.height - expected_three_h).abs() < 0.01,
        "three-line height"
    );
    // Grew upward: taller box, same bottom edge, higher (smaller y) top.
    assert!(three.height > one.height, "box grew with more lines");
    assert!(three.y < one.y, "box grew UPWARD (top moved up)");
    assert!(
        (three.y + three.height - (region.y + region.height)).abs() < 0.01,
        "box stays pinned to the region bottom while growing"
    );
    // Width and x are untouched (portal outer geometry unaffected).
    assert_eq!(three.x, region.x);
    assert_eq!(three.width, region.width);
}

/// The box height is clamped to the region height so a huge line count cannot
/// exceed the portal.
#[test]
fn multiline_input_box_clamped_to_region() {
    let region = Rect::new(0.0, 0.0, 400.0, 50.0);
    let box_rect = Compositor::composer_input_box(
        region,
        16.0,
        NX_LH_MULT,
        20.0,
        ComposerVerticalAnchor::Bottom,
        6.0, // default content inset
    );
    assert!(
        box_rect.height <= region.height + 0.01,
        "clamped to region height"
    );
    assert!(box_rect.y >= region.y - 0.01, "top not above the region");
}

/// hud-nottc: with the TOP anchor the composer input box pins to the region TOP
/// (the pane content origin) and grows DOWNWARD as `visible_lines` rises, so the
/// caret rests at the pane top-left when the draft is empty and the top edge
/// never moves when the first glyph is typed. Contrast the Bottom anchor, which
/// pins a single-line box near the region bottom — the "teleport" the owner saw.
#[test]
fn top_anchored_input_box_pins_to_region_top_and_grows_down() {
    let region = Rect::new(10.0, 100.0, 600.0, 300.0); // bottom edge at y=400
    let font = 16.0;
    let line_height = font * NX_LH_MULT;
    let margin = 6.0; // COMPOSER_TEXT_MARGIN

    let one = Compositor::composer_input_box(
        region,
        font,
        NX_LH_MULT,
        1.0,
        ComposerVerticalAnchor::Top,
        margin,
    );
    // Empty / single-line draft: box top IS the region top (pane content origin),
    // not pinned to the region bottom.
    assert_eq!(one.y, region.y, "top-anchored box pins to the region TOP");
    assert!(
        (one.height - (line_height + margin * 2.0)).abs() < 0.01,
        "one-line height"
    );

    let three = Compositor::composer_input_box(
        region,
        font,
        NX_LH_MULT,
        3.0,
        ComposerVerticalAnchor::Top,
        margin,
    );
    // Grows DOWNWARD: taller box, SAME top edge (the first line does not teleport).
    assert_eq!(
        three.y, region.y,
        "top edge stays fixed as the box grows down"
    );
    assert!(three.height > one.height, "box grew with more lines");
    assert_eq!(three.x, region.x);
    assert_eq!(three.width, region.width);

    // Contrast: the Bottom anchor would place the single-line box far below the
    // top anchor — the teleport-to-bottom the owner reported for the empty draft.
    let bottom_one = Compositor::composer_input_box(
        region,
        font,
        NX_LH_MULT,
        1.0,
        ComposerVerticalAnchor::Bottom,
        margin,
    );
    assert!(
        bottom_one.y > one.y + 100.0,
        "bottom anchor sits far below the top anchor (top y={}, bottom y={})",
        one.y,
        bottom_one.y
    );
}

/// `portal.composer.anchor` selects the composer vertical anchor; the default is
/// `Bottom` so every existing bottom-chat-strip profile is unchanged (hud-nottc).
#[test]
fn composer_anchor_token_resolves() {
    use std::collections::HashMap;

    let default = resolve_composer_overlay_tokens(&HashMap::new());
    assert_eq!(
        default.anchor,
        ComposerVerticalAnchor::Bottom,
        "default anchor is the bottom-chat strip"
    );

    let mut top = HashMap::new();
    top.insert("portal.composer.anchor".to_string(), "top".to_string());
    assert_eq!(
        resolve_composer_overlay_tokens(&top).anchor,
        ComposerVerticalAnchor::Top
    );

    // Case- and whitespace-insensitive.
    let mut mixed = HashMap::new();
    mixed.insert("portal.composer.anchor".to_string(), "  TOP ".to_string());
    assert_eq!(
        resolve_composer_overlay_tokens(&mixed).anchor,
        ComposerVerticalAnchor::Top
    );

    // Unknown / malformed value falls back to Bottom.
    let mut bogus = HashMap::new();
    bogus.insert("portal.composer.anchor".to_string(), "sideways".to_string());
    assert_eq!(
        resolve_composer_overlay_tokens(&bogus).anchor,
        ComposerVerticalAnchor::Bottom,
        "unknown anchor value must fall back to Bottom"
    );
}

/// hud-6ti2z: the non-composer tile-render spacing literals (code-panel backdrop
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

    // Defaults equal the historical literals (no visual regression).
    let default = resolve_tile_spacing_tokens(&HashMap::new());
    assert_eq!(default.transcript_fallback_inset_px, 8.0);
    assert_eq!(default.code_panel_margin_x_px, 4.0);
    assert_eq!(default.code_panel_pad_y_px, 2.0);
    assert_eq!(default.image_margin_px, 4.0);

    // Each key overrides its own field, independently of the others.
    let mut over = HashMap::new();
    over.insert(
        "portal.spacing.transcript_fallback_inset_px".to_string(),
        "12".to_string(),
    );
    over.insert(
        "portal.spacing.code_panel_margin_x_px".to_string(),
        "7".to_string(),
    );
    over.insert(
        "portal.spacing.code_panel_pad_y_px".to_string(),
        "3.5".to_string(),
    );
    over.insert(
        "portal.spacing.image_margin_px".to_string(),
        "9".to_string(),
    );
    let resolved = resolve_tile_spacing_tokens(&over);
    assert_eq!(resolved.transcript_fallback_inset_px, 12.0);
    assert_eq!(resolved.code_panel_margin_x_px, 7.0);
    assert_eq!(resolved.code_panel_pad_y_px, 3.5);
    assert_eq!(resolved.image_margin_px, 9.0);

    // Flush (0.0) is a valid inset/margin and must be honored.
    let mut flush = HashMap::new();
    flush.insert(
        "portal.spacing.code_panel_margin_x_px".to_string(),
        "0".to_string(),
    );
    assert_eq!(
        resolve_tile_spacing_tokens(&flush).code_panel_margin_x_px,
        0.0
    );

    // Malformed / negative / non-finite overrides fall back to the default.
    for bad in ["not-a-number", "-4", "NaN", "inf", ""] {
        let mut m = HashMap::new();
        m.insert(
            "portal.spacing.image_margin_px".to_string(),
            bad.to_string(),
        );
        assert_eq!(
            resolve_tile_spacing_tokens(&m).image_margin_px,
            4.0,
            "malformed image_margin override {bad:?} must fall back to the 4.0 default"
        );
    }
}

/// hud-nottc live P1 (round 5): with the TOP anchor the composer caret must
/// render at the input pane's top-left CONTENT ORIGIN when the draft is EMPTY
/// (region top + margin — NOT the window's (0,0) top-left, NOT the region
/// bottom), and it must NOT teleport when the first glyph is typed. Asserts the
/// `TextItem` geometry directly (no pixel readback), so it is safe headless.
#[tokio::test]
async fn top_anchored_empty_caret_sits_at_pane_origin_no_teleport() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(600, 400).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    // Select the top-anchored exemplar input-pane profile.
    compositor
        .token_map
        .insert("portal.composer.anchor".to_string(), "top".to_string());

    let mut scene = SceneGraph::new(600.0, 400.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 600.0, 400.0),
            1,
        )
        .unwrap();
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.02, 0.02, 0.02, 1.0),
                    bounds: Rect::new(0.0, 0.0, 600.0, 400.0),
                    radius: None,
                }),
            },
        )
        .unwrap();

    // Short input pane at the tile TOP-LEFT (exemplar style). local bounds are
    // tile-relative; the tile is at the scene origin so pane top == y 0.
    const PANE_Y: f32 = 0.0;
    const PANE_H: f32 = 80.0;
    let hit_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: hit_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, PANE_Y, 400.0, PANE_H),
                    interaction_id: "composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    let tokens = resolve_composer_overlay_tokens(&compositor.token_map);
    let margin = 6.0f32; // COMPOSER_TEXT_MARGIN

    // ── Empty draft: caret at the pane content origin. ──
    compositor.local_composer = Some(LocalComposerState {
        text: String::new(),
        cursor_byte: 0,
        selection_anchor: 0,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });
    compositor.prime_composer_scroll_offset(&scene);
    let empty_item = {
        let tile = scene.tiles.get(&tile_id).unwrap();
        compositor
            .collect_composer_text_item(tile, &scene, 600.0, 400.0, &tokens)
            .expect("empty focused composer must still produce a caret text item")
    };
    assert!(
        (empty_item.pixel_y - (PANE_Y + margin)).abs() < 0.01,
        "empty caret y must be the pane top + margin (content origin), got {}",
        empty_item.pixel_y
    );
    assert!(
        (empty_item.pixel_x - margin).abs() < 0.01,
        "empty caret x must be the pane left + margin, got {}",
        empty_item.pixel_x
    );

    // ── First glyph typed: NO teleport — the first line keeps the same origin. ──
    compositor.local_composer = Some(LocalComposerState {
        text: "h".to_owned(),
        cursor_byte: 1,
        selection_anchor: 1,
        at_capacity: false,
        node_id: hit_id,
        placeholder: None,
    });
    compositor.prime_composer_scroll_offset(&scene);
    let typed_item = {
        let tile = scene.tiles.get(&tile_id).unwrap();
        compositor
            .collect_composer_text_item(tile, &scene, 600.0, 400.0, &tokens)
            .expect("typed composer must produce a text item")
    };
    assert!(
        (typed_item.pixel_y - empty_item.pixel_y).abs() < 0.01,
        "caret teleported on the first keystroke: empty y={} typed y={}",
        empty_item.pixel_y,
        typed_item.pixel_y
    );
}

// ─── Caret stays in the box for a short composer pane (hud-nottc) ─────────────

/// `composer_region_fit_lines` reports how many text lines a region interior fits.
#[test]
fn composer_region_fit_lines_computes_capacity() {
    let lh = 16.0 * NX_LH_MULT; // 22.4
    let m = 6.0;
    // (60 - 12) / 22.4 = 2.14 → 2 lines (the exemplar-style short input pane).
    assert_eq!(image_cache::composer_region_fit_lines(60.0, lh, m), 2);
    // Tall region fits many lines: (300 - 12) / 22.4 = 12.85 → 12.
    assert_eq!(image_cache::composer_region_fit_lines(300.0, lh, m), 12);
    // Too short for even one line → floors to 1 (never zero).
    assert_eq!(image_cache::composer_region_fit_lines(20.0, lh, m), 1);
    // Degenerate line height → 1.
    assert_eq!(image_cache::composer_region_fit_lines(60.0, 0.0, m), 1);
}

/// Regression for hud-nottc: a wrapped draft in a SHORT composer pane must keep
/// the caret line inside the visible box. Bounding growth + scroll only by the
/// `max_lines` token (not the region capacity) left the caret clipped OUTSIDE the
/// box — the top-left-caret live symptom. Bounding by the region fit keeps it in.
#[test]
fn caret_stays_within_short_composer_pane() {
    let region_h = 60.0;
    let line_height = 16.0 * NX_LH_MULT; // 22.4
    let margin = 6.0;
    let fit = image_cache::composer_region_fit_lines(region_h, line_height, margin);
    assert_eq!(fit, 2, "the 60px pane fits 2 text lines");

    // A long single line wrapped to 3 visual rows; caret at the end (last row).
    let (total_lines, caret_line, token_max) = (3usize, 2usize, 6usize);

    // BUGGY path — scroll bounded only by the token: the box only fits `fit` rows,
    // but the caret's box-relative row is >= fit, i.e. clipped out of the box.
    let bad_first = composer_vertical_line_offset(caret_line, total_lines, token_max);
    assert!(
        caret_line - bad_first >= fit,
        "token-only scroll leaves the caret outside the {fit}-line box (the bug)"
    );

    // FIXED path — bound growth AND scroll by the region fit.
    let eff = token_max.min(fit).max(1);
    let good_first = composer_vertical_line_offset(caret_line, total_lines, eff);
    let good_visible = composer_visible_line_count(total_lines, eff);
    assert!(
        good_visible <= fit,
        "box never grows past what the pane fits ({good_visible} <= {fit})"
    );
    assert!(
        caret_line >= good_first && caret_line < good_first + good_visible,
        "caret line {caret_line} within the visible window [{good_first}, {})",
        good_first + good_visible
    );
    assert!(
        caret_line - good_first < fit,
        "caret's box-relative row fits inside the box"
    );
}

/// The `portal.composer.max_lines` token defaults to 6, parses an override, and
/// clamps a stray `0` up to the single-line floor of 1.
#[test]
fn multiline_max_lines_token_default_and_clamp() {
    use std::collections::HashMap;
    // Default (empty map) → 6.
    let def = resolve_composer_overlay_tokens(&HashMap::new());
    assert_eq!(def.max_lines, 6, "default max_lines is 6");
    // Explicit override.
    let mut m = HashMap::new();
    m.insert("portal.composer.max_lines".to_owned(), "3".to_owned());
    assert_eq!(resolve_composer_overlay_tokens(&m).max_lines, 3);
    // Single-line profile.
    let mut m1 = HashMap::new();
    m1.insert("portal.composer.max_lines".to_owned(), "1".to_owned());
    assert_eq!(resolve_composer_overlay_tokens(&m1).max_lines, 1);
    // Stray 0 → rejected, falls back to the default (never a zero-height box).
    let mut m0 = HashMap::new();
    m0.insert("portal.composer.max_lines".to_owned(), "0".to_owned());
    assert_eq!(resolve_composer_overlay_tokens(&m0).max_lines, 6);
}

/// hud-ar10c: the composer content inset is token-driven via
/// `portal.spacing.content_inset_px`. The resolver defaults to 6.0 (the historical
/// `COMPOSER_TEXT_MARGIN` literal, so the default profile is unchanged), parses a
/// finite non-negative override, and rejects malformed / negative / non-finite
/// values back to the default. The resolved value must flow into the composer box
/// geometry: `composer_input_box`'s vertical padding is `content_inset * 2`, so a
/// widened inset grows the box by exactly twice the delta, and the default inset
/// reproduces the prior box height.
#[test]
fn composer_content_inset_token_drives_box_geometry() {
    use std::collections::HashMap;

    // Default (empty map) → 6.0.
    let def = resolve_composer_overlay_tokens(&HashMap::new());
    assert_eq!(
        def.content_inset_px, 6.0,
        "default content inset is 6.0 (matches the prior COMPOSER_TEXT_MARGIN)"
    );

    // Explicit finite override is taken verbatim.
    let mut m = HashMap::new();
    m.insert(
        "portal.spacing.content_inset_px".to_owned(),
        "12".to_owned(),
    );
    assert_eq!(resolve_composer_overlay_tokens(&m).content_inset_px, 12.0);

    // Zero (flush) is permitted.
    let mut mz = HashMap::new();
    mz.insert("portal.spacing.content_inset_px".to_owned(), "0".to_owned());
    assert_eq!(resolve_composer_overlay_tokens(&mz).content_inset_px, 0.0);

    // Negative / non-finite / malformed → rejected, falls back to the default.
    for bad in ["-4", "NaN", "inf", "wat", ""] {
        let mut mb = HashMap::new();
        mb.insert("portal.spacing.content_inset_px".to_owned(), bad.to_owned());
        assert_eq!(
            resolve_composer_overlay_tokens(&mb).content_inset_px,
            6.0,
            "malformed inset {bad:?} falls back to the default"
        );
    }

    // The resolved inset flows into the box geometry. `composer_input_box` pads
    // the box height by `content_inset * 2` on top of the text lines, so a wider
    // inset grows the box by exactly twice the delta while the default reproduces
    // the prior height.
    let region = Rect::new(0.0, 0.0, 600.0, 1000.0); // tall enough to avoid clamp
    let font = 16.0;
    let lhm = crate::markdown::MarkdownTokens::default().line_height_multiplier;
    let line_height = font * lhm;

    let box_default = Compositor::composer_input_box(
        region,
        font,
        lhm,
        1.0,
        ComposerVerticalAnchor::Bottom,
        def.content_inset_px,
    );
    assert!(
        (box_default.height - (line_height + 6.0 * 2.0)).abs() < 0.01,
        "default inset reproduces the prior one-line box height"
    );

    let wide_inset = resolve_composer_overlay_tokens(&m).content_inset_px; // 12.0
    let box_wide = Compositor::composer_input_box(
        region,
        font,
        lhm,
        1.0,
        ComposerVerticalAnchor::Bottom,
        wide_inset,
    );
    assert!(
        (box_wide.height - (line_height + 12.0 * 2.0)).abs() < 0.01,
        "wider inset grows the box height by twice the inset"
    );
    assert!(
        (box_wide.height - box_default.height - 2.0 * (12.0 - 6.0)).abs() < 0.01,
        "box height delta equals twice the inset delta"
    );
}

/// The default `ComposerLayout` is the inert single-line profile, so a frame with
/// no active composer (or an unmeasured one) never wraps or scrolls — preserving
/// the hud-zlfi4 single-line behavior exactly.
#[test]
fn multiline_default_layout_is_single_line() {
    let d = ComposerLayout::default();
    assert!(!d.wrap, "default profile is single-line");
    assert_eq!(d.h_scroll_px, 0.0);
    assert_eq!(d.vscroll_px, 0.0);
    assert_eq!(d.visible_lines, 1.0, "default box is one line tall");
    assert_eq!(d.total_lines, 1.0);
}

// ─── Transcript turn separators (hud-nx7yq.4) ────────────────────────────────

/// A divider rect is placed on each thematic-break line, centred vertically and
/// spanning the node width, at the newline-counted y-offset.
#[test]
fn separator_rects_placed_on_break_lines() {
    // "A\n\nB\n\nC": two blank lines (index 1 and 3) hold dividers.
    let plain = "A\n\nB\n\nC";
    let breaks = vec![2usize, 5usize]; // start of blank line 1, start of blank line 3
    let line_height = 20.0;
    let thickness = 2.0;
    let rects = tile_render::transcript_separator_rects(
        plain,
        &breaks,
        100.0,
        50.0,
        300.0,
        line_height,
        thickness,
    );
    assert_eq!(rects.len(), 2, "one rect per break");
    // First divider: blank line index 1 → center_y = 1.5*20 = 30 → y = 50+30-1 = 79.
    assert!(
        (rects[0].y - 79.0).abs() < 0.01,
        "first divider y, got {}",
        rects[0].y
    );
    assert_eq!(rects[0].x, 100.0, "spans from the node origin");
    assert_eq!(rects[0].width, 300.0, "spans the node width");
    assert_eq!(rects[0].height, 2.0, "thickness drives height");
    // Second divider: blank line index 3 → center_y = 3.5*20 = 70 → y = 50+70-1 = 119.
    assert!(
        (rects[1].y - 119.0).abs() < 0.01,
        "second divider y, got {}",
        rects[1].y
    );
}

/// Viewer-echo history renders a token-styled divider between each adjacent
/// pair of entries (hud-hsc1t): N entries → N−1 dividers at the cumulative
/// wrapped-line boundary, centred on the boundary line.
#[test]
fn viewer_echo_divider_rects_between_adjacent_entries() {
    // 3 entries with wrapped line counts [1, 2, 1]; block_top=100, lh=20, t=2.
    // Boundary after entry0 → 100 + 1*20 = 120; after entry1 → 100 + 3*20 = 160.
    let counts = [1usize, 2, 1];
    let rects =
        tile_render::viewer_echo_divider_rects(&counts, 10.0, 100.0, 200.0, 20.0, 2.0, 0.0, 1000.0);
    assert_eq!(rects.len(), 2, "N-1 dividers between N entries");
    assert!(
        (rects[0].y - 119.0).abs() < 0.01,
        "first boundary centred on y=120, got {}",
        rects[0].y
    );
    assert_eq!(rects[0].x, 10.0, "spans from the block origin");
    assert_eq!(rects[0].width, 200.0, "spans the zone width");
    assert_eq!(rects[0].height, 2.0, "thickness drives height");
    assert!(
        (rects[1].y - 159.0).abs() < 0.01,
        "second boundary centred on y=160, got {}",
        rects[1].y
    );
}

/// Boundaries scrolled above the visible band clip out; a single entry, zero
/// width, or zero thickness produce no dividers.
#[test]
fn viewer_echo_divider_rects_clips_and_degenerate() {
    let counts = [1usize, 1, 1];
    // Boundaries at y=120 and y=140; band_top=130 clips the first.
    let rects = tile_render::viewer_echo_divider_rects(
        &counts, 0.0, 100.0, 200.0, 20.0, 2.0, 130.0, 1000.0,
    );
    assert_eq!(rects.len(), 1, "boundary above band_top is clipped");
    assert!(
        (rects[0].y - 139.0).abs() < 0.01,
        "surviving divider is the in-band one, got {}",
        rects[0].y
    );
    assert!(
        tile_render::viewer_echo_divider_rects(&[3usize], 0.0, 0.0, 200.0, 20.0, 2.0, 0.0, 1000.0)
            .is_empty(),
        "single entry → no interior divider"
    );
    assert!(
        tile_render::viewer_echo_divider_rects(&counts, 0.0, 0.0, 0.0, 20.0, 2.0, 0.0, 1000.0)
            .is_empty(),
        "zero width → no rects"
    );
    assert!(
        tile_render::viewer_echo_divider_rects(&counts, 0.0, 0.0, 200.0, 20.0, 0.0, 0.0, 1000.0)
            .is_empty(),
        "zero thickness → no rects"
    );
}

/// hud-acfvp: the input-history block's top slides within the fixed band by the
/// input tile's clamped displayed scroll offset, so the viewer can wheel-scroll
/// UP through older entries. Pure scroll math — no rasterizer, no GPU.
#[test]
fn input_history_block_top_slides_with_scroll_offset() {
    // Band [100, 300] → band_height 200; a 500px history overflows by 300px.
    let band_top = 100.0_f32;
    let band_bottom = 300.0_f32;
    let block_height = 500.0_f32;
    let max_scrollback = block_height - (band_bottom - band_top); // 300

    // No scroll config → pin to the tail: block bottom (top + height) rests on the
    // band bottom, newest visible, oldest clipped above — the pre-scroll window.
    let tail = tile_render::input_history_block_top(band_top, band_bottom, block_height, None);
    assert!(
        (tail - (band_bottom - block_height)).abs() < 0.01,
        "no scroll config pins the block to the tail (band_bottom - block_height), got {tail}"
    );
    assert!(
        (tail - (band_top - max_scrollback)).abs() < 0.01,
        "tail equals band_top - max_scrollback"
    );

    // Offset seeded at the tail (max_scrollback) reproduces the tail exactly.
    let at_tail = tile_render::input_history_block_top(
        band_top,
        band_bottom,
        block_height,
        Some(max_scrollback),
    );
    assert!(
        (at_tail - tail).abs() < 0.01,
        "offset at the tail matches the no-config tail, got {at_tail}"
    );

    // Scrolling up (offset eases toward 0) slides the block DOWN, revealing older
    // lines; fully scrolled up rests the oldest line on the band top.
    let scrolled_up =
        tile_render::input_history_block_top(band_top, band_bottom, block_height, Some(0.0));
    assert!(
        (scrolled_up - band_top).abs() < 0.01,
        "fully scrolled up rests the oldest line on band_top, got {scrolled_up}"
    );
    assert!(
        scrolled_up > at_tail,
        "scrolling up moves the block DOWN (older revealed): {scrolled_up} > {at_tail}"
    );

    // A partial offset lands proportionally between the two, and the offset is
    // clamped so it can never overscroll past the oldest line or below the tail.
    let mid =
        tile_render::input_history_block_top(band_top, band_bottom, block_height, Some(100.0));
    assert!(
        (mid - (band_top - 100.0)).abs() < 0.01,
        "partial scroll-back is band_top - clamp(offset), got {mid}"
    );
    let over_up =
        tile_render::input_history_block_top(band_top, band_bottom, block_height, Some(-50.0));
    assert!(
        (over_up - band_top).abs() < 0.01,
        "negative offset clamps to the fully-scrolled-up bound, got {over_up}"
    );
    let over_down =
        tile_render::input_history_block_top(band_top, band_bottom, block_height, Some(9999.0));
    assert!(
        (over_down - tail).abs() < 0.01,
        "offset past the tail clamps to the tail, got {over_down}"
    );

    // History that fits the band has zero scroll range: every offset yields the
    // same bottom-aligned position (no spurious motion for short histories).
    let short = 120.0_f32; // < band_height (200)
    let fit_tail = tile_render::input_history_block_top(band_top, band_bottom, short, None);
    let fit_scrolled =
        tile_render::input_history_block_top(band_top, band_bottom, short, Some(50.0));
    assert!(
        (fit_tail - fit_scrolled).abs() < 0.01,
        "a history that fits the band never scrolls (max_scrollback == 0)"
    );
    assert!(
        (fit_tail - (band_bottom - short)).abs() < 0.01,
        "a fitting history stays bottom-aligned at band_bottom - block_height, got {fit_tail}"
    );
}

/// hud-3nus3: the input-history band is placed on the side of the composer box
/// AWAY from its anchored edge, so a viewer's submissions always have on-pane
/// room to paint. This is the pure geometry proof for the live report "input
/// tracked, nothing rendered" on the tzehouse exemplar (`portal.composer.anchor
/// = top`): with a top-pinned composer box the old band-above-box collapsed to
/// zero height and the whole history silently failed to paint. No rasterizer, no
/// GPU — asserts the band/block geometry directly.
#[test]
fn input_history_band_layout_places_history_below_a_top_anchored_composer() {
    // A tall composer region (the exemplar LEFT input pane) with a one-line box.
    let region = Rect::new(0.0, 0.0, 400.0, 300.0);
    let box_height = 24.0_f32;
    let block_height = 40.0_f32;

    // Bottom anchor (default): box rests on the region bottom; the band is the
    // space ABOVE it and the block bottom-aligns just above the box — byte-for-byte
    // the pre-fix behavior (band_top == region top, band_bottom == box top).
    let bottom_box = Rect::new(0.0, region.height - box_height, region.width, box_height);
    let (bt_top, bt_bottom, bt_block_top) = tile_render::input_history_band_layout(
        ComposerVerticalAnchor::Bottom,
        region,
        bottom_box,
        block_height,
        None,
    )
    .expect("bottom-anchored band has positive height");
    assert_eq!(
        bt_top, region.y,
        "bottom-anchor band starts at the region top"
    );
    assert_eq!(
        bt_bottom, bottom_box.y,
        "bottom-anchor band ends at the box top"
    );
    assert!(
        (bt_block_top - (bt_bottom - block_height)).abs() < 0.01,
        "bottom-anchor history bottom-aligns just above the box"
    );

    // Top anchor (exemplar two-pane input pane): box pins to the region TOP, so the
    // OLD band (`[region.y, draft_box.y]`) would be zero-height and paint nothing.
    // The band must instead open BELOW the box, with the block flowing downward
    // (top-aligned) beneath it — this is the fix.
    let top_box = Rect::new(0.0, region.y, region.width, box_height);
    assert!(
        top_box.y - region.y <= 0.0,
        "precondition: the old band-above-box is degenerate for a top-pinned box"
    );
    let (tp_top, tp_bottom, tp_block_top) = tile_render::input_history_band_layout(
        ComposerVerticalAnchor::Top,
        region,
        top_box,
        block_height,
        None,
    )
    .expect("top-anchored band must have positive height (the hud-3nus3 fix)");
    assert!(
        (tp_top - (top_box.y + top_box.height)).abs() < 0.01,
        "top-anchor band starts at the box bottom, got {tp_top}"
    );
    assert!(
        (tp_bottom - (region.y + region.height)).abs() < 0.01,
        "top-anchor band ends at the region bottom, got {tp_bottom}"
    );
    assert!(
        tp_bottom - tp_top > 0.0,
        "top-anchor band has room to paint submitted history"
    );
    assert!(
        (tp_block_top - tp_top).abs() < 0.01,
        "top-anchor history flows downward from just beneath the box (top-aligned), got {tp_block_top}"
    );
    // The first history line sits BELOW the composer box — "beneath the composer",
    // exactly the submit event's expected visual.
    assert!(
        tp_block_top >= top_box.y + top_box.height,
        "top-anchor history paints beneath the composer box"
    );
}

/// hud-acfvp end-to-end: the rendered input-history block honors the input tile's
/// scroll offset. With no scroll config the block pins to the tail; registering a
/// vertical scroll config and setting the offset to 0 slides the (overflowing)
/// block DOWN to reveal older entries, and an offset past the tail clamps back to
/// the tail. GPU-gated (needs `new_headless`); skips when no text rasterizer.
#[tokio::test]
async fn input_history_block_honors_tile_scroll_offset() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 100).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    if compositor.text_rasterizer.is_none() {
        eprintln!("skipping: no text rasterizer (viewer-echo text path unavailable headless)");
        return;
    }

    // Short tile so a handful of multi-line echoes overflow the band above the
    // composer box (max_scrollback > 0), making the scroll shift observable.
    let mut scene = SceneGraph::new(400.0, 100.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 100.0),
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
                    bounds: Rect::new(0.0, 0.0, 400.0, 100.0),
                    interaction_id: "portal-composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    // Several multi-line replies → a history block far taller than the band.
    for i in 0..4 {
        compositor
            .viewer_echoes
            .append(tile_id, format!("reply {i}\nline b\nline c"), i as u64);
    }
    compositor.prime_viewer_echo_layout(&scene);

    let echo_y = |c: &Compositor, s: &SceneGraph| -> f32 {
        let tile = s.visible_tiles()[0].clone();
        let tokens = super::token_colors::resolve_viewer_echo_tokens(&c.token_map);
        let items = c.collect_viewer_echo_text_items(&tile, s, 400.0, 100.0, &tokens);
        items
            .iter()
            .find(|t| t.color == VIEWER_ECHO_COLOR)
            .expect("a viewer-echo block must render")
            .pixel_y
    };

    // No scroll config → tail (bottom-aligned newest-fit window).
    let tail_y = echo_y(&compositor, &scene);

    // Register a vertical scroll config; the offset now drives the block.
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();

    // Offset 0 = fully scrolled up: the block slides DOWN, revealing older lines.
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 0.0)
        .unwrap();
    let scrolled_up_y = echo_y(&compositor, &scene);
    assert!(
        scrolled_up_y > tail_y + 1.0,
        "scrolling the input tile up must move the history block DOWN to reveal older \
         entries: scrolled_up_y {scrolled_up_y} should exceed tail_y {tail_y}"
    );

    // An offset far past the tail clamps back to the tail position.
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 100_000.0)
        .unwrap();
    let clamped_tail_y = echo_y(&compositor, &scene);
    assert!(
        (clamped_tail_y - tail_y).abs() < 0.5,
        "an offset past the tail clamps to the tail: {clamped_tail_y} vs {tail_y}"
    );
}

/// No breaks, zero width, or zero thickness produce no divider rects.
#[test]
fn separator_rects_degenerate_inputs_are_empty() {
    assert!(
        tile_render::transcript_separator_rects("abc", &[], 0.0, 0.0, 300.0, 20.0, 1.0).is_empty()
    );
    assert!(
        tile_render::transcript_separator_rects("a\n\nb", &[2], 0.0, 0.0, 0.0, 20.0, 1.0)
            .is_empty(),
        "zero width → no rects"
    );
    assert!(
        tile_render::transcript_separator_rects("a\n\nb", &[2], 0.0, 0.0, 300.0, 20.0, 0.0)
            .is_empty(),
        "zero thickness → no rects"
    );
}

/// The `portal.divider.*` canonical tokens flow into the compositor's resolved
/// `markdown_tokens`, so separators render by default (owner "mini border").
#[test]
fn portal_divider_canonical_tokens_reach_markdown_tokens() {
    use std::collections::HashMap;
    // Simulate the canonical-resolved token map the runtime hands the compositor.
    let mut map = HashMap::new();
    map.insert("portal.divider.color".to_owned(), "#2A3344".to_owned());
    map.insert("portal.divider.thickness_px".to_owned(), "1".to_owned());
    let mt = crate::markdown::MarkdownTokens::from_token_map(&map);
    assert!(
        mt.separator_color.is_some(),
        "canonical divider color resolved"
    );
    assert_eq!(mt.separator_thickness_px, 1.0);
}

/// Turn-divider (`---` thematic-break) quads emitted by `render_node` must track
/// the tile's display scroll offset exactly like the text glyphs do (hud-6n9iv):
/// at a nonzero scroll the divider's pixel-top must equal its unscrolled top
/// minus the offset, and a divider scrolled above the tile top must be clipped
/// to the tile bounds (never painted outside the pane, matching the text viewport
/// clip). Drives the real `render_node` separator path (no readback).
#[tokio::test]
async fn transcript_divider_rects_track_tile_scroll_offset() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 400).await);

    // The separator path is gated on a live text rasterizer (the divider quads
    // ride the same `text_rasterizer.is_some()` branch as the glyphs). When the
    // headless environment has no font stack the rasterizer is absent and
    // `render_node` takes the placeholder fallback instead — skip rather than
    // assert against the wrong path. CI provisions fonts, so it runs there.
    if compositor.text_rasterizer.is_none() {
        eprintln!("skipping: no text rasterizer (separator path unavailable headless)");
        return;
    }

    // Portal divider tokens so the separator path emits quads at all.
    let mut map = std::collections::HashMap::new();
    map.insert("portal.divider.color".to_owned(), "#FF0000".to_owned());
    map.insert("portal.divider.thickness_px".to_owned(), "2".to_owned());
    compositor.set_token_map(map);

    let sw = 400.0_f32;
    let sh = 400.0_f32;
    let mut scene = SceneGraph::new(sw, sh);
    let tab_id = scene.create_tab("divider-scroll", 0).unwrap();
    let lease_id = scene.grant_lease("divider-scroll", 120_000);
    let tile_y = 30.0_f32;
    let tile_id = scene
        .create_tile(
            tab_id,
            "divider-scroll",
            lease_id,
            Rect::new(20.0, tile_y, 300.0, 300.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            tile_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(1200.0),
            },
        )
        .unwrap();

    // A transcript with one thematic break between two entries.
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "Entry A\n\n---\n\nEntry B".to_string(),
            bounds: Rect::new(0.0, 0.0, 300.0, 1200.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();
    compositor.prime_markdown_cache(&scene);

    let tile = scene.tiles.get(&tile_id).unwrap().clone();
    let root_id = tile.root_node.unwrap();

    // Recover the divider quad's top edge in pixel space from the emitted NDC
    // vertices. With `background: None` and no code tokens, the only quads
    // `render_node` emits for this node are the divider rects, so the maximum
    // NDC y (`top = 1 - 2*y/sh`) is the topmost divider's top edge.
    let divider_top_px = |scene: &SceneGraph| -> Option<f32> {
        let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
        let mut cmds = Vec::new();
        compositor.render_node(root_id, &tile, scene, &mut verts, &mut cmds, sw, sh);
        verts
            .iter()
            .map(|v| v.position[1])
            .fold(None, |acc: Option<f32>, y| {
                Some(acc.map_or(y, |a| a.max(y)))
            })
            .map(|top_ndc| (1.0 - top_ndc) * sh / 2.0)
    };

    // Unscrolled baseline.
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 0.0)
        .unwrap();
    let base = divider_top_px(&scene).expect("divider emitted at scroll 0");

    // A modest scroll keeps the divider inside the tile viewport: its top must
    // move up by exactly the scroll offset (tracking the glyphs).
    let scroll = 40.0_f32;
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, scroll)
        .unwrap();
    let scrolled = divider_top_px(&scene).expect("divider still visible after modest scroll");
    assert!(
        (base - scrolled - scroll).abs() < 0.5,
        "divider must track scroll: base={base}, scrolled={scrolled}, expected delta {scroll}"
    );

    // Scrolling the divider above the pane top must clip it out entirely — it may
    // not paint outside the tile bounds.
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 400.0)
        .unwrap();
    assert!(
        divider_top_px(&scene).is_none(),
        "divider scrolled above the tile top must be clipped, not painted outside the pane"
    );

    // Whole-portal resize: the divider must ride the SAME scaled line pitch the
    // glyphs are laid out with, so it stays glued to its entries instead of
    // detaching further down the transcript (hud-6n9iv). At a >1 font scale the
    // (line_index + 0.5) * line_height offset grows, so the unscrolled divider
    // top must move DOWN relative to the default-scale position. If the divider
    // ignored the scale (used the raw `tm.font_size_px`) the two would be equal —
    // this guards against reverting to the unscaled line height.
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 0.0)
        .unwrap();
    scene.set_tile_font_scale(tile_id, 1.5);
    let scaled_top = divider_top_px(&scene).expect("divider still emitted under resize");
    assert!(
        scaled_top > base + 5.0,
        "divider must track the scaled line pitch under portal resize: \
         base(scale 1.0)={base}, scaled(scale 1.5)={scaled_top}"
    );
}
