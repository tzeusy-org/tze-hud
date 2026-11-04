//! GPU availability canary — always-on test that enforces the GPU contract.
//!
//! ## Purpose
//!
//! Compositor GPU tests silently self-skip (bare `return`) when no wgpu adapter
//! is found or `TZE_HUD_SKIP_GPU_TESTS=1` is set.  On a host that is supposed
//! to have a GPU (real or software), a silently-skipped suite is
//! indistinguishable from a passing one, which masks regressions (see hud-2sxfk,
//! hud-5jbra).
//!
//! This module provides a single **always-on** canary test:
//!
//! - If `TZE_HUD_REQUIRE_GPU=1` is set, the test asserts a wgpu adapter IS
//!   available.  Failure is loud and CI-blocking.
//! - If the variable is absent (normal dev / headless builds) the test passes
//!   immediately — no GPU required.
//!
//! ## CI integration
//!
//! Set `TZE_HUD_REQUIRE_GPU=1` in any CI job that installs Mesa / llvmpipe or
//! runs on a GPU-enabled runner.  The canary will then fail the job if the
//! adapter disappears (e.g. missing mesa-vulkan-drivers, broken llvmpipe build).
//!
//! ## Skip contract
//!
//! `TZE_HUD_SKIP_GPU_TESTS=1` is intentionally honoured: if the operator
//! explicitly opts out of GPU tests, the canary does not override that choice.
//! `TZE_HUD_REQUIRE_GPU=1` and `TZE_HUD_SKIP_GPU_TESTS=1` are mutually
//! exclusive; setting both is a misconfiguration caught by a separate assertion.

use tze_hud_compositor::{Compositor, CompositorError};

/// Always-on canary: fails loudly when GPU tests would silently skip on a
/// host that is supposed to have a GPU.
///
/// | `TZE_HUD_REQUIRE_GPU` | `TZE_HUD_SKIP_GPU_TESTS` | Outcome |
/// |-----------------------|--------------------------|---------|
/// | unset                 | any                      | pass (no-op) |
/// | `1`                   | `1`                      | **panic** (misconfiguration) |
/// | `1`                   | unset / other            | assert adapter available |
#[tokio::test]
async fn gpu_adapter_available_when_required() {
    let require_gpu = std::env::var("TZE_HUD_REQUIRE_GPU")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);

    if !require_gpu {
        // No GPU required in this environment — canary passes unconditionally.
        return;
    }

    let skip_gpu = std::env::var("TZE_HUD_SKIP_GPU_TESTS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);

    assert!(
        !skip_gpu,
        "Misconfiguration: TZE_HUD_REQUIRE_GPU=1 and TZE_HUD_SKIP_GPU_TESTS=1 \
         are mutually exclusive. A GPU-required CI lane must not also opt out of \
         GPU tests. Unset one of the two variables."
    );

    // TZE_HUD_REQUIRE_GPU=1 is set and GPU tests are not opted out.
    // Attempt to acquire a wgpu adapter.  Any outcome other than Ok is a hard
    // failure — the adapter is required and its absence is a CI regression.
    let compositor_init = {
        let _guard = crate::common::lock_gpu_init().await;
        Compositor::new_headless(64, 64).await
    };
    match compositor_init {
        Ok(_compositor) => {
            // Adapter found — GPU lane is healthy.
            eprintln!(
                "[gpu_canary] wgpu adapter available (TZE_HUD_REQUIRE_GPU=1): \
                 GPU test suite will run."
            );
        }
        Err(CompositorError::NoAdapter) => {
            panic!(
                "GPU adapter required (TZE_HUD_REQUIRE_GPU=1) but none found.\n\
                 \n\
                 This failure means the GPU test suite would have silently \
                 self-skipped, masking potential regressions.\n\
                 \n\
                 Likely causes:\n\
                 - Mesa / llvmpipe not installed (apt: mesa-vulkan-drivers)\n\
                 - Vulkan loader missing (apt: libvulkan1)\n\
                 - HEADLESS_FORCE_SOFTWARE=1 not set when using llvmpipe\n\
                 - GPU driver regression on a hardware runner\n\
                 \n\
                 Fix: ensure the CI job installs Mesa and sets \
                 HEADLESS_FORCE_SOFTWARE=1, or remove TZE_HUD_REQUIRE_GPU=1 \
                 if this lane genuinely has no GPU."
            );
        }
        Err(e) => {
            panic!(
                "GPU adapter required (TZE_HUD_REQUIRE_GPU=1) but compositor \
                 initialisation failed with an unexpected error: {e}\n\
                 \n\
                 This is not a NoAdapter error — investigate the compositor \
                 initialisation path for the root cause."
            );
        }
    }
}
