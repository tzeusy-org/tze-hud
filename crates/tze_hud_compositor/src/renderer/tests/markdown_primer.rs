use super::*;

/// hud-991cj regression: a pure scroll (and a steady-state re-render) must
/// re-shape ZERO text buffers, because `prepare_text_items` now reuses a shaped
/// `Buffer` keyed by a scroll-invariant `ShapeKey` (content+geometry+font+runs,
/// NOT offset). A content append re-shapes exactly the one changed item.
///
/// This was the flicker source: the output pane is a full-content `Clip` node,
/// so before the cache every scroll frame rebuilt+shaped the whole transcript
/// through `shape_until_scroll`. Measured over a small and a ~64KiB transcript
/// so the win is visible at both sizes. GPU builds the compositor;
/// `render_frame_headless` is used (no pixel readback → no llvmpipe deadlock).
#[tokio::test]
async fn scroll_reshape_bench_hud991cj() {
    use std::time::Instant;
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
    };

    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let line = "The quick brown fox jumps over the lazy dog.\n"; // 45 bytes
    let white = tze_hud_scene::types::Rgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };

    // A small transcript and a ~64KiB transcript (45 B/line × 1440 ≈ 63 KiB, just under MAX_MARKDOWN_BYTES).
    for (label, repeat) in [("small", 12usize), ("64KiB", 1440usize)] {
        let content = line.repeat(repeat);
        let content_bytes = content.len();
        // Laid out far taller than the 256px tile so the pane is scrollable
        // (windowed by offset, not truncated).
        let make_node = |text: String| Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: text,
                bounds: Rect::new(0.0, 0.0, 240.0, repeat as f32 * 20.0 + 200.0),
                font_size_px: 14.0,
                font_family: FontFamily::SystemMonospace,
                color: white,
                background: None,
                alignment: TextAlign::Start,
                overflow: TextOverflow::Clip,
                color_runs: Box::default(),
            }),
        };

        let mut scene = scene_with_node(make_node(content.clone()));
        let tile_id = *scene.tiles.keys().next().unwrap();
        scene
            .register_tile_scroll_config(
                tile_id,
                tze_hud_scene::types::TileScrollConfig::vertical(),
            )
            .unwrap();

        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);

        // Warm frame establishes the initial shape.
        let _ = compositor.render_frame_headless(&mut scene, &surface);

        // ── REST: re-render the identical scene; shaped buffer is reused. ──
        const REST_FRAMES: u64 = 30;
        let before_rest = compositor.text_shape_call_count();
        let rest_start = Instant::now();
        for _ in 0..REST_FRAMES {
            let _ = compositor.render_frame_headless(&mut scene, &surface);
        }
        let rest_elapsed = rest_start.elapsed();
        let rest_reshapes = compositor.text_shape_call_count() - before_rest;

        // ── SCROLL: bump the offset each frame; shape inputs unchanged. ──
        const SCROLL_FRAMES: u64 = 30;
        let before_scroll = compositor.text_shape_call_count();
        let scroll_start = Instant::now();
        for i in 1..=SCROLL_FRAMES {
            scene
                .set_tile_scroll_offset_local(tile_id, 0.0, i as f32 * 40.0)
                .unwrap();
            let _ = compositor.render_frame_headless(&mut scene, &surface);
        }
        let scroll_elapsed = scroll_start.elapsed();
        let scroll_reshapes = compositor.text_shape_call_count() - before_scroll;

        // ── APPEND: change content once → exactly one re-shape (one item). ──
        let appended = format!("{content}Appended tail line for hud-991cj.\n");
        scene.set_tile_root(tile_id, make_node(appended)).unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        let before_append = compositor.text_shape_call_count();
        let _ = compositor.render_frame_headless(&mut scene, &surface);
        let append_reshapes = compositor.text_shape_call_count() - before_append;

        eprintln!(
            "hud-991cj bench [{label}] bytes={content_bytes} | \
             rest: {rest_reshapes} reshapes {:.3} ms/frame | \
             scroll: {scroll_reshapes} reshapes {:.3} ms/frame | \
             append: {append_reshapes} reshape ({:?} rest, {:?} scroll)",
            rest_elapsed.as_secs_f64() * 1000.0 / REST_FRAMES as f64,
            scroll_elapsed.as_secs_f64() * 1000.0 / SCROLL_FRAMES as f64,
            rest_elapsed,
            scroll_elapsed,
        );

        // Steady-state (rest) and pure scroll reuse the shaped buffer: 0 re-shapes.
        assert_eq!(
            rest_reshapes, 0,
            "[{label}] steady-state re-render must reuse shaped buffers (0 re-shapes), got {rest_reshapes}"
        );
        assert_eq!(
            scroll_reshapes, 0,
            "[{label}] pure scroll must reuse shaped buffers (0 re-shapes), got {scroll_reshapes} over {SCROLL_FRAMES} frames"
        );
        // A content change re-shapes exactly the one changed text item.
        assert_eq!(
            append_reshapes, 1,
            "[{label}] a content append must re-shape exactly the one changed item, got {append_reshapes}"
        );
    }
}

/// Shared test helper: build a scrollable single-tile scene whose root is a
/// markdown transcript of `repeat` copies of a fixed 45-byte line, mirroring
/// `scroll_reshape_bench_hud991cj`'s scenario shape (hud-u4lq2).
fn hud_u4lq2_scenario_scene(repeat: usize) -> SceneGraph {
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
    };

    let line = "The quick brown fox jumps over the lazy dog.\n";
    let content = line.repeat(repeat);
    let white = tze_hud_scene::types::Rgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content,
            bounds: Rect::new(0.0, 0.0, 240.0, repeat as f32 * 20.0 + 200.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: white,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);
    let tile_id = *scene.tiles.keys().next().unwrap();
    scene
        .register_tile_scroll_config(tile_id, tze_hud_scene::types::TileScrollConfig::vertical())
        .unwrap();
    scene
}

/// hud-u4lq2 baseline: a fresh, single-use `Compositor` (no reuse across
/// scenarios) must observe its own `prime_markdown_cache` background-parsed
/// snapshot for a large (>`INLINE_PARSE_BYTE_THRESHOLD`) transcript within a
/// few frames. This was never actually broken (the bug required Compositor
/// reuse across scenes, see `reused_compositor_across_scenes_lands_markdown_primer_hud_u4lq2`
/// below) but is kept as a fast sanity baseline for the miss-counter mechanism.
#[tokio::test]
async fn markdown_primer_landing_converges_on_render_path_hud_u4lq2() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = hud_u4lq2_scenario_scene(1440); // ~64KiB, matching hud-991cj's large case.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    // Poll with the SAME budget as primer_large_payload_swaps_in_off_thread
    // (200 * 5ms = up to 1s), rendering one frame per try so we observe the
    // RENDER PATH's own view of the snapshot, not just the primer's internal
    // ArcSwap in isolation.
    let mut misses_before = compositor.markdown_cache_miss_count();
    let mut converged = false;
    for _ in 0..200 {
        let _ = compositor.render_frame_headless(&mut scene, &surface);
        let misses_after = compositor.markdown_cache_miss_count();
        if misses_after == misses_before {
            // No NEW miss this frame -> the snapshot landed and is visible to
            // the render path. Confirm it holds for a few more frames too.
            let mut still_converged = true;
            for _ in 0..5 {
                let before = compositor.markdown_cache_miss_count();
                let _ = compositor.render_frame_headless(&mut scene, &surface);
                if compositor.markdown_cache_miss_count() != before {
                    still_converged = false;
                    break;
                }
            }
            if still_converged {
                converged = true;
                break;
            }
        }
        misses_before = misses_after;
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert!(
        converged,
        "markdown cache misses on the render path never stopped growing within \
         the 1s poll budget (misses so far: {}) -- the primer's background-parsed \
         snapshot never became visible to collect_text_items_from_node [hud-u4lq2]",
        compositor.markdown_cache_miss_count(),
    );
}

/// hud-u4lq2 regression (the confirmed bug): reuse ONE `Compositor` across a
/// "small" (inline, under `INLINE_PARSE_BYTE_THRESHOLD`) scenario THEN a
/// "64KiB" scenario, exactly mirroring `scroll_reshape_bench_hud991cj`'s
/// shared-compositor-across-scenarios shape.
///
/// # Root cause (confirmed via direct instrumentation of `publish_if_newer`)
///
/// Each loop iteration builds a brand-new `SceneGraph` via `scene_with_node`,
/// whose `version` counter always restarts at 0 and reaches a small number
/// (4 in the traced repro) after tab/tile/root-node setup — identical for
/// both scenarios, since the setup sequence is the same. Before the fix,
/// `Compositor::prime_markdown_cache` passed this raw `scene.version` straight
/// through to `MarkdownPrimer::prime`, which used it as `MarkdownPrimer`'s
/// OWN stale-clobber ordering token (`published_version`) — a value that
/// persists across the primer's ENTIRE LIFETIME, not reset per scene. After
/// the "small" scenario's append step primed at `scene.version == 5`,
/// `published_version` sat at `5`. The "64KiB" scenario's first prime then
/// dispatched with `scene.version == 4` (its OWN fresh count, unrelated to the
/// "small" scenario's numbering). When that background parse completed,
/// `publish_if_newer` saw `4 < 5` and discarded it — instrumented directly:
/// `publish_if_newer: version=4 cur=5 will_drop=true`. The 64KiB content's
/// entry never landed in the primer's cache, so EVERY subsequent frame's
/// `collect_text_items_from_node` lookup missed and paid a full synchronous
/// re-parse, exactly matching the measured hud-991cj symptom (5-15ms/frame
/// steady-state cost against a 16.6ms/4ms budget).
///
/// The fix makes `MarkdownPrimer` stamp every dispatch with its OWN internal,
/// ever-increasing epoch (see the hud-u4lq2 note on `MarkdownPrimer`) instead
/// of trusting the caller's scene-relative version — an epoch can never go
/// backwards regardless of how many `SceneGraph` instances share the primer,
/// so this scenario can no longer drop a legitimate background result.
///
/// This test asserts convergence within a TIGHT budget (a handful of frames)
/// for the reused-compositor case, not just "eventually converges" — the fix
/// makes the background snapshot land close to immediately, the same as the
/// non-reused baseline above. Fails on unpatched code: the 64KiB scenario
/// never converges within the budget (misses grow every frame).
#[tokio::test]
async fn reused_compositor_across_scenes_lands_markdown_primer_hud_u4lq2() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    for (label, repeat) in [("small", 12usize), ("64KiB", 1440usize)] {
        let mut scene = hud_u4lq2_scenario_scene(repeat);

        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);

        // A content append (mirroring hud-991cj's append step) exercises the
        // same re-prime path the original bench does, without which the
        // "small" scenario would never advance the primer's internal epoch
        // the way the confirmed repro did.
        let tile_id = *scene.tiles.keys().next().unwrap();
        let appended = format!(
            "{}Appended tail line for hud-u4lq2.\n",
            "The quick brown fox jumps over the lazy dog.\n".repeat(repeat)
        );
        let node = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: tze_hud_scene::types::NodeData::TextMarkdown(
                tze_hud_scene::types::TextMarkdownNode {
                    content: appended,
                    bounds: tze_hud_scene::types::Rect::new(
                        0.0,
                        0.0,
                        240.0,
                        repeat as f32 * 20.0 + 200.0,
                    ),
                    font_size_px: 14.0,
                    font_family: tze_hud_scene::types::FontFamily::SystemMonospace,
                    color: tze_hud_scene::types::Rgba {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 1.0,
                    },
                    background: None,
                    alignment: tze_hud_scene::types::TextAlign::Start,
                    overflow: tze_hud_scene::types::TextOverflow::Clip,
                    color_runs: Box::default(),
                },
            ),
        };
        scene.set_tile_root(tile_id, node).unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);

        // Tight budget (unlike the 10s diagnostic used during root-causing):
        // the fix makes this converge in a handful of frames, same as an
        // unreused Compositor. 60 tries * 5ms = up to 300ms.
        let mut misses_before = compositor.markdown_cache_miss_count();
        let mut converged = false;
        for _ in 0..60 {
            let _ = compositor.render_frame_headless(&mut scene, &surface);
            let misses_after = compositor.markdown_cache_miss_count();
            if misses_after == misses_before {
                let mut still_converged = true;
                for _ in 0..5 {
                    let before = compositor.markdown_cache_miss_count();
                    let _ = compositor.render_frame_headless(&mut scene, &surface);
                    if compositor.markdown_cache_miss_count() != before {
                        still_converged = false;
                        break;
                    }
                }
                if still_converged {
                    converged = true;
                    break;
                }
            }
            misses_before = misses_after;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert!(
            converged,
            "[{label}] markdown cache misses never stopped growing on a Compositor \
             reused across scenes (misses so far: {}) -- the confirmed hud-u4lq2 \
             regression: MarkdownPrimer's stale-clobber guard dropped the \
             background-parsed snapshot because it compared raw scene.version \
             across independently-constructed SceneGraph instances",
            compositor.markdown_cache_miss_count(),
        );
    }
}
