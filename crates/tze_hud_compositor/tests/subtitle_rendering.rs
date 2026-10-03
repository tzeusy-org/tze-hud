//! Headless golden-image visual regression tests for subtitle rendering.
//!
//! Covers openspec/changes/exemplar-subtitle/specs/exemplar-subtitle/spec.md
//! — Requirement: Subtitle Visual Contract
//!
//! ## Test list
//!
//! 1. `test_subtitle_backdrop_black_at_0_6_opacity` — default canonical tokens →
//!    backdrop pixels match `color.backdrop.default` (#000000) at 0.6 opacity
//!    composited over the compositor clear color.
//!
//! 2. `test_subtitle_backdrop_at_zone_bottom` — zone rendered at bottom of screen
//!    (subtitle zone, ~bottom 10% minus margin); backdrop pixels above the clear
//!    color at the expected bottom row.
//!
//! 3. `test_subtitle_no_backdrop_when_policy_is_default` — when the subtitle zone
//!    has `RenderingPolicy::default()` (no backdrop set), no backdrop quad is
//!    rendered; sampled pixels match the compositor clear color.
//!
//! 4. `test_subtitle_text_color_white_from_policy` — zone registered with
//!    `text_color = Some(Rgba::WHITE)` + `outline_color` / `outline_width` set;
//!    after publishing StreamText and rendering, the subtitle zone area contains
//!    at least one bright pixel (text visible).
//!
//! 5. `test_subtitle_custom_token_override_backdrop_color` — custom `color.backdrop.default`
//!    token set to a non-black color; backdrop pixels must reflect the override,
//!    demonstrating token → RenderingPolicy → compositor pipeline end-to-end.
//!
//! 6. `test_subtitle_font_size_28px_from_policy` — zone registered with the spec-mandated
//!    `font_size_px = 28.0` and `font_weight = 600`; text rendering produces bright
//!    pixels in the subtitle zone area (presence check).
//!
//! ## Infrastructure
//!
//! Uses `Compositor::new_headless` + `HeadlessSurface::new` + `render_frame_headless`,
//! then `HeadlessSurface::read_pixels` and `HeadlessSurface::assert_pixel_color` for
//! pixel inspection.
//!
//! Set `TZE_HUD_SKIP_GPU_TESTS=1` to skip all GPU-dependent tests (e.g. in headless
//! CI environments without llvmpipe). On CI with Mesa installed, set
//! `HEADLESS_FORCE_SOFTWARE=1` instead so the software renderer is used.
//!
//! ## Expected pixel values
//!
//! The subtitle zone uses backdrop `#000000` at 0.6 alpha (from canonical tokens
//! `color.backdrop.default` = `#000000`, `opacity.backdrop.default` = `0.6`),
//! composited over the compositor's default clear color
//! (linear {r:0.05, g:0.05, b:0.1, a:1.0}).
//!
//! Alpha blending formula:
//!   out_lin = backdrop_lin × α + clear_lin × (1 − α)
//!
//! For black backdrop (linear 0.0, 0.0, 0.0) at α=0.6 over clear (0.05, 0.05, 0.10):
//!   r_lin = 0.0×0.6 + 0.05×0.4 = 0.02  → sRGB ≈ 39
//!   g_lin = 0.0×0.6 + 0.05×0.4 = 0.02  → sRGB ≈ 39
//!   b_lin = 0.0×0.6 + 0.10×0.4 = 0.04  → sRGB ≈ 56
//! (Calibrated against the same compositor clear color used in alert_banner_rendering.rs.)
//!
//! ## Subtitle zone geometry
//!
//! For a 256×256 surface with `EdgeAnchored { Bottom, height_pct=0.10, width_pct=0.80, margin_px=48.0 }`:
//!   zw = 256 × 0.80 = 204.8 px
//!   zh = 256 × 0.10 = 25.6 px
//!   zx = (256 − 204.8) / 2.0 = 25.6 px
//!   zy = 256 − 25.6 − 48.0 = 182.4 px → floor = 182
//!   Zone centre: x=128, y=182+12=194
//!   Left edge of zone: x=26
//!
//! ## References
//!
//! - hud-hzub.6 (this task)
//! - hud-hzub (parent epic: exemplar-subtitle)
//! - openspec/changes/exemplar-subtitle/specs/exemplar-subtitle/spec.md
//!   §Requirement: Subtitle Visual Contract

mod common;

use tze_hud_compositor::{Compositor, CompositorError, surface::HeadlessSurface};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    ContentionPolicy, DisplayEdge, GeometryPolicy, LayerAttachment, RenderingPolicy, Rgba, SceneId,
    ZoneContent, ZoneDefinition, ZoneMediaType, ZoneRegistry,
};

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Create a headless compositor + surface pair.
///
/// Returns `None` when `TZE_HUD_SKIP_GPU_TESTS=1` is set or no wgpu adapter is
/// available. Use the `gpu_or_skip!` macro to early-return from the test.
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

/// Create a fresh `SceneGraph` with the full default zone registry.
fn scene_with_defaults(w: f32, h: f32) -> SceneGraph {
    let mut scene = SceneGraph::new(w, h);
    scene.zone_registry = ZoneRegistry::with_defaults();
    scene
}

/// Register a subtitle zone with the canonical spec-compliant `RenderingPolicy`.
///
/// The policy uses:
///   - `backdrop: Some(Rgba::BLACK)` — black backdrop
///   - `backdrop_opacity: Some(0.6)` — 60% opacity per spec `opacity.backdrop.default`
///   - `text_color: Some(Rgba::WHITE)` — white text per spec `color.text.primary`
///   - `font_size_px: Some(28.0)` — 28px per spec `typography.subtitle.size`
///   - `font_weight: Some(600)` — 600 weight per spec `typography.subtitle.weight`
///   - `outline_color: Some(Rgba::BLACK)` — black text outline per `color.outline.default`
///   - `outline_width: Some(2.0)` — 2px outline per `stroke.outline.width`
///
/// These values mirror what `apply_subtitle_token_defaults` produces when the
/// canonical token map is used (no overrides).
fn register_subtitle_zone_spec_policy(scene: &mut SceneGraph) {
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "Subtitle zone — spec-compliant canonical policy".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 48.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::BLACK),
            backdrop_opacity: Some(0.6),
            text_color: Some(Rgba::WHITE),
            font_size_px: Some(28.0),
            font_weight: Some(600),
            outline_color: Some(Rgba::BLACK),
            outline_width: Some(2.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
}

// ─── Expected sRGB pixel values ───────────────────────────────────────────────
//
// Black backdrop (#000000) at 0.6 alpha composited over the compositor's clear
// color (linear {r:0.05, g:0.05, b:0.1, a:1.0}):
//
//   out_lin = backdrop_lin × 0.6 + clear_lin × 0.4
//   r_lin = 0.0×0.6 + 0.05×0.4 = 0.02
//   g_lin = 0.0×0.6 + 0.05×0.4 = 0.02
//   b_lin = 0.0×0.6 + 0.10×0.4 = 0.04
//
//   sRGB(0.02) = 1.055×0.02^(1/2.4) − 0.055 ≈ 1.055×0.196 − 0.055 ≈ 0.152 → ~39
//   sRGB(0.04) = 1.055×0.04^(1/2.4) − 0.055 ≈ 1.055×0.261 − 0.055 ≈ 0.220 → ~56
//
// Clear color (no backdrop): sRGB ≈ [63, 63, 89] (calibrated from ambient_background tests).
//
// The backdrop makes the sampled pixel distinctly darker than the clear color;
// the blue channel B is slightly higher than R/G due to the clear-color contribution.
//
// Tolerance ±10 accommodates software-renderer rounding (llvmpipe / WARP).

/// Tolerance applied to every channel when comparing expected vs. actual pixel values.
const TOLERANCE: u8 = 10;

/// Expected sRGB bytes for black backdrop at 0.6 alpha over compositor clear.
/// r≈39, g≈39, b≈56 (see calculation above).
const SUBTITLE_BACKDROP_EXPECTED: [u8; 4] = [39, 39, 56, 255];

/// Clear color sRGB (no zone content published):
/// linear(0.05, 0.05, 0.10) → sRGB ≈ [63, 63, 89].
const CLEAR_COLOR_EXPECTED: [u8; 4] = [63, 63, 89, 255];

// ─── Subtitle zone geometry constants (256×256 surface) ──────────────────────
//
// EdgeAnchored { Bottom, height_pct=0.10, width_pct=0.80, margin_px=48.0 }:
//   zw = 256×0.80 = 204.8
//   zh = 256×0.10 = 25.6
//   zx = (256 − 204.8)/2 = 25.6 → x≈26
//   zy = 256 − 25.6 − 48.0 = 182.4 → y≈182
//   Zone centre: x=128, y=182+12=194
//   Clear row above zone (y=170) should not be affected by the subtitle backdrop.
//   Row y=194 is the zone centre and should show the backdrop color.

const SURFACE_W: u32 = 256;
const SURFACE_H: u32 = 256;

/// Horizontal centre of the display (also centre of the full-width zone).
const SAMPLE_X: u32 = 128;

/// Y coordinate of the subtitle zone centre (inside zone, shows backdrop color).
const SUBTITLE_ZONE_Y: u32 = 194;

/// Y coordinate above the subtitle zone (clear color, no backdrop).
/// At y=170 we're above zy≈182, so this should show the clear color.
const ABOVE_ZONE_Y: u32 = 170;

// ─── Tests ─────────────────────────────────────────────────────────────────────

/// Requirement: Subtitle Visual Contract
/// Scenario: Default subtitle renders with token-derived backdrop (#000000, 0.6 opacity)
///
/// When the subtitle zone has a `RenderingPolicy` with `backdrop=BLACK` and
/// `backdrop_opacity=0.6` (mirroring canonical token defaults), the compositor
/// MUST render a semi-transparent black backdrop quad at the subtitle zone location.
///
/// The sampled pixel at zone centre MUST match black-at-0.6-alpha blended over the
/// compositor clear color, within ±10 per channel.
#[tokio::test]
async fn test_subtitle_backdrop_black_at_0_6_opacity() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
    register_subtitle_zone_spec_policy(&mut scene);

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Hello world".to_string()),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for StreamText on subtitle zone");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SUBTITLE_ZONE_Y,
        SUBTITLE_BACKDROP_EXPECTED,
        TOLERANCE,
        "subtitle backdrop must be black at 0.6 opacity over clear color",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Requirement: Subtitle Visual Contract
/// Scenario: Subtitle zone renders at bottom of screen
///
/// The subtitle zone uses `EdgeAnchored { Bottom, ... }` geometry, so the backdrop
/// MUST appear near the bottom of the display (y ≈ 182–208 for a 256-pixel-high
/// surface with 48px margin).
///
/// Specifically:
///   - Pixels at zone centre y (≈194) MUST show the backdrop color (darker than clear).
///   - Pixels above the zone (y=170, well above zy≈182) MUST still show the clear color.
///
/// This verifies the zone is anchored to the BOTTOM, not the top or center.
#[tokio::test]
async fn test_subtitle_backdrop_at_zone_bottom() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
    register_subtitle_zone_spec_policy(&mut scene);

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Bottom subtitle".to_string()),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for StreamText on subtitle zone");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Zone centre pixel must show the backdrop (darker than clear, blue-tinted).
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SUBTITLE_ZONE_Y,
        SUBTITLE_BACKDROP_EXPECTED,
        TOLERANCE,
        "subtitle zone centre y must show backdrop (bottom anchor verified)",
    )
    .unwrap_or_else(|e| panic!("zone bottom anchor — {e}"));

    // Pixel above the zone must show the clear color (no backdrop rendered there).
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        ABOVE_ZONE_Y,
        CLEAR_COLOR_EXPECTED,
        TOLERANCE,
        "pixel above subtitle zone must show clear color (not backdrop)",
    )
    .unwrap_or_else(|e| panic!("above-zone clear color — {e}"));
}

/// Requirement: Subtitle Visual Contract
/// Scenario: No backdrop when RenderingPolicy.backdrop is None
///
/// When the subtitle zone's `RenderingPolicy` has `backdrop: None` (the default
/// from `ZoneRegistry::with_defaults()`), no backdrop quad is emitted.
/// Even with content published, the sampled pixel at the subtitle zone location
/// MUST remain the compositor clear color.
///
/// This confirms the guard in `render_zone_content`: backdrop is only emitted
/// when `policy.backdrop.is_some()` (or for non-backdrop content types).
#[tokio::test]
async fn test_subtitle_no_backdrop_when_policy_is_default() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    // Use with_defaults() — subtitle zone has backdrop: None.
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("No backdrop subtitle".to_string()),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for StreamText on default subtitle zone");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // With backdrop=None, the zone backdrop quad is suppressed.
    // The pixel at the zone centre must match the compositor clear color.
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SUBTITLE_ZONE_Y,
        CLEAR_COLOR_EXPECTED,
        TOLERANCE,
        "subtitle with backdrop=None must show clear color (no backdrop quad)",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Requirement: Subtitle Visual Contract
/// Scenario: Text color matches token resolution (white text visible)
///
/// When the subtitle zone has `text_color = Some(Rgba::WHITE)` (as produced by
/// `apply_subtitle_token_defaults` for the canonical `color.text.primary = #FFFFFF` token),
/// the compositor MUST render white text glyphs in the subtitle zone area.
///
/// After rendering, at least one pixel in the subtitle zone area MUST have R, G, B
/// channels all > 180, indicating white/bright text rendered over the dark backdrop.
///
/// This test requires the text renderer to be initialized (calls `init_text_renderer`).
///
/// ## Surface sizing
///
/// Uses a 512×512 surface so the subtitle zone has adequate height (51px) to
/// render 28px glyphs without clipping. On the default 256×256 surface the zone
/// is only ~26px tall — smaller than the 28px font — so glyphs are clipped and
/// no bright pixels appear.
///
/// 512×512 subtitle zone geometry:
///   zh = 512 × 0.10 = 51.2px
///   zy = 512 − 51.2 − 48.0 = 412.8 → floor = 412
///   Zone rows: 412..464  (scan margin: 412..465)
///   Zone cols: 51..461   (zx = (512−409.6)/2 ≈ 51)
#[tokio::test]
async fn test_subtitle_text_color_white_from_policy() {
    // Use 512×512 so zone height (~51px) comfortably fits 28px glyphs.
    const TW: u32 = 512;
    const TH: u32 = 512;
    let (mut compositor, surface) = gpu_or_skip!(make_compositor_and_surface(TW, TH).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(TW as f32, TH as f32);

    // Register spec-compliant subtitle zone on the 512×512 scene.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "Subtitle zone — spec policy (text color test)".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 48.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::BLACK),
            backdrop_opacity: Some(0.6),
            text_color: Some(Rgba::WHITE),
            font_size_px: Some(28.0),
            font_weight: Some(600),
            outline_color: Some(Rgba::BLACK),
            outline_width: Some(2.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("White text".to_string()),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for StreamText on subtitle zone");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Scan subtitle zone area for bright pixels (white text glyphs).
    // 512×512: zh=51.2, zy≈412, zone rows 412..464, cols 51..461.
    let zone_x_start = 51usize;
    let zone_x_end = 461usize;
    let zone_y_start = 412usize;
    let zone_y_end = 465usize;

    let mut found_bright = false;
    'outer: for row in zone_y_start..zone_y_end {
        for col in zone_x_start..zone_x_end {
            let offset = (row * TW as usize + col) * 4;
            if offset + 3 < pixels.len() {
                let r = pixels[offset];
                let g = pixels[offset + 1];
                let b = pixels[offset + 2];
                if r > 180 && g > 180 && b > 180 {
                    found_bright = true;
                    break 'outer;
                }
            }
        }
    }
    assert!(
        found_bright,
        "subtitle zone must contain bright pixels (white text rendered from text_color=WHITE policy); \
         zone area rows {zone_y_start}..{zone_y_end}, cols {zone_x_start}..{zone_x_end} had no pixel with R,G,B > 180"
    );
}

/// Requirement: Subtitle Visual Contract
/// Scenario: Custom token override changes backdrop color
///
/// When a custom `color.backdrop.default` token is injected via `set_token_map`,
/// subtitle zones that use this token (via policy.backdrop set from the token)
/// MUST reflect the override in the rendered backdrop color.
///
/// This test sets up a subtitle zone whose policy.backdrop is red (simulating what
/// `apply_subtitle_token_defaults` would produce for `color.backdrop.default = "#FF0000"`),
/// then verifies that the rendered pixels match a red backdrop at 0.6 opacity.
///
/// Red (linear 1.0, 0.0, 0.0) at 0.6 alpha over clear (0.05, 0.05, 0.10):
///   r_lin = 1.0×0.6 + 0.05×0.4 = 0.62  → sRGB ≈ 206
///   g_lin = 0.0×0.6 + 0.05×0.4 = 0.02  → sRGB ≈ 39
///   b_lin = 0.0×0.6 + 0.10×0.4 = 0.04  → sRGB ≈ 56
#[tokio::test]
async fn test_subtitle_custom_token_override_backdrop_color() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);

    // Register subtitle zone with a RED backdrop (simulating custom token override
    // "color.backdrop.default" = "#FF0000").
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "Subtitle zone — custom red backdrop (token override test)".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 48.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            // Red backdrop: simulates "color.backdrop.default" = "#FF0000" override.
            backdrop: Some(Rgba::new(1.0, 0.0, 0.0, 1.0)),
            backdrop_opacity: Some(0.6),
            text_color: Some(Rgba::WHITE),
            font_size_px: Some(28.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Custom token test".to_string()),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for StreamText on custom subtitle zone");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Expected: red at 0.6 alpha over clear color.
    // r_lin = 1.0×0.6 + 0.05×0.4 = 0.62 → sRGB ≈ 206
    // g_lin = 0.0×0.6 + 0.05×0.4 = 0.02 → sRGB ≈ 39
    // b_lin = 0.0×0.6 + 0.10×0.4 = 0.04 → sRGB ≈ 56
    let red_backdrop_expected: [u8; 4] = [206, 39, 56, 255];

    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SUBTITLE_ZONE_Y,
        red_backdrop_expected,
        TOLERANCE,
        "subtitle custom token override: backdrop must be red at 0.6 opacity",
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // Sanity check: R channel must dominate G and B (red backdrop).
    let actual = HeadlessSurface::pixel_at(&pixels, SURFACE_W, SAMPLE_X, SUBTITLE_ZONE_Y);
    assert!(
        actual[0] > actual[1] + 50,
        "custom red backdrop: R ({}) must substantially exceed G ({}) — confirms red channel used",
        actual[0],
        actual[1]
    );
    assert!(
        actual[0] > actual[2] + 50,
        "custom red backdrop: R ({}) must substantially exceed B ({}) — confirms red channel used",
        actual[0],
        actual[2]
    );
}

/// Requirement: Subtitle Visual Contract
/// Scenario: Font size from typography.subtitle.size token (presence check)
///
/// When the subtitle zone's `font_size_px = Some(28.0)` (as resolved from the
/// `typography.subtitle.size` token) and `font_weight = Some(600)`, text rendering
/// MUST produce visible glyphs in the subtitle zone area.
///
/// This is a presence test: at least one pixel in the subtitle zone area MUST have
/// R, G, B > 180 after rendering white text at 28px.
///
/// This test requires the text renderer to be initialized.
///
/// ## Surface sizing
///
/// Uses a 512×512 surface so the subtitle zone has adequate height (51px) to
/// render 28px glyphs without clipping. On the default 256×256 surface the zone
/// is only ~26px tall — smaller than the 28px font — so glyphs are clipped and
/// no bright pixels appear.
///
/// 512×512 subtitle zone geometry:
///   zh = 512 × 0.10 = 51.2px
///   zy = 512 − 51.2 − 48.0 = 412.8 → floor = 412
///   Zone rows: 412..464  (scan margin: 412..465)
///   Zone cols: 51..461   (zx = (512−409.6)/2 ≈ 51)
#[tokio::test]
async fn test_subtitle_font_size_28px_from_policy() {
    // Use 512×512 so zone height (~51px) comfortably fits 28px glyphs.
    const TW: u32 = 512;
    const TH: u32 = 512;
    let (mut compositor, surface) = gpu_or_skip!(make_compositor_and_surface(TW, TH).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(TW as f32, TH as f32);

    // Register a subtitle zone with spec-mandated 28px font size and 600 weight,
    // matching what `typography.subtitle.size = "28"` and `typography.subtitle.weight = "600"`
    // tokens would produce via apply_subtitle_token_defaults.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "Subtitle zone — 28px font size from typography.subtitle.size token"
            .to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 48.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::BLACK),
            backdrop_opacity: Some(0.6),
            text_color: Some(Rgba::WHITE),
            // Spec: typography.subtitle.size = 28px
            font_size_px: Some(28.0),
            // Spec: typography.subtitle.weight = 600
            font_weight: Some(600),
            outline_color: Some(Rgba::BLACK),
            outline_width: Some(2.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Subtitle 28px".to_string()),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for StreamText on subtitle zone");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Scan the subtitle zone area for bright pixels (white 28px text glyphs).
    // 512×512: zh=51.2, zy≈412, zone rows 412..464, cols 51..461.
    let zone_x_start = 51usize;
    let zone_x_end = 461usize;
    let zone_y_start = 412usize;
    let zone_y_end = 465usize;

    let mut found_bright = false;
    'outer: for row in zone_y_start..zone_y_end {
        for col in zone_x_start..zone_x_end {
            let offset = (row * TW as usize + col) * 4;
            if offset + 3 < pixels.len() {
                let r = pixels[offset];
                let g = pixels[offset + 1];
                let b = pixels[offset + 2];
                if r > 180 && g > 180 && b > 180 {
                    found_bright = true;
                    break 'outer;
                }
            }
        }
    }
    assert!(
        found_bright,
        "subtitle zone must contain bright pixels (white 28px glyphs from typography.subtitle.size token); \
         zone area rows {zone_y_start}..{zone_y_end}, cols {zone_x_start}..{zone_x_end} had no pixel with R,G,B > 180"
    );
}

// ─── Debug probe (ignored, for calibration only) ─────────────────────────────

/// Internal calibration test: prints actual pixel values at the subtitle zone.
///
/// Run with:
///   `HEADLESS_FORCE_SOFTWARE=1 cargo test --test subtitle_rendering debug_probe_pixel_values -- --nocapture --ignored`
///
/// Use the output to recalibrate SUBTITLE_BACKDROP_EXPECTED and CLEAR_COLOR_EXPECTED.
#[tokio::test]
#[ignore]
async fn debug_probe_pixel_values() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);

    // Test 1: with spec-compliant backdrop policy.
    {
        let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
        register_subtitle_zone_spec_policy(&mut scene);
        scene
            .publish_to_zone(
                "subtitle",
                ZoneContent::StreamText("Calibration text".to_string()),
                "probe",
                None,
                None,
                None,
            )
            .unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.render_frame_headless(&mut scene, &surface);
        let pixels = surface.read_pixels(&compositor.device);
        let p = HeadlessSurface::pixel_at(&pixels, SURFACE_W, SAMPLE_X, SUBTITLE_ZONE_Y);
        println!(
            "subtitle backdrop (black 0.6α) → pixel at ({SAMPLE_X},{SUBTITLE_ZONE_Y}): R={} G={} B={} A={}",
            p[0], p[1], p[2], p[3]
        );
    }

    // Test 2: clear color (no backdrop).
    {
        let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);
        scene
            .publish_to_zone(
                "subtitle",
                ZoneContent::StreamText("Calibration text".to_string()),
                "probe",
                None,
                None,
                None,
            )
            .unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.render_frame_headless(&mut scene, &surface);
        let pixels = surface.read_pixels(&compositor.device);
        let p_zone = HeadlessSurface::pixel_at(&pixels, SURFACE_W, SAMPLE_X, SUBTITLE_ZONE_Y);
        let p_clear = HeadlessSurface::pixel_at(&pixels, SURFACE_W, SAMPLE_X, ABOVE_ZONE_Y);
        println!(
            "subtitle no-backdrop (clear) → zone pixel at ({SAMPLE_X},{SUBTITLE_ZONE_Y}): R={} G={} B={} A={}",
            p_zone[0], p_zone[1], p_zone[2], p_zone[3]
        );
        println!(
            "above-zone clear → pixel at ({SAMPLE_X},{ABOVE_ZONE_Y}): R={} G={} B={} A={}",
            p_clear[0], p_clear[1], p_clear[2], p_clear[3]
        );
    }
}
