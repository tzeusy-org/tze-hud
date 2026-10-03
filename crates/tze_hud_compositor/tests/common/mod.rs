//! Shared helpers for the compositor GPU integration tests.

use tokio::sync::Mutex;
use tze_hud_compositor::{Compositor, CompositorError};

/// Serializes headless device creation within one test binary.
///
/// Concurrent wgpu/Vulkan device construction from the default parallel libtest
/// harness can wedge the driver. Every multi-test binary under `tests/` must
/// create its device through this helper; single-test binaries (`gpu_canary`,
/// `widget_raster_count`) need no guard. Run GPU suites with `just test-gpu`,
/// which also pins the Vulkan loader to Mesa llvmpipe.
static GPU_INIT_MUTEX: Mutex<()> = Mutex::const_new(());

pub async fn new_headless_serialized(w: u32, h: u32) -> Result<Compositor, CompositorError> {
    let _guard = GPU_INIT_MUTEX.lock().await;
    Compositor::new_headless(w, h).await
}
