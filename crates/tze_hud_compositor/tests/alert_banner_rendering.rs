//! Integration tests for alert-banner rendering pipeline — backdrop colors and stacking.
//!
//! Covers openspec/changes/exemplar-alert-banner/specs/exemplar-alert-banner/spec.md
//! — Requirement: Alert-Banner Integration Test Suite
//!
//! ## Test list
//!
//! 1. `test_alert_banner_urgency0_info_backdrop` — urgency=0 → backdrop pixels match
//!    `color.severity.info` (#4A9EFF, 0.9 alpha over clear).
//!
//! 2. `test_alert_banner_urgency1_info_backdrop` — urgency=1 → same info-blue backdrop as
//!    urgency=0, proving both map to the same severity token.
//!
//! 3. `test_alert_banner_urgency2_warning_backdrop` — urgency=2 → backdrop pixels match
//!    `color.severity.warning` (#FFB800, 0.9 alpha over clear).
//!
//! 4. `test_alert_banner_urgency3_critical_backdrop` — urgency=3 → backdrop pixels match
//!    `color.severity.critical` (#FF0000, 0.9 alpha over clear).
//!
//! 5. `test_alert_banner_stacking_order` — info + warning + critical published simultaneously
//!    → critical (red) at top, warning (amber) middle, info (blue) at bottom.
//!
//! 6. `test_alert_banner_stream_text_uses_default_policy_color` — StreamText published to
//!    alert-banner → backdrop uses the zone's default RenderingPolicy color, not severity
//!    color mapping.
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
//! The alert-banner zone renders severity-colored backdrop quads at 0.9 alpha over the
//! compositor's default clear color (linear {r:0.05, g:0.05, b:0.1, a:1.0}).
//!
//! Severity colors (linear sRGB constants, per spec §Canonical Token Schema):
//!   color.severity.info     → SEVERITY_INFO     = linear(0.078, 0.384, 1.0)
//!   color.severity.warning  → SEVERITY_WARNING   = linear(1.0, 0.722, 0.0)
//!   color.severity.critical → SEVERITY_CRITICAL  = linear(1.0, 0.0, 0.0)
//!
//! At 0.9 alpha composited over the compositor's dark clear color
//! (linear {r:0.05, g:0.05, b:0.1, a:1.0}), calibrated from llvmpipe output:
//!   info     → sRGB ≈ (78, 160, 245)  [B-dominant; blue]
//!   warning  → sRGB ≈ (244, 211, 26)  [R/G-dominant, B small; amber]
//!   critical → sRGB ≈ (244, 15, 26)   [R-dominant; red]
//!
//! Tolerances of ±8 accommodate software-renderer (llvmpipe / WARP) rounding differences
//! and sRGB compositing precision variation across platforms.
//!
//! ## Alert-banner zone geometry
//!
//! For a 256×256 surface:
//!   - EdgeAnchored { Top, height_pct=0.06, width_pct=1.0, margin_px=0.0 }
//!   - Nominal zone height = 256 × 0.06 = 15.36px (unused for alert-banner)
//!   - Dynamic slot height = line_height(24*1.4=33.6) + 2*margin_vertical(0) + SLOT_BASELINE_GAP(4) = 37.6px
//!   - Single banner: y ∈ [0, 37.6), full width x ∈ [0, 256)
//!   - Sample centre: x=128, y=18 (mid-slot)
//!
//! ## References
//!
//! - hud-w3o6.5 (this task)
//! - hud-w3o6 (parent epic: exemplar-alert-banner)
//! - openspec/changes/exemplar-alert-banner/specs/exemplar-alert-banner/spec.md
//!   §Requirement: Alert-Banner Integration Test Suite

mod common;

use tze_hud_compositor::{Compositor, CompositorError, surface::HeadlessSurface};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{NotificationPayload, ZoneContent, ZoneRegistry};

// ─── Helpers ─────────────────────────────────────────────────────────────────

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

/// Create a fresh `SceneGraph` with the full default zone registry
/// (subtitle, notification-area, status-bar, pip, ambient-background, alert-banner).
fn scene_with_defaults(w: f32, h: f32) -> SceneGraph {
    let mut scene = SceneGraph::new(w, h);
    scene.zone_registry = ZoneRegistry::with_defaults();
    scene
}

// ─── Expected sRGB pixel values ──────────────────────────────────────────────
//
// The alert-banner zone uses SEVERITY_INFO/WARNING/CRITICAL linear colors at 0.9 alpha,
// composited via ALPHA_BLENDING over the compositor's clear color
// (linear {r:0.05, g:0.05, b:0.1, a:1.0}).
//
// Expected sRGB u8 values after blending and sRGB gamma encoding (calibrated from llvmpipe):
//   info     (linear 0.078,0.384,1.0 at 0.9α over clear) → sRGB ≈ (78, 160, 245)
//   warning  (linear 1.0,0.722,0.0  at 0.9α over clear) → sRGB ≈ (244, 211, 26)
//   critical (linear 1.0,0.0,0.0    at 0.9α over clear) → sRGB ≈ (244, 15, 26)
//
// Note: the default alert-banner backdrop for non-Notification content:
//   linear(0.1, 0.1, 0.16) at 0.9 alpha → sRGB ≈ (87, 87, 109) over clear.
//
// Tolerances of ±8 accommodate llvmpipe / software rasterizer variance.

/// Tolerance applied to every channel when comparing expected vs. actual pixel values.
/// Matches the tolerance used in other compositor integration tests (ambient_background, etc.).
const TOLERANCE: u8 = 8;

// Expected sRGB bytes for info backdrop (0.9 alpha).
//
// SEVERITY_INFO = linear(0.078, 0.384, 1.0) at alpha=0.9, blended over clear
// linear(0.05, 0.05, 0.1, 1.0):
//   r_lin = 0.078×0.9 + 0.05×0.1 = 0.0752  → sRGB ≈ 78
//   g_lin = 0.384×0.9 + 0.05×0.1 = 0.3506  → sRGB ≈ 160
//   b_lin = 1.0×0.9  + 0.1×0.1  = 0.91     → sRGB ≈ 245
// (Calibrated from llvmpipe output.)
const INFO_EXPECTED: [u8; 4] = [78, 160, 245, 255];

// Expected sRGB bytes for warning backdrop (0.9 alpha).
//
// SEVERITY_WARNING = linear(1.0, 0.722, 0.0) at alpha=0.9, blended over clear:
//   r_lin = 1.0×0.9  + 0.05×0.1 = 0.905   → sRGB ≈ 244
//   g_lin = 0.722×0.9 + 0.05×0.1 = 0.6548 → sRGB ≈ 211
//   b_lin = 0.0×0.9  + 0.1×0.1  = 0.01    → sRGB ≈ 26
// (Calibrated from llvmpipe output.)
const WARNING_EXPECTED: [u8; 4] = [244, 211, 26, 255];

// Expected sRGB bytes for critical backdrop (0.9 alpha).
//
// SEVERITY_CRITICAL = linear(1.0, 0.0, 0.0) at alpha=0.9, blended over clear:
//   r_lin = 1.0×0.9  + 0.05×0.1 = 0.905  → sRGB ≈ 244
//   g_lin = 0.0×0.9  + 0.05×0.1 = 0.005  → sRGB ≈ 15
//   b_lin = 0.0×0.9  + 0.1×0.1  = 0.01   → sRGB ≈ 26
// (Calibrated from llvmpipe output.)
const CRITICAL_EXPECTED: [u8; 4] = [244, 15, 26, 255];

// ─── Alert-banner geometry constants ─────────────────────────────────────────
//
// For a 256×256 surface with the default alert-banner rendering policy:
//   font_size_px = 24, line_height = 24 * 1.4 = 33.6
//   margin_vertical = 0.0, SLOT_BASELINE_GAP = 4
//   slot_h = 33.6 + 2*0 + 4 = 37.6px ≈ 38px
//
// Slot centres (one publication per slot):
//   slot 0 (top)    → y = 0    + 37.6/2 ≈ 18
//   slot 1 (middle) → y = 37.6 + 37.6/2 ≈ 56
//   slot 2 (bottom) → y = 75.2 + 37.6/2 ≈ 94

const SURFACE_W: u32 = 256;
const SURFACE_H: u32 = 256;

/// Centre x-coordinate for sampling (full-width zone → middle of display width).
const SAMPLE_X: u32 = SURFACE_W / 2;

/// Centre y-coordinate of slot 0 (topmost slot, highest-severity).
const SLOT0_Y: u32 = 18;
/// Centre y-coordinate of slot 1.
const SLOT1_Y: u32 = 56;
/// Centre y-coordinate of slot 2 (bottommost slot, lowest-severity).
const SLOT2_Y: u32 = 94;

// ─── Tests ────────────────────────────────────────────────────────────────────

/// Requirement: Alert-Banner Integration Test Suite
/// Scenario: Test urgency 0 backdrop color
///
/// Publishing a `NotificationPayload` with `urgency = 0` to the alert-banner zone
/// MUST produce a backdrop quad whose rendered pixels match `color.severity.info`
/// (#4A9EFF) at 0.9 alpha, within ±8 per channel.
#[tokio::test]
async fn test_alert_banner_urgency0_info_backdrop() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "System check complete".to_string(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for urgency=0 Notification on alert-banner");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        INFO_EXPECTED,
        TOLERANCE,
        "alert-banner urgency=0 info backdrop",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Requirement: Alert-Banner Integration Test Suite
/// Scenario: Test urgency 1 backdrop color
///
/// Publishing a `NotificationPayload` with `urgency = 1` to the alert-banner zone
/// MUST produce a backdrop quad with the same `color.severity.info` (#4A9EFF)
/// backdrop as urgency 0, proving both map to the same severity token.
#[tokio::test]
async fn test_alert_banner_urgency1_info_backdrop() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Update available".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for urgency=1 Notification on alert-banner");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        INFO_EXPECTED,
        TOLERANCE,
        "alert-banner urgency=1 info backdrop (same as urgency=0)",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Requirement: Alert-Banner Integration Test Suite
/// Scenario: Test urgency 2 backdrop color
///
/// Publishing a `NotificationPayload` with `urgency = 2` to the alert-banner zone
/// MUST produce a backdrop quad whose rendered pixels match `color.severity.warning`
/// (#FFB800) at 0.9 alpha, within ±8 per channel.
#[tokio::test]
async fn test_alert_banner_urgency2_warning_backdrop() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Disk space low".to_string(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for urgency=2 Notification on alert-banner");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        WARNING_EXPECTED,
        TOLERANCE,
        "alert-banner urgency=2 warning backdrop",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Requirement: Alert-Banner Integration Test Suite
/// Scenario: Test urgency 3 backdrop color
///
/// Publishing a `NotificationPayload` with `urgency = 3` to the alert-banner zone
/// MUST produce a backdrop quad whose rendered pixels match `color.severity.critical`
/// (#FF0000) at 0.9 alpha, within ±8 per channel.
#[tokio::test]
async fn test_alert_banner_urgency3_critical_backdrop() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Security breach detected".to_string(),
                icon: String::new(),
                urgency: 3,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for urgency=3 Notification on alert-banner");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        CRITICAL_EXPECTED,
        TOLERANCE,
        "alert-banner urgency=3 critical backdrop",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Requirement: Alert-Banner Stack-by-Severity Ordering / Integration Test Suite
/// Scenario: Test stacking order with three severity levels
///
/// When info (urgency=1), warning (urgency=2), and critical (urgency=3) alerts are
/// published simultaneously to the alert-banner zone from three different agent
/// namespaces, the compositor MUST render them in severity-descending order:
///   slot 0 (top)    → critical (red,   sampled at y=18)
///   slot 1 (middle) → warning  (amber, sampled at y=56)
///   slot 2 (bottom) → info     (blue,  sampled at y=94)
///
/// Each slot has height 37.6px (line_height=24*1.4=33.6 + 2×margin_vertical=0 + baseline_gap=4).
/// The alert-banner zone uses `ContentionPolicy::Stack { max_depth: 8 }` with
/// `max_publishers: 1` (one banner per agent namespace), so three different
/// namespaces ("agent-critical", "agent-warning", "agent-info") are used.
#[tokio::test]
async fn test_alert_banner_stacking_order() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    // Publish info first (lowest severity) — should sort to bottom.
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Info: system nominal".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-info",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for info alert");

    // Publish warning (middle severity) — should sort to middle.
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Warning: disk space low".to_string(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-warning",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for warning alert");

    // Publish critical last (highest severity) — should sort to top.
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "CRITICAL: security breach detected".to_string(),
                icon: String::new(),
                urgency: 3,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-critical",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for critical alert");

    // All 3 banners must be active.
    let pub_count = scene
        .zone_registry
        .active_publishes
        .get("alert-banner")
        .map(|v| v.len())
        .unwrap_or(0);
    assert_eq!(
        pub_count, 3,
        "alert-banner must have 3 active publications; got {pub_count}"
    );

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Slot 0 (top): critical → red backdrop.
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        CRITICAL_EXPECTED,
        TOLERANCE,
        "slot 0 (top) must be critical red",
    )
    .unwrap_or_else(|e| panic!("stacking order — {e}"));

    // Slot 1 (middle): warning → amber backdrop.
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT1_Y,
        WARNING_EXPECTED,
        TOLERANCE,
        "slot 1 (middle) must be warning amber",
    )
    .unwrap_or_else(|e| panic!("stacking order — {e}"));

    // Slot 2 (bottom): info → blue backdrop.
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT2_Y,
        INFO_EXPECTED,
        TOLERANCE,
        "slot 2 (bottom) must be info blue",
    )
    .unwrap_or_else(|e| panic!("stacking order — {e}"));
}

/// Requirement: Alert-Banner Severity-Colored Backdrop Rendering
/// Scenario: Non-notification content uses default backdrop
///
/// Publishing `ZoneContent::StreamText` to the alert-banner zone MUST use the zone's
/// default `RenderingPolicy.backdrop` color, NOT the urgency-to-severity color mapping.
///
/// The default alert-banner backdrop is `Rgba { r:0.1, g:0.1, b:0.16, a:0.9 }` (dark
/// blue-tinted). After sRGB encoding the pixel must be dark and only slightly blue-tinted
/// (distinctly different from any severity color: far from the bright blue used for
/// info, not amber, not red).
///
/// The key invariant checked:
///   - Pixel is close to the expected default backdrop value (~[87, 87, 109]) within ±20,
///     which distinguishes it from both the bare clear color (~[62, 62, 89]) and any
///     severity color (all of which are much brighter in at least one channel).
///   - B channel is slightly dominant over R (ensures the dark blue-tint, not red hue).
///   - All channels are in the dark range (< 150), ruling out any severity color.
#[tokio::test]
async fn test_alert_banner_stream_text_uses_default_policy_color() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);

    // StreamText is accepted by the alert-banner zone (per ZoneMediaType::StreamText).
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::StreamText("System alert (stream)".to_string()),
            "test-agent",
            None,
            None,
            None,
        )
        .expect("publish_to_zone must succeed for StreamText on alert-banner");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // The default alert-banner backdrop: linear(0.1, 0.1, 0.16) at 0.9 alpha.
    // After sRGB encoding over the clear color (linear 0.05, 0.05, 0.1):
    //   Final linear: r≈0.095, g≈0.095, b≈0.154
    //   sRGB×255: r≈87, g≈87, b≈109
    //
    // Use assert_pixel_color with wide tolerance (±20) to:
    //   (a) confirm the backdrop quad was actually rendered (distinguishes from bare clear
    //       color ~[62, 62, 89] which is too dark to match [87, 87, 109] at ±20), and
    //   (b) rule out any severity color (info ~[78,160,245], warning ~[244,211,26],
    //       critical ~[244,15,26]).
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        [87, 87, 109, 255],
        20,
        "alert-banner StreamText default policy backdrop",
    )
    .unwrap_or_else(|e| panic!("{e}"));

    let actual = HeadlessSurface::pixel_at(&pixels, SURFACE_W, SAMPLE_X, SLOT0_Y);

    // All channels must be dark (< 150), ruling out any severity color.
    assert!(
        actual[0] < 150,
        "StreamText backdrop R must be dark (< 150, not severity red/warning amber); got {}",
        actual[0]
    );
    assert!(
        actual[1] < 150,
        "StreamText backdrop G must be dark (< 150, not severity warning amber); got {}",
        actual[1]
    );
    assert!(
        actual[2] < 150,
        "StreamText backdrop B must be dark (< 150, not severity info blue ~230); got {}",
        actual[2]
    );

    // B must be higher than R (dark blue-tinted, not red-dominant).
    assert!(
        actual[2] >= actual[0],
        "StreamText backdrop B ({}) must be >= R ({}) — dark blue-tint, not red",
        actual[2],
        actual[0]
    );
}

// ─── Debug probe (will be removed after calibration) ─────────────────────────

/// Internal calibration test: prints actual pixel values at the banner region.
///
/// Useful for recalibrating expected values after colour-pipeline changes.
///
/// Run with:
///   `HEADLESS_FORCE_SOFTWARE=1 cargo test --test alert_banner_rendering debug_probe_pixel_values -- --nocapture --ignored`
#[tokio::test]
#[ignore]
async fn debug_probe_pixel_values() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);

    for urgency in [0u32, 1, 2, 3] {
        let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);
        scene
            .publish_to_zone(
                "alert-banner",
                ZoneContent::Notification(NotificationPayload {
                    text: format!("urgency={urgency}"),
                    icon: String::new(),
                    urgency,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                "probe",
                None,
                None,
                None,
            )
            .unwrap();
        compositor.render_frame_headless(&mut scene, &surface);
        let pixels = surface.read_pixels(&compositor.device);
        let p = HeadlessSurface::pixel_at(&pixels, SURFACE_W, SAMPLE_X, SLOT0_Y);
        println!(
            "urgency={urgency} → pixel at ({SAMPLE_X},{SLOT0_Y}): R={} G={} B={} A={}",
            p[0], p[1], p[2], p[3]
        );
    }

    // Probe StreamText default backdrop.
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::StreamText("stream".to_string()),
            "probe",
            None,
            None,
            None,
        )
        .unwrap();
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    let p = HeadlessSurface::pixel_at(&pixels, SURFACE_W, SAMPLE_X, SLOT0_Y);
    println!(
        "StreamText → pixel at ({SAMPLE_X},{SLOT0_Y}): R={} G={} B={} A={}",
        p[0], p[1], p[2], p[3]
    );

    // Probe stacking: 3 banners at different slots.
    let mut scene = scene_with_defaults(SURFACE_W as f32, SURFACE_H as f32);
    for (urgency, ns) in [
        (1u32, "probe-info"),
        (2, "probe-warning"),
        (3, "probe-critical"),
    ] {
        scene
            .publish_to_zone(
                "alert-banner",
                ZoneContent::Notification(NotificationPayload {
                    text: format!("urgency={urgency}"),
                    icon: String::new(),
                    urgency,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                ns,
                None,
                None,
                None,
            )
            .unwrap();
    }
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    for (label, y) in [
        ("slot0(critical)", SLOT0_Y),
        ("slot1(warning)", SLOT1_Y),
        ("slot2(info)", SLOT2_Y),
    ] {
        let p = HeadlessSurface::pixel_at(&pixels, SURFACE_W, SAMPLE_X, y);
        println!(
            "stacking {label} → pixel at ({SAMPLE_X},{y}): R={} G={} B={} A={}",
            p[0], p[1], p[2], p[3]
        );
    }
}
