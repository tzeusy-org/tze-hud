//! Shared helpers for Layer 1 pixel readback tests.
//!
//! This module defines tolerance constants, colour-assertion helpers, and
//! scene-rendering utilities used by `pixel_readback.rs`.  It is compiled
//! only when running integration tests (`#[cfg(test)]` is implicit for
//! `tests/` files in a Rust crate).
//!
//! # Tolerance Strategy
//!
//! Per `heart-and-soul/validation.md` line 117 the ideal tolerances are:
//! - Solid fills:     ±1 per channel (ideal), ±6 per channel (llvmpipe CI)
//! - Alpha blending:  ±2 per channel (ideal), ±8 per channel (llvmpipe CI)
//!
//! Tests assert against the CI constants so they pass on software-rasterised
//! runners (Mesa llvmpipe, SwiftShader) and real GPU alike.  The IDEAL_*
//! constants document the tighter target for hardware validation.
//!
//! # sRGB Encoding
//!
//! The compositor render target uses `Rgba8UnormSrgb`.  The GPU writes
//! sRGB-encoded bytes to the readback buffer.  Expected values in this file
//! are therefore **sRGB**, not linear.
//!
//! Approximate conversion for reference:
//! ```text
//! linear 0.05  → sRGB ≈  64
//! linear 0.08  → sRGB ≈  75
//! linear 0.10  → sRGB ≈  89
//! linear 0.15  → sRGB ≈ 106
//! linear 0.20  → sRGB ≈ 124
//! linear 0.30  → sRGB ≈ 148
//! linear 0.40  → sRGB ≈ 174
//! linear 0.50  → sRGB ≈ 188
//! linear 0.70  → sRGB ≈ 214
//! linear 0.80  → sRGB ≈ 228
//! linear 1.00  → sRGB = 255
//! ```

// ─── Tolerance constants ─────────────────────────────────────────────────────

/// CI tolerance for solid-fill pixels on llvmpipe / SwiftShader (±6/channel).
/// All pixel assertions in `pixel_readback.rs` use this constant.
pub const CI_SOLID_TOLERANCE: u8 = 6;

/// CI tolerance for alpha-blended pixels on llvmpipe / SwiftShader (±8/channel).
/// All pixel assertions involving alpha blending use this constant.
pub const CI_BLEND_TOLERANCE: u8 = 8;

// ─── Background clear colour ──────────────────────────────────────────────────

/// Expected background clear colour in sRGB.
///
/// Compositor clears to linear (0.05, 0.05, 0.10, 1.0).
/// sRGB ≈ (64, 64, 89, 255).
pub const BG_SRGB: [u8; 4] = [64, 64, 89, 255];

// ─── Dimensions used across all scene tests ────────────────────────────────

/// Display width used for all scene pixel tests.
///
/// Must match `TestSceneRegistry::new()` default (1920×1080).  Scenes are
/// built with `TestSceneRegistry::new()` whose tile bounds are relative to a
/// 1920×1080 canvas; using a smaller runtime would cause `BoundsOutOfRange`
/// errors during scene construction.
pub const SCENE_W: u32 = 1920;

/// Display height used for all scene pixel tests.
pub const SCENE_H: u32 = 1080;

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build a [`HeadlessRuntime`] sized for the scene tests (1920×1080).
///
/// Uses 1920×1080 to match [`tze_hud_scene::test_scenes::TestSceneRegistry::new`]'s
/// default display area.  Scenes are built with tile bounds relative to 1920×1080;
/// a smaller runtime would cause `BoundsOutOfRange` at scene construction time.
pub async fn make_scene_runtime() -> tze_hud_runtime::HeadlessRuntime {
    use tze_hud_runtime::headless::HeadlessConfig;
    tze_hud_runtime::HeadlessRuntime::new(HeadlessConfig {
        width: SCENE_W,
        height: SCENE_H,
        grpc_port: 0,
        bind_all_interfaces: false,
        psk: "test".to_string(),
        config_toml: None,
    })
    .await
    .expect("HeadlessRuntime::new failed")
}

/// Replace the runtime's scene with `scene`, render one frame, and return
/// the RGBA8 pixel buffer.
pub async fn render_scene_pixels(
    runtime: &mut tze_hud_runtime::HeadlessRuntime,
    scene: tze_hud_scene::graph::SceneGraph,
) -> Vec<u8> {
    {
        let state = runtime.shared_state().lock().await;
        *state.scene.lock().await = scene;
    }
    runtime.render_frame().await;
    runtime.read_pixels()
}
