use super::*;

// ── Portal per-node streaming-reveal tracking (hud-tbdfx) ────────────────────
//
// These tests exercise `update_portal_tile_reveals`, which keys reveal state per
// `(tile, markdown-node)` so the tracker is robust to which node in a portal
// tile's subtree is "first eligible" from one frame to the next. They follow the
// existing `require_gpu!` + `prime_markdown_cache` idiom; the logic under test is
// pure CPU (no GPU draw) — the compositor is only needed for its markdown cache.
// The scene graph is flat (`Node::children` is `Vec<SceneId>`), so a tree is
// built with `add_node_to_tile` and mutated in place with `update_node_content`
// (node ids stay stable across "frames", exactly as the resident bridge's
// in-place content updates do).

/// hud-tbdfx helper: `TextMarkdown` node-data for a portal transcript node.
fn portal_reveal_md_data(
    content: &str,
    color_runs: Box<[tze_hud_scene::types::TextColorRun]>,
    top: f32,
) -> NodeData {
    NodeData::TextMarkdown(TextMarkdownNode {
        content: content.to_owned(),
        bounds: Rect::new(0.0, top, 256.0, 200.0),
        font_size_px: 14.0,
        font_family: FontFamily::SystemMonospace,
        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
        background: None,
        alignment: TextAlign::Start,
        overflow: TextOverflow::Clip,
        color_runs,
    })
}

/// hud-tbdfx helper: create a scrollable (portal) tile with a non-markdown
/// container root, returning `(tile_id, root_id)`.
fn portal_reveal_tile(scene: &mut SceneGraph) -> (SceneId, SceneId) {
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("portal", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, tze_hud_scene::types::TileScrollConfig::vertical())
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
                    color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    (tile_id, root_id)
}

/// hud-tbdfx helper: add a markdown child node under `parent_id` and return its
/// stable id.
fn portal_reveal_add_md(
    scene: &mut SceneGraph,
    tile_id: SceneId,
    parent_id: SceneId,
    content: &str,
    color_runs: Box<[tze_hud_scene::types::TextColorRun]>,
    top: f32,
) -> SceneId {
    let id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(parent_id),
            Node {
                layout: Default::default(),
                id,
                children: vec![],
                data: portal_reveal_md_data(content, color_runs, top),
            },
        )
        .unwrap();
    id
}

/// hud-tbdfx (red-first): a portal tile whose *first eligible* markdown node
/// changes between frames must NOT start a spurious word-by-word reveal of
/// already-settled content.
///
/// Reproduces the live tzehouse bug. The portal input tile carries a settled
/// history markdown node plus a composer draft node whose pixel-bearing color
/// runs toggle its eligibility. Under the old per-*tile* keying,
/// `update_portal_tile_reveals` tracked only the first-eligible node's
/// plain-text; when the draft node's runs flipped it from eligible to skipped,
/// the tracked text swapped from the short draft to the long history and was
/// mistaken for growth → the whole history re-revealed (~0.5s/word). Per-node
/// keying (hud-tbdfx) diffs each node only against its own prior snapshot.
#[tokio::test]
async fn test_portal_reveal_node_flip_does_not_spuriously_reveal() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(256.0, 256.0);
    let (tile_id, root_id) = portal_reveal_tile(&mut scene);

    // History is longer than the draft, so a per-tile tracker that swaps from the
    // draft's text to the history's text would see it as "growth". The draft node
    // is added FIRST so it is the "first eligible" node while it stays eligible.
    let draft_text = "hi";
    let history_text = "one two three four five six seven";
    let draft_id = portal_reveal_add_md(
        &mut scene,
        tile_id,
        root_id,
        draft_text,
        Box::default(),
        0.0,
    );
    let history_id = portal_reveal_add_md(
        &mut scene,
        tile_id,
        root_id,
        history_text,
        Box::default(),
        24.0,
    );

    // Frame 1: draft eligible (empty runs). Both nodes anchor settled.
    compositor.prime_markdown_cache(&scene);
    compositor.update_portal_tile_reveals(&scene);
    assert!(
        compositor
            .portal_tile_reveal_states
            .values()
            .all(|s| !s.is_revealing()),
        "first sight of every node must anchor settled, never revealing"
    );

    // Frame 2: the draft node gains a pixel-bearing color run → it becomes
    // INELIGIBLE, so the "first eligible" node flips to the (unchanged) history
    // node. Nothing may reveal.
    let pixel_run = tze_hud_scene::types::TextColorRun {
        start_byte: 0,
        end_byte: 1,
        color: Rgba::new(0.9, 0.1, 0.1, 1.0),
    };
    scene
        .update_node_content(
            tile_id,
            draft_id,
            portal_reveal_md_data(draft_text, Box::from([pixel_run]), 0.0),
        )
        .unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.update_portal_tile_reveals(&scene);

    assert!(
        compositor
            .portal_tile_reveal_states
            .values()
            .all(|s| !s.is_revealing()),
        "a first-eligible-node flip must NOT start a spurious reveal of settled history"
    );
    let hist = compositor
        .portal_tile_reveal_states
        .get(&(tile_id, history_id))
        .expect("history node must retain its reveal state across the draft flip");
    assert_eq!(
        hist.reveal_start,
        history_text.len(),
        "settled history reveal_start must equal its full length (nothing to fade)"
    );
}

/// hud-tbdfx (red-first): in a batched multi-node update where exactly one node
/// grows, only THAT node's appended suffix fades — the other nodes stay settled,
/// and the fade starts at the common prefix of the grown node's own prior text.
///
/// The old per-tile tracker only ever watched the first-eligible node, so growth
/// of any later node was silently missed (no reveal at all).
#[tokio::test]
async fn test_portal_reveal_batched_multinode_reveals_only_grown_node() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(256.0, 256.0);
    let (tile_id, root_id) = portal_reveal_tile(&mut scene);

    let a_id = portal_reveal_add_md(&mut scene, tile_id, root_id, "alpha", Box::default(), 0.0);
    let b_id = portal_reveal_add_md(&mut scene, tile_id, root_id, "beta", Box::default(), 24.0);

    // Frame 1: both eligible, both settled.
    compositor.prime_markdown_cache(&scene);
    compositor.update_portal_tile_reveals(&scene);

    // Frame 2: A unchanged, B grows "beta" → "beta gamma" (a genuine append).
    scene
        .update_node_content(
            tile_id,
            b_id,
            portal_reveal_md_data("beta gamma", Box::default(), 24.0),
        )
        .unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.update_portal_tile_reveals(&scene);

    let state_a = compositor
        .portal_tile_reveal_states
        .get(&(tile_id, a_id))
        .expect("unchanged node A must keep its reveal state");
    assert!(
        !state_a.is_revealing(),
        "the unchanged node must stay settled while a sibling grows"
    );

    let state_b = compositor
        .portal_tile_reveal_states
        .get(&(tile_id, b_id))
        .expect("grown node B must have reveal state");
    assert!(
        state_b.is_revealing(),
        "the grown node's appended suffix must fade in"
    );
    assert_eq!(
        state_b.reveal_start,
        "beta".len(),
        "the fade must start at the common prefix of node B's OWN prior text"
    );
}

/// hud-tbdfx regression guard: a single-node genuine append still reveals its
/// appended suffix (per-node keying must not disable legitimate reveals).
#[tokio::test]
async fn test_portal_reveal_single_node_append_still_reveals() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(256.0, 256.0);
    let (tile_id, root_id) = portal_reveal_tile(&mut scene);
    let node_id = portal_reveal_add_md(&mut scene, tile_id, root_id, "hello", Box::default(), 0.0);

    // Frame 1: settled.
    compositor.prime_markdown_cache(&scene);
    compositor.update_portal_tile_reveals(&scene);
    assert!(
        !compositor
            .portal_tile_reveal_states
            .get(&(tile_id, node_id))
            .expect("node must have reveal state")
            .is_revealing(),
        "first sight must be settled"
    );

    // Frame 2: genuine append "hello" → "hello world".
    scene
        .update_node_content(
            tile_id,
            node_id,
            portal_reveal_md_data("hello world", Box::default(), 0.0),
        )
        .unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.update_portal_tile_reveals(&scene);

    let state = compositor
        .portal_tile_reveal_states
        .get(&(tile_id, node_id))
        .expect("node must have reveal state after append");
    assert!(
        state.is_revealing(),
        "a single-node genuine append must still start a reveal"
    );
    assert_eq!(
        state.reveal_start,
        "hello".len(),
        "the reveal must fade only the appended suffix (start at the common prefix)"
    );
}

/// hud-g8xpg (review follow-up, red-first): when a portal tile carries two
/// eligible markdown nodes with the SAME plain-text and only one of them is
/// revealing, the fade must route by node identity — the settled sibling with
/// identical text must stay fully opaque, not inherit the revealing node's
/// partial-alpha suffix.
///
/// Regression for the tile-wide, plain-text-matched reveal post-pass:
/// `apply_portal_reveal_fade` guarded solely on `item.text == reveal.plain_text`,
/// so a reveal anchored to one node dimmed EVERY same-text `TextItem` in the tile
/// (and, with two same-text reveals in flight, which fade won was
/// non-deterministic in `HashMap` iteration order). Routing the fade by
/// `(tile, node)` at collection time fixes both.
#[tokio::test]
async fn test_portal_reveal_identical_text_nodes_do_not_cross_fade() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(256.0, 256.0);
    let (tile_id, root_id) = portal_reveal_tile(&mut scene);

    // settled node already shows "ok"; grow node will grow "o" -> "ok" so it
    // reveals while ending up with the SAME plain-text as the settled node.
    let settled_id = portal_reveal_add_md(&mut scene, tile_id, root_id, "ok", Box::default(), 0.0);
    let grow_id = portal_reveal_add_md(&mut scene, tile_id, root_id, "o", Box::default(), 24.0);

    // Frame 1: both settled.
    compositor.prime_markdown_cache(&scene);
    compositor.update_portal_tile_reveals(&scene);

    // Frame 2: grow node "o" -> "ok" (now identical plain-text to the settled node).
    scene
        .update_node_content(
            tile_id,
            grow_id,
            portal_reveal_md_data("ok", Box::default(), 24.0),
        )
        .unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.update_portal_tile_reveals(&scene);

    // Precondition: only the grown node is revealing; both track "ok".
    assert!(
        !compositor
            .portal_tile_reveal_states
            .get(&(tile_id, settled_id))
            .expect("settled node state")
            .is_revealing(),
        "settled sibling must not be revealing"
    );
    assert!(
        compositor
            .portal_tile_reveal_states
            .get(&(tile_id, grow_id))
            .expect("grown node state")
            .is_revealing(),
        "grown node must be revealing"
    );

    // Render: collecting text items applies the reveal fade.
    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    let ok_items: Vec<&crate::text::TextItem> =
        items.iter().filter(|it| it.text.as_ref() == "ok").collect();
    assert_eq!(
        ok_items.len(),
        2,
        "expected one TextItem per 'ok' node, got {}",
        ok_items.len()
    );

    // The settled node sits at top=0, the grown node at top=24; route by pixel_y.
    let settled_item = ok_items
        .iter()
        .min_by(|a, b| a.pixel_y.total_cmp(&b.pixel_y))
        .unwrap();
    let grown_item = ok_items
        .iter()
        .max_by(|a, b| a.pixel_y.total_cmp(&b.pixel_y))
        .unwrap();

    let min_run_alpha = |it: &crate::text::TextItem| -> u8 {
        it.styled_runs
            .iter()
            .filter_map(|r| r.color.map(|c| c[3]))
            .min()
            .unwrap_or(255)
    };

    // The settled sibling must be fully opaque — it must NOT inherit the grown
    // node's fade just because it lays out the same "ok" text.
    assert_eq!(
        min_run_alpha(settled_item),
        255,
        "settled same-text sibling must stay fully opaque, not cross-fade"
    );

    // The grown node must still fade its appended suffix (reveal not disabled).
    assert!(
        min_run_alpha(grown_item) < 255,
        "the grown node's appended suffix must fade in"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Multi-node portal-part layout (hud-s4lrw)
//
// The compositor consumes the first-class `PortalSurface` descriptor (PR #1092)
// and renders each declared part node distinctly: only the `Transcript` part
// receives transcript overflow treatment (optimal-measure clamp + tail-anchored
// ellipsis), and each part node is clipped to its own band so one part's
// overflow can never paint over a sibling's region. These first two tests are
// pure CPU (no GPU) — they exercise the classification + index helpers directly.
// ─────────────────────────────────────────────────────────────────────────────

/// A tile that declares a `PortalSurface` yields a node→part index over the
/// *materialized* parts only, and `node_gets_transcript_treatment` classifies
/// each node by its declared kind (Transcript → true; every other kind and any
/// undeclared node → false). With no index (legacy single-node portal), every
/// node inherits transcript treatment so pre-promotion behavior is unchanged.
#[test]
fn portal_part_index_classifies_parts_and_falls_back() {
    use tze_hud_scene::types::{PortalPart, PortalPartKind, PortalSurface};

    let mut scene = SceneGraph::new(800.0, 600.0);
    let tab = scene.create_tab("t", 0).unwrap();
    let lease = scene.grant_lease("ns", 120_000);
    let tile = scene
        .create_tile(tab, "ns", lease, Rect::new(0.0, 0.0, 300.0, 200.0), 1)
        .unwrap();

    // Helper: resolve a node's effective part (no inherited ancestor) and ask
    // whether it gets transcript treatment — mirrors the collector call shape.
    let treats_as_transcript = |parts: Option<&[PortalPart]>, node: SceneId| {
        let ep = text::resolve_effective_part(parts, node, None);
        text::node_gets_transcript_treatment(parts, ep)
    };

    // No surface declared → index is None (legacy path).
    assert!(
        text::portal_part_index(&scene, tile).is_none(),
        "no surface → no part index (legacy tile-level behavior)"
    );
    // …and with no index every node is treated as a transcript.
    assert!(treats_as_transcript(None, SceneId::new()));

    let transcript_node = SceneId::new();
    let composer_node = SceneId::new();
    let surface = PortalSurface {
        parts: vec![
            PortalPart {
                kind: PortalPartKind::Transcript,
                bounds: Rect::new(0.0, 0.0, 300.0, 150.0),
                node: Some(transcript_node),
            },
            PortalPart {
                kind: PortalPartKind::Composer,
                bounds: Rect::new(0.0, 150.0, 300.0, 50.0),
                node: Some(composer_node),
            },
            // Geometry-only divider with no materialized node — carried in the
            // slice but never matches a node lookup.
            PortalPart {
                kind: PortalPartKind::Divider,
                bounds: Rect::new(0.0, 148.0, 300.0, 2.0),
                node: None,
            },
        ],
        ..Default::default()
    };
    scene.overlay.portal_surfaces.insert(tile, surface);

    let parts = text::portal_part_index(&scene, tile).expect("surface present → parts");
    assert_eq!(
        parts.len(),
        3,
        "the slice carries every declared part; node:None parts simply never match"
    );
    assert!(
        treats_as_transcript(Some(parts), transcript_node),
        "the Transcript part receives transcript treatment"
    );
    assert!(
        !treats_as_transcript(Some(parts), composer_node),
        "the Composer part must NOT tail-follow / clamp like the transcript"
    );
    assert!(
        !treats_as_transcript(Some(parts), SceneId::new()),
        "a node not declared as any part is not a transcript under a surface"
    );

    // Codex P2 (PR #1099): a part whose `node` is a container scopes its whole
    // subtree — a descendant text node (not itself a declared part) inherits the
    // ancestor Transcript part and still gets transcript treatment + clip band.
    let transcript_part = parts
        .iter()
        .find(|p| p.node == Some(transcript_node))
        .copied()
        .unwrap();
    let descendant = SceneId::new();
    let inherited = text::resolve_effective_part(Some(parts), descendant, Some(&transcript_part));
    assert!(
        text::node_gets_transcript_treatment(Some(parts), inherited),
        "a descendant of the Transcript container part inherits transcript treatment"
    );
}

/// A surface whose parts are all geometry-only (no materialized nodes) produces
/// no text-part index, so overflow/clip treatment keeps the legacy tile-level
/// fallback rather than using a spurious empty map. Its display translation is
/// separately resolved from the declared part bounds (hud-yrcev).
#[test]
fn portal_part_index_none_when_no_materialized_nodes() {
    use tze_hud_scene::types::{PortalPart, PortalPartKind, PortalSurface};

    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab = scene.create_tab("t", 0).unwrap();
    let lease = scene.grant_lease("ns", 120_000);
    let tile = scene
        .create_tile(tab, "ns", lease, Rect::new(0.0, 0.0, 200.0, 120.0), 1)
        .unwrap();

    let surface = PortalSurface {
        parts: vec![
            PortalPart {
                kind: PortalPartKind::Frame,
                bounds: Rect::new(0.0, 0.0, 200.0, 120.0),
                node: None,
            },
            PortalPart {
                kind: PortalPartKind::Divider,
                bounds: Rect::new(0.0, 60.0, 200.0, 2.0),
                node: None,
            },
        ],
        ..Default::default()
    };
    scene.overlay.portal_surfaces.insert(tile, surface);

    assert!(
        text::portal_part_index(&scene, tile).is_none(),
        "a surface with no materialized part nodes → None (legacy fallback)"
    );
}

/// End-to-end (software-GPU) proof that `collect_text_items` consumes the
/// `PortalSurface` per part: a portal tile whose root is the transcript node and
/// whose child is a bounded composer node, both `Ellipsis`, at-tail.
///
/// Asserts the two render-side promotion invariants (hud-s4lrw):
/// 1. **Per-part overflow scope** — only the declared `Transcript` part gets the
///    tail-anchored viewport; the `Composer` part (same overflow mode) stays
///    head-anchored, so a bounded composer never inherits the transcript's
///    tail-follow. This is what unblocks a precisely-bounded composer color run
///    (hud-9gyao) instead of a whole-tile zero-length sentinel.
/// 2. **Per-part clip containment** — the transcript's tall content is clipped
///    to its own 150px band, NOT the full 200px tile, so it can never paint over
///    the composer strip beneath it. The composer is clipped to its own band.
#[tokio::test]
async fn portal_surface_renders_parts_with_per_part_scope_and_clip() {
    use tze_hud_scene::types::{PortalPart, PortalPartKind, PortalSurface};

    // collect_text_items is a CPU path but building the Compositor needs the GPU
    // text renderer (matches the sibling at-tail test).
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(640, 480).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let tile_w = 300.0_f32;
    let tile_h = 200.0_f32;
    // Transcript band = top 150px; composer band = bottom 50px.
    let transcript_band_h = 150.0_f32;
    let composer_band_y = 150.0_f32;
    let composer_band_h = 50.0_f32;

    let mut scene = SceneGraph::new(640.0, 480.0);
    let tab = scene.create_tab("test", 0).unwrap();
    let lease = scene.grant_lease("portal", 120_000);
    let tile = scene
        .create_tile(tab, "portal", lease, Rect::new(0.0, 0.0, tile_w, tile_h), 1)
        .unwrap();
    // Scrollable surface (portal token scope + at-tail machinery).
    let _ =
        scene.register_tile_scroll_config(tile, tze_hud_scene::types::TileScrollConfig::vertical());

    // Transcript root: many lines, taller than its band → overflow.
    let transcript_content = "Line A\nLine B\nLine C\nLine D\nLine E\nLine F\nLine G\nLine H";
    let transcript_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: transcript_content.to_owned(),
            // Content box is tall (spans past the band) so the clip, not the
            // layout, is what bounds it to the band.
            bounds: Rect::new(0.0, 0.0, tile_w, 400.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    let transcript_id = transcript_node.id;
    scene.set_tile_root(tile, transcript_node).unwrap();

    // Composer child: bounded strip at the bottom, same Ellipsis overflow.
    let composer_content = "draft reply text";
    let composer_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: composer_content.to_owned(),
            bounds: Rect::new(0.0, composer_band_y, tile_w, composer_band_h),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    let composer_id = composer_node.id;
    scene
        .add_node_to_tile(tile, Some(transcript_id), composer_node)
        .unwrap();

    // Declare the first-class surface mapping each part to its node + band.
    let surface = PortalSurface {
        parts: vec![
            PortalPart {
                kind: PortalPartKind::Transcript,
                bounds: Rect::new(0.0, 0.0, tile_w, transcript_band_h),
                node: Some(transcript_id),
            },
            PortalPart {
                kind: PortalPartKind::Composer,
                bounds: Rect::new(0.0, composer_band_y, tile_w, composer_band_h),
                node: Some(composer_id),
            },
        ],
        ..Default::default()
    };
    scene.overlay.portal_surfaces.insert(tile, surface);

    // At tail so the transcript would tail-anchor.
    scene.set_tile_follow_tail_at_tail(tile, true);
    compositor.prime_markdown_cache(&scene);

    let items = compositor.collect_text_items(&scene, 640.0, 480.0);
    let transcript_item = items
        .iter()
        .find(|it| it.text.contains("Line A"))
        .expect("transcript TextItem present");
    let composer_item = items
        .iter()
        .find(|it| it.text.contains("draft reply text"))
        .expect("composer TextItem present");

    // 1a. The transcript part tail-anchors.
    assert_eq!(
        transcript_item.viewport,
        crate::overflow::TruncationViewport::TailAnchored,
        "the Transcript part must tail-anchor at tail"
    );
    // 1b. The composer part — same Ellipsis overflow — stays head-anchored.
    assert_eq!(
        composer_item.viewport,
        crate::overflow::TruncationViewport::HeadAnchored,
        "the Composer part must NOT inherit the transcript's tail-follow"
    );

    // 2a. The tall transcript is clipped to the BOTTOM of its band, not the
    //     bottom of the tile — so it cannot paint over the composer strip. (The
    //     clip top may sit a few px in from the band top due to content inset;
    //     the containment invariant is the band bottom, which equals the
    //     composer band start.) Without the per-part clip this bottom would be
    //     the tile bottom (200), overlapping the composer.
    let _ = transcript_band_h;
    let transcript_clip_bottom = transcript_item.clip_pixel_y + transcript_item.clip_bounds_height;
    assert!(
        (transcript_clip_bottom - composer_band_y).abs() < 0.5,
        "transcript clip bottom {transcript_clip_bottom} must sit at its band edge \
         {composer_band_y} (the composer band start), not the tile bottom {tile_h}"
    );
    assert!(
        transcript_clip_bottom < tile_h,
        "transcript clip must be contained to its part band, not the whole tile"
    );
    // 2b. The composer is clipped to its own band.
    assert!(
        composer_item.clip_pixel_y >= composer_band_y - 0.5,
        "composer clip top {} must sit at/below its band start {}",
        composer_item.clip_pixel_y,
        composer_band_y
    );
    assert!(
        composer_item.clip_pixel_y + composer_item.clip_bounds_height <= tile_h + 0.5,
        "composer clip must stay within the tile"
    );
}

/// End-to-end (software-GPU) proof of the container-part scope propagation
/// (hud-s4lrw, PR #1099 Codex P2): a `Transcript` part whose `node` is a
/// `SolidColor` *container* scopes its whole subtree. The transcript's actual
/// text lives in a `TextMarkdown` **child** that is not itself a declared part;
/// it must still tail-anchor and clip to the transcript band by inheriting the
/// container part's scope through the recursion.
#[tokio::test]
async fn portal_surface_container_part_scopes_descendant_text() {
    use tze_hud_scene::types::{PortalPart, PortalPartKind, PortalSurface, SolidColorNode};

    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(640, 480).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let tile_w = 300.0_f32;
    let tile_h = 200.0_f32;
    let band_h = 150.0_f32; // transcript band = top 150px of a 200px tile.

    let mut scene = SceneGraph::new(640.0, 480.0);
    let tab = scene.create_tab("test", 0).unwrap();
    let lease = scene.grant_lease("portal", 120_000);
    let tile = scene
        .create_tile(tab, "portal", lease, Rect::new(0.0, 0.0, tile_w, tile_h), 1)
        .unwrap();
    let _ =
        scene.register_tile_scroll_config(tile, tze_hud_scene::types::TileScrollConfig::vertical());

    // Container root (the declared Transcript part node) — geometry-only.
    let container = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.0, 0.0, 0.0, 1.0),
            bounds: Rect::new(0.0, 0.0, tile_w, band_h),
            radius: None,
        }),
    };
    let container_id = container.id;
    scene.set_tile_root(tile, container).unwrap();

    // Transcript text lives in a CHILD of the container, not a declared part.
    let text_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "Line A\nLine B\nLine C\nLine D\nLine E\nLine F\nLine G\nLine H".to_owned(),
            bounds: Rect::new(0.0, 0.0, tile_w, 400.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    scene
        .add_node_to_tile(tile, Some(container_id), text_node)
        .unwrap();

    // The Transcript part points at the CONTAINER, not the text node.
    scene.overlay.portal_surfaces.insert(
        tile,
        PortalSurface {
            parts: vec![PortalPart {
                kind: PortalPartKind::Transcript,
                bounds: Rect::new(0.0, 0.0, tile_w, band_h),
                node: Some(container_id),
            }],
            ..Default::default()
        },
    );
    scene.set_tile_follow_tail_at_tail(tile, true);
    compositor.prime_markdown_cache(&scene);

    let items = compositor.collect_text_items(&scene, 640.0, 480.0);
    let text_item = items
        .iter()
        .find(|it| it.text.contains("Line A"))
        .expect("descendant transcript TextItem present");

    // Inherited transcript scope: tail-anchored despite not being a declared part.
    assert_eq!(
        text_item.viewport,
        crate::overflow::TruncationViewport::TailAnchored,
        "a descendant of the container Transcript part must inherit tail-follow"
    );
    // Inherited clip band: clipped to the 150px band bottom, not the tile bottom.
    let clip_bottom = text_item.clip_pixel_y + text_item.clip_bounds_height;
    assert!(
        (clip_bottom - band_h).abs() < 0.5,
        "descendant clip bottom {clip_bottom} must sit at the inherited band bottom {band_h}"
    );
    assert!(
        clip_bottom < tile_h,
        "descendant transcript must be contained to the inherited band, not the whole tile"
    );
}
