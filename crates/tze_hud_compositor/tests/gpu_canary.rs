//! A required lane must create a device even when running only the canary.

#[tokio::test]
async fn gpu_adapter_available_when_required() {
    if crate::common::gpu_required() {
        crate::common::new_gpu(64, 64).await.expect("GPU canary");
        eprintln!("[gpu_canary] wgpu adapter available (TZE_HUD_REQUIRE_GPU=1)");
    }
}
