//! Boots the headless runtime with the example's committed
//! `config/production.toml` and its paired agents (`config/agents.toml`).
//!
//! If either file is malformed or config parsing regresses, `cargo run -p
//! vertical_slice` would fail at startup; this makes that CI-visible. The
//! lifecycle itself is covered by the `vertical_slice` binary's own tests, and
//! the unpaired-PSK gate by `tze_hud_app`'s `production_boot`.
//!
//!   just production-boot
//!
//! This binary has one GPU test. It needs no in-process initialization gate;
//! the recipe still pins the Vulkan loader to llvmpipe on Linux.

use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;

const PRODUCTION_CONFIG: &str = include_str!("../config/production.toml");
const PRODUCTION_AGENTS: &str = include_str!("../config/agents.toml");

#[tokio::test]
async fn production_config_boot_succeeds() {
    let agents = tze_hud_config::AgentsFile::parse(PRODUCTION_AGENTS)
        .and_then(|file| file.directory())
        .expect("config/agents.toml must be valid");
    let result = HeadlessRuntime::new(HeadlessConfig {
        width: 320,
        height: 240,
        grpc_port: 0, // No gRPC server: pure boot test.
        agents,
        config_toml: Some(PRODUCTION_CONFIG.to_string()),
    })
    .await;
    assert!(
        result.is_ok(),
        "runtime failed to start with production config: {:?}",
        result.err()
    );
}
