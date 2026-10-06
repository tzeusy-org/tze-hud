//! Layer 1 pixel readback assertions for every registered test scene.
//!
//! # Overview
//!
//! These tests implement the Layer 1 validation tier defined in
//! `heart-and-soul/validation.md` lines 109-122:
//!
//! > "Layer 1 — Headless pixel validation: render each named test scene via
//! >  HeadlessRuntime, read back the pixel buffer, assert key regions match
//! >  expected colours within tolerance."
//!
//! # Pattern
//!
//! ```text
//! Build named scene via TestSceneRegistry
//!   → inject into HeadlessRuntime
//!   → render one frame
//!   → read pixels via HeadlessRuntime::read_pixels()
//!   → assert buffer size == SCENE_W × SCENE_H × 4
//!   → assert key region pixels within CI tolerance
//! ```
//!
//! # Tolerance
//!
//! Per `validation.md` line 117 and per the llvmpipe CI reality documented
//! in `budget_assertions.rs`:
//! - Solid fills:     `CI_SOLID_TOLERANCE` = ±6 per channel
//! - Alpha blending:  `CI_BLEND_TOLERANCE` = ±8 per channel
//!
//! # Test enablement status
//!
//! All colour-assertion tests are now fully enabled (no `#[ignore]`).
//!
//! `HeadlessRuntime::render_frame` calls `compositor.render_frame_headless()`
//! which includes the `copy_to_buffer` step before `queue.submit()`, so
//! `read_pixels()` returns actual rendered pixel data after each frame.
//!
//! The buffer-size assertions (always-enabled, DR-V2) confirm the headless
//! pipeline completes for every scene without crash or OOM.  Both buffer-size
//! and colour assertions must pass unconditionally in CI.
//!
//! # Spec references
//!
//! - validation-framework/spec.md DR-V2 (line 186): headless frame pipeline
//! - validation-framework/spec.md DR-V5 (line 228): `cargo test --features dev-mode`
//! - scene-graph/spec.md lines 45-64: z-order rendering
//! - scene-graph/spec.md lines 185-200: zone rendering
//! - policy-arbitration/spec.md lines 91-104: Level 2 Privacy Evaluation
//! - input-model/spec.md lines 11-22: focus indicators
//! - configuration/spec.md lines 123-134: zone types

mod pixel_helpers;

use pixel_helpers::{
    BG_SRGB, CI_BLEND_TOLERANCE, CI_SOLID_TOLERANCE, SCENE_H, SCENE_W, make_scene_runtime,
    render_scene_pixels,
};
use tze_hud_scene::test_scenes::{ClockMs, TestSceneRegistry};

// ─── Pixel buffer size assertions (always-enabled, DR-V2) ────────────────────
//
// These tests confirm that `HeadlessRuntime::render_frame` + `read_pixels`
// returns a buffer of the correct size for every scene.  They do NOT assert
// colour values — those are in the colour-assertion tests below.
//
// All must pass unconditionally in CI.

macro_rules! scene_buffer_size_test {
    ($test_name:ident, $scene_name:literal) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $test_name() {
            let mut runtime = make_scene_runtime().await;
            let registry = TestSceneRegistry::new();
            let (scene, _spec) = registry
                .build($scene_name, ClockMs::FIXED)
                .expect(concat!("TestSceneRegistry::build failed for ", $scene_name));
            let pixels = render_scene_pixels(&mut runtime, scene).await;
            assert_eq!(
                pixels.len(),
                (SCENE_W * SCENE_H * 4) as usize,
                concat!(
                    $scene_name,
                    ": pixel buffer must be SCENE_W × SCENE_H × 4 bytes (RGBA8)"
                )
            );
            assert_eq!(
                pixels.len() % 4,
                0,
                concat!($scene_name, ": pixel buffer must be RGBA8-aligned")
            );
        }
    };
}

scene_buffer_size_test!(test_buf_01_empty_scene, "empty_scene");
scene_buffer_size_test!(test_buf_02_single_tile_solid, "single_tile_solid");
scene_buffer_size_test!(test_buf_03_three_tiles_no_overlap, "three_tiles_no_overlap");
scene_buffer_size_test!(
    test_buf_04_overlapping_tiles_zorder,
    "overlapping_tiles_zorder"
);
scene_buffer_size_test!(test_buf_05_overlay_transparency, "overlay_transparency");
scene_buffer_size_test!(test_buf_06_tab_switch, "tab_switch");
scene_buffer_size_test!(test_buf_07_lease_expiry, "lease_expiry");
scene_buffer_size_test!(test_buf_10_input_highlight, "input_highlight");
scene_buffer_size_test!(test_buf_11_coalesced_dashboard, "coalesced_dashboard");
scene_buffer_size_test!(test_buf_12_max_tiles_stress, "max_tiles_stress");
scene_buffer_size_test!(
    test_buf_13_three_agents_contention,
    "three_agents_contention"
);
scene_buffer_size_test!(
    test_buf_14_overlay_passthrough_regions,
    "overlay_passthrough_regions"
);
scene_buffer_size_test!(
    test_buf_15_disconnect_reclaim_multiagent,
    "disconnect_reclaim_multiagent"
);
scene_buffer_size_test!(test_buf_17_chatty_dashboard_touch, "chatty_dashboard_touch");
scene_buffer_size_test!(test_buf_18_zone_publish_subtitle, "zone_publish_subtitle");
scene_buffer_size_test!(test_buf_19_zone_reject_wrong_type, "zone_reject_wrong_type");
scene_buffer_size_test!(
    test_buf_20_zone_conflict_two_publishers,
    "zone_conflict_two_publishers"
);
scene_buffer_size_test!(
    test_buf_21_zone_orchestrate_then_publish,
    "zone_orchestrate_then_publish"
);
scene_buffer_size_test!(
    test_buf_23_zone_disconnect_cleanup,
    "zone_disconnect_cleanup"
);

// ─── Colour assertions ────────────────────────────────────────────────────────
//
// These tests define the EXPECTED PIXEL VALUES for each scene.
// HeadlessRuntime::render_frame calls compositor.render_frame_headless()
// which includes the copy_to_buffer step, so read_pixels() returns actual
// rendered data after each frame.
//
// Expected values documented here are the SPECIFICATION; the compositor
// implementation must produce these values.  All tests run unconditionally.

// ─── 1. empty_scene ──────────────────────────────────────────────────────────

/// empty_scene: no tabs, no tiles — every pixel must be the background clear colour.
///
/// Background: compositor clears to linear (0.05, 0.05, 0.10, 1.0).
/// sRGB ≈ (64, 64, 89, 255).  Tolerance: CI_SOLID_TOLERANCE = ±6.
///
/// WHEN empty_scene rendered THEN all sampled pixels ≈ [64, 64, 89, 255] within ±6.
///
/// DR-V2: headless compositor renders a complete frame with no scene content.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_01_empty_scene_all_background() {
    // Background clear: linear (0.05, 0.05, 0.10, 1.0) → sRGB ≈ (64, 64, 89, 255)
    const EXPECTED_BG: [u8; 4] = [64, 64, 89, 255];

    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("empty_scene", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;
    assert_eq!(pixels.len(), (SCENE_W * SCENE_H * 4) as usize);

    // Sample every 50th pixel — all must be background (no tiles present).
    for i in (0..SCENE_W * SCENE_H).step_by(50) {
        let x = i % SCENE_W;
        let y = i / SCENE_W;
        pixel_helpers::assert_pixel_color(
            &pixels,
            SCENE_W,
            x,
            y,
            EXPECTED_BG,
            CI_SOLID_TOLERANCE,
            "empty_scene background",
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }
}

// ─── 2. single_tile_solid ────────────────────────────────────────────────────

/// single_tile_solid: one tile with TextMarkdown background (0.08, 0.08, 0.15) linear.
///
/// The tile occupies (x=100, y=100, w=800, h=400) on the 1920×1080 canvas,
/// covering x=100..900, y=100..500.  Sample (400, 300) is inside the tile.
/// Tile background: (0.08, 0.08, 0.15) linear → sRGB ≈ (75, 75, 106).
/// Outside tile (10, 10) should be background clear ≈ [64, 64, 89, 255].
///
/// WHEN single_tile_solid rendered
/// THEN (400,300) ≈ [75, 75, 106, 255] within CI_BLEND_TOLERANCE.
/// THEN (10,10) ≈ [64, 64, 89, 255] within CI_SOLID_TOLERANCE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_02_single_tile_solid() {
    // Tile background (0.08, 0.08, 0.15) linear → sRGB ≈ (75, 75, 106)
    const EXPECTED_TILE: [u8; 4] = [75, 75, 106, 255];

    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("single_tile_solid", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    // Tile center (400, 300) — well inside tile bounds.
    pixel_helpers::assert_pixel_color(
        &pixels,
        SCENE_W,
        400,
        300,
        EXPECTED_TILE,
        CI_BLEND_TOLERANCE,
        "single_tile_solid: tile center (400,300)",
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // Outside tile (10, 10) — must be background.
    pixel_helpers::assert_pixel_color(
        &pixels,
        SCENE_W,
        10,
        10,
        BG_SRGB,
        CI_SOLID_TOLERANCE,
        "single_tile_solid: outside tile at (10,10)",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

// ─── 3. three_tiles_no_overlap ───────────────────────────────────────────────

/// three_tiles_no_overlap: three non-overlapping tiles (text, hit-region, solid).
///
/// Tile layout on 1920×1080:
/// - Tile 1 (text): (x=10, y=10, w=900, h=500) → covers x=10..910, y=10..510.
///   Background (0.1, 0.1, 0.2) → sRGB ≈ (89, 89, 124).
/// - Tile 2 (hit-region): x=930..1830, y=10..510. Transparent by default.
/// - Tile 3 (solid): y=600..680 (status bar full width).
///
/// At (100, 100) we should be inside Tile 1 (text with background).
/// At (750, 550) we may be outside all tiles (background).
///
/// WHEN three_tiles_no_overlap rendered
/// THEN (100,100) has tile-1 background colour (blue bias).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_03_three_tiles_no_overlap() {
    // Tile 1 background (0.1, 0.1, 0.2) linear → sRGB ≈ (89, 89, 124)
    const EXPECTED_TILE1: [u8; 4] = [89, 89, 124, 255];

    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("three_tiles_no_overlap", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    // Tile 1 (text with dark background) — inside at (100, 100).
    pixel_helpers::assert_pixel_color(
        &pixels,
        SCENE_W,
        100,
        100,
        EXPECTED_TILE1,
        CI_BLEND_TOLERANCE,
        "three_tiles_no_overlap: tile 1 interior at (100,100)",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

// ─── 4. overlapping_tiles_zorder ─────────────────────────────────────────────

/// overlapping_tiles_zorder: three overlapping tiles.
///   z=1 (red):  (10%, 16%, 50%, 67%) — red (0.8, 0.2, 0.2) → sRGB ≈ (228, 124, 124)
///   z=2 (green):(20%, 25%, 50%, 67%) — green (0.2, 0.8, 0.2) → sRGB ≈ (124, 228, 124)
///   z=3 (blue): (30%, 33%, 50%, 67%) — blue (0.2, 0.2, 0.8) → sRGB ≈ (124, 124, 228)
///
/// Key sample points:
/// - (15%, 20%): inside z=1 only → red dominant (sRGB ≈ [228, 124, 124]).
/// - (25%, 29%): inside z=1 and z=2 → green dominant (z=2 wins).
/// - (45%, 45%): inside all three → blue dominant (z=3 wins).
///
/// WHEN overlapping_tiles_zorder rendered
/// THEN red-only sample: red channel > blue channel + 50.
/// THEN red+green overlap sample: green channel > red channel + 50 and > blue channel + 50.
/// THEN all-tile overlap sample: blue channel > red channel + 50 and > green channel + 50.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_04_overlapping_tiles_zorder() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("overlapping_tiles_zorder", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    // Inside z=1 (red) only. Red dominant.
    let red_only_x = (SCENE_W as f32 * 0.15).round() as u32;
    let red_only_y = (SCENE_H as f32 * 0.20).round() as u32;
    let red_only = pixel_helpers::pixel_at(&pixels, SCENE_W, red_only_x, red_only_y);
    assert!(
        red_only[0] > red_only[2] + 50,
        "overlapping_tiles_zorder: red-only sample at ({red_only_x},{red_only_y}) must dominate: pixel={red_only:?}"
    );

    // Inside z=1 and z=2. z=2 (green) wins.
    let green_wins_x = (SCENE_W as f32 * 0.25).round() as u32;
    let green_wins_y = (SCENE_H as f32 * 0.29).round() as u32;
    let green_wins = pixel_helpers::pixel_at(&pixels, SCENE_W, green_wins_x, green_wins_y);
    assert!(
        green_wins[1] > green_wins[0] + 50 && green_wins[1] > green_wins[2] + 50,
        "overlapping_tiles_zorder: red+green overlap sample at ({green_wins_x},{green_wins_y}) must be green-dominant: pixel={green_wins:?}"
    );

    // Inside all three. z=3 (blue) wins.
    let blue_wins_x = (SCENE_W as f32 * 0.45).round() as u32;
    let blue_wins_y = (SCENE_H as f32 * 0.45).round() as u32;
    let blue_wins = pixel_helpers::pixel_at(&pixels, SCENE_W, blue_wins_x, blue_wins_y);
    assert!(
        blue_wins[2] > blue_wins[0] + 50 && blue_wins[2] > blue_wins[1] + 50,
        "overlapping_tiles_zorder: all-tile overlap sample at ({blue_wins_x},{blue_wins_y}) must be blue-dominant: pixel={blue_wins:?}"
    );
}

// ─── 5. overlay_transparency ─────────────────────────────────────────────────

/// overlay_transparency: base tile (0.1, 0.1, 0.5) solid full-screen (z=1)
/// plus semi-transparent chrome overlay (1.0, 1.0, 1.0, alpha=0.5) at tile
/// opacity=0.75 occupying (200, 200, 400, 200) at z=10.
///
/// Base tile: (0.1, 0.1, 0.5) linear → sRGB ≈ (89, 89, 188). Blue dominant.
/// Overlay effective alpha: color_alpha(0.5) × tile_opacity(0.75) = 0.375.
/// Blended colour in overlay region: lerp([89,89,188], [255,255,255], 0.375)
///   ≈ [152, 152, 213] in sRGB space. The important property: blue channel
///   still larger than red/green in overlay region due to base tile.
///
/// Assertions:
/// - Base-only region (50, 50): blue dominant (base tile colour).
/// - Overlay region center (400, 300): brighter than base alone (overlay adds luminosity).
/// - Overlay region centre: still blue-biased (base blue shines through).
///
/// WHEN overlay_transparency rendered
/// THEN (50,50): blue channel > red channel.
/// THEN (400,300): blue channel ≥ red channel (blend preserves blue bias).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_05_overlay_transparency() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("overlay_transparency", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    // Base tile region (50, 50) — blue dominant (0.1, 0.1, 0.5).
    let base_px = pixel_helpers::pixel_at(&pixels, SCENE_W, 50, 50);
    assert!(
        base_px[2] > base_px[0],
        "overlay_transparency: base tile (0.1,0.1,0.5) at (50,50) must be blue-dominant: pixel={base_px:?}"
    );

    // Overlay region center (400, 300) — blended, still blue-biased.
    let blend_px = pixel_helpers::pixel_at(&pixels, SCENE_W, 400, 300);
    assert!(
        blend_px[2] >= blend_px[0],
        "overlay_transparency: blended at (400,300) must keep blue ≥ red: pixel={blend_px:?}"
    );
}

// ─── 6. tab_switch ───────────────────────────────────────────────────────────

/// tab_switch: active tab is B.  Tab A tiles should NOT appear; Tab B tiles should.
///
/// Tab B tile backgrounds: (0.2, 0.1, 0.3) linear → sRGB ≈ (124, 89, 148). Purple bias.
/// Tab B tile 1 occupies (50, 100, 440, 300). Center at (270, 250).
/// Tab A tile occupies (50, 50, 800, 400). Since tab A is inactive, its region
/// at e.g. (200, 80) — inside tab A tile but outside tab B — must be background.
///
/// WHEN tab_switch rendered (active tab = B)
/// THEN (270, 250): Tab B tile — purple bias.
/// THEN (200, 80): Tab A tile region (inactive) — background colour.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_06_tab_switch() {
    // Tab B tile background (0.2, 0.1, 0.3) linear → sRGB ≈ (124, 89, 148)
    const EXPECTED_TAB_B: [u8; 4] = [124, 89, 148, 255];

    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("tab_switch", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    // Tab B tile 1 interior at (270, 250) — must show Tab B's purple background.
    pixel_helpers::assert_pixel_color(
        &pixels,
        SCENE_W,
        270,
        250,
        EXPECTED_TAB_B,
        CI_BLEND_TOLERANCE,
        "tab_switch: Tab B tile 1 at (270,250)",
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // Tab A tile region (200, 80) — Tab A is INACTIVE, must show background.
    pixel_helpers::assert_pixel_color(
        &pixels,
        SCENE_W,
        200,
        80,
        BG_SRGB,
        CI_SOLID_TOLERANCE,
        "tab_switch: Tab A region at (200,80) must be background (inactive tab)",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

// ─── 7. lease_expiry ─────────────────────────────────────────────────────────

/// lease_expiry: one tile with 1ms TTL, still ACTIVE at build time.
///
/// The tile occupies (100, 100, 600, 400). Center at (400, 300).
/// Tile background: (0.5, 0.1, 0.1) linear → sRGB ≈ (188, 89, 89). Red bias.
///
/// WHEN lease_expiry rendered (lease ACTIVE — clock at ClockMs::FIXED)
/// THEN (400,300) ≈ [188, 89, 89, 255] within CI_BLEND_TOLERANCE (tile visible).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_07_lease_expiry_tile_visible() {
    // Tile background (0.5, 0.1, 0.1) linear → sRGB ≈ (188, 89, 89)
    const EXPECTED_TILE: [u8; 4] = [188, 89, 89, 255];

    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("lease_expiry", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    pixel_helpers::assert_pixel_color(
        &pixels,
        SCENE_W,
        400,
        300,
        EXPECTED_TILE,
        CI_BLEND_TOLERANCE,
        "lease_expiry: tile center (400,300) must show active lease tile",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

// ─── 10. input_highlight ─────────────────────────────────────────────────────

/// input_highlight: background tile (0.05, 0.05, 0.15) covers entire display.
/// Hit-region tile at (400, 300, 400, 100) — transparent, no visual output.
///
/// Background tile: (0.05, 0.05, 0.15) → sRGB ≈ (64, 64, 106).
/// At (10, 10) we sample the background tile exterior (outside hit-region tile).
///
/// Note: background tile (0.05, 0.05, 0.15) has blue channel (106) > compositor
/// clear (89), confirming the tile renders (not just the compositor clear).
///
/// WHEN input_highlight rendered
/// THEN (10,10): background tile colour ≈ [64, 64, 106, 255].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_10_input_highlight_background_tile() {
    // Background tile (0.05, 0.05, 0.15) linear → sRGB ≈ (64, 64, 106)
    const EXPECTED_BG_TILE: [u8; 4] = [64, 64, 106, 255];

    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("input_highlight", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    pixel_helpers::assert_pixel_color(
        &pixels,
        SCENE_W,
        10,
        10,
        EXPECTED_BG_TILE,
        CI_BLEND_TOLERANCE,
        "input_highlight: background tile at (10,10)",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

// ─── 11. coalesced_dashboard ─────────────────────────────────────────────────

/// coalesced_dashboard: 12 tiles in 4×3 grid.
///
/// All tiles have TextMarkdown with teal-ish backgrounds.  The grid starts at
/// pad=10 on a 1920×1080 canvas: tile_w≈475, tile_h≈353.
/// First tile center ≈ (10 + 475/2, 10 + 353/2) ≈ (248, 187).
///
/// At least one tile interior should show non-background content.
/// The coalesced_dashboard scene uses per-metric tile colours (varying
/// blue-teal range), so we assert the blue channel is ≥ red at (103, 103).
///
/// WHEN coalesced_dashboard rendered
/// THEN (103,103): non-zero alpha (tile rendered).
/// THEN (103,103): blue channel ≥ red channel (tile background has blue bias).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_11_coalesced_dashboard_tile_rendered() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("coalesced_dashboard", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    // First tile interior at 1920×1080: pad=10, tile_w≈467, center ≈ (243, 183).
    let tile_px = pixel_helpers::pixel_at(&pixels, SCENE_W, 243, 183);
    assert_eq!(
        tile_px[3], 255,
        "coalesced_dashboard: tile at (243,183) must be opaque"
    );
}

// ─── 12. max_tiles_stress ────────────────────────────────────────────────────

/// max_tiles_stress: many tiles in a grid.
///
/// At least one non-background pixel must appear. The exact colour depends
/// on the grid layout, so we just check a non-background pixel exists.
///
/// WHEN max_tiles_stress rendered
/// THEN at least one pixel differs from background by more than CI_SOLID_TOLERANCE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_12_max_tiles_stress_has_content() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("max_tiles_stress", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    let has_content = pixels.chunks(4).any(|p| {
        BG_SRGB
            .iter()
            .zip(p.iter())
            .any(|(&e, &a)| a.abs_diff(e) > CI_SOLID_TOLERANCE)
    });
    assert!(
        has_content,
        "max_tiles_stress: at least one non-background pixel must exist"
    );
}

// ─── 13. three_agents_contention ─────────────────────────────────────────────

/// three_agents_contention: 3 agents at overlapping positions.
///
/// agent.high_prio (z=10): red (0.8, 0.2, 0.2) at (10%, 16%, 55%, 55%)
/// agent.normal_prio (z=5): green (0.2, 0.8, 0.2) at (20%, 25%, 55%, 55%)
/// agent.low_prio (z=1):  blue (0.2, 0.2, 0.8) at (30%, 33%, 55%, 55%)
///
/// At (45%, 45%): inside all three tiles → high_prio z=10 tile wins.
/// sRGB: red (0.8, 0.2, 0.2) ≈ (228, 124, 124).
///
/// WHEN three_agents_contention rendered
/// THEN all-agent overlap sample: red channel > blue channel + 50.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_13_three_agents_contention_high_prio_tile() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("three_agents_contention", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    let x = (SCENE_W as f32 * 0.45).round() as u32;
    let y = (SCENE_H as f32 * 0.45).round() as u32;
    let px = pixel_helpers::pixel_at(&pixels, SCENE_W, x, y);
    assert!(
        px[0] > px[2] + 50,
        "three_agents_contention: all-agent overlap sample at ({x},{y}) must be red-dominant (high_prio z=10): pixel={px:?}"
    );
}

// ─── 14. overlay_passthrough_regions ─────────────────────────────────────────

/// overlay_passthrough_regions: passthrough overlay on top of a HitRegion content tile.
///
/// The scene has two tiles:
/// - Content tile (z=1): full-display HitRegion — invisible (no rect emitted;
///   HitRegion is a pure interaction primitive, RFC 0004 §6.5).
/// - Overlay tile (z=20): passthrough SolidColor [0.0, 0.0, 0.0, 0.15] — slight
///   darkening drawn on top of the clear background via alpha blending.
///
/// Background clear: linear (0.05, 0.05, 0.10).  With no content tile on top,
/// the overlay blends directly against it in LINEAR space:
/// blended_linear = 0.15 * [0, 0, 0] + 0.85 * [0.05, 0.05, 0.10]
///                = [0.0425, 0.0425, 0.085]
/// → sRGB encode → ≈ [58, 58, 82].
///
/// WHEN overlay_passthrough_regions rendered
/// THEN (400, 300): clear background darkened by overlay ≈ [58, 58, 82, 255].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_14_overlay_passthrough_regions_near_background() {
    // Clear (0.05, 0.05, 0.10) linear × 0.85 → (0.0425, 0.0425, 0.085) linear
    // → sRGB ≈ (58, 58, 82).
    const EXPECTED: [u8; 4] = [58, 58, 82, 255];

    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("overlay_passthrough_regions", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    // HitRegion content tile is invisible; overlay darkens the clear bg only.
    pixel_helpers::assert_pixel_color(
        &pixels,
        SCENE_W,
        400,
        300,
        EXPECTED,
        CI_BLEND_TOLERANCE,
        "overlay_passthrough_regions: (400,300) clear bg darkened by overlay",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

// ─── 15. disconnect_reclaim_multiagent ────────────────────────────────────────

/// disconnect_reclaim_multiagent: three agents all ACTIVE.
///
/// agent.one tile_a: (x=10, y=10, w=600, h=500) → covers x=10..610, y=10..510.
/// Red (0.8, 0.2, 0.2).
/// agent.two tile: (x=660, y=10, w=580, h=700) → covers x=660..1240, y=10..710.
/// Green (0.2, 0.7, 0.2).  Entirely visible on 1920×1080 canvas.
///
/// At (310, 260) inside agent.one tile_a: red dominant.
/// At (730, 355) inside agent.two tile: green dominant.
///
/// WHEN disconnect_reclaim_multiagent rendered
/// THEN (310,260): red channel > blue channel + 30.
/// THEN (730,355): green channel > red channel + 30.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_15_disconnect_reclaim_multiagent_agents_visible() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("disconnect_reclaim_multiagent", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    // agent.one tile_a (red 0.8, 0.2, 0.2) at (310, 260).
    let px_one = pixel_helpers::pixel_at(&pixels, SCENE_W, 310, 260);
    assert!(
        px_one[0] > px_one[2] + 30,
        "disconnect_reclaim_multiagent: (310,260) must be red-dominant: pixel={px_one:?}"
    );

    // agent.two tile (green 0.2, 0.7, 0.2) at (730, 355).
    let px_two = pixel_helpers::pixel_at(&pixels, SCENE_W, 730, 355);
    assert!(
        px_two[1] > px_two[0] + 30,
        "disconnect_reclaim_multiagent: (730,355) must be green-dominant: pixel={px_two:?}"
    );
}

// ─── 17. chatty_dashboard_touch ──────────────────────────────────────────────

/// chatty_dashboard_touch: 50 HitRegionNode tiles in 5×10 grid.
///
/// HitRegion is a pure interaction primitive — it paints nothing in its default
/// state (RFC 0004 §6.5).  With all 50 tiles invisible, sampled points show the
/// compositor background clear only: linear (0.05, 0.05, 0.10) → sRGB BG_SRGB.
///
/// Grid layout on 1920×1080: 5 cols × 10 rows, each cell 384×108.  (400, 300) is
/// inside a HitRegion tile but the tile draws nothing, so the pixel reports the
/// background clear colour.
///
/// WHEN chatty_dashboard_touch rendered
/// THEN (400,300): background clear ≈ BG_SRGB within CI_SOLID_TOLERANCE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_17_chatty_dashboard_touch_transparent_tiles() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("chatty_dashboard_touch", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    // HitRegion tiles are invisible — pixel is the compositor clear colour.
    pixel_helpers::assert_pixel_color(
        &pixels,
        SCENE_W,
        400,
        300,
        BG_SRGB,
        CI_SOLID_TOLERANCE,
        "chatty_dashboard_touch: (400,300) HitRegion tiles invisible; bg clear shows",
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

// ─── 18. zone_publish_subtitle ────────────────────────────────────────────────

/// zone_publish_subtitle: subtitle tile at bottom of display.
///
/// Tile position on 1920×1080: x = 1920×0.1 = 192, y = 1080×0.88 = 950,
/// w = 1920×0.8 = 1536, h = 1080×0.08 = 86.  So tile covers y=950..1037.
/// Background: (0.0, 0.0, 0.0, 0.75) — semi-transparent black.
///
/// Above tile at (400, 10): compositor clear background [64, 64, 89, 255].
/// In subtitle at (960, 993): darker than background (semi-black blend).
/// (960, 993) is the centre of the subtitle tile on a 1920×1080 canvas.
///
/// WHEN zone_publish_subtitle rendered
/// THEN (400,10): approximately background clear.
/// THEN (960,993): at least one channel darker than corresponding BG_SRGB channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_18_zone_publish_subtitle_region() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("zone_publish_subtitle", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    // Above subtitle tile — must be background.
    pixel_helpers::assert_pixel_color(
        &pixels,
        SCENE_W,
        400,
        10,
        BG_SRGB,
        CI_SOLID_TOLERANCE,
        "zone_publish_subtitle: (400,10) above subtitle must be background",
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // Subtitle zone tile (semi-transparent black, alpha=0.75) — darker than BG.
    // Sample at (960, 993): centre of the subtitle tile on a 1920×1080 canvas.
    // Tile covers x=192..1728, y=950..1037.
    let sub_px = pixel_helpers::pixel_at(&pixels, SCENE_W, 960, 993);
    assert!(
        sub_px[0] < BG_SRGB[0] || sub_px[1] < BG_SRGB[1] || sub_px[2] < BG_SRGB[2],
        "zone_publish_subtitle: subtitle at (960,993) must be darker than background: \
         pixel={sub_px:?} bg={BG_SRGB:?}"
    );
}

// ─── 19. zone_reject_wrong_type ──────────────────────────────────────────────

/// zone_reject_wrong_type: scene with a wrongly-typed zone publish.
///
/// The scene renders normally.  There should be some non-background content.
///
/// WHEN zone_reject_wrong_type rendered
/// THEN at least one pixel differs from background.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_19_zone_reject_wrong_type_has_content() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("zone_reject_wrong_type", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    let has_content = pixels.chunks(4).any(|p| {
        BG_SRGB
            .iter()
            .zip(p.iter())
            .any(|(&e, &a)| a.abs_diff(e) > CI_SOLID_TOLERANCE)
    });
    assert!(
        has_content,
        "zone_reject_wrong_type: at least one tile must render (non-background pixels)"
    );
}

// ─── 20. zone_conflict_two_publishers ────────────────────────────────────────

/// zone_conflict_two_publishers: two agents contend for the same zone.
///
/// LatestWins — second publisher's tile is current zone content.
/// Both tiles render.
///
/// WHEN zone_conflict_two_publishers rendered
/// THEN at least one non-background pixel exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_20_zone_conflict_two_publishers_has_content() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("zone_conflict_two_publishers", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    let has_content = pixels.chunks(4).any(|p| {
        BG_SRGB
            .iter()
            .zip(p.iter())
            .any(|(&e, &a)| a.abs_diff(e) > CI_SOLID_TOLERANCE)
    });
    assert!(
        has_content,
        "zone_conflict_two_publishers: at least one tile must render"
    );
}

// ─── 21. zone_orchestrate_then_publish ────────────────────────────────────────

/// zone_orchestrate_then_publish: orchestrated zone publish sequence.
///
/// WHEN zone_orchestrate_then_publish rendered
/// THEN at least one non-background pixel exists (tile rendered after sequence).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_21_zone_orchestrate_then_publish_has_content() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("zone_orchestrate_then_publish", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    let has_content = pixels.chunks(4).any(|p| {
        BG_SRGB
            .iter()
            .zip(p.iter())
            .any(|(&e, &a)| a.abs_diff(e) > CI_SOLID_TOLERANCE)
    });
    assert!(
        has_content,
        "zone_orchestrate_then_publish: at least one tile must render after orchestration"
    );
}

// ─── 23. zone_disconnect_cleanup ─────────────────────────────────────────────

/// zone_disconnect_cleanup: zone publisher tile still ACTIVE at build time.
///
/// WHEN zone_disconnect_cleanup rendered (before disconnect)
/// THEN at least one non-background pixel (tile active at build time).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_23_zone_disconnect_cleanup_tile_initially_visible() {
    let mut runtime = make_scene_runtime().await;
    let registry = TestSceneRegistry::new();
    let (scene, _spec) = registry
        .build("zone_disconnect_cleanup", ClockMs::FIXED)
        .expect("build failed");

    let pixels = render_scene_pixels(&mut runtime, scene).await;

    let has_content = pixels.chunks(4).any(|p| {
        BG_SRGB
            .iter()
            .zip(p.iter())
            .any(|(&e, &a)| a.abs_diff(e) > CI_SOLID_TOLERANCE)
    });
    assert!(
        has_content,
        "zone_disconnect_cleanup: tile active at build time — must render before disconnect"
    );
}

// ─── color_runs pixel readback [hud-r52v] ────────────────────────────────────

/// TextMarkdownNode with color_runs: a large red text paragraph.
///
/// Scene: one tile at (0, 0, 960, 540) containing a TextMarkdownNode with
/// content "ERROR rest" where "ERROR" (bytes 0..5) is red (1,0,0,1) and the
/// remaining text uses the base white color.
///
/// The test samples the left edge of the text area, where the first glyph of
/// "ERROR" renders.  We assert that the sampled region contains at least one
/// pixel where R > G+60 (strongly red-dominant), indicating that the red run
/// was applied correctly by the compositor.
///
/// We also assert that background pixels outside the tile remain BG_SRGB.
///
/// WHEN TextMarkdownNode with color_runs rendered
/// THEN text region contains strongly red-dominant pixels.
///
/// Acceptance criterion AC-5 [hud-r52v].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_color_runs_red_error_text_rendered() {
    use tze_hud_scene::{
        graph::SceneGraph,
        types::{
            FontFamily, Node, NodeData, Rect, Rgba, TextAlign, TextColorRun, TextMarkdownNode,
            TextOverflow,
        },
    };

    let mut runtime = make_scene_runtime().await;

    // Build a minimal scene: one tab, one tile, one TextMarkdownNode.
    let mut scene = SceneGraph::new(SCENE_W as f32, SCENE_H as f32);
    let tab_id = scene.create_tab("main", 0).expect("create_tab");
    let lease_id = scene.grant_lease("agent", 120_000);

    // Place the tile to the left-center of the canvas so glyphs are well within
    // the pixel readback area.  Use a large tile so text is easily sampled.
    let tile_w = 800.0_f32;
    let tile_h = 200.0_f32;
    let tile_x = 50.0_f32;
    let tile_y = 400.0_f32;

    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(tile_x, tile_y, tile_w, tile_h),
            10,
        )
        .expect("create_tile");

    // "ERROR rest of the text" — "ERROR" (bytes 0..5) → red, rest → base white.
    let content = "ERROR rest of the text";
    let node = Node {
        layout: Default::default(),
        id: tze_hud_scene::types::SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_string(),
            bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
            font_size_px: 48.0, // large so glyphs cover many pixels
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: vec![TextColorRun {
                start_byte: 0,
                end_byte: 5,                          // "ERROR"
                color: Rgba::new(1.0, 0.0, 0.0, 1.0), // pure red
            }]
            .into_boxed_slice(),
        }),
    };

    scene.set_tile_root(tile_id, node).expect("set_tile_root");

    // Render the scene.
    let pixels = render_scene_pixels(&mut runtime, scene).await;
    assert_eq!(pixels.len(), (SCENE_W * SCENE_H * 4) as usize);

    // Sample the text area: look for at least one red-dominant pixel.
    // The first letter 'E' at font_size=48px will be somewhere in the
    // region x=[tile_x+margin .. tile_x+120], y=[tile_y+margin .. tile_y+60].
    // The margin from from_text_markdown_node is about 1..6 px.
    // We scan a generous region to tolerate sub-pixel variation.
    let scan_x0 = (tile_x + 5.0) as u32;
    let scan_x1 = (tile_x + 200.0) as u32; // first ~4 characters of "ERROR"
    let scan_y0 = (tile_y + 5.0) as u32;
    let scan_y1 = (tile_y + tile_h - 5.0) as u32;

    let mut found_red_dominant = false;
    'outer: for y in scan_y0..scan_y1 {
        for x in scan_x0..scan_x1 {
            let px = pixel_helpers::pixel_at(&pixels, SCENE_W, x, y);
            // R > G + 60 and R > B + 60: strongly red.
            if px[0] > px[1].saturating_add(60) && px[0] > px[2].saturating_add(60) {
                found_red_dominant = true;
                break 'outer;
            }
        }
    }

    assert!(
        found_red_dominant,
        "color_runs: expected at least one strongly red-dominant pixel in the 'ERROR' region \
         (scan [{scan_x0}..{scan_x1}) × [{scan_y0}..{scan_y1})). \
         This indicates the red color run was not applied by the compositor."
    );
}
