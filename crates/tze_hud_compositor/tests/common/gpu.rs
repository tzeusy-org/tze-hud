//! Headless device setup shared by the compositor's unit and integration suites.

use super::{Compositor, CompositorError, HeadlessSurface};
use tokio::sync::Mutex;

/// Serialize device creation within each test binary. Concurrent wgpu/Vulkan
/// construction can wedge the driver. Release this guard before rendering.
pub(super) static GPU_INIT_MUTEX: Mutex<()> = Mutex::const_new(());

pub(crate) fn gpu_required() -> bool {
    std::env::var("TZE_HUD_REQUIRE_GPU").is_ok_and(|value| value.trim() == "1")
}

pub(crate) async fn new_gpu(width: u32, height: u32) -> Option<(Compositor, HeadlessSurface)> {
    let required = gpu_required();
    if std::env::var("TZE_HUD_SKIP_GPU_TESTS").is_ok_and(|value| value.trim() == "1") {
        assert!(
            !required,
            "TZE_HUD_REQUIRE_GPU=1 conflicts with TZE_HUD_SKIP_GPU_TESTS=1"
        );
        eprintln!("SKIPPED (no GPU): TZE_HUD_SKIP_GPU_TESTS=1");
        return None;
    }
    let device = {
        let _guard = GPU_INIT_MUTEX.lock().await;
        Compositor::new_headless(width, height).await
    };
    match device {
        Ok(compositor) => {
            let surface = HeadlessSurface::new(&compositor.device, width, height);
            Some((compositor, surface))
        }
        Err(CompositorError::NoAdapter) => {
            assert!(
                !required,
                "TZE_HUD_REQUIRE_GPU=1 but no wgpu adapter; check Mesa/llvmpipe setup"
            );
            eprintln!("SKIPPED (no GPU): no wgpu adapter");
            None
        }
        Err(error) => panic!("initialize headless compositor: {error}"),
    }
}
