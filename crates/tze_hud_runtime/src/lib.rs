//! # tze_hud_runtime
//!
//! Runtime kernel for tze_hud — the **orchestration layer**.
//!
//! ## Governance Authority Map
//!
//! The runtime orchestrates; it does not own policy arbitration or resource accounting.
//! The authority split is:
//!
//! | Authority | Crate | Role |
//! |-----------|-------|------|
//! | Resource accounting | `tze_hud_resource` | Decoded-byte budget registry; GC; dedup |
//! | Budget enforcement | `tze_hud_runtime::budget` | Enforcement ladder (Warning/Throttle/Revoke) |
//! | Attention budgets | `tze_hud_runtime::attention_budget` | Stateful event-pipeline tracker |
//! | Override state | `tze_hud_runtime::shell::SafeModeController` | Sole writer of freeze/safe-mode flags |
//! | Scene orchestration | `tze_hud_runtime` (this crate) | Wires authority modules; drives pipeline |
//!
//! **Budget enforcement is self-contained in `tze_hud_runtime::budget`. The
//! `BudgetEnforcer` owns the per-agent enforcement state machine
//! (`Normal` → `Warning` → `Throttled` → `Revoked`), the enforcement ladder tick, the
//! frame-time guardian, and the per-mutation admission gate. All enforcement
//! decisions originate from `budget.rs` and `attention_budget/`.**
//!
//! See `budget.rs`, `attention_budget/`, and `shell/safe_mode.rs` for boundary
//! doc comments in each authority module.
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
//! (ArcSwap-backed lock-free tile bounds for Stage 2), as well as
//! `MutationIntakeStage` for Stage 3 budget-gated mutation intake.
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
//! ## Bead 3: Interruption classification and quiet hours
//!
//! - [`attention_budget`] — per-agent and per-zone rolling interruption budgets,
//!   80% warning, exhaustion coalescing, earned-urgency tracker.
//! - [`quiet_hours`] — quiet-hours gate (deliver / queue / discard) and
//!   per-zone FIFO queues with LatestWins coalescing.

pub mod admission;
pub mod agent_events;
pub mod attention_budget;
pub mod budget;
pub mod channels;
pub mod component_startup;
pub mod degradation;
pub mod diag;
pub mod element_store;
pub mod event_bus;
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
pub mod quiet_hours;
pub mod reload_triggers;
pub mod resident_grpc_bridge;
pub mod runtime_context;
pub mod session;
pub mod shell;
pub mod subscriptions;
pub mod tab_switch_trigger;
#[cfg(test)]
pub(crate) mod test_support;
pub mod threads;
pub mod trace_capture;
mod widget_hover;
pub mod widget_runtime_registration;
pub mod widget_startup;
pub mod window;
pub mod windowed;

pub use agent_events::rate_limiter::AgentEventRateLimiter;
pub use agent_events::{
    AgentEventHandler, DEFAULT_MAX_EVENTS_PER_SECOND, EmissionError, EmissionOutcome,
    EmissionResult, MAX_PAYLOAD_BYTES,
};
pub use attention_budget::{
    AttentionBudgetOutcome, AttentionBudgetTracker, DEFAULT_AGENT_BUDGET,
    DEFAULT_STACK_ZONE_BUDGET, DEFAULT_ZONE_BUDGET, EarnedUrgencyConfig, EarnedUrgencyTracker,
    ROLLING_WINDOW_US, UrgencyRecord, WARNING_FRACTION,
};
pub use budget::{
    AgentResourceState, BudgetCheckOutcome, BudgetEnforcer, BudgetState, BudgetTelemetrySink,
    CollectingTelemetrySink, NoopTelemetrySink,
};
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
pub use degradation::{DegradationConfig, DegradationController, DegradationLevel, TileDescriptor};
pub use event_bus::{
    AGGREGATE_RATE_CAP, AggregateRateLimiter, ClassifiedEvent, EventBus, InterruptionClass,
    SubscriberQueue,
};
pub use headless::HeadlessRuntime;
pub use idle_efficiency::{
    IdleEfficiencyCounters, IdleEfficiencyDeltaError, IdleEfficiencySnapshot, RuntimeWakeupSource,
};
pub use mcp::{McpServerConfig, start_mcp_http_server};
pub use mutation_budget_bridge::RuntimeMutationBudgetEnforcer;
pub use quiet_hours::{
    GateDecision, QuietHoursConfig, QuietHoursGate, ZoneContentionPolicy, ZoneQueue,
};
pub use runtime_context::{FallbackPolicy, RuntimeContext, SharedRuntimeContext};
pub use shell::chrome::{
    AgentVisibleTopology, AuditPayload, AuditTrigger, ChromeLayout, ChromeRenderer, ChromeShortcut,
    ChromeState, ChromeTab, CollectingAuditSink, DiagnosticSnapshot, DismissTileResult,
    NoopAuditSink, RevokeReason, SafeModeEntryReason, ShellAuditEvent, ShellAuditSink,
    ShortcutResult, SystemHealth, TabBarPosition, ViewerClass, ViewerClassTransition,
    collect_diagnostic, handle_shortcut, strip_chrome_from_topology,
};
pub use shell::safe_mode::{
    LeaseResumeInfo, SafeModeController, SafeModeEntryResult, SafeModeExitResult, SafeModeInput,
    SafeModeInputResult, ShellOverrideState, classify_safe_mode_input,
};
pub use subscriptions::{
    AgentSubscriptions, CATEGORY_AGENT_EVENTS, CATEGORY_ATTENTION_EVENTS,
    CATEGORY_DEGRADATION_NOTICES, CATEGORY_FOCUS_EVENTS, CATEGORY_INPUT_EVENTS,
    CATEGORY_LEASE_CHANGES, CATEGORY_SCENE_TOPOLOGY, CATEGORY_TELEMETRY_FRAMES,
    CATEGORY_ZONE_EVENTS, MANDATORY_CATEGORIES, MAX_SUBSCRIPTIONS_PER_AGENT, Subscription,
    SubscriptionChangeOutcome, SubscriptionRegistry, category_prefix, required_capability,
};
pub use widget_runtime_registration::{RuntimeWidgetAssetError, register_runtime_widget_svg_asset};
pub use windowed::{WindowedConfig, WindowedRuntime};
// ChromeDrawCmd is defined in tze_hud_compositor to avoid circular deps.
pub use shell::{
    DEFAULT_AUTO_UNFREEZE_MS, DEFAULT_FREEZE_QUEUE_CAPACITY, EnqueueResult, FreezeManager,
    FreezeQueue, FreezeState, MutationTrafficClass, QUEUE_PRESSURE_FRACTION, QueuedMutation,
    classify_mutation_batch,
};
pub use tab_switch_trigger::{
    ACTIVE_TAB_CHANGED_EVENT_TYPE, AttentionGate, BlockingGate, PermissiveGate, TabSwitchOutcome,
    TabSwitchTrigger,
};
pub use tze_hud_compositor::ChromeDrawCmd;

pub use shell::redaction::{
    ContentClassification, MAX_PATTERN_ACCENT_RECTS, PATTERN_CELL_PX, REDACTION_BLANK_COLOR,
    REDACTION_PATTERN_ACCENT, REDACTION_PATTERN_BASE, RedactionFrame, RedactionStyle,
    TileRedactionState, build_redaction_cmds, hit_regions_enabled, is_tile_redacted,
};

pub use admission::{
    AdmissionController, AdmissionOutcome, DEFAULT_MAX_GUEST_SESSIONS,
    DEFAULT_MAX_RESIDENT_SESSIONS, DEFAULT_MAX_TOTAL_SESSIONS, HARD_MAX_GUEST_SESSIONS,
    HARD_MAX_RESIDENT_SESSIONS, HARD_MAX_TOTAL_SESSIONS, HotConnectSnapshot, LimitKind,
    ResourceExhaustedDetail, SessionLimits,
};
pub use pipeline::{
    DEFAULT_POST_REVOCATION_CLEANUP_DELAY_MS, FramePipeline, HitTestSnapshot,
    INPUT_TO_LOCAL_ACK_BUDGET_US, INPUT_TO_NEXT_PRESENT_BUDGET_US, INPUT_TO_SCENE_COMMIT_BUDGET_US,
    IntakeResult, MAX_POST_REVOCATION_CLEANUP_DELAY_MS, MIN_POST_REVOCATION_CLEANUP_DELAY_MS,
    MutationIntakeStage, PendingCleanup, STAGE1_BUDGET_US, STAGE2_BUDGET_US, STAGE3_BUDGET_US,
    STAGE4_BUDGET_US, STAGE5_BUDGET_US, STAGE6_BUDGET_US, STAGE7_BUDGET_US, STAGE8_BUDGET_US,
    STAGE12_COMBINED_BUDGET_US, TOTAL_PIPELINE_BUDGET_US, TileBoundsEntry,
};
pub use session::{
    AgentKind, DEFAULT_MAX_ACTIVE_LEASES, DEFAULT_MAX_NODES_PER_TILE, DEFAULT_MAX_TEXTURE_BYTES,
    DEFAULT_MAX_TILES, DEFAULT_MAX_UPDATE_RATE_HZ, HARD_MAX_ACTIVE_LEASES, HARD_MAX_NODES_PER_TILE,
    HARD_MAX_TEXTURE_BYTES, HARD_MAX_TILES, HARD_MAX_UPDATE_RATE_HZ, SessionEnvelope,
    assert_memory_overhead_within_budget,
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

// ── Record/Replay Trace capture ───────────────────────────────────────────────
pub use trace_capture::{TraceRecorder, build_regression_trace};

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
