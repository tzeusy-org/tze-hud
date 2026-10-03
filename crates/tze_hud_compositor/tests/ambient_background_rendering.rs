//! Compositor integration tests for ambient-background rendering.
//!
//! Covers openspec/changes/exemplar-ambient-background/specs/exemplar-ambient-background/spec.md
//! — Requirements:
//!   - "Ambient Background Zone Visual Contract"
//!     Scenarios: "Solid color background fills the display",
//!     "No publication yields transparent/clear background",
//!     "StaticImage renders placeholder in v1"
//!   - "Ambient Background Latest-Wins Contention"
//!     Scenarios: "New solid color replaces previous solid color",
//!     "Rapid replacement under contention"
//!   - "Background Layer Z-Order"
//!     Scenarios: "Content tile renders on top of background",
//!     "Background layer uses LayerAttachment::Background"
//!
//! ## Test list
//!
//! 1. `test_ambient_background_solid_color_renders` — Publish dark-blue SolidColor,
//!    render frame, assert pixels are blue-dominant (not the clear color).
//!
//! 2. `test_ambient_background_no_publication_empty` — No content published → zone's
//!    active_publishes is empty and background renders as the runtime clear color.
//!
//! 3. `test_ambient_background_replacement_contention` — Publish red then blue, render
//!    one frame, assert blue pixels + publication count is 1.
//!
//! 4. `test_ambient_background_rapid_replacement` — Publish 10 colors in sequence
//!    within a single frame interval, assert only last color visible + count is 1.
//!
//! 5. `test_ambient_background_zorder_below_content_zones` — ambient-background
//!    (Background layer, dark blue) + content zone (Content layer, red at center)
//!    → pixel inside content zone is red (content occludes background), pixel at
//!    corner is dark blue (background visible).
//!
//! 6. `test_ambient_background_static_image_renders_placeholder` — Publish
//!    `ZoneContent::StaticImage` to ambient-background → rendered pixels match
//!    the warm-gray placeholder color (linear 0.3,0.3,0.3; sRGB ≈ 149).
//!
//! 7. `test_ambient_background_layer_attachment_is_background` — `ZoneRegistry::with_defaults()`
//!    MUST register ambient-background with `LayerAttachment::Background` (pure scene-state
//!    assertion, no GPU required).
//!
//! ## Infrastructure
//!
//! Uses `Compositor::new_headless` + `HeadlessSurface::new` (matching the pattern
//! of the inline test helper `make_compositor_and_surface` in renderer.rs), plus
//! `HeadlessSurface::assert_pixel_color` and `HeadlessSurface::pixel_at` for pixel
//! inspection.
//!
//! Set `TZE_HUD_SKIP_GPU_TESTS=1` to skip all GPU-dependent tests (e.g. in headless
//! CI environments without llvmpipe). On CI with Mesa installed, set
//! `HEADLESS_FORCE_SOFTWARE=1` instead so the software renderer is used.
//!
//! ## sRGB note
//!
//! The headless surface uses `Rgba8UnormSrgb`. wgpu applies linear→sRGB gamma
//! conversion automatically when writing to the framebuffer. Tolerances of ±8
//! are used to accommodate software-renderer (llvmpipe / WARP) rounding differences.
//!
//! ## Expected pixel values for new tests
//!
//! Z-order test (256×256 surface):
//!   - Background: linear(0.0, 0.0, 0.5, 1.0) → sRGB [0, 0, 188]
//!   - Content zone (center 50%): linear(1.0, 0.0, 0.0, 1.0) → sRGB [255, 0, 0]
//!   - Corner pixels (background area): sRGB [0, 0, 188] ± 8
//!   - Centre pixel (content zone area): sRGB [255, 0, 0] ± 8
//!
//! StaticImage placeholder (warm-gray):
//!   - STATIC_IMAGE_PLACEHOLDER_COLOR = linear(0.3, 0.3, 0.3, 1.0)
//!   - sRGB: 1.055 × 0.3^(1/2.4) − 0.055 ≈ 0.584 × 255 ≈ 149
//!   - Expected ≈ [149, 149, 149, 255] ± 8
//!
//! ## References
//!
//! - hud-gwhr.1 (SolidColor + contention tests)
//! - hud-gwhr.2 (this task: z-order, StaticImage, LayerAttachment tests)
//! - hud-gwhr (parent epic: exemplar-ambient-background)
//! - design.md §Decision 3: Tests validate renderer output, not just scene state

mod common;

use tze_hud_compositor::{Compositor, CompositorError, surface::HeadlessSurface};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, ImageFitMode, LayerAttachment, Node, NodeData, Rect,
    RenderingPolicy, ResourceId, Rgba, SceneId, StaticImageNode, ZoneContent, ZoneDefinition,
    ZoneMediaType, ZoneRegistry,
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Create a headless compositor and matching surface pair.
///
/// Returns `None` when `TZE_HUD_SKIP_GPU_TESTS=1` is set or no wgpu adapter is
/// available. Use the `gpu_or_skip!` macro to early-return from the test.
///
/// Mirrors the private `make_compositor_and_surface` helper in renderer.rs.
async fn make_compositor_and_surface(w: u32, h: u32) -> Option<(Compositor, HeadlessSurface)> {
    if std::env::var("TZE_HUD_SKIP_GPU_TESTS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
    {
        eprintln!("skipping GPU test: TZE_HUD_SKIP_GPU_TESTS=1");
        return None;
    }
    match common::new_headless_serialized(w, h).await {
        Ok(compositor) => {
            let surface = HeadlessSurface::new(&compositor.device, w, h);
            Some((compositor, surface))
        }
        Err(CompositorError::NoAdapter) => {
            eprintln!("skipping GPU test: no wgpu adapter available");
            None
        }
        Err(e) => panic!("unexpected compositor error: {e}"),
    }
}

/// Early-return from an async test when no GPU is available.
macro_rules! gpu_or_skip {
    ($expr:expr) => {
        match $expr {
            Some(v) => v,
            None => return,
        }
    };
}

/// Create a fresh `SceneGraph` with the full default zone registry
/// (subtitle, notification-area, status-bar, pip, ambient-background, alert-banner).
fn scene_with_defaults(w: f32, h: f32) -> SceneGraph {
    let mut scene = SceneGraph::new(w, h);
    scene.zone_registry = ZoneRegistry::with_defaults();
    scene
}

// ─── sRGB expected values ─────────────────────────────────────────────────────
//
// The compositor renders to Rgba8UnormSrgb; wgpu converts linear→sRGB on write.
// Approximate expected byte values for the test colors (linear → sRGB → u8):
//
//   Rgba { r:0.1, g:0.1, b:0.3, a:1.0 } (dark blue)
//     r: 1.055*0.1^(1/2.4)-0.055 ≈ 0.349 → ~89
//     g: same ≈ 89
//     b: 1.055*0.3^(1/2.4)-0.055 ≈ 0.581 → ~148
//
//   Rgba { r:1.0, g:0.0, b:0.0, a:1.0 } (red) → sRGB [255, 0, 0]
//   Rgba { r:0.0, g:0.0, b:1.0, a:1.0 } (blue) → sRGB [0, 0, 255]
//
//   Clear color (non-overlay): linear { r:0.05, g:0.05, b:0.1, a:1.0 }
//     r: 1.055*0.05^(1/2.4)-0.055 ≈ 0.242 → ~62 (approximate, closer to 56-65)
//     b: ≈ 0.349 → ~89
//
// Tolerances of ±8 accommodate software-renderer (llvmpipe / WARP) differences
// and minor platform-specific precision variation.

const TOLERANCE: u8 = 8;

// ─── Tests ────────────────────────────────────────────────────────────────────

/// Requirement: Ambient Background Zone Visual Contract
/// Scenario: "Solid color background fills the display"
///
/// Publish `SolidColor(Rgba { r:0.1, g:0.1, b:0.3, a:1.0 })` to the
/// `ambient-background` zone, render a frame, and assert that the rendered
/// pixels are distinctly dark blue (blue channel dominant, not the clear color).
///
/// The ambient-background zone is registered with `Relative { 0,0,1,1 }` geometry
/// so its SolidColor quad covers the full display surface.
#[tokio::test]
async fn test_ambient_background_solid_color_renders() {
    let (mut compositor, surface) = gpu_or_skip!(make_compositor_and_surface(64, 64).await);
    let mut scene = scene_with_defaults(64.0, 64.0);

    // Publish dark-blue solid color to ambient-background.
    scene
        .publish_to_zone(
            "ambient-background",
            ZoneContent::SolidColor(Rgba {
                r: 0.1,
                g: 0.1,
                b: 0.3,
                a: 1.0,
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for SolidColor on ambient-background zone");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    assert_eq!(
        pixels.len(),
        64 * 64 * 4,
        "pixel buffer must be 64×64×4 bytes"
    );

    // Sample corners and centre to confirm the ambient-background quad covers the full surface.
    // Expected sRGB bytes: R≈89, G≈89, B≈148 (linear 0.1,0.1,0.3 → sRGB gamma).
    //
    // Sampling only the centre would pass even if the zone quad were incorrectly
    // sized/positioned yet still covered the centre.  Corners + centre together
    // validate full-screen coverage.
    let sample_points: &[(u32, u32)] = &[
        (0, 0),   // top-left corner
        (63, 0),  // top-right corner
        (0, 63),  // bottom-left corner
        (63, 63), // bottom-right corner
        (32, 32), // centre
    ];

    for &(cx, cy) in sample_points {
        HeadlessSurface::assert_pixel_color(
            &pixels,
            64,
            cx,
            cy,
            [89, 89, 148, 255],
            TOLERANCE,
            "ambient-background SolidColor",
        )
        .unwrap_or_else(|e| panic!("pixel assertion failed at ({cx},{cy}): {e}"));
    }
}

/// Requirement: Ambient Background Zone Visual Contract
/// Scenario: "No publication yields transparent/clear background"
///
/// When no content has been published to `ambient-background`, the zone's
/// `active_publishes` list MUST be empty and the background layer MUST render
/// as the runtime clear color with no zone-specific visual output.
///
/// We verify both the scene state (empty active_publishes) and that the rendered
/// pixels match the clear color rather than any zone backdrop color.
#[tokio::test]
async fn test_ambient_background_no_publication_empty() {
    let (mut compositor, surface) = gpu_or_skip!(make_compositor_and_surface(64, 64).await);
    let mut scene = scene_with_defaults(64.0, 64.0);

    // Assert: no publications in zone before render.
    let ambient_publishes = scene
        .zone_registry
        .active_publishes
        .get("ambient-background");
    let is_empty = ambient_publishes.map(|v| v.is_empty()).unwrap_or(true);
    assert!(
        is_empty,
        "ambient-background active_publishes must be empty when no content has been published"
    );

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    assert_eq!(
        pixels.len(),
        64 * 64 * 4,
        "pixel buffer must be 64×64×4 bytes"
    );

    // With no zone publication, the centre pixel should match the runtime clear color.
    // Clear color: linear {r:0.05, g:0.05, b:0.1, a:1.0} → sRGB ≈ {R≈62, G≈62, B≈89}.
    // A published dark-blue zone would give ≈{R:89, G:89, B:148} — well above clear.
    let cx = 32u32;
    let cy = 32u32;
    HeadlessSurface::assert_pixel_color(
        &pixels,
        64,
        cx,
        cy,
        [62, 62, 89, 255],
        TOLERANCE,
        "clear color (no ambient-background publication)",
    )
    .expect("rendered pixel must match the runtime clear color when no ambient-background content is published");
}

/// Requirement: Ambient Background Latest-Wins Contention
/// Scenario: "New solid color replaces previous solid color"
///
/// Publish red, then blue, to `ambient-background`. After the second publish
/// the zone's `active_publishes` count MUST be 1 (Replace policy), and the
/// rendered background MUST show blue pixels (the second publication).
#[tokio::test]
async fn test_ambient_background_replacement_contention() {
    let (mut compositor, surface) = gpu_or_skip!(make_compositor_and_surface(64, 64).await);
    let mut scene = scene_with_defaults(64.0, 64.0);

    // First publish: red.
    scene
        .publish_to_zone(
            "ambient-background",
            ZoneContent::SolidColor(Rgba {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("first publish (red) must succeed");

    // Second publish: blue — must evict red under Replace policy.
    scene
        .publish_to_zone(
            "ambient-background",
            ZoneContent::SolidColor(Rgba {
                r: 0.0,
                g: 0.0,
                b: 1.0,
                a: 1.0,
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("second publish (blue) must succeed");

    // Scene state: Replace policy allows only one active publication.
    let pub_count = scene
        .zone_registry
        .active_publishes
        .get("ambient-background")
        .map(|v| v.len())
        .unwrap_or(0);
    assert_eq!(
        pub_count, 1,
        "ambient-background active publication count MUST be 1 after Replace (latest-wins); got {pub_count}"
    );

    // Render and verify only blue appears (not red).
    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Centre pixel must show pure blue (second publication); red must not bleed through.
    // linear (0,0,1) → sRGB blue ≈ 255; linear (1,0,0) → sRGB red = 255.
    let cx = 32u32;
    let cy = 32u32;
    HeadlessSurface::assert_pixel_color(
        &pixels,
        64,
        cx,
        cy,
        [0, 0, 255, 255],
        TOLERANCE,
        "blue replaces red",
    )
    .expect("rendered pixel must be pure blue — first (red) publish must not bleed through");
}

/// Requirement: Ambient Background Latest-Wins Contention
/// Scenario: "Rapid replacement under contention"
///
/// Publish 10 different solid colors in sequence (within a single frame interval).
/// After all publishes, the zone's `active_publishes` count MUST be exactly 1
/// (Replace policy), and the rendered frame MUST show only the last published color.
///
/// The last color is a saturated green `Rgba { r:0.0, g:1.0, b:0.0, a:1.0 }`.
#[tokio::test]
async fn test_ambient_background_rapid_replacement() {
    let (mut compositor, surface) = gpu_or_skip!(make_compositor_and_surface(64, 64).await);
    let mut scene = scene_with_defaults(64.0, 64.0);

    // Publish 10 different solid colors in rapid succession (no frame render between them).
    // The last published color (index 9) is saturated green.
    let colors: Vec<Rgba> = vec![
        Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }, // 0: red
        Rgba {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            a: 1.0,
        }, // 1: blue
        Rgba {
            r: 1.0,
            g: 1.0,
            b: 0.0,
            a: 1.0,
        }, // 2: yellow
        Rgba {
            r: 1.0,
            g: 0.0,
            b: 1.0,
            a: 1.0,
        }, // 3: magenta
        Rgba {
            r: 0.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        }, // 4: cyan
        Rgba {
            r: 0.5,
            g: 0.5,
            b: 0.5,
            a: 1.0,
        }, // 5: gray
        Rgba {
            r: 1.0,
            g: 0.5,
            b: 0.0,
            a: 1.0,
        }, // 6: orange
        Rgba {
            r: 0.5,
            g: 0.0,
            b: 0.5,
            a: 1.0,
        }, // 7: purple
        Rgba {
            r: 0.0,
            g: 0.5,
            b: 0.0,
            a: 1.0,
        }, // 8: dark green
        Rgba {
            r: 0.0,
            g: 1.0,
            b: 0.0,
            a: 1.0,
        }, // 9: bright green (last)
    ];

    for color in &colors {
        scene
            .publish_to_zone(
                "ambient-background",
                ZoneContent::SolidColor(*color),
                "test-agent",
                None,
                None,
                None,
            )
            .expect("rapid publish must succeed for ambient-background zone");
    }

    // Scene state: Replace policy evicts on every publish → exactly 1 active record.
    let pub_count = scene
        .zone_registry
        .active_publishes
        .get("ambient-background")
        .map(|v| v.len())
        .unwrap_or(0);
    assert_eq!(
        pub_count, 1,
        "ambient-background active publication count MUST be exactly 1 after 10 rapid Replace \
         publishes; got {pub_count}"
    );

    // Render a single frame and verify only the last color (bright green) is visible.
    // linear (0,1,0) → sRGB green ≈ 255.
    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // The last published color (index 9) is bright green: linear (0,1,0) → sRGB ≈ [0, 255, 0].
    // All prior colors (red, blue, yellow, …) must not bleed through.
    let cx = 32u32;
    let cy = 32u32;
    HeadlessSurface::assert_pixel_color(
        &pixels,
        64,
        cx,
        cy,
        [0, 255, 0, 255],
        TOLERANCE,
        "rapid replacement green",
    )
    .expect("rendered pixel must match the last published color (bright green) — earlier publishes must not bleed through");
}

// ─── sRGB expected values for z-order and StaticImage tests ──────────────────
//
// Z-order test background color: linear(0.0, 0.0, 0.5, 1.0) → sRGB blue channel:
//   b: 1.055 × 0.5^(1/2.4) − 0.055 ≈ 1.055 × 0.7492 − 0.055 ≈ 0.735 → ~188
// Content zone (opaque red): linear(1.0, 0.0, 0.0, 1.0) → sRGB [255, 0, 0]
//
// StaticImage placeholder: STATIC_IMAGE_PLACEHOLDER_COLOR = linear(0.3, 0.3, 0.3, 1.0):
//   1.055 × 0.3^(1/2.4) − 0.055 ≈ 1.055 × 0.6038 − 0.055 ≈ 0.584 → ~149
//
// All calibrated with ±8 tolerance for llvmpipe / software-renderer variance.

/// Expected sRGB bytes for the dark blue background zone color.
///
/// linear(0.0, 0.0, 0.5, 1.0): r=0, g=0, b≈188.
const DARK_BLUE_BG_EXPECTED: [u8; 4] = [0, 0, 188, 255];

/// Expected sRGB bytes for pure red content zone.
///
/// linear(1.0, 0.0, 0.0, 1.0): r=255, g=0, b=0.
const RED_CONTENT_EXPECTED: [u8; 4] = [255, 0, 0, 255];

/// Expected sRGB bytes for StaticImage warm-gray placeholder.
///
/// linear(0.3, 0.3, 0.3, 1.0): all channels ≈ 149.
const STATIC_IMAGE_PLACEHOLDER_EXPECTED: [u8; 4] = [149, 149, 149, 255];

// Surface size for z-order and StaticImage tests.
const SURFACE_W: u32 = 256;
const SURFACE_H: u32 = 256;

/// Requirement: Background Layer Z-Order
/// Scenario: "Content tile renders on top of background"
///
/// Publish `SolidColor(dark_blue)` to the `ambient-background` zone (Background layer),
/// and register a content-layer zone covering the centre 50% of the display with
/// `SolidColor(red)`. After rendering:
///   - Pixels at the display corners MUST be dark blue (ambient-background visible).
///   - The pixel at the display centre MUST be red (content zone occludes background).
///
/// This verifies that `LayerAttachment::Background` renders before `LayerAttachment::Content`,
/// so content tiles occlude the background zone where they overlap.
#[tokio::test]
async fn test_ambient_background_zorder_below_content_zones() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);

    // Use the default registry (includes ambient-background at LayerAttachment::Background).
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    // Register an additional content-layer zone covering the centre 50% of the display.
    // This zone uses ContentionPolicy::Replace so a single publish fills it.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "test-content-zone".to_owned(),
        description: "Centre content zone for z-order test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.25,
            y_pct: 0.25,
            width_pct: 0.50,
            height_pct: 0.50,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Replace,
        max_publishers: 1,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish dark blue to the background zone.
    scene
        .publish_to_zone(
            "ambient-background",
            ZoneContent::SolidColor(Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.5,
                a: 1.0,
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish dark blue to ambient-background must succeed");

    // Publish red to the content zone (centre 50%).
    scene
        .publish_to_zone(
            "test-content-zone",
            ZoneContent::SolidColor(Rgba {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish red to test-content-zone must succeed");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Corner pixels are outside the content zone: background (dark blue) must be visible.
    // The content zone covers x ∈ [64, 192), y ∈ [64, 192) for a 256×256 surface.
    // Corners at (0,0), (255,0), (0,255), (255,255) are well outside this region.
    let corners: &[(u32, u32)] = &[(0, 0), (255, 0), (0, 255), (255, 255)];
    for &(cx, cy) in corners {
        HeadlessSurface::assert_pixel_color(
            &pixels,
            SURFACE_W,
            cx,
            cy,
            DARK_BLUE_BG_EXPECTED,
            TOLERANCE,
            "corner pixel (background area) must be dark blue",
        )
        .unwrap_or_else(|e| {
            panic!("z-order test — background not visible at corner ({cx},{cy}): {e}")
        });
    }

    // Centre pixel (128, 128) is inside the content zone: red MUST occlude the background.
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SURFACE_W / 2,
        SURFACE_H / 2,
        RED_CONTENT_EXPECTED,
        TOLERANCE,
        "centre pixel (content zone area) must be red — content occludes background",
    )
    .unwrap_or_else(|e| {
        panic!("z-order test — content zone does not occlude background at centre: {e}")
    });
}

/// Requirement: Ambient Background Zone Visual Contract
/// Scenario: "StaticImage renders placeholder in v1"
///
/// Publish `ZoneContent::StaticImage(resource_id)` to the `ambient-background` zone.
/// The GPU texture upload pipeline is deferred to a future iteration; the compositor
/// MUST render a warm-gray placeholder quad (linear 0.3,0.3,0.3) instead.
///
/// After rendering, multiple sample points across the display MUST show the warm-gray
/// sRGB approximation (≈ [149, 149, 149]) within ±8 per channel, confirming that:
///   - The zone accepted the StaticImage publication (not rejected by validation).
///   - The placeholder quad was rendered full-screen (Background layer coverage).
///   - The `resource_id` was accepted (preserved in the publication record).
#[tokio::test]
async fn test_ambient_background_static_image_renders_placeholder() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    // Construct a ResourceId from deterministic test bytes (content-addressed).
    let resource_id = ResourceId::of(b"ambient-background-test-image");

    // Publish StaticImage — the ambient-background zone accepts ZoneMediaType::StaticImage.
    scene
        .publish_to_zone(
            "ambient-background",
            ZoneContent::StaticImage(resource_id),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish StaticImage to ambient-background must succeed");

    // Confirm the publication was recorded (resource_id preserved in active record).
    let publishes = scene
        .zone_registry
        .active_publishes
        .get("ambient-background")
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let pub_count = publishes.len();
    assert_eq!(
        pub_count, 1,
        "ambient-background must have exactly 1 active publication after StaticImage publish; got {pub_count}"
    );
    assert_eq!(
        publishes[0].content,
        ZoneContent::StaticImage(resource_id),
        "active publication must preserve the published resource_id"
    );

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // The placeholder color is STATIC_IMAGE_PLACEHOLDER_COLOR = linear(0.3, 0.3, 0.3).
    // sRGB ≈ [149, 149, 149]; all channels equal (neutral gray, no blue or red tint).
    //
    // Sample multiple points to confirm full-screen coverage.
    let sample_points: &[(u32, u32)] = &[
        (0, 0),     // top-left
        (255, 0),   // top-right
        (0, 255),   // bottom-left
        (255, 255), // bottom-right
        (128, 128), // centre
    ];

    for &(cx, cy) in sample_points {
        HeadlessSurface::assert_pixel_color(
            &pixels,
            SURFACE_W,
            cx,
            cy,
            STATIC_IMAGE_PLACEHOLDER_EXPECTED,
            TOLERANCE,
            "StaticImage ambient-background placeholder (warm-gray)",
        )
        .unwrap_or_else(|e| {
            panic!("StaticImage placeholder pixel assertion failed at ({cx},{cy}): {e}")
        });
    }
}

/// Requirement: Background Layer Z-Order
/// Scenario: "Background layer uses LayerAttachment::Background"
///
/// `ZoneRegistry::with_defaults()` MUST register the `ambient-background` zone with
/// `LayerAttachment::Background`. This is a pure scene-state assertion — no GPU or
/// rendering required.
///
/// This verifies that the zone definition contract is correct at the registry level,
/// independently of the pixel-based rendering tests above.
#[test]
fn test_ambient_background_layer_attachment_is_background() {
    let registry = ZoneRegistry::with_defaults();

    let zone = registry
        .zones
        .get("ambient-background")
        .expect("ZoneRegistry::with_defaults() must include an 'ambient-background' zone");

    assert_eq!(
        zone.layer_attachment,
        LayerAttachment::Background,
        "ambient-background zone MUST use LayerAttachment::Background so it renders \
         behind all content and chrome zones; got {:?}",
        zone.layer_attachment
    );
}

// ─── GPU texture rendering tests ────────────────────────────────────────────

/// When decoded RGBA bytes are registered for a StaticImage resource, the
/// compositor renders the actual image texture instead of the warm-gray
/// placeholder.
///
/// This test:
/// 1. Creates a solid-red 8x8 RGBA image programmatically
/// 2. Registers it via `compositor.register_image_bytes()`
/// 3. Publishes `ZoneContent::StaticImage(resource_id)` to ambient-background
/// 4. Asserts that rendered pixels are red, not warm-gray
#[tokio::test]
async fn test_ambient_background_static_image_renders_texture_when_bytes_registered() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    // Create a solid-red 8x8 RGBA image.
    let img_w: u32 = 8;
    let img_h: u32 = 8;
    let red_pixel: [u8; 4] = [255, 0, 0, 255]; // sRGB red
    let rgba_data: Vec<u8> = red_pixel.repeat((img_w * img_h) as usize);
    let resource_id = ResourceId::of(&rgba_data);

    // Register the decoded RGBA bytes with the compositor.
    compositor.register_image_bytes(
        resource_id,
        std::sync::Arc::from(rgba_data.as_slice()),
        img_w,
        img_h,
    );

    // Publish the StaticImage to ambient-background.
    scene
        .publish_to_zone(
            "ambient-background",
            ZoneContent::StaticImage(resource_id),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish StaticImage must succeed");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // The image is solid red. When rendered as a texture, centre pixels should be
    // red-dominant, NOT the warm-gray placeholder (~149,149,149).
    //
    // Note: The 8x8 image is upscaled to fill the 256x256 zone via bilinear filtering.
    // sRGB red [255,0,0] stored in Rgba8UnormSrgb is decoded to linear (1.0,0,0) by the
    // GPU, then the fragment shader outputs it, and the surface re-encodes to sRGB.
    // Expected ≈ [255, 0, 0, 255] ± tolerance.
    let centre_pixel = HeadlessSurface::pixel_at(&pixels, SURFACE_W, 128, 128);
    assert!(
        centre_pixel[0] > 200 && centre_pixel[1] < 30 && centre_pixel[2] < 30,
        "centre pixel should be red (from texture) not warm-gray placeholder; got {centre_pixel:?}"
    );
}

/// Regression test: non-square zone images must render as textures, not the
/// warm-gray placeholder.
///
/// The old code used a square-root heuristic (`(bytes.len() / 4).sqrt()`) to
/// determine image dimensions for `ZoneContent::StaticImage` zone publications.
/// This heuristic silently produces wrong dimensions for non-square images
/// (e.g. 16×8 = 128 pixels → sqrt(128) ≈ 11.3 → truncated to 11, which is
/// neither 16 nor 8), causing the byte-count validation inside
/// `ensure_image_texture` to fail and fall back to the placeholder.
///
/// The fix stores explicit `(width, height)` in `Compositor::image_dims` via
/// the updated `register_image_bytes(id, bytes, width, height)` API and uses
/// those dimensions in `ensure_scene_image_textures`.
#[tokio::test]
async fn test_ambient_background_static_image_non_square_renders_texture() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    // Non-square image: 16 wide × 8 tall (aspect ratio 2:1).
    // The old sqrt heuristic would compute sqrt(16*8) = sqrt(128) ≈ 11, which is
    // wrong for both dimensions, causing ensure_image_texture to reject it.
    let img_w: u32 = 16;
    let img_h: u32 = 8;
    let blue_pixel: [u8; 4] = [0, 0, 255, 255]; // sRGB blue
    let rgba_data: Vec<u8> = blue_pixel.repeat((img_w * img_h) as usize);
    let resource_id = ResourceId::of(&rgba_data);

    // Register with explicit dimensions — the fix under test.
    compositor.register_image_bytes(
        resource_id,
        std::sync::Arc::from(rgba_data.as_slice()),
        img_w,
        img_h,
    );

    scene
        .publish_to_zone(
            "ambient-background",
            ZoneContent::StaticImage(resource_id),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish non-square StaticImage must succeed");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // The image is solid blue. Centre pixels should be blue, NOT the warm-gray
    // placeholder (~149, 149, 149).
    let centre_pixel = HeadlessSurface::pixel_at(&pixels, SURFACE_W, 128, 128);
    assert!(
        centre_pixel[2] > 200 && centre_pixel[0] < 30 && centre_pixel[1] < 30,
        "non-square zone image: centre pixel should be blue (texture), not warm-gray; got {centre_pixel:?}"
    );
}

/// When no bytes are registered, the compositor falls back to the warm-gray
/// placeholder — even though `ZoneContent::StaticImage` is published.
/// This ensures backward compatibility with the pre-texture-rendering behavior.
#[tokio::test]
async fn test_ambient_background_static_image_falls_back_to_placeholder_without_bytes() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    // Use a ResourceId for which no bytes are registered.
    let resource_id = ResourceId::of(b"unregistered-image-data");

    scene
        .publish_to_zone(
            "ambient-background",
            ZoneContent::StaticImage(resource_id),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish StaticImage must succeed");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Should render the warm-gray placeholder.
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        128,
        128,
        STATIC_IMAGE_PLACEHOLDER_EXPECTED,
        TOLERANCE,
        "StaticImage without registered bytes should render warm-gray placeholder",
    )
    .unwrap_or_else(|e| panic!("placeholder fallback pixel assertion failed: {e}"));
}

// ─── Tile-node StaticImage rendering tests with fit modes ───────────────────

/// Helper: create a solid-color RGBA8 image of the given dimensions.
fn make_solid_rgba(width: u32, height: u32, r: u8, g: u8, b: u8, a: u8) -> Vec<u8> {
    let pixel = [r, g, b, a];
    pixel.repeat((width * height) as usize)
}

/// Helper: create a scene with a single tile containing a StaticImage node.
fn scene_with_static_image_tile(
    surface_w: f32,
    surface_h: f32,
    resource_id: ResourceId,
    img_w: u32,
    img_h: u32,
    fit_mode: ImageFitMode,
) -> SceneGraph {
    let mut scene = SceneGraph::new(surface_w, surface_h);
    // Register the resource ID so the scene graph allows refcounting.
    scene.register_resource(resource_id);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test-agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test-agent",
            lease_id,
            Rect::new(0.0, 0.0, surface_w, surface_h),
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
                data: NodeData::StaticImage(StaticImageNode {
                    resource_id,
                    width: img_w,
                    height: img_h,
                    decoded_bytes: (img_w as u64) * (img_h as u64) * 4,
                    fit_mode,
                    bounds: Rect::new(0.0, 0.0, surface_w, surface_h),
                }),
            },
        )
        .unwrap();
    scene
}

/// ImageFitMode::Fill — renders a solid-green 8x8 image stretched to fill
/// the entire 256x256 tile. All pixels should be green.
#[tokio::test]
async fn test_tile_static_image_fill_mode_renders_texture() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);

    let img_w = 8u32;
    let img_h = 8u32;
    let rgba_data = make_solid_rgba(img_w, img_h, 0, 255, 0, 255);
    let resource_id = ResourceId::of(&rgba_data);

    compositor.register_image_bytes(
        resource_id,
        std::sync::Arc::from(rgba_data.as_slice()),
        img_w,
        img_h,
    );

    let mut scene = scene_with_static_image_tile(
        SURFACE_W as f32,
        SURFACE_H as f32,
        resource_id,
        img_w,
        img_h,
        ImageFitMode::Fill,
    );

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Centre pixel should be green.
    let centre = HeadlessSurface::pixel_at(&pixels, SURFACE_W, 128, 128);
    assert!(
        centre[0] < 30 && centre[1] > 200 && centre[2] < 30,
        "Fill mode: centre pixel should be green; got {centre:?}"
    );
}

/// ImageFitMode::Contain — renders a wide 16x8 solid-blue image into a square
/// 256x256 tile. The image should be letterboxed (bars at top/bottom).
/// Centre should be blue; top-edge should be the tile background (not blue).
#[tokio::test]
async fn test_tile_static_image_contain_mode_letterboxes() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);

    let img_w = 16u32;
    let img_h = 8u32;
    let rgba_data = make_solid_rgba(img_w, img_h, 0, 0, 255, 255);
    let resource_id = ResourceId::of(&rgba_data);

    compositor.register_image_bytes(
        resource_id,
        std::sync::Arc::from(rgba_data.as_slice()),
        img_w,
        img_h,
    );

    let mut scene = scene_with_static_image_tile(
        SURFACE_W as f32,
        SURFACE_H as f32,
        resource_id,
        img_w,
        img_h,
        ImageFitMode::Contain,
    );

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // For a 16:8 (2:1) image in a 256x256 square:
    // Contain: width fills 256, height = 256/2 = 128, centered vertically.
    // Letterbox bars: y=0..64 and y=192..256 should be tile background.
    // Image region: y=64..192, all blue.

    let centre = HeadlessSurface::pixel_at(&pixels, SURFACE_W, 128, 128);
    assert!(
        centre[2] > 200 && centre[0] < 30 && centre[1] < 30,
        "Contain mode: centre pixel (in image region) should be blue; got {centre:?}"
    );

    // Letterbox bar (y=30, below the chrome drag handle at y=0..8) should be
    // tile background (dark), not blue.  The letterbox region is y=0..64;
    // y=30 is safely below the drag handle and still in the letterbox zone.
    let top = HeadlessSurface::pixel_at(&pixels, SURFACE_W, 128, 30);
    assert!(
        top[2] < 100,
        "Contain mode: top letterbox pixel should not be blue; got {top:?}"
    );
}

/// ImageFitMode::Cover — renders a tall 8x16 solid-magenta image into a square
/// 256x256 tile. The image should be cropped (sides cut off) to fill.
/// All visible pixels should be magenta.
#[tokio::test]
async fn test_tile_static_image_cover_mode_fills_completely() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);

    let img_w = 8u32;
    let img_h = 16u32;
    let rgba_data = make_solid_rgba(img_w, img_h, 255, 0, 255, 255);
    let resource_id = ResourceId::of(&rgba_data);

    compositor.register_image_bytes(
        resource_id,
        std::sync::Arc::from(rgba_data.as_slice()),
        img_w,
        img_h,
    );

    let mut scene = scene_with_static_image_tile(
        SURFACE_W as f32,
        SURFACE_H as f32,
        resource_id,
        img_w,
        img_h,
        ImageFitMode::Cover,
    );

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Cover fills the entire destination. Since the image is solid magenta,
    // all pixels should be magenta regardless of cropping.
    let centre = HeadlessSurface::pixel_at(&pixels, SURFACE_W, 128, 128);
    assert!(
        centre[0] > 200 && centre[1] < 30 && centre[2] > 200,
        "Cover mode: centre pixel should be magenta; got {centre:?}"
    );

    // Corner should also be magenta (Cover fills everything).
    let corner = HeadlessSurface::pixel_at(&pixels, SURFACE_W, 2, 2);
    assert!(
        corner[0] > 200 && corner[1] < 30 && corner[2] > 200,
        "Cover mode: corner pixel should also be magenta; got {corner:?}"
    );
}

/// ImageFitMode::ScaleDown — renders a small 4x4 solid-yellow image into a
/// 256x256 tile. Since 4 < 256, the image should be rendered at native 4x4
/// size centered in the tile, with the tile background visible around it.
#[tokio::test]
async fn test_tile_static_image_scale_down_mode_native_size() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);

    let img_w = 4u32;
    let img_h = 4u32;
    let rgba_data = make_solid_rgba(img_w, img_h, 255, 255, 0, 255);
    let resource_id = ResourceId::of(&rgba_data);

    compositor.register_image_bytes(
        resource_id,
        std::sync::Arc::from(rgba_data.as_slice()),
        img_w,
        img_h,
    );

    let mut scene = scene_with_static_image_tile(
        SURFACE_W as f32,
        SURFACE_H as f32,
        resource_id,
        img_w,
        img_h,
        ImageFitMode::ScaleDown,
    );

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // ScaleDown with 4x4 image in 256x256 dest: renders at native 4x4 size, centred.
    // Centre of image: pixel (128,128) should be yellow.
    // (The 4x4 image spans x=126..130, y=126..130)
    let centre = HeadlessSurface::pixel_at(&pixels, SURFACE_W, 128, 128);
    assert!(
        centre[0] > 200 && centre[1] > 200 && centre[2] < 30,
        "ScaleDown mode: centre pixel should be yellow (image at native size); got {centre:?}"
    );

    // Far corner (0,0) should be tile background (dark), not yellow.
    let corner = HeadlessSurface::pixel_at(&pixels, SURFACE_W, 0, 0);
    assert!(
        corner[0] < 100 && corner[1] < 100,
        "ScaleDown mode: far corner should be tile background, not yellow; got {corner:?}"
    );
}
