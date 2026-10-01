//! System shell subsystem.
//!
//! The system shell owns the chrome layer — the set of UI elements that are ALWAYS
//! rendered on top of all agent content and are NEVER accessible to agents.
//!
//! The shell layer also implements human-override semantics: chrome sovereignty,
//! safe mode, freeze, and disconnection badges.
//!
//! # Sovereignty contract
//!
//! - Chrome renders above all agent tiles in every frame (background → content → chrome).
//! - No agent API exposes any chrome element or viewer context.
//! - Shell state transitions are the SOLE owner of chrome layer governance.
//! - The shell is the **sole** owner of `OverrideState` transitions (freeze, safe mode).
//!   No other subsystem may write these fields.
//! - Override controls are local, frame-bounded, unconditional, and cannot be vetoed.
//!
//! # Badges (Shell #5)
//!
//! Disconnection badges and backpressure signals are handled by [`badges`]. The
//! shell is the sole owner of badge rendering. Badge state is owned by the
//! `badges` subsystem (per-tile [`badges::TileBadgeState`] and frame-level
//! [`badges::BadgeFrame`]) and consumed by the chrome render pass.
//! Agents are never notified of badge state. See `badges.rs`.
//!
//! See `chrome.rs` for chrome layer implementation.
//! See `freeze.rs` for freeze semantics.

pub mod badges;
pub mod chrome;
pub mod freeze;
pub mod safe_mode;

pub use chrome::{
    // Agent exclusion
    AgentVisibleTopology,
    AuditPayload,
    AuditTrigger,
    // Layout
    ChromeLayout,
    // Rendering
    ChromeRenderer,
    // Keyboard
    ChromeShortcut,
    // Core state
    ChromeState,
    ChromeTab,
    CollectingAuditSink,
    // Diagnostics
    DiagnosticSnapshot,
    // Dismiss / override
    DismissTileResult,
    NoopAuditSink,
    RevokeReason,
    SafeModeEntryReason,
    // Audit
    ShellAuditEvent,
    ShellAuditSink,
    ShortcutResult,
    SystemHealth,
    TabBarPosition,
    collect_diagnostic,
    handle_shortcut,
    strip_chrome_from_topology,
};
pub use safe_mode::{
    // Results
    LeaseResumeInfo,
    // Controller
    SafeModeController,
    SafeModeEntryResult,
    SafeModeExitResult,
    // Input handling
    SafeModeInput,
    SafeModeInputResult,
    // Core state
    ShellOverrideState,
    classify_safe_mode_input,
};
// ChromeDrawCmd lives in tze_hud_compositor to avoid circular dependencies.
pub use tze_hud_compositor::ChromeDrawCmd;

pub use freeze::{
    DEFAULT_AUTO_UNFREEZE_MS, DEFAULT_FREEZE_QUEUE_CAPACITY, EnqueueResult, FreezeManager,
    FreezeQueue, FreezeState, MutationTrafficClass, QUEUE_PRESSURE_FRACTION, QueuedMutation,
    classify_mutation_batch,
};

pub use badges::{
    BUDGET_WARNING_AMBER_COLOR,
    BUDGET_WARNING_BORDER_OPACITY,
    BUDGET_WARNING_BORDER_PX,
    // Backpressure signals
    BackpressureSignal,
    // Frame-level badge snapshot
    BadgeFrame,
    DISCONNECTED_BADGE_OPACITY,
    // Rendering constants
    DISCONNECTED_CONTENT_OPACITY,
    DISCONNECTION_BADGE_BG_COLOR,
    DISCONNECTION_BADGE_ICON_COLOR,
    DISCONNECTION_BADGE_OFFSET_PX,
    DISCONNECTION_BADGE_SIZE_PX,
    DISCONNECTION_CONTENT_SCRIM_COLOR,
    // Media-disconnection badge (B11)
    // Per-tile badge state (written by control plane, read by chrome render pass)
    TileBadgeState,
    // Draw command builders
    build_badge_cmds,
};
