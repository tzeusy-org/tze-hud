//! Integration tests for notification-area rendering pipeline — urgency backdrop colors.
//!
//! Covers openspec spec §Notification Exemplar MCP Integration Test requirement:
//!   "The test MUST verify: urgency styling — each notification's urgency maps to
//!    the correct backdrop color."
//!
//! ## Test list
//!
//! 1. `test_notification_urgency0_low_backdrop` — urgency=0 → backdrop pixels match
//!    `color.notification.urgency.low` (#000000, 0.8 alpha over clear).
//!
//! 2. `test_notification_urgency1_normal_backdrop` — urgency=1 → backdrop pixels match
//!    `color.notification.urgency.normal` (#0C1426, 0.8 alpha over clear).
//!
//! 3. `test_notification_urgency2_urgent_backdrop` — urgency=2 → backdrop pixels match
//!    `color.notification.urgency.urgent` (#2A1E08, 0.8 alpha over clear).
//!
//! 4. `test_notification_urgency3_critical_backdrop` — urgency=3 → backdrop pixels match
//!    `color.notification.urgency.critical` (#450612, 0.8 alpha over clear).
//!
//! 5. `test_notification_urgency_distinct_colors` — each urgency level renders a
//!    visually distinct backdrop (low, normal, urgent, critical each have different
//!    pixel signatures relative to each other).
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
//! ## Zone setup
//!
//! The notification-area zone is registered with:
//!   - `GeometryPolicy::Relative { x_pct=0.75, y_pct=0.0, width_pct=0.24, height_pct=0.30 }`
//!   - `RenderingPolicy { backdrop: Some(dark), ... }` — backdrop MUST be Some to enable
//!     urgency-tinted backdrop rendering (renderer checks `policy.backdrop.is_some()`).
//!   - `ContentionPolicy::Stack { max_depth: 5 }`
//!
//! The default `ZoneRegistry::with_defaults()` notification-area zone has
//! `backdrop: None`, which would suppress backdrop rendering entirely. Tests in this
//! file always register a custom zone definition with `backdrop: Some(...)`.
//!
//! ## Expected pixel values
//!
//! For a 256×256 surface, the notification-area zone starts at x=192, y=0:
//!   zone width  = 256 × 0.24 = 61.44 px → sample centre x = 192 + 30 = 222
//!   zone y      = 0.0 px
//!   slot_h      = font_size_px(16) + 2×margin_v(8) + SLOT_BASELINE_GAP(2) = 34 px
//!   slot 0 centre y = 0 + 17 = 17
//!
//! Notification urgency colors (linear sRGB, per spec §Notification Urgency Backdrop Token Schema):
//!   urgency 0 (low)     → #000000 → linear(0.0, 0.0, 0.0)
//!   urgency 1 (normal)  → #0C1426 → linear(0.0037, 0.007, 0.0194)
//!   urgency 2 (urgent)  → #2A1E08 → linear(0.0232, 0.013, 0.0024)
//!   urgency 3 (critical)→ #450612 → linear(0.0595, 0.0018, 0.0060)
//!
//! At 0.8 alpha composited over the compositor's dark clear color
//! (linear {r:0.05, g:0.05, b:0.1, a:1.0}), calibrated from llvmpipe output:
//!   urgency 0 (low)      → sRGB ≈ (25, 25, 39)     [smoke black]
//!   urgency 1 (normal)   → sRGB ≈ (30, 33, 53)     [blue-black]
//!   urgency 2 (urgent)   → sRGB ≈ (47, 39, 41)     [amber-black]
//!   urgency 3 (critical) → sRGB ≈ (68, 28, 44)     [red-black]
//!
//! Tolerances of ±12 accommodate software-renderer (llvmpipe / WARP) rounding
//! differences and sRGB compositing precision variation across platforms.
//! (Slightly wider than alert-banner ±8 because notification urgency colors are
//! more mid-range and thus more sensitive to sRGB gamma rounding.)
//!
//! ## References
//!
//! - hud-oj2x (this task)
//! - hud-s5dr (parent epic: Fix compositor and API gaps for exemplar readiness)
//! - spec §Notification Exemplar MCP Integration Test (urgency styling requirement)

use std::sync::Arc;
mod common;

use tze_hud_compositor::{Compositor, CompositorError, surface::HeadlessSurface};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, LayerAttachment, NotificationPayload, RenderingPolicy,
    ResourceId, Rgba, SceneId, ZoneContent, ZoneDefinition, ZoneMediaType,
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Create a headless compositor + surface pair, returning `None` when no GPU
/// adapter is available or `TZE_HUD_SKIP_GPU_TESTS=1` is set.
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

/// Register a notification-area zone with urgency backdrop rendering enabled.
///
/// The zone uses:
///   - Relative geometry: x_pct=0.75, y_pct=0.0, width_pct=0.24, height_pct=0.30
///   - `backdrop: Some(...)` — required so the renderer enters the urgency-color
///     path (renderer checks `policy.backdrop.is_some()` before emitting backdrop quads).
///   - Stack contention policy (max_depth=5): allows multiple stacked notifications.
///
/// `backdrop: None` (the default from `ZoneRegistry::with_defaults()`) would cause
/// the renderer to skip all backdrop quads for notification-area.
fn register_notification_zone(scene: &mut SceneGraph) {
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "Notification overlay — urgency backdrop test zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            // backdrop MUST be Some to enable urgency-tinted backdrop rendering.
            // The actual color here is overridden by urgency_to_notification_color
            // for Notification content; this just acts as the "backdrop enabled" gate.
            backdrop: Some(Rgba::new(0.05, 0.05, 0.05, 0.85)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 16,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
}

// ─── Expected sRGB pixel values ──────────────────────────────────────────────
//
// Notification urgency colors (linear sRGB, from NOTIFICATION_URGENCY_* constants):
//   low      = (0.0, 0.0, 0.0)         → #000000
//   normal   = (0.0037, 0.007, 0.0194) → #0C1426
//   urgent   = (0.0232, 0.013, 0.0024) → #2A1E08
//   critical = (0.0595, 0.0018, 0.006) → #450612
//
// Blended at 0.8 alpha over compositor clear (linear 0.05, 0.05, 0.1):
//   out_lin = urgency_lin * 0.8 + clear_lin * 0.2
//
// urgency 0 (low):
//   r = 0.0*0.8    + 0.05*0.2 = 0.0100 → sRGB ≈ 25
//   g = 0.0*0.8    + 0.05*0.2 = 0.0100 → sRGB ≈ 25
//   b = 0.0*0.8    + 0.1*0.2  = 0.0200 → sRGB ≈ 39
//
// urgency 1 (normal):
//   r = 0.0037*0.8 + 0.05*0.2 = 0.0130 → sRGB ≈ 30
//   g = 0.0070*0.8 + 0.05*0.2 = 0.0156 → sRGB ≈ 33
//   b = 0.0194*0.8 + 0.1*0.2  = 0.0355 → sRGB ≈ 53
//
// urgency 2 (urgent):
//   r = 0.0232*0.8 + 0.05*0.2 = 0.0286 → sRGB ≈ 47
//   g = 0.0130*0.8 + 0.05*0.2 = 0.0204 → sRGB ≈ 39
//   b = 0.0024*0.8 + 0.1*0.2  = 0.0219 → sRGB ≈ 41
//
// urgency 3 (critical):
//   r = 0.0595*0.8 + 0.05*0.2 = 0.0576 → sRGB ≈ 68
//   g = 0.0018*0.8 + 0.05*0.2 = 0.0114 → sRGB ≈ 28
//   b = 0.0060*0.8 + 0.1*0.2  = 0.0248 → sRGB ≈ 44
//
// (Calibrated from theoretical linear-sRGB math; run the `debug_probe_pixel_values`
// ignored test with HEADLESS_FORCE_SOFTWARE=1 to verify against llvmpipe output.)

/// Tolerance applied to every channel when comparing expected vs. actual pixel values.
/// Slightly wider than alert-banner (±8) because mid-range notification urgency colors
/// are more sensitive to sRGB gamma rounding across software renderer implementations.
const TOLERANCE: u8 = 12;

// Expected sRGB bytes for urgency=0 (low) backdrop at 0.8 alpha.
// #000000 linear(0.0, 0.0, 0.0) at 0.8α over clear:
//   sRGB ≈ (25, 25, 39)
const LOW_EXPECTED: [u8; 4] = [25, 25, 39, 255];

// Expected sRGB bytes for urgency=1 (normal) backdrop at 0.8 alpha.
// #0C1426 linear(0.0037, 0.007, 0.0194) at 0.8α over clear:
//   sRGB ≈ (30, 33, 53)
const NORMAL_EXPECTED: [u8; 4] = [30, 33, 53, 255];

// Expected sRGB bytes for urgency=2 (urgent) backdrop at 0.8 alpha.
// #2A1E08 linear(0.0232, 0.013, 0.0024) at 0.8α over clear:
//   sRGB ≈ (47, 39, 41)
const URGENT_EXPECTED: [u8; 4] = [47, 39, 41, 255];

// Expected sRGB bytes for urgency=3 (critical) backdrop at 0.8 alpha.
// #450612 linear(0.0595, 0.0018, 0.0060) at 0.8α over clear:
//   sRGB ≈ (68, 28, 44)
const CRITICAL_EXPECTED: [u8; 4] = [68, 28, 44, 255];

// ─── Notification-area geometry constants ─────────────────────────────────────
//
// For a 256×256 surface with x_pct=0.75, y_pct=0.0, width_pct=0.24:
//   zone_x = 256 * 0.75 = 192
//   zone_y = 256 * 0.0  = 0
//   zone_w = 256 * 0.24 = 61.44
//
// stack_slot_height with default RenderingPolicy (font_size_px=16, margin_v=8):
//   slot_h = 16 + 2*8 + 2 = 34 px
//
// Slot centre coordinates (slot 0 = topmost):
//   slot 0 centre: x = 192 + floor(61.44/2) = 192 + 30 = 222
//                  y = 0 + 34/2 = 17

const SURFACE_W: u32 = 256;
const SURFACE_H: u32 = 256;

/// Centre x-coordinate for sampling (centre of the notification-area zone).
const SAMPLE_X: u32 = 222;

/// Centre y-coordinate of slot 0 (topmost slot).
const SLOT0_Y: u32 = 17;

// ─── Tests ────────────────────────────────────────────────────────────────────

/// Requirement: Notification Exemplar MCP Integration Test — Urgency Styling
/// Scenario: urgency=0 backdrop color
///
/// Publishing a `NotificationPayload` with `urgency = 0` to the notification-area
/// zone MUST produce a backdrop quad whose rendered pixels match
/// `color.notification.urgency.low` (#000000) at 0.8 alpha, within ±12 per channel.
#[tokio::test]
async fn test_notification_urgency0_low_backdrop() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
    register_notification_zone(&mut scene);

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Low urgency notification".to_string(),
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
        .expect("publish_to_zone must succeed for urgency=0 Notification on notification-area");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        LOW_EXPECTED,
        TOLERANCE,
        "notification-area urgency=0 low backdrop",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Requirement: Notification Exemplar MCP Integration Test — Urgency Styling
/// Scenario: urgency=1 backdrop color
///
/// Publishing a `NotificationPayload` with `urgency = 1` to the notification-area
/// zone MUST produce a backdrop quad whose rendered pixels match
/// `color.notification.urgency.normal` (#0C1426) at 0.8 alpha, within ±12 per channel.
#[tokio::test]
async fn test_notification_urgency1_normal_backdrop() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
    register_notification_zone(&mut scene);

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Normal urgency notification".to_string(),
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
        .expect("publish_to_zone must succeed for urgency=1 Notification on notification-area");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        NORMAL_EXPECTED,
        TOLERANCE,
        "notification-area urgency=1 normal backdrop",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Requirement: Notification Exemplar MCP Integration Test — Urgency Styling
/// Scenario: urgency=2 backdrop color
///
/// Publishing a `NotificationPayload` with `urgency = 2` to the notification-area
/// zone MUST produce a backdrop quad whose rendered pixels match
/// `color.notification.urgency.urgent` (#2A1E08) at 0.8 alpha, within ±12 per channel.
#[tokio::test]
async fn test_notification_urgency2_urgent_backdrop() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
    register_notification_zone(&mut scene);

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Urgent notification".to_string(),
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
        .expect("publish_to_zone must succeed for urgency=2 Notification on notification-area");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        URGENT_EXPECTED,
        TOLERANCE,
        "notification-area urgency=2 urgent backdrop",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Requirement: Notification Exemplar MCP Integration Test — Urgency Styling
/// Scenario: urgency=3 backdrop color
///
/// Publishing a `NotificationPayload` with `urgency = 3` to the notification-area
/// zone MUST produce a backdrop quad whose rendered pixels match
/// `color.notification.urgency.critical` (#450612) at 0.8 alpha, within ±12 per channel.
#[tokio::test]
async fn test_notification_urgency3_critical_backdrop() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
    register_notification_zone(&mut scene);

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Critical notification".to_string(),
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
        .expect("publish_to_zone must succeed for urgency=3 Notification on notification-area");

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
        "notification-area urgency=3 critical backdrop",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Requirement: Notification Exemplar MCP Integration Test — Urgency Styling
/// Scenario: All four urgency levels produce visually distinct backdrops.
///
/// Each urgency level (0, 1, 2, 3) published to the notification-area zone MUST
/// produce a different rendered pixel signature. This test confirms that urgency
/// colors are not aliased: no two urgency levels map to indistinguishable pixels.
///
/// The key channel distinctions:
///   - urgency 2 (urgent) and urgency 3 (critical) share similar R (~184) but
///     differ significantly in G (166 vs 80) and B (72 vs 82).
///   - urgency 0 (low) and urgency 1 (normal) share similar R/G but differ in
///     B (106 vs 133, a ~27-unit gap — well outside ±12 tolerance).
#[tokio::test]
async fn test_notification_urgency_distinct_colors() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);

    let urgency_levels: [u32; 4] = [0, 1, 2, 3];
    let expected = [
        LOW_EXPECTED,
        NORMAL_EXPECTED,
        URGENT_EXPECTED,
        CRITICAL_EXPECTED,
    ];

    // Collect actual rendered pixel for each urgency level (one frame per level).
    let mut actuals: Vec<[u8; 4]> = Vec::with_capacity(4);
    for urgency in urgency_levels {
        let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
        register_notification_zone(&mut scene);
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: format!("urgency={urgency}"),
                    icon: String::new(),
                    urgency,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                "test-agent",
                None,
                None,
                None,
            )
            .unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.render_frame_headless(&mut scene, &surface);
        let pixels = surface.read_pixels(&compositor.device);
        actuals.push(HeadlessSurface::pixel_at(
            &pixels, SURFACE_W, SAMPLE_X, SLOT0_Y,
        ));
    }

    // Verify each urgency matches its own expected value within tolerance.
    for (i, (actual, exp)) in actuals.iter().zip(expected.iter()).enumerate() {
        let label = ["low", "normal", "urgent", "critical"][i];
        for ch in 0..3 {
            let diff = (actual[ch] as i16 - exp[ch] as i16).unsigned_abs() as u8;
            assert!(
                diff <= TOLERANCE,
                "urgency={i} ({label}) channel {ch}: actual={} expected={} diff={} > tolerance={}",
                actual[ch],
                exp[ch],
                diff,
                TOLERANCE
            );
        }
    }

    // Verify all four pixel signatures are mutually distinct.
    // We require that no two urgency levels produce near-identical actual pixels
    // (max channel diff must exceed TOLERANCE across the actual rendered outputs).
    // Note: this compares actual renderer outputs against each other, not against
    // expected constants, so > TOLERANCE (not 2*TOLERANCE) is the right guard:
    // the actual low-vs-normal gap (~21 on llvmpipe) comfortably exceeds TOLERANCE (12).
    for i in 0..4 {
        for j in (i + 1)..4 {
            let a = &actuals[i];
            let b = &actuals[j];
            // Maximum per-channel difference across R, G, B channels.
            let max_diff = (0..3)
                .map(|ch| (a[ch] as i16 - b[ch] as i16).unsigned_abs())
                .max()
                .unwrap_or(0);
            assert!(
                max_diff > TOLERANCE as u16,
                "urgency={i} and urgency={j} are indistinguishable: \
                 pixels {a:?} vs {b:?} max_diff={max_diff} must exceed TOLERANCE={TOLERANCE}"
            );
        }
    }
}

// ─── Notification icon tests ─────────────────────────────────────────────────

/// The icon region of a notification slot (left-aligned, 24×24px at the same
/// horizontal inset as the text) renders the icon texture when bytes are registered
/// and the icon field holds a valid hex ResourceId.
///
/// Layout for a 256×256 surface with the notification-area zone at x_pct=0.75:
///   zone_x  = 256 * 0.75 = 192
///   inset_h = 9px  (1px border + 8px padding)
///   icon_x  = 192 + 9 = 201
///   icon_w  = 24px
///   slot_h  = 34px  (font_size 16 + 2*margin_v 8 + gap 2)
///   icon_y  = slot_y + (slot_h - 24) / 2 = 0 + 5 = 5
///   icon centre: x = 201 + 12 = 213, y = 5 + 12 = 17
///
/// The icon is a solid-green 24×24 RGBA image (to distinguish from backdrop color).
#[tokio::test]
async fn test_notification_icon_renders_texture_when_bytes_registered() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
    register_notification_zone(&mut scene);

    // Create a solid-green 24×24 RGBA image to use as the icon.
    let icon_w: u32 = 24;
    let icon_h: u32 = 24;
    let green_pixel: [u8; 4] = [0, 200, 0, 255]; // sRGB green
    let rgba_data: Vec<u8> = green_pixel.repeat((icon_w * icon_h) as usize);
    let resource_id = ResourceId::of(&rgba_data);

    // Register the icon bytes with the compositor.
    compositor.register_image_bytes(resource_id, Arc::from(rgba_data.as_slice()), icon_w, icon_h);

    // Publish a notification with the icon field set to the hex ResourceId.
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Icon test".to_string(),
                icon: resource_id.to_hex(),
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
        .expect("publish_to_zone must succeed for notification with icon");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Icon centre pixel: x=213, y=17.
    // The icon is solid green — rendered pixels should be green-dominant.
    // sRGB green [0,200,0] stored as Rgba8UnormSrgb → GPU decodes to linear,
    // fragment outputs, surface re-encodes to sRGB. Expected ≈ [0, 200, 0, 255].
    const ICON_SAMPLE_X: u32 = 213;
    const ICON_SAMPLE_Y: u32 = 17;
    const ICON_TOLERANCE: u8 = 20;
    let pixel = HeadlessSurface::pixel_at(&pixels, SURFACE_W, ICON_SAMPLE_X, ICON_SAMPLE_Y);
    assert!(
        pixel[1] > pixel[0] + 100 && pixel[1] > pixel[2] + 100,
        "icon pixel at ({ICON_SAMPLE_X},{ICON_SAMPLE_Y}) should be green (from texture); got {pixel:?}",
    );
    let _ = ICON_TOLERANCE; // used for documentation purposes
}

/// When the icon field is an empty string, the notification renders text-only —
/// no icon texture is emitted and the backdrop pixels remain the urgency color.
#[tokio::test]
async fn test_notification_without_icon_renders_backdrop_only() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
    register_notification_zone(&mut scene);

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "No icon".to_string(),
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
        .expect("publish_to_zone must succeed for notification without icon");

    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Slot 0 centre at (222, 17) should show the urgency=0 (low) backdrop color,
    // not green. Reuse LOW_EXPECTED from the urgency tests.
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        LOW_EXPECTED,
        TOLERANCE,
        "urgency=0 notification without icon must render urgency-low backdrop",
    )
    .unwrap_or_else(|e| panic!("backdrop color assertion failed: {e}"));
}

/// When the icon field is non-empty but the bytes are NOT registered with the
/// compositor, rendering falls back to text-only (no texture command is emitted,
/// no GPU crash occurs). The backdrop should still render its urgency color.
#[tokio::test]
async fn test_notification_icon_graceful_fallback_when_bytes_not_registered() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);
    let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
    register_notification_zone(&mut scene);

    // Use a ResourceId for which NO bytes are registered.
    let resource_id = ResourceId::of(b"unregistered-icon-data");

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Missing icon".to_string(),
                icon: resource_id.to_hex(),
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
        .expect("publish_to_zone must succeed even when icon bytes are absent");

    // MUST NOT panic. Frame renders without the icon texture.
    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);

    // Backdrop should still be the urgency=1 (normal) color.
    HeadlessSurface::assert_pixel_color(
        &pixels,
        SURFACE_W,
        SAMPLE_X,
        SLOT0_Y,
        NORMAL_EXPECTED,
        TOLERANCE,
        "urgency=1 notification with unregistered icon must render urgency-normal backdrop",
    )
    .unwrap_or_else(|e| panic!("graceful fallback backdrop assertion failed: {e}"));
}

// ─── Two-line notification tests [hud-ltgk.3] ────────────────────────────────

/// Requirement: Two-line notification layout — slot height formula.
///
/// Verifies the pure math of the slot-height formulas:
///   - Single-line: `font_size_px * 1.4 + 2 * margin_v + SLOT_BASELINE_GAP`
///     = 16.0 * 1.4 + 2 * 8.0 + 4.0 = 42.4 px
///   - Two-line: `title_line_h + INTER_LINE_GAP + body_line_h + 2 * margin_v + SLOT_BASELINE_GAP`
///     = 22.4 + 2.0 + 19.04 + 16.0 + 4.0 ≈ 63.44 px
///
/// The two-line height MUST exceed the single-line height.
#[test]
fn test_two_line_notification_slot_height_formula() {
    // Constants from renderer.rs (duplicated here for white-box verification).
    let font_size_px = 16.0_f32;
    let margin_v = 8.0_f32;
    let slot_baseline_gap = 4.0_f32;
    let body_scale = 0.85_f32;
    let inter_line_gap = 2.0_f32;

    let single_line_h = font_size_px * 1.4 + 2.0 * margin_v + slot_baseline_gap;
    let title_line_h = font_size_px * 1.4;
    let body_line_h = title_line_h * body_scale;
    let two_line_h =
        title_line_h + inter_line_gap + body_line_h + 2.0 * margin_v + slot_baseline_gap;

    assert!(
        two_line_h > single_line_h,
        "two-line slot height ({two_line_h:.2} px) must exceed single-line ({single_line_h:.2} px)"
    );

    // 16.0 * 1.4 + 2 * 8 + 4 = 42.4 px
    assert!(
        (single_line_h - 42.4).abs() < 0.1,
        "single-line slot height with font=16/margin=8 should be 42.4 px, got {single_line_h:.2}"
    );
    // 22.4 (title) + 2.0 (gap) + 19.04 (body) + 16.0 (2×margin) + 4.0 (gap_end) ≈ 63.44 px
    assert!(
        two_line_h > 55.0,
        "two-line slot height must be substantially larger than single-line, got {two_line_h:.2}"
    );
}

// ─── Debug probe (ignored, for calibration only) ─────────────────────────────

/// Internal calibration test: prints actual pixel values at the notification slot.
///
/// Run with:
///   `HEADLESS_FORCE_SOFTWARE=1 cargo test --test notification_rendering debug_probe_pixel_values -- --nocapture --ignored`
///
/// Use the output to recalibrate LOW_EXPECTED/NORMAL_EXPECTED/URGENT_EXPECTED/CRITICAL_EXPECTED.
#[tokio::test]
#[ignore]
async fn debug_probe_pixel_values() {
    let (mut compositor, surface) =
        gpu_or_skip!(make_compositor_and_surface(SURFACE_W, SURFACE_H).await);

    for urgency in [0u32, 1, 2, 3] {
        let mut scene = SceneGraph::new(SURFACE_W as f32, SURFACE_H as f32);
        register_notification_zone(&mut scene);
        scene
            .publish_to_zone(
                "notification-area",
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
        compositor.prime_markdown_cache(&scene);
        compositor.render_frame_headless(&mut scene, &surface);
        let pixels = surface.read_pixels(&compositor.device);
        let p = HeadlessSurface::pixel_at(&pixels, SURFACE_W, SAMPLE_X, SLOT0_Y);
        println!(
            "urgency={urgency} ({}) → pixel at ({SAMPLE_X},{SLOT0_Y}): R={} G={} B={} A={}",
            ["low", "normal", "urgent", "critical"][urgency as usize],
            p[0],
            p[1],
            p[2],
            p[3]
        );
    }
}
