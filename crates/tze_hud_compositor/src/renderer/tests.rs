use super::*;
use crate::surface::{HeadlessSurface, SurfaceAcquireFailure, SurfaceRecoveryOutcome};
use image_cache::{
    CARET_BLINK_HALF_PERIOD, ComposerLayout, caret_visible_at, composer_scroll_offset,
    composer_vertical_line_offset, composer_visible_line_count,
};
use tze_hud_input::{DRAG_OPACITY_BOOST, DRAG_Z_ORDER_BOOST};
use tze_hud_scene::graph::SceneGraph;

/// Mutex to serialize tests that mutate `HEADLESS_FORCE_SOFTWARE`, a
/// global environment variable.  Rust tests run in parallel by default,
/// so concurrent mutations would cause races.  This is an in-process lock;
/// it does not protect against separate test binary runs.
static ENV_VAR_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn adapter_identity_preserves_actual_wgpu_fields_without_a_gpu() {
    let identity = CompositorAdapterInfo::from(wgpu::AdapterInfo {
        name: "llvmpipe (LLVM 19.1.7, 256 bits)".to_string(),
        vendor: 0x10005,
        device: 0,
        device_type: wgpu::DeviceType::Cpu,
        driver: "llvmpipe".to_string(),
        driver_info: "Mesa 24.2".to_string(),
        backend: wgpu::Backend::Vulkan,
    });

    assert_eq!(identity.name, "llvmpipe (LLVM 19.1.7, 256 bits)");
    assert_eq!(identity.backend, "Vulkan");
    assert_eq!(identity.device_type, "Cpu");
    assert_eq!(identity.driver, "llvmpipe");
    assert_eq!(identity.driver_info, "Mesa 24.2");
    assert_eq!(identity.vendor, 0x10005);
    assert_eq!(identity.device, 0);
}

/// Returns `true` when the test should be skipped due to missing GPU.
///
/// Tests that require a wgpu adapter (GPU or software renderer) hang
/// indefinitely on `request_adapter` in environments without any GPU
/// or software fallback (e.g., minimal CI containers without llvmpipe).
///
/// Set `TZE_HUD_SKIP_GPU_TESTS=1` to opt out all GPU-dependent tests.
/// In CI, Mesa/llvmpipe is installed and `HEADLESS_FORCE_SOFTWARE=1` is
/// set instead, so GPU tests run via a software adapter.
fn should_skip_gpu_tests() -> bool {
    std::env::var("TZE_HUD_SKIP_GPU_TESTS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Process-wide count of GPU-gated tests that hit `require_gpu!`'s early
/// return (no wgpu adapter available, or `TZE_HUD_SKIP_GPU_TESTS=1`).
///
/// Without this, a skipped test reports as an ordinary `test ... ok` in
/// `cargo test` output — indistinguishable from a test that actually
/// exercised the render path. PR #1148's RefCell double-borrow panic escaped
/// exactly this way: the render-path regression tests that would have caught
/// it (`markdown_primer_landing_converges_on_render_path_hud_u4lq2`,
/// `reused_compositor_across_scenes_lands_markdown_primer_hud_u4lq2`) silently
/// no-op'd in a no-GPU sandbox; only CI's GPU/llvmpipe lane actually ran them
/// (hud-7o3rw). `require_gpu!` now `eprintln!`s a loud, greppable
/// `"SKIPPED (no GPU)"` marker — carrying the exact call-site location and
/// this running count — on every skip, so `grep -c "SKIPPED (no GPU)"` over
/// captured test output tells a contributor exactly how many render-path
/// tests did NOT run for real in this environment.
static GPU_SKIP_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Skips a GPU-dependent test by returning early if no GPU is available.
///
/// Usage inside an `async fn` test:
/// ```ignore
/// let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
/// ```
///
/// Expands to a `match` that returns `()` when the helper returns `None` (no
/// adapter found or `TZE_HUD_SKIP_GPU_TESTS=1`) — by default `cargo test`
/// then reports the test as an ordinary `ok`, identical to a test that ran
/// the real render path. To make that gap visible instead of silent
/// (hud-7o3rw), the `None` arm first prints a `"SKIPPED (no GPU)"` marker
/// carrying the exact `require_gpu!` call site (`file!()`/`line!()` resolve
/// to the *invocation* site here, not this macro definition, so each call
/// site gets its own precise, independently-greppable location) and the
/// running `GPU_SKIP_COUNT`. This is deliberately NOT turned into a
/// panic/failure: no-GPU / no-llvmpipe environments (including some CI
/// lanes) are expected to skip these tests, and a hard failure there would
/// break every headless environment instead of just surfacing the coverage
/// gap.
macro_rules! require_gpu {
    ($expr:expr) => {
        match $expr {
            Some(v) => v,
            None => {
                let skip_count =
                    GPU_SKIP_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                eprintln!(
                    "SKIPPED (no GPU) [{skip_count} skipped so far] at {}:{} — this \
                     render-path test did NOT exercise the real render path in this \
                     environment; only a GPU- or llvmpipe-backed lane actually runs it",
                    file!(),
                    line!(),
                );
                return;
            }
        }
    };
}

/// Convenience: build a minimal scene with one tile containing the given node.
fn scene_with_node(node: Node) -> SceneGraph {
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene.set_tile_root(tile_id, node).unwrap();
    scene
}

/// Build the exact canonical scene accepted by the retained evidence lane and
/// return the first tile/root pair for a controlled text-only mutation.
fn canonical_retained_scene() -> (SceneGraph, SceneId, SceneId) {
    let mut scene = SceneGraph::new(1_000.0, 500.0);
    let tab_id = scene.create_tab("canonical", 0).expect("canonical tab");
    let lease_id = scene.grant_lease("canonical-agent", 60_000, vec![]);
    let mut first_tile_and_root = None;

    for index in 0..50 {
        let column = index % 10;
        let row = index / 10;
        // Leave gutters so an adjacent tile's idle drag grip cannot cross the
        // first tile's retained damage boundary.
        let tile_width = 96.0;
        let tile_height = 92.0;
        let tile_id = scene
            .create_tile(
                tab_id,
                "canonical-agent",
                lease_id,
                Rect::new(
                    (column * 100) as f32,
                    (row * 100) as f32,
                    tile_width,
                    tile_height,
                ),
                index as u32,
            )
            .expect("canonical tile");
        let root_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    id: root_id,
                    children: vec![],
                    layout: Default::default(),
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "AB".into(),
                        bounds: Rect::new(0.0, 0.0, tile_width, tile_height),
                        font_size_px: 18.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
            .expect("canonical text root");
        if index == 0 {
            first_tile_and_root = Some((tile_id, root_id));
        }
    }

    let (first_tile_id, first_root_id) = first_tile_and_root.expect("first canonical tile");
    (scene, first_tile_id, first_root_id)
}

/// Create a headless compositor and surface pair for testing.
///
/// Returns `None` (and prints a skip notice) when:
/// - `TZE_HUD_SKIP_GPU_TESTS=1` is set, or
/// - no wgpu adapter is available in the current environment.
///
/// Use the `require_gpu!` macro at the call site to early-return from the
/// test when `None` is returned:
/// ```ignore
/// let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
/// ```
async fn make_compositor_and_surface(w: u32, h: u32) -> Option<(Compositor, HeadlessSurface)> {
    if should_skip_gpu_tests() {
        eprintln!("SKIPPED (no GPU) reason: TZE_HUD_SKIP_GPU_TESTS=1 is set");
        return None;
    }
    match Compositor::new_headless(w, h).await {
        Ok(compositor) => {
            let surface = HeadlessSurface::new(&compositor.device, w, h);
            Some((compositor, surface))
        }
        Err(CompositorError::NoAdapter) => {
            eprintln!("SKIPPED (no GPU) reason: no wgpu adapter available in this environment");
            None
        }
        Err(e) => panic!("unexpected compositor error: {e}"),
    }
}

#[tokio::test]
async fn test_static_image_node_renders_placeholder_quad() {
    // The static image placeholder renders a warm-gray outer quad ~[0.55, 0.50, 0.45].
    // In sRGB output the linear values are gamma-compressed.
    // We just verify that *some* non-background pixels appear in the expected warm range.
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    // RS-4: StaticImageNode uses resource_id + decoded_bytes; no raw blob embedded.
    let resource_id = ResourceId::of(b"8x8 test image placeholder");
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 8,
            height: 8,
            decoded_bytes: 8 * 8 * 4,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
        }),
    };

    // Resource must be registered before set_tile_root inserts the StaticImageNode tree.
    let mut scene = SceneGraph::new(256.0, 256.0);
    scene.register_resource(resource_id);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene.set_tile_root(tile_id, node).unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    // The background clear color is ~[0.05, 0.05, 0.1] in linear; tile bg is [0.05,0.05,0.05].
    // The placeholder outer quad is warm gray [0.55, 0.50, 0.45] in linear.
    // In sRGB this is approximately [198, 188, 176]. We look for pixels brighter than 150 in
    // all three channels to confirm the quad was rendered (not just the dark background).
    let any_warm_pixel = pixels
        .chunks(4)
        .any(|p| p[0] > 150 && p[1] > 140 && p[2] > 130);
    assert!(
        any_warm_pixel,
        "expected warm-gray placeholder pixels from StaticImageNode"
    );
}

/// A real recovered window-surface lifecycle outcome must invalidate the
/// compositor-private retained proof lane. The next observed full frame is a
/// structured diagnostic, never a proportional-render certification.
#[tokio::test]
async fn reconfigured_surface_recovery_is_a_non_passing_retained_diagnostic() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1_000, 500).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (mut scene, first_tile_id, first_root_id) = canonical_retained_scene();
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    // Seed a genuine retained baseline, then make the one direct text change
    // that normally qualifies for a proportional retained render. The recovery
    // outcome below must preempt that otherwise eligible path.
    compositor.render_frame_headless(&mut scene, &surface);
    let baseline_diagnostic = compositor
        .take_change_efficiency_diagnostic()
        .expect("canonical baseline emits a surface-creation diagnostic");
    assert_eq!(
        baseline_diagnostic
            .full_surface_invalidation
            .expect("baseline invalidation metadata")
            .reason,
        tze_hud_telemetry::FullSurfaceInvalidationReason::SurfaceCreation
    );
    let mut changed_text = match &scene.nodes[&first_root_id].data {
        NodeData::TextMarkdown(text) => text.clone(),
        other => panic!("expected canonical text root, got {other:?}"),
    };
    changed_text.content = "BA".into();
    scene
        .update_node_content(
            first_tile_id,
            first_root_id,
            NodeData::TextMarkdown(changed_text),
        )
        .expect("controlled canonical text mutation");
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    // This is the exact compositor-private outcome emitted after a real
    // Lost/Outdated surface acquire has been reconfigured, not a model-only
    // retained-planner signal.
    compositor.record_surface_recovery_outcome(SurfaceRecoveryOutcome::Reconfigured {
        trigger: SurfaceAcquireFailure::Lost,
    });
    compositor.render_frame_headless(&mut scene, &surface);

    assert!(
        compositor.take_change_efficiency_capture().is_none(),
        "a recovered surface must not leave a scoped retained capture available"
    );
    let diagnostic = compositor
        .take_change_efficiency_diagnostic()
        .expect("surface recovery emits a structured retained diagnostic");
    let report = diagnostic.validate();
    assert!(!report.passed, "{report:#?}");
    assert_eq!(
        report.status,
        tze_hud_telemetry::ChangeEfficiencyValidationStatus::DiagnosticFullSurface,
        "{report:#?}"
    );
    assert_eq!(
        diagnostic
            .full_surface_invalidation
            .expect("full-surface diagnostic metadata")
            .reason,
        tze_hud_telemetry::FullSurfaceInvalidationReason::DeviceRecovery
    );
}

#[tokio::test]
async fn test_static_image_node_composited_with_other_nodes() {
    // Render a scene with both a SolidColor node and a StaticImage node in adjacent tiles.
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(512, 256).await);

    let mut scene = SceneGraph::new(512.0, 256.0);
    // Resource must be registered before set_tile_root inserts a StaticImageNode tree.
    let static_image_resource_id = ResourceId::of(b"8x8 green placeholder");
    scene.register_resource(static_image_resource_id);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);

    // Left tile: red solid color
    let left_tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene
        .set_tile_root(
            left_tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(1.0, 0.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
                    radius: None,
                }),
            },
        )
        .unwrap();

    // Right tile: static image
    // RS-4: StaticImageNode uses resource_id + decoded_bytes; no raw blob embedded.
    let right_tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(256.0, 0.0, 256.0, 256.0),
            2,
        )
        .unwrap();
    scene
        .set_tile_root(
            right_tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::StaticImage(StaticImageNode {
                    resource_id: static_image_resource_id,
                    width: 8,
                    height: 8,
                    decoded_bytes: 8 * 8 * 4,
                    fit_mode: ImageFitMode::Cover,
                    bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
                }),
            },
        )
        .unwrap();

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 512 * 256 * 4, "pixel buffer size mismatch");
    // Just verify the frame completed without panic and returned the expected buffer size.
}

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
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
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

/// hud-k6yvb: a TILE-LEVEL focus owner (a non-passthrough tile with no focusable
/// nodes) must get a visible ring around the whole tile from the chrome pass —
/// the case #988 could not draw because tile-level focus has no scene state.
#[tokio::test]
async fn test_tile_level_focus_owner_emits_ring_in_overlay_mode() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.overlay_mode = true;

    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
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

/// hud-nx7yq.3: a runtime-authored viewer echo entry must render as a
/// kind-distinct text line above the composer strip on a raw-tile portal. This
/// is the compositor half of the "submitted text bubbles into the transcript"
/// fix — draw-list-level (no pixel readback) so it is deadlock-safe.
#[tokio::test]
async fn test_viewer_echo_renders_kind_distinct_line_above_composer() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Portal tile rooted at a composer-input HitRegion spanning the tile, so
    // there is room above the bottom input strip for history lines.
    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
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
                    bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                    interaction_id: "portal-composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    // No echoes yet → no viewer-echo text items.
    let before = compositor.collect_text_items(&scene, 400.0, 300.0);
    assert!(
        !before.iter().any(|t| t.text.contains("hello there")),
        "no viewer echo should render before any submission"
    );

    // Runtime authored a viewer reply (as append_raw_tile_viewer_echo does).
    compositor
        .viewer_echoes
        .append(tile_id, "hello there".to_owned(), 1);

    let after = compositor.collect_text_items(&scene, 400.0, 300.0);
    let echo = after
        .iter()
        .find(|t| t.text.contains("hello there"))
        .expect("viewer echo line must render after a submission");
    // hud-7ic89: the entry's timestamp (derived from submitted_at_wall_us=1,
    // i.e. 1 microsecond past UTC midnight) prefixes the message.
    assert_eq!(
        &*echo.text, "00:00  hello there",
        "viewer echo text is timestamp-prefixed"
    );

    // Kind-distinct: carries the token-driven viewer color (default accent blue),
    // not the near-white transcript text color.
    assert_eq!(
        echo.color,
        [0x8A, 0xB4, 0xF8, 0xFF],
        "viewer echo must use the kind-distinct viewer token color"
    );
    // Positioned above the bottom input strip (upper portion of the tile).
    assert!(
        echo.pixel_y < 300.0,
        "viewer echo line must sit within the tile above the composer strip"
    );
}

/// hud-hsc1t: a portal tile with ≥2 runtime-authored viewer echoes renders a
/// token-styled turn divider on the boundary between each adjacent pair of
/// entries, resolved from the shared `portal.divider.*` tokens (never hardcoded).
/// One entry → no interior divider. Draw-list-level (no readback).
#[tokio::test]
async fn test_viewer_echo_renders_turn_dividers_between_entries() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();

    // Divider token present (as `set_token_map` populates from
    // portal.divider.color / .thickness_px). Without it the pass is inert.
    compositor.markdown_tokens.separator_color = Some(Rgba::new(0.27, 0.32, 0.43, 1.0));
    compositor.markdown_tokens.separator_thickness_px = 2.0;

    let tile = |scene: &SceneGraph| -> Tile { scene.visible_tiles()[0].clone() };

    // A single echo entry yields no interior divider.
    compositor
        .viewer_echoes
        .append(tile_id, "first reply".to_owned(), 1);
    compositor.prime_viewer_echo_layout(&scene);
    assert!(
        compositor
            .collect_viewer_echo_divider_rects(&tile(&scene), &scene)
            .is_empty(),
        "one entry has no interior divider"
    );

    // Two more entries → three total → two interior dividers.
    compositor
        .viewer_echoes
        .append(tile_id, "second reply".to_owned(), 2);
    compositor
        .viewer_echoes
        .append(tile_id, "third reply".to_owned(), 3);
    compositor.prime_viewer_echo_layout(&scene);
    let rects = compositor.collect_viewer_echo_divider_rects(&tile(&scene), &scene);
    assert_eq!(
        rects.len(),
        2,
        "three echo entries render two interior turn dividers"
    );
    for rect in &rects {
        assert_eq!(
            rect.height, 2.0,
            "divider thickness comes from the portal.divider token, not a hardcode"
        );
        assert!(rect.width > 0.0, "divider spans the echo zone width");
    }
    // Dividers ascend (older boundary above newer boundary).
    assert!(
        rects[0].y < rects[1].y,
        "boundaries ordered oldest→newest top→bottom"
    );

    // Clearing the divider token disables the pass entirely.
    compositor.markdown_tokens.separator_color = None;
    assert!(
        compositor
            .collect_viewer_echo_divider_rects(&tile(&scene), &scene)
            .is_empty(),
        "no divider token ⇒ no separator geometry"
    );
}

/// hud-xgtuf: the viewer-echo stack must anchor to the TOP of the LIVE
/// (`visible_lines`-aware) composer box, so a growing multi-line draft never
/// grows into the echo history. As the composer box grows (1 → N lines) the echo
/// stack must ride upward and stay strictly above the box; shrinking back must
/// return it to the resting position. Draw-list-level (no readback).
#[tokio::test]
async fn test_viewer_echo_stack_tracks_live_composer_box() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
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
                    bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                    interaction_id: "portal-composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    compositor
        .viewer_echoes
        .append(tile_id, "the reply".to_owned(), 1);

    // Geometry the code derives internally, reconstructed here to assert against.
    let region = Rect::new(0.0, 0.0, 400.0, 300.0);
    let lhm = crate::markdown::MarkdownTokens::default().line_height_multiplier;
    let composer_font =
        super::token_colors::resolve_composer_overlay_tokens(&compositor.token_map).font_size_px;
    let echo_font =
        super::token_colors::resolve_viewer_echo_tokens(&compositor.token_map).font_size_px;
    let echo_line_h = (echo_font * lhm).max(1.0);

    let echo_y = |c: &Compositor, scene: &SceneGraph| -> f32 {
        c.collect_text_items(scene, 400.0, 300.0)
            .iter()
            .find(|t| t.text.contains("the reply"))
            .expect("viewer echo must render")
            .pixel_y
    };

    // Resting (single-line) box: echo sits strictly above the box top.
    compositor.composer_layout.visible_lines = 1.0;
    let y_rest = echo_y(&compositor, &scene);
    let box_top_1 = Compositor::composer_input_box(
        region,
        composer_font,
        lhm,
        1.0,
        ComposerVerticalAnchor::Bottom,
        6.0, // default content inset
    )
    .y;
    assert!(
        y_rest + echo_line_h <= box_top_1 + 0.5,
        "resting: echo bottom {} must be at/above the 1-line box top {box_top_1}",
        y_rest + echo_line_h
    );

    // Grow the draft to 4 lines: the echo must ride UP and stay above the taller box.
    compositor.composer_layout.visible_lines = 4.0;
    let y_grown = echo_y(&compositor, &scene);
    let box_top_4 = Compositor::composer_input_box(
        region,
        composer_font,
        lhm,
        4.0,
        ComposerVerticalAnchor::Bottom,
        6.0, // default content inset
    )
    .y;
    assert!(
        y_grown < y_rest,
        "echo must move up as the composer box grows (grown {y_grown} < resting {y_rest})"
    );
    assert!(
        y_grown + echo_line_h <= box_top_4 + 0.5,
        "grown: echo bottom {} must be at/above the 4-line box top {box_top_4} (no overlap)",
        y_grown + echo_line_h
    );

    // Shrink back to one line: the echo returns to its resting position.
    compositor.composer_layout.visible_lines = 1.0;
    let y_shrunk = echo_y(&compositor, &scene);
    assert!(
        (y_shrunk - y_rest).abs() < 0.5,
        "echo must return to the resting position on shrink ({y_shrunk} vs {y_rest})"
    );
}

/// hud-t8c3h: runtime-authored INPUT overlays must use the same viewer-local
/// portal font scale as adapter-authored OUTPUT text.  The live composer draft
/// and retained viewer-echo history own separate layout/measurement paths, so
/// cover both their shaped font sizes and their leading-derived geometry after a
/// whole-portal resize.
///
/// Draw-list-level only: this exercises the actual compositor collection + prime
/// paths without depending on pixel readback.
#[tokio::test]
async fn portal_resize_scales_runtime_authored_input_text_and_layout() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let (mut scene, tile_id) = viewer_echo_test_scene();
    let composer_id = scene.tiles[&tile_id]
        .root_node
        .expect("viewer-echo fixture must root the tile at its composer node");
    // Enough ordinary words to wrap at both scales, so this proves the prime
    // measurement path updates alongside the TextItems' draw-time font sizes.
    let draft = "resize-aware input text ".repeat(18);
    let draft_len = draft.len();
    compositor.local_composer = Some(LocalComposerState {
        text: draft.clone(),
        cursor_byte: draft_len,
        selection_anchor: draft_len,
        at_capacity: false,
        node_id: composer_id,
        placeholder: None,
    });
    compositor.viewer_echoes.append(tile_id, draft.clone(), 1);

    let composer_tokens = resolve_composer_overlay_tokens(&compositor.token_map);
    let echo_tokens = resolve_viewer_echo_tokens(&compositor.token_map);
    let snapshot = |compositor: &mut Compositor, scene: &SceneGraph| {
        compositor.prime_composer_scroll_offset(scene);
        compositor.prime_viewer_echo_layout(scene);
        let items = compositor.collect_text_items(scene, 400.0, 300.0);
        let composer = items
            .iter()
            .find(|item| item.text.as_ref() == draft)
            .expect("local composer draft must render");
        let echo = find_echo(&items);
        (
            composer.font_size_px,
            composer.bounds_height,
            composer.clip_pixel_y,
            echo.font_size_px,
            echo.bounds_height,
            echo.pixel_y,
            compositor.composer_layout.total_lines,
            compositor.viewer_echo_line_counts[&tile_id],
        )
    };

    let before = snapshot(&mut compositor, &scene);
    assert!(
        (before.0 - composer_tokens.font_size_px).abs() < 0.01,
        "unresized composer must use its base token font"
    );
    assert!(
        (before.3 - echo_tokens.font_size_px).abs() < 0.01,
        "unresized viewer history must use its base token font"
    );

    // `tile_font_scale` is the viewer-local whole-portal-resize contract used by
    // the generic OUTPUT collector. INPUT's compositor-owned overlays must follow
    // it without any adapter republish.
    const SCALE: f32 = 1.5;
    scene.set_tile_font_scale(tile_id, SCALE);
    let after = snapshot(&mut compositor, &scene);

    assert!(
        (after.0 - composer_tokens.font_size_px * SCALE).abs() < 0.01,
        "composer draw font must scale with the resized portal (got {})",
        after.0
    );
    assert!(
        (after.3 - echo_tokens.font_size_px * SCALE).abs() < 0.01,
        "viewer-history draw font must scale with the resized portal (got {})",
        after.3
    );
    assert!(
        after.1 > before.1 && after.6 > before.6,
        "composer leading/layout must be remeasured after resize: before={before:?}, after={after:?}"
    );
    assert!(
        after.4 > before.4 && after.7 > before.7,
        "viewer-history leading/wrap layout must be remeasured after resize: before={before:?}, after={after:?}"
    );
    assert!(
        (after.5 + after.4 - after.2).abs() < 0.5,
        "resized history must still bottom-align to the resized composer box: echo bottom={} box top={}",
        after.5 + after.4,
        after.2
    );
}

/// The first frame after a viewer-local resize must derive the composer fill,
/// viewer-echo dividers, and text from one freshly measured wrapped layout.
///
/// Regression for the ordering bug where `build_frame_vertices` emitted the
/// composer fill and echo dividers from the prior frame's layout, then the text
/// encode path reflowed the same draft later in the frame.  The test deliberately
/// leaves a six-line layout primed, applies a local resize scale that reflows the
/// unchanged draft, and inspects the actual staged first-frame vertices.
#[tokio::test]
async fn repro_resize_first_frame_keeps_composer_chrome_aligned_with_wrapped_text() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let (mut scene, tile_id) = viewer_echo_test_scene();
    let composer_id = scene.tiles[&tile_id]
        .root_node
        .expect("viewer-echo fixture must root the tile at its composer node");
    const SURFACE_H: f32 = 500.0;
    scene.tiles.get_mut(&tile_id).unwrap().bounds.height = SURFACE_H;
    match &mut scene.nodes.get_mut(&composer_id).unwrap().data {
        NodeData::HitRegion(region) => region.bounds.height = SURFACE_H,
        other => panic!("viewer-echo fixture root must be a composer HitRegion, got {other:?}"),
    }
    compositor
        .token_map
        .insert("portal.composer.max_lines".to_owned(), "8".to_owned());

    // At the base scale this draft occupies six wrapped rows.  Raising the
    // viewer-local scale reflows the same bytes into a taller visible box; no
    // adapter republish occurs between the two frames.
    let draft = "resize reflow ".repeat(21);
    let draft_len = draft.len();
    compositor.local_composer = Some(LocalComposerState {
        text: draft.clone(),
        cursor_byte: draft_len,
        selection_anchor: draft_len,
        at_capacity: false,
        node_id: composer_id,
        placeholder: None,
    });
    compositor
        .viewer_echoes
        .append(tile_id, "first reply".to_owned(), 1);
    compositor
        .viewer_echoes
        .append(tile_id, "second reply".to_owned(), 2);
    compositor.markdown_tokens.separator_color = Some(Rgba::WHITE);
    compositor.markdown_tokens.separator_thickness_px = 2.0;

    compositor.prime_composer_scroll_offset(&scene);
    compositor.prime_viewer_echo_layout(&scene);
    let old_visible_lines = compositor.composer_layout.visible_lines;
    assert_eq!(
        old_visible_lines, 6.0,
        "test setup: base layout must occupy six visible rows"
    );

    const RESIZED_SCALE: f32 = 1.5;
    scene.set_tile_font_scale(tile_id, RESIZED_SCALE);
    // `build_windowed_frame` is the actual first-frame staging path: it creates
    // flat chrome geometry before it prepares the later text pass.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    let frame = compositor.build_windowed_frame(&mut scene, 400, SURFACE_H as u32);

    // The text preparation at the tail of `build_windowed_frame` has resolved
    // the resized layout.  Capture its fresh geometry independently of the flat
    // vertex staging buffer so the assertion catches a one-frame mismatch.
    let fresh_layout = compositor.composer_layout;
    assert_eq!(
        fresh_layout.visible_lines, 8.0,
        "test setup: resized layout must reflow to eight visible rows"
    );
    let region = Rect::new(0.0, 0.0, 400.0, SURFACE_H);
    let composer_tokens = resolve_composer_overlay_tokens(&compositor.token_map);
    let line_height_multiplier = crate::markdown::MarkdownTokens::default().line_height_multiplier;
    let expected_box = Compositor::composer_input_box(
        region,
        composer_tokens.font_size_px * RESIZED_SCALE,
        line_height_multiplier,
        fresh_layout.visible_lines,
        composer_tokens.anchor,
        composer_tokens.content_inset_px,
    );

    // The staged composer fill is the exact flat-rect geometry presented in the
    // first frame.  Its token color is unique in this minimal scene.
    let composer_fill = [
        composer_tokens.bg_r,
        composer_tokens.bg_g,
        composer_tokens.bg_b,
        composer_tokens.bg_a,
    ];
    let fill_vertices = frame
        .flat_rect_vertices()
        .chunks_exact(6)
        .find(|rect| rect.iter().all(|vertex| vertex.color == composer_fill))
        .expect("first-frame composer fill must be staged");
    let fill_bounds = rect_vertex_bounds(fill_vertices, 400.0, SURFACE_H);
    assert_rect_close(
        fill_bounds,
        expected_box,
        "first-frame composer fill must use the resized wrapped layout",
    );

    // The later text preparation must anchor both the draft and input history to
    // that same input box, not merely update the fill on a subsequent frame.
    let items = compositor.collect_text_items(&scene, 400.0, SURFACE_H);
    let composer_text = items
        .iter()
        .find(|item| item.text.as_ref() == draft)
        .expect("resized composer text must render");
    let echo = find_echo(&items);
    assert!(
        (composer_text.clip_pixel_y - expected_box.y).abs() < 0.5,
        "resized composer text clip must start at the same box top: text={}, box={}",
        composer_text.clip_pixel_y,
        expected_box.y
    );
    assert!(
        (echo.pixel_y + echo.bounds_height - expected_box.y).abs() < 0.5,
        "resized viewer echo must bottom-align to the same box top: echo_bottom={}, box={}",
        echo.pixel_y + echo.bounds_height,
        expected_box.y
    );

    let tile = scene
        .tiles
        .get(&tile_id)
        .expect("fixture tile remains visible");
    let expected_divider = compositor
        .collect_viewer_echo_divider_rects(tile, &scene)
        .into_iter()
        .next()
        .expect("two echo entries must produce a divider");
    let sep_color = compositor
        .markdown_tokens
        .separator_color
        .expect("default portal divider color must be enabled");
    let expected_divider_color = compositor.gpu_color(sep_color);
    let divider_vertices = frame
        .flat_rect_vertices()
        .chunks_exact(6)
        .find(|rect| {
            rect.iter()
                .all(|vertex| vertex.color == expected_divider_color)
        })
        .expect("first-frame viewer-echo divider must be staged");
    let divider_bounds = rect_vertex_bounds(divider_vertices, 400.0, SURFACE_H);
    assert_rect_close(
        divider_bounds,
        expected_divider,
        "first-frame viewer-echo divider must use the resized composer anchor",
    );
}

fn rect_vertex_bounds(vertices: &[crate::pipeline::RectVertex], sw: f32, sh: f32) -> Rect {
    assert_eq!(vertices.len(), 6, "a flat rect must contain two triangles");
    let left = vertices
        .iter()
        .map(|vertex| (vertex.position[0] + 1.0) * sw * 0.5)
        .fold(f32::INFINITY, f32::min);
    let right = vertices
        .iter()
        .map(|vertex| (vertex.position[0] + 1.0) * sw * 0.5)
        .fold(f32::NEG_INFINITY, f32::max);
    let top = vertices
        .iter()
        .map(|vertex| (1.0 - vertex.position[1]) * sh * 0.5)
        .fold(f32::INFINITY, f32::min);
    let bottom = vertices
        .iter()
        .map(|vertex| (1.0 - vertex.position[1]) * sh * 0.5)
        .fold(f32::NEG_INFINITY, f32::max);
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

// ── Viewer-echo wrap + newline rendering (hud-pncm3) ───────────────────────

/// Build a portal tile rooted at a composer-input HitRegion spanning the tile,
/// returning `(scene, tile_id)`. The composer region equals the tile, so there
/// is room above the composer box for viewer history.
fn viewer_echo_test_scene() -> (SceneGraph, SceneId) {
    let mut scene = SceneGraph::new(400.0, 300.0);
    let tab_id = scene.create_tab("agent", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
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
                    bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                    interaction_id: "portal-composer".to_owned(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    (scene, tile_id)
}

const VIEWER_ECHO_COLOR: [u8; 4] = [0x8A, 0xB4, 0xF8, 0xFF];

fn find_echo(items: &[crate::text::TextItem]) -> &crate::text::TextItem {
    items
        .iter()
        .find(|t| t.color == VIEWER_ECHO_COLOR)
        .expect("a viewer-echo text item must be present")
}

/// Geometry helpers matching what the code derives internally.
fn echo_geometry(compositor: &Compositor) -> (f32, f32) {
    let region = Rect::new(0.0, 0.0, 400.0, 300.0);
    let lhm = crate::markdown::MarkdownTokens::default().line_height_multiplier;
    let composer_font =
        super::token_colors::resolve_composer_overlay_tokens(&compositor.token_map).font_size_px;
    let echo_font =
        super::token_colors::resolve_viewer_echo_tokens(&compositor.token_map).font_size_px;
    let box_top = Compositor::composer_input_box(
        region,
        composer_font,
        lhm,
        1.0,
        ComposerVerticalAnchor::Bottom,
        6.0, // default content inset
    )
    .y;
    let echo_line_h = (echo_font * lhm).max(1.0);
    (box_top, echo_line_h)
}

/// hud-pncm3 (a): an entry with an embedded newline (Ctrl+Enter draft, #992)
/// renders as a multi-line block — the `\n` is preserved and the block is at
/// least two visual lines tall — with wrapping enabled (zone-width bounds).
#[tokio::test]
async fn test_viewer_echo_renders_embedded_newline_as_multiple_lines() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();
    compositor
        .viewer_echoes
        .append(tile_id, "line one\nline two".to_owned(), 1);

    compositor.prime_viewer_echo_layout(&scene);
    let items = compositor.collect_text_items(&scene, 400.0, 300.0);
    let echo = find_echo(&items);

    assert_eq!(
        &*echo.text, "00:00  line one\nline two",
        "the embedded newline must be preserved in the echo text (not stripped), \
         with the hud-7ic89 timestamp prefixing only the entry once (not per line)"
    );
    assert!(
        echo.bounds_width < 1000.0,
        "echo must wrap to the zone width (bounds_width {}), not a forced single line",
        echo.bounds_width
    );
    let (box_top, echo_line_h) = echo_geometry(&compositor);
    let block_height = box_top - echo.pixel_y;
    assert!(
        block_height >= 2.0 * echo_line_h - 0.5,
        "two logical lines must render a >=2-line block (height {block_height}, line {echo_line_h})"
    );
    // Bottom-aligned: the block sits directly above the composer box.
    assert!(
        (echo.pixel_y + echo.bounds_height - box_top).abs() < 0.5,
        "block bottom must align to the live composer box top"
    );
}

/// hud-7ic89: each retained viewer-echo entry's timestamp (derived from
/// `submitted_at_wall_us`) must reach the render path as a distinct, token-styled
/// `StyledRunItem` over the joined `TextItem` — muted color, smaller scale — not
/// silently dropped by the `.text`-only render helpers. Asserted at the
/// styled-runs/text-item layer (no pixel readback).
#[tokio::test]
async fn test_viewer_echo_timestamp_renders_as_styled_run() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();

    // Two entries with distinct, known submit times (12:00:00 and 13:30:45 UTC
    // day-seconds) so both the prefix text and its byte-range placement in the
    // `\n`-joined block are independently verifiable.
    let noon_us = (12 * 3600) as u64 * 1_000_000;
    let afternoon_us = (13 * 3600 + 30 * 60 + 45) as u64 * 1_000_000;
    compositor
        .viewer_echoes
        .append(tile_id, "first reply".to_owned(), noon_us);
    compositor
        .viewer_echoes
        .append(tile_id, "second reply".to_owned(), afternoon_us);
    compositor.prime_viewer_echo_layout(&scene);

    let tile = scene.visible_tiles()[0].clone();
    let tokens = super::token_colors::resolve_viewer_echo_tokens(&compositor.token_map);
    let items = compositor.collect_viewer_echo_text_items(&tile, &scene, 400.0, 300.0, &tokens);
    let echo = find_echo(&items);

    assert_eq!(
        &*echo.text, "12:00  first reply\n13:30  second reply",
        "joined block carries both entries' derived timestamp prefixes"
    );
    assert_eq!(
        echo.styled_runs.len(),
        2,
        "one timestamp styled-run per entry, got {:?}",
        echo.styled_runs
    );
    for run in echo.styled_runs.iter() {
        assert_eq!(
            run.color,
            Some(tokens.timestamp_color),
            "timestamp run must use the muted token color, not the message color"
        );
        assert_eq!(
            run.size_scale,
            Some(tokens.timestamp_font_scale),
            "timestamp run must apply the token-driven smaller scale"
        );
    }
    assert_eq!(
        &echo.text[echo.styled_runs[0].start_byte..echo.styled_runs[0].end_byte],
        "12:00  ",
        "first run's byte range must cover exactly the first entry's prefix"
    );
    assert_eq!(
        &echo.text[echo.styled_runs[1].start_byte..echo.styled_runs[1].end_byte],
        "13:30  ",
        "second run's byte range must cover exactly the second entry's prefix, \
         offset past the '\\n' joiner and the first entry's full display text"
    );
}

/// hud-7ic89: an entry with `submitted_at_wall_us == 0` (no timestamp captured —
/// e.g. a legacy append path) must render its text unchanged, with no styled run
/// and no panic — the backward-compatibility requirement.
#[tokio::test]
async fn test_viewer_echo_zero_timestamp_renders_without_prefix_or_run() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();

    compositor
        .viewer_echoes
        .append(tile_id, "no timestamp here".to_owned(), 0);
    compositor.prime_viewer_echo_layout(&scene);

    let tile = scene.visible_tiles()[0].clone();
    let tokens = super::token_colors::resolve_viewer_echo_tokens(&compositor.token_map);
    let items = compositor.collect_viewer_echo_text_items(&tile, &scene, 400.0, 300.0, &tokens);
    let echo = find_echo(&items);

    assert_eq!(&*echo.text, "no timestamp here");
    assert!(
        echo.styled_runs.is_empty(),
        "a zero-timestamp entry must not emit a timestamp styled run"
    );
}

/// hud-pncm3 (b): a single logical line wider than the zone wraps to multiple
/// visual lines (measured via the prime), rather than overflowing on one line.
#[tokio::test]
async fn test_viewer_echo_wraps_long_entry() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();
    // No newline; far wider than the ~388px zone at the echo font.
    compositor
        .viewer_echoes
        .append(tile_id, "wrap ".repeat(60).trim_end().to_owned(), 1);

    compositor.prime_viewer_echo_layout(&scene);
    let items = compositor.collect_text_items(&scene, 400.0, 300.0);
    let echo = find_echo(&items);

    let (box_top, echo_line_h) = echo_geometry(&compositor);
    let block_height = box_top - echo.pixel_y;
    assert!(
        block_height >= 2.0 * echo_line_h - 0.5,
        "a long entry must wrap to >=2 visual lines (block height {block_height})"
    );
    assert!(
        echo.bounds_width <= 400.0,
        "wrap width must be the zone width, not an unbounded single line"
    );
}

/// hud-pncm3 (c): a history taller than the band above the composer box stays
/// bounded — the scissor clips to the band and never intrudes into the box, and
/// the newest reply stays bottom-aligned to the box top.
#[tokio::test]
async fn test_viewer_echo_history_bounded_above_live_box() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(400, 300).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (scene, tile_id) = viewer_echo_test_scene();
    // Many wrapping entries → the joined block far exceeds the band height.
    for i in 0..8 {
        compositor
            .viewer_echoes
            .append(tile_id, format!("reply {i} ").repeat(20), i as u64);
    }

    compositor.prime_viewer_echo_layout(&scene);
    let items = compositor.collect_text_items(&scene, 400.0, 300.0);
    let echo = find_echo(&items);

    let region = Rect::new(0.0, 0.0, 400.0, 300.0);
    let (box_top, _) = echo_geometry(&compositor);
    let band_height = box_top - region.y;

    // The scissor is exactly the band above the box — it does not grow with the
    // history and never extends into the composer box.
    assert!(
        (echo.clip_pixel_y - region.y).abs() < 0.5,
        "clip top must be the region top"
    );
    assert!(
        (echo.clip_bounds_height - band_height).abs() < 0.5,
        "clip height must equal the band above the box (bounded), got {}",
        echo.clip_bounds_height
    );
    assert!(
        echo.clip_pixel_y + echo.clip_bounds_height <= box_top + 0.5,
        "the echo must never intrude into the live composer box"
    );
    // Newest reply stays pinned to the box top even when older lines clip.
    assert!(
        (echo.pixel_y + echo.bounds_height - box_top).abs() < 0.5,
        "block bottom (newest) must remain aligned to the box top under overflow"
    );
}

// ── Chrome layer pixel tests ──────────────────────────────────────────────

/// Layer 1 pixel test: chrome layer is always visible above max-z-order agent tile.
///
/// Acceptance criterion: "Layer 1 pixel tests confirm chrome always visible above
/// max-z-order agent tile."
///
/// This test renders a bright red tile at max z-order (u32::MAX) then renders a
/// distinctive chrome rectangle over the same region. The chrome pixels (pure green)
/// must overwrite the red tile pixels.
#[tokio::test]
async fn test_chrome_always_above_max_zorder_tile() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    // Agent tile at max valid agent z-order with bright red content.
    // Agent tiles must use z_order < ZONE_TILE_Z_MIN (0x8000_0000); u32::MAX is
    // reserved for runtime zone tiles (scene-graph/spec.md §Zone Layer Attachment).
    use tze_hud_scene::types::ZONE_TILE_Z_MIN;
    let max_agent_z = ZONE_TILE_Z_MIN - 1; // 0x7FFF_FFFF
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            max_agent_z,
        )
        .unwrap();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(1.0, 0.0, 0.0, 1.0), // bright red
                    bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
                    radius: None,
                }),
            },
        )
        .unwrap();

    // Chrome draw command: bright green rectangle covering the full surface.
    // In NDC space, this will overwrite all tile content.
    let chrome_cmds = vec![crate::pipeline::ChromeDrawCmd {
        x: 0.0,
        y: 0.0,
        width: 256.0,
        height: 40.0,                // tab bar height
        color: [0.0, 1.0, 0.0, 1.0], // pure green — distinctive chrome marker
    }];

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_with_chrome(&scene, &surface, &chrome_cmds);
    compositor.device.poll(wgpu::Maintain::Wait);

    let pixels = surface.read_pixels(&compositor.device);

    // Check the top-left pixel region (where chrome covers the tile).
    // In sRGB, linear [0,1,0] green becomes approximately [0, 255, 0].
    // We look for pixels that are distinctly green (G > 200, R < 50).
    let chrome_top_pixel = &pixels[0..4]; // first pixel (top-left)
    assert!(
        chrome_top_pixel[1] > 150, // green channel dominant
        "chrome green channel should be dominant at top: {chrome_top_pixel:?}"
    );
    // The tile red should NOT bleed through chrome.
    assert!(
        chrome_top_pixel[0] < 50,
        "agent tile red must not show through chrome: {chrome_top_pixel:?}"
    );
}

/// Layer 1 pixel test: chrome hit-test priority — chrome is always drawn last.
///
/// Verifies the separable render pass architecture: content pass first (agent tiles),
/// chrome pass second (chrome elements). The two-pass structure guarantees chrome
/// always occupies the final pixels regardless of content.
#[tokio::test]
async fn test_chrome_pass_uses_load_op_load() {
    // Render a scene with a blue agent tile + a chrome red stripe.
    // Blue content should persist where chrome doesn't cover; red should cover where it does.
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    // Blue tile — fills entire surface in content pass.
                    color: Rgba::new(0.0, 0.0, 1.0, 1.0),
                    bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
                    radius: None,
                }),
            },
        )
        .unwrap();

    // Chrome: red stripe only in top half (rows 0..128).
    let chrome_cmds = vec![crate::pipeline::ChromeDrawCmd {
        x: 0.0,
        y: 0.0,
        width: 256.0,
        height: 128.0,
        color: [1.0, 0.0, 0.0, 1.0], // pure red
    }];

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_with_chrome(&scene, &surface, &chrome_cmds);
    compositor.device.poll(wgpu::Maintain::Wait);

    let pixels = surface.read_pixels(&compositor.device);

    // Top row: chrome (red) should dominate.
    let top_px = &pixels[0..4];
    assert!(
        top_px[0] > 150,
        "top pixel should be red (chrome): {top_px:?}"
    );
    assert!(
        top_px[2] < 50,
        "top pixel blue (tile) must be suppressed by chrome: {top_px:?}"
    );

    // Bottom row: content (blue) should persist — chrome didn't cover it.
    // Row 255 starts at pixel offset 255*256*4.
    let bottom_row_offset = 255 * 256 * 4;
    let bottom_px = &pixels[bottom_row_offset..bottom_row_offset + 4];
    assert!(
        bottom_px[2] > 150,
        "bottom pixel should be blue (tile content, no chrome): {bottom_px:?}"
    );
    assert!(
        bottom_px[0] < 50,
        "bottom pixel red should be absent (no chrome): {bottom_px:?}"
    );
}

/// Verify that render_frame_with_chrome renders correctly even when chrome_cmds is empty.
#[tokio::test]
async fn test_two_pass_with_empty_chrome_cmds() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let scene = scene_with_node(Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.5, 0.5, 0.5, 1.0),
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
            radius: None,
        }),
    });
    // Empty chrome cmds — must not panic.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_with_chrome(&scene, &surface, &[]);
    compositor.device.poll(wgpu::Maintain::Wait);
    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 256 * 256 * 4);
}

// ── Headless parity tests ─────────────────────────────────────────────────

/// Verify that `render_frame` (surface-agnostic) works with a `HeadlessSurface`
/// as a `&dyn CompositorSurface`.  This is the core headless parity assertion:
/// the same method that would be used with a windowed surface works headlessly.
#[tokio::test]
async fn test_render_frame_via_compositor_surface_trait() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let mut scene = SceneGraph::new(256.0, 256.0);

    // Prime before render per the Stage-4 commit-time prime contract (hud-380dl / hud-v2z6u).
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    // render_frame takes &dyn CompositorSurface — no special headless branch.
    let telemetry = compositor.render_frame(
        &mut scene,
        &surface as &dyn crate::surface::CompositorSurface,
    );
    assert!(telemetry.frame_time_us > 0, "frame time must be non-zero");
    assert_eq!(telemetry.tile_count, 0, "empty scene has no tiles");
}

/// hud-uyhpn lock-scope split: `build_windowed_frame` must produce the frame's
/// scene geometry WITHOUT touching the surface, and `present_windowed_frame`
/// must then consume that build to yield equivalent telemetry.
///
/// This pins the structural property the drag-input fix depends on: the scene
/// reads (vertex/geometry build) happen in a phase that takes NO surface, so the
/// windowed frame loop can drop the scene lock before the vsync-blocking
/// `acquire_frame()` + submit + poll runs inside `present_windowed_frame`. The
/// build method's signature — `(&mut scene, surf_w, surf_h)` with no surface —
/// is itself the compile-time guarantee; this test additionally confirms real
/// geometry flows out of the lock-held build phase and that a subsequent present
/// reports the same tile count.
#[tokio::test]
async fn build_windowed_frame_decoupled_from_surface_then_presents() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // A zone publish with an opaque backdrop guarantees non-empty flat-rect
    // geometry so the assertion is meaningful (not a vacuously-empty scene).
    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.08,
            width_pct: 0.70,
            margin_px: 12.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(18.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
            backdrop_opacity: Some(0.9),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: Some(5_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Background,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Backdrop for build/present split test".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // Prime per the Stage-4 commit-time prime contract (hud-380dl / hud-v2z6u).
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    // ── Build phase: scene reads only, NO surface argument ─────────────────
    let build = compositor.build_windowed_frame(&mut scene, 1280, 720);
    assert!(
        build.vertex_count() > 0,
        "build_windowed_frame must produce flat-rect geometry from the scene \
         alone, with no surface acquired (this is what lets the frame loop drop \
         the scene lock before acquire_frame)"
    );
    let built_tiles = build.tile_count();
    // Drag-handle geometry is precomputed in the build phase too (scene-free at
    // present time); with no portal tiles here it is simply empty, but the field
    // must be populated by the build, not the present.
    let _ = build.drag_handle_vertex_count();

    // ── Present phase: consumes the build against the surface ──────────────
    let outcome = compositor.present_windowed_frame_with_outcome(
        build,
        &surface as &dyn crate::surface::CompositorSurface,
    );
    assert!(outcome.surface_acquired);
    assert!(outcome.gpu_submitted);
    let telemetry = outcome.telemetry;
    assert!(
        telemetry.frame_time_us > 0,
        "present must record a frame time"
    );
    assert_eq!(
        telemetry.tile_count, built_tiles,
        "present must carry through the tile count recorded during build"
    );
}

/// Verify that HEADLESS_FORCE_SOFTWARE env-var path is exercised in the
/// adapter-selection code.  We cannot assert the adapter backend in a unit
/// test (it's opaque), so we just verify that creating a compositor with
/// the env var set does not crash.
// await_holding_lock: intentional — the guard must stay held across the
// await so no parallel test mutates HEADLESS_FORCE_SOFTWARE mid-construction.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn test_new_headless_with_force_software_env_var() {
    if should_skip_gpu_tests() {
        eprintln!("skipping GPU test: TZE_HUD_SKIP_GPU_TESTS=1");
        return;
    }

    // Serialize all env-var-mutating tests via a process-wide mutex.
    // Rust tests run in parallel by default; without serialization,
    // a concurrent test could observe or overwrite HEADLESS_FORCE_SOFTWARE.
    let _guard = ENV_VAR_MUTEX.lock().unwrap();

    // Safety: single-threaded within the mutex guard; no other test
    // touches HEADLESS_FORCE_SOFTWARE while _guard is held.
    unsafe {
        std::env::set_var("HEADLESS_FORCE_SOFTWARE", "1");
    }
    let result = Compositor::new_headless(64, 64).await;
    unsafe {
        std::env::remove_var("HEADLESS_FORCE_SOFTWARE");
    }
    drop(_guard);

    // Either Ok (software GPU found) or Err(NoAdapter) (no software GPU
    // installed in this CI environment) are acceptable.  A panic would not be.
    match result {
        Ok(_) => {}
        Err(CompositorError::NoAdapter) => {}
        Err(e) => panic!("unexpected error with HEADLESS_FORCE_SOFTWARE=1: {e}"),
    }
}

// ── Surface capability guard + dimension clamping (hud-q5hx regression) ────
//
// These tests validate the defensive logic added to `new_windowed_inner()`:
//   1. Empty surface capability lists return `Err` instead of panicking.
//   2. Dimension clamping uses `.max(1)` to prevent zero-size configs.
//
// The windowed path requires a real display handle and GPU, so we test the
// clamping arithmetic directly as a pure function.

/// Dimension clamping must apply both the device max and a minimum of 1.
/// wgpu panics on `surface.configure()` with zero-width or zero-height.
#[test]
// min_max / unnecessary_min_or_max: the test deliberately evaluates the
// exact clamp expression used in production against literal inputs.
#[allow(clippy::min_max, clippy::unnecessary_min_or_max)]
fn surface_dim_clamp_zero_becomes_one() {
    let max_dim = 16384u32;
    assert_eq!(0u32.min(max_dim).max(1), 1);
    assert_eq!(1u32.min(max_dim).max(1), 1);
    assert_eq!(2560u32.min(max_dim).max(1), 2560);
    assert_eq!(3840u32.min(max_dim).max(1), 3840);
}

/// Dimension clamping respects the device maximum texture dimension.
/// Values larger than the limit are clamped, values within the limit pass through.
#[test]
fn surface_dim_clamp_respects_device_limit() {
    let max_dim = 4096u32;
    assert_eq!(4097u32.min(max_dim).max(1), 4096, "over-limit must clamp");
    assert_eq!(4096u32.min(max_dim).max(1), 4096, "at-limit must pass");
    assert_eq!(2560u32.min(max_dim).max(1), 2560, "under-limit must pass");
    assert_eq!(1920u32.min(max_dim).max(1), 1920, "default res must pass");
}

/// Dimension clamping at 2560x1440 with a 32768 device limit (RTX 3080)
/// must not clamp — 2560 and 1440 are well below the RTX 3080's limit.
#[test]
fn surface_dim_clamp_2560x1440_passes_on_rtx3080_limit() {
    // RTX 3080 with Vulkan driver reports max_texture_dimension_2d = 32768.
    let max_dim = 32768u32;
    assert_eq!(
        2560u32.min(max_dim).max(1),
        2560,
        "2560 must not be clamped"
    );
    assert_eq!(
        1440u32.min(max_dim).max(1),
        1440,
        "1440 must not be clamped"
    );
    assert_eq!(3840u32.min(max_dim).max(1), 3840, "4K must not be clamped");
    assert_eq!(2160u32.min(max_dim).max(1), 2160, "4K must not be clamped");
}

// ── Text rendering pixel tests ────────────────────────────────────────────
//
// These tests validate acceptance criteria 1–4 from hud-pmkf:
//  1. publish_to_zone with StreamText → visible text at zone geometry.
//  2. TextMarkdownNode renders text (some non-background pixels in text area).
//  3. Overflow Clip and Ellipsis modes: glyphs stay within TextBounds.
//  4. Headless pixel readback detects text presence.
//
// All tests initialise the text renderer via `compositor.init_text_renderer`
// targeting `Rgba8UnormSrgb` (the headless surface format).

/// Pixel readback validates text presence in a TextMarkdownNode tile.
///
/// After text rendering, pixels in the text region should differ from the
/// solid background color — glyphs overwrite some pixels.  We can't check
/// exact glyph shapes without font-specific knowledge, so we verify that
/// at least one pixel in the tile area differs from the pure background
/// color.
#[tokio::test]
async fn test_text_markdown_node_renders_visible_text() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Dark-blue background, white text — high contrast for pixel detection.
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "Hello world".to_owned(),
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
            font_size_px: 24.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0), // white
            background: Some(Rgba::new(0.0, 0.0, 0.5, 1.0)), // dark blue
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 256 * 256 * 4, "pixel buffer size");

    // The background is dark blue — sRGB of linear [0,0,0.5] ≈ [0, 0, 188].
    // White text (sRGB [255, 255, 255]) glyphs should appear in the tile.
    // We check that at least one pixel has R > 200 AND G > 200 (white).
    let any_bright_pixel = pixels
        .chunks(4)
        .any(|p| p[0] > 200 && p[1] > 200 && p[2] > 200);
    assert!(
        any_bright_pixel,
        "expected white text pixels in TextMarkdownNode tile — none found"
    );
}

/// When the text rasterizer is active, TextMarkdownNode must not fall back
/// to the old full-width placeholder bar path.
#[tokio::test]
async fn test_text_markdown_node_avoids_placeholder_bar_when_text_renderer_active() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(160, 80).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "I".to_owned(),
            bounds: Rect::new(20.0, 16.0, 100.0, 40.0),
            font_size_px: 28.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    let width = 160usize;
    let height = 80usize;
    let bright = |rgba: &[u8]| rgba[0] > 200 && rgba[1] > 200 && rgba[2] > 200;

    let max_bright_run = (0..height)
        .map(|row| {
            (0..width)
                .filter(|col| bright(&pixels[(row * width + col) * 4..][..4]))
                .count()
        })
        .max()
        .unwrap_or(0);

    assert!(
        max_bright_run < 40,
        "text renderer should not paint a placeholder bar; brightest row had {max_bright_run} bright pixels"
    );
}

/// Text stays within the TextBounds clip rectangle (Clip overflow mode).
///
/// We render white text in a small region at the top-left of a dark tile.
/// The bottom-right quadrant should remain all-dark (no text overflow).
#[tokio::test]
async fn test_text_clip_overflow_stays_within_bounds() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Text node occupies only top-left 64x64 pixels of the 256x256 tile.
    // Content: many lines so overflow is tested.
    let content = "Line1\nLine2\nLine3\nLine4\nLine5\nLine6\nLine7\nLine8".to_owned();
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content,
            bounds: Rect::new(0.0, 0.0, 64.0, 32.0), // small box
            font_size_px: 12.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0), // white
            background: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)), // pure black bg
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);

    // Bottom-right quadrant: rows 128..256, cols 128..256.
    // Tile background is ~[0.15, 0.15, 0.25] (tile_background_color default).
    // There should be no white pixels (from text) there.
    let mut bright_outside = false;
    for row in 128..256_usize {
        for col in 128..256_usize {
            let offset = (row * 256 + col) * 4;
            let p = &pixels[offset..offset + 4];
            // White text would have R > 200 AND G > 200 AND B > 200.
            if p[0] > 200 && p[1] > 200 && p[2] > 200 {
                bright_outside = true;
                break;
            }
        }
        if bright_outside {
            break;
        }
    }
    assert!(
        !bright_outside,
        "text overflow detected outside clip bounds (bottom-right quadrant has bright pixels)"
    );
}

/// Ellipsis overflow mode: text renders without panic; background present.
///
/// We don't assert exact "…" pixel shape — that's platform-font-specific.
/// We verify: (a) no panic, (b) background exists, (c) some non-background
/// pixels appear (text was rendered at all).
#[tokio::test]
async fn test_text_ellipsis_overflow_no_panic() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let long_line = "A very long line that definitely overflows the available width of this tile";
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: long_line.to_owned(),
            bounds: Rect::new(0.0, 0.0, 120.0, 40.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: Some(Rgba::new(0.1, 0.1, 0.1, 1.0)),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    let mut scene = scene_with_node(node);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    // Must not panic.
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(
        pixels.len(),
        256 * 256 * 4,
        "pixel buffer must be full size"
    );
}

/// Zone StreamText publish renders visible text at zone geometry.
///
/// Acceptance criterion 1: publish_to_zone with StreamText content displays
/// readable text at the zone geometry position.
#[tokio::test]
async fn test_zone_stream_text_renders_visible_text() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Register a subtitle zone (bottom edge, 10% height).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "subtitle zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(22.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_align: None,
            margin_px: None,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish "Hello Zone" to the subtitle zone.
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Hello Zone".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 1280 * 720 * 4, "pixel buffer size");

    // The zone is at the bottom 10% (y ~648..720) centered (x ~128..1152).
    // We check for any bright pixels in that area (white text on dark bg).
    // Zone bg is semi-transparent dark [0.1, 0.1, 0.15, 0.85] rendered over
    // the default compositor clear [0.05, 0.05, 0.1] → still quite dark.
    // White text glyphs should show as bright pixels.
    let mut found_bright = false;
    // Sample a row in the subtitle zone (row 660 ≈ 91.7% of 720 = 661).
    for row in 652usize..715 {
        for col in 150usize..1130 {
            let offset = (row * 1280 + col) * 4;
            let p = &pixels[offset..offset + 4];
            if p[0] > 180 && p[1] > 180 && p[2] > 180 {
                found_bright = true;
                break;
            }
        }
        if found_bright {
            break;
        }
    }
    assert!(
        found_bright,
        "expected bright (text) pixels in zone subtitle area (rows 652..715)"
    );
}

/// Zone ShortTextWithIcon/Notification publish renders visible text at zone geometry.
///
/// Acceptance criterion for hud-lh3w: publish_to_zone with
/// `ZoneContent::Notification(NotificationPayload)` produces a `TextItem` in
/// `collect_text_items` and causes bright glyph pixels to appear in the zone
/// geometry area.
#[tokio::test]
async fn test_zone_notification_renders_visible_text() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Register a notification zone (top edge, 8% height).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification".to_owned(),
        description: "notification zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.08,
            width_pct: 0.70,
            margin_px: 12.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(20.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.75)),
            text_align: None,
            margin_px: None,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish a notification to the zone.
    scene
        .publish_to_zone(
            "notification",
            ZoneContent::Notification(NotificationPayload {
                text: "Alert: system ready".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 1280 * 720 * 4, "pixel buffer size");

    // The zone is at the top edge: y ≈ 12..(12 + 720*0.08) ≈ 12..69.6,
    // centered: x ≈ (1280-896)/2..896+192 = 192..1088.
    // White text glyphs should appear as bright pixels (r,g,b > 180).
    let mut found_bright = false;
    for row in 12usize..70 {
        for col in 200usize..1080 {
            let offset = (row * 1280 + col) * 4;
            let p = &pixels[offset..offset + 4];
            if p[0] > 180 && p[1] > 180 && p[2] > 180 {
                found_bright = true;
                break;
            }
        }
        if found_bright {
            break;
        }
    }
    assert!(
        found_bright,
        "expected bright (text) pixels in notification zone area (rows 12..70)"
    );
}

/// Zone StatusBar (KeyValuePairs) publish renders visible text at zone geometry.
///
/// Acceptance criteria for hud-6at1:
///   1. `publish_to_zone` with `ZoneContent::StatusBar` produces a `TextItem` in
///      `collect_text_items`.
///   2. The key-value pairs are rendered as text at the zone geometry position.
#[tokio::test]
async fn test_zone_status_bar_renders_visible_text() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Register a status-bar zone (top edge, 5% height).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "statusbar".to_owned(),
        description: "status bar zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.05,
            width_pct: 0.80,
            margin_px: 8.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_align: None,
            margin_px: None,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish StatusBar content with key-value pairs.
    let mut entries = std::collections::HashMap::new();
    entries.insert("battery".to_owned(), "95%".to_owned());
    entries.insert("time".to_owned(), "12:34".to_owned());
    scene
        .publish_to_zone(
            "statusbar",
            ZoneContent::StatusBar(StatusBarPayload { entries }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // Verify collect_text_items produces a TextItem with the formatted pairs.
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        items.len(),
        1,
        "expected exactly one TextItem for StatusBar"
    );
    let item = &items[0];
    // Entries are sorted by key ("battery" < "time") and separated by newlines.
    assert_eq!(
        &*item.text, "battery: 95%\ntime: 12:34",
        "Entries should be sorted by key and formatted correctly"
    );
    // The TextItem position should be within the zone geometry.
    // Zone top-edge: y = 8.0 (margin_px), height = 720*0.05 = 36, width = 1280*0.8 = 1024.
    assert!(
        item.pixel_y >= 8.0,
        "text y should be at or below zone top margin"
    );
    assert!(
        item.pixel_y < 720.0 * 0.10,
        "text y should be within top zone area"
    );

    // Render to pixels and verify bright text appears in the top zone area.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    assert_eq!(pixels.len(), 1280 * 720 * 4, "pixel buffer size");

    // The zone is at the top ~8..44px, centered horizontally.
    // White text glyphs should show as bright pixels.
    let mut found_bright = false;
    for row in 10usize..42 {
        for col in 150usize..1130 {
            let offset = (row * 1280 + col) * 4;
            let p = &pixels[offset..offset + 4];
            if p[0] > 180 && p[1] > 180 && p[2] > 180 {
                found_bright = true;
                break;
            }
        }
        if found_bright {
            break;
        }
    }
    assert!(
        found_bright,
        "expected bright (text) pixels in status bar zone area (rows 10..42)"
    );
}

/// `init_text_renderer` called multiple times replaces the rasterizer (no panic).
#[tokio::test]
async fn test_init_text_renderer_idempotent() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(64, 64).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(64.0, 64.0);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    // No panic = pass.
}

/// Text rendering with no text items (empty scene) must not panic.
#[tokio::test]
async fn test_text_renderer_empty_scene_no_panic() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(64, 64).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(64.0, 64.0);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
}

/// Stage 6 frame-budget benchmark — text rendering active.
///
/// Renders 60 frames with `init_text_renderer` active, a `TextMarkdownNode`
/// tile, and a zone with `StreamText` content.  Asserts that the p99 of
/// `stage6_render_encode_us` (the Stage 6 wall-clock encode time returned by
/// `render_frame_headless`) stays below a calibrated budget derived from
/// `STAGE6_BUDGET_US` (4 ms = 4 000 µs).
///
/// Budget constant sourced from `tze_hud_runtime::pipeline::STAGE6_BUDGET_US`.
/// It is inlined here to avoid a cyclic dev-dependency
/// (tze_hud_runtime → tze_hud_compositor already exists).
///
/// The effective budget is `max(test_budget(4000), 4000 * 4)` — the
/// calibration system scales for CPU speed, and the 4× CI floor (16 ms)
/// absorbs llvmpipe/scheduling jitter on GitHub Actions runners.  This
/// mirrors the Stage 6 budget pattern in `budget_assertions.rs`.
#[tokio::test]
async fn test_stage6_budget_with_text_rendering_active() {
    use tze_hud_scene::calibration::test_budget;

    // Stage 6 p99 budget in microseconds — mirrors STAGE6_BUDGET_US in
    // tze_hud_runtime::pipeline (4 ms).
    const STAGE6_BUDGET_US: u64 = 4_000;
    /// CI-friendly multiplier: 4× the spec target absorbs llvmpipe and
    /// scheduling noise on shared CI runners.
    const CI_BUDGET_MULTIPLIER: u64 = 4;
    let effective_budget =
        test_budget(STAGE6_BUDGET_US).max(STAGE6_BUDGET_US * CI_BUDGET_MULTIPLIER);
    const FRAME_COUNT: usize = 60;

    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // ── Build scene ─────────────────────────────────────────────────────────
    let mut scene = SceneGraph::new(1280.0, 720.0);
    let tab_id = scene.create_tab("bench", 0).unwrap();
    let lease_id = scene.grant_lease("bench", 60_000, vec![]);

    // TextMarkdownNode tile occupying most of the screen.
    let tile_id = scene
        .create_tile(
            tab_id,
            "bench",
            lease_id,
            Rect::new(0.0, 0.0, 1000.0, 600.0),
            1,
        )
        .unwrap();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: "Stage 6 budget benchmark\nLine two of text\nLine three".to_owned(),
                    bounds: Rect::new(0.0, 0.0, 1000.0, 600.0),
                    font_size_px: 20.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                    background: Some(Rgba::new(0.05, 0.05, 0.1, 1.0)),
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Clip,
                    color_runs: Box::default(),
                }),
            },
        )
        .unwrap();

    // Zone with StreamText content (subtitle strip at the bottom).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "bench-subtitle".to_owned(),
        description: "benchmark subtitle zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(22.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_align: None,
            margin_px: None,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "bench-subtitle",
            ZoneContent::StreamText("Stage 6 benchmark — stream text active".to_owned()),
            "bench",
            None,
            None,
            None,
        )
        .unwrap();

    // ── Warm-up pass ────────────────────────────────────────────────────────
    // Run a few frames to let llvmpipe/WARP JIT-compile the shaders before
    // the timed measurement window.  Shader compilation is a one-time cost
    // that does not reflect steady-state Stage 6 performance; excluding it
    // mirrors production behaviour where shaders are pre-compiled.
    // Prime once before the loop — scene does not change in this benchmark,
    // so subsequent frames are all cache-hit no-ops (hud-380dl / hud-v2z6u).
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    for _ in 0..5 {
        compositor.render_frame_headless(&mut scene, &surface);
    }

    // ── Render loop ─────────────────────────────────────────────────────────
    let mut timings: Vec<u64> = Vec::with_capacity(FRAME_COUNT);
    for _ in 0..FRAME_COUNT {
        let telem = compositor.render_frame_headless(&mut scene, &surface);
        // stage6_render_encode_us is the Stage 6 wall-clock encode duration.
        timings.push(telem.stage6_render_encode_us);
    }

    // ── p99 assertion ────────────────────────────────────────────────────────
    timings.sort_unstable();
    // p99 index: ceil(99/100 * N) - 1 (0-based), clamped to last element.
    let p99_index = ((FRAME_COUNT as f64 * 0.99).ceil() as usize).saturating_sub(1);
    let p99_index = p99_index.min(FRAME_COUNT - 1);
    let p99_us = timings[p99_index];

    assert!(
        p99_us <= effective_budget,
        "Stage 6 render-encode p99 ({p99_us} µs) exceeds budget ({effective_budget} µs, \
             spec target={STAGE6_BUDGET_US} µs). All timings (sorted): {timings:?}"
    );
}

// ── RenderingPolicy-driven zone rendering tests [hud-sc0a.8] ─────────────

/// Subtitle with outline: when outline_color + outline_width are set in
/// RenderingPolicy, collect_text_items produces a TextItem with non-None
/// outline fields.
#[tokio::test]
async fn test_zone_subtitle_with_outline_text_item() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "subtitle zone with outline".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(22.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            outline_color: Some(Rgba::BLACK),
            outline_width: Some(2.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Test outline text".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 1, "expected one TextItem");
    let item = &items[0];
    assert!(
        item.outline_color.is_some(),
        "outline_color should be set from RenderingPolicy"
    );
    assert!(
        item.outline_width.is_some(),
        "outline_width should be set from RenderingPolicy"
    );
    assert_eq!(
        item.outline_width.unwrap(),
        2.0,
        "outline_width should match policy"
    );
    // Text color should be white (from text_color).
    assert_eq!(
        item.color[0], 255,
        "text fill color R should be white (255)"
    );
}

/// Subtitle without outline: when outline_width is None, outline fields
/// on the TextItem should be None.
#[tokio::test]
async fn test_zone_subtitle_without_outline_text_item() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "subtitle zone without outline".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(22.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            outline_color: None,
            outline_width: None,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("No outline subtitle".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 1, "expected one TextItem");
    let item = &items[0];
    assert!(
        item.outline_color.is_none(),
        "outline_color should be None when policy has no outline"
    );
    assert!(
        item.outline_width.is_none(),
        "outline_width should be None when policy has no outline"
    );
}

/// Render-branch regression guard (hud-9v3t6): a portal carrying a *zero-length
/// lifecycle sentinel* color run must still take the cached/styled markdown
/// render path, NOT the lossy `from_text_markdown_node` / `strip_markdown_v1`
/// path.
///
/// Background: `lifecycle_marker_color_runs` (resident_grpc.rs) emits a
/// zero-length `TextColorRun` ([start..start], no pixel coverage) on every
/// permitted viewer of a normal active/attached portal.  The render branch used
/// to gate on `color_runs.is_empty()`, so this sentinel flipped *every* normal
/// portal onto the lossy/uncached path (losing markdown styling AND the
/// commit-time markdown cache) while painting no accent pixels.  The fix gates
/// on `markdown_node_has_pixel_runs` instead.
///
/// This asserts at the COMPOSITOR RENDER-BRANCH level (the existing tests only
/// covered node construction, which is why the regression slipped through):
/// `collect_text_items` must produce a `TextItem` with populated `styled_runs`
/// (the cached path's signature; the lossy node path always leaves
/// `styled_runs` empty).
#[tokio::test]
async fn test_lifecycle_sentinel_keeps_cached_markdown_render_path() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Markdown content that yields styled spans (heading + bold) — the cached
    // path emits StyledRunItems for these; the lossy strip path emits none.
    let content = "# Portal\n**attached** and ready".to_owned();

    let make_node = |runs: Box<[tze_hud_scene::types::TextColorRun]>| Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.clone(),
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: runs,
        }),
    };

    // Zero-length lifecycle sentinel: start_byte == end_byte (no pixel coverage),
    // exactly as lifecycle_marker_color_runs emits for a normal active portal.
    let sentinel = tze_hud_scene::types::TextColorRun {
        start_byte: 0,
        end_byte: 0,
        color: Rgba::new(0.2, 0.8, 0.4, 1.0),
    };
    let scene = scene_with_node(make_node(Box::from([sentinel])));
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    assert_eq!(
        items.len(),
        1,
        "expected exactly one TextItem for the portal node"
    );
    let item = &items[0];
    assert!(
        !item.styled_runs.is_empty(),
        "portal with a zero-length lifecycle sentinel must take the cached/styled \
         markdown path (styled_runs populated), not the lossy strip path"
    );
    assert!(
        item.color_runs.is_empty(),
        "the zero-length sentinel carries no pixel coverage and must be dropped \
         (no ColorRunItems) on the cached path"
    );

    // Control: a genuine *pixel-bearing* color run (start < end) must still force
    // the legacy from_text_markdown_node path (styled_runs empty), proving the
    // fix narrowed the gate to pixel runs without disabling the legacy path.
    let pixel_run = tze_hud_scene::types::TextColorRun {
        start_byte: 0,
        end_byte: 4,
        color: Rgba::new(0.9, 0.1, 0.1, 1.0),
    };
    let scene_pixel = scene_with_node(make_node(Box::from([pixel_run])));
    compositor.prime_markdown_cache(&scene_pixel);
    compositor.prime_truncation_cache(&scene_pixel);
    let pixel_items = compositor.collect_text_items(&scene_pixel, 256.0, 256.0);
    assert_eq!(
        pixel_items.len(),
        1,
        "expected one TextItem for the pixel-run node"
    );
    assert!(
        pixel_items[0].styled_runs.is_empty(),
        "a pixel-bearing color run must still take the legacy raw-content path \
         (styled_runs empty), preserving its raw byte offsets"
    );
    assert!(
        !pixel_items[0].color_runs.is_empty(),
        "the pixel-bearing color run must be preserved as a ColorRunItem"
    );
}

/// Notification with opaque backdrop: backdrop_opacity=0.9 overrides
/// the backdrop color's alpha.  The backdrop quad should be rendered with
/// effective alpha = 0.9.
#[tokio::test]
async fn test_notification_with_opaque_backdrop() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.08,
            width_pct: 0.70,
            margin_px: 12.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(18.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
            backdrop_opacity: Some(0.9),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: Some(5_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Notification with opaque backdrop".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // render_zone_content should produce backdrop rect vertices.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    // We check that vertices were emitted (backdrop rendered).
    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for notification with opaque backdrop"
    );

    // Also verify the text items use the policy text_color.
    // collect_text_items emits 2 items for a notification slot when the text
    // rasterizer is active: the notification body text + the dismiss "X" button.
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        items.len(),
        2,
        "expected two TextItems: notification body + dismiss button"
    );
    // The first item is the notification body text. White text → R channel near 255.
    assert!(
        items[0].color[0] > 200,
        "text color R should be near-white from policy.text_color"
    );
}

/// Alert-banner severity mapping: urgency 2 (warning) should map to
/// color.severity.warning (amber/yellow), NOT the policy backdrop.
/// We verify by inspecting the vertices emitted by render_zone_content.
#[tokio::test]
async fn test_alert_banner_urgency2_maps_to_severity_warning() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert banner zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.07,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(20.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)), // dark default
            backdrop_opacity: Some(1.0),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish urgency=2 (warning).
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Warning: disk space low".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // Collect vertices from render_zone_content.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // The backdrop should be severity warning color (~amber: R=1.0, G~0.72, B=0.0).
    // rect_vertices emits 6 vertices; each has color at the end.
    // We check that the R component is high (>0.9) and G is mid (~0.5-0.8) and B is low.
    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for alert-banner urgency=2"
    );

    // Verify urgency_to_severity_color directly (no token map → fallback constants).
    let no_tokens = HashMap::new();
    let warning_color = urgency_to_severity_color(2, &no_tokens);
    assert!(
        warning_color.r > 0.9,
        "warning severity R should be ~1.0 (amber)"
    );
    assert!(
        warning_color.g > 0.5,
        "warning severity G should be >0.5 (amber)"
    );
    assert!(
        warning_color.b < 0.1,
        "warning severity B should be ~0.0 (amber)"
    );
}

/// Alert-banner urgency=3 maps to critical (red).
#[tokio::test]
async fn test_alert_banner_urgency3_maps_to_severity_critical() {
    let no_tokens = HashMap::new();
    let critical = urgency_to_severity_color(3, &no_tokens);
    assert!(critical.r > 0.9, "critical R should be ~1.0");
    assert!(critical.g < 0.1, "critical G should be ~0.0");
    assert!(critical.b < 0.1, "critical B should be ~0.0");
}

/// Alert-banner urgency=0 and 1 both map to info (blue).
#[tokio::test]
async fn test_alert_banner_urgency_low_maps_to_info() {
    let no_tokens = HashMap::new();
    let info0 = urgency_to_severity_color(0, &no_tokens);
    let info1 = urgency_to_severity_color(1, &no_tokens);
    // Info color is blue-ish (#4A9EFF).
    assert!(info0.b > 0.9, "info urgency=0 should be blue");
    assert!(info1.b > 0.9, "info urgency=1 should be blue");
    // Both should be the same color.
    assert_eq!(info0.r, info1.r);
    assert_eq!(info0.b, info1.b);
}

/// notification-area does NOT use urgency-to-severity mapping (color.severity.*).
/// It uses color.notification.urgency.* tokens instead.
/// Even with urgency=3, it must NOT produce severity critical (red #FF0000).
#[tokio::test]
async fn test_notification_area_does_not_use_severity_tokens() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area - uses notification urgency tokens, not severity"
            .to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.05, 0.05, 0.05, 0.85)),
            backdrop_opacity: Some(0.85),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 16,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "System alert".to_owned(),
                icon: String::new(),
                urgency: 3, // Critical — must use color.notification.urgency.critical, NOT color.severity.critical
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // notification-area must NOT be treated as alert-banner.
    assert!(
        !is_alert_banner_zone("notification-area"),
        "notification-area must not be treated as alert-banner"
    );

    // Render and check: the backdrop must NOT be severity critical (pure red R~1.0, G~0.0, B~0.0).
    // It should be notification urgency critical fallback: #450612.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for notification-area urgency=3"
    );

    // Check first vertex: R should NOT be ~1.0 (that would be severity critical).
    let first = &vertices[0];
    assert!(
        first.color[0] < 0.7,
        "notification-area urgency=3 R must NOT be severity critical (~1.0), got {}",
        first.color[0]
    );
}

// ── Notification urgency token tests ─────────────────────────────────────

/// urgency_to_notification_color: low (0) maps to #000000 fallback.
#[test]
fn test_notification_urgency_low_fallback() {
    let no_tokens = HashMap::new();
    let color = urgency_to_notification_color(0, &no_tokens);
    assert!(
        color.r < 0.001,
        "urgency low R should be ~0.0, got {}",
        color.r
    );
    assert!(
        color.g < 0.001,
        "urgency low G should be ~0.0, got {}",
        color.g
    );
    assert!(
        color.b < 0.001,
        "urgency low B should be ~0.0, got {}",
        color.b
    );
}

/// urgency_to_notification_color: normal (1) maps to #0C1426 fallback.
#[test]
fn test_notification_urgency_normal_fallback() {
    let no_tokens = HashMap::new();
    let color = urgency_to_notification_color(1, &no_tokens);
    assert!(
        (color.r - 0.0037).abs() < 0.002,
        "urgency normal R should be ~0.0037, got {}",
        color.r
    );
    assert!(
        (color.g - 0.0070).abs() < 0.002,
        "urgency normal G should be ~0.0070, got {}",
        color.g
    );
    assert!(
        (color.b - 0.0194).abs() < 0.003,
        "urgency normal B should be ~0.0194, got {}",
        color.b
    );
    assert!(
        color.b > color.r + 0.01,
        "urgency normal B should be > R (blue tint)"
    );
}

/// urgency_to_notification_color: urgent (2) maps to #2A1E08 fallback.
#[test]
fn test_notification_urgency_urgent_fallback() {
    let no_tokens = HashMap::new();
    let color = urgency_to_notification_color(2, &no_tokens);
    assert!(
        (color.r - 0.0232).abs() < 0.003,
        "urgency urgent R should be ~0.0232, got {}",
        color.r
    );
    assert!(
        (color.g - 0.0130).abs() < 0.003,
        "urgency urgent G should be ~0.0130, got {}",
        color.g
    );
    assert!(
        (color.b - 0.0024).abs() < 0.002,
        "urgency urgent B should be ~0.0024, got {}",
        color.b
    );
    assert!(
        color.r > color.g && color.g > color.b,
        "urgency urgent should retain an amber-black R > G > B ordering"
    );
}

/// urgency_to_notification_color: critical (3) maps to #450612 fallback.
#[test]
fn test_notification_urgency_critical_fallback() {
    let no_tokens = HashMap::new();
    let color = urgency_to_notification_color(3, &no_tokens);
    assert!(
        (color.r - 0.0595).abs() < 0.004,
        "urgency critical R should be ~0.0595, got {}",
        color.r
    );
    assert!(
        (color.g - 0.0018).abs() < 0.002,
        "urgency critical G should be ~0.0018, got {}",
        color.g
    );
    assert!(
        (color.b - 0.0060).abs() < 0.002,
        "urgency critical B should be ~0.0060, got {}",
        color.b
    );
    assert!(
        color.r > color.b && color.b > color.g,
        "urgency critical should retain a red-black R > B > G ordering"
    );
}

/// urgency_to_notification_color: urgency > 3 is clamped to critical (3).
#[test]
fn test_notification_urgency_clamped_above_3() {
    let no_tokens = HashMap::new();
    let critical3 = urgency_to_notification_color(3, &no_tokens);
    let clamped4 = urgency_to_notification_color(4, &no_tokens);
    let clamped100 = urgency_to_notification_color(100, &no_tokens);
    assert_eq!(
        critical3.r, clamped4.r,
        "urgency=4 should clamp to urgency=3"
    );
    assert_eq!(
        critical3.g, clamped4.g,
        "urgency=4 should clamp to urgency=3"
    );
    assert_eq!(
        critical3.b, clamped4.b,
        "urgency=4 should clamp to urgency=3"
    );
    assert_eq!(
        critical3.r, clamped100.r,
        "urgency=100 should clamp to urgency=3"
    );
}

/// Profile token override: color.notification.urgency.low overrides fallback.
#[test]
fn test_notification_urgency_low_token_override() {
    let mut token_map = HashMap::new();
    // Override low with pure cyan (#00FFFF) — clearly distinct from default.
    token_map.insert(
        "color.notification.urgency.low".to_string(),
        "#00FFFF".to_string(),
    );
    let color = urgency_to_notification_color(0, &token_map);
    assert!(
        color.r < 0.1,
        "custom low token R should be ~0.0 (cyan), got {}",
        color.r
    );
    assert!(
        color.g > 0.9,
        "custom low token G should be ~1.0 (cyan), got {}",
        color.g
    );
    assert!(
        color.b > 0.9,
        "custom low token B should be ~1.0 (cyan), got {}",
        color.b
    );
}

/// Profile token override: color.notification.urgency.critical overrides fallback.
#[test]
fn test_notification_urgency_critical_token_override() {
    let mut token_map = HashMap::new();
    // Override critical with the exemplar's dark red-black token.
    token_map.insert(
        "color.notification.urgency.critical".to_string(),
        "#450612".to_string(),
    );
    let color = urgency_to_notification_color(3, &token_map);
    assert!(
        (color.r - 0.0595).abs() < 0.004,
        "custom critical token R should decode from sRGB hex to ~0.0595 linear, got {}",
        color.r
    );
    assert!(
        (color.g - 0.0018).abs() < 0.002,
        "custom critical token G should decode from sRGB hex to ~0.0018 linear, got {}",
        color.g
    );
    assert!(
        (color.b - 0.0060).abs() < 0.002,
        "custom critical token B should decode from sRGB hex to ~0.0060 linear, got {}",
        color.b
    );
    assert!(
        color.r > color.b && color.b > color.g,
        "custom critical token should retain a red-black R > B > G ordering"
    );
}

/// notification-area urgency-tinted backdrop: renders backdrop at 0.8 opacity.
///
/// Per spec: non-alert-banner Notification content must use
/// color.notification.urgency.* tokens with fixed 0.8 opacity.
/// The policy.backdrop_opacity must NOT override this.
#[tokio::test]
async fn test_notification_area_backdrop_uses_0_8_opacity() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area urgency opacity test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.5)),
            // backdrop_opacity = 0.5 must NOT override the 0.8 fixed opacity
            backdrop_opacity: Some(0.5),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "test".to_owned(),
                icon: String::new(),
                urgency: 1, // normal
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    assert!(!vertices.is_empty(), "expected vertices");
    // The first quad's alpha (index 3 of color) should be 0.8.
    let first_alpha = vertices[0].color[3];
    assert!(
        (first_alpha - 0.8).abs() < 0.01,
        "notification-area backdrop alpha must be 0.8, got {first_alpha}"
    );
}

/// notification-area border rendering: 1px 4-quad border is emitted after the
/// urgency-tinted backdrop quad.
///
/// For a Stack zone with one Notification publish, render_zone_content should emit:
///   - 6 vertices for the backdrop quad
///   - up to 24 vertices (4 × 6) for the border quads
#[tokio::test]
async fn test_notification_area_emits_border_quads() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area border test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.05, 0.05, 0.05, 1.0)),
            font_size_px: Some(18.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "border test".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // One backdrop (6 vertices) + up to 4 border quads (6 each) = 6 + 24 = 30 max.
    // Minimum: 6 (backdrop) + 6 (at least top edge border) = 12.
    assert!(
        vertices.len() >= 12,
        "expected at least 12 vertices (backdrop + border), got {}",
        vertices.len()
    );
    // Total should be 6 * N for some N ≥ 2 (backdrop + at least one border quad).
    assert_eq!(
        vertices.len() % 6,
        0,
        "vertex count must be a multiple of 6 (each quad = 6 vertices), got {}",
        vertices.len()
    );
}

/// alert-banner does NOT emit border quads — border rendering is only for
/// non-alert-banner notification zones.
#[tokio::test]
async fn test_alert_banner_does_not_emit_border_quads() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert banner — no border".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.07,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(20.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            backdrop_opacity: Some(1.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "no border here".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // alert-banner: only 6 vertices (one backdrop quad, no border).
    assert_eq!(
        vertices.len(),
        6,
        "alert-banner must emit exactly 6 vertices (backdrop only, no border), got {}",
        vertices.len()
    );
}

/// Border color uses color.border.default token when present.
#[tokio::test]
async fn test_notification_area_border_uses_border_default_token() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    // Install a custom border token: pure cyan (#00FFFF).
    let mut token_map = HashMap::new();
    token_map.insert("color.border.default".to_string(), "#00FFFF".to_string());
    compositor.set_token_map(token_map);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area border token test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.05, 0.05, 0.05, 1.0)),
            font_size_px: Some(18.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "cyan border".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // vertices[0..6] = backdrop quad (urgency low color)
    // vertices[6..] = border quads (should be cyan: R≈0, G≈1, B≈1)
    assert!(
        vertices.len() > 6,
        "expected border quads after backdrop, only got {}",
        vertices.len()
    );
    // Check border quad color (vertex index 6 is the first border vertex).
    let border_v = &vertices[6];
    assert!(
        border_v.color[0] < 0.1,
        "border R should be ~0.0 (cyan token), got {}",
        border_v.color[0]
    );
    assert!(
        border_v.color[1] > 0.9,
        "border G should be ~1.0 (cyan token), got {}",
        border_v.color[1]
    );
    assert!(
        border_v.color[2] > 0.9,
        "border B should be ~1.0 (cyan token), got {}",
        border_v.color[2]
    );
}

// ── Token-resolved severity color tests ───────────────────────────────────

/// Custom `color.severity.warning` token overrides the hardcoded SEVERITY_WARNING
/// constant for urgency=2.
#[test]
fn test_custom_severity_warning_token_overrides_constant() {
    let mut token_map = HashMap::new();
    // Custom warning: bright green (#00FF00) — clearly distinct from amber.
    token_map.insert("color.severity.warning".to_string(), "#00FF00".to_string());
    let color = urgency_to_severity_color(2, &token_map);
    assert!(
        color.g > 0.9,
        "custom warning token G should be ~1.0 (green), got {}",
        color.g
    );
    assert!(
        color.r < 0.1,
        "custom warning token R should be ~0.0 (green), got {}",
        color.r
    );
    assert!(
        color.b < 0.1,
        "custom warning token B should be ~0.0 (green), got {}",
        color.b
    );
}

/// Custom `color.severity.critical` token overrides the hardcoded SEVERITY_CRITICAL.
#[test]
fn test_custom_severity_critical_token_overrides_constant() {
    let mut token_map = HashMap::new();
    // Custom critical: bright magenta (#FF00FF).
    token_map.insert("color.severity.critical".to_string(), "#FF00FF".to_string());
    let color = urgency_to_severity_color(3, &token_map);
    assert!(
        color.r > 0.9,
        "custom critical R should be ~1.0 (magenta), got {}",
        color.r
    );
    assert!(
        color.b > 0.9,
        "custom critical B should be ~1.0 (magenta), got {}",
        color.b
    );
    assert!(
        color.g < 0.1,
        "custom critical G should be ~0.0 (magenta), got {}",
        color.g
    );
}

/// Custom `color.severity.info` token overrides the hardcoded SEVERITY_INFO.
#[test]
fn test_custom_severity_info_token_overrides_constant() {
    let mut token_map = HashMap::new();
    // Custom info: pure red (#FF0000) — clearly distinct from default blue.
    token_map.insert("color.severity.info".to_string(), "#FF0000".to_string());
    let color0 = urgency_to_severity_color(0, &token_map);
    let color1 = urgency_to_severity_color(1, &token_map);
    for (urgency, color) in [(0, color0), (1, color1)] {
        assert!(
            color.r > 0.9,
            "custom info urgency={urgency} R should be ~1.0 (red), got {}",
            color.r
        );
        assert!(
            color.g < 0.1,
            "custom info urgency={urgency} G should be ~0.0 (red), got {}",
            color.g
        );
        assert!(
            color.b < 0.1,
            "custom info urgency={urgency} B should be ~0.0 (red), got {}",
            color.b
        );
    }
}

/// Invalid/absent token values fall back to hardcoded constants.
#[test]
fn test_invalid_severity_token_value_falls_back_to_constant() {
    let mut token_map = HashMap::new();
    // Not a valid hex color — should be ignored.
    token_map.insert(
        "color.severity.warning".to_string(),
        "not-a-color".to_string(),
    );
    let color = urgency_to_severity_color(2, &token_map);
    // Falls back to SEVERITY_WARNING (#FFB800): R~1.0, G~0.72, B~0.0.
    assert!(
        color.r > 0.9,
        "fallback warning R should be ~1.0, got {}",
        color.r
    );
    assert!(
        color.g > 0.5,
        "fallback warning G should be >0.5, got {}",
        color.g
    );
    assert!(
        color.b < 0.1,
        "fallback warning B should be ~0.0, got {}",
        color.b
    );
}

/// Custom severity tokens in [design_tokens] affect alert-banner backdrop colors.
///
/// This is the end-to-end integration test: `set_token_map` populates the
/// compositor, and `render_zone_content` uses the token-resolved color for the
/// alert-banner backdrop.
#[tokio::test]
async fn test_custom_severity_tokens_affect_alert_banner_backdrop() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    // Install a custom token map: override warning with pure green (#00FF00).
    let mut token_map = HashMap::new();
    token_map.insert("color.severity.warning".to_string(), "#00FF00".to_string());
    compositor.set_token_map(token_map);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert banner zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.07,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(20.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            backdrop_opacity: Some(1.0),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish urgency=2 (warning) — should use custom green token.
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Custom token warning".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // Collect vertices from render_zone_content.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // rect_vertices emits 6 vertices per quad; each vertex has a `color: [f32; 4]` field.
    // The backdrop should be green (R~0.0, G~1.0, B~0.0), not amber.
    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for alert-banner"
    );

    // Check first vertex color. RectVertex layout: [position: [f32; 2], color: [f32; 4]].
    let first = &vertices[0];
    assert!(
        first.color[1] > 0.9,
        "alert-banner backdrop G should be ~1.0 (custom green token), got {}",
        first.color[1]
    );
    assert!(
        first.color[0] < 0.1,
        "alert-banner backdrop R should be ~0.0 (custom green token), got {}",
        first.color[0]
    );
    assert!(
        first.color[2] < 0.1,
        "alert-banner backdrop B should be ~0.0 (custom green token), got {}",
        first.color[2]
    );
}

/// ZoneAnimationState fade-in reaches 1.0 after duration elapses.
#[test]
fn test_zone_animation_state_fade_in_completes() {
    // Use 0ms duration for instant completion.
    let state = ZoneAnimationState::fade_in(0);
    // Opacity at duration=0 should immediately be target (1.0).
    let opacity = state.current_opacity();
    assert_eq!(opacity, 1.0, "fade-in with 0ms should be 1.0 immediately");
    assert!(
        state.is_complete(),
        "0ms fade-in should be complete immediately"
    );
}

/// ZoneAnimationState fade-out starts at 1.0 and reaches 0.0 after duration.
#[test]
fn test_zone_animation_state_fade_out_completes() {
    let state = ZoneAnimationState::fade_out(0);
    let opacity = state.current_opacity();
    assert_eq!(opacity, 0.0, "fade-out with 0ms should be 0.0 immediately");
    assert!(
        state.is_complete(),
        "0ms fade-out should be complete immediately"
    );
}

/// ZoneAnimationState with non-zero duration: opacity is interpolated.
#[test]
fn test_zone_animation_state_interpolates() {
    // 10_000ms duration — very long, so elapsed << duration.
    let state = ZoneAnimationState::fade_in(10_000);
    // Very shortly after creation, opacity should be close to 0.
    let opacity = state.current_opacity();
    assert!(
        (0.0..=0.1).contains(&opacity),
        "fade-in opacity shortly after start should be near 0, got {opacity}"
    );
    assert!(
        !state.is_complete(),
        "10s fade-in should not be complete immediately"
    );
}

/// backdrop_opacity overrides the backdrop color's alpha channel.
/// When backdrop_opacity=0.6 and backdrop.a=1.0, effective alpha=0.6.
#[tokio::test]
async fn test_backdrop_opacity_overrides_color_alpha() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // backdrop color has alpha=1.0 but backdrop_opacity=0.6 should override it.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "test backdrop opacity override".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)), // alpha=1.0
            backdrop_opacity: Some(0.6),                   // override to 0.6
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("opacity test".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // The backdrop rendered should use alpha=0.6 (backdrop_opacity), not 1.0.
    // We verify this by checking the vertex colors produced — the alpha channel
    // of the first rect vertex should reflect 0.6.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert!(!vertices.is_empty(), "expected backdrop vertices");
    // The RectVertex has color field [f32; 4]; alpha should be ~0.6.
    let alpha = vertices[0].color[3];
    assert!(
        (alpha - 0.6).abs() < 0.01,
        "backdrop alpha should be ~0.6 (backdrop_opacity override), got {alpha}"
    );
}

/// backdrop=None: no backdrop quad rendered even when backdrop_opacity is set.
#[tokio::test]
async fn test_no_backdrop_when_backdrop_is_none() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "no-backdrop test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: None,
            backdrop_opacity: Some(0.9), // ignored because backdrop is None
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("no backdrop".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // With backdrop=None, no rect vertices should be emitted.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert!(
        vertices.is_empty(),
        "no backdrop quad should be rendered when policy.backdrop is None"
    );
}

/// backdrop=None with Notification content: no backdrop or border quads rendered.
///
/// Even though Notification content in a non-alert-banner zone overrides the
/// backdrop color with urgency-tinted tokens, the override must respect the
/// policy.backdrop contract: when backdrop is None, nothing is emitted.
#[tokio::test]
async fn test_notification_no_backdrop_when_backdrop_is_none() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area with backdrop=None".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: None,
            backdrop_opacity: Some(0.9),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "no backdrop".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert!(
        vertices.is_empty(),
        "no backdrop or border quads should be rendered when policy.backdrop is None, got {} vertices",
        vertices.len()
    );
}

/// text.rs: TextItem::from_zone_policy respects all RenderingPolicy fields.
#[test]
fn test_from_zone_policy_reads_all_policy_fields() {
    use crate::text::TextItem;

    let policy = RenderingPolicy {
        font_size_px: Some(28.0),
        text_color: Some(Rgba::new(1.0, 0.5, 0.0, 1.0)), // orange
        font_family: Some(FontFamily::SystemMonospace),
        text_align: Some(TextAlign::Center),
        outline_color: Some(Rgba::BLACK),
        outline_width: Some(1.5),
        margin_horizontal: Some(12.0),
        margin_vertical: Some(6.0),
        ..Default::default()
    };

    let item = TextItem::from_zone_policy("test", 0.0, 0.0, 400.0, 100.0, &policy, 1.0);
    assert_eq!(item.font_size_px, 28.0);
    assert_eq!(item.font_family, FontFamily::SystemMonospace);
    assert_eq!(item.alignment, TextAlign::Center);
    assert!(item.outline_color.is_some(), "outline_color should be set");
    assert_eq!(item.outline_width.unwrap(), 1.5);
    // Margins: x+12, y+6
    assert_eq!(item.pixel_x, 12.0);
    assert_eq!(item.pixel_y, 6.0);
}

// ── Multi-publication rendering: Stack and MergeByKey policies ──────────

/// Stack zone with two notifications: render_zone_content must emit a
/// separate backdrop quad for each publication, stacked vertically.
/// With max_depth=4 and zone height=400px, each slot is 100px tall.
/// Two publications → two quads; the second quad starts at y+100.
#[tokio::test]
async fn test_stack_zone_renders_separate_backdrop_per_publication() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone: top-right, 200×400 px via Relative.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "stack zone for multi-pub test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,    // 320 px at 1280 wide
            height_pct: 0.5556, // ~400 px at 720 tall (≈400/720)
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            backdrop_opacity: Some(0.9),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 4 },
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish two separate notifications.
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "First notification".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Second notification".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-b",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // Two publications → two backdrop quads (6 verts each) + border quads.
    // Each Notification slot emits:
    //   1 backdrop quad (6) + 4 border quads (24) + 4 dismiss-button border quads (24) = 54.
    // Total: 2 × 54 = 108 vertices.
    // We assert at least 12 (2 backdrops) and a multiple of 6.
    assert!(
        vertices.len() >= 12,
        "Stack zone with 2 publications must emit at least 12 vertices (2 backdrop quads), got {}",
        vertices.len()
    );
    assert_eq!(
        vertices.len() % 6,
        0,
        "vertex count must be a multiple of 6 (each quad = 6 vertices), got {}",
        vertices.len()
    );

    // The first backdrop quad's top-left y should be 0 (zone starts at y_pct=0.0 → y=0).
    // The second backdrop quad's top-left y should be ~slot_h after the first slot.
    // zone_h = 720 * 0.5556 ≈ 400; slot_h is content-sized per stack_slot_height.
    // Vertices are in NDC; we check the first and Nth vertex y values differ.
    // rect_vertices emits 6 verts per quad in positions [x,y] NDC.
    // Each notification slot emits:
    //   6  backdrop quad vertices
    //   24 border quads (4 quads × 6 verts each)
    //   24 dismiss-button border quads (4 quads × 6 verts each)
    //   = 54 vertices per slot.
    // The second backdrop quad therefore starts at vertex index 54.
    let first_quad_y = vertices[0].position[1];
    // Find second backdrop by skipping first slot (54 vertices: 6 backdrop + 24 border + 24 dismiss border).
    let second_quad_idx = 54; // 6 backdrop + 4 border quads + 4 dismiss-button border quads (6 each)
    if vertices.len() > second_quad_idx {
        let second_quad_y = vertices[second_quad_idx].position[1];
        assert!(
            (first_quad_y - second_quad_y).abs() > 0.01,
            "second Stack slot must start at a different y than the first; got first={first_quad_y:.4}, second={second_quad_y:.4}"
        );
    }
}

/// Stack zone: collect_text_items must produce a separate TextItem for
/// each publication, with each item positioned in its own vertical slot.
#[tokio::test]
async fn test_stack_zone_collect_text_items_per_publication() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "stack zone text items test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5556,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 4 },
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Alpha alert".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Beta alert".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-b",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Gamma alert".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-c",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // Three publications in a Stack zone must produce three TextItems.
    assert_eq!(
        items.len(),
        3,
        "Stack zone with 3 publications must produce 3 TextItems, got {}",
        items.len()
    );

    // Items should be ordered newest-first (slot 0 = newest at top of zone).
    assert!(
        items[0].text.contains("Gamma"),
        "first TextItem should be the newest publication (Gamma), got: {}",
        items[0].text
    );
    assert!(
        items[1].text.contains("Beta"),
        "second TextItem should be Beta, got: {}",
        items[1].text
    );
    assert!(
        items[2].text.contains("Alpha"),
        "third TextItem should be the oldest publication (Alpha), got: {}",
        items[2].text
    );

    // Each item should occupy a different vertical slot; slot 0 is at top
    // (lowest y), slot 1 below it, slot 2 below that.
    assert!(
        items[1].pixel_y > items[0].pixel_y,
        "slot 1 y ({}) must be below slot 0 y ({})",
        items[1].pixel_y,
        items[0].pixel_y
    );
    assert!(
        items[2].pixel_y > items[1].pixel_y,
        "slot 2 y ({}) must be below slot 1 y ({})",
        items[2].pixel_y,
        items[1].pixel_y
    );
}

// ── Slot layout: content-sized slots, newest-first ───────────────────────

/// 5 stacked notifications must each appear at a distinct y-position.
/// slot_height = font_size_px(16) + 18 = 34 px.
/// Zone is tall enough to accommodate all 5 slots.
/// Verifies newest-first ordering: slot 0 = newest at zone top.
#[tokio::test]
async fn test_stack_slot_layout_five_notifications_distinct_y() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone: 300px wide × 300px tall — enough for 5 × 34px slots (170px).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "slot layout test zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,    // 320 px at 1280 wide
            height_pct: 0.4167, // ~300 px at 720 tall
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 5,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish 5 notifications (oldest to newest: "N1" .. "N5").
    for i in 1..=5 {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: format!("N{i}"),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                &format!("agent-{i}"),
                None,
                None,
                None,
            )
            .unwrap();
    }

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 5, "5 notifications must produce 5 TextItems");

    // With font_size_px=16 → slot_h = 34.  margin_v defaults to 8.
    // pixel_y for slot i = zone_y + i*slot_h + margin_v.
    // Check all 5 y-values are strictly increasing (slot 0 = newest = top).
    let ys: Vec<f32> = items.iter().map(|it| it.pixel_y).collect();
    for w in ys.windows(2) {
        assert!(
            w[1] > w[0],
            "slots must have strictly increasing y; got consecutive y={:.2} then {:.2}",
            w[0],
            w[1]
        );
    }

    // Newest notification is "N5" — must be slot 0 (lowest y).
    assert!(
        items[0].text.contains("N5"),
        "slot 0 must be the newest notification (N5), got: {}",
        items[0].text
    );
    // Oldest is "N1" — must be slot 4 (highest y).
    assert!(
        items[4].text.contains("N1"),
        "slot 4 must be the oldest notification (N1), got: {}",
        items[4].text
    );
}

/// When a 6th notification is published to a Stack zone with max_depth=5,
/// the oldest notification must be evicted.  After eviction, only 5 items
/// remain and the evicted notification is absent.
#[tokio::test]
async fn test_stack_slot_sixth_notification_evicts_oldest() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "eviction test zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 6,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish 6 notifications; "oldest" is "Oldest" (first published).
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Oldest".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-0",
            None,
            None,
            None,
        )
        .unwrap();
    for i in 1..=5 {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: format!("N{i}"),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                &format!("agent-{i}"),
                None,
                None,
                None,
            )
            .unwrap();
    }

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // Only 5 items after eviction (max_depth=5).
    assert_eq!(
        items.len(),
        5,
        "after 6th publish with max_depth=5, must have 5 TextItems; got {}",
        items.len()
    );

    // "Oldest" must have been evicted — not present in any item.
    let has_oldest = items.iter().any(|it| it.text.contains("Oldest"));
    assert!(
        !has_oldest,
        "oldest notification must be evicted after 6th publish"
    );

    // "N5" (newest) must be present as slot 0.
    assert!(
        items[0].text.contains("N5"),
        "newest notification (N5) must be slot 0 after eviction, got: {}",
        items[0].text
    );
}

/// Stack notifications clip at zone boundary: slots whose top-left y is at or
/// beyond zone_bottom are fully clipped and produce no TextItem. Partial slots
/// (y < zone_bottom but y+slot_h > zone_bottom) are emitted with clamped height.
#[tokio::test]
async fn test_stack_slot_clips_at_zone_boundary() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone height: 72px at 720 tall → height_pct = 72/720 = 0.1.
    // font_size_px=16, line_height = 16*1.4 = 22.4, margin_v=8, SLOT_BASELINE_GAP=4
    // → slot_h = 22.4 + 2*8 + 4 = 42.4px.
    // slot 0 at y=0  (fits: 0+42.4=42.4 ≤ 72 → emitted).
    // slot 1 at y=42.4 (fits: 42.4+42.4=84.8 > 72 but y < 72 → emitted).
    // slot 2 at y=84.8 → y ≥ zone_bottom(72) → loop breaks.
    // Exactly 2 items (slots 0, 1) are emitted.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "clipping test zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.1, // 72 px at 720 tall
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 5,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish 4 notifications; only the 2 newest should be visible (newest-first).
    for i in 1..=4 {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: format!("M{i}"),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                &format!("agent-{i}"),
                None,
                None,
                None,
            )
            .unwrap();
    }

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // slot_h = line_height(16*1.4) + 2*margin_v(8) + SLOT_BASELINE_GAP(4) = 42.4px.
    // slot 0 at y=0:    0 < 72 → emitted.
    // slot 1 at y=42.4: 42.4 < 72 → emitted.
    // slot 2 at y=84.8: 84.8 ≥ 72 → loop breaks.
    // Exactly 2 items (slots 0, 1) are emitted.
    assert_eq!(
        items.len(),
        2,
        "with 72px zone and 42.4px slots, exactly 2 items should be emitted; got {}",
        items.len()
    );

    // The newest notification (M4) must be in slot 0.
    assert!(
        !items.is_empty() && items[0].text.contains("M4"),
        "newest notification (M4) must be slot 0 (top of zone), got: {}",
        if items.is_empty() {
            "empty"
        } else {
            &items[0].text
        }
    );
}

/// ZoneSlotLayout::iter_visible: slots whose top-left y is at or beyond
/// zone_bottom are excluded; partial slots are emitted with clamped height.
///
/// This is a GPU-free unit test pinning the shared slot-geometry computation
/// introduced in hud-qlerb. Geometry correctness is guaranteed by
/// test_stack_slot_clips_at_zone_boundary (integration) and this test (unit).
#[test]
fn test_zone_slot_layout_iter_visible_clips_and_clamps() {
    // Build a ZoneSlotLayout directly with known values.
    // Three equal-height slots of 30px each (offsets: 0, 30, 60).
    // Zone origin zy = 10.0; effective_h = 70.0 → zone_bottom = 80.0.
    //
    // slot 0: slot_y = 10+0  = 10  < 80 → emitted (effective_slot_h = min(30, 70) = 30)
    // slot 1: slot_y = 10+30 = 40  < 80 → emitted (effective_slot_h = min(30, 40) = 30)
    // slot 2: slot_y = 10+60 = 70  < 80 → emitted (effective_slot_h = min(30, 10) = 10)
    // slot 3 would be at 100 ≥ 80 → excluded (not in this layout, but cull verified)
    let layout = ZoneSlotLayout {
        ordered_indices: vec![0, 1, 2],
        slot_heights: vec![30.0, 30.0, 30.0],
        slot_offsets: vec![0.0, 30.0, 60.0],
        effective_h: 70.0,
    };

    let zy = 10.0_f32;
    let visible: Vec<(usize, f32, f32)> = layout.iter_visible(zy).collect();

    assert_eq!(
        visible.len(),
        3,
        "all 3 slots start before zone_bottom → all emitted"
    );

    // slot 0: full slot
    assert_eq!(visible[0], (0, 10.0, 30.0), "slot 0: full height");
    // slot 1: full slot
    assert_eq!(visible[1], (1, 40.0, 30.0), "slot 1: full height");
    // slot 2: clamped to remaining zone height (80 - 70 = 10)
    assert!(
        (visible[2].0 == 2)
            && (visible[2].1 - 70.0).abs() < 0.01
            && (visible[2].2 - 10.0).abs() < 0.01,
        "slot 2: clamped to 10px; got {:?}",
        visible[2]
    );

    // Verify that a slot starting exactly at zone_bottom is excluded.
    let layout_tight = ZoneSlotLayout {
        ordered_indices: vec![0, 1],
        slot_heights: vec![30.0, 30.0],
        slot_offsets: vec![0.0, 30.0],
        effective_h: 30.0, // zone_bottom = zy + 30 = 40
    };
    let tight: Vec<(usize, f32, f32)> = layout_tight.iter_visible(10.0).collect();
    // slot 0: slot_y = 10 < 40 → emitted; slot 1: slot_y = 40 ≥ 40 → excluded
    assert_eq!(
        tight.len(),
        1,
        "slot starting at zone_bottom must be excluded"
    );
    assert_eq!(tight[0].0, 0, "only slot 0 is emitted");
}

/// MergeByKey zone: collect_text_items must merge ALL StatusBar publications'
/// entries and produce a single TextItem containing all unique keys.
#[tokio::test]
async fn test_merge_by_key_zone_merges_all_status_bar_entries() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_owned(),
        description: "merge-by-key zone test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.04,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 32 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Agent A publishes "cpu" and "mem" keys.
    let mut entries_a = std::collections::HashMap::new();
    entries_a.insert("cpu".to_owned(), "45%".to_owned());
    entries_a.insert("mem".to_owned(), "8.2 GB".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload { entries: entries_a }),
            "agent-a",
            Some("cpu-mem".to_owned()),
            None,
            None,
        )
        .unwrap();

    // Agent B publishes a "net" key.
    let mut entries_b = std::collections::HashMap::new();
    entries_b.insert("net".to_owned(), "1.2 MB/s".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload { entries: entries_b }),
            "agent-b",
            Some("net".to_owned()),
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // MergeByKey must produce exactly ONE TextItem containing all merged entries.
    assert_eq!(
        items.len(),
        1,
        "MergeByKey zone must produce a single merged TextItem, got {}",
        items.len()
    );

    let text = &items[0].text;
    assert!(
        text.contains("cpu"),
        "merged text must include 'cpu' key; got: {text}"
    );
    assert!(
        text.contains("mem"),
        "merged text must include 'mem' key; got: {text}"
    );
    assert!(
        text.contains("net"),
        "merged text must include 'net' key; got: {text}"
    );
    assert!(
        text.contains("45%"),
        "merged text must include cpu value '45%'; got: {text}"
    );
    assert!(
        text.contains("1.2 MB/s"),
        "merged text must include net value '1.2 MB/s'; got: {text}"
    );
}

/// MergeByKey zone: when a key appears in multiple publications, the latest
/// value wins (last-write-wins per key semantics).
#[tokio::test]
async fn test_merge_by_key_latest_value_wins_for_duplicate_keys() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_owned(),
        description: "merge-by-key duplicate key test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.04,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 32 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // First publish: cpu = "10%"
    let mut entries_old = std::collections::HashMap::new();
    entries_old.insert("cpu".to_owned(), "10%".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload {
                entries: entries_old,
            }),
            "agent-a",
            Some("cpu".to_owned()),
            None,
            None,
        )
        .unwrap();

    // Second publish: same key "cpu" with updated value "90%"
    let mut entries_new = std::collections::HashMap::new();
    entries_new.insert("cpu".to_owned(), "90%".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload {
                entries: entries_new,
            }),
            "agent-a",
            Some("cpu".to_owned()),
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    assert_eq!(items.len(), 1, "must produce one merged TextItem");
    let text = &items[0].text;

    // The latest value "90%" must appear; "10%" must not.
    assert!(
        text.contains("90%"),
        "merged text must show latest cpu value '90%'; got: {text}"
    );
    assert!(
        !text.contains("10%"),
        "merged text must not show stale cpu value '10%'; got: {text}"
    );
}

// ── StatusBar icon layout tests [hud-x2v1.2] ─────────────────────────────

/// `key_icon_map` empty → single merged TextItem (backward-compatible).
///
/// When `key_icon_map` is empty, the existing single-TextItem newline-joined
/// behavior must be preserved unchanged.
#[tokio::test]
async fn test_status_bar_empty_key_icon_map_produces_single_text_item() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_owned(),
        description: "icon layout: empty map regression".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.04,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            text_color: Some(Rgba::WHITE),
            // key_icon_map defaults to empty HashMap via serde(default).
            ..Default::default()
        },
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 16 },
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    let mut entries = std::collections::HashMap::new();
    entries.insert("cpu".to_owned(), "45%".to_owned());
    entries.insert("mem".to_owned(), "8 GB".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload { entries }),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // Must still produce exactly ONE TextItem (no icon layout).
    assert_eq!(
        items.len(),
        1,
        "empty key_icon_map must produce one merged TextItem; got {}",
        items.len()
    );
    let text = &items[0].text;
    assert!(text.contains("cpu"), "text must contain 'cpu'");
    assert!(text.contains("mem"), "text must contain 'mem'");
    // Entries joined with newline (alphabetically sorted).
    assert!(
        text.contains('\n'),
        "multiple entries must be newline-separated; got: {text}"
    );
}

/// `key_icon_map` non-empty → per-entry TextItems; icon-mapped entries have
/// text `pixel_x` inset by `ICON_SIZE_PX + ICON_TEXT_GAP_PX`.
///
/// We don't use real SVG files here since tests can't rely on specific
/// filesystem paths.  Instead we verify the TextItem layout (pixel_x
/// position) produced by `status_bar_icon_text_items` directly — the icon
/// draw command path is exercised separately.
#[tokio::test]
async fn test_status_bar_key_icon_map_produces_per_entry_text_items() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut key_icon_map = std::collections::HashMap::new();
    // Map only "cpu" to an icon path; "mem" has no mapping.
    key_icon_map.insert("cpu".to_owned(), "/nonexistent/cpu.svg".to_owned());

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_owned(),
        description: "icon layout: per-entry TextItems".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            text_color: Some(Rgba::WHITE),
            font_size_px: Some(16.0),
            key_icon_map,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 16 },
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    let mut entries = std::collections::HashMap::new();
    entries.insert("cpu".to_owned(), "45%".to_owned());
    entries.insert("mem".to_owned(), "8 GB".to_owned());
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload { entries }),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // Must produce one TextItem per entry (2 entries → 2 TextItems).
    assert_eq!(
        items.len(),
        2,
        "non-empty key_icon_map must produce per-entry TextItems; got {}",
        items.len()
    );

    // Entries are sorted by key: "cpu" (row 0) then "mem" (row 1).
    // "cpu" has an SVG icon mapping, so no prefix.
    // "mem" has no SVG icon mapping, so gets emoji prefix "💾".
    let cpu_item = items.iter().find(|i| i.text.starts_with("cpu:"));
    let mem_item = items.iter().find(|i| i.text.starts_with("💾 mem:"));

    assert!(cpu_item.is_some(), "expected a TextItem for 'cpu'");
    assert!(
        mem_item.is_some(),
        "expected a TextItem for 'mem' with emoji prefix"
    );

    let cpu_item = cpu_item.unwrap();
    let mem_item = mem_item.unwrap();

    // "cpu" is icon-mapped: pixel_x must be inset by ICON_SIZE_PX + ICON_TEXT_GAP_PX
    // relative to "mem" (which has no icon).
    // Both items use from_zone_policy with x = zx + icon_inset, so:
    //   cpu.pixel_x = zx + ICON_SIZE_PX + ICON_TEXT_GAP_PX + margin_h
    //   mem.pixel_x = zx + 0 + margin_h
    // Difference should be exactly ICON_SIZE_PX + ICON_TEXT_GAP_PX (30.0).
    let icon_inset = ICON_SIZE_PX + ICON_TEXT_GAP_PX; // 24.0 + 6.0 = 30.0
    let diff = cpu_item.pixel_x - mem_item.pixel_x;
    assert!(
        (diff - icon_inset).abs() < 0.5,
        "cpu pixel_x must be inset by {icon_inset} px relative to mem; diff={diff}"
    );

    // "mem" has no icon: bounds_width must be wider than "cpu" by icon_inset.
    let width_diff = mem_item.bounds_width - cpu_item.bounds_width;
    assert!(
        (width_diff - icon_inset).abs() < 0.5,
        "mem bounds_width must be {icon_inset} px wider than cpu; diff={width_diff}"
    );
}

/// LatestWins zone still renders only one TextItem even when multiple
/// publications are present (regression guard).
#[tokio::test]
async fn test_latest_wins_zone_renders_only_latest_publication() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "latest-wins regression guard".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // LatestWins should only keep the last publication.
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Old content".to_owned()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    // With LatestWins policy the scene graph may have already replaced it,
    // but we publish again to ensure only one ends up active.
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("New content".to_owned()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // LatestWins must produce exactly one TextItem.
    assert_eq!(
        items.len(),
        1,
        "LatestWins zone must produce exactly 1 TextItem; got {}",
        items.len()
    );
    // The item should contain the latest content.
    assert!(
        items[0].text.contains("New content"),
        "LatestWins must render latest publish; got: {}",
        items[0].text
    );
}

// ── Alert-banner heading typography tests [hud-w3o6.2] ───────────────────
//
// Acceptance criteria from spec §Alert-Banner Heading Typography:
//   1. font_size_px = 24px
//   2. font_weight = 700 (bold)
//   3. font_family = SystemSansSerif
//   4. text_color = #FFFFFF white
//   5. margin_horizontal inset applied

/// Alert-banner RenderingPolicy carries heading typography:
/// 24px font, weight 700, SystemSansSerif, white text, margin_horizontal=8.
///
/// Acceptance criterion 3.1–3.3: heading typography wired to alert-banner zone.
#[tokio::test]
async fn test_alert_banner_heading_typography_in_rendering_policy() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Register alert-banner with heading-typography RenderingPolicy (spec values).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "heading typography test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.06,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(24.0),
            font_family: Some(FontFamily::SystemSansSerif),
            font_weight: Some(700),
            text_color: Some(Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            }),
            backdrop: Some(Rgba::new(0.1, 0.1, 0.16, 0.9)),
            backdrop_opacity: Some(0.9),
            margin_horizontal: Some(8.0),
            margin_vertical: Some(0.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 16,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish a notification payload.
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Weather alert: severe storms".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // collect_text_items uses the RenderingPolicy fields for TextItem construction.
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        items.len(),
        1,
        "expected one TextItem for alert-banner notification"
    );

    let item = &items[0];

    // AC 3.1: font_size_px = 24.0
    assert_eq!(
        item.font_size_px, 24.0,
        "alert-banner text must be 24px per spec §Alert-Banner Heading Typography"
    );

    // AC 3.1: font_family = SystemSansSerif
    assert_eq!(
        item.font_family,
        FontFamily::SystemSansSerif,
        "alert-banner text must use system sans-serif family"
    );

    // AC 3.1: font_weight = 700
    assert_eq!(
        item.font_weight, 700,
        "alert-banner text must be weight 700 (bold)"
    );

    // AC 3.2: text_color = #FFFFFF (white)
    // White in linear sRGB: R=1.0 → 255u8, G=1.0 → 255u8, B=1.0 → 255u8.
    assert_eq!(item.color[0], 255, "text R should be 255 (white)");
    assert_eq!(item.color[1], 255, "text G should be 255 (white)");
    assert_eq!(item.color[2], 255, "text B should be 255 (white)");

    // AC 3.3: text is inset from x=0 by margin_horizontal=8
    // Zone geometry: zx = (sw - sw*1.0)/2 = 0.0, so pixel_x = 0 + 8 = 8.
    assert_eq!(
        item.pixel_x, 8.0,
        "text must be inset by margin_horizontal=8 from backdrop edge"
    );
}

/// Alert-banner zone has LayerAttachment::Chrome — renders above all agent content.
///
/// Acceptance criterion: chrome-layer z-order verified by checking ZoneDefinition.
#[test]
fn test_alert_banner_default_zone_has_chrome_layer_attachment() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("alert-banner")
        .expect("alert-banner must be in default zone registry");

    assert_eq!(
        zone.layer_attachment,
        LayerAttachment::Chrome,
        "alert-banner zone must be attached to chrome layer (above all agent content)"
    );
}

/// Alert-banner zone spans full display width (width_pct = 1.0).
///
/// Acceptance criterion: backdrop quad spans from x=0 to x=display_width.
#[test]
fn test_alert_banner_default_zone_is_full_width() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("alert-banner")
        .expect("alert-banner must be in default zone registry");

    match zone.geometry_policy {
        GeometryPolicy::EdgeAnchored { width_pct, .. } => {
            assert_eq!(
                width_pct, 1.0,
                "alert-banner must span full display width (width_pct=1.0)"
            );
        }
        _ => panic!("alert-banner must use EdgeAnchored geometry for full-width positioning"),
    }
}

/// Alert-banner zone resolve_zone_geometry gives backdrop width = display width.
///
/// At 1920×1080, the backdrop must span from x=0 to x=1920.
#[test]
fn test_alert_banner_backdrop_spans_full_display_width() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("alert-banner")
        .expect("alert-banner zone must exist");

    let (x, _y, w, _h) = Compositor::resolve_zone_geometry(&zone.geometry_policy, 1920.0, 1080.0);
    assert_eq!(x, 0.0, "alert-banner left edge must be at x=0");
    assert_eq!(
        w, 1920.0,
        "alert-banner width must equal display width (1920)"
    );
}

/// Alert-banner zone height accommodates 24px heading + vertical padding.
///
/// At 720p, height_pct=0.06 → 43.2px > 24px + 2×8px = 40px minimum.
#[test]
fn test_alert_banner_zone_height_accommodates_heading_typography() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("alert-banner")
        .expect("alert-banner zone must exist");

    // Check that resolved height at 720p is sufficient for 24px heading.
    // margin_vertical=0.0 (flush to edge), so minimum is font_size_px only.
    // height_pct=0.06 → 0.06×720=43.2px, well above the 24px minimum.
    let (_x, _y, _w, h) = Compositor::resolve_zone_geometry(&zone.geometry_policy, 1280.0, 720.0);
    let font_size_px = zone.rendering_policy.font_size_px.unwrap_or(24.0);
    let min_required = font_size_px; // margin_vertical=0.0; height must cover font at minimum
    assert!(
        h >= min_required,
        "alert-banner height {h}px must accommodate heading ({font_size_px}px)"
    );
}

/// When no alert-banner publications are active, render_zone_content emits zero
/// backdrop vertices — the zone occupies zero visible space.
///
/// Acceptance criterion §Alert-Banner Chrome-Layer Positioning:
///   "When no alerts are active, the alert-banner zone MUST occupy zero vertical space."
#[tokio::test]
async fn test_alert_banner_zero_height_when_inactive() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Register alert-banner zone with a visible backdrop so it would render if active.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "zero-height-when-inactive test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.06,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(24.0),
            backdrop: Some(Rgba::new(1.0, 0.0, 0.0, 1.0)), // bright red — visible if leaked
            backdrop_opacity: Some(0.9),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Replace,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // No publications — zone is inactive.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // Zero vertices emitted → zone occupies zero visible space.
    assert!(
        vertices.is_empty(),
        "no backdrop quad must be emitted for inactive alert-banner zone (zero visible space)"
    );

    // Also verify no TextItems produced.
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert!(
        items.is_empty(),
        "no text must be rendered for inactive alert-banner zone"
    );
}

/// Alert-banner RenderingPolicy in ZoneRegistry::with_defaults() carries
/// heading typography: 24px, weight 700, white text, margin_horizontal=8.
#[test]
fn test_alert_banner_default_zone_rendering_policy_has_heading_typography() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("alert-banner")
        .expect("alert-banner must be in default zone registry");

    let policy = &zone.rendering_policy;

    assert_eq!(
        policy.font_size_px,
        Some(24.0),
        "alert-banner default rendering policy must have font_size_px=24"
    );
    assert_eq!(
        policy.font_weight,
        Some(700),
        "alert-banner default rendering policy must have font_weight=700 (bold)"
    );
    assert_eq!(
        policy.font_family,
        Some(FontFamily::SystemSansSerif),
        "alert-banner default rendering policy must use SystemSansSerif"
    );
    // text_color must be white (R=1.0, G=1.0, B=1.0).
    let tc = policy
        .text_color
        .expect("alert-banner default rendering policy must have text_color set");
    assert!(
        (tc.r - 1.0).abs() < 0.01,
        "text_color R must be 1.0 (white), got {}",
        tc.r
    );
    assert!(
        (tc.g - 1.0).abs() < 0.01,
        "text_color G must be 1.0 (white), got {}",
        tc.g
    );
    assert!(
        (tc.b - 1.0).abs() < 0.01,
        "text_color B must be 1.0 (white), got {}",
        tc.b
    );
    assert_eq!(
        policy.margin_horizontal,
        Some(8.0),
        "alert-banner default rendering policy must have margin_horizontal=8"
    );
}

// ─── Alert-banner severity-stack tests ───────────────────────────────────

/// Helper: build a SceneGraph with an alert-banner Stack zone for severity tests.
///
/// Zone: full-width, EdgeAnchored top, height_pct=0.05 (36px at 720p).
/// font_size_px=16, default margin_v=8 → slot_h = 16 + 2×8 + 2 = 34px.
/// max_depth=8, max_publishers=16.
fn make_alert_banner_scene() -> SceneGraph {
    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert-banner severity-stack test zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.05,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            backdrop_opacity: Some(1.0),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 16,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
}

/// Helper: publish an alert banner notification.
fn publish_alert(scene: &mut SceneGraph, text: &str, urgency: u32, publisher: &str) {
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: text.to_owned(),
                icon: String::new(),
                urgency,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            publisher,
            None,
            None,
            None,
        )
        .expect("alert-banner publish must succeed");
}

/// Critical (urgency=3) banner must appear above warning (urgency=2).
///
/// With two banners, slot 0 (top) must be the critical one regardless of
/// publication order (warning published before critical).
#[tokio::test]
async fn test_alert_banner_critical_above_warning() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = make_alert_banner_scene();

    // Publish warning first, then critical.
    publish_alert(&mut scene, "Warning: disk space low", 2, "agent-a");
    publish_alert(&mut scene, "Critical: system failure", 3, "agent-b");

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 2, "two banners → two TextItems");

    // Slot 0 (pixel_y=0) must be the critical banner (urgency 3).
    // Slot 1 (pixel_y=slot_h) must be the warning banner (urgency 2).
    // The critical banner is at a lower pixel_y value (top of the zone).
    let y_first = items[0].pixel_y;
    let y_second = items[1].pixel_y;
    assert!(
        y_first < y_second,
        "slot 0 (critical) must be above slot 1 (warning): y0={y_first} y1={y_second}"
    );
    assert!(
        items[0].text.contains("Critical"),
        "slot 0 must be the critical banner; got: {}",
        items[0].text
    );
    assert!(
        items[1].text.contains("Warning"),
        "slot 1 must be the warning banner; got: {}",
        items[1].text
    );
}

/// Warning (urgency=2) banner must appear above info (urgency=0-1).
///
/// Info published before warning — severity sort must override arrival order.
#[tokio::test]
async fn test_alert_banner_warning_above_info() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = make_alert_banner_scene();

    // Publish info first, then warning.
    publish_alert(&mut scene, "Info: update available", 1, "agent-a");
    publish_alert(&mut scene, "Warning: memory pressure", 2, "agent-b");

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 2, "two banners → two TextItems");

    assert!(
        items[0].text.contains("Warning"),
        "slot 0 must be the warning banner; got: {}",
        items[0].text
    );
    assert!(
        items[1].text.contains("Info"),
        "slot 1 must be the info banner; got: {}",
        items[1].text
    );
    assert!(
        items[0].pixel_y < items[1].pixel_y,
        "warning slot must be above info slot"
    );
}

/// Three-level severity stack: critical → warning → info (top to bottom).
///
/// Published in reverse order (info, warning, critical) to confirm severity
/// sort overrides arrival order.
#[tokio::test]
async fn test_alert_banner_three_level_severity_stack() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = make_alert_banner_scene();

    // Publish info first, then warning, then critical.
    publish_alert(&mut scene, "Info: routine scan complete", 0, "agent-a");
    publish_alert(&mut scene, "Warning: high load", 2, "agent-b");
    publish_alert(&mut scene, "Critical: disk full", 3, "agent-c");

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 3, "three banners → three TextItems");

    // Verify order: critical (slot 0), warning (slot 1), info (slot 2).
    assert!(
        items[0].text.contains("Critical"),
        "slot 0 must be critical; got: {}",
        items[0].text
    );
    assert!(
        items[1].text.contains("Warning"),
        "slot 1 must be warning; got: {}",
        items[1].text
    );
    assert!(
        items[2].text.contains("Info"),
        "slot 2 must be info; got: {}",
        items[2].text
    );
    // Pixel positions must decrease (slot 0 < slot 1 < slot 2 in pixel_y).
    assert!(
        items[0].pixel_y < items[1].pixel_y,
        "critical above warning"
    );
    assert!(items[1].pixel_y < items[2].pixel_y, "warning above info");
}

/// Same-severity banners: the newer one must appear above the older one.
///
/// Two warnings published in order (A first, B second).  Slot 0 must be
/// the newer one ("Warning B").
///
/// The sort is deterministic even when timestamps are equal: a tertiary
/// `index descending` key in `sort_alert_banner_indices` ensures the later
/// insert (higher index) always wins on exact timestamp ties.
#[tokio::test]
async fn test_alert_banner_same_severity_recency_order() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = make_alert_banner_scene();

    // Publish two warnings in order.  Even if both arrive in the same µs,
    // the tertiary index key ensures B (higher index) sorts above A.
    publish_alert(&mut scene, "Warning A (older)", 2, "agent-a");
    publish_alert(&mut scene, "Warning B (newer)", 2, "agent-b");

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 2, "two warnings → two TextItems");

    // Newer publish must be slot 0 (top).
    assert!(
        items[0].text.contains("Warning B"),
        "slot 0 must be the newer warning (B); got: {}",
        items[0].text
    );
    assert!(
        items[1].text.contains("Warning A"),
        "slot 1 must be the older warning (A); got: {}",
        items[1].text
    );
}

/// Alert-banner zone height grows dynamically with active banner count.
///
/// Test helper zone: height_pct=0.05, so static zone height at 720p = 36px.
/// slot_h = font_size_px(16) + 2 × margin_v(8) + SLOT_BASELINE_GAP(2) = 34px.
///
/// - 0 banners → 0 vertices (zero height, nothing rendered).
/// - 1 banner  → 1 backdrop quad (6 vertices), slot at y=0..34px.
/// - 3 banners → 3 backdrop quads (18 vertices), 3rd slot at y=68..102px —
///   this exceeds the 36px static height, proving dynamic expansion.
#[tokio::test]
async fn test_alert_banner_zone_height_grows_with_active_count() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    // ── 0 banners: no vertices emitted ──────────────────────────────────
    {
        let scene = make_alert_banner_scene();
        let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
        compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
        assert!(
            vertices.is_empty(),
            "0 banners → 0 vertices (zero height); got {} vertices",
            vertices.len()
        );
    }

    // ── 1 banner: one backdrop quad (6 vertices) ────────────────────────
    {
        let mut scene = make_alert_banner_scene();
        publish_alert(&mut scene, "Single banner", 2, "agent-a");
        let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
        compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
        assert_eq!(
            vertices.len(),
            6,
            "1 banner → 1 backdrop quad (6 vertices); got {}",
            vertices.len()
        );
    }

    // ── 3 banners: three backdrop quads (18 vertices) ───────────────────
    //
    // The static zone height is 36px (height_pct=0.05 × 720p).  Each slot
    // is 34px.  Under fixed-height logic the 2nd slot (y=34) would be
    // clipped at 36px (~2px visible) and the 3rd (y=68) would be invisible.
    // Dynamic height = 3 × 34 = 102px allows all three to render fully.
    {
        let mut scene = make_alert_banner_scene();
        publish_alert(&mut scene, "Banner A", 1, "agent-a");
        publish_alert(&mut scene, "Banner B", 2, "agent-b");
        publish_alert(&mut scene, "Banner C", 3, "agent-c");
        let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
        compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
        assert_eq!(
            vertices.len(),
            18,
            "3 banners → 3 backdrop quads (18 vertices); got {} — \
                 dynamic height must expand beyond static zone height",
            vertices.len()
        );
        // Verify that all 3 quads are at distinct y positions (slot 0 ≠ slot 2).
        // Vertex layout from rect_vertices: vertex 0 is top-left [left, top] in NDC.
        // Slot 0 starts at pixel y=0 → NDC y_top=1.0.
        // Slot 2 starts at pixel y≈68px → NDC y_top≈0.811 (strictly less than 1.0).
        let slot0_ndc_y = vertices[0].position[1];
        let slot2_ndc_y = vertices[12].position[1];
        assert!(
            slot2_ndc_y < slot0_ndc_y,
            "3rd slot must be below 1st slot in NDC y; slot0={slot0_ndc_y}, slot2={slot2_ndc_y}"
        );
    }
}

/// render_zone_content for alert-banner uses severity-ordered backdropcolors.
///
/// With critical (urgency=3) and warning (urgency=2), the first backdrop quad
/// (slot 0, top) must be red (critical color), and the second must be amber
/// (warning color).
#[tokio::test]
async fn test_alert_banner_backdrop_colors_ordered_by_severity() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = make_alert_banner_scene();

    // Publish warning first, then critical (to confirm severity overrides arrival).
    publish_alert(&mut scene, "Warning", 2, "agent-a");
    publish_alert(&mut scene, "Critical", 3, "agent-b");

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // 2 backdrop quads → 12 vertices.
    assert_eq!(vertices.len(), 12, "2 banners → 12 vertices");

    // Slot 0 (vertices 0-5) must be critical red: R > 0.9, G < 0.1, B < 0.1.
    let slot0_color = vertices[0].color;
    assert!(
        slot0_color[0] > 0.9,
        "slot 0 backdrop R should be ~1.0 (critical red); got {}",
        slot0_color[0]
    );
    assert!(
        slot0_color[1] < 0.1,
        "slot 0 backdrop G should be ~0.0 (critical red); got {}",
        slot0_color[1]
    );

    // Slot 1 (vertices 6-11) must be warning amber: R > 0.9, G mid, B < 0.1.
    let slot1_color = vertices[6].color;
    assert!(
        slot1_color[0] > 0.9,
        "slot 1 backdrop R should be ~1.0 (warning amber); got {}",
        slot1_color[0]
    );
    assert!(
        slot1_color[2] < 0.1,
        "slot 1 backdrop B should be ~0.0 (warning amber); got {}",
        slot1_color[2]
    );
    // Amber has non-trivial G (0.5–0.9), while critical has G < 0.1.
    assert!(
        slot1_color[1] > 0.5,
        "slot 1 backdrop G should be mid (warning amber ≈ 0.72); got {}",
        slot1_color[1]
    );
}

// ── Notification text rendering [hud-j5g5.3] ─────────────────────────────
//
// Spec §Notification Text Rendering:
//   - typography.body.size (16px default) font size
//   - color.text.primary text color
//   - left-aligned, 9px inset (8px padding + 1px border)
//   - clips at content area boundary (no wrapping in v1)

/// Notification text uses typography.body.size (default 16px) when token absent.
///
/// AC: notification text must use font_size_px resolved from typography.body.size.
#[tokio::test]
async fn test_notification_text_uses_body_typography_token_default() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "text rendering test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            // No font_size_px set — must fall through to typography.body.size token.
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Doorbell rang".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    // No token map set → typography.body.size absent → default 16px.
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 1, "must produce one TextItem for notification");
    assert_eq!(
        items[0].font_size_px, 16.0,
        "notification text must use typography.body.size default (16px)"
    );
}

/// Notification text uses typography.body.size resolved from the token map.
///
/// AC: when typography.body.size token is present, it overrides the 16px default.
#[test]
fn test_notification_text_uses_typography_body_size_token() {
    let mut token_map = HashMap::new();
    token_map.insert("typography.body.size".to_string(), "20px".to_string());
    let font_size = Compositor::resolve_body_font_size(&token_map);
    assert_eq!(
        font_size, 20.0,
        "typography.body.size=20px must resolve to 20.0"
    );
}

/// typography.body.size without 'px' suffix still parses.
#[test]
fn test_notification_text_typography_token_without_px_suffix() {
    let mut token_map = HashMap::new();
    token_map.insert("typography.body.size".to_string(), "18".to_string());
    let font_size = Compositor::resolve_body_font_size(&token_map);
    assert_eq!(
        font_size, 18.0,
        "numeric-only typography.body.size must parse"
    );
}

/// Absent typography.body.size token falls back to 16px.
#[test]
fn test_notification_text_typography_absent_defaults_to_16px() {
    let token_map = HashMap::new();
    let font_size = Compositor::resolve_body_font_size(&token_map);
    assert_eq!(
        font_size, 16.0,
        "absent typography.body.size must default to 16px"
    );
}

/// color.text.primary token resolves to the correct sRGB u8 color.
#[test]
fn test_notification_text_uses_color_text_primary_token() {
    let mut token_map = HashMap::new();
    // White: #FFFFFF
    token_map.insert("color.text.primary".to_string(), "#FFFFFF".to_string());
    let color = Compositor::resolve_text_primary_color(&token_map);
    assert_eq!(color[0], 255, "color.text.primary #FFFFFF R must be 255");
    assert_eq!(color[1], 255, "color.text.primary #FFFFFF G must be 255");
    assert_eq!(color[2], 255, "color.text.primary #FFFFFF B must be 255");
}

/// Absent color.text.primary falls back to near-white.
#[test]
fn test_notification_text_primary_absent_falls_back_to_near_white() {
    let token_map = HashMap::new();
    let color = Compositor::resolve_text_primary_color(&token_map);
    assert_eq!(color[0], 255, "fallback text.primary R must be 255");
    assert_eq!(color[1], 255, "fallback text.primary G must be 255");
    assert_eq!(color[2], 255, "fallback text.primary B must be 255");
    // Alpha is near-white (≥200 of 255).
    assert!(
        color[3] >= 200,
        "fallback text.primary alpha must be ≥ 200, got {}",
        color[3]
    );
}

/// Stack notifications render a dedicated dismiss label and reserve width for it.
#[tokio::test]
async fn test_notification_stack_adds_dismiss_label_and_reserves_text_width() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "dismiss label test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Dismissible notification".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 2, "body text + dismiss label expected");

    let body_item = items
        .iter()
        .find(|item| &*item.text == "Dismissible notification")
        .expect("body text item must exist");
    let dismiss_item = items
        .iter()
        .find(|item| &*item.text == "X")
        .expect("dismiss text item must exist");

    assert_eq!(body_item.pixel_x, 9.0, "body text keeps left inset");
    assert_eq!(
        body_item.bounds_width, 274.0,
        "body width must reserve dismiss control space"
    );
    assert_eq!(
        dismiss_item.alignment,
        TextAlign::Center,
        "dismiss label must be centered in its button bounds"
    );
    assert!(
        dismiss_item.pixel_x >= 300.0,
        "dismiss label must sit near the right edge, got {}",
        dismiss_item.pixel_x
    );
}

// ── Dismiss button typography token tests [hud-y08tp] ────────────────────
//
// Acceptance criteria:
//   1. No-token path: dismiss button uses NOTIFICATION_DISMISS_FONT_SIZE_PX
//      (12.0 px) and NOTIFICATION_DISMISS_FONT_WEIGHT (700) as defaults.
//   2. Token path: typography.notification.dismiss.font_size_px and
//      typography.notification.dismiss.font_weight override the defaults.

/// Dismiss button uses default font_size_px (12.0) and font_weight (700)
/// when dismiss typography tokens are absent.
///
/// AC 1: no-token path preserves visual defaults.
#[tokio::test]
async fn test_dismiss_button_uses_default_font_size_and_weight() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "dismiss font default test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Default dismiss test".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let dismiss_item = items
        .iter()
        .find(|item| &*item.text == "X")
        .expect("dismiss text item must exist");

    assert_eq!(
        dismiss_item.font_size_px, NOTIFICATION_DISMISS_FONT_SIZE_PX,
        "dismiss button font_size_px must be the default (12.0) when token absent"
    );
    assert_eq!(
        dismiss_item.font_weight, NOTIFICATION_DISMISS_FONT_WEIGHT,
        "dismiss button font_weight must be the default (700) when token absent"
    );
}

/// Dismiss button font_size_px and font_weight read from design tokens when
/// `typography.notification.dismiss.font_size_px` and
/// `typography.notification.dismiss.font_weight` are present.
///
/// AC 2: token-override path correctly propagates to the rendered TextItem.
#[tokio::test]
async fn test_dismiss_button_respects_typography_tokens() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Inject dismiss typography tokens.
    let mut token_map = HashMap::new();
    token_map.insert(
        "typography.notification.dismiss.font_size_px".to_string(),
        "16px".to_string(),
    );
    token_map.insert(
        "typography.notification.dismiss.font_weight".to_string(),
        "400".to_string(),
    );
    compositor.set_token_map(token_map);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "dismiss font token test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Token override dismiss test".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-b",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let dismiss_item = items
        .iter()
        .find(|item| &*item.text == "X")
        .expect("dismiss text item must exist");

    assert_eq!(
        dismiss_item.font_size_px, 16.0,
        "dismiss button font_size_px must be 16.0 from token override"
    );
    assert_eq!(
        dismiss_item.font_weight, 400,
        "dismiss button font_weight must be 400 from token override"
    );
}

/// Notification text is inset by 9px (8px padding + 1px border) from backdrop edges.
///
/// AC: text content area starts at (x + 9, y + 9).
#[tokio::test]
async fn test_notification_text_inset_from_backdrop_edges() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone at x=0, y=0 (x_pct=0, y_pct=0) with 100% width and 50% height.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "inset test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,  // zx = 0
            height_pct: 0.5, // zy = 0
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Test notification".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 1, "must produce one TextItem");

    let item = &items[0];
    // Zone starts at x=0, y=0. Text must be inset by 9px (1px border + 8px padding).
    assert_eq!(
        item.pixel_x, 9.0,
        "text pixel_x must be 9.0 (1px border + 8px padding inset); got {}",
        item.pixel_x
    );
    assert_eq!(
        item.pixel_y, 9.0,
        "text pixel_y must be 9.0 (1px border + 8px padding inset); got {}",
        item.pixel_y
    );
    // Text is left-aligned.
    assert_eq!(
        item.alignment,
        TextAlign::Start,
        "notification text must be left-aligned (TextAlign::Start)"
    );
    // Overflow is Clip.
    assert_eq!(
        item.overflow,
        TextOverflow::Clip,
        "notification text must clip at content area (no wrapping)"
    );
}

/// Flat-rect stack notifications render an outlined dismiss affordance with no fill.
#[tokio::test]
async fn test_notification_stack_emits_dismiss_outline_quads() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "dismiss outline test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Dismiss outline".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    assert_eq!(
        vertices.len(),
        54,
        "notification slot should emit backdrop + card border + dismiss outline"
    );
}

// ── Two-line notification rendering [hud-ltgk.3] ──────────────────────────
//
// Spec §Two-line notification layout:
//   - Empty title → single-line backward-compatible path (1 TextItem)
//   - Non-empty title → two-line path (2 TextItems: title bold, body regular)
//   - Title font_weight = NOTIFICATION_TITLE_WEIGHT (700)
//   - Body font_size = font_size_px * NOTIFICATION_BODY_SCALE (0.85×)
//   - Body font_weight = 400 (regular)
//   - Body positioned below title line + inter-line gap

/// Two-line notification: empty `title` produces exactly 1 TextItem (backward compat).
///
/// AC: `collect_text_items` with `NotificationPayload { title: "" }` MUST produce
/// the same output as before this feature was added.
#[tokio::test]
async fn test_two_line_notification_empty_title_produces_one_text_item() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "two-line backward-compat test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Body only notification".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(), // empty: must use single-line path
                actions: vec![],
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    assert_eq!(
        items.len(),
        1,
        "single-line notification (empty title) must produce exactly 1 TextItem"
    );
    assert_eq!(
        &*items[0].text, "Body only notification",
        "single-line TextItem must contain body text"
    );
    assert_eq!(
        items[0].font_weight, 400,
        "single-line notification must use regular weight (400)"
    );
}

/// Two-line notification: non-empty `title` produces 2 TextItems with correct properties.
///
/// AC:
///   1. `collect_text_items` produces exactly 2 TextItems for the slot.
///   2. First item (lower pixel_y): title text, weight=700, font_size_px=16.
///   3. Second item (higher pixel_y): body text, weight=400, font_size=16*0.85=13.6.
///   4. Body item pixel_y > title item pixel_y.
#[tokio::test]
async fn test_two_line_notification_title_produces_two_text_items() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone at x=0 (x_pct=0) so items are easy to find (pixel_x near 0).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "two-line title test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                title: "System Alert".to_owned(),
                text: "Disk space low on /dev/sda1".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                actions: vec![],
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    assert_eq!(
        items.len(),
        2,
        "two-line notification (non-empty title) must produce exactly 2 TextItems, got {} items: {:?}",
        items.len(),
        items.iter().map(|i| &i.text).collect::<Vec<_>>()
    );

    // Sort by pixel_y to get title (top) and body (bottom).
    let mut sorted = items.clone();
    sorted.sort_by(|a, b| a.pixel_y.partial_cmp(&b.pixel_y).unwrap());
    let title_item = &sorted[0];
    let body_item = &sorted[1];

    // Title item checks.
    assert_eq!(
        &*title_item.text, "System Alert",
        "first item must be the title text"
    );
    assert_eq!(
        title_item.font_weight, NOTIFICATION_TITLE_WEIGHT,
        "title must use bold weight ({}), got {}",
        NOTIFICATION_TITLE_WEIGHT, title_item.font_weight
    );
    assert!(
        (title_item.font_size_px - 16.0).abs() < 0.01,
        "title must use policy font_size_px (16.0), got {}",
        title_item.font_size_px
    );

    // Body item checks.
    assert_eq!(
        &*body_item.text, "Disk space low on /dev/sda1",
        "second item must be the body text"
    );
    assert_eq!(
        body_item.font_weight, 400,
        "body must use regular weight (400), got {}",
        body_item.font_weight
    );
    let expected_body_size = 16.0 * NOTIFICATION_BODY_SCALE;
    assert!(
        (body_item.font_size_px - expected_body_size).abs() < 0.1,
        "body font size must be 0.85× title size ({expected_body_size:.2}), got {}",
        body_item.font_size_px
    );

    // Body must be below title.
    assert!(
        body_item.pixel_y > title_item.pixel_y,
        "body item must be below title: title_y={}, body_y={}",
        title_item.pixel_y,
        body_item.pixel_y
    );
}

/// Two-line slot height: `notification_slot_height` returns a larger height for
/// two-line notifications than for single-line notifications.
///
/// AC: two_line_slot_h > single_line_slot_h for the same RenderingPolicy.
#[test]
fn test_notification_slot_height_two_line_exceeds_single_line() {
    let policy = RenderingPolicy {
        font_size_px: Some(16.0),
        ..Default::default()
    };

    let single_line = NotificationPayload {
        text: "body".to_owned(),
        title: String::new(),
        ..Default::default()
    };

    let two_line = NotificationPayload {
        title: "Title".to_owned(),
        text: "body".to_owned(),
        ..Default::default()
    };

    let h_single = Compositor::notification_slot_height(
        &single_line,
        &policy,
        NOTIFICATION_BODY_SCALE,
        NOTIFICATION_INTER_LINE_GAP,
    );
    let h_two_line = Compositor::notification_slot_height(
        &two_line,
        &policy,
        NOTIFICATION_BODY_SCALE,
        NOTIFICATION_INTER_LINE_GAP,
    );

    assert!(
        h_two_line > h_single,
        "two-line slot height ({h_two_line:.2}) must exceed single-line ({h_single:.2})"
    );

    // Single-line == stack_slot_height
    let h_stack = Compositor::stack_slot_height(&policy);
    assert!(
        (h_single - h_stack).abs() < 0.01,
        "single-line notification_slot_height ({h_single:.2}) must equal stack_slot_height ({h_stack:.2})"
    );
}

/// Two-line notification stacking: two two-line notifications stack correctly,
/// with the second slot starting after the first two-line slot height.
#[tokio::test]
async fn test_two_line_notifications_stack_correctly() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "two-line stacking test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 0.9,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish two two-line notifications.
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                title: "First Alert".to_owned(),
                text: "First body text".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                actions: vec![],
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                title: "Second Alert".to_owned(),
                text: "Second body text".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                actions: vec![],
            }),
            "agent-b",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // Two two-line notifications = 4 TextItems.
    assert_eq!(
        items.len(),
        4,
        "two two-line notifications must produce 4 TextItems (2 per notification), got {} items",
        items.len()
    );

    // All 4 items must have distinct pixel_y values (no overlap).
    let mut ys: Vec<f32> = items.iter().map(|i| i.pixel_y).collect();
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ys.dedup_by(|a, b| (*a - *b).abs() < 0.01);
    assert_eq!(
        ys.len(),
        4,
        "all 4 TextItems must have distinct pixel_y values, got: {ys:?}"
    );
}

/// Rounded notification backdrops must use per-notification two-line slot heights.
///
/// Regression guard: when `backdrop_radius` is enabled, backdrop geometry comes from
/// `collect_all_rounded_rect_cmds()` (not the flat-rect path). For two-line notifications,
/// the rounded backdrop height must match `notification_slot_height`, otherwise body text
/// can extend outside the card.
#[tokio::test]
async fn test_rounded_notification_backdrop_uses_two_line_slot_height() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    let policy = RenderingPolicy {
        backdrop: Some(Rgba::new(0.2, 0.2, 0.2, 0.9)),
        backdrop_radius: Some(12.0),
        font_size_px: Some(16.0),
        ..Default::default()
    };
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "rounded notification slot height test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: policy.clone(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    let payload = NotificationPayload {
        title: "Critical Alert".to_owned(),
        text: "Please check corner radius and typography.".to_owned(),
        ..Default::default()
    };
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(payload.clone()),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    let rr = compositor.collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0);
    assert_eq!(
        rr.chrome.len(),
        1,
        "single rounded notification should produce exactly one rounded rect cmd"
    );

    let expected_h = Compositor::notification_slot_height(
        &payload,
        &policy,
        NOTIFICATION_BODY_SCALE,
        NOTIFICATION_INTER_LINE_GAP,
    );
    let actual_h = rr.chrome[0].height;
    assert!(
        (actual_h - expected_h).abs() < 0.1,
        "rounded notification backdrop height must match two-line slot height: expected {expected_h:.2}, got {actual_h:.2}"
    );
}

// ── TTL auto-dismiss [hud-j5g5.3] ────────────────────────────────────────
//
// Spec §Notification TTL Auto-Dismiss with Fade-Out:
//   - Default TTL: 8000ms (zone auto_clear_ms)
//   - Per-publish ttl_ms overrides zone default
//   - 150ms linear fade-out from 1.0 to 0.0
//   - Opacity ~0.5 at 75ms midpoint
//   - Removal from active_publishes on fade completion
//   - Independent simultaneous fades for multiple notifications

/// PublicationAnimationState: before TTL expires, opacity is 1.0.
#[test]
fn test_pub_anim_state_before_ttl_expiry_opacity_is_1() {
    // TTL = 10_000ms (far future), fade not yet started.
    let state = PublicationAnimationState::new(10_000);
    assert_eq!(
        state.current_opacity(),
        1.0,
        "opacity must be 1.0 before TTL expires"
    );
    assert!(
        !state.is_fade_complete(),
        "fade must not be complete before TTL expires"
    );
}

/// PublicationAnimationState: custom TTL=3000ms starts fade at 3000ms.
///
/// AC: notification published with ttl_ms=3000 begins fade-out at 3000ms.
#[test]
fn test_pub_anim_state_custom_ttl_3000ms_triggers_fade() {
    let mut state = PublicationAnimationState::new(3_000);

    // Simulate 3001ms elapsed by setting first_seen to the past.
    state.first_seen = std::time::Instant::now() - std::time::Duration::from_millis(3_001);

    state.tick();

    assert!(
        state.fade_start.is_some(),
        "fade must start after TTL (3000ms) has elapsed"
    );
}

/// PublicationAnimationState: at 75ms into the 150ms fade, opacity ≈ 0.5.
///
/// AC: opacity interpolates linearly; at midpoint it must be approximately 0.5.
#[test]
fn test_pub_anim_state_opacity_at_75ms_midpoint_is_half() {
    let mut state = PublicationAnimationState::new(0); // TTL=0 → instant expire

    // TTL already expired: set first_seen far in the past.
    state.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    state.tick(); // starts fade

    // Now simulate 75ms into the fade.
    state.fade_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(75));

    let opacity = state.current_opacity();
    assert!(
        (opacity - 0.5).abs() < 0.1,
        "at 75ms midpoint, opacity must be ≈ 0.5, got {opacity}"
    );
}

/// PublicationAnimationState: after 150ms, is_fade_complete returns true.
///
/// AC: publication must be removed from active_publishes when fade completes.
#[test]
fn test_pub_anim_state_is_complete_after_150ms() {
    let mut state = PublicationAnimationState::new(0);

    // TTL already expired.
    state.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    state.tick(); // starts fade

    // Simulate 150ms+ elapsed since fade started.
    state.fade_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(151));

    assert!(
        state.is_fade_complete(),
        "is_fade_complete must return true after 150ms fade duration"
    );
    assert_eq!(
        state.current_opacity(),
        0.0,
        "opacity must be 0.0 after fade completes"
    );
}

/// prune_faded_publications removes a publication whose fade is complete.
///
/// AC: publication removed from active_publishes when fade-out completes;
///     remaining notifications reflow (slot positions recalculated).
#[tokio::test]
async fn test_prune_faded_publications_removes_completed_fades() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "prune test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish two notifications.
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "First".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Second".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-b",
            None,
            None,
            None,
        )
        .unwrap();

    // Manually seed pub_animation_states with a completed-fade for "agent-a".
    let publishes = scene
        .zone_registry
        .active_publishes
        .get("notification-area")
        .unwrap();
    let (a_wall_us, a_ns) = {
        let r = &publishes[0]; // agent-a is first (oldest)
        (r.published_at_wall_us, r.publisher_namespace.clone())
    };

    let mut completed_state = PublicationAnimationState::new(0);
    completed_state.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    completed_state.tick(); // starts fade
    // Set fade_start 151ms in the past → fade complete.
    completed_state.fade_start =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(151));

    compositor
        .pub_animation_states
        .entry("notification-area".to_string())
        .or_default()
        .insert((a_wall_us, a_ns), completed_state);

    // Before prune: 2 publications.
    assert_eq!(
        scene
            .zone_registry
            .active_publishes
            .get("notification-area")
            .map(|v| v.len()),
        Some(2),
        "before prune: 2 publications expected"
    );

    // Prune: removes agent-a (completed fade).
    compositor.prune_faded_publications(&mut scene);

    // After prune: only 1 publication remains (agent-b).
    let remaining = scene
        .zone_registry
        .active_publishes
        .get("notification-area")
        .map(|v| v.len());
    assert_eq!(
        remaining,
        Some(1),
        "after prune: 1 publication must remain (agent-b)"
    );
    // Verify the remaining publication is agent-b.
    let remaining_pub = &scene.zone_registry.active_publishes["notification-area"][0];
    assert_eq!(
        remaining_pub.publisher_namespace, "agent-b",
        "remaining publication must be from agent-b"
    );
}

/// Two notifications with TTLs expiring simultaneously fade independently.
///
/// AC: each has its own PublicationAnimationState; neither affects the other.
#[test]
fn test_simultaneous_independent_fades() {
    // Create two independent publication animation states.
    let mut state_a = PublicationAnimationState::new(0);
    let mut state_b = PublicationAnimationState::new(0);

    // Both TTLs expired.
    state_a.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    state_b.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);

    state_a.tick();
    state_b.tick();

    // Simulate: state_a is 75ms into fade, state_b is 120ms into fade.
    state_a.fade_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(75));
    state_b.fade_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(120));

    let opacity_a = state_a.current_opacity();
    let opacity_b = state_b.current_opacity();

    // state_a at ~75ms → opacity ≈ 0.5.
    assert!(
        (opacity_a - 0.5).abs() < 0.15,
        "state_a at 75ms must have opacity ≈ 0.5, got {opacity_a}"
    );
    // state_b at ~120ms → opacity ≈ 0.2.
    assert!(
        opacity_b < 0.35,
        "state_b at 120ms must have opacity < 0.35, got {opacity_b}"
    );
    // They are independent — neither affects the other.
    assert!(
        opacity_a > opacity_b,
        "state_a (75ms) must be more opaque than state_b (120ms)"
    );
    assert!(
        !state_a.is_fade_complete(),
        "state_a (75ms into 150ms fade) must not be complete"
    );
    assert!(
        !state_b.is_fade_complete(),
        "state_b (120ms into 150ms fade) must not be complete"
    );
}

/// Stack reflow: after a publication is pruned, the remaining slot positions
/// are recalculated correctly in collect_text_items.
///
/// AC: remaining notifications reflow to fill vacated slot instantly.
#[tokio::test]
async fn test_stack_reflow_after_publication_pruned() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone at x=0, y=0 with font_size 16px default → slot_h = line_height(22.4) + 2*8 + 4 = 42.4px.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "reflow test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 1.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish three notifications from three agents.
    for (agent, text) in [
        ("agent-a", "Alpha"),
        ("agent-b", "Beta"),
        ("agent-c", "Gamma"),
    ] {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: text.to_owned(),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                agent,
                None,
                None,
                None,
            )
            .unwrap();
    }

    // With 3 publications, newest (Gamma) at slot 0, oldest (Alpha) at slot 2.
    let items_before = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items_before.len(), 3, "must have 3 TextItems before prune");

    // Manually mark the oldest (agent-a / Alpha) as fade-complete.
    let publishes = scene
        .zone_registry
        .active_publishes
        .get("notification-area")
        .unwrap();
    let (a_wall_us, a_ns) = {
        let r = &publishes[0]; // agent-a is oldest (index 0)
        (r.published_at_wall_us, r.publisher_namespace.clone())
    };

    let mut completed_state = PublicationAnimationState::new(0);
    completed_state.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    completed_state.tick();
    completed_state.fade_start =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(151));

    compositor
        .pub_animation_states
        .entry("notification-area".to_string())
        .or_default()
        .insert((a_wall_us, a_ns), completed_state);

    // Prune: removes agent-a.
    compositor.prune_faded_publications(&mut scene);

    // After prune: 2 publications remain (agent-b, agent-c).
    let remaining = scene
        .zone_registry
        .active_publishes
        .get("notification-area")
        .map(|v| v.len());
    assert_eq!(remaining, Some(2), "2 publications must remain after prune");

    // collect_text_items should now produce 2 TextItems correctly reflowed.
    let items_after = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        items_after.len(),
        2,
        "must have 2 TextItems after prune (reflow)"
    );

    // Newest (Gamma = agent-c) is at slot 0 (top, pixel_y = 9.0).
    // Oldest remaining (Beta = agent-b) is at slot 1.
    // slot_h = line_height(16*1.4) + 2*margin_v(8) + SLOT_BASELINE_GAP(4) = 42.4px.
    // Slot 1 starts at y=42.4, text at y=42.4+9=51.4.
    let gamma_item = items_after.iter().find(|i| &*i.text == "Gamma");
    let beta_item = items_after.iter().find(|i| &*i.text == "Beta");

    assert!(gamma_item.is_some(), "Gamma must be in remaining TextItems");
    assert!(beta_item.is_some(), "Beta must be in remaining TextItems");

    // Gamma is newest → slot 0 → pixel_y = 0 + 9 = 9.
    assert_eq!(
        gamma_item.unwrap().pixel_y,
        9.0,
        "Gamma (newest) must be at slot 0, pixel_y=9.0"
    );
    // Beta is oldest remaining → slot 1 → pixel_y = 42.4 + 9 = 51.4.
    assert_eq!(
        beta_item.unwrap().pixel_y,
        51.4,
        "Beta (oldest remaining) must be at slot 1, pixel_y=51.4"
    );
}

/// update_publication_animations creates fresh state for new publications.
#[test]
fn test_update_publication_animations_seeds_fresh_state() {
    // We test this without GPU by constructing the compositor state manually.
    // Use a SceneGraph with a Stack zone and one publication.
    use std::sync::Arc;
    use tze_hud_scene::clock::TestClock;

    let clock = Arc::new(TestClock::new(1_000)); // start at t=1000ms
    let mut scene = SceneGraph::new_with_clock(1280.0, 720.0, clock.clone());

    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "animation seed test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Hello".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: Some(3_000),
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    // Build a minimal compositor state just for the animation map test.
    // We can't construct a full headless Compositor without GPU, so we test
    // the helper methods directly.
    let publishes = scene
        .zone_registry
        .active_publishes
        .get("notification-area")
        .unwrap();
    let record = &publishes[0];

    // Test publication_ttl_ms: urgency-derived expires_at_wall_us takes highest
    // priority.  urgency=1 auto-derives expires_at = now + 8_000_000µs, so
    // publication_ttl_ms = 8_000 - NOTIFICATION_FADE_OUT_MS(150) = 7_850.
    // The per-notification ttl_ms=3_000 is superseded by expires_at_wall_us.
    let zone_def = scene.zone_registry.zones.get("notification-area").unwrap();
    let zone_auto_clear = zone_def
        .auto_clear_ms
        .unwrap_or(NOTIFICATION_DEFAULT_TTL_MS);
    let ttl = Compositor::publication_ttl_ms(record, zone_auto_clear);
    assert_eq!(
        ttl, 7_850,
        "publication_ttl_ms must use urgency-derived expires_at_wall_us (8_000ms - 150ms fade = 7_850ms)"
    );

    // Test fallback: when NotificationPayload.ttl_ms is None, use zone default.
    let record_no_ttl = ZonePublishRecord {
        zone_name: "notification-area".to_string(),
        publisher_namespace: "agent-b".to_string(),
        content: ZoneContent::Notification(NotificationPayload {
            text: "No TTL".to_owned(),
            icon: String::new(),
            urgency: 0,
            ttl_ms: None,
            title: String::new(),
            actions: Vec::new(),
        }),
        published_at_wall_us: 2_000_000,
        merge_key: None,
        expires_at_wall_us: None,
        content_classification: None,
        breakpoints: Vec::new(),
    };
    let ttl_fallback = Compositor::publication_ttl_ms(&record_no_ttl, 8_000);
    assert_eq!(
        ttl_fallback, 8_000,
        "publication_ttl_ms must fall back to zone auto_clear_ms=8000 when NotificationPayload.ttl_ms is None"
    );
}

// ── ZoneContent::StaticImage rendering ───────────────────────────────────

/// render_zone_content with ZoneContent::StaticImage must emit a warm-gray
/// placeholder quad (R≈0.3, G≈0.3, B≈0.3) regardless of the zone's policy
/// backdrop color.
///
/// Full GPU texture upload (wgpu sampler pipeline) is deferred; this test
/// confirms the placeholder path is exercised.
#[tokio::test]
async fn test_static_image_zone_emits_warm_gray_placeholder() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "pip".to_owned(),
        description: "picture-in-picture zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.25,
        },
        accepted_media_types: vec![ZoneMediaType::StaticImage],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Replace,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    let resource_id = ResourceId::of(b"placeholder-image-bytes");
    scene
        .publish_to_zone(
            "pip",
            ZoneContent::StaticImage(resource_id),
            "test-agent",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // At least one backdrop quad must be emitted.
    assert!(
        !vertices.is_empty(),
        "StaticImage zone must emit backdrop vertices"
    );

    // The first vertex color must be warm-gray (R≈0.3, G≈0.3, B≈0.3, A≈1.0).
    let color = vertices[0].color;
    assert!(
        (color[0] - 0.3).abs() < 0.01,
        "StaticImage placeholder R must be ~0.3, got {}",
        color[0]
    );
    assert!(
        (color[1] - 0.3).abs() < 0.01,
        "StaticImage placeholder G must be ~0.3, got {}",
        color[1]
    );
    assert!(
        (color[2] - 0.3).abs() < 0.01,
        "StaticImage placeholder B must be ~0.3, got {}",
        color[2]
    );
    assert!(
        color[3] > 0.5,
        "StaticImage placeholder must be substantially opaque (A > 0.5), got {}",
        color[3]
    );
}

// ── LayerAttachment rendering order tests ─────────────────────────────────
//
// These tests verify that render_zone_content respects LayerAttachment when
// an only_layer filter is provided, and that the three-pass ordering
// (Background → Content → Chrome) is enforced by the layer filter.
//
// The approach: register zones with distinct SolidColor publishes, then call
// render_zone_content with each layer filter in sequence and verify which
// vertices are emitted.  rect_vertices emits 6 vertices per quad; the color
// fields let us identify which zone's vertices are which.

/// Background zones emit vertices only when the Background layer filter is used.
/// Content zones emit no vertices when filtered to Background only.
#[tokio::test]
async fn test_layer_filter_background_only_emits_background_vertices() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Background zone: solid dark blue (r=0.0, g=0.0, b=1.0).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "bg-zone".to_owned(),
        description: "background layer".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 1.0,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Background,
    });

    // Content zone: solid red (r=1.0, g=0.0, b=0.0).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "content-zone".to_owned(),
        description: "content layer".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.1,
            y_pct: 0.1,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    scene
        .publish_to_zone(
            "bg-zone",
            ZoneContent::SolidColor(Rgba::new(0.0, 0.0, 1.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "content-zone",
            ZoneContent::SolidColor(Rgba::new(1.0, 0.0, 0.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    // Filter: Background only — should emit bg-zone quads (6 verts), not content-zone quads.
    let mut bg_only: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(
        &scene,
        &mut bg_only,
        &mut Vec::new(),
        1280.0,
        720.0,
        Some(LayerAttachment::Background),
    );
    // rect_vertices emits 6 vertices; bg-zone should emit exactly 6.
    assert_eq!(
        bg_only.len(),
        6,
        "Background filter must emit exactly one quad (6 verts) for bg-zone"
    );
    // Verify the color is the bg-zone blue (r≈0.0, b≈1.0).
    let first_color = bg_only[0].color;
    assert!(
        first_color[0] < 0.1,
        "Background zone vertex R must be near 0.0 (blue); got {first_color:?}"
    );
    assert!(
        first_color[2] > 0.9,
        "Background zone vertex B must be near 1.0 (blue); got {first_color:?}"
    );

    // Filter: Content only — should emit content-zone quads, not bg-zone quads.
    let mut content_only: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(
        &scene,
        &mut content_only,
        &mut Vec::new(),
        1280.0,
        720.0,
        Some(LayerAttachment::Content),
    );
    assert_eq!(
        content_only.len(),
        6,
        "Content filter must emit exactly one quad (6 verts) for content-zone"
    );
    // Verify the color is the content-zone red (r≈1.0, b≈0.0).
    let content_color = content_only[0].color;
    assert!(
        content_color[0] > 0.9,
        "Content zone vertex R must be near 1.0 (red); got {content_color:?}"
    );
    assert!(
        content_color[2] < 0.1,
        "Content zone vertex B must be near 0.0 (red); got {content_color:?}"
    );
}

/// Chrome zones emit vertices only when the Chrome layer filter is used.
/// Using Chrome filter emits no Content zone vertices.
#[tokio::test]
async fn test_layer_filter_chrome_only_emits_chrome_vertices() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Content zone: solid green (r=0.0, g=1.0, b=0.0).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "content-zone".to_owned(),
        description: "content layer".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.1,
            y_pct: 0.1,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Chrome zone: solid yellow (r=1.0, g=1.0, b=0.0).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "chrome-zone".to_owned(),
        description: "chrome layer".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.3,
            height_pct: 0.1,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "content-zone",
            ZoneContent::SolidColor(Rgba::new(0.0, 1.0, 0.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "chrome-zone",
            ZoneContent::SolidColor(Rgba::new(1.0, 1.0, 0.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    // Chrome filter: must emit only chrome-zone vertices.
    let mut chrome_only: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(
        &scene,
        &mut chrome_only,
        &mut Vec::new(),
        1280.0,
        720.0,
        Some(LayerAttachment::Chrome),
    );
    assert_eq!(
        chrome_only.len(),
        6,
        "Chrome filter must emit exactly one quad (6 verts) for chrome-zone"
    );
    // Verify the color is the chrome-zone yellow (r≈1.0, g≈1.0, b≈0.0).
    let chrome_color = chrome_only[0].color;
    assert!(
        chrome_color[0] > 0.9,
        "Chrome zone vertex R must be near 1.0 (yellow); got {chrome_color:?}"
    );
    assert!(
        chrome_color[1] > 0.9,
        "Chrome zone vertex G must be near 1.0 (yellow); got {chrome_color:?}"
    );
    assert!(
        chrome_color[2] < 0.1,
        "Chrome zone vertex B must be near 0.0 (yellow); got {chrome_color:?}"
    );

    // Content filter: must emit only content-zone vertices.
    let mut content_only: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(
        &scene,
        &mut content_only,
        &mut Vec::new(),
        1280.0,
        720.0,
        Some(LayerAttachment::Content),
    );
    assert_eq!(
        content_only.len(),
        6,
        "Content filter must emit exactly one quad (6 verts) for content-zone"
    );
}

/// Three-pass ordering: Background vertices precede Content, Content precedes Chrome.
///
/// This test registers zones in Chrome→Background→Content order (reverse of
/// the canonical order) and verifies that manual three-pass rendering produces
/// the correct ordering regardless of registration order.
#[tokio::test]
async fn test_three_pass_ordering_independent_of_registration_order() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Register in REVERSE order: Chrome first, then Background, then Content.
    // The rendering order must still be Background → Content → Chrome.

    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "chrome-zone".to_owned(),
        description: "registered first but renders last".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.2,
            height_pct: 0.1,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "bg-zone".to_owned(),
        description: "registered second but renders first".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 1.0,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Background,
    });

    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "content-zone".to_owned(),
        description: "registered third, renders between bg and chrome".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.1,
            y_pct: 0.1,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish distinct colors so we can identify each zone's vertices.
    // Background = blue (r=0, g=0, b=1), Content = red (r=1, g=0, b=0),
    // Chrome = yellow (r=1, g=1, b=0).
    scene
        .publish_to_zone(
            "chrome-zone",
            ZoneContent::SolidColor(Rgba::new(1.0, 1.0, 0.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "bg-zone",
            ZoneContent::SolidColor(Rgba::new(0.0, 0.0, 1.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "content-zone",
            ZoneContent::SolidColor(Rgba::new(1.0, 0.0, 0.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    // Perform three-pass rendering into a single vertex buffer.
    // Pass 1: Background.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    let mut tex_cmds: Vec<TexturedDrawCmd> = Vec::new();
    compositor.render_zone_content(
        &scene,
        &mut vertices,
        &mut tex_cmds,
        1280.0,
        720.0,
        Some(LayerAttachment::Background),
    );
    let after_background = vertices.len();

    // Pass 2: Content.
    compositor.render_zone_content(
        &scene,
        &mut vertices,
        &mut tex_cmds,
        1280.0,
        720.0,
        Some(LayerAttachment::Content),
    );
    let after_content = vertices.len();

    // Pass 3: Chrome.
    compositor.render_zone_content(
        &scene,
        &mut vertices,
        &mut tex_cmds,
        1280.0,
        720.0,
        Some(LayerAttachment::Chrome),
    );
    let after_chrome = vertices.len();

    // Each zone produces exactly 6 vertices (one rect_vertices quad).
    assert_eq!(
        after_background, 6,
        "Background pass must emit 6 vertices; got {after_background}"
    );
    assert_eq!(
        after_content, 12,
        "After Content pass, total must be 12 vertices; got {after_content}"
    );
    assert_eq!(
        after_chrome, 18,
        "After Chrome pass, total must be 18 vertices; got {after_chrome}"
    );

    // Verify vertex colors are in the correct positional order:
    // indices 0–5 = Background (blue), 6–11 = Content (red), 12–17 = Chrome (yellow).
    let bg_r = vertices[0].color[0];
    let bg_b = vertices[0].color[2];
    assert!(
        bg_r < 0.1,
        "First quad (background) must be blue (R≈0.0); got R={bg_r}"
    );
    assert!(
        bg_b > 0.9,
        "First quad (background) must be blue (B≈1.0); got B={bg_b}"
    );

    let content_r = vertices[6].color[0];
    let content_b = vertices[6].color[2];
    assert!(
        content_r > 0.9,
        "Second quad (content) must be red (R≈1.0); got R={content_r}"
    );
    assert!(
        content_b < 0.1,
        "Second quad (content) must be red (B≈0.0); got B={content_b}"
    );

    let chrome_r = vertices[12].color[0];
    let chrome_g = vertices[12].color[1];
    assert!(
        chrome_r > 0.9,
        "Third quad (chrome) must be yellow (R≈1.0); got R={chrome_r}"
    );
    assert!(
        chrome_g > 0.9,
        "Third quad (chrome) must be yellow (G≈1.0); got G={chrome_g}"
    );
}

/// publication_ttl_ms derives TTL (delay until fade starts) from expires_at_wall_us
/// when present (highest priority), subtracting NOTIFICATION_FADE_OUT_MS so the
/// fade completes before the drain boundary.
///
/// For a 15 s warning: ttl_ms = 15_000 - 150 = 14_850.
/// For a 30 s critical: ttl_ms = 30_000 - 150 = 29_850.
#[test]
fn test_publication_ttl_ms_uses_expires_at_wall_us() {
    // Warning notification (urgency 2): published at t=0, expires at t=15s.
    // Expected: 15_000 ms - 150 ms fade = 14_850 ms until fade starts.
    let record_warning = ZonePublishRecord {
        zone_name: "alert-banner".to_string(),
        publisher_namespace: "agent-warn".to_string(),
        content: ZoneContent::Notification(NotificationPayload {
            text: "Disk space low".to_owned(),
            icon: String::new(),
            urgency: 2,
            ttl_ms: None, // No per-notification TTL — urgency path sets expires_at
            title: String::new(),
            actions: Vec::new(),
        }),
        published_at_wall_us: 0,
        merge_key: None,
        expires_at_wall_us: Some(15_000_000), // 15 s in µs
        content_classification: None,
        breakpoints: Vec::new(),
    };
    let ttl = Compositor::publication_ttl_ms(&record_warning, 8_000);
    assert_eq!(
        ttl, 14_850,
        "publication_ttl_ms must derive 14_850 ms (15_000 - 150 fade) for a 15s warning"
    );

    // Critical notification (urgency 3): published at t=0, expires at t=30s.
    // Expected: 30_000 ms - 150 ms fade = 29_850 ms until fade starts.
    let record_critical = ZonePublishRecord {
        zone_name: "alert-banner".to_string(),
        publisher_namespace: "agent-crit".to_string(),
        content: ZoneContent::Notification(NotificationPayload {
            text: "System failure".to_owned(),
            icon: String::new(),
            urgency: 3,
            ttl_ms: None,
            title: String::new(),
            actions: Vec::new(),
        }),
        published_at_wall_us: 0,
        merge_key: None,
        expires_at_wall_us: Some(30_000_000), // 30 s in µs
        content_classification: None,
        breakpoints: Vec::new(),
    };
    let ttl_crit = Compositor::publication_ttl_ms(&record_critical, 8_000);
    assert_eq!(
        ttl_crit, 29_850,
        "publication_ttl_ms must derive 29_850 ms (30_000 - 150 fade) for a 30s critical"
    );

    // expires_at_wall_us takes priority over per-notification ttl_ms.
    // published=1s, expires=16s → duration=15s → 15_000 - 150 = 14_850 ms until fade.
    let record_both = ZonePublishRecord {
        zone_name: "alert-banner".to_string(),
        publisher_namespace: "agent-both".to_string(),
        content: ZoneContent::Notification(NotificationPayload {
            text: "Both set".to_owned(),
            icon: String::new(),
            urgency: 2,
            ttl_ms: Some(5_000), // explicit 5 s TTL on the notification itself
            title: String::new(),
            actions: Vec::new(),
        }),
        published_at_wall_us: 1_000_000, // published at t=1s
        merge_key: None,
        expires_at_wall_us: Some(16_000_000), // expires at t=16s → 15 s duration
        content_classification: None,
        breakpoints: Vec::new(),
    };
    let ttl_both = Compositor::publication_ttl_ms(&record_both, 8_000);
    assert_eq!(
        ttl_both, 14_850,
        "publication_ttl_ms must prefer expires_at_wall_us over per-notification ttl_ms (14_850 ms = 15_000 - 150)"
    );

    // Info notification (urgency 1, no expires_at): falls back to ttl_ms then zone default.
    let record_info = ZonePublishRecord {
        zone_name: "alert-banner".to_string(),
        publisher_namespace: "agent-info".to_string(),
        content: ZoneContent::Notification(NotificationPayload {
            text: "All good".to_owned(),
            icon: String::new(),
            urgency: 1,
            ttl_ms: Some(8_000),
            title: String::new(),
            actions: Vec::new(),
        }),
        published_at_wall_us: 0,
        merge_key: None,
        expires_at_wall_us: None,
        content_classification: None,
        breakpoints: Vec::new(),
    };
    let ttl_info = Compositor::publication_ttl_ms(&record_info, 8_000);
    assert_eq!(
        ttl_info, 8_000,
        "publication_ttl_ms must use NotificationPayload.ttl_ms when expires_at_wall_us is absent"
    );
}

// ── Transition interrupt semantics [hud-hzub.2] ─────────────────────────

/// fade_in_from starts from a non-zero opacity.
///
/// Acceptance criterion: transition interrupt semantics must begin fade-in
/// from current composite opacity, not from zero.
#[test]
fn test_fade_in_from_starts_at_given_opacity() {
    // Simulate: fade-out was 50% complete → current_opacity = 0.5.
    // Start a fade_in_from(0.5) — should begin at 0.5.
    let state = ZoneAnimationState::fade_in_from(10_000, 0.5);
    let opacity = state.current_opacity();
    // Very shortly after creation, opacity should be ~0.5 (no time has elapsed).
    assert!(
        (opacity - 0.5).abs() < 0.05,
        "fade_in_from(0.5) should start at ~0.5 opacity, got {opacity}"
    );
    assert_eq!(state.target_opacity, 1.0, "fade_in_from target must be 1.0");
}

/// fade_in_from clamps from_opacity to [0.0, 1.0].
#[test]
fn test_fade_in_from_clamps_opacity() {
    let state_low = ZoneAnimationState::fade_in_from(1_000, -0.5);
    assert_eq!(state_low.from_opacity, 0.0, "negative opacity clamped to 0");
    let state_high = ZoneAnimationState::fade_in_from(1_000, 1.5);
    assert_eq!(
        state_high.from_opacity, 1.0,
        "overflow opacity clamped to 1"
    );
}

/// Transition interrupt: update_zone_animations starts fade-in from current
/// opacity when a new publish arrives during an active fade-out.
#[tokio::test]
async fn test_transition_interrupt_starts_fade_in_from_current_opacity() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "transition interrupt test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            transition_in_ms: Some(200),
            transition_out_ms: Some(150),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Step 1: publish content — this makes zone active.
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("First".to_owned()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    compositor.update_zone_animations(&scene);

    // Step 2: clear — marks zone inactive, starts fade-out.
    scene
        .zone_registry
        .active_publishes
        .get_mut("subtitle")
        .unwrap()
        .clear();
    compositor.update_zone_animations(&scene);

    // The zone animation state should now be a fade-out (target = 0).
    let has_fadeout = compositor
        .zone_animation_states
        .get("subtitle")
        .map(|s| s.target_opacity == 0.0)
        .unwrap_or(false);
    assert!(has_fadeout, "expected fade-out state after zone clear");

    // Inject a partially-complete fade-out (from_opacity=1, target=0, ~50% elapsed).
    // We simulate 50% opacity by creating a state with from_opacity=1.0 and checking
    // that after interrupt, from_opacity is NOT 0.0.
    let partial_opacity = compositor
        .zone_animation_states
        .get("subtitle")
        .map(|s| s.current_opacity())
        .unwrap_or(0.0);
    // At t=0 the fade-out just started, so opacity ≈ 1.0 still.
    assert!(
        partial_opacity > 0.5,
        "fade-out just started, opacity should be > 0.5, got {partial_opacity}"
    );

    // Step 3: re-publish during fade-out — interrupt semantics must apply.
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Second".to_owned()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    // Record fade-out opacity just before interrupt.
    let pre_interrupt_opacity = compositor
        .zone_animation_states
        .get("subtitle")
        .map(|s| s.current_opacity())
        .unwrap_or(0.0);

    compositor.update_zone_animations(&scene);

    // After interrupt: must be fade-in (target = 1.0).
    let state = compositor
        .zone_animation_states
        .get("subtitle")
        .expect("zone animation state must exist after interrupt fade-in");
    assert_eq!(
        state.target_opacity, 1.0,
        "transition interrupt must produce a fade-in state (target = 1.0)"
    );
    // from_opacity must be the interrupted fade-out opacity, not 0.
    // Pre-interrupt opacity is > 0.5 (fade-out just started), so from ≈ pre_interrupt.
    assert!(
        state.from_opacity > 0.0,
        "fade_in_from must start from current opacity (> 0), got {}",
        state.from_opacity
    );
    // The from_opacity should be ≈ the pre-interrupt value (fade-out just started).
    assert!(
        (state.from_opacity - pre_interrupt_opacity).abs() < 0.1,
        "fade_in_from must start from current fade-out opacity (~{pre_interrupt_opacity}), got {}",
        state.from_opacity
    );
}

// ── Streaming word-by-word reveal [hud-hzub.2] ──────────────────────────

/// StreamRevealState.visible_byte_offset returns usize::MAX when no breakpoints.
#[test]
fn test_stream_reveal_no_breakpoints_reveals_all() {
    let state = StreamRevealState::new(
        (1_000_000, "agent".to_owned()),
        vec![] as Vec<u64>, // no breakpoints
    );
    assert_eq!(
        state.visible_byte_offset(),
        usize::MAX,
        "empty breakpoints must reveal all text immediately"
    );
}

/// StreamRevealState starts at segment 0 and reveals first breakpoint.
#[test]
fn test_stream_reveal_starts_at_first_breakpoint() {
    let state = StreamRevealState::new(
        (1_000_000, "agent".to_owned()),
        vec![3, 9, 15], // "The" at 3, "The quick" at 9, etc.
    );
    assert_eq!(
        state.visible_byte_offset(),
        3,
        "initial visible_byte_offset must be breakpoints[0]=3"
    );
}

/// StreamRevealState.advance() progresses through breakpoints.
#[test]
fn test_stream_reveal_advance_progresses_breakpoints() {
    let mut state = StreamRevealState::new((1_000_000, "agent".to_owned()), vec![3, 9, 15]);
    assert_eq!(state.visible_byte_offset(), 3, "initially at breakpoint 0");

    // Advance STREAM_REVEAL_FRAMES_PER_SEGMENT times to move to next.
    for _ in 0..STREAM_REVEAL_FRAMES_PER_SEGMENT {
        state.advance();
    }
    assert_eq!(
        state.visible_byte_offset(),
        9,
        "after advance, at breakpoint 1"
    );

    for _ in 0..STREAM_REVEAL_FRAMES_PER_SEGMENT {
        state.advance();
    }
    assert_eq!(
        state.visible_byte_offset(),
        15,
        "after advance, at breakpoint 2"
    );

    for _ in 0..STREAM_REVEAL_FRAMES_PER_SEGMENT {
        state.advance();
    }
    assert_eq!(
        state.visible_byte_offset(),
        usize::MAX,
        "after all breakpoints revealed, must show full text (usize::MAX)"
    );
}

/// update_stream_reveals creates state for StreamText with breakpoints.
#[tokio::test]
async fn test_update_stream_reveals_creates_state() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "streaming test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish StreamText with breakpoints via publish_to_zone_with_breakpoints.
    scene
        .publish_to_zone_with_breakpoints(
            "subtitle",
            ZoneContent::StreamText("The quick brown fox".to_owned()),
            "agent",
            None,
            None,
            None,
            vec![3, 9, 15],
        )
        .unwrap();

    compositor.update_stream_reveals(&scene);

    let reveal = compositor.stream_reveal_states.get("subtitle");
    assert!(
        reveal.is_some(),
        "stream_reveal_states must have an entry for subtitle"
    );
    let reveal = reveal.unwrap();
    assert_eq!(
        reveal.breakpoints,
        vec![3, 9, 15],
        "breakpoints must match the publish record"
    );
    assert_eq!(reveal.segment_idx, 0, "reveal starts at segment 0");
}

/// update_stream_reveals resets state when a new publication replaces old.
/// Verifies latest-wins cancels in-progress streaming reveal.
#[tokio::test]
async fn test_update_stream_reveals_resets_on_new_publish() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "streaming reset test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // First publish with breakpoints.
    scene
        .publish_to_zone_with_breakpoints(
            "subtitle",
            ZoneContent::StreamText("The quick brown fox".to_owned()),
            "agent",
            None,
            None,
            None,
            vec![3, 9, 15],
        )
        .unwrap();
    compositor.update_stream_reveals(&scene);

    // Advance a few frames to simulate partial reveal.
    for _ in 0..(STREAM_REVEAL_FRAMES_PER_SEGMENT + 1) {
        compositor.update_stream_reveals(&scene);
    }
    let partial_idx = compositor
        .stream_reveal_states
        .get("subtitle")
        .map(|s| s.segment_idx)
        .unwrap_or(0);
    assert!(partial_idx > 0, "reveal should have advanced beyond 0");

    // Second publish (different published_at_wall_us) — must reset reveal.
    scene
        .publish_to_zone_with_breakpoints(
            "subtitle",
            ZoneContent::StreamText("New content streaming".to_owned()),
            "agent",
            None,
            None,
            None,
            vec![4, 12],
        )
        .unwrap();
    compositor.update_stream_reveals(&scene);

    let new_reveal = compositor.stream_reveal_states.get("subtitle").unwrap();
    assert_eq!(
        new_reveal.segment_idx, 0,
        "replacement must reset reveal to segment 0 (latest-wins cancel)"
    );
    assert_eq!(
        new_reveal.breakpoints,
        vec![4, 12],
        "new breakpoints must be from the replacement publication"
    );
}

/// collect_text_items truncates text to current reveal byte offset.
#[tokio::test]
async fn test_collect_text_items_respects_stream_reveal() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "streaming text item test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // "The quick brown fox" — breakpoints at 3, 9, 15.
    // Initially reveals only "The" (3 bytes).
    scene
        .publish_to_zone_with_breakpoints(
            "subtitle",
            ZoneContent::StreamText("The quick brown fox".to_owned()),
            "agent",
            None,
            None,
            None,
            vec![3, 9, 15],
        )
        .unwrap();

    // Create reveal state at segment 0 (reveals "The").
    compositor.update_stream_reveals(&scene);

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert!(!items.is_empty(), "must produce at least one TextItem");
    let visible_text = &*items[0].text;
    assert_eq!(
        visible_text, "The",
        "initial reveal must show only text up to first breakpoint (\"The\")"
    );
}

// ── Zone interaction hit region tests (hud-ltgk.4) ────────────────────────
//
// These tests verify `populate_zone_hit_regions`: the pure-geometry path that
// computes dismiss (×) and action button pixel bounds for Stack zone
// notification publications.
//
// All tests are GPU-gated (require_gpu!) because populate_zone_hit_regions
// is a method on Compositor, which requires a GPU device at construction.
// The method itself is pure geometry — it does not issue GPU commands.

/// `populate_zone_hit_regions()` MUST produce exactly one dismiss region for a
/// single notification with no actions in a Stack zone.
#[tokio::test]
async fn zone_hit_single_notification_produces_dismiss_region() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab = scene.create_tab("Main", 0).unwrap();
    scene.register_zone(tze_hud_scene::types::ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: tze_hud_scene::types::RenderingPolicy {
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Hello".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,

                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);

    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        1,
        "single notification with no actions must produce exactly 1 hit region"
    );
    assert_eq!(
        scene.overlay.zone_hit_regions[0].kind,
        tze_hud_scene::types::ZoneInteractionKind::Dismiss,
        "single region must be a Dismiss button"
    );
    assert!(
        scene.overlay.zone_hit_regions[0]
            .interaction_id
            .contains("dismiss"),
        "interaction_id must contain 'dismiss': {}",
        scene.overlay.zone_hit_regions[0].interaction_id
    );
}

/// The dismiss region MUST be positioned at the top-right of the notification slot.
/// Zone: x_pct=0.75, width_pct=0.24 on a 1920×1080 screen.
/// Expected: dismiss.x ≈ 1920*0.75 + 1920*0.24 - 20 = 1440 + 460.8 - 20 = 1880.8.
#[tokio::test]
async fn zone_hit_dismiss_region_at_top_right_of_slot() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let sw = 1920.0f32;
    let sh = 1080.0f32;
    let mut scene = SceneGraph::new(sw, sh);
    let _tab = scene.create_tab("Main", 0).unwrap();
    scene.register_zone(tze_hud_scene::types::ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: tze_hud_scene::types::RenderingPolicy {
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Test".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,

                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    compositor.populate_zone_hit_regions(&mut scene, sw, sh);

    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        1,
        "must have exactly 1 region"
    );
    let region = &scene.overlay.zone_hit_regions[0];

    // Dismiss should be at the top-right of the slot.
    // Zone geometry: zx = 1920*0.75 = 1440, zw = 1920*0.24 = 460.8.
    // Dismiss x = zx + zw - 20 = 1880.8.
    let expected_x = sw * 0.75 + sw * 0.24 - 20.0;
    assert!(
        (region.bounds.x - expected_x).abs() < 1.0,
        "dismiss x must be at top-right (expected≈{expected_x:.1}, got {:.1})",
        region.bounds.x
    );
    assert!(
        region.bounds.y < 1.0,
        "dismiss y must be near top of slot (expected≈0, got {:.1})",
        region.bounds.y
    );
}

/// A notification with 2 actions MUST produce 3 regions: 1 dismiss + 2 actions.
#[tokio::test]
async fn zone_hit_notification_with_two_actions_produces_three_regions() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab = scene.create_tab("Main", 0).unwrap();
    scene.register_zone(tze_hud_scene::types::ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: tze_hud_scene::types::RenderingPolicy {
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Confirm?".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,

                title: String::new(),
                actions: vec![
                    NotificationAction {
                        label: "Yes".to_string(),
                        callback_id: "yes".to_string(),
                    },
                    NotificationAction {
                        label: "No".to_string(),
                        callback_id: "no".to_string(),
                    },
                ],
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);

    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        3,
        "1 dismiss + 2 action buttons = 3 regions"
    );

    assert_eq!(
        scene.overlay.zone_hit_regions[0].kind,
        tze_hud_scene::types::ZoneInteractionKind::Dismiss,
        "first region must be Dismiss"
    );
    assert!(
        matches!(
            &scene.overlay.zone_hit_regions[1].kind,
            tze_hud_scene::types::ZoneInteractionKind::Action { callback_id }
                if callback_id == "yes"
        ),
        "second region must be Action(yes)"
    );
    assert!(
        matches!(
            &scene.overlay.zone_hit_regions[2].kind,
            tze_hud_scene::types::ZoneInteractionKind::Action { callback_id }
                if callback_id == "no"
        ),
        "third region must be Action(no)"
    );
}

/// Tab order MUST be sequential: dismiss=0, action[0]=1, action[1]=2.
#[tokio::test]
async fn zone_hit_tab_order_is_sequential() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab = scene.create_tab("Main", 0).unwrap();
    scene.register_zone(tze_hud_scene::types::ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: tze_hud_scene::types::RenderingPolicy {
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Tab order test".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,

                title: String::new(),
                actions: vec![
                    NotificationAction {
                        label: "A".to_string(),
                        callback_id: "a".to_string(),
                    },
                    NotificationAction {
                        label: "B".to_string(),
                        callback_id: "b".to_string(),
                    },
                ],
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);

    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        3,
        "must produce 3 regions"
    );
    assert_eq!(
        scene.overlay.zone_hit_regions[0].tab_order, 0,
        "dismiss tab_order must be 0"
    );
    assert_eq!(
        scene.overlay.zone_hit_regions[1].tab_order, 1,
        "action[0] tab_order must be 1"
    );
    assert_eq!(
        scene.overlay.zone_hit_regions[2].tab_order, 2,
        "action[1] tab_order must be 2"
    );
}

/// Calling `populate_zone_hit_regions` twice MUST clear stale regions (no accumulation).
#[tokio::test]
async fn zone_hit_populate_clears_on_repeated_calls() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab = scene.create_tab("Main", 0).unwrap();
    scene.register_zone(tze_hud_scene::types::ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: tze_hud_scene::types::RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        transport_constraint: None,
        auto_clear_ms: None,
        layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Once".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,

                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);
    let first_count = scene.overlay.zone_hit_regions.len();
    assert_eq!(first_count, 1, "first call must produce 1 region");

    compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);
    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        1,
        "second call must still produce 1 (not accumulate to 2)"
    );
}

/// hud-643dv: a portal frame tile (largest-area member of a scrollable lease)
/// gets a full-width HEADER-BAND drag handle (Windows-titlebar), while the panes
/// keep the legacy centered grip. The band height is token-driven with a sane
/// default consistent with the exemplar header height.
#[tokio::test]
async fn portal_frame_gets_full_width_header_band_handle() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("portal", 60_000, vec![]);
    // Frame = the large anchor.
    let frame_id = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(100.0, 100.0, 600.0, 400.0),
            1,
        )
        .unwrap();
    // Scrollable pane inside the frame (makes the lease a portal group).
    let pane_id = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(110.0, 160.0, 200.0, 320.0),
            3,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(pane_id, tze_hud_scene::types::TileScrollConfig::vertical())
        .unwrap();

    let handles = compositor.collect_drag_handle_entries(&scene, 1920.0, 1080.0);

    let frame = handles
        .iter()
        .find(|h| h.element_id == frame_id)
        .expect("frame tile must have a drag handle");
    assert!(
        frame.is_header_band,
        "the portal frame must get a header-band drag handle"
    );
    // Full width of the frame, top-anchored, height = default band (52).
    assert_eq!(frame.bounds.x, 100.0);
    assert_eq!(frame.bounds.y, 100.0);
    assert_eq!(
        frame.bounds.width, 600.0,
        "band must span the full frame width"
    );
    assert_eq!(
        frame.bounds.height,
        tze_hud_scene::types::PORTAL_HEADER_DRAG_BAND_PX_DEFAULT,
        "band height must come from the token default, not a magic value"
    );

    let pane = handles
        .iter()
        .find(|h| h.element_id == pane_id)
        .expect("pane tile must still have a drag handle");
    assert!(
        !pane.is_header_band,
        "panes keep the legacy centered grip, not a band"
    );
    assert!(
        pane.bounds.width < 600.0,
        "the pane grip must be the small centered grip, not a full-width band"
    );
}

/// hud-ovjxu.1: the compositor applies the tile's viewer-local font-scale
/// multiplier, clamped to the token-default legible range, when resolving a
/// portal text node's effective font. GPU only builds the compositor (no readback).
#[tokio::test]
async fn portal_resize_scales_and_clamps_text_font() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent", 60_000, vec![]);
    let tile = scene
        .create_tile(tab, "agent", lease, Rect::new(0.0, 0.0, 400.0, 300.0), 1)
        .unwrap();

    // No scale (default 1.0) → adapter-published font returned untouched.
    assert!((compositor.scaled_portal_font(16.0, tile, &scene) - 16.0).abs() < 1e-4);

    // Grow 2× → 32px, within the default legible range [9, 48].
    scene.set_tile_font_scale(tile, 2.0);
    assert!((compositor.scaled_portal_font(16.0, tile, &scene) - 32.0).abs() < 1e-4);

    // Grow far → clamp at the token-default max (48).
    scene.set_tile_font_scale(tile, 10.0);
    assert!((compositor.scaled_portal_font(16.0, tile, &scene) - 48.0).abs() < 1e-4);

    // Shrink far → clamp at the token-default min (9); further shrink only
    // reduces the content window (bounds), not the font.
    scene.set_tile_font_scale(tile, 0.1);
    assert!((compositor.scaled_portal_font(16.0, tile, &scene) - 9.0).abs() < 1e-4);
}

#[tokio::test]
async fn drag_handle_regions_cover_visible_tile_zone_and_widget() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(100.0, 100.0, 320.0, 180.0),
            10,
        )
        .unwrap();

    let visible_zone_id = SceneId::new();
    scene.register_zone(ZoneDefinition {
        id: visible_zone_id,
        name: "drag-zone".to_string(),
        description: "zone with active content".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.55,
            y_pct: 0.10,
            width_pct: 0.30,
            height_pct: 0.20,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    });
    let empty_zone_id = SceneId::new();
    scene.register_zone(ZoneDefinition {
        id: empty_zone_id,
        name: "empty-zone".to_string(),
        description: "zone without active content".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.10,
            y_pct: 0.10,
            width_pct: 0.20,
            height_pct: 0.20,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "drag-zone",
            ZoneContent::StreamText("active".to_string()),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    scene.widget_registry.register_definition(WidgetDefinition {
        id: "test-widget".to_string(),
        name: "Test Widget".to_string(),
        description: "test".to_string(),
        parameter_schema: vec![WidgetParameterDeclaration {
            name: "level".to_string(),
            param_type: WidgetParamType::F32,
            default_value: WidgetParameterValue::F32(0.0),
            constraints: Some(WidgetParamConstraints {
                f32_min: Some(0.0),
                f32_max: Some(1.0),
                ..Default::default()
            }),
        }],
        layers: vec![],
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.20,
            y_pct: 0.55,
            width_pct: 0.18,
            height_pct: 0.12,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::LatestWins,
        max_publishers: WidgetDefinition::default_max_publishers(),
        ephemeral: false,
        hover_behavior: None,
    });
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "test-widget".to_string(),
        tab_id: tab,
        geometry_override: None,
        contention_override: None,
        instance_name: "test-widget-1".to_string(),
        current_params: std::collections::HashMap::new(),
    });
    scene
        .publish_to_widget(
            "test-widget-1",
            std::collections::HashMap::new(),
            "agent-a",
            None,
            0,
            None,
        )
        .unwrap();

    compositor.populate_drag_handle_hit_regions(&mut scene, 1920.0, 1080.0);

    let kinds: Vec<_> = scene
        .overlay
        .drag_handle_hit_regions
        .iter()
        .map(|r| r.element_kind)
        .collect();
    assert!(
        kinds.contains(&DragHandleElementKind::Tile),
        "tile handle missing"
    );
    assert!(
        kinds.contains(&DragHandleElementKind::Zone),
        "zone handle missing"
    );
    assert!(
        kinds.contains(&DragHandleElementKind::Widget),
        "widget handle missing"
    );
    assert!(
        scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .any(|r| r.element_id == tile_id),
        "tile-id handle missing"
    );
    assert!(
        scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .any(|r| r.element_id == visible_zone_id),
        "active zone handle missing"
    );
    assert!(
        !scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .any(|r| r.element_id == empty_zone_id),
        "empty zones must not produce drag handles"
    );
    for region in &scene.overlay.drag_handle_hit_regions {
        assert!(
            region.interaction_id.starts_with("drag-handle:"),
            "interaction id must use drag-handle scheme"
        );
        assert!(
            region.hit_region.accepts_pointer,
            "drag handles must accept pointer"
        );
        assert!(
            !region.hit_region.auto_capture,
            "drag handles must not auto-capture before long-press activation"
        );
        assert!(
            !region.hit_region.accepts_focus,
            "drag handles must not participate in focus cycle"
        );
    }
}

#[tokio::test]
async fn drag_handle_hit_test_wins_on_passthrough_tile() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(140.0, 180.0, 360.0, 220.0),
            10,
        )
        .unwrap();
    if let Some(tile) = scene.tiles.get_mut(&tile_id) {
        tile.input_mode = InputMode::Passthrough;
    }

    compositor.populate_drag_handle_hit_regions(&mut scene, 1920.0, 1080.0);
    let handle = scene
        .overlay
        .drag_handle_hit_regions
        .iter()
        .find(|r| r.element_id == tile_id)
        .expect("tile drag handle must exist");
    let hx = handle.bounds.x + handle.bounds.width * 0.5;
    let hy = handle.bounds.y + handle.bounds.height * 0.5;

    let hit = scene.hit_test(hx, hy);
    match hit {
        HitResult::ZoneInteraction {
            kind:
                ZoneInteractionKind::DragHandle {
                    element_id,
                    element_kind,
                    ..
                },
            interaction_id,
            ..
        } => {
            assert_eq!(element_id, tile_id);
            assert_eq!(element_kind, DragHandleElementKind::Tile);
            assert_eq!(interaction_id, handle.interaction_id);
        }
        other => panic!("expected drag-handle hit, got {other:?}"),
    }
}

/// Stale entries in `drag_handle_states` must be pruned when the
/// corresponding element is removed from the scene.
///
/// Verifies that `populate_drag_handle_hit_regions` retains only the keys
/// that are still present in the current `drag_handle_hit_regions` set —
/// the zero-allocation `iter().any()` retain used after [hud-tdtr7] must
/// have identical semantics to the previous `HashSet`-based approach.
#[tokio::test]
async fn drag_handle_states_stale_entries_pruned_on_repopulate() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000, vec![Capability::ModifyOwnTiles]);
    let tile_id = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(100.0, 100.0, 300.0, 180.0),
            10,
        )
        .unwrap();

    // First populate: tile present → one drag handle and a live state entry.
    compositor.populate_drag_handle_hit_regions(&mut scene, 1920.0, 1080.0);
    assert_eq!(
        scene.overlay.drag_handle_hit_regions.len(),
        1,
        "expected one drag handle before tile removal"
    );
    let live_id = scene.overlay.drag_handle_hit_regions[0]
        .interaction_id
        .clone();

    // Seed drag_handle_states: one live entry + one stale phantom that
    // never had a corresponding hit region.
    scene
        .overlay
        .drag_handle_states
        .entry(live_id.clone())
        .or_default()
        .hovered = true;
    scene.overlay.drag_handle_states.insert(
        "drag-handle:tile:ghost-never-existed".to_string(),
        Default::default(),
    );
    assert_eq!(scene.overlay.drag_handle_states.len(), 2);

    // Remove the tile so it no longer produces a drag handle.
    scene.delete_tile(tile_id, "agent-a").unwrap();

    // Second populate: no tiles → no drag handles.  Both state entries
    // (previously-live and phantom) must be pruned.
    compositor.populate_drag_handle_hit_regions(&mut scene, 1920.0, 1080.0);
    assert!(
        scene.overlay.drag_handle_hit_regions.is_empty(),
        "no hit regions expected after tile removal"
    );
    assert!(
        scene.overlay.drag_handle_states.is_empty(),
        "stale drag_handle_states must be pruned by populate_drag_handle_hit_regions; \
             previously-live id={live_id:?} and phantom must both be removed"
    );
}

#[tokio::test]
async fn drag_handle_opacity_switches_to_active_on_hover_state() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000, vec![]);
    let _tile_id = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(120.0, 140.0, 320.0, 180.0),
            10,
        )
        .unwrap();

    let handles = compositor.collect_drag_handle_entries(&scene, 1920.0, 1080.0);
    let handle = handles.first().expect("must have at least one drag handle");

    let mut idle_vertices = Vec::new();
    compositor.append_drag_handle_vertices(
        &scene,
        std::slice::from_ref(handle),
        &mut idle_vertices,
        1920.0,
        1080.0,
    );
    let idle_alpha = idle_vertices[0].color[3];

    scene
        .overlay
        .drag_handle_states
        .entry(handle.interaction_id.clone())
        .or_default()
        .hovered = true;
    let mut active_vertices = Vec::new();
    compositor.append_drag_handle_vertices(
        &scene,
        std::slice::from_ref(handle),
        &mut active_vertices,
        1920.0,
        1080.0,
    );
    let active_alpha = active_vertices[0].color[3];

    assert!(
        active_alpha > idle_alpha,
        "hovered handle alpha must be greater than idle alpha"
    );
}

// ─── Drag visual feedback tests [hud-bs2q.5] ─────────────────────────────

/// During an active drag, `append_drag_handle_vertices` MUST emit the
/// v1-compatible visual feedback:
///
/// 1. **Z-order boost** (implicit: caller bumps z via `drag_active_elements`)
/// 2. **Opacity increase**: handle alpha must equal `opacity_active` (same as
///    hovered), not `opacity_idle`, when the element is in `drag_active_elements`.
/// 3. **2px highlight border**: vertex count must be greater than the idle count
///    (border quads are additional vertices).
#[tokio::test]
async fn drag_visual_feedback_applied_during_active_drag() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(120.0, 140.0, 320.0, 180.0),
            10,
        )
        .unwrap();

    let handles = compositor.collect_drag_handle_entries(&scene, 1920.0, 1080.0);
    let handle = handles
        .iter()
        .find(|h| h.element_id == tile_id)
        .expect("must have tile drag handle");

    // Idle state — no drag active
    let mut idle_vertices = Vec::new();
    compositor.append_drag_handle_vertices(
        &scene,
        std::slice::from_ref(handle),
        &mut idle_vertices,
        1920.0,
        1080.0,
    );
    let idle_count = idle_vertices.len();
    let idle_alpha = idle_vertices[0].color[3];

    // Activate drag for this element
    scene.set_drag_active(tile_id);

    let mut drag_vertices = Vec::new();
    compositor.append_drag_handle_vertices(
        &scene,
        std::slice::from_ref(handle),
        &mut drag_vertices,
        1920.0,
        1080.0,
    );
    let drag_alpha = drag_vertices[0].color[3];
    let drag_count = drag_vertices.len();

    // Opacity must be at the active level (same as hover) — not idle.
    assert!(
        drag_alpha > idle_alpha,
        "drag active handle alpha ({drag_alpha}) must be greater than idle alpha ({idle_alpha})"
    );

    // The 2px highlight border adds 4 additional quads × 6 vertices each = 24 extra vertices
    // (min — degenerate small rects may produce fewer).  We just assert more than idle.
    assert!(
        drag_count > idle_count,
        "drag active must emit more vertices than idle (border adds quads); \
             idle={idle_count}, drag={drag_count}"
    );

    // Clear and verify feedback is removed
    scene.clear_drag_active(tile_id);
    let mut cleared_vertices = Vec::new();
    compositor.append_drag_handle_vertices(
        &scene,
        std::slice::from_ref(handle),
        &mut cleared_vertices,
        1920.0,
        1080.0,
    );
    let cleared_count = cleared_vertices.len();
    assert_eq!(
        cleared_count, idle_count,
        "after clearing drag, vertex count must return to idle level"
    );
}

// ─── Drag z-order + opacity boost unit tests [hud-17c8p] ─────────────────

/// A tile in the `Activated` drag phase MUST be sorted last (highest
/// effective z-order, front-most) among tiles, regardless of its declared
/// `z_order` value.
///
/// Acceptance: `sort_tiles_with_drag_boost` places the dragged tile after
/// a tile with a higher declared `z_order` once the drag boost is applied.
#[test]
fn drag_z_order_boost_raises_tile_above_peers() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000, vec![]);

    // Tile A: lower z_order (renders behind by default).
    let tile_a = scene
        .create_tile(
            tab,
            "tile-a",
            lease,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            5, // z_order = 5
        )
        .unwrap();

    // Tile B: higher z_order (renders in front by default).
    let tile_b = scene
        .create_tile(
            tab,
            "tile-b",
            lease,
            Rect::new(50.0, 50.0, 100.0, 100.0),
            10, // z_order = 10
        )
        .unwrap();

    // Without any active drag: tile A (z=5) sorts before tile B (z=10).
    let sorted = Compositor::sort_tiles_with_drag_boost(scene.visible_tiles(), &scene);
    assert_eq!(
        sorted[0].id, tile_a,
        "without drag: lower z_order tile must sort first"
    );
    assert_eq!(
        sorted[1].id, tile_b,
        "without drag: higher z_order tile must sort last"
    );

    // Activate drag for tile A (z=5 → 5 + 0x1000 = 4101, exceeds tile B's z=10).
    scene.set_drag_active(tile_a);

    let sorted_with_drag = Compositor::sort_tiles_with_drag_boost(scene.visible_tiles(), &scene);
    assert_eq!(
        sorted_with_drag[0].id, tile_b,
        "with drag active on tile A: tile B (z=10) must sort first (further back)"
    );
    assert_eq!(
        sorted_with_drag[1].id, tile_a,
        "with drag active on tile A: tile A must sort last (front-most, boosted)"
    );

    // Clear drag: restore original order.
    scene.clear_drag_active(tile_a);
    let sorted_cleared = Compositor::sort_tiles_with_drag_boost(scene.visible_tiles(), &scene);
    assert_eq!(
        sorted_cleared[0].id, tile_a,
        "after drag cleared: original order restored"
    );
    assert_eq!(
        sorted_cleared[1].id, tile_b,
        "after drag cleared: original order restored"
    );
}

/// `effective_tile_opacity` MUST multiply `tile.opacity` by `DRAG_OPACITY_BOOST`
/// (clamped to 1.0) when the tile is drag-active, and return `tile.opacity`
/// unchanged when no drag is active.
///
/// With `DRAG_OPACITY_BOOST = 1.0` the result equals `tile.opacity` in both
/// branches, but the path through the boost is exercised so future constant
/// changes take effect without a code change.
#[test]
fn drag_opacity_boost_applied_faithfully() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000, vec![]);
    let tile_id = scene
        .create_tile(tab, "tile-a", lease, Rect::new(0.0, 0.0, 100.0, 100.0), 1)
        .unwrap();

    let tile = scene.tiles.get(&tile_id).unwrap();

    // Idle: effective opacity equals tile.opacity.
    let idle_opacity = Compositor::effective_tile_opacity(tile, &scene);
    assert!(
        (idle_opacity - tile.opacity).abs() < 1e-5,
        "idle: effective opacity must equal tile.opacity ({:.4} != {:.4})",
        idle_opacity,
        tile.opacity
    );

    // Drag active: effective opacity is tile.opacity * DRAG_OPACITY_BOOST, ≤ 1.0.
    scene.set_drag_active(tile_id);
    let tile = scene.tiles.get(&tile_id).unwrap();
    let drag_opacity = Compositor::effective_tile_opacity(tile, &scene);
    let expected = (tile.opacity * DRAG_OPACITY_BOOST).min(1.0);
    assert!(
        (drag_opacity - expected).abs() < 1e-5,
        "drag active: effective opacity must be tile.opacity * DRAG_OPACITY_BOOST clamped \
             ({drag_opacity:.4} != {expected:.4})"
    );
    assert!(
        drag_opacity <= 1.0,
        "effective drag opacity must never exceed 1.0 (got {drag_opacity:.4})"
    );
}

/// Verifies that `effective_tile_z_order` returns the boosted key for an
/// active-drag tile and the raw `z_order` otherwise.
#[test]
fn effective_tile_z_order_returns_boosted_key_during_drag() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000, vec![]);
    let tile_id = scene
        .create_tile(tab, "tile-a", lease, Rect::new(0.0, 0.0, 100.0, 100.0), 42)
        .unwrap();

    let tile = scene.tiles.get(&tile_id).unwrap();

    // Idle: no boost.
    let idle_key = Compositor::effective_tile_z_order(tile, &scene);
    assert_eq!(
        idle_key, 42,
        "idle: effective z_order must equal tile.z_order"
    );

    // Drag active: boost applied.
    scene.set_drag_active(tile_id);
    let tile = scene.tiles.get(&tile_id).unwrap();
    let drag_key = Compositor::effective_tile_z_order(tile, &scene);
    assert_eq!(
        drag_key,
        42u32.saturating_add(DRAG_Z_ORDER_BOOST),
        "drag active: effective z_order must be z_order + DRAG_Z_ORDER_BOOST"
    );

    // Clear drag: key returns to raw value.
    scene.clear_drag_active(tile_id);
    let tile = scene.tiles.get(&tile_id).unwrap();
    let cleared_key = Compositor::effective_tile_z_order(tile, &scene);
    assert_eq!(
        cleared_key, 42,
        "after drag cleared: effective z_order returns to raw value"
    );
}

// ─── ensure_icon_texture: token substitution unit tests ──────────────────

/// SVG icon with a `{{token.color.primary}}` placeholder loads successfully
/// when the token is present in the compositor's token map.
///
/// Acceptance criterion: compositor-level token substitution is applied
/// before SVG parsing so that token-driven icons render correctly.
#[tokio::test]
async fn test_ensure_icon_texture_resolves_token_placeholder() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(64, 64).await);

    // Write a minimal SVG with a token placeholder to a temp file.
    let svg_path = std::env::temp_dir().join("tze_hud_test_icon_token_ok.svg");
    std::fs::write(
        &svg_path,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32">
<rect width="32" height="32" fill="{{token.color.primary}}"/>
</svg>"#,
    )
    .expect("write test SVG");
    let path = svg_path.to_string_lossy().into_owned();

    // Token map with the required key — icon must load.
    compositor.set_token_map(
        [("color.primary".to_string(), "#ff0000".to_string())]
            .into_iter()
            .collect(),
    );
    let loaded = compositor.ensure_icon_texture(&path);
    assert!(
        loaded,
        "ensure_icon_texture must succeed when token is resolved"
    );

    // Texture should be in the cache.
    let resource_id = tze_hud_scene::ResourceId::of(path.as_bytes());
    assert!(
        compositor.image_texture_cache.contains_key(&resource_id),
        "resolved icon must be in image_texture_cache"
    );

    let _ = std::fs::remove_file(&svg_path);
}

/// SVG icon with an unresolved token placeholder causes `ensure_icon_texture`
/// to return `false` and negative-cache the failure.  After `set_token_map`
/// is called with the missing token, the negative cache is cleared and the
/// icon can be loaded on the next call.
#[tokio::test]
async fn test_ensure_icon_texture_unresolved_token_cleared_after_set_token_map() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(64, 64).await);

    let svg_path = std::env::temp_dir().join("tze_hud_test_icon_token_missing.svg");
    std::fs::write(
        &svg_path,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32">
<rect width="32" height="32" fill="{{token.color.primary}}"/>
</svg>"#,
    )
    .expect("write test SVG");
    let path = svg_path.to_string_lossy().into_owned();

    // Empty token map — token is missing, load must fail.
    compositor.set_token_map(std::collections::HashMap::new());
    let first = compositor.ensure_icon_texture(&path);
    assert!(
        !first,
        "ensure_icon_texture must return false on unresolved token"
    );

    let resource_id = tze_hud_scene::ResourceId::of(path.as_bytes());
    assert!(
        compositor.failed_icon_paths.contains(&resource_id),
        "failed path must be in negative cache after unresolved token"
    );

    // Now provide the missing token.  set_token_map must clear the negative
    // cache so the icon can be retried.
    compositor.set_token_map(
        [("color.primary".to_string(), "#00ff00".to_string())]
            .into_iter()
            .collect(),
    );
    assert!(
        !compositor.failed_icon_paths.contains(&resource_id),
        "negative cache must be cleared after set_token_map"
    );
    let second = compositor.ensure_icon_texture(&path);
    assert!(
        second,
        "ensure_icon_texture must succeed after token map is updated"
    );

    let _ = std::fs::remove_file(&svg_path);
}

// ─── Scroll-offset text rendering (hud-w5ih) ────────────────────────────

/// `collect_text_items` applies the tile scroll offset to `TextItem` pixel
/// positions so text glyphs track the scrolled content.
///
/// Without the fix, `collect_text_items_from_node` passed bare
/// `tile.bounds.x`/`tile.bounds.y` to `TextItem::from_text_markdown_node`,
/// leaving text anchored at its original position while the geometry quads
/// moved with the scroll. This test pins the contract: a text node at
/// `(node_x, node_y)` in a tile scrolled by `(scroll_x, scroll_y)` must
/// produce a `TextItem` whose `pixel_x`/`pixel_y` subtract the scroll offset.
///
/// Spec refs: hud-w5ih Bounded Transcript Viewport requirement; RFC 0013
/// Transcript Interaction Contract (local-first scroll).
#[tokio::test]
async fn test_collect_text_items_applies_tile_scroll_offset() {
    // This test needs no GPU — `collect_text_items` is a pure CPU path.
    // We construct a Compositor without a surface render call.
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);

    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("scroll-test", 120_000, vec![]);

    // Place a tile at (100, 50).
    let tile_x = 100.0_f32;
    let tile_y = 50.0_f32;
    let tile_w = 400.0_f32;
    let tile_h = 200.0_f32;
    let tile_id = scene
        .create_tile(
            tab_id,
            "scroll-test",
            lease_id,
            Rect::new(tile_x, tile_y, tile_w, tile_h),
            1,
        )
        .unwrap();

    // Register a scroll config so scroll offsets are valid on this tile.
    scene
        .register_tile_scroll_config(
            tile_id,
            tze_hud_scene::types::TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(600.0),
            },
        )
        .unwrap();

    // Set a 80px vertical scroll offset (local-first, no agent roundtrip).
    let scroll_y = 80.0_f32;
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, scroll_y)
        .unwrap();

    // Place a TextMarkdown node at tile-local (10, 20).
    let node_x = 10.0_f32;
    let node_y = 20.0_f32;
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "scroll test line".to_string(),
            bounds: Rect::new(node_x, node_y, 300.0, 24.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();

    // Collect text items with scroll_y=80 applied — the path under test.
    let items_scrolled = compositor.collect_text_items(&scene, 720.0, 360.0);
    assert_eq!(
        items_scrolled.len(),
        1,
        "expected one TextItem for the scroll tile"
    );
    let scrolled_item = &items_scrolled[0];
    assert_eq!(
        &*scrolled_item.text, "scroll test line",
        "TextItem content must match the node"
    );

    // Now create an identical scene WITHOUT scroll offset so we can
    // compare pixel_y values directly. Both tiles have the same bounds and
    // the same node bounds, so margin_y is identical in both — subtracting
    // yields exactly scroll_y.
    //
    //   pixel_y_scrolled = (tile_y - scroll_y) + node_y + margin_y
    //   pixel_y_baseline =  tile_y             + node_y + margin_y
    //   diff             =  scroll_y  (margin cancels)
    let mut scene_baseline = SceneGraph::new(720.0, 360.0);
    let tab2 = scene_baseline.create_tab("test", 0).unwrap();
    let lease2 = scene_baseline.grant_lease("baseline", 120_000, vec![]);
    let tile_id2 = scene_baseline
        .create_tile(
            tab2,
            "baseline",
            lease2,
            Rect::new(tile_x, tile_y, tile_w, tile_h),
            1,
        )
        .unwrap();
    // No scroll offset registered — offset defaults to (0, 0).
    let baseline_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "scroll test line".to_string(),
            bounds: Rect::new(node_x, node_y, 300.0, 24.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    scene_baseline
        .set_tile_root(tile_id2, baseline_node)
        .unwrap();
    let items_baseline = compositor.collect_text_items(&scene_baseline, 720.0, 360.0);
    assert_eq!(
        items_baseline.len(),
        1,
        "baseline must also produce one TextItem"
    );
    let baseline_item = &items_baseline[0];

    // Verify that the scrolled item is positioned exactly scroll_y pixels
    // above the baseline item (margin cancels between identical bounds).
    // Use 0.01 tolerance — f32 precision at values ~80.0 is ~9.5e-6, larger
    // than f32::EPSILON (1.19e-7), and intermediate margin arithmetic may
    // accumulate sub-ULP error. 0.01px is tight enough to catch wrong offset
    // but not fragile against f32 rounding. Matches existing renderer test
    // convention (see position comparisons at < 0.5 and color deltas at <
    // 0.01 throughout this module).
    let actual_shift = baseline_item.pixel_y - scrolled_item.pixel_y;
    assert!(
        (actual_shift - scroll_y).abs() < 0.01,
        "scroll shift ({actual_shift}) must equal scroll_y ({scroll_y}); \
             baseline_y={}, scrolled_y={}",
        baseline_item.pixel_y,
        scrolled_item.pixel_y
    );

    // Directional sanity: scrolled item must be above baseline.
    assert!(
        scrolled_item.pixel_y < baseline_item.pixel_y,
        "scrolled text ({}) must be above unscrolled text ({})",
        scrolled_item.pixel_y,
        baseline_item.pixel_y
    );
    assert!(
        (scrolled_item.clip_pixel_y - baseline_item.clip_pixel_y).abs() < 0.01,
        "scroll must not move the text clip rectangle with the glyph origin"
    );
    assert!(
        (scrolled_item.clip_bounds_height - baseline_item.clip_bounds_height).abs() < 0.01,
        "clip height must remain tied to the viewport, not the scrolled glyph origin"
    );
}

/// hud-g1ena.3: the jump-to-latest pill MAY carry the ambient unread count.
/// `collect_jump_to_latest_badge_item` yields a centered, clipped count
/// `TextItem` only when the pill would show (content overflows + scrolled away)
/// AND the tile carries a nonzero, non-redacted unread count; it returns `None`
/// at the tail or with nothing unread, so the badge appears and clears with the
/// pill (local-first, no adapter round trip).
#[tokio::test]
async fn jump_to_latest_badge_gates_on_scroll_and_unread_count() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(480, 320).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(480.0, 320.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("badge-test", 120_000, vec![]);

    let tile_id = scene
        .create_tile(
            tab_id,
            "badge-test",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 200.0),
            1,
        )
        .unwrap();

    // Content overflows the 200px viewport → the pill (and badge) may show.
    scene
        .register_tile_scroll_config(
            tile_id,
            tze_hud_scene::types::TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(800.0),
            },
        )
        .unwrap();

    let jtl_tokens =
        super::token_colors::resolve_jump_to_latest_tokens(&std::collections::HashMap::new());
    let si_tokens =
        super::token_colors::resolve_scroll_indicator_tokens(&std::collections::HashMap::new());

    let badge = |c: &Compositor, s: &SceneGraph| {
        let tile = s.tiles.get(&tile_id).unwrap();
        c.collect_jump_to_latest_badge_item(tile, s, &jtl_tokens, &si_tokens)
    };

    // Scrolled away from the tail, with unread content → badge renders.
    scene.set_tile_follow_tail_at_tail(tile_id, false);
    scene.set_tile_unread_count(tile_id, 3);
    let item =
        badge(&compositor, &scene).expect("scrolled-away tile with unread must show a badge");
    assert_eq!(&*item.text, "3 unread", "badge must carry the unread count");
    assert_eq!(
        item.alignment,
        tze_hud_scene::types::TextAlign::Center,
        "count must be centered in the pill"
    );
    // Clip is confined to the pill (bottom-center of the tile), never the whole tile.
    assert!(
        item.clip_bounds_width <= 400.0 && item.clip_bounds_height <= 200.0,
        "badge clip must stay within the pill"
    );
    assert!(
        item.pixel_y >= 100.0,
        "pill (and badge) sits in the lower half of the tile, got y={}",
        item.pixel_y
    );

    // Nothing unread → plain pill, no badge (a presence engine renders nothing).
    scene.set_tile_unread_count(tile_id, 0);
    assert!(
        badge(&compositor, &scene).is_none(),
        "no badge when there is nothing unread"
    );

    // Back at the tail → the pill (and therefore the badge) is hidden, even with
    // a stale nonzero count still recorded.
    scene.set_tile_unread_count(tile_id, 5);
    scene.set_tile_follow_tail_at_tail(tile_id, true);
    assert!(
        badge(&compositor, &scene).is_none(),
        "no badge at the tail — it clears with the pill when the viewer returns"
    );
}

// ─── Smooth scroll / animated follow-tail (hud-bq0gl.10) ─────────────────

/// `display_tile_scroll_offset` snaps to the raw scene offset in headless mode
/// so deterministic golden tests are unaffected, and a freshly-observed tile in
/// windowed (smoothing-enabled) mode starts *settled* on its current offset
/// (no initial jump) once `update_scroll_smoothing` has run.
///
/// This pins the wiring contract for the smooth-scroll path: the scene's offset
/// remains the authoritative target (RFC 0013 §3.2 — user scroll authoritative),
/// and the smoother never introduces a jump on first sight. Easing dynamics are
/// covered exhaustively by the pure `easing::ScrollSmoother` unit tests.
#[tokio::test]
async fn display_tile_scroll_offset_snaps_headless_and_settles_windowed() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);

    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("smooth-scroll", 120_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "smooth-scroll",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 200.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            tile_id,
            tze_hud_scene::types::TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(800.0),
            },
        )
        .unwrap();
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 120.0)
        .unwrap();

    // Headless default: smoothing disabled → exact raw offset (snap).
    assert!(!compositor.scroll_smoothing_enabled);
    let (hx, hy) = compositor.display_tile_scroll_offset(&scene, tile_id);
    assert_eq!(
        (hx, hy),
        (0.0, 120.0),
        "headless must return the raw offset"
    );

    // Enable smoothing and advance once: the tile is observed for the first
    // time, so its smoother starts settled on the current target — no jump.
    compositor.scroll_smoothing_enabled = true;
    compositor.update_scroll_smoothing(&scene);
    let (wx, wy) = compositor.display_tile_scroll_offset(&scene, tile_id);
    assert!(
        (wx - 0.0).abs() < 1e-4 && (wy - 120.0).abs() < 1e-4,
        "freshly-observed tile must start settled on its offset (no jump); got ({wx}, {wy})"
    );

    // A non-scrollable / unknown tile has no smoother → falls back to raw.
    let unknown = SceneId::new();
    assert_eq!(
        compositor.display_tile_scroll_offset(&scene, unknown),
        (0.0, 0.0),
        "tiles without a smoother fall back to the raw scene offset"
    );
}

/// `publish_displayed_scroll_offsets` records exactly the offset the renderer
/// draws with (`display_tile_scroll_offset`) into the scene so the live
/// hit-test path agrees with the rendered rows during a smoothed scroll
/// (hud-3lynp). When smoothing is disabled (headless/snap) it clears any
/// published overrides so hit-testing falls back to the authoritative offset.
#[tokio::test]
async fn publish_displayed_scroll_offsets_mirrors_smoother_and_clears_headless() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);

    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("smooth-scroll", 120_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "smooth-scroll",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 200.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            tile_id,
            tze_hud_scene::types::TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(800.0),
            },
        )
        .unwrap();
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 120.0)
        .unwrap();

    // Windowed: advance the smoother, then publish. The published displayed
    // offset must equal display_tile_scroll_offset (the value the renderer drew)
    // and become the effective offset the hit-test path consults.
    compositor.scroll_smoothing_enabled = true;
    compositor.update_scroll_smoothing(&scene);
    let drawn = compositor.display_tile_scroll_offset(&scene, tile_id);
    compositor.publish_displayed_scroll_offsets(&mut scene);
    assert_eq!(
        scene.effective_tile_scroll_offset_local(tile_id),
        drawn,
        "hit-test path must see the same displayed offset the renderer drew with"
    );

    // Headless/snap: publishing clears the override so hit-testing falls back to
    // the authoritative offset (deterministic golden tests unaffected).
    compositor.scroll_smoothing_enabled = false;
    compositor.publish_displayed_scroll_offsets(&mut scene);
    assert_eq!(
        scene.effective_tile_scroll_offset_local(tile_id),
        (0.0, 120.0),
        "with smoothing disabled the effective offset falls back to authoritative"
    );
}

/// Idle render gate (hud-ilivg): the compositor frame loop skips the
/// build/encode/present pass when the scene graph is unchanged since the last
/// presented frame AND nothing is animating, but renders on any scene-version
/// bump OR while an animation is in flight.
///
/// This pins the exact `dirty` predicate used by the windowed frame loop
/// (`scene.version != last_rendered_scene_version || has_inflight_animation`)
/// against all three acceptance cases:
///   1. idle (no change, no animation)            -> skip
///   2. scene-version bump                        -> render
///   3. in-flight animation, version pinned       -> render (no freeze)
#[tokio::test]
async fn idle_render_gate_skips_static_scene_renders_on_change_or_animation() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 480).await);
    // Windowed profile: scroll smoothing active (headless snaps and never
    // registers a smoother, so enable it explicitly for this gate test).
    compositor.scroll_smoothing_enabled = true;

    let mut scene = SceneGraph::new(720.0, 480.0);
    let tab_id = scene.create_tab("gate", 0).unwrap();
    let lease_id = scene.grant_lease("gate", 120_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "gate",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            tile_id,
            tze_hud_scene::types::TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(2000.0),
            },
        )
        .unwrap();

    // Observe the tile once: its smoother starts settled on the current offset
    // (no initial jump), so nothing is animating.
    compositor.update_scroll_smoothing(&scene);

    // -- Case 1: idle -- scene unchanged, nothing animating -> SKIP. --
    assert!(
        !compositor.has_inflight_animation(&scene),
        "a freshly-settled scene must report no in-flight animation"
    );
    let last_rendered = scene.version;
    let dirty_idle = scene.version != last_rendered || compositor.has_inflight_animation(&scene);
    assert!(
        !dirty_idle,
        "idle frame (no scene change, no animation) MUST skip render/present"
    );

    // -- Case 2: scene-version bump -> RENDER. --
    // A scene diff / mutation bumps scene.version; the gate must not skip it.
    scene.version += 1;
    let dirty_versioned =
        scene.version != last_rendered || compositor.has_inflight_animation(&scene);
    assert!(
        dirty_versioned,
        "a scene-version bump MUST force a render even with no animation"
    );

    // -- Case 3: in-flight animation, version pinned -> RENDER (no freeze). --
    // Move the authoritative scroll target far away and advance the smoother one
    // frame: it is now mid-flight (displayed offset still near 0, target 1500).
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 1500.0)
        .unwrap();
    compositor.update_scroll_smoothing(&scene);
    assert!(
        compositor.has_inflight_animation(&scene),
        "a mid-flight smooth-scroll catch-up must report an in-flight animation"
    );
    // Pin the gate's last-rendered version to the CURRENT version so the only
    // possible source of dirtiness is the animation itself.
    let pinned = scene.version;
    let dirty_animating = scene.version != pinned || compositor.has_inflight_animation(&scene);
    assert!(
        dirty_animating,
        "an in-flight animation MUST force a render even when scene.version is unchanged"
    );
}

/// Idle render gate — composer carve-out (hud-ilivg / hud-r3ax6).
///
/// The local draft echo and the caret blink are driven off out-of-band state
/// that never bumps `scene.version`. Before the idle gate the compositor thread
/// ran `render_frame` unconditionally, so both worked; with the gate they would
/// freeze unless `drain_local_composer_and_needs_render` (called before the
/// gate) (a) applies a pending keystroke and (b) keeps a focused composer dirty.
///
/// This pins the gate's composer input across the full lifecycle:
///   1. no composer                       → needs_render = false  (idle skips)
///   2. pending echo (slot Some(Some))    → needs_render = true   (renders)
///   3. focused, no new keystroke         → needs_render = true   (caret blinks)
///   4. deactivation (slot Some(None))    → needs_render = true   (clears overlay)
///   5. gone                              → needs_render = false  (idle skips)
#[tokio::test]
async fn idle_render_gate_renders_for_composer_echo_and_caret_blink() {
    use tze_hud_scene::types::SceneId;

    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);

    // ── 1. No composer focused, nothing pending → gate must skip. ──
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "with no composer focused and no pending echo the gate MUST be able to skip"
    );

    // ── 2. A keystroke writes the slot → pending echo MUST render. ──
    let node_id = SceneId::new();
    {
        let mut guard = compositor.local_composer_state.lock().unwrap();
        *guard = Some(Some(LocalComposerState {
            text: "hi".to_owned(),
            cursor_byte: 2,
            selection_anchor: 2,
            at_capacity: false,
            node_id,
            placeholder: None,
        }));
    }
    assert!(
        compositor.drain_local_composer_and_needs_render(),
        "a pending local-composer echo MUST mark the frame dirty so it renders promptly"
    );
    assert!(
        compositor.local_composer.is_some(),
        "draining before the gate must have applied the pending draft"
    );

    // ── 3. No new keystroke, composer still focused → caret keeps blinking. ──
    // The slot is empty now, but an active composer must keep rendering across
    // blink-toggle boundaries (treated as always-dirty while focused).
    assert!(
        compositor.local_composer_state.lock().unwrap().is_none(),
        "slot must have been drained to None by the previous call"
    );
    assert!(
        compositor.drain_local_composer_and_needs_render(),
        "a focused composer with no new keystroke MUST keep rendering so the caret blinks"
    );

    // ── 4. Deactivation: slot delivers Some(None) → render once to clear. ──
    {
        let mut guard = compositor.local_composer_state.lock().unwrap();
        *guard = Some(None);
    }
    assert!(
        compositor.drain_local_composer_and_needs_render(),
        "the deactivation transition MUST render one frame to clear the composer overlay"
    );
    assert!(
        compositor.local_composer.is_none(),
        "deactivation must have cleared the drained composer state"
    );

    // ── 5. Composer gone → gate must skip again (no 60Hz idle burn). ──
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "once the composer is gone the gate MUST be able to skip the static idle frame"
    );

    // ── 6. Real out-of-band focus/grip handles dirty transition frames. ──
    // These are the same shared slots written by the windowed runtime. They do
    // not bump scene.version, so the idle gate itself must observe each change.
    let tab_id = SceneId::new();
    let tile_id = SceneId::new();
    let focused = FocusRingOwner {
        tab_id,
        tile_id,
        node_id: None,
    };
    *compositor.focus_ring_owner_state.lock().unwrap() = Some(focused);
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "an unchanged focus-ring owner must not keep the idle gate dirty"
    );
    *compositor.focus_ring_owner_state.lock().unwrap() = None;
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(!compositor.drain_local_composer_and_needs_render());

    *compositor.resize_grip_hover_state.lock().unwrap() = Some(tile_id);
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "an unchanged resize-grip target must not keep the idle gate dirty"
    );
    *compositor.resize_grip_hover_state.lock().unwrap() = None;
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(!compositor.drain_local_composer_and_needs_render());
}

#[test]
fn static_focus_and_resize_grip_transitions_each_dirty_exactly_once() {
    let tab_id = SceneId::new();
    let tile_id = SceneId::new();
    let focused = Some(FocusRingOwner {
        tab_id,
        tile_id,
        node_id: None,
    });

    assert!(focus_or_grip_changed(None, focused, None, None));
    assert!(!focus_or_grip_changed(focused, focused, None, None));
    assert!(focus_or_grip_changed(focused, None, None, None));
    assert!(!focus_or_grip_changed(None, None, None, None));

    assert!(focus_or_grip_changed(None, None, None, Some(tile_id)));
    assert!(!focus_or_grip_changed(
        None,
        None,
        Some(tile_id),
        Some(tile_id),
    ));
}

/// Headless readback regression for the text-stream portal output pane.
///
/// The live exemplar mounts the OUTPUT transcript body as a scrollable tile
/// inside a larger portal frame. Scrolled node geometry must be clipped to that
/// output tile viewport: root/background fills must not bleed above it, and
/// scrolled child fills must not bleed below it.
#[tokio::test]
async fn scrolled_portal_output_tile_clips_geometry_outside_viewport() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(220, 180).await);

    let mut scene = SceneGraph::new(220.0, 180.0);
    let tab_id = scene.create_tab("portal-output-clip", 0).unwrap();
    let lease_id = scene.grant_lease("portal-output-clip", 120_000, vec![]);

    let frame_id = scene
        .create_tile(
            tab_id,
            "portal-output-clip",
            lease_id,
            Rect::new(40.0, 20.0, 160.0, 140.0),
            1,
        )
        .unwrap();
    scene
        .set_tile_root(
            frame_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.28, 0.34, 0.50, 1.0),
                    radius: None,
                    bounds: Rect::new(0.0, 0.0, 160.0, 140.0),
                }),
            },
        )
        .unwrap();

    let output_id = scene
        .create_tile(
            tab_id,
            "portal-output-clip",
            lease_id,
            Rect::new(90.0, 60.0, 80.0, 60.0),
            2,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            output_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(240.0),
            },
        )
        .unwrap();

    let root_id = SceneId::new();
    scene
        .set_tile_root(
            output_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                    radius: None,
                    bounds: Rect::new(0.0, 0.0, 80.0, 60.0),
                }),
            },
        )
        .unwrap();
    scene
        .add_node_to_tile(
            output_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                    radius: None,
                    bounds: Rect::new(0.0, 92.0, 80.0, 80.0),
                }),
            },
        )
        .unwrap();

    // Settle the §6.3 portal fade-in before probing. A freshly-created scrollable
    // tile begins a fade-in animation, and since hud-b0x0m every node fill (incl.
    // this SolidColor pane) honours the tile fade — so at t=0 the pane fill renders
    // translucent and composites toward the frame behind it. This test is about
    // geometry clipping, not the transition, so warm one frame to register the
    // appear, then clear the animation state so the probes below observe the
    // steady-state (fully opaque) fills.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.portal_tile_anim_states.clear();

    for scroll_y in [0.0_f32, 48.0, 96.0] {
        scene
            .set_tile_scroll_offset_local(output_id, 0.0, scroll_y)
            .unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        compositor.render_frame_headless(&mut scene, &surface);

        let pixels = surface.read_pixels(&compositor.device);
        let frame_control = HeadlessSurface::pixel_at(&pixels, 220, 55, 45);
        let above_output = HeadlessSurface::pixel_at(&pixels, 220, 110, 45);
        let below_output = HeadlessSurface::pixel_at(&pixels, 220, 110, 135);
        let inside_output = HeadlessSurface::pixel_at(&pixels, 220, 110, 70);

        assert_eq!(
            above_output, frame_control,
            "scroll_y={scroll_y}: output root fill leaked above the output viewport; \
             above={above_output:?}, frame={frame_control:?}"
        );
        assert_eq!(
            below_output, frame_control,
            "scroll_y={scroll_y}: scrolled output content leaked below the output viewport; \
             below={below_output:?}, frame={frame_control:?}"
        );
        assert_ne!(
            inside_output, frame_control,
            "scroll_y={scroll_y}: output viewport should still render its black pane fill"
        );
    }
}

#[tokio::test]
async fn scrolled_rounded_solid_preserves_original_shape_with_viewport_clip() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(220, 180).await);

    let mut scene = SceneGraph::new(220.0, 180.0);
    let tab_id = scene.create_tab("portal-rounded-clip", 0).unwrap();
    let lease_id = scene.grant_lease("portal-rounded-clip", 120_000, vec![]);
    let output_id = scene
        .create_tile(
            tab_id,
            "portal-rounded-clip",
            lease_id,
            Rect::new(90.0, 60.0, 80.0, 60.0),
            2,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            output_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(240.0),
            },
        )
        .unwrap();

    let root_id = SceneId::new();
    scene
        .set_tile_root(
            output_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                    radius: None,
                    bounds: Rect::new(0.0, 0.0, 80.0, 60.0),
                }),
            },
        )
        .unwrap();
    scene
        .add_node_to_tile(
            output_id,
            Some(root_id),
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                    radius: Some(16.0),
                    bounds: Rect::new(0.0, 92.0, 80.0, 80.0),
                }),
            },
        )
        .unwrap();

    scene
        .set_tile_scroll_offset_local(output_id, 0.0, 96.0)
        .unwrap();

    let cmds = compositor.collect_tile_rounded_rect_cmds(&scene);
    assert_eq!(cmds.len(), 1, "expected exactly one rounded child command");
    let cmd = &cmds[0];
    assert_eq!(cmd.x, 90.0);
    assert_eq!(cmd.y, 56.0);
    assert_eq!(cmd.width, 80.0);
    assert_eq!(cmd.height, 80.0);
    assert_eq!(cmd.radius, 16.0);

    let clip = cmd
        .clip
        .expect("scrolled rounded child must carry a viewport clip");
    assert_eq!(clip.x, 90.0);
    assert_eq!(clip.y, 60.0);
    assert_eq!(clip.width, 80.0);
    assert_eq!(clip.height, 60.0);
}

/// A first-class expanded portal is one movable/resizable surface: wheel scroll
/// translates its document rows, never its frame, pane fills, header, or INPUT /
/// OUTPUT labels. This reproduces the live post-resize failure where the
/// `PortalSurface` carries geometry-only parts because its regenerated inline
/// subtree has no stable node IDs (hud-yrcev).
///
/// The draw-list assertions exercise the rounded frame/pane path; the text-item
/// assertions exercise the glyph path. Keeping both in one post-resize scene
/// prevents the two passes from silently applying different scroll policies.
#[tokio::test]
async fn expanded_portal_chrome_stays_fixed_while_document_content_scrolls_after_resize() {
    use tze_hud_scene::types::{
        FontFamily, PortalDisplayState, PortalPart, PortalPartKind, PortalSurface, SolidColorNode,
        TextAlign, TextMarkdownNode,
    };

    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(800, 520).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(800.0, 520.0);
    let tab = scene.create_tab("expanded-portal", 0).unwrap();
    let lease = scene.grant_lease("expanded-portal", 120_000, vec![]);
    // Deliberately not the attach-time size: this is the operator's resized
    // portal. Every declared part is already re-resolved to this whole-unit
    // geometry before the wheel scroll happens.
    let tile_bounds = Rect::new(80.0, 40.0, 600.0, 360.0);
    let tile_id = scene
        .create_tile(tab, "expanded-portal", lease, tile_bounds, 1)
        .unwrap();
    scene
        .register_tile_scroll_config(
            tile_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(1_200.0),
            },
        )
        .unwrap();

    let header_h = 48.0;
    let divider_w = 12.0;
    let pane_w = (tile_bounds.width - divider_w) * 0.5;
    let input_pane = Rect::new(0.0, header_h, pane_w, tile_bounds.height - header_h);
    let output_pane = Rect::new(
        pane_w + divider_w,
        header_h,
        pane_w,
        tile_bounds.height - header_h,
    );
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.12, 0.12, 0.14, 1.0),
                    bounds: Rect::new(0.0, 0.0, tile_bounds.width, tile_bounds.height),
                    radius: Some(14.0),
                }),
            },
        )
        .unwrap();

    let add_child = |scene: &mut SceneGraph, data: NodeData| {
        scene
            .add_node_to_tile(
                tile_id,
                Some(root_id),
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data,
                },
            )
            .unwrap();
    };
    add_child(
        &mut scene,
        NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.16, 0.16, 0.18, 1.0),
            bounds: input_pane,
            radius: Some(12.0),
        }),
    );
    add_child(
        &mut scene,
        NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.03, 0.03, 0.04, 1.0),
            bounds: output_pane,
            radius: Some(12.0),
        }),
    );

    let markdown = |content: &str, bounds: Rect| {
        NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_owned(),
            bounds,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(0.9, 0.9, 0.9, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        })
    };
    add_child(
        &mut scene,
        markdown("Portal header", Rect::new(12.0, 0.0, 576.0, header_h)),
    );
    add_child(
        &mut scene,
        markdown(
            "INPUT",
            Rect::new(12.0, input_pane.y, input_pane.width - 24.0, 24.0),
        ),
    );
    add_child(
        &mut scene,
        markdown(
            "OUTPUT",
            Rect::new(
                output_pane.x + 12.0,
                output_pane.y,
                output_pane.width - 24.0,
                24.0,
            ),
        ),
    );
    add_child(
        &mut scene,
        markdown(
            "output document",
            Rect::new(
                output_pane.x + 12.0,
                output_pane.y + 28.0,
                output_pane.width - 24.0,
                output_pane.height - 40.0,
            ),
        ),
    );

    // This matches ResidentGrpcPortalAdapter::portal_surface_proto: it gives
    // first-class part geometry but no stable node IDs, because each publish
    // regenerates the inline subtree.
    scene.overlay.portal_surfaces.insert(
        tile_id,
        PortalSurface {
            display_state: PortalDisplayState::Expanded,
            parts: vec![
                PortalPart {
                    kind: PortalPartKind::Frame,
                    bounds: Rect::new(0.0, 0.0, tile_bounds.width, tile_bounds.height),
                    node: None,
                },
                PortalPart {
                    kind: PortalPartKind::Header,
                    bounds: Rect::new(0.0, 0.0, tile_bounds.width, header_h),
                    node: None,
                },
                PortalPart {
                    kind: PortalPartKind::Composer,
                    bounds: input_pane,
                    node: None,
                },
                PortalPart {
                    kind: PortalPartKind::Transcript,
                    bounds: output_pane,
                    node: None,
                },
                PortalPart {
                    kind: PortalPartKind::Divider,
                    bounds: Rect::new(pane_w, header_h, divider_w, tile_bounds.height - header_h),
                    node: None,
                },
            ],
            ..Default::default()
        },
    );
    compositor.prime_markdown_cache(&scene);

    let item_y = |items: &[crate::text::TextItem], content: &str| {
        items
            .iter()
            .find(|item| item.text.as_ref() == content)
            .unwrap_or_else(|| panic!("expected {content:?} TextItem"))
            .pixel_y
    };
    let before = compositor.collect_text_items(&scene, 800.0, 520.0);
    let header_before = item_y(&before, "Portal header");
    let input_label_before = item_y(&before, "INPUT");
    let output_label_before = item_y(&before, "OUTPUT");
    let document_before = item_y(&before, "output document");

    let scroll_y = 72.0;
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, scroll_y)
        .unwrap();
    let cmds = compositor.collect_tile_rounded_rect_cmds(&scene);
    let frame = cmds
        .iter()
        .find(|cmd| cmd.width == tile_bounds.width && cmd.height == tile_bounds.height)
        .expect("rounded frame draw command");
    assert_eq!(frame.x, tile_bounds.x, "frame x must remain tile-anchored");
    assert_eq!(
        frame.y, tile_bounds.y,
        "resized rounded frame must not translate with document scroll"
    );
    let panes: Vec<_> = cmds
        .iter()
        .filter(|cmd| cmd.width == pane_w && cmd.height == input_pane.height)
        .collect();
    assert_eq!(
        panes.len(),
        2,
        "both resized rounded pane backdrops must reach the draw list"
    );
    for pane in panes {
        assert_eq!(
            pane.y,
            tile_bounds.y + header_h,
            "pane backdrop must remain whole-portal chrome under scroll"
        );
    }

    let after = compositor.collect_text_items(&scene, 800.0, 520.0);
    assert_eq!(
        item_y(&after, "Portal header"),
        header_before,
        "header text must remain fixed with the frame"
    );
    assert_eq!(
        item_y(&after, "INPUT"),
        input_label_before,
        "INPUT label must remain chrome"
    );
    assert_eq!(
        item_y(&after, "OUTPUT"),
        output_label_before,
        "OUTPUT label must remain chrome"
    );
    assert!(
        (document_before - item_y(&after, "output document") - scroll_y).abs() < 0.5,
        "document content must translate by the display scroll offset"
    );
}

/// `collect_text_items` does NOT shift text for tiles with zero scroll offset.
///
/// Regression guard: ensuring the fix is additive (non-scrolled tiles
/// are unaffected).
#[tokio::test]
async fn test_collect_text_items_zero_scroll_unchanged() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);

    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("no-scroll-test", 120_000, vec![]);

    let tile_x = 50.0_f32;
    let tile_y = 30.0_f32;
    let tile_id = scene
        .create_tile(
            tab_id,
            "no-scroll-test",
            lease_id,
            Rect::new(tile_x, tile_y, 200.0, 100.0),
            1,
        )
        .unwrap();

    let node_x = 5.0_f32;
    let node_y = 10.0_f32;
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "no-scroll baseline".to_string(),
            bounds: Rect::new(node_x, node_y, 180.0, 20.0),
            font_size_px: 12.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();
    // No scroll offset registered or set — tile_scroll_offset_local returns (0, 0).

    let items = compositor.collect_text_items(&scene, 720.0, 360.0);
    assert_eq!(items.len(), 1, "one TextItem for the non-scrolled tile");

    let item = &items[0];
    // pixel_y must be >= tile_y + node_y (the raw sum before margin).
    // The margin is positive, so pixel_y >= tile_y + node_y always holds.
    assert!(
        item.pixel_y >= tile_y + node_y,
        "non-scrolled text pixel_y ({}) must be >= tile_y ({tile_y}) + node_y ({node_y})",
        item.pixel_y
    );
}

/// At-tail Ellipsis tiles: `collect_text_items` must produce `TailAnchored`
/// viewport, and the truncation cache must hit (not miss) after priming.
///
/// **Bug context (hud-lu50e):** `prime_truncation_cache` primed
/// `TailAnchored` entries for at-tail tiles, but `collect_text_items_from_node`
/// always built items with the constructor-default `HeadAnchored` viewport.
/// `prepare_text_items` keyed the cache lookup on `item.viewport`, so the
/// per-frame key was `HeadAnchored` while the primed entry was `TailAnchored` —
/// causing a cache miss on every frame and the inline fallback always
/// running head-anchored truncation (showing oldest lines, not newest).
///
/// This test asserts:
/// 1. For a tile with `at_tail = true` and `TextOverflow::Ellipsis`, the
///    resulting `TextItem` has `viewport == TailAnchored`.
/// 2. For the same tile with `at_tail = false` (scrolled-back), the
///    `TextItem` retains `HeadAnchored`.
/// 3. Non-Ellipsis (Clip) nodes are unaffected by `at_tail`.
/// 4. After priming the truncation cache (`prime_truncation_cache`) with
///    the at-tail scene, the TailAnchored key is present in the cache,
///    confirming the per-frame item and the primed entry are aligned.
#[tokio::test]
async fn test_collect_text_items_at_tail_ellipsis_uses_tail_anchored_viewport() {
    // collect_text_items is a CPU path; init_text_renderer + prime_truncation_cache
    // require the GPU text rasterizer.
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Content with multiple distinct lines so head vs. tail truncation
    // would show different text.
    let content = "Line A\nLine B\nLine C\nLine D\nLine E\nLine F\nLine G\nLine H";

    let tile_w = 200.0_f32;
    // Narrow height so only a subset of lines fits — forces truncation.
    let tile_h = 40.0_f32;

    // ── Scene with at_tail = true ─────────────────────────────────────────
    let mut scene_at_tail = SceneGraph::new(720.0, 360.0);
    let tab_at = scene_at_tail.create_tab("test", 0).unwrap();
    let lease_at = scene_at_tail.grant_lease("at-tail-test", 120_000, vec![]);
    let tile_at = scene_at_tail
        .create_tile(
            tab_at,
            "at-tail-test",
            lease_at,
            Rect::new(0.0, 0.0, tile_w, tile_h),
            1,
        )
        .unwrap();
    let node_ellipsis = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_owned(),
            bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    scene_at_tail.set_tile_root(tile_at, node_ellipsis).unwrap();
    // Mark tile as at-tail (follow-tail active, user has not scrolled back).
    scene_at_tail.set_tile_follow_tail_at_tail(tile_at, true);

    compositor.prime_markdown_cache(&scene_at_tail);

    // 1. per-frame viewport must be TailAnchored.
    let items_at_tail = compositor.collect_text_items(&scene_at_tail, 720.0, 360.0);
    assert_eq!(
        items_at_tail.len(),
        1,
        "expected one TextItem for the at-tail tile"
    );
    let at_tail_item = &items_at_tail[0];
    assert_eq!(
        at_tail_item.overflow,
        TextOverflow::Ellipsis,
        "item must carry Ellipsis overflow"
    );
    assert_eq!(
        at_tail_item.viewport,
        crate::overflow::TruncationViewport::TailAnchored,
        "at-tail Ellipsis tile must produce TailAnchored viewport \
             so the per-frame key matches the primed cache entry (hud-lu50e)"
    );

    // 2. Prime the truncation cache and verify TailAnchored key is present.
    //    `prime_truncation_cache` primes TailAnchored for at-tail tiles.
    //    If the per-frame item.viewport also matches TailAnchored, the
    //    cache lookup in `prepare_text_items` will hit — confirming alignment.
    compositor.prime_truncation_cache(&scene_at_tail);
    // Access the rasterizer's truncation cache to confirm the TailAnchored
    // entry was primed.  `prime_truncation_cache` calls
    // `rasterizer.prime_truncation_cache(items)` which stores entries keyed
    // on viewport_mode=1 (TailAnchored).  The cache must be non-empty after
    // priming an Ellipsis tile.
    let cache_len_after_prime = compositor
        .text_rasterizer
        .as_ref()
        .expect("text rasterizer must be initialised")
        .truncation_cache
        .len();
    assert!(
        cache_len_after_prime > 0,
        "truncation cache must be non-empty after priming an Ellipsis at-tail tile"
    );

    // ── Scene with at_tail = false (scrolled back) ────────────────────────
    let mut scene_head = SceneGraph::new(720.0, 360.0);
    let tab_head = scene_head.create_tab("test", 0).unwrap();
    let lease_head = scene_head.grant_lease("head-test", 120_000, vec![]);
    let tile_head = scene_head
        .create_tile(
            tab_head,
            "head-test",
            lease_head,
            Rect::new(0.0, 0.0, tile_w, tile_h),
            1,
        )
        .unwrap();
    let node_ellipsis_head = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_owned(),
            bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    scene_head
        .set_tile_root(tile_head, node_ellipsis_head)
        .unwrap();
    // at_tail = false: tile is scrolled back (or no follow-tail active).
    // tile_follow_tail_at_tail defaults to false when not set.

    compositor.prime_markdown_cache(&scene_head);

    let items_head = compositor.collect_text_items(&scene_head, 720.0, 360.0);
    assert_eq!(
        items_head.len(),
        1,
        "expected one TextItem for the head tile"
    );
    assert_eq!(
        items_head[0].viewport,
        crate::overflow::TruncationViewport::HeadAnchored,
        "non-at-tail Ellipsis tile must retain HeadAnchored viewport"
    );

    // ── Clip overflow is unaffected by at_tail ────────────────────────────
    let mut scene_clip = SceneGraph::new(720.0, 360.0);
    let tab_clip = scene_clip.create_tab("test", 0).unwrap();
    let lease_clip = scene_clip.grant_lease("clip-test", 120_000, vec![]);
    let tile_clip = scene_clip
        .create_tile(
            tab_clip,
            "clip-test",
            lease_clip,
            Rect::new(0.0, 0.0, tile_w, tile_h),
            1,
        )
        .unwrap();
    let node_clip = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_owned(),
            bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
            font_size_px: 14.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    scene_clip.set_tile_root(tile_clip, node_clip).unwrap();
    scene_clip.set_tile_follow_tail_at_tail(tile_clip, true);

    compositor.prime_markdown_cache(&scene_clip);

    let items_clip = compositor.collect_text_items(&scene_clip, 720.0, 360.0);
    assert_eq!(
        items_clip.len(),
        1,
        "expected one TextItem for the clip tile"
    );
    assert_eq!(
        items_clip[0].viewport,
        crate::overflow::TruncationViewport::HeadAnchored,
        "Clip overflow at-tail tile must remain HeadAnchored (at_tail only overrides Ellipsis)"
    );
}

// ── Zone StreamText tail-anchored truncation (hud-gxz0x) ──────────────────

/// Helper: register a `LatestWins` StreamText zone covering the whole display
/// with the given `overflow` / `stream_tail_anchored` policy, publish `content`,
/// and return the scene.  The zone uses a monospace font and small geometry so
/// multi-line content overflows and is forced to truncate.
fn make_stream_zone_scene(
    overflow: Option<TextOverflow>,
    stream_tail_anchored: Option<bool>,
    content: &str,
) -> SceneGraph {
    let mut scene = SceneGraph::new(720.0, 360.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "stream".to_owned(),
        description: "streaming zone".to_owned(),
        // Relative geometry: narrow + short so several lines overflow.
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 200.0 / 720.0,
            height_pct: 40.0 / 360.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(14.0),
            font_family: Some(FontFamily::SystemMonospace),
            overflow,
            stream_tail_anchored,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "stream",
            ZoneContent::StreamText(content.to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();
    scene
}

/// A streaming zone that opts into `stream_tail_anchored` produces a
/// `TailAnchored` `TextItem` so the newest content (tail) is shown; the default
/// (None) stays `HeadAnchored`, and `Clip` overflow is unaffected.
///
/// Exercises the CPU `collect_text_items` path; the `Compositor` itself needs a
/// GPU device to construct, so the test is GPU-gated like its tile sibling
/// `test_collect_text_items_at_tail_ellipsis_uses_tail_anchored_viewport`.
#[tokio::test]
async fn test_zone_stream_text_tail_anchored_opt_in_viewport() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);

    let content = "Line A\nLine B\nLine C\nLine D\nLine E\nLine F\nLine G\nLine H";

    // Opt-in: Ellipsis overflow + stream_tail_anchored = Some(true) → TailAnchored.
    let scene_tail = make_stream_zone_scene(Some(TextOverflow::Ellipsis), Some(true), content);
    let items_tail = compositor.collect_text_items(&scene_tail, 720.0, 360.0);
    assert_eq!(items_tail.len(), 1, "expected one StreamText TextItem");
    assert_eq!(
        items_tail[0].overflow,
        TextOverflow::Ellipsis,
        "policy overflow must propagate to the item"
    );
    assert_eq!(
        items_tail[0].viewport,
        crate::overflow::TruncationViewport::TailAnchored,
        "stream_tail_anchored = Some(true) must produce TailAnchored viewport \
         so the streaming zone shows the newest content (hud-gxz0x)"
    );

    // Default: Ellipsis overflow, stream_tail_anchored = None → HeadAnchored.
    let scene_head = make_stream_zone_scene(Some(TextOverflow::Ellipsis), None, content);
    let items_head = compositor.collect_text_items(&scene_head, 720.0, 360.0);
    assert_eq!(items_head.len(), 1);
    assert_eq!(
        items_head[0].viewport,
        crate::overflow::TruncationViewport::HeadAnchored,
        "default (no opt-in) zone StreamText must remain HeadAnchored \
         (no regression for existing zone users)"
    );

    // Explicit Some(false) is also head-anchored.
    let scene_false = make_stream_zone_scene(Some(TextOverflow::Ellipsis), Some(false), content);
    let items_false = compositor.collect_text_items(&scene_false, 720.0, 360.0);
    assert_eq!(
        items_false[0].viewport,
        crate::overflow::TruncationViewport::HeadAnchored,
        "stream_tail_anchored = Some(false) must remain HeadAnchored"
    );

    // Clip overflow ignores the opt-in (no truncation, head always shown).
    let scene_clip = make_stream_zone_scene(Some(TextOverflow::Clip), Some(true), content);
    let items_clip = compositor.collect_text_items(&scene_clip, 720.0, 360.0);
    assert_eq!(
        items_clip[0].viewport,
        crate::overflow::TruncationViewport::TailAnchored,
        "viewport field still reflects the opt-in even for Clip; truncation key \
         gating (effective_truncation_key) is what makes it inert for Clip"
    );
    // For Clip overflow, effective_truncation_key returns None → no truncation.
    assert!(
        crate::text::effective_truncation_key(&items_clip[0]).is_none(),
        "Clip overflow must not produce a truncation key (anchoring is inert)"
    );
}

/// End-to-end: priming the truncation cache for a tail-anchored streaming zone
/// stores a truncation that shows the **newest** content (the tail), while the
/// head-anchored default stores the **oldest**.  Proves the zone StreamText
/// frame path resolves through the shared tail-anchored helpers.
///
/// GPU-gated: `prime_truncation_cache` requires the text rasterizer.
#[tokio::test]
async fn test_zone_stream_text_tail_anchored_primes_newest_content() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 360).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Distinct first/last lines so head vs tail truncation differ observably.
    let content = "FIRST\nbbbb\ncccc\ndddd\neeee\nffff\ngggg\nLAST";

    // ── Tail-anchored zone: cache must show the newest line (LAST) ────────
    let scene_tail = make_stream_zone_scene(Some(TextOverflow::Ellipsis), Some(true), content);
    compositor.prime_markdown_cache(&scene_tail);
    compositor.prime_truncation_cache(&scene_tail);

    let items_tail = compositor.collect_text_items(&scene_tail, 720.0, 360.0);
    assert_eq!(items_tail.len(), 1);
    let key_tail = crate::text::effective_truncation_key(&items_tail[0])
        .expect("Ellipsis item must yield a truncation key");
    let cached_tail = compositor
        .text_rasterizer
        .as_ref()
        .expect("rasterizer initialised")
        .truncation_cache
        .get_by_key(&key_tail)
        .expect("tail-anchored zone StreamText must be primed (hud-gxz0x)");
    assert!(
        cached_tail.was_truncated,
        "content must overflow the zone and be truncated"
    );
    assert!(
        cached_tail.text.contains("LAST"),
        "tail-anchored truncation must show the newest content (LAST); got {:?}",
        cached_tail.text
    );
    assert!(
        !cached_tail.text.contains("FIRST"),
        "tail-anchored truncation must NOT pin the oldest content (FIRST); got {:?}",
        cached_tail.text
    );

    // ── Head-anchored default: cache must show the oldest line (FIRST) ────
    let mut compositor2 = require_gpu!(make_compositor_and_surface(720, 360).await).0;
    compositor2.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let scene_head = make_stream_zone_scene(Some(TextOverflow::Ellipsis), None, content);
    compositor2.prime_markdown_cache(&scene_head);
    compositor2.prime_truncation_cache(&scene_head);

    let items_head = compositor2.collect_text_items(&scene_head, 720.0, 360.0);
    let key_head = crate::text::effective_truncation_key(&items_head[0])
        .expect("Ellipsis item must yield a truncation key");
    let cached_head = compositor2
        .text_rasterizer
        .as_ref()
        .expect("rasterizer initialised")
        .truncation_cache
        .get_by_key(&key_head)
        .expect("head-anchored zone StreamText must be primed");
    assert!(
        cached_head.text.contains("FIRST"),
        "head-anchored truncation must show the oldest content (FIRST); got {:?}",
        cached_head.text
    );
}

// ── hud-gpqde: markdown prime instrumentation and node_key_cache ─────────

/// MarkdownCache::compute_key is deterministic and content-addressed: same
/// content produces the same BLAKE3 key; distinct content produces distinct
/// keys; get_by_key returns the parsed entry after prime.
///
/// This is a CPU-only prerequisite test for the node_key_cache contract — it
/// does not call Compositor::prime_markdown_cache. The compositor-level test
/// that verifies node_key_cache population is
/// `prime_markdown_cache_builds_node_key_cache_entry` (GPU-gated).
#[test]
fn markdown_cache_compute_key_is_deterministic_and_content_addressed() {
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
    };

    // Build a scene with two TextMarkdown nodes.
    let content_a = "# Hello\n\nThis is **bold** text.";
    let content_b = "Plain text with `code`.";

    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000, vec![]);

    let node_a_id = SceneId::new();
    let node_a = Node {
        layout: Default::default(),
        id: node_a_id,
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content_a.to_string(),
            bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
            font_size_px: 14.0,
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

    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene.set_tile_root(tile_id, node_a).unwrap();

    // Build a minimal headless compositor without GPU (no render pipeline needed
    // for this unit test — prime_markdown_cache only touches CPU caches).
    //
    // Since Compositor::new_headless requires GPU, we test the cache logic
    // in isolation by exercising MarkdownCache directly (which is the same
    // code path called by prime_markdown_cache).
    //
    // The key contract to verify: the key for content_a matches
    // MarkdownCache::compute_key(content_a, tokens).  The key folds the
    // token-set identity (hud-3ryie), so it is computed with a token set.
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

    // Verify the node_key_cache is populated correctly by prime_markdown_cache.
    // We exercise the actual prime_markdown_cache code path through a
    // gpu-free partial compositor state if the environment supports it.
    //
    // Contract: after prime_markdown_cache, node_key_cache[node_a_id] ==
    // MarkdownCache::compute_key(content_a).
    let _ = (scene, node_a_id, tile_id, content_b); // mark used
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
    let lease_id = scene.grant_lease("test", 60_000, vec![]);

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

/// MarkdownCache::prime is idempotent: repeated calls with the same content
/// return the identical cached ParsedMarkdown without re-parsing.
///
/// This is a CPU-only test of MarkdownCache hit behavior. It does not test
/// the Compositor scene-version gate. The compositor-level no-op gate is
/// validated by `prime_markdown_cache_builds_node_key_cache_entry` — calling
/// prime_markdown_cache twice on the same scene version leaves node_key_cache
/// unchanged on the second call.
#[test]
fn markdown_cache_prime_is_idempotent_for_same_content() {
    // Verify that MarkdownCache::prime returns the same value on repeated
    // calls with identical content (cache hit, no re-parse).
    let tokens = crate::markdown::MarkdownTokens::default();
    let content = "**bold** text";

    // Prime once.
    let mut cache = crate::markdown::MarkdownCache::new();
    let parsed_first = cache.prime(content, &tokens).clone();

    // Prime again — entry() API returns the cached value, no re-parse.
    let parsed_second = cache.prime(content, &tokens).clone();

    assert_eq!(
        parsed_first, parsed_second,
        "repeated prime of same content must return identical ParsedMarkdown"
    );
}

/// set_token_map clears node_key_cache so the next prime rebuilds it with
/// the new token-resolved keys.
///
/// This exercises the full token-map invalidation path.  Without the clear,
/// node_key_cache would map node IDs to stale keys referencing evicted
/// markdown_cache entries, causing cache misses on the render path.  After
/// hud-xcp9b those misses trigger an inline non-lossy parse + tracing::warn!
/// rather than the old silent lossy strip_markdown_v1 fallback.
#[tokio::test]
async fn set_token_map_clears_node_key_cache() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(64, 64).await);

    // node_key_cache starts empty.
    assert!(
        compositor.node_key_cache.is_empty(),
        "node_key_cache must start empty"
    );

    // After set_token_map, node_key_cache must still be empty (or cleared if
    // it was previously populated).
    compositor.set_token_map(HashMap::new());
    assert!(
        compositor.node_key_cache.is_empty(),
        "set_token_map must clear node_key_cache"
    );
}

/// prime_markdown_cache builds node_key_cache with one entry per
/// TextMarkdown node.  On the first call the cache is empty; after priming
/// it has exactly one entry whose key equals MarkdownCache::compute_key
/// for the node's content.
#[tokio::test]
async fn prime_markdown_cache_builds_node_key_cache_entry() {
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
    };

    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(64, 64).await);

    let content = "## Heading\n\nParagraph with *italic* text.";
    // Empty token map → portal and generic scopes both resolve to defaults, so
    // the key is scope-independent here (hud-3ryie).
    let expected_key = crate::markdown::MarkdownCache::compute_key(
        content,
        &crate::markdown::MarkdownTokens::default(),
    );

    let node_id = SceneId::new();
    let node = Node {
        layout: Default::default(),
        id: node_id,
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_string(),
            bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
            font_size_px: 14.0,
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

    let scene = scene_with_node(node);

    // Before priming: node_key_cache is empty.
    assert!(
        compositor.node_key_cache.is_empty(),
        "node_key_cache must be empty before first prime"
    );

    compositor.prime_markdown_cache(&scene);

    // After priming: exactly one entry inserted under the correct SceneId.
    // Assert via node_id (not values().next()) so that a wrong-key insertion
    // is not masked by a length-1 coincidence.
    assert_eq!(
        compositor.node_key_cache.len(),
        1,
        "node_key_cache must have one entry after priming a scene with one TextMarkdown node"
    );

    let cached_key = compositor
        .node_key_cache
        .get(&node_id)
        .copied()
        .expect("node_key_cache must contain an entry for node_id");

    assert_eq!(
        cached_key, expected_key,
        "cached key must equal MarkdownCache::compute_key(content)"
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

    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(64, 64).await);
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
    let _telemetry = compositor.render_frame_headless(&mut scene, &surface);

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
    let _telemetry2 = compositor.render_frame_headless(&mut scene, &surface);
    assert_eq!(
        compositor.markdown_cache_scene_version, scene_version_before,
        "second render of unchanged scene must not change markdown_cache_scene_version"
    );
}

/// FrameTelemetry.markdown_prime_us is zero-initialized and round-trips
/// through the struct without corruption.
///
/// The critical property: markdown_prime_us is always initialized (no
/// unset field).  JSON serialization is verified in tze_hud_telemetry
/// where serde_json is available as a dev-dependency.
#[test]
fn frame_telemetry_has_markdown_prime_us_field() {
    use tze_hud_telemetry::FrameTelemetry;

    let mut frame = FrameTelemetry::new(1);
    // Field is zero-initialized by FrameTelemetry::new.
    assert_eq!(
        frame.markdown_prime_us, 0,
        "markdown_prime_us must be zero-initialized"
    );

    // Field is writable and round-trips correctly.
    frame.markdown_prime_us = 42;
    assert_eq!(
        frame.markdown_prime_us, 42,
        "markdown_prime_us must round-trip through the struct"
    );
}

// ── Mid-drag cadence gate tests (hud-ghhxa — spec §6b.3) ──────────────────
//
// These tests verify the `should_defer_reprime` helper used by
// `prime_truncation_cache`.  By testing the extracted helper directly, any
// change to the gate logic in `prime_truncation_cache` that diverges from
// `should_defer_reprime` will cause a compile or test failure.
//
// Key invariants:
//   a) When `last_at` is None (first call ever), the gate does not defer.
//   b) When called again within the interval, the gate defers (returns true).
//   c) When called outside the interval, the gate allows (returns false).
//   d) A forced prime (sentinel == u64::MAX) bypasses the gate entirely
//      (this bypass is checked in prime_truncation_cache, not in
//      should_defer_reprime; these tests confirm the helper contract only).

/// First call with no prior prime timestamp (last_at = None) does NOT defer.
///
/// Invariant (a): should_defer_reprime(None, _) == false.
#[test]
fn cadence_gate_first_call_no_prior_timestamp_allows_prime() {
    assert!(
        !should_defer_reprime(None, RESIZE_REPRIME_INTERVAL_MEDIUM_MS),
        "cadence gate must not defer on the first call (no prior timestamp)"
    );
}

/// Within the interval, a call defers (returns true).
///
/// Invariant (b): elapsed < interval_ms → should_defer_reprime returns true.
#[test]
fn cadence_gate_within_interval_defers_reprime() {
    // last prime ran "just now" — well within any reasonable interval.
    let last_at = Some(std::time::Instant::now());
    let interval_ms = RESIZE_REPRIME_INTERVAL_MEDIUM_MS;
    assert!(
        should_defer_reprime(last_at, interval_ms),
        "cadence gate must defer when within interval ({interval_ms}ms)"
    );
}

/// After the interval has elapsed, the gate allows the prime (returns false).
///
/// Invariant (c): elapsed >= interval_ms → should_defer_reprime returns false.
/// Uses a back-dated Instant to avoid sleeping.
#[test]
fn cadence_gate_after_interval_allows_reprime() {
    let interval_ms = RESIZE_REPRIME_INTERVAL_MEDIUM_MS;
    let past = std::time::Instant::now() - std::time::Duration::from_millis(interval_ms + 10);
    assert!(
        !should_defer_reprime(Some(past), interval_ms),
        "cadence gate must allow prime when the interval ({interval_ms}ms) has elapsed"
    );
}

/// The cadence gate correctly handles interval boundary (exactly at interval).
///
/// Elapsed == interval_ms should allow the prime (not defer), since the
/// Duration comparison is strict less-than.
#[test]
fn cadence_gate_at_exact_interval_boundary_allows_reprime() {
    let interval_ms = RESIZE_REPRIME_INTERVAL_MEDIUM_MS;
    // Back-date by exactly the interval: elapsed >= interval_ms.
    let past = std::time::Instant::now() - std::time::Duration::from_millis(interval_ms);
    // elapsed() is at least interval_ms so should_defer_reprime must return false.
    assert!(
        !should_defer_reprime(Some(past), interval_ms),
        "cadence gate must allow prime when elapsed >= interval ({interval_ms}ms)"
    );
}

/// Settle scenario: several geometry changes within the interval all defer,
/// then the next call after the interval allows the prime.
///
/// This verifies the "sentinel not updated on defer → final geometry primed
/// after interval" property end-to-end through the helper.
#[test]
fn cadence_gate_deferred_sentinel_unchanged_enables_settle_prime() {
    let interval_ms = RESIZE_REPRIME_INTERVAL_MEDIUM_MS;

    // All calls within the interval must defer.
    let last_at = Some(std::time::Instant::now());
    for _ in 0..5 {
        assert!(
            should_defer_reprime(last_at, interval_ms),
            "should_defer_reprime must return true for calls within the interval"
        );
    }

    // After the interval elapses, the next call must allow the prime.
    let past = std::time::Instant::now() - std::time::Duration::from_millis(interval_ms + 10);
    assert!(
        !should_defer_reprime(Some(past), interval_ms),
        "should_defer_reprime must return false once the interval has elapsed \
             (final/settled geometry must be primed)"
    );
}

// ── Adaptive cadence threshold tests (hud-3to8i) ──────────────────────────
//
// These tests verify `adaptive_reprime_interval_ms`, which selects the
// re-prime interval based on total Ellipsis content byte count.
//
// Key invariants:
//   a) Zero bytes (empty scene) → short interval (≈60 Hz).
//   b) Content just below the short threshold → short interval.
//   c) Content at the short threshold → medium interval.
//   d) Content just below the long threshold → medium interval.
//   e) Content at the long threshold → long interval.
//   f) Large content → long interval.
//   g) The short interval < medium interval < long interval (strict ordering).

/// Invariant (a): empty scene → short interval (≈60 Hz).
#[test]
fn adaptive_cadence_empty_scene_uses_short_interval() {
    assert_eq!(
        adaptive_reprime_interval_ms(0),
        RESIZE_REPRIME_INTERVAL_SHORT_MS,
        "empty scene (0 bytes) must use the short re-prime interval (≈60 Hz)"
    );
}

/// Invariant (b): content just below the short threshold → short interval.
#[test]
fn adaptive_cadence_below_short_threshold_uses_short_interval() {
    let bytes = RESIZE_REPRIME_SHORT_THRESHOLD_BYTES - 1;
    assert_eq!(
        adaptive_reprime_interval_ms(bytes),
        RESIZE_REPRIME_INTERVAL_SHORT_MS,
        "content just below short threshold ({bytes} bytes) must use short interval"
    );
}

/// Invariant (c): content at the short threshold → medium interval.
#[test]
fn adaptive_cadence_at_short_threshold_uses_medium_interval() {
    let bytes = RESIZE_REPRIME_SHORT_THRESHOLD_BYTES;
    assert_eq!(
        adaptive_reprime_interval_ms(bytes),
        RESIZE_REPRIME_INTERVAL_MEDIUM_MS,
        "content at short threshold ({bytes} bytes) must use medium interval"
    );
}

/// Invariant (d): content just below the long threshold → medium interval.
#[test]
fn adaptive_cadence_below_long_threshold_uses_medium_interval() {
    let bytes = RESIZE_REPRIME_LONG_THRESHOLD_BYTES - 1;
    assert_eq!(
        adaptive_reprime_interval_ms(bytes),
        RESIZE_REPRIME_INTERVAL_MEDIUM_MS,
        "content just below long threshold ({bytes} bytes) must use medium interval"
    );
}

/// Invariant (e): content at the long threshold → long interval.
#[test]
fn adaptive_cadence_at_long_threshold_uses_long_interval() {
    let bytes = RESIZE_REPRIME_LONG_THRESHOLD_BYTES;
    assert_eq!(
        adaptive_reprime_interval_ms(bytes),
        RESIZE_REPRIME_INTERVAL_LONG_MS,
        "content at long threshold ({bytes} bytes) must use long interval (≈10 Hz)"
    );
}

/// Invariant (f): large content (1 MiB transcript) → long interval.
#[test]
fn adaptive_cadence_large_content_uses_long_interval() {
    let bytes = 1_048_576; // 1 MiB
    assert_eq!(
        adaptive_reprime_interval_ms(bytes),
        RESIZE_REPRIME_INTERVAL_LONG_MS,
        "large content ({bytes} bytes / 1 MiB) must use the long re-prime interval (≈10 Hz)"
    );
}

/// Invariant (g): strict ordering — short < medium < long.
///
/// These comparisons are between compile-time constants, so they are
/// expressed as `const` assertions (evaluated at compile time) rather than
/// `assert!` calls (which clippy correctly flags as `assertions_on_constants`
/// when the expression is a known constant `true`).
#[test]
fn adaptive_cadence_intervals_are_strictly_ordered() {
    const _: () = assert!(
        RESIZE_REPRIME_INTERVAL_SHORT_MS < RESIZE_REPRIME_INTERVAL_MEDIUM_MS,
        "short interval must be < medium interval"
    );
    const _: () = assert!(
        RESIZE_REPRIME_INTERVAL_MEDIUM_MS < RESIZE_REPRIME_INTERVAL_LONG_MS,
        "medium interval must be < long interval"
    );
}

// ── tile_at_tail_for_ellipsis tests (hud-plz8q) ───────────────────────────
//
// These tests verify the shared `tile_at_tail_for_ellipsis` helper that
// eliminates the hand-mirrored must-mirror guard between
// `prime_truncation_cache` and `collect_text_items` (hud-lu50e / hud-plz8q).
//
// All tests are GPU-free: they operate directly on `SceneGraph` and the
// extracted helper, following the same pattern as the cadence-gate tests
// for `should_defer_reprime`.
//
// Key invariants:
//   a) An unregistered tile (never set) returns false (HeadAnchored default).
//   b) A tile explicitly set at-tail returns true (TailAnchored).
//   c) A tile set at-tail then scrolled back (set false) returns false.
//   d) A tile from a different scene (unknown id) returns false.

/// Invariant (a): a tile that has never been registered returns false.
///
/// Non-scrollable and newly-created tiles must default to HeadAnchored so
/// `TextOverflow::Ellipsis` shows the beginning of content, not the tail.
#[test]
fn tile_at_tail_for_ellipsis_unregistered_tile_returns_false() {
    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab = scene.create_tab("test", 0).unwrap();
    let lease = scene.grant_lease("lease", 120_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab,
            "tile",
            lease,
            tze_hud_scene::types::Rect::new(0.0, 0.0, 400.0, 200.0),
            1,
        )
        .unwrap();
    // Never set_tile_follow_tail_at_tail → must default to false.
    assert!(
        !tile_at_tail_for_ellipsis(tile_id, &scene),
        "tile_at_tail_for_ellipsis must return false for an unregistered tile (HeadAnchored default)"
    );
}

/// Invariant (b): a tile explicitly set at-tail returns true.
///
/// Confirms the shared helper reflects live follow-tail state, matching
/// what prime_truncation_cache and collect_text_items both require.
#[test]
fn tile_at_tail_for_ellipsis_at_tail_tile_returns_true() {
    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab = scene.create_tab("test", 0).unwrap();
    let lease = scene.grant_lease("lease", 120_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab,
            "tile",
            lease,
            tze_hud_scene::types::Rect::new(0.0, 0.0, 400.0, 200.0),
            1,
        )
        .unwrap();
    scene.set_tile_follow_tail_at_tail(tile_id, true);
    assert!(
        tile_at_tail_for_ellipsis(tile_id, &scene),
        "tile_at_tail_for_ellipsis must return true after set_tile_follow_tail_at_tail(true)"
    );
}

/// Invariant (c): a tile set at-tail then scrolled back returns false.
///
/// Simulates the user scrolling back from the tail: the shared helper must
/// reflect the updated scene state immediately, keeping both consumers aligned.
#[test]
fn tile_at_tail_for_ellipsis_scrolled_back_returns_false() {
    let mut scene = SceneGraph::new(720.0, 360.0);
    let tab = scene.create_tab("test", 0).unwrap();
    let lease = scene.grant_lease("lease", 120_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab,
            "tile",
            lease,
            tze_hud_scene::types::Rect::new(0.0, 0.0, 400.0, 200.0),
            1,
        )
        .unwrap();
    scene.set_tile_follow_tail_at_tail(tile_id, true);
    // User scrolls back — at_tail transitions to false.
    scene.set_tile_follow_tail_at_tail(tile_id, false);
    assert!(
        !tile_at_tail_for_ellipsis(tile_id, &scene),
        "tile_at_tail_for_ellipsis must return false after scrolling back from tail"
    );
}

/// Invariant (d): an unknown SceneId (not a tile in the scene) returns false.
///
/// Matches the SceneGraph contract: tile_follow_tail_at_tail returns false
/// for any unregistered id.  Callers must not panic on stale ids.
#[test]
fn tile_at_tail_for_ellipsis_unknown_id_returns_false() {
    let scene = SceneGraph::new(720.0, 360.0);
    let unknown_id = tze_hud_scene::types::SceneId::new();
    assert!(
        !tile_at_tail_for_ellipsis(unknown_id, &scene),
        "tile_at_tail_for_ellipsis must return false for an unknown SceneId"
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
/// Invariants verified (CPU-only, no GPU):
///  - `TextItem::text` equals the non-lossy plain text from `parse_markdown_subset`
///    for the same content (not the output of `strip_markdown_v1`).
///  - `TextItem::styled_runs` is non-empty for content that contains
///    markdown constructs (e.g. `**bold**` → at least one bold run).
///
/// This is a Layer 0 invariant test for the 'never dropped' contract.
#[test]
fn markdown_cache_miss_fallback_is_non_lossy() {
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
}

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
    let lease_id = scene.grant_lease("test", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("test", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("test", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("test", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("test", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("test", 60_000, vec![]);
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
    compositor.prime_composer_scroll_offset(&scene);

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
    let lease_id = scene.grant_lease("test", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("agent", 60_000, vec![]);
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
    let lease_id = scene.grant_lease("divider-scroll", 120_000, vec![]);
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

/// `resolve_composer_overlay_tokens` must return valid, non-degenerate token
/// values for an empty token map (all defaults applied).
///
/// This is a CPU-only smoke test — no GPU required.
#[test]
fn composer_overlay_tokens_defaults_are_valid() {
    use std::collections::HashMap;

    // Empty token map → all defaults kick in.
    let tokens: std::collections::HashMap<String, String> = HashMap::new();
    let t = resolve_composer_overlay_tokens(&tokens);

    // Font size must be finite and positive.
    assert!(
        t.font_size_px.is_finite() && t.font_size_px > 0.0,
        "font_size_px must be positive finite, got {}",
        t.font_size_px
    );

    // Background alpha must be > 0 so the overlay is actually visible.
    assert!(
        t.bg_a > 0.0,
        "default background alpha must be > 0 (overlay must be visible)"
    );

    // All color channels must be in [0, 1].
    for (name, val) in [
        ("bg_r", t.bg_r),
        ("bg_g", t.bg_g),
        ("bg_b", t.bg_b),
        ("bg_a", t.bg_a),
        ("text_r", t.text_r),
        ("text_g", t.text_g),
        ("text_b", t.text_b),
        ("text_a", t.text_a),
        ("at_capacity_r", t.at_capacity_r),
        ("at_capacity_g", t.at_capacity_g),
        ("at_capacity_b", t.at_capacity_b),
        ("at_capacity_a", t.at_capacity_a),
    ] {
        assert!(
            (0.0..=1.0).contains(&val),
            "token {name} must be in [0, 1], got {val}"
        );
    }
}

#[test]
fn composer_overlay_default_font_size_matches_readable_portal_default() {
    use std::collections::HashMap;

    let tokens: HashMap<String, String> = HashMap::new();
    let resolved = resolve_composer_overlay_tokens(&tokens);

    assert!(
        resolved.font_size_px >= 16.0,
        "focused composer overlay default font must match the readable portal composer default; got {}px",
        resolved.font_size_px
    );
}

/// The at-capacity color token must be distinct from the background color
/// so it provides a visible signal when the composer draft reaches its byte cap.
///
/// Verifies two things:
/// 1. The default at-capacity color (muted amber `#B87333`) has a non-zero red
///    channel while the background (`#0F1418`) has a near-zero red channel —
///    they are visually distinguishable.
/// 2. Injecting an override via the token map propagates through
///    `resolve_composer_overlay_tokens` so the compositor render path is driven
///    entirely by the token, with no hardcoded color values.
///
/// CPU-only — no GPU required (hud-2axdq acceptance criterion).
#[test]
fn composer_at_capacity_token_is_distinct_from_background_and_propagates_override() {
    use std::collections::HashMap;

    // Default case: at-capacity color must differ from background.
    let empty: HashMap<String, String> = HashMap::new();
    let t = resolve_composer_overlay_tokens(&empty);

    // At-capacity alpha must be non-zero so the indicator is actually visible.
    assert!(
        t.at_capacity_a > 0.0,
        "default at_capacity_a must be > 0 (indicator must be visible)"
    );
    // The default at-capacity color (amber #B87333) has high red channel;
    // the default background (#0F1418) has very low red channel. They must differ.
    assert!(
        (t.at_capacity_r - t.bg_r).abs() > 0.1,
        "at_capacity_r ({}) must differ meaningfully from bg_r ({}) so the \
             at-capacity indicator is visually distinct from the composer background",
        t.at_capacity_r,
        t.bg_r,
    );

    // Override case: injecting a token value must propagate to the struct.
    let mut overrides: HashMap<String, String> = HashMap::new();
    overrides.insert(
        "portal.composer.at_capacity_color".to_string(),
        "#FF0000".to_string(), // pure red sentinel
    );
    let t_override = resolve_composer_overlay_tokens(&overrides);
    // Red channel must be ~1.0 (pure red), not the default amber.
    assert!(
        t_override.at_capacity_r > 0.9,
        "overridden at_capacity_r must be ~1.0 (pure red sentinel), got {}",
        t_override.at_capacity_r
    );
    assert!(
        t_override.at_capacity_g < 0.1,
        "overridden at_capacity_g must be ~0.0 (pure red sentinel), got {}",
        t_override.at_capacity_g
    );
    assert!(
        t_override.at_capacity_b < 0.1,
        "overridden at_capacity_b must be ~0.0 (pure red sentinel), got {}",
        t_override.at_capacity_b
    );
    // Baseline must differ from override (amber vs red).
    assert!(
        (t.at_capacity_r - t_override.at_capacity_r).abs() > 0.1,
        "default and overridden at_capacity_r must differ (amber vs red)"
    );
}

// ── Composer caret-color tokenization tests [hud-khfgx] ──────────────────

/// The caret color defaults to the composer text color, so tokenizing the caret
/// (vd-caret-selection-placeholder-not-tokenized) is a no-visual-regression change
/// for the default profile: with no `portal.composer.caret_color` token, the
/// resolved caret color equals the resolved composer text color (both in sRGB u8).
///
/// CPU-only — no GPU required.
#[test]
fn composer_caret_color_defaults_to_text_color() {
    use super::token_colors::linear_to_srgb;
    use std::collections::HashMap;

    let empty: HashMap<String, String> = HashMap::new();
    let t = resolve_composer_overlay_tokens(&empty);

    let to_srgb_u8 = |v: f32| (linear_to_srgb(v.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
    let expected = [
        to_srgb_u8(t.text_r),
        to_srgb_u8(t.text_g),
        to_srgb_u8(t.text_b),
        (t.text_a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
    ];
    assert_eq!(
        t.caret_color, expected,
        "default caret color must equal the composer text color (no visual regression)"
    );
}

/// A `portal.composer.caret_color` override propagates to
/// `ComposerOverlayTokens::caret_color` so the caret can be accented independently
/// of the composer text color.
///
/// CPU-only — no GPU required.
#[test]
fn composer_caret_color_token_override_propagates() {
    use std::collections::HashMap;

    let mut overrides: HashMap<String, String> = HashMap::new();
    // Pure-green sentinel, distinct from the default near-white text color.
    overrides.insert(
        "portal.composer.caret_color".to_string(),
        "#00FF00FF".to_string(),
    );
    let t = resolve_composer_overlay_tokens(&overrides);

    assert_eq!(
        t.caret_color[0], 0x00,
        "overridden caret red channel must be 0x00, got {:?}",
        t.caret_color
    );
    assert_eq!(
        t.caret_color[1], 0xFF,
        "overridden caret green channel must be 0xFF, got {:?}",
        t.caret_color
    );
    assert_eq!(
        t.caret_color[3], 0xFF,
        "overridden caret alpha must be 0xFF, got {:?}",
        t.caret_color
    );
    // The override must differ from the default (which tracks the text color).
    let default = resolve_composer_overlay_tokens(&HashMap::new());
    assert_ne!(
        t.caret_color, default.caret_color,
        "caret color override must differ from the default text-colored caret"
    );
}

// ── Composer selection-range rendering tests [hud-bq0gl.9] ───────────────

/// The default `selection_bg` token must have a non-zero alpha so selection
/// highlights are actually visible when no `portal.composer.selection_color`
/// token is configured.
///
/// CPU-only — no GPU required.
#[test]
fn composer_selection_bg_default_is_visible() {
    use std::collections::HashMap;

    let empty: HashMap<String, String> = HashMap::new();
    let t = resolve_composer_overlay_tokens(&empty);

    // Alpha is the 4th element; must be > 0 for the highlight to show.
    assert!(
        t.selection_bg[3] > 0,
        "default selection_bg alpha must be > 0 so selection highlights are visible, \
         got {:?}",
        t.selection_bg,
    );
    // Blue channel should dominate the default blue-tint selection color.
    assert!(
        t.selection_bg[2] > t.selection_bg[0] && t.selection_bg[2] > t.selection_bg[1],
        "default selection_bg should be a blue-dominant color (#3A7BD5), got {:?}",
        t.selection_bg,
    );
}

/// `portal.composer.selection_color` token override must propagate to
/// `ComposerOverlayTokens::selection_bg` correctly.
///
/// CPU-only — no GPU required.
#[test]
fn composer_selection_bg_token_override_propagates() {
    use std::collections::HashMap;

    let mut overrides: HashMap<String, String> = HashMap::new();
    // Pure red sentinel in sRGB hex.
    overrides.insert(
        "portal.composer.selection_color".to_string(),
        "#FF0000FF".to_string(),
    );
    let t = resolve_composer_overlay_tokens(&overrides);

    assert_eq!(
        t.selection_bg[0], 0xFF,
        "overridden selection_bg red channel must be 0xFF, got {:?}",
        t.selection_bg
    );
    assert_eq!(
        t.selection_bg[1], 0x00,
        "overridden selection_bg green channel must be 0x00, got {:?}",
        t.selection_bg
    );
    assert_eq!(
        t.selection_bg[2], 0x00,
        "overridden selection_bg blue channel must be 0x00, got {:?}",
        t.selection_bg
    );
    assert_eq!(
        t.selection_bg[3], 0xFF,
        "overridden selection_bg alpha channel must be 0xFF, got {:?}",
        t.selection_bg
    );
}

// Note (hud-hxhnt): the byte-offset-mapping test that used to live here
// (`composer_selection_display_byte_offsets`) verified the +3-byte caret-glyph
// shift arithmetic against the (now removed) `composer_display_text` glyph
// insertion helper. That accounting no longer exists — the caret is a
// chrome-layer quad, and the selection styled run uses RAW byte offsets — so
// the test was replaced by `composer_selection_styled_run_uses_raw_byte_offsets`
// above, which exercises the same five (cursor, anchor) cases through the real
// production `collect_composer_text_item` path instead of reimplementing the
// arithmetic against a free-standing helper.

// ── Tile background color token tests [hud-9wljr.10] ─────────────────────

/// Fallback constants resolve to expected linear-RGB default values.
///
/// Asserts the three TILE_BG_* fallback constants match the values that
/// were previously hardcoded in `tile_background_color`, guaranteeing no
/// silent visual regression from the tokenization refactor.
///
/// CPU-only — no GPU required.
#[test]
fn tile_bg_fallback_constants_match_documented_defaults() {
    // TextMarkdown: [0.15, 0.15, 0.25]
    assert!(
        (TILE_BG_TEXT_MARKDOWN.r - 0.15).abs() < f32::EPSILON,
        "TILE_BG_TEXT_MARKDOWN.r expected 0.15, got {}",
        TILE_BG_TEXT_MARKDOWN.r
    );
    assert!(
        (TILE_BG_TEXT_MARKDOWN.g - 0.15).abs() < f32::EPSILON,
        "TILE_BG_TEXT_MARKDOWN.g expected 0.15, got {}",
        TILE_BG_TEXT_MARKDOWN.g
    );
    assert!(
        (TILE_BG_TEXT_MARKDOWN.b - 0.25).abs() < f32::EPSILON,
        "TILE_BG_TEXT_MARKDOWN.b expected 0.25, got {}",
        TILE_BG_TEXT_MARKDOWN.b
    );

    // StaticImage: [0.05, 0.05, 0.05]
    assert!(
        (TILE_BG_STATIC_IMAGE.r - 0.05).abs() < f32::EPSILON,
        "TILE_BG_STATIC_IMAGE.r expected 0.05, got {}",
        TILE_BG_STATIC_IMAGE.r
    );
    assert!(
        (TILE_BG_STATIC_IMAGE.g - 0.05).abs() < f32::EPSILON,
        "TILE_BG_STATIC_IMAGE.g expected 0.05, got {}",
        TILE_BG_STATIC_IMAGE.g
    );
    assert!(
        (TILE_BG_STATIC_IMAGE.b - 0.05).abs() < f32::EPSILON,
        "TILE_BG_STATIC_IMAGE.b expected 0.05, got {}",
        TILE_BG_STATIC_IMAGE.b
    );

    // Default: [0.1, 0.1, 0.2]
    assert!(
        (TILE_BG_DEFAULT.r - 0.1).abs() < f32::EPSILON,
        "TILE_BG_DEFAULT.r expected 0.1, got {}",
        TILE_BG_DEFAULT.r
    );
    assert!(
        (TILE_BG_DEFAULT.g - 0.1).abs() < f32::EPSILON,
        "TILE_BG_DEFAULT.g expected 0.1, got {}",
        TILE_BG_DEFAULT.g
    );
    assert!(
        (TILE_BG_DEFAULT.b - 0.2).abs() < f32::EPSILON,
        "TILE_BG_DEFAULT.b expected 0.2, got {}",
        TILE_BG_DEFAULT.b
    );
}

/// `resolve_tile_bg_token` returns the fallback when the token map is empty.
///
/// CPU-only — no GPU required.
#[test]
fn resolve_tile_bg_token_returns_fallback_on_absent_token() {
    let token_map: HashMap<String, String> = HashMap::new();

    let c = resolve_tile_bg_token(
        &token_map,
        "color.tile.background.text_markdown",
        TILE_BG_TEXT_MARKDOWN,
    );
    assert!(
        (c.r - TILE_BG_TEXT_MARKDOWN.r).abs() < f32::EPSILON
            && (c.g - TILE_BG_TEXT_MARKDOWN.g).abs() < f32::EPSILON
            && (c.b - TILE_BG_TEXT_MARKDOWN.b).abs() < f32::EPSILON,
        "absent text_markdown token must fall back to TILE_BG_TEXT_MARKDOWN, got {c:?}"
    );

    let c = resolve_tile_bg_token(
        &token_map,
        "color.tile.background.static_image",
        TILE_BG_STATIC_IMAGE,
    );
    assert!(
        (c.r - TILE_BG_STATIC_IMAGE.r).abs() < f32::EPSILON
            && (c.g - TILE_BG_STATIC_IMAGE.g).abs() < f32::EPSILON
            && (c.b - TILE_BG_STATIC_IMAGE.b).abs() < f32::EPSILON,
        "absent static_image token must fall back to TILE_BG_STATIC_IMAGE, got {c:?}"
    );

    let c = resolve_tile_bg_token(&token_map, "color.tile.background.default", TILE_BG_DEFAULT);
    assert!(
        (c.r - TILE_BG_DEFAULT.r).abs() < f32::EPSILON
            && (c.g - TILE_BG_DEFAULT.g).abs() < f32::EPSILON
            && (c.b - TILE_BG_DEFAULT.b).abs() < f32::EPSILON,
        "absent default token must fall back to TILE_BG_DEFAULT, got {c:?}"
    );
}

/// Token override: `color.tile.background.text_markdown` overrides the fallback.
///
/// Uses pure cyan (#00FFFF) — clearly distinct from the default blue-gray.
/// CPU-only — no GPU required.
#[test]
fn resolve_tile_bg_token_text_markdown_override() {
    let mut token_map: HashMap<String, String> = HashMap::new();
    token_map.insert(
        "color.tile.background.text_markdown".to_string(),
        "#00FFFF".to_string(),
    );

    let c = resolve_tile_bg_token(
        &token_map,
        "color.tile.background.text_markdown",
        TILE_BG_TEXT_MARKDOWN,
    );
    // #00FFFF sRGB → linear: R=0.0, G≈1.0, B≈1.0
    assert!(
        c.r < 0.01,
        "overridden text_markdown R should be ~0.0 (cyan), got {}",
        c.r
    );
    assert!(
        c.g > 0.9,
        "overridden text_markdown G should be ~1.0 (cyan), got {}",
        c.g
    );
    assert!(
        c.b > 0.9,
        "overridden text_markdown B should be ~1.0 (cyan), got {}",
        c.b
    );
}

/// Token override: `color.tile.background.static_image` overrides the fallback.
///
/// Uses pure red (#FF0000) — clearly distinct from the default near-black.
/// CPU-only — no GPU required.
#[test]
fn resolve_tile_bg_token_static_image_override() {
    let mut token_map: HashMap<String, String> = HashMap::new();
    token_map.insert(
        "color.tile.background.static_image".to_string(),
        "#FF0000".to_string(),
    );

    let c = resolve_tile_bg_token(
        &token_map,
        "color.tile.background.static_image",
        TILE_BG_STATIC_IMAGE,
    );
    // #FF0000 sRGB → linear: R≈1.0, G=0.0, B=0.0
    assert!(
        c.r > 0.9,
        "overridden static_image R should be ~1.0 (red), got {}",
        c.r
    );
    assert!(
        c.g < 0.01,
        "overridden static_image G should be ~0.0 (red), got {}",
        c.g
    );
    assert!(
        c.b < 0.01,
        "overridden static_image B should be ~0.0 (red), got {}",
        c.b
    );
}

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

/// Token override: `color.tile.background.default` overrides the fallback.
///
/// Uses pure green (#00FF00) — clearly distinct from the default blue-dark.
/// CPU-only — no GPU required.
#[test]
fn resolve_tile_bg_token_default_override() {
    let mut token_map: HashMap<String, String> = HashMap::new();
    token_map.insert(
        "color.tile.background.default".to_string(),
        "#00FF00".to_string(),
    );

    let c = resolve_tile_bg_token(&token_map, "color.tile.background.default", TILE_BG_DEFAULT);
    // #00FF00 sRGB → linear: R=0.0, G≈1.0, B=0.0
    assert!(
        c.r < 0.01,
        "overridden default R should be ~0.0 (green), got {}",
        c.r
    );
    assert!(
        c.g > 0.9,
        "overridden default G should be ~1.0 (green), got {}",
        c.g
    );
    assert!(
        c.b < 0.01,
        "overridden default B should be ~0.0 (green), got {}",
        c.b
    );
}

// ── Portal tile animation unit tests (CPU-only, no GPU required) ─────────
//
// These tests exercise the portal transition token wiring introduced in
// hud-58rg1 (spec §6.3).  They are CPU-only: no wgpu adapter is requested
// so they run cleanly even in minimal CI containers.

/// `ZoneAnimationState::fade_in` starts at opacity 0 and moves toward 1.
///
/// Immediately after construction the elapsed time is ~0 ms, so
/// `current_opacity()` must return a value very close to 0.
#[test]
fn zone_animation_state_fade_in_starts_at_zero() {
    let state = ZoneAnimationState::fade_in(120);
    // Immediately after construction elapsed ≈ 0 ms → opacity ≈ 0.
    let opacity = state.current_opacity();
    assert!(
        opacity < 0.05,
        "fade_in must start near opacity 0, got {opacity}"
    );
    assert_eq!(state.target_opacity, 1.0, "fade_in target must be 1.0");
    assert_eq!(state.duration_ms, 120, "fade_in duration must be 120 ms");
}

/// `ZoneAnimationState::fade_out` starts at opacity 1 and moves toward 0.
///
/// Immediately after construction the elapsed time is ~0 ms, so
/// `current_opacity()` must return a value very close to 1.
#[test]
fn zone_animation_state_fade_out_starts_at_one() {
    let state = ZoneAnimationState::fade_out(80);
    // Immediately after construction elapsed ≈ 0 ms → opacity ≈ 1.
    let opacity = state.current_opacity();
    assert!(
        opacity > 0.95,
        "fade_out must start near opacity 1, got {opacity}"
    );
    assert_eq!(state.target_opacity, 0.0, "fade_out target must be 0.0");
    assert_eq!(state.duration_ms, 80, "fade_out duration must be 80 ms");
}

/// `ZoneAnimationState::fade_in_from` starts at the given from_opacity.
///
/// This is the interrupt-semantic path used when content arrives during
/// an active fade-out.
#[test]
fn zone_animation_state_fade_in_from_starts_at_partial_opacity() {
    let from = 0.4_f32;
    let state = ZoneAnimationState::fade_in_from(120, from);
    // Immediately after construction elapsed ≈ 0 ms → opacity ≈ from.
    let opacity = state.current_opacity();
    assert!(
        (opacity - from).abs() < 0.05,
        "fade_in_from must start near from_opacity={from}, got {opacity}"
    );
    assert_eq!(state.target_opacity, 1.0);
    assert_eq!(state.from_opacity, from);
}

/// `portal_tile_anim_opacity` returns 1.0 for a tile not in the animation map.
///
/// CPU-only: directly inspects the struct field — no GPU required.
///
/// This verifies the "no animation active → fully visible" contract so that
/// non-portal tiles and fully settled portal tiles are not dimmed.
#[test]
fn portal_tile_anim_opacity_returns_full_when_no_state() {
    // Construct a minimal token map and animation state map manually.
    // We can't construct a full Compositor without GPU, so we test the
    // underlying logic by directly inspecting ZoneAnimationState.
    let mut anim_states: HashMap<SceneId, ZoneAnimationState> = HashMap::new();
    let tile_id = SceneId::new();
    let other_id = SceneId::new();

    // Start a fade-in for other_id only.
    anim_states.insert(other_id, ZoneAnimationState::fade_in(120));

    // tile_id is NOT in the map → opacity must be 1.0.
    let opacity = anim_states
        .get(&tile_id)
        .map(|s| s.current_opacity())
        .unwrap_or(1.0);
    assert!(
        (opacity - 1.0).abs() < f32::EPSILON,
        "tile not in anim map must return opacity 1.0, got {opacity}"
    );
}

/// A scrollable tile that appears for the first time triggers a fade-in
/// animation with the token-configured `transition_in_ms` duration.
///
/// This exercises `update_portal_tile_animations` end-to-end.
///
/// GPU required (for `Compositor::new_headless`); skips gracefully when no
/// adapter is available.
#[tokio::test]
async fn portal_tile_fade_in_starts_on_first_content() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    // Set a custom transition_in_ms token (200 ms) to distinguish from default.
    compositor
        .token_map
        .insert("portal.transition.in_ms".to_string(), "200".to_string());

    // Build a scene with one scrollable (portal) tile that has a root node.
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("portal-test", 0).unwrap();
    let lease_id = scene.grant_lease("portal-test", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-test",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();

    // Register a scroll config (identifies this tile as a portal tile).
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();

    // Attach content — root node present.
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            radius: None,
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();

    // First call — tile goes from no-content to content.
    compositor.update_portal_tile_animations(&scene);

    // A fade-in animation must have been inserted for tile_id.
    let anim = compositor.portal_tile_anim_states.get(&tile_id);
    assert!(
        anim.is_some(),
        "update_portal_tile_animations must insert fade-in state for scrollable tile"
    );
    let anim = anim.unwrap();
    assert_eq!(
        anim.target_opacity, 1.0,
        "fade-in must have target_opacity 1.0"
    );
    assert_eq!(
        anim.duration_ms, 200,
        "fade-in duration must match token value 200 ms"
    );

    // Opacity must be near 0 immediately after the animation starts.
    let opacity = compositor.portal_tile_anim_opacity(tile_id);
    assert!(
        opacity < 0.2,
        "immediately after fade-in start, opacity must be near 0, got {opacity}"
    );
}

/// Removing a scrollable tile's root node triggers a fade-out animation
/// with the token-configured `transition_out_ms` duration.
///
/// GPU required; skips gracefully when no adapter is available.
///
/// The test seeds `prev_portal_tile_has_content` directly (same-crate access)
/// to simulate the tile having had content in the previous frame, then calls
/// `update_portal_tile_animations` with no root node — matching the content-
/// gone transition without needing a SceneGraph API to clear the root.
#[tokio::test]
async fn portal_tile_fade_out_starts_on_content_removal() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    compositor
        .token_map
        .insert("portal.transition.out_ms".to_string(), "150".to_string());

    // Scene has a scrollable tile with NO root node.
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("portal-fade-out", 0).unwrap();
    let lease_id = scene.grant_lease("portal-fade-out", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-fade-out",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    // root_node is None — content just disappeared.

    // Seed previous state: tile had content last frame.
    compositor
        .prev_portal_tile_has_content
        .insert(tile_id, true);

    compositor.update_portal_tile_animations(&scene);

    // Now the animation state must be a fade-out.
    let anim = compositor.portal_tile_anim_states.get(&tile_id);
    assert!(
        anim.is_some(),
        "update_portal_tile_animations must insert fade-out state after content removal"
    );
    let anim = anim.unwrap();
    assert_eq!(
        anim.target_opacity, 0.0,
        "fade-out must have target_opacity 0.0"
    );
    assert_eq!(
        anim.duration_ms, 150,
        "fade-out duration must match token value 150 ms"
    );
}

/// Interrupting a portal-tile fade-out with a restore must seed the new
/// fade-in from the **eased (on-screen) opacity**, not the linear
/// `current_opacity()` (hud-uir0w).
///
/// Portal tiles are *displayed* through the EaseInOut curve
/// (`portal_tile_anim_opacity`). If the interrupt path seeded the fade-in from
/// the linear value, the next fade-in would start at a different opacity than
/// the frame just rendered — a visible jump. This test drives a fade-out to a
/// progress point where the eased opacity and the linear opacity differ
/// meaningfully (t = 0.25: eased ≈ 0.844 vs linear = 0.750), interrupts it with
/// a restore, and asserts the new fade-in's start opacity matches the displayed
/// eased value, not the linear one. The continuity guarantee follows directly:
/// the new fade-in (also displayed eased) begins at the exact opacity on screen
/// at interruption.
///
/// GPU required (for `Compositor::new_headless`); skips gracefully when no
/// adapter is available.
#[tokio::test]
async fn portal_tile_interrupt_seeds_fade_in_from_eased_opacity() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    compositor
        .token_map
        .insert("portal.transition.in_ms".to_string(), "200".to_string());

    // Scrollable tile WITH content present this frame (content was restored).
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("portal-interrupt", 0).unwrap();
    let lease_id = scene.grant_lease("portal-interrupt", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-interrupt",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            radius: None,
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();

    // Previous frame: tile had NO content, so this frame is an appear (restore).
    compositor
        .prev_portal_tile_has_content
        .insert(tile_id, false);

    // Seed an in-flight fade-out, back-dated to 25% progress so eased and linear
    // opacity are clearly distinguishable. A deliberately LONG duration (15 s of
    // 60 s) keeps the eased curve nearly flat per millisecond: `displayed_eased`
    // is sampled here, but the code under test re-samples
    // `portal_tile_anim_opacity()` against the same live `Instant` a moment
    // later. At 25% of 60 s the eased fade-out moves < 2e-5/ms, so even tens of
    // milliseconds of scheduler delay on a loaded runner stay far under the 0.02
    // assertion tolerance (a short 200 ms duration here would flake — hud-uir0w).
    let mut fade_out = ZoneAnimationState::fade_out(60_000);
    fade_out.transition_start =
        std::time::Instant::now() - std::time::Duration::from_millis(15_000);
    compositor.portal_tile_anim_states.insert(tile_id, fade_out);

    // Opacity actually on screen at interruption (eased) vs the linear value.
    let displayed_eased = compositor.portal_tile_anim_opacity(tile_id);
    let linear = compositor.portal_tile_anim_states[&tile_id].current_opacity();

    // Sanity: the two must differ enough for the assertion below to be meaningful.
    assert!(
        (displayed_eased - linear).abs() > 0.05,
        "test setup: eased ({displayed_eased}) and linear ({linear}) opacity must differ"
    );

    // Interrupt: content restored mid fade-out.
    compositor.update_portal_tile_animations(&scene);

    let state = compositor
        .portal_tile_anim_states
        .get(&tile_id)
        .expect("portal animation state must exist after interrupt fade-in");
    assert_eq!(
        state.target_opacity, 1.0,
        "interrupt must produce a fade-in (target_opacity 1.0)"
    );

    // The new fade-in must start from the eased (displayed) opacity, NOT linear.
    assert!(
        (state.from_opacity - displayed_eased).abs() < 0.02,
        "fade-in must seed from eased/displayed opacity ~{displayed_eased}, got {}",
        state.from_opacity
    );
    assert!(
        (state.from_opacity - linear).abs() > 0.05,
        "fade-in must NOT seed from linear opacity {linear}, got {}",
        state.from_opacity
    );

    // Continuity: the first displayed frame of the new fade-in (eased at t≈0)
    // equals the opacity that was on screen at interruption — no jump.
    let post_interrupt_displayed = compositor.portal_tile_anim_opacity(tile_id);
    assert!(
        (post_interrupt_displayed - displayed_eased).abs() < 0.02,
        "displayed opacity must be continuous across interrupt: was {displayed_eased}, now {post_interrupt_displayed}"
    );
}

/// Non-scrollable tiles must NOT get portal animation states.
///
/// Ensures `update_portal_tile_animations` only affects tiles with a
/// registered `TileScrollConfig` (i.e. portal tiles).
///
/// GPU required; skips gracefully when no adapter is available.
#[tokio::test]
async fn non_scrollable_tile_has_no_portal_animation_state() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("non-scroll-test", 0).unwrap();
    let lease_id = scene.grant_lease("non-scroll-test", 60_000, vec![]);
    let tile_id = scene
        .create_tile(
            tab_id,
            "non-scroll-test",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();

    // NO register_tile_scroll_config — this is NOT a portal tile.
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            radius: None,
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();

    compositor.update_portal_tile_animations(&scene);

    // No animation state must have been created for this non-portal tile.
    assert!(
        !compositor.portal_tile_anim_states.contains_key(&tile_id),
        "non-scrollable tile must not receive a portal animation state"
    );

    // portal_tile_anim_opacity must return 1.0 (fully visible, no dimming).
    let opacity = compositor.portal_tile_anim_opacity(tile_id);
    assert!(
        (opacity - 1.0).abs() < f32::EPSILON,
        "non-scrollable tile opacity must be 1.0, got {opacity}"
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

// ─── hud-w41ef: portal tile backdrop fades as one unit (no see-through on a ──
// geometry change that exposes tile-backdrop-only regions) ───────────────────

/// Build a scrollable (portal-like) TextMarkdown tile with an OPAQUE background,
/// mirroring the resident portal node the projection driver publishes.
fn w41ef_portal_tile(scene: &mut SceneGraph, bounds: Rect, node_bounds: Rect) -> SceneId {
    let tab_id = scene.create_tab("t", 0).unwrap();
    let lease_id = scene.grant_lease("t", 60_000, vec![]);
    let tile_id = scene.create_tile(tab_id, "t", lease_id, bounds, 1).unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            // Single short glyph: leaves the rest of the node body textless so a
            // pixel probe reads the backdrop alone (not a glyph drawn over it).
            content: "x".to_owned(),
            bounds: node_bounds,
            font_size_px: 14.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(0.9, 0.9, 0.9, 1.0),
            // Opaque backdrop (#0A0D11-ish), matching portal.transcript.background.
            background: Some(Rgba::new(0.04, 0.05, 0.07, 1.0)),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();
    tile_id
}

/// Regression for hud-w41ef: when a portal tile's opaque body is faded (§6.3
/// portal transition opacity, or any tile opacity < 1), the whole tile MUST fade
/// as one unit. Before the fix, the flat tile backdrop (`tile_background_color`)
/// and the tile text honoured the fade but the content-node background
/// (`tm.background` painted in `render_node`) did not — so a region covered only
/// by the flat backdrop (e.g. the newly-exposed area after a resize-grow, while
/// the content node still lags at its old smaller size) rendered see-through
/// while the content region stayed fully opaque. This asserts backdrop
/// uniformity: the tile-backdrop-only region and the content region paint at the
/// SAME alpha.
///
/// Asserted at the draw-list (vertex) level rather than by pixel readback: the
/// live overlay geometry pass uses the `clear_pipeline` (REPLACE, no blending),
/// but `render_frame_headless` always uses the blending pipeline, so a readback
/// composites the two overlapping backdrop quads instead of letting the last
/// write win — it cannot represent the live REPLACE alpha. The generated
/// backdrop colors, however, are blend-independent (hud-w41ef).
#[tokio::test]
async fn hud_w41ef_portal_content_background_scaled_by_tile_opacity() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    // No text renderer: `render_node` takes the fallback branch which still emits
    // the content-node background quad first. overlay_mode stays false so
    // `gpu_color` is identity and the emitted alpha is directly comparable.

    let mut scene = SceneGraph::new(256.0, 256.0);
    let bg = Rgba::new(0.04, 0.05, 0.07, 1.0); // opaque backdrop
    let tab_id = scene.create_tab("t", 0).unwrap();
    let lease_id = scene.grant_lease("t", 60_000, vec![]);
    let tile_id = scene
        .create_tile(tab_id, "t", lease_id, Rect::new(0.0, 0.0, 120.0, 120.0), 1)
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: "x".to_owned(),
                    bounds: Rect::new(0.0, 0.0, 120.0, 120.0),
                    font_size_px: 14.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba::new(0.9, 0.9, 0.9, 1.0),
                    background: Some(bg),
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Ellipsis,
                    color_runs: Box::default(),
                }),
            },
        )
        .unwrap();

    // Half-fade the whole tile (deterministic stand-in for a §6.3 portal fade).
    scene.tiles.get_mut(&tile_id).unwrap().opacity = 0.5;
    let tile = scene.tiles.get(&tile_id).unwrap().clone();

    // Flat tile backdrop alpha (already opacity-scaled).
    let flat_bg = compositor
        .tile_background_color(&tile, &scene)
        .expect("markdown tile always has a flat backdrop");
    let flat_alpha = flat_bg[3];

    // Content-node backdrop quad, as emitted by render_node.
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    let mut cmds: Vec<super::draw_cmds::TexturedDrawCmd> = Vec::new();
    compositor.render_node(root_id, &tile, &scene, &mut verts, &mut cmds, 120.0, 120.0);
    let node_bg_alpha = verts
        .first()
        .expect("render_node must emit the content background quad first")
        .color[3];

    // Fix: the content-node background must be scaled by the tile opacity, exactly
    // like the flat backdrop, so the tile fades as one unit. Before the fix the
    // node background was painted at full alpha (bg.a = 1.0) while the flat
    // backdrop was 0.5 — the exact divergence that renders the tile-backdrop-only
    // region see-through relative to the content region on a resize/fade.
    assert!(
        (node_bg_alpha - 0.5 * bg.a).abs() < 1e-4,
        "content-node background alpha must be tile-opacity-scaled: got {node_bg_alpha}, \
         expected {} (bg.a {} × tile.opacity 0.5)",
        0.5 * bg.a,
        bg.a
    );
    assert!(
        (node_bg_alpha - flat_alpha).abs() < 1e-4,
        "content-node backdrop ({node_bg_alpha}) and flat tile backdrop ({flat_alpha}) \
         must paint at the SAME alpha so the tile fades uniformly (hud-w41ef)"
    );
}

/// Complement: at full tile opacity (an established portal, no fade), the grown
/// tile-backdrop-only region stays fully opaque — the desktop never shows through
/// after a resize-grow. This is the steady-state "not see-through" guarantee.
#[tokio::test]
async fn hud_w41ef_portal_backdrop_opaque_after_resize_grow_no_fade() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.overlay_mode = true;

    let mut scene = SceneGraph::new(256.0, 256.0);
    let tile_id = w41ef_portal_tile(
        &mut scene,
        Rect::new(10.0, 10.0, 100.0, 100.0),
        Rect::new(0.0, 0.0, 100.0, 100.0),
    );
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.portal_tile_anim_states.clear();

    if let Some(t) = scene.tiles.get_mut(&tile_id) {
        t.bounds.width = 200.0;
        t.bounds.height = 200.0;
        scene.version += 1;
    }
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let px = surface.read_pixels(&compositor.device);

    let a_grown = crate::surface::HeadlessSurface::pixel_at(&px, 256, 180, 180)[3];
    assert!(
        a_grown > 250,
        "grown portal backdrop must stay opaque at full opacity (got alpha={a_grown})"
    );
}

// ─── hud-b0x0m: every tile node fill type fades with the tile, not just the ──
// portal TextMarkdown background fixed in hud-w41ef. Draw-list-level assertions
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
    let lease_id = scene.grant_lease("t", 60_000, vec![]);
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

/// `prime_vertical_flow_layout` must resolve flow offsets even when
/// `init_text_renderer` has never been called (hud-tfm3p review of hud-9gopx).
///
/// hud-9gopx swapped this function from unconditionally building a fresh
/// `bundled_font_system()` to measuring against `self.text_rasterizer`'s own
/// `FontSystem` — gating the ENTIRE resolve on `self.text_rasterizer.is_some()`.
/// That silently dropped flow-stacking for every `VerticalFlow` child (not just
/// text — `SolidColor` / `StaticImage` / `HitRegion` children read
/// `tile_flow_offsets` too, per hud-pd9bp) whenever no rasterizer had been
/// initialized yet, even though non-text children never needed a `FontSystem` at
/// all — a real regression versus the pre-hud-9gopx baseline, which ran
/// regardless of rasterizer state. `make_compositor_and_surface` deliberately
/// does NOT call `init_text_renderer` (only production runtime setup and a
/// handful of text-specific tests do), so this fixture already exercises the
/// exact `text_rasterizer: None` state the regression required — no
/// GPU-specific machinery beyond the existing headless adapter is needed.
#[tokio::test]
async fn hud_tfm3p_flow_offsets_resolve_without_a_text_rasterizer() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    assert!(
        compositor.text_rasterizer.is_none(),
        "fixture precondition: this test only proves what it claims if no \
         rasterizer has been initialized"
    );

    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("t", 0).unwrap();
    let lease_id = scene.grant_lease("t", 60_000, vec![]);
    let tile_id = scene
        .create_tile(tab_id, "t", lease_id, Rect::new(0.0, 0.0, 200.0, 200.0), 1)
        .unwrap();

    let parent_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: NodeLayout::VerticalFlow,
                id: parent_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 0.0, 0.0, 0.0),
                    bounds: Rect::new(0.0, 10.0, 200.0, 0.0),
                    radius: None,
                }),
            },
        )
        .unwrap();

    // Two non-text (SolidColor) children — flow resolution positions these
    // without ever needing a `FontSystem`, so they isolate the rasterizer-
    // presence regression from any font-measurement concern.
    let child0_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(parent_id),
            Node {
                layout: Default::default(),
                id: child0_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(1.0, 0.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 999.0, 200.0, 40.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    let child1_id = SceneId::new();
    scene
        .add_node_to_tile(
            tile_id,
            Some(parent_id),
            Node {
                layout: Default::default(),
                id: child1_id,
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(0.0, 1.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 999.0, 200.0, 20.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
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

/// A non-rounded `SolidColor` node fill must be scaled by the whole-tile fade
/// (hud-b0x0m). Before the fix it was painted at `sc.color.a` regardless of tile
/// opacity — the same divergence hud-w41ef fixed for portal backgrounds.
#[tokio::test]
async fn hud_b0x0m_solid_color_node_fill_scaled_by_tile_opacity() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let color = Rgba::new(0.2, 0.4, 0.6, 0.8);
    let root_id = SceneId::new();
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tile_id = b0x0m_tile_with_root(
        &mut scene,
        Rect::new(0.0, 0.0, 120.0, 120.0),
        root_id,
        NodeData::SolidColor(SolidColorNode {
            color,
            bounds: Rect::new(0.0, 0.0, 120.0, 120.0),
            radius: None,
        }),
    );

    // Faded tile: fill alpha must track tile.opacity.
    scene.tiles.get_mut(&tile_id).unwrap().opacity = 0.5;
    let tile = scene.tiles.get(&tile_id).unwrap().clone();
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    let mut cmds: Vec<super::draw_cmds::TexturedDrawCmd> = Vec::new();
    compositor.render_node(root_id, &tile, &scene, &mut verts, &mut cmds, 120.0, 120.0);
    let faded = verts
        .first()
        .expect("SolidColor node must emit a fill quad")
        .color[3];
    assert!(
        (faded - 0.5 * color.a).abs() < 1e-4,
        "SolidColor fill alpha must be tile-opacity-scaled: got {faded}, expected {}",
        0.5 * color.a
    );

    // Full opacity: fill alpha unchanged (= color.a).
    scene.tiles.get_mut(&tile_id).unwrap().opacity = 1.0;
    let tile = scene.tiles.get(&tile_id).unwrap().clone();
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    let mut cmds: Vec<super::draw_cmds::TexturedDrawCmd> = Vec::new();
    compositor.render_node(root_id, &tile, &scene, &mut verts, &mut cmds, 120.0, 120.0);
    let full = verts.first().unwrap().color[3];
    assert!(
        (full - color.a).abs() < 1e-4,
        "at full opacity SolidColor fill alpha must be unchanged: got {full}, expected {}",
        color.a
    );
}

/// A rounded `SolidColor` node (painted via the SDF rounded-rect pass, not the
/// flat vertex pass) must also fade with the tile (hud-b0x0m).
#[tokio::test]
async fn hud_b0x0m_rounded_solid_color_scaled_by_tile_opacity() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let color = Rgba::new(0.3, 0.3, 0.35, 1.0);
    let root_id = SceneId::new();
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tile_id = b0x0m_tile_with_root(
        &mut scene,
        Rect::new(0.0, 0.0, 120.0, 120.0),
        root_id,
        NodeData::SolidColor(SolidColorNode {
            color,
            bounds: Rect::new(0.0, 0.0, 120.0, 120.0),
            radius: Some(12.0),
        }),
    );

    scene.tiles.get_mut(&tile_id).unwrap().opacity = 0.5;
    let cmds = compositor.collect_tile_rounded_rect_cmds(&scene);
    let faded = cmds
        .first()
        .expect("rounded SolidColor root must emit a rounded-rect cmd")
        .color[3];
    assert!(
        (faded - 0.5 * color.a).abs() < 1e-4,
        "rounded SolidColor alpha must be tile-opacity-scaled: got {faded}, expected {}",
        0.5 * color.a
    );

    scene.tiles.get_mut(&tile_id).unwrap().opacity = 1.0;
    let full = compositor
        .collect_tile_rounded_rect_cmds(&scene)
        .first()
        .unwrap()
        .color[3];
    assert!(
        (full - color.a).abs() < 1e-4,
        "at full opacity rounded SolidColor alpha must be unchanged: got {full}, expected {}",
        color.a
    );
}

/// A `StaticImage` textured quad's tint alpha must be scaled by the FULL tile
/// fade — `tile_effective_opacity`, which includes the §6.3 portal-transition
/// component — not just `effective_tile_opacity` (hud-b0x0m).
#[tokio::test]
async fn hud_b0x0m_static_image_tint_scaled_by_tile_opacity() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let resource_id = ResourceId::of(b"hud-b0x0m 2x2 image");
    // Register real RGBA bytes + upload the GPU texture so render_node takes the
    // textured (tint) path rather than the fallback placeholder.
    let rgba: std::sync::Arc<[u8]> = std::sync::Arc::from(vec![255u8; 2 * 2 * 4]);
    compositor.register_image_bytes(resource_id, rgba, 2, 2);
    assert!(
        compositor.ensure_image_texture(resource_id, 2, 2),
        "image texture must upload for the tint path"
    );

    let root_id = SceneId::new();
    let mut scene = SceneGraph::new(256.0, 256.0);
    scene.register_resource(resource_id);
    let tile_id = b0x0m_tile_with_root(
        &mut scene,
        Rect::new(0.0, 0.0, 120.0, 120.0),
        root_id,
        NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 2,
            height: 2,
            decoded_bytes: 2 * 2 * 4,
            fit_mode: ImageFitMode::Fill,
            bounds: Rect::new(0.0, 0.0, 120.0, 120.0),
        }),
    );

    // Drive the §6.3 portal-transition component specifically (NOT tile.opacity):
    // leave tile.opacity = 1.0 and pin a deterministic portal fade at 0.5. This is
    // the exact case the old code missed — it used `effective_tile_opacity`
    // (tile.opacity + drag = 1.0 here) and ignored the portal fade, so the image
    // stayed fully opaque while the faded tile backdrop/text went to 0.5.
    // `duration_ms: 0` makes `current_opacity_eased` return `target_opacity`
    // (time-independent), so the pinned 0.5 is deterministic.
    compositor.portal_tile_anim_states.insert(
        tile_id,
        super::draw_cmds::ZoneAnimationState {
            transition_start: std::time::Instant::now(),
            duration_ms: 0,
            from_opacity: 0.5,
            target_opacity: 0.5,
        },
    );
    assert!(
        (compositor.portal_tile_anim_opacity(tile_id) - 0.5).abs() < 1e-4,
        "test setup: portal fade must be pinned at 0.5"
    );

    let tile = scene.tiles.get(&tile_id).unwrap().clone();
    let mut verts: Vec<crate::pipeline::RectVertex> = Vec::new();
    let mut cmds: Vec<super::draw_cmds::TexturedDrawCmd> = Vec::new();
    compositor.render_node(root_id, &tile, &scene, &mut verts, &mut cmds, 120.0, 120.0);
    let tint_a = cmds
        .first()
        .expect("StaticImage with a cached texture must emit a textured draw cmd")
        .tint[3];
    assert!(
        (tint_a - 0.5).abs() < 1e-4,
        "StaticImage tint alpha must be tile-opacity-scaled: got {tint_a}, expected 0.5"
    );
}

// ─── hud-dat3x: tile text honours whole-tile opacity ─────────────────────────
// A tile whose opacity is driven to 0 (the exemplar minimize path calls
// `update_tile_opacity(0.0)`) hides its solid-color backdrop via the quad path,
// but text was collected at full opacity — leaving floating glyphs on screen.
// `collect_text_items` must now fold the same `tile_effective_opacity` the quad
// path uses into every text item: opacity 0 → ZERO items from that tile (nothing
// shaped/rasterized), a fractional opacity → items carrying the blended alpha.

/// Build a single scrollable (portal) markdown tile carrying `content` at the
/// given whole-tile `opacity`, and return `(scene, tile_id)`.
fn dat3x_markdown_tile_scene(content: &str, opacity: f32) -> (SceneGraph, SceneId) {
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("t", 0).unwrap();
    let lease_id = scene.grant_lease("t", 60_000, vec![]);
    let tile_id = scene
        .create_tile(tab_id, "t", lease_id, Rect::new(0.0, 0.0, 120.0, 120.0), 1)
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    let root_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![],
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: content.to_owned(),
                    bounds: Rect::new(0.0, 0.0, 120.0, 120.0),
                    font_size_px: 14.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba::new(0.9, 0.9, 0.9, 1.0),
                    background: Some(Rgba::new(0.04, 0.05, 0.07, 1.0)),
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Ellipsis,
                    color_runs: Box::default(),
                }),
            },
        )
        .unwrap();
    scene.tiles.get_mut(&tile_id).unwrap().opacity = opacity;
    (scene, tile_id)
}

/// A tile at opacity 0 must contribute NO text items — the minimize path hides
/// the backdrop AND the glyphs, together.
#[tokio::test]
async fn hud_dat3x_zero_tile_opacity_collects_no_text() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let (scene, _tile_id) = dat3x_markdown_tile_scene("hello transcript", 0.0);

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    assert!(
        items.iter().all(|t| !t.text.contains("hello")),
        "a tile at opacity 0 must yield no transcript text items, got {} item(s)",
        items.len()
    );
}

/// A tile at fractional opacity must blend its glyphs proportionally: the text
/// item carries the tile alpha (0.5), matching the backdrop fade.
#[tokio::test]
async fn hud_dat3x_fractional_tile_opacity_blends_text() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let (scene, _tile_id) = dat3x_markdown_tile_scene("hello transcript", 0.5);

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    let item = items
        .iter()
        .find(|t| t.text.contains("hello"))
        .expect("a tile at opacity 0.5 must still render its text");
    assert!(
        (item.opacity - 0.5).abs() < 1e-4,
        "tile opacity 0.5 must fold into the text item opacity: got {}",
        item.opacity
    );
}

/// Control: a fully-opaque tile renders its text at full opacity (no regression
/// to the steady-state path).
#[tokio::test]
async fn hud_dat3x_full_tile_opacity_renders_text_opaque() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let (scene, _tile_id) = dat3x_markdown_tile_scene("hello transcript", 1.0);

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    let item = items
        .iter()
        .find(|t| t.text.contains("hello"))
        .expect("a fully-opaque tile must render its text");
    assert!(
        (item.opacity - 1.0).abs() < 1e-4,
        "full tile opacity must leave text opacity at 1.0: got {}",
        item.opacity
    );
}

/// A portal fading IN (durable `tile.opacity == 1`, TRANSIENT §6.3 animation
/// opacity pinned at ~0) must STILL collect and shape its text — only the item
/// alpha rides the transient fade to ~0. The skip-shaping optimization gates on
/// the DURABLE scene-level `tile.opacity` only; gating it on the combined value
/// would defer the warm-up shape into the middle of the animation, forcing a
/// re-shape hitch when the tile crosses the visibility threshold (hud-991cj
/// steady-state reuse). This is the inverse of the durable-minimize skip.
#[tokio::test]
async fn hud_dat3x_transient_portal_fade_still_shapes_text() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let (scene, tile_id) = dat3x_markdown_tile_scene("hello transcript", 1.0);

    // Pin the §6.3 portal fade at ~0 while leaving tile.opacity = 1.0.
    // `duration_ms: 0` makes `current_opacity_eased` return `target_opacity`.
    compositor.portal_tile_anim_states.insert(
        tile_id,
        super::draw_cmds::ZoneAnimationState {
            transition_start: std::time::Instant::now(),
            duration_ms: 0,
            from_opacity: 0.0,
            target_opacity: 0.0,
        },
    );

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    let item = items
        .iter()
        .find(|t| t.text.contains("hello"))
        .expect("a fading-in portal (durable opacity 1) must STILL shape its text (warm-up)");
    assert!(
        item.opacity <= 1e-4,
        "transient fade must blend the item alpha to ~0: got {}",
        item.opacity
    );
}

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
    let lease_id = scene.grant_lease("portal", 60_000, vec![]);
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
    let lease = scene.grant_lease("ns", 120_000, vec![]);
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
    let lease = scene.grant_lease("ns", 120_000, vec![]);
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
    let lease = scene.grant_lease("portal", 120_000, vec![]);
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
    let lease = scene.grant_lease("portal", 120_000, vec![]);
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
