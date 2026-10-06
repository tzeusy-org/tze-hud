//! # tze_hud_runtime
//!
//! Runtime kernel for tze_hud — the **orchestration layer**.
//!
//! ## Authority Map
//!
//! | Authority | Module | Role |
//! |-----------|--------|------|
//! | Resource accounting | `tze_hud_resource` | Decoded-byte budget registry; GC; dedup |
//! | Mutation budgets | `tze_hud_runtime::mutation_budget_bridge` | Per-session and aggregate hard caps |
//! | Override state | `tze_hud_runtime::shell::safe_mode` | Sole writer of safe-mode state |
//! | Scene orchestration | `tze_hud_runtime` (this crate) | Wires authority modules; drives pipeline |
//!
//! ## Threads
//!
//! Four fixed thread groups — no dynamic spawning after startup:
//!
//! - **Main thread**: winit event loop, input drain, local feedback,
//!   surface.present() when signalled by the frame-ready watch.
//! - **Compositor thread**: scene commit, render encode, GPU submit.
//!   Exclusively owns wgpu Device and Queue.
//! - **Network threads**: Tokio multi-thread runtime for gRPC, MCP, sessions.
//!
//! `pipeline.rs` holds the `HitTestSnapshot` (ArcSwap-backed lock-free tile
//! bounds) the main thread reads for local feedback.
//!
//! ## Feature flags
//!
//! | Feature | Purpose |
//! |---------|---------|
//! | `headless` | Enable headless GPU surface (required for CI and tests) |
//! | `dev-mode` | Allow `HeadlessConfig { config_toml: None }` — grants unrestricted capabilities to all agents; opt into compositor/telemetry headless work observations. **MUST NOT be enabled in production binaries.** Safe for integration tests, examples, and local development. |
//!
//! In unit tests (compiled with `cfg(test)`), the `dev-mode` bypass is also
//! available without the feature flag, because unit tests run inside the
//! library and `cfg(test)` is set by the compiler. Integration test binaries
//! (in `tests/` directories) require `features = ["dev-mode"]` explicitly.
//!
pub(crate) mod channels;
pub(crate) mod degradation;
pub mod diag;
pub mod element_store;
pub(crate) mod firewall;
pub mod headless;
pub mod http;
pub(crate) mod idle_efficiency;
pub(crate) mod mcp;
pub(crate) mod mutation_budget_bridge;
pub(crate) mod net_addrs;
pub mod operator;
pub mod pairing;
pub(crate) mod pipeline;
pub mod portal_projection_driver;
pub mod portal_tokens;
pub(crate) mod runtime_context;
pub(crate) mod scene_startup;
pub(crate) mod shell;
#[cfg(test)]
pub(crate) mod test_support;
pub mod threads;
mod widget_hover;
pub(crate) mod widget_runtime_registration;
pub(crate) mod widget_startup;
pub mod window;
pub mod windowed;

pub use firewall::remote as remote_firewall;
pub use headless::HeadlessRuntime;
pub use idle_efficiency::{IdleEfficiencyCounters, RuntimeWakeupSource};
pub use mcp::{McpServerConfig, start_mcp_http_server};
pub use pipeline::{
    INPUT_TO_NEXT_PRESENT_BUDGET_US, STAGE3_BUDGET_US, STAGE4_BUDGET_US, STAGE5_BUDGET_US,
};
pub use shell::chrome::{ChromeState, collect_diagnostic};
pub use shell::freeze::{
    EnqueueResult, FreezeQueue, MutationTrafficClass, QueuedMutation, classify_mutation_batch,
};
