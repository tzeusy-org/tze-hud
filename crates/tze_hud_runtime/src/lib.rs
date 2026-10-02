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
//! | Override state | `tze_hud_runtime::shell::SafeModeController` | Sole writer of freeze/safe-mode flags |
//! | Scene orchestration | `tze_hud_runtime` (this crate) | Wires authority modules; drives pipeline |
//!
//! ## Frame Pipeline
//!
//! Orchestrates the 8-stage frame pipeline:
//!
//! | Stage | Name               | Thread     | Budget (p99) |
//! |-------|--------------------|------------|-------------|
//! | 1     | Input Drain        | Main       | < 500µs     |
//! | 2     | Local Feedback     | Main       | < 500µs     |
//! | 3     | Mutation Intake    | Compositor | < 1ms       |
//! | 4     | Scene Commit       | Compositor | < 1ms       |
//! | 5     | Layout Resolve     | Compositor | < 1ms       |
//! | 6     | Render Encode      | Compositor | < 4ms       |
//! | 7     | GPU Submit+Present | Comp+Main  | < 8ms       |
//! | 8     | Telemetry Emit     | Telemetry  | < 200µs     |
//!
//! See `pipeline.rs` for the `FramePipeline` orchestrator and `HitTestSnapshot`
//! (ArcSwap-backed lock-free tile bounds for Stage 2).
//!
//! ## Architecture (spec §Thread Model, line 19)
//!
//! Four fixed thread groups — no dynamic spawning after startup:
//!
//! - **Main thread**: winit event loop, input drain, local feedback,
//!   surface.present() when signalled by FrameReadySignal.
//! - **Compositor thread**: scene commit, render encode, GPU submit.
//!   Exclusively owns wgpu Device and Queue.
//! - **Network threads**: Tokio multi-thread runtime for gRPC, MCP, sessions.
//! - **Telemetry thread**: async structured emission.
//!
//! Inter-thread communication uses bounded channels only (spec §Channel Topology,
//! line 272). See [`channels`] for the complete channel inventory.
//!
//! ## Feature flags
//!
//! | Feature | Purpose |
//! |---------|---------|
//! | `headless` | Enable headless GPU surface (required for CI and tests) |
//! | `dev-mode` | Allow `HeadlessConfig { config_toml: None }` — grants unrestricted capabilities to all agents. **MUST NOT be enabled in production binaries.** Safe for integration tests, examples, and local development. |
//!
//! In unit tests (compiled with `cfg(test)`), the `dev-mode` bypass is also
//! available without the feature flag, because unit tests run inside the
//! library and `cfg(test)` is set by the compiler. Integration test binaries
//! (in `tests/` directories) require `features = ["dev-mode"]` explicitly.
//!
pub mod channels;
pub mod degradation;
pub mod diag;
pub mod element_store;
pub mod font_loader;
pub mod gpu_lock;
pub mod headless;
pub mod idle_efficiency;
pub mod mcp;
pub mod mutation_budget_bridge;
pub mod pipeline;
pub mod portal_cadence;
pub mod portal_projection_driver;
pub mod portal_tokens;
pub mod reload_triggers;
pub mod resident_grpc_bridge;
pub mod runtime_context;
pub mod scene_startup;
pub mod shell;
#[cfg(test)]
pub(crate) mod test_support;
pub mod threads;
mod widget_hover;
pub mod widget_runtime_registration;
pub mod widget_startup;
pub mod window;
pub mod windowed;

pub use channels::{
    BackpressureReceiver,
    // Backpressure channel types
    BackpressureSender,
    ChannelSet,
    CoalesceKeyReceiver,
    // Coalesce-key channel types
    CoalesceKeySender,
    CoalesceKeyed,
    EphemeralEventKind,
    FrameReadyRx,
    // FrameReadySignal
    FrameReadyTx,
    // Capacity constants
    INPUT_EVENT_CAPACITY,
    // Message payloads
    InputEvent,
    InputEventKind,
    LocalPatchKind,
    OverflowCounters,
    // Ring-buffer types
    RingBuffer,
    SCENE_EVENT_EPHEMERAL_CAPACITY,
    SCENE_EVENT_STATE_STREAM_CAPACITY,
    SCENE_EVENT_TRANSACTIONAL_CAPACITY,
    SCENE_LOCAL_PATCH_CAPACITY,
    SceneEventEphemeral,
    SceneEventStateStream,
    SceneEventTransactional,
    SceneLocalPatch,
    StateStreamEventKind,
    StateStreamKey,
    StateStreamPayload,
    TELEMETRY_RECORD_CAPACITY,
    TelemetryRecord,
    TransactionalEventKind,
    backpressure_channel,
    coalesce_key_channel,
    frame_ready_channel,
};
pub use degradation::{DegradationConfig, DegradationController, DegradationLevel};
pub use headless::HeadlessRuntime;
pub use idle_efficiency::{
    IdleEfficiencyCounters, IdleEfficiencyDeltaError, IdleEfficiencySnapshot, RuntimeWakeupSource,
};
pub use mcp::{McpServerConfig, start_mcp_http_server};
pub use mutation_budget_bridge::RuntimeMutationBudgetEnforcer;
pub use runtime_context::{FallbackPolicy, RuntimeContext, SharedRuntimeContext};
pub use shell::chrome::{
    AgentVisibleTopology, AuditPayload, AuditTrigger, ChromeLayout, ChromeRenderer, ChromeShortcut,
    ChromeState, ChromeTab, CollectingAuditSink, DiagnosticSnapshot, DismissTileResult,
    NoopAuditSink, RevokeReason, SafeModeEntryReason, ShellAuditEvent, ShellAuditSink,
    ShortcutResult, SystemHealth, TabBarPosition, collect_diagnostic, handle_shortcut,
    strip_chrome_from_topology,
};
pub use shell::safe_mode::{
    LeaseResumeInfo, SafeModeController, SafeModeEntryResult, SafeModeExitResult, SafeModeInput,
    SafeModeInputResult, ShellOverrideState, classify_safe_mode_input,
};
pub use widget_runtime_registration::{RuntimeWidgetAssetError, register_runtime_widget_svg_asset};
pub use windowed::{WindowedConfig, WindowedRuntime};
// ChromeDrawCmd is defined in tze_hud_compositor to avoid circular deps.
pub use shell::{
    DEFAULT_AUTO_UNFREEZE_MS, DEFAULT_FREEZE_QUEUE_CAPACITY, EnqueueResult, FreezeManager,
    FreezeQueue, FreezeState, MutationTrafficClass, QUEUE_PRESSURE_FRACTION, QueuedMutation,
    classify_mutation_batch,
};
pub use tze_hud_compositor::ChromeDrawCmd;

pub use pipeline::{
    FramePipeline, HitTestSnapshot, INPUT_TO_LOCAL_ACK_BUDGET_US, INPUT_TO_NEXT_PRESENT_BUDGET_US,
    INPUT_TO_SCENE_COMMIT_BUDGET_US, STAGE1_BUDGET_US, STAGE2_BUDGET_US, STAGE3_BUDGET_US,
    STAGE4_BUDGET_US, STAGE5_BUDGET_US, STAGE6_BUDGET_US, STAGE7_BUDGET_US, STAGE8_BUDGET_US,
    STAGE12_COMBINED_BUDGET_US, TOTAL_PIPELINE_BUDGET_US, TileBoundsEntry,
};
pub use shell::badges::{
    BUDGET_WARNING_AMBER_COLOR, BUDGET_WARNING_BORDER_OPACITY, BUDGET_WARNING_BORDER_PX,
    BackpressureSignal, BadgeFrame, DISCONNECTED_BADGE_OPACITY, DISCONNECTED_CONTENT_OPACITY,
    DISCONNECTION_BADGE_BG_COLOR, DISCONNECTION_BADGE_ICON_COLOR, DISCONNECTION_BADGE_OFFSET_PX,
    DISCONNECTION_BADGE_SIZE_PX, DISCONNECTION_CONTENT_SCRIM_COLOR, TileBadgeState,
    build_badge_cmds,
};
pub use threads::{
    CompositorReady, CompositorThreadHandle, NetworkRuntime, ShutdownConfig, ShutdownReason,
    ShutdownToken, ThreadRole, elevate_main_thread_priority, graceful_shutdown,
    spawn_compositor_thread, spawn_telemetry_thread,
};
pub use window::{
    FallbackReason, HitRegion, OverlaySupport, WindowConfig, WindowMode, check_overlay_support,
    resolve_window_mode, should_capture_pointer_event,
};

// ── Font loader (resource store → compositor bridge) ─────────────────────────
pub use font_loader::FontLoader;

// ── Reload triggers (RFC 0006 §9) ─────────────────────────────────────────────
pub use reload_triggers::{RuntimeServiceImpl, spawn_sighup_listener};

// ── Widget startup integration ────────────────────────────────────────────────
pub use widget_startup::{collect_tab_name_to_id, init_widget_registry};

// ── GPU lock (Windows GPU scheduling policy, hud-940e4) ───────────────────────
// Cross-platform: GpuLock::acquire() is a no-op on non-Windows targets.
pub use gpu_lock::{GpuLock, GpuLockConflict, GpuLockGuard};

// ── Portal cadence coalescing (hud-5jbra.5, tasks.md §5.1–5.4) ───────────────
pub use portal_cadence::{
    CADENCE_BURST_BYTES, CADENCE_BURST_WINDOW_MS, CADENCE_MIN_INCREMENTS_PER_SEC,
    CADENCE_MIN_SCALARS_PER_SEC, CADENCE_SUSTAINED_SECS, CadenceWorkload, FairnessProbe,
    MAX_PORTAL_SNAPSHOT_BYTES, PortalCadenceCoalescer,
};
