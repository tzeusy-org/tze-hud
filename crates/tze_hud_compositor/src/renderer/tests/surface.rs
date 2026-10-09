use super::*;

fn primary_of(build: &super::frame::WindowedFrameBuild) -> crate::FrameTarget {
    let (w, h) = build.size();
    crate::FrameTarget::primary(w, h)
}

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

/// Invariant 3 pixel test: the safe-mode overlay is drawn by the windowed frame
/// above even the highest-z agent tile, so no agent content can occlude it.
///
/// Builds the same scene twice through `build_windowed_frame` and captures the
/// pixels: without the overlay the max-z red tile shows through; with it the
/// tile is dimmed.
#[tokio::test]
async fn test_chrome_always_above_max_zorder_tile() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

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
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let build = compositor.build_windowed_frame(&mut scene, 256, 256);
    let plain = compositor
        .capture_windowed_frame(&build, &primary_of(&build), format)
        .expect("capture");
    let center = (128 * 256 + 128) * 4;
    assert!(
        plain.rgba[center] > 200,
        "without the overlay the max-z tile is plain red: {:?}",
        &plain.rgba[center..center + 4]
    );

    compositor.set_safe_mode_overlay(true);
    let build = compositor.build_windowed_frame(&mut scene, 256, 256);
    let dimmed = compositor
        .capture_windowed_frame(&build, &primary_of(&build), format)
        .expect("capture");
    assert!(
        dimmed.rgba[center] < plain.rgba[center] - 40,
        "the safe-mode overlay must cover the max-z tile: {:?}",
        &dimmed.rgba[center..center + 4]
    );
}

/// hud-w5zon: the system card (backdrop AND text) is the last pass, above a
/// max-z tile and the safe-mode overlay, so the pairing code stays readable.
/// Pixel readback via the admin-capture seam (no swapchain, no deadlock).
#[tokio::test]
async fn test_system_card_drawn_above_safe_mode_overlay_and_content() {
    use crate::renderer::{SystemCardKind, SystemCardModel};
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(800, 600).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(800.0, 600.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 800.0, 600.0),
            tze_hud_scene::types::ZONE_TILE_Z_MIN - 1,
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
                    color: Rgba::new(1.0, 0.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.set_safe_mode_overlay(true);
    compositor.set_system_card(Some(SystemCardModel {
        kind: SystemCardKind::Pairing,
        title: "Pair an agent".into(),
        lines: vec!["482913".into()],
    }));

    let build = compositor.build_windowed_frame(&mut scene, 800, 600);
    let frame = compositor
        .capture_windowed_frame(
            &build,
            &primary_of(&build),
            wgpu::TextureFormat::Rgba8UnormSrgb,
        )
        .expect("capture");

    // Default card geometry: 420 wide, centred. Tall enough that the centre
    // row is inside it; scan that card-wide band for pixels.
    let (x0, x1) = (190usize, 610usize);
    let (y0, y1) = (240usize, 360usize);
    let px = |x: usize, y: usize| &frame.rgba[(y * 800 + x) * 4..(y * 800 + x) * 4 + 4];
    // Backdrop (#0C1426, ~95% opaque): the red tile must not bleed through and
    // the 70% black dim must not darken it to black. Sample inside the card,
    // right of the text, below the accent bar.
    let backdrop = px(x1 - 10, y0 + 12);
    assert!(
        backdrop[0] < 60 && backdrop[2] > 15,
        "card backdrop must be drawn above tile and dim, got {backdrop:?}"
    );
    // Text (white): some pixels inside the card must be near-white. Under the
    // safe-mode dim (alpha 0.7) white would drop to ~77, so this fails if the
    // text pass runs before the overlay.
    let bright = (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .filter(|&(x, y)| px(x, y)[..3].iter().all(|&c| c > 200))
        .count();
    assert!(
        bright > 20,
        "card text must stay bright above the safe-mode dim ({bright} bright px)"
    );
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
    let tab = scene.create_tab("capture", 0).unwrap();
    let lease = scene.grant_lease("capture", 60_000);
    let tile = scene
        .create_tile(tab, "capture", lease, Rect::new(16.0, 16.0, 128.0, 64.0), 1)
        .unwrap();
    scene
        .set_tile_root(
            tile,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(1.0, 0.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 0.0, 128.0, 64.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    let build = compositor.build_windowed_frame(&mut scene, 320, 200);
    let frame = compositor
        .capture_windowed_frame(
            &build,
            &primary_of(&build),
            wgpu::TextureFormat::Rgba8UnormSrgb,
        )
        .expect("capture");
    assert_eq!((frame.width, frame.height), (320, 200));
    assert_eq!(frame.rgba.len(), 320 * 200 * 4);
    let inside = (40 * frame.width as usize + 40) * 4;
    let red = &frame.rgba[inside..inside + 4];
    assert!(
        red[0] > 200 && red[1] < 40 && red[2] < 40 && red[3] == 255,
        "captured tile must contain actual red pixels: {red:?}"
    );
    let outside = (180 * frame.width as usize + 290) * 4;
    assert_ne!(
        red,
        &frame.rgba[outside..outside + 4],
        "capture must distinguish the rendered tile from its background"
    );

    let build = compositor.build_windowed_frame(&mut scene, 320, 200);
    let unsupported = compositor.capture_windowed_frame(
        &build,
        &primary_of(&build),
        wgpu::TextureFormat::Rgba16Float,
    );
    assert!(matches!(
        unsupported,
        Err(crate::CaptureError::UnsupportedFormat(_))
    ));
}

/// The admin capture path builds a frame outside the render gate, so building
/// an idle scene must neither bump the versions the idle gate watches nor start
/// an animation (hud-i2e10.8): a capture cannot cause or consume a repaint.
#[tokio::test]
async fn building_an_idle_frame_leaves_the_idle_gate_inputs_unchanged() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(320.0, 200.0);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    assert!(!compositor.has_inflight_animation(&scene));
    let (version, epoch) = (scene.version, scene.geometry_epoch);

    let _ = compositor.build_windowed_frame(&mut scene, 320, 200);

    assert_eq!((scene.version, scene.geometry_epoch), (version, epoch));
    assert!(!compositor.has_inflight_animation(&scene));
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

// ── Multi-display targets (hud-1pt5h) ──────────────────────────────────────

/// A 128x128 tile of `color` at the scene origin.
fn corner_tile_scene(color: Rgba) -> SceneGraph {
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 128.0, 128.0),
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
                    color,
                    bounds: Rect::new(0.0, 0.0, 128.0, 128.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    scene
}

/// A display window up and to the left of the primary sees the scene origin
/// at its own (128, 128): the shared frame is translated, not stretched.
#[tokio::test]
async fn offset_display_target_shows_scene_translated() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = corner_tile_scene(Rgba::new(1.0, 0.0, 0.0, 1.0));
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    let build = compositor.build_windowed_frame(&mut scene, 256, 256);
    let target = crate::FrameTarget {
        x: -128.0,
        y: -128.0,
        width: 256,
        height: 256,
        primary: false,
    };
    let frame = compositor
        .capture_windowed_frame(&build, &target, wgpu::TextureFormat::Rgba8UnormSrgb)
        .expect("capture");
    let px = |x: usize, y: usize| &frame.rgba[(y * 256 + x) * 4..(y * 256 + x) * 4 + 4];
    assert!(
        px(200, 200)[0] > 200,
        "tile lands at the target's lower right: {:?}",
        px(200, 200)
    );
    assert!(
        px(50, 50)[0] < 100,
        "nothing at the target's upper left: {:?}",
        px(50, 50)
    );
}

/// A window's signature only reflects content inside it: changing a tile on
/// the primary leaves a window that does not overlap it unchanged.
#[tokio::test]
async fn frame_signature_ignores_content_outside_the_target() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let away = crate::FrameTarget {
        x: 1000.0,
        y: 0.0,
        width: 256,
        height: 256,
        primary: false,
    };
    let overlapping = crate::FrameTarget {
        x: -64.0,
        y: -64.0,
        width: 256,
        height: 256,
        primary: false,
    };
    let mut signatures = Vec::new();
    for color in [Rgba::new(1.0, 0.0, 0.0, 1.0), Rgba::new(0.0, 1.0, 0.0, 1.0)] {
        let mut scene = corner_tile_scene(color);
        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        let build = compositor.build_windowed_frame(&mut scene, 256, 256);
        signatures.push((
            compositor.frame_signature(&build, &away),
            compositor.frame_signature(&build, &overlapping),
        ));
    }
    assert_eq!(
        signatures[0].0, signatures[1].0,
        "off-window change is invisible"
    );
    assert_ne!(
        signatures[0].1, signatures[1].1,
        "on-window change is visible"
    );
    let mut scene = corner_tile_scene(Rgba::new(1.0, 0.0, 0.0, 1.0));
    let tile_id = scene.visible_tiles()[0].id;
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    let idle = compositor.build_windowed_frame(&mut scene, 256, 256);
    let idle_away = compositor.frame_signature(&idle, &away);
    let idle_overlap = compositor.frame_signature(&idle, &overlapping);
    let version = scene.version;
    let epoch = scene.geometry_epoch;
    scene.set_drag_active(tile_id);
    assert_eq!(scene.version, version);
    assert_eq!(
        scene.geometry_epoch,
        epoch + 1,
        "border admission bypasses content-cache churn"
    );
    let active = compositor.build_windowed_frame(&mut scene, 256, 256);
    assert_eq!(compositor.frame_signature(&active, &away), idle_away);
    assert_ne!(
        compositor.frame_signature(&active, &overlapping),
        idle_overlap
    );
    scene.set_drag_active(tile_id);
    assert_eq!(scene.geometry_epoch, epoch + 1);
    let unchanged = compositor.build_windowed_frame(&mut scene, 256, 256);
    assert_eq!(
        compositor.frame_signature(&unchanged, &overlapping),
        compositor.frame_signature(&active, &overlapping)
    );
    scene.clear_drag_active(tile_id);
    let cleared = compositor.build_windowed_frame(&mut scene, 256, 256);
    assert_eq!(
        compositor.frame_signature(&cleared, &overlapping),
        idle_overlap
    );
}

/// A zone assigned to a secondary display renders in that display's window
/// and its hit regions move with it; the primary no longer shows it.
#[tokio::test]
async fn zone_assigned_to_secondary_display_renders_there() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.set_display_layout(crate::DisplayLayout::new(
        vec![
            crate::DisplayRect {
                name: "MAIN".into(),
                rect: Rect::new(0.0, 0.0, 256.0, 256.0),
                primary: true,
            },
            crate::DisplayRect {
                name: "SIDE".into(),
                rect: Rect::new(256.0, -128.0, 256.0, 256.0),
                primary: false,
            },
        ],
        &HashMap::from([("notif".to_string(), "side".to_string())]),
    ));
    let side = crate::FrameTarget {
        x: 256.0,
        y: -128.0,
        width: 256,
        height: 256,
        primary: false,
    };

    let mut frames = Vec::new();
    for publish in [false, true] {
        let mut scene = SceneGraph::new(256.0, 256.0);
        let _tab = scene.create_tab("Main", 0).unwrap();
        scene.register_zone(tze_hud_scene::types::ZoneDefinition {
            id: SceneId::new(),
            name: "notif".to_string(),
            description: "Stack zone".to_string(),
            geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
                x_pct: 0.1,
                y_pct: 0.1,
                width_pct: 0.8,
                height_pct: 0.5,
            },
            accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
            rendering_policy: tze_hud_scene::types::RenderingPolicy {
                font_size_px: Some(16.0),
                ..Default::default()
            },
            contention_policy: ContentionPolicy::Stack { max_depth: 5 },
            max_publishers: 8,
            auto_clear_ms: None,
            layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
            ephemeral: false,
        });
        if publish {
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
        }
        compositor.prime_markdown_cache(&scene);
        compositor.prime_truncation_cache(&scene);
        let build = compositor.build_windowed_frame(&mut scene, 256, 256);
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let primary = compositor
            .capture_windowed_frame(&build, &primary_of(&build), format)
            .expect("capture primary");
        let secondary = compositor
            .capture_windowed_frame(&build, &side, format)
            .expect("capture secondary");
        if publish {
            compositor.populate_zone_hit_regions(&mut scene, 256.0, 256.0);
            let dismiss = scene.overlay.zone_hit_regions[0].bounds;
            assert!(
                dismiss.x >= 256.0 && dismiss.y >= -128.0 && dismiss.y < 128.0,
                "hit regions are in scene space on the secondary: {dismiss:?}"
            );
        }
        frames.push((primary.rgba, secondary.rgba));
    }
    assert_eq!(
        frames[0].0, frames[1].0,
        "the primary does not show the zone"
    );
    assert_ne!(frames[0].1, frames[1].1, "the secondary shows the zone");
}

/// Invariant 3 on every display: flipping safe mode changes what a
/// secondary window shows, so it presents the overlay (and its removal) even
/// though the scene did not change.
#[tokio::test]
async fn safe_mode_flip_changes_secondary_signature() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let side = crate::FrameTarget {
        x: 256.0,
        y: 0.0,
        width: 256,
        height: 256,
        primary: false,
    };
    let mut scene = SceneGraph::new(256.0, 256.0);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    let mut signatures = Vec::new();
    for safe_mode in [false, true, false] {
        compositor.set_safe_mode_overlay(safe_mode);
        let build = compositor.build_windowed_frame(&mut scene, 256, 256);
        signatures.push(compositor.frame_signature(&build, &side));
    }
    assert_ne!(signatures[0], signatures[1], "entering safe mode repaints");
    assert_ne!(signatures[1], signatures[2], "leaving safe mode repaints");
    assert_eq!(signatures[0], signatures[2]);
}
