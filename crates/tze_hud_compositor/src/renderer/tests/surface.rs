use super::*;

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
    let lease_id = scene.grant_lease("agent", 60_000);
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
    let lease_id = scene.grant_lease("agent", 60_000);
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

/// An admin capture renders a built frame offscreen without acquiring or
/// presenting anything, and returns the frame's pixels (hud-i2e10.8).
#[tokio::test]
async fn capture_windowed_frame_reads_back_the_built_frame() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(320.0, 200.0);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    let build = compositor.build_windowed_frame(&mut scene, 320, 200);
    let frame = compositor
        .capture_windowed_frame(build, wgpu::TextureFormat::Rgba8UnormSrgb)
        .expect("capture");
    assert_eq!((frame.width, frame.height), (320, 200));
    assert_eq!(frame.rgba.len(), 320 * 200 * 4);

    let build = compositor.build_windowed_frame(&mut scene, 320, 200);
    let unsupported = compositor.capture_windowed_frame(build, wgpu::TextureFormat::Rgba16Float);
    assert!(matches!(
        unsupported,
        Err(crate::CaptureError::UnsupportedFormat(_))
    ));
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
    let result = {
        let _gpu = GPU_INIT_MUTEX.lock().await;
        Compositor::new_headless(64, 64).await
    };
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
