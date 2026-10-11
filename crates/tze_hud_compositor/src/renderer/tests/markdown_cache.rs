use super::*;

// ── hud-gpqde: markdown prime instrumentation and node_key_cache ─────────

/// MarkdownCache::compute_key is deterministic and content-addressed: same
/// content produces the same BLAKE3 key; distinct content produces distinct
/// keys; get_by_key returns the parsed entry after prime.
///
/// CPU-only cache key and lookup behavior; this does not construct a compositor.
#[test]
fn markdown_cache_compute_key_is_deterministic_and_content_addressed() {
    let content_a = "# Hello\n\nThis is **bold** text.";
    let content_b = "Plain text with `code`.";

    // The key includes token-set identity as well as content (hud-3ryie).
    let tokens = crate::markdown::MarkdownTokens::default();
    let expected_key_a = crate::markdown::MarkdownCache::compute_key(content_a, &tokens);
    let expected_key_b = crate::markdown::MarkdownCache::compute_key(content_b, &tokens);

    // Verify that compute_key is deterministic (same content + tokens → same key).
    assert_eq!(
        expected_key_a,
        crate::markdown::MarkdownCache::compute_key(content_a, &tokens),
        "compute_key must be deterministic"
    );

    // Verify that distinct content produces distinct keys.
    assert_ne!(
        expected_key_a, expected_key_b,
        "distinct content must produce distinct keys"
    );

    // Verify that the cache hit path returns the same data as compute_key.
    let mut cache = crate::markdown::MarkdownCache::new();
    cache.prime(content_a, &tokens);
    assert!(
        cache.get_by_key(&expected_key_a).is_some(),
        "get_by_key must find content after prime"
    );
    assert!(
        cache.get(content_a, &tokens).is_some(),
        "get must also find content after prime"
    );
}

/// Per-tile markdown scoping (hud-3ryie): `portal_markdown_node_ids` classifies
/// a markdown node under a scrollable (portal) tile as portal-scoped, while a
/// node under a non-scrollable tile is NOT — so the compositor selects the
/// portal token set only for the governed portal surface and the generic set
/// everywhere else.  GPU-free: exercises the classifier directly.
#[test]
fn portal_markdown_node_ids_scopes_by_scroll_config() {
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow, TileScrollConfig,
    };

    fn md_node(id: SceneId, content: &str) -> Node {
        Node {
            layout: Default::default(),
            id,
            children: vec![],
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: content.to_string(),
                bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                font_size_px: 14.0,
                font_family: FontFamily::SystemSansSerif,
                color: tze_hud_scene::types::Rgba::new(1.0, 1.0, 1.0, 1.0),
                background: None,
                alignment: TextAlign::Start,
                overflow: TextOverflow::Clip,
                color_runs: Box::default(),
            }),
        }
    }

    let mut scene = SceneGraph::new(512.0, 256.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);

    // Portal tile: has a scroll config → governed portal surface.
    let portal_node_id = SceneId::new();
    let portal_tile = scene
        .create_tile(
            tab_id,
            "portal",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene
        .set_tile_root(portal_tile, md_node(portal_node_id, "# Portal"))
        .unwrap();
    scene
        .register_tile_scroll_config(portal_tile, TileScrollConfig::vertical())
        .unwrap();

    // Plain tile: NO scroll config → non-portal markdown surface.
    let plain_node_id = SceneId::new();
    let plain_tile = scene
        .create_tile(
            tab_id,
            "plain",
            lease_id,
            Rect::new(256.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene
        .set_tile_root(plain_tile, md_node(plain_node_id, "# Plain"))
        .unwrap();

    let portal_ids = super::portal_markdown_node_ids(&scene);

    assert!(
        portal_ids.contains(&portal_node_id),
        "a node under a scrollable (portal) tile must be portal-scoped"
    );
    assert!(
        !portal_ids.contains(&plain_node_id),
        "a node under a non-scrollable tile must NOT be portal-scoped, so \
         portal.transcript.* preferences cannot reach it"
    );
}

/// Verify the commit-time prime contract (hud-380dl, Option A):
///
/// When `prime_markdown_cache` is called BEFORE `render_frame_headless`
/// (as the runtime now does at Stage 4 commit time), the render-frame path
/// finds the cache already populated and contributes 0 parse cost.
///
/// Specifically, `render_frame_headless` MUST NOT increment
/// `markdown_cache_scene_version` relative to the version set by the
/// commit-time prime — meaning the cache-miss fallback in render_frame_headless
/// must not fire, and the scene version sentinel must remain equal to
/// `scene.version` after the prime.
///
/// This is the canonical Layer 0 assertion for the commit-time prime contract.
#[tokio::test]
async fn render_frame_headless_is_parse_free_after_commit_time_prime() {
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
    };

    let gpu = make_compositor_and_surface(64, 64).await;
    if std::env::var("TZE_HUD_REQUIRE_GPU").ok().as_deref() == Some("1") {
        assert!(gpu.is_some(), "this parent requires a real GPU constructor");
    }
    let (mut compositor, surface) = require_gpu!(gpu);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let content = "# Commit-time prime test\n\n**bold** and *italic*.";

    let node_id = SceneId::new();
    let node = Node {
        layout: Default::default(),
        id: node_id,
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_string(),
            bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
            font_size_px: 12.0,
            font_family: FontFamily::SystemSansSerif,
            color: tze_hud_scene::types::Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };

    let mut scene = scene_with_node(node);

    // ── Commit-time prime (mimics Stage 4 runtime behavior) ───────────────
    // Before render_frame_headless runs, the runtime primes the cache.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    // After commit-time prime: the cache scene version sentinel must equal
    // scene.version.  This is the invariant that render_frame_headless checks
    // to confirm it is parse-free.
    assert_eq!(
        compositor.markdown_cache_scene_version, scene.version,
        "after commit-time prime, markdown_cache_scene_version must match scene.version"
    );

    // ── Render frame — must be parse-free ────────────────────────────────
    // render_frame_headless checks `scene.version != markdown_cache_scene_version`
    // and finds them equal → no parse occurs → the cache-miss fallback is NOT
    // triggered.  The scene version sentinel is not modified by render_frame_headless.
    let parses_before_frame =
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get());
    let _telemetry = compositor.render_frame_headless(&mut scene, &surface);
    assert_eq!(
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get())
            - parses_before_frame,
        0,
        "the first commit-primed frame must perform no current-thread markdown parsing"
    );

    // After render: the sentinel must still equal scene.version (render_frame_headless
    // must NOT have re-primed or changed the sentinel as a side-effect of rendering).
    assert_eq!(
        compositor.markdown_cache_scene_version, scene.version,
        "render_frame_headless must not alter markdown_cache_scene_version \
             when cache was already commit-primed"
    );

    // The node_key_cache populated at commit-time must still be intact after render.
    assert_eq!(
        compositor.node_key_cache.len(),
        1,
        "node_key_cache populated by commit-time prime must survive render_frame_headless"
    );
    assert!(
        compositor.node_key_cache.contains_key(&node_id),
        "node_key_cache must contain the primed node after render_frame_headless"
    );

    // ── Second frame — unchanged scene, still parse-free ─────────────────
    // Rendering the same scene a second time must also be parse-free.
    let scene_version_before = scene.version;
    let parses_before_second_frame =
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get());
    let _telemetry2 = compositor.render_frame_headless(&mut scene, &surface);
    assert_eq!(
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get())
            - parses_before_second_frame,
        0,
        "the second unchanged frame must perform no current-thread markdown parsing"
    );
    assert_eq!(
        compositor.markdown_cache_scene_version, scene_version_before,
        "second render of unchanged scene must not change markdown_cache_scene_version"
    );

    // A real content mutation and small inline commit prime prove that this
    // current-thread counter can observe actual parse work, not just zeroes.
    let mut changed = match &scene.nodes.get(&node_id).unwrap().data {
        NodeData::TextMarkdown(node) => node.clone(),
        _ => panic!("the retained fixture must contain a TextMarkdown node"),
    };
    changed.content = "Updated **bold** content".to_owned();
    let tile_id = scene
        .tiles
        .values()
        .find(|tile| tile.root_node == Some(node_id))
        .unwrap()
        .id;
    let version_before_change = scene.version;
    scene
        .update_node_content_checked(tile_id, node_id, NodeData::TextMarkdown(changed), "test")
        .unwrap();
    assert!(scene.version > version_before_change);
    let parses_before_commit =
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get());
    compositor.prime_markdown_cache(&scene);
    assert!(
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get())
            > parses_before_commit,
        "changed content must actually parse during the small commit prime"
    );
    compositor.prime_truncation_cache(&scene);
    assert_eq!(compositor.markdown_cache_scene_version, scene.version);
    let parses_before_changed_frame =
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get());
    let _changed_telemetry = compositor.render_frame_headless(&mut scene, &surface);
    let changed_items = compositor.collect_text_items(&scene, 64.0, 64.0);
    assert_eq!(changed_items.len(), 1);
    assert_eq!(&*changed_items[0].text, "Updated bold content");
    assert!(changed_items[0].styled_runs.iter().any(|run| {
        run.start_byte <= 8
            && run.end_byte >= 12
            && run.weight.map(|weight| weight >= 700).unwrap_or(false)
    }));
    assert_eq!(
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get())
            - parses_before_changed_frame,
        0,
        "changed content is already primed before its real frame and collection"
    );
}

/// Cache-miss fallback produces non-lossy styled output (hud-xcp9b, spec task 2.2).
///
/// When the markdown cache is cold (no prior prime) and a node with
/// `color_runs.is_empty()` is rendered, the renderer's fallback path must
/// NOT use the lossy `strip_markdown_v1` path.  Instead it must call
/// `parse_markdown_subset` inline and produce `styled_runs` that encode
/// the markdown structure.
///
/// Retain direct parser/adapter controls, then verify the real cold collector:
///  - `TextItem::text` equals the non-lossy plain text from `parse_markdown_subset`
///    for the same content (not the output of `strip_markdown_v1`).
///  - `TextItem::styled_runs` is non-empty for content that contains
///    markdown constructs (e.g. `**bold**` → at least one bold run).
///
/// Parser-entry deltas cover only synchronous work on this test thread.
#[tokio::test]
async fn markdown_cache_miss_fallback_is_non_lossy() {
    use tze_hud_scene::types::{FontFamily, Rect, Rgba, TextAlign, TextMarkdownNode, TextOverflow};

    // Content with markdown constructs that distinguish the lossy path from
    // the non-lossy path:
    //  - `strip_markdown_v1` would produce "Hello bold world" (strips ** and #)
    //  - `parse_markdown_subset` would produce "Hello bold world" in `plain_text`
    //    AND a bold StyledSpan covering "bold".
    let content = "Hello **bold** world";

    let node = TextMarkdownNode {
        content: content.to_owned(),
        bounds: Rect::new(0.0, 0.0, 200.0, 50.0),
        font_size_px: 12.0,
        font_family: FontFamily::SystemSansSerif,
        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
        background: None,
        alignment: TextAlign::Start,
        overflow: TextOverflow::Clip,
        color_runs: Box::default(), // empty: this is the cache-miss path
    };

    // Construct a cold (empty) markdown cache and token set — simulates the
    // first-frame-before-any-prime scenario.
    let cold_cache = crate::markdown::MarkdownCache::new();
    let tokens = crate::markdown::MarkdownTokens::default();

    // Verify the cache is cold (no entry for this content).
    let content_key = crate::markdown::MarkdownCache::compute_key(content, &tokens);
    assert!(
        cold_cache.get_by_key(&content_key).is_none(),
        "cache must be cold before the test"
    );

    // Invoke the non-lossy inline-parse path directly (mirrors what the
    // renderer does on a cache miss after hud-xcp9b).
    let parsed = crate::markdown::parse_markdown_subset(content, &tokens);
    let item = crate::text::TextItem::from_text_markdown_cached(&node, 0.0, 0.0, &parsed);

    // The plain text must be the non-lossy form — same for both paths in
    // this example, but `styled_runs` must be non-empty to distinguish
    // from the lossy strip path.
    assert_eq!(
        &*item.text, "Hello bold world",
        "non-lossy fallback must produce plain text without markdown syntax"
    );

    // The non-lossy path must produce styled runs encoding markdown structure.
    // The lossy strip_markdown_v1 path produces no styled_runs at all.
    assert!(
        !item.styled_runs.is_empty(),
        "non-lossy cache-miss fallback must produce styled_runs for markdown content \
             (lossy strip_markdown_v1 would leave styled_runs empty)"
    );

    // At least one run must be bold (weight >= 700) covering "bold".
    let has_bold_run = item
        .styled_runs
        .iter()
        .any(|r| r.weight.map(|w| w >= 700).unwrap_or(false));
    assert!(
        has_bold_run,
        "non-lossy fallback must produce a bold styled run for **bold** markdown syntax"
    );

    let gpu = make_compositor_and_surface(256, 256).await;
    if std::env::var("TZE_HUD_REQUIRE_GPU").ok().as_deref() == Some("1") {
        assert!(gpu.is_some(), "this parent requires a real GPU constructor");
    }
    let (compositor, _surface) = require_gpu!(gpu);
    let scene = scene_with_node(Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: tze_hud_scene::types::NodeData::TextMarkdown(node),
    });
    let actual_key =
        crate::markdown::MarkdownCache::compute_key(content, &compositor.markdown_tokens_generic);
    assert!(
        compositor
            .markdown_cache()
            .get_by_key(&actual_key)
            .is_none()
    );
    let misses_before = compositor.markdown_cache_miss_count();
    let parses_before = crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get());
    let first = compositor.collect_text_items(&scene, 256.0, 256.0);
    let parses_after_first = crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get());
    assert_eq!(first.len(), 1);
    assert_eq!(&*first[0].text, "Hello bold world");
    assert!(!first[0].styled_runs.is_empty());
    assert!(first[0].styled_runs.iter().any(|run| {
        run.start_byte <= 6
            && run.end_byte >= 10
            && run.weight.map(|weight| weight >= 700).unwrap_or(false)
    }));
    assert_eq!(parses_after_first - parses_before, 1);
    assert_eq!(compositor.markdown_cache_miss_count() - misses_before, 1);

    let parses_before_second =
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get());
    let second = compositor.collect_text_items(&scene, 256.0, 256.0);
    let parses_after_second =
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.get());
    assert_eq!(second.len(), 1);
    assert_eq!(&*second[0].text, "Hello bold world");
    assert!(!second[0].styled_runs.is_empty());
    assert!(second[0].styled_runs.iter().any(|run| {
        run.start_byte <= 6
            && run.end_byte >= 10
            && run.weight.map(|weight| weight >= 700).unwrap_or(false)
    }));
    assert_eq!(parses_after_second - parses_before_second, 0);
    // The authoritative snapshot still misses; its miss counter is not a parse counter.
    assert_eq!(compositor.markdown_cache_miss_count() - misses_before, 2);
    assert!(
        compositor
            .markdown_cache()
            .get_by_key(&actual_key)
            .is_none()
    );
}

/// Verify the commit-time truncation cache prime contract (hud-v2z6u):
///
/// When `prime_truncation_cache` is called BEFORE `render_frame_headless`
/// (as the runtime now does at Stage 4 commit time), the render-frame path
/// finds the cache already populated and `truncation_cache_scene_version`
/// equals `scene.version`, so the safety-fallback branch is NOT triggered.
///
/// After rendering the frame, the sentinel must still equal `scene.version`
/// — confirming that `render_frame_headless` did not re-prime the cache.
///
/// GPU required; skips gracefully when no adapter is available.
#[tokio::test]
async fn prime_truncation_cache_is_commit_primed_before_render_frame_headless() {
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
    };

    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(64, 64).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let content = "The quick brown fox jumps over the lazy dog.".repeat(4);

    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.clone(),
            bounds: Rect::new(0.0, 0.0, 64.0, 16.0),
            font_size_px: 12.0,
            font_family: FontFamily::SystemMonospace,
            color: tze_hud_scene::types::Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };

    let mut scene = scene_with_node(node);

    // ── Commit-time prime (mimics Stage 4 runtime behavior) ──────────────
    // Also prime markdown cache so render_frame_headless doesn't hit its
    // own safety-fallback for markdown (orthogonal to this test).
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    // After commit-time prime: sentinel must equal scene.version.
    assert_eq!(
        compositor.truncation_cache_scene_version, scene.version,
        "after commit-time prime, truncation_cache_scene_version must match scene.version \
             [hud-v2z6u]"
    );

    // ── Render frame — must not re-prime truncation cache ─────────────────
    // render_frame_headless checks `scene.version != truncation_cache_scene_version`
    // and finds them equal → no re-prime occurs → sentinel unchanged.
    let _telemetry = compositor.render_frame_headless(&mut scene, &surface);

    // After render: sentinel must still equal scene.version (render_frame_headless
    // must NOT have altered truncation_cache_scene_version when cache was commit-primed).
    assert_eq!(
        compositor.truncation_cache_scene_version, scene.version,
        "render_frame_headless must not alter truncation_cache_scene_version \
             when cache was already commit-primed [hud-v2z6u]"
    );
}

/// The mid-resize re-prime cadence gate bounds re-prime cost while still
/// picking up the settled geometry: a scene change inside the interval is
/// deferred (marker unchanged), and once the interval has elapsed the next
/// prime advances to the latest scene version. Time is controlled by
/// setting `resize_reprime_last_at` (future = inside the interval, far past =
/// elapsed) so there are no sleeps and no wall-clock races.
#[tokio::test]
async fn resize_reprime_cadence_defers_inside_interval_then_primes_settled_geometry() {
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
    };

    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(64, 64).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "The quick brown fox jumps over the lazy dog.".repeat(4),
            bounds: Rect::new(0.0, 0.0, 64.0, 16.0),
            font_size_px: 12.0,
            font_family: FontFamily::SystemMonospace,
            color: tze_hud_scene::types::Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);

    // First prime ever (no prior timestamp) is never deferred.
    compositor.prime_truncation_cache(&scene);
    assert_eq!(compositor.truncation_cache_scene_version, scene.version);
    let primed_version = scene.version;

    // A geometry change inside the interval is deferred.
    scene.version += 1;
    compositor.resize_reprime_last_at =
        Some(std::time::Instant::now() + std::time::Duration::from_secs(3600));
    compositor.prime_truncation_cache(&scene);
    assert_eq!(
        compositor.truncation_cache_scene_version, primed_version,
        "re-prime inside the cadence interval must be deferred"
    );

    // Once the interval has elapsed the settled geometry is primed.
    compositor.resize_reprime_last_at =
        std::time::Instant::now().checked_sub(std::time::Duration::from_secs(3600));
    compositor.prime_truncation_cache(&scene);
    assert_eq!(
        compositor.truncation_cache_scene_version, scene.version,
        "re-prime after the interval elapsed must pick up the latest scene version"
    );
}

/// hud-uyhpn benchmark: a portal drag-move is position-only, so it must NOT
/// re-prime the version-gated content caches. This measures re-primes per drag
/// frame two ways over the REAL cache gates:
///
///   * BASELINE (pre-fix) — each drag frame bumps `scene.version` (the old
///     `translate_portal_group_on_drag` behavior). The markdown cache has no
///     cadence gate, so it re-hashes all content + rebuilds the node-key cache
///     EVERY frame → `FRAMES` re-primes. This is the per-frame re-shape the live
///     low-fps drag exhibited.
///   * FIXED — each drag frame bumps `scene.geometry_epoch` and leaves
///     `scene.version` frozen (the new drag path). The version gate short-circuits
///     immediately → ZERO re-primes, near-zero wall time.
///
/// The eprintln! line carries the before/after numbers for the PR body. GPU is
/// required only to construct the compositor/text renderer; the test never
/// renders (no pixel readback), so it is safe under llvmpipe.
#[tokio::test]
async fn drag_move_position_only_skips_content_cache_reprimes_bench() {
    use std::time::Instant;
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
    };

    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // A realistically large transcript so the per-frame re-hash cost is visible.
    let content = "The quick brown fox jumps over the lazy dog. ".repeat(400);
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content,
            bounds: Rect::new(0.0, 0.0, 256.0, 240.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: tze_hud_scene::types::Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);
    let tile_id = *scene.tiles.keys().next().unwrap();

    // Commit-time prime at rest — caches now match scene.version.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    assert_eq!(compositor.markdown_cache_scene_version, scene.version);

    const FRAMES: u64 = 60;
    let (dx, dy) = (3.0_f32, -2.0_f32);

    // ── BASELINE: pre-fix translate — move + bump scene.version each frame ──
    let mut md_reprimes_baseline = 0u64;
    let baseline_start = Instant::now();
    for _ in 0..FRAMES {
        if let Some(t) = scene.tiles.get_mut(&tile_id) {
            t.bounds.x += dx;
            t.bounds.y += dy;
        }
        scene.version += 1; // old drag path invalidated content caches here
        let before = compositor.markdown_cache_scene_version;
        compositor.prime_markdown_cache(&scene);
        if compositor.markdown_cache_scene_version != before {
            md_reprimes_baseline += 1;
        }
    }
    let baseline_us = baseline_start.elapsed().as_micros();

    // ── FIXED: new drag path — move + bump geometry_epoch each frame ──
    let mut md_reprimes_fixed = 0u64;
    let fixed_start = Instant::now();
    for _ in 0..FRAMES {
        if let Some(t) = scene.tiles.get_mut(&tile_id) {
            t.bounds.x += dx;
            t.bounds.y += dy;
        }
        scene.bump_geometry_epoch(); // position-only: version stays frozen
        let before = compositor.markdown_cache_scene_version;
        compositor.prime_markdown_cache(&scene);
        if compositor.markdown_cache_scene_version != before {
            md_reprimes_fixed += 1;
        }
    }
    let fixed_us = fixed_start.elapsed().as_micros();

    eprintln!(
        "hud-uyhpn bench (FRAMES={FRAMES}): markdown cache re-primes \
         baseline={md_reprimes_baseline} fixed={md_reprimes_fixed}; \
         prime wall-time baseline={baseline_us}us fixed={fixed_us}us"
    );

    assert_eq!(
        md_reprimes_baseline, FRAMES,
        "pre-fix: a version bump per drag frame re-primes the markdown cache every frame"
    );
    assert_eq!(
        md_reprimes_fixed, 0,
        "fixed: a geometry-epoch (position-only) drag must NEVER re-prime the markdown cache"
    );
}

/// Verify that `load_font_bytes` resets the truncation cache sentinel to
/// `u64::MAX` when a NEW font is loaded, forcing a re-prime on the next
/// `prime_truncation_cache` call (hud-v2z6u item b).
///
/// A new font can change shaping advance widths, so all truncation points
/// cached under the old font metrics are stale.  The sentinel reset ensures
/// the next `prime_truncation_cache` call re-resolves all entries with the
/// updated `FontSystem`.
///
/// GPU required; skips gracefully when no adapter is available.
#[tokio::test]
async fn load_font_bytes_new_font_resets_truncation_cache_scene_version() {
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
    };

    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(64, 64).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "The quick brown fox jumps over the lazy dog.".to_string(),
            bounds: Rect::new(0.0, 0.0, 64.0, 16.0),
            font_size_px: 12.0,
            font_family: FontFamily::SystemMonospace,
            color: tze_hud_scene::types::Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };

    let scene = scene_with_node(node);

    // ── Stage 4: commit-time prime sets sentinel to scene.version ─────────
    compositor.prime_truncation_cache(&scene);
    assert_eq!(
        compositor.truncation_cache_scene_version, scene.version,
        "after commit-time prime, sentinel must equal scene.version"
    );

    // ── Load a new font: sentinel must be reset to u64::MAX ───────────────
    // Use a minimal valid TTF/OTF-like byte slice.  The font loader may
    // reject invalid data, but `load_font_bytes` is required to reset the
    // sentinel regardless — the guard is on `was_new`, not parse success.
    // We use a unique resource_id that the compositor has never seen.
    let new_resource_id: [u8; 32] = [0xAB; 32]; // never-before-seen id
    // A minimal placeholder payload; glyphon/fontdb will silently skip invalid
    // font bytes, but the resource_id deduplication check (`has_font`) must
    // return false for this novel id — triggering the sentinel reset.
    let dummy_font_bytes: &[u8] = b"OTTO"; // not a real font, but triggers the path
    compositor.load_font_bytes(new_resource_id, dummy_font_bytes);

    // After loading a new (unknown) resource_id, the sentinel must be u64::MAX,
    // signaling that the next prime_truncation_cache must re-resolve all entries.
    assert_eq!(
        compositor.truncation_cache_scene_version,
        u64::MAX,
        "load_font_bytes with a new resource_id must reset truncation_cache_scene_version \
             to u64::MAX so the next prime re-resolves all entries [hud-v2z6u]"
    );

    // ── Loading the SAME resource_id again must NOT reset the sentinel ─────
    // Re-prime first so sentinel is back to a known scene version.
    compositor.prime_truncation_cache(&scene);
    assert_eq!(
        compositor.truncation_cache_scene_version, scene.version,
        "re-prime must restore sentinel to scene.version"
    );

    // Now load the same resource_id again — dedup guard returns early, sentinel
    // must remain at scene.version (not u64::MAX).
    compositor.load_font_bytes(new_resource_id, dummy_font_bytes);
    assert_eq!(
        compositor.truncation_cache_scene_version, scene.version,
        "load_font_bytes with an already-loaded resource_id must NOT reset the sentinel \
             — dedup guard prevents spurious cache invalidation [hud-v2z6u]"
    );
}
