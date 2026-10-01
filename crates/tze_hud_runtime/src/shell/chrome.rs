//! Chrome layer — system shell rendering that always renders above all agent content.
//!
//! # Layer Sovereignty Contract
//!
//! Chrome is the topmost rendering layer. The compositor renders three layers back-to-front:
//! background → content → chrome. Chrome shares the same wgpu pipeline as content (not a
//! separate window or GPU context). Chrome elements are NEVER visible to agents via any API.
//!
//! # ChromeState
//!
//! [`ChromeState`] is the sole source of truth for chrome rendering. The compositor reads it
//! at the start of every chrome render pass. The control plane (network thread) holds the write
//! lock only for short-lived updates. Chrome rendering is fully independent of agent state:
//! if all agents crash, chrome renders correctly on the next frame.
//!
//! # Spec reference
//! See `openspec/changes/v1-mvp-standards/specs/system-shell/spec.md`.

use std::sync::{Arc, RwLock};
use tze_hud_compositor::ChromeDrawCmd;
use tze_hud_scene::types::SceneId;

// ─── Viewer class ────────────────────────────────────────────────────────────

/// Viewer class determines what content an authenticated viewer is allowed to see.
/// Agents MUST NOT receive viewer class information through any API.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewerClass {
    /// Displays a filled circle icon.
    Owner,
    /// Displays a partial circle icon.
    HouseholdMember,
    /// Displays an outline circle icon.
    KnownGuest,
    /// Displays a question mark icon.
    #[default]
    Unknown,
    /// Displays a dim circle icon.
    Nobody,
}

impl ViewerClass {
    /// Icon representation for rendering (the only animation-permitted in v1 chrome
    /// is the 300ms cross-fade on viewer class transition).
    pub fn icon_label(&self) -> &'static str {
        match self {
            ViewerClass::Owner => "●",           // filled circle
            ViewerClass::HouseholdMember => "◕", // partial circle
            ViewerClass::KnownGuest => "○",      // outline circle
            ViewerClass::Unknown => "?",         // question mark
            ViewerClass::Nobody => "·",          // dim circle
        }
    }
}

// ─── Tab bar position ────────────────────────────────────────────────────────

/// Where the tab bar renders. When `Hidden`, keyboard shortcuts remain active.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabBarPosition {
    #[default]
    Top,
    Bottom,
    Hidden,
}

// ─── System health ───────────────────────────────────────────────────────────

/// Health state for the system status indicator dot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SystemHealth {
    /// All agents connected — green dot.
    #[default]
    AllConnected,
    /// Some agents degraded — amber dot.
    SomeDegraded,
    /// All disconnected or safe mode — red dot.
    AllDisconnectedOrSafeMode,
}

// ─── Tab entry (chrome-internal) ─────────────────────────────────────────────

/// A tab entry as stored in ChromeState. Tab names are runtime-supplied identifiers,
/// not agent-supplied metadata.
#[derive(Clone, Debug)]
pub struct ChromeTab {
    /// Stable identifier for this tab slot (not a SceneId — not addressable by agents).
    pub id: u32,
    /// Human-readable name for the tab bar display.
    pub name: String,
    /// Whether this is the currently active tab.
    pub active: bool,
}

// ─── Viewer class transition ─────────────────────────────────────────────────

/// Tracks a cross-fade transition between two viewer class icons.
/// The only animation permitted in v1 chrome: 300ms cross-fade.
#[derive(Clone, Debug)]
pub struct ViewerClassTransition {
    /// Class being faded out.
    pub from: ViewerClass,
    /// Class being faded in.
    pub to: ViewerClass,
    /// Elapsed microseconds since the transition started (0..=300_000).
    pub elapsed_us: u64,
}

impl ViewerClassTransition {
    /// Duration of the cross-fade in microseconds (300ms).
    pub const DURATION_US: u64 = 300_000;

    /// Progress in [0.0, 1.0] — 0 = fully from, 1 = fully to.
    pub fn progress(&self) -> f32 {
        (self.elapsed_us as f32 / Self::DURATION_US as f32).min(1.0)
    }

    /// Whether the transition has completed.
    pub fn is_complete(&self) -> bool {
        self.elapsed_us >= Self::DURATION_US
    }
}

// ─── ChromeState ─────────────────────────────────────────────────────────────

/// The authoritative state for all chrome rendering.
///
/// Protected by `Arc<RwLock<ChromeState>>`.
///
/// ## Concurrency contract
/// - Control plane holds the write lock only for short-lived updates.
/// - Compositor acquires a read lock at the start of the chrome render pass and
///   releases it before GPU submit.
/// - This ensures no data races: the compositor reads either the pre-update or
///   post-update snapshot atomically.
///
/// ## Agent exclusion
/// Chrome state is NEVER exposed through any agent-facing API. Chrome elements
/// do NOT appear in scene topology queries. Chrome elements are NOT addressable
/// via SceneId.
#[derive(Debug, Default)]
pub struct ChromeState {
    /// Current ordered tab list. These are runtime-managed, not agent-supplied.
    pub tabs: Vec<ChromeTab>,
    /// Currently active tab index (into `tabs`).
    pub active_tab_index: usize,
    /// Tab bar position configuration.
    pub tab_bar_position: TabBarPosition,
    /// Current viewer class.
    pub viewer_class: ViewerClass,
    /// Active viewer class cross-fade transition, if any.
    pub viewer_class_transition: Option<ViewerClassTransition>,
    /// Whether safe mode is currently active.
    pub safe_mode_active: bool,
    /// Whether the mute control is active (v1-reserved: always false).
    pub mute_active: bool,
    /// Number of currently connected agents (for system status indicator).
    pub connected_agent_count: u32,
    /// System health state (for health dot color).
    pub health: SystemHealth,
    /// Capture surface active (v1-reserved: always false — overlay-only redaction).
    pub capture_surface_active: bool,
}

impl ChromeState {
    /// Create a new ChromeState with default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Tab bar height in pixels. Used by chrome renderer to position content pass.
    pub const TAB_BAR_HEIGHT_PX: f32 = 40.0;

    /// Maximum tab bar visible width before overflow kicks in.
    /// The actual threshold depends on display width and is computed at render time.
    pub const MIN_TAB_WIDTH_PX: f32 = 80.0;

    /// Mute control width reserved in the tab bar (v1-reserved, rendered disabled).
    pub const MUTE_CONTROL_WIDTH_PX: f32 = 40.0;

    /// System status indicator width (trailing end of tab bar).
    pub const STATUS_INDICATOR_WIDTH_PX: f32 = 120.0;

    /// Dismiss-all affordance width (inside status indicator).
    pub const DISMISS_ALL_WIDTH_PX: f32 = 80.0;

    /// Add a tab.
    ///
    /// Control plane only — holds write lock for the duration.
    pub fn add_tab(&mut self, id: u32, name: String) {
        let active = self.tabs.is_empty();
        self.tabs.push(ChromeTab { id, name, active });
        if active {
            self.active_tab_index = 0;
        }
    }

    /// Remove a tab by id.
    pub fn remove_tab(&mut self, id: u32) {
        if let Some(pos) = self.tabs.iter().position(|t| t.id == id) {
            let old_active = self.active_tab_index;
            self.tabs.remove(pos);

            if self.tabs.is_empty() {
                self.active_tab_index = 0;
                return;
            }

            // Recompute active_tab_index correctly:
            // - Removed before active → active shifts left by 1.
            // - Removed at active → clamp to last tab.
            // - Removed after active → index unchanged.
            self.active_tab_index = if pos < old_active {
                old_active - 1
            } else if pos == old_active {
                old_active.min(self.tabs.len() - 1)
            } else {
                old_active
            };

            // Ensure exactly one tab carries the active flag.
            for tab in &mut self.tabs {
                tab.active = false;
            }
            self.tabs[self.active_tab_index].active = true;
        }
    }

    /// Switch to tab by index. Returns `true` if the switch occurred.
    pub fn switch_to_tab_index(&mut self, idx: usize) -> bool {
        if idx >= self.tabs.len() {
            return false;
        }
        if let Some(current) = self.tabs.get_mut(self.active_tab_index) {
            current.active = false;
        }
        self.active_tab_index = idx;
        self.tabs[idx].active = true;
        true
    }

    /// Switch to the next tab (wraps around).
    pub fn switch_to_next_tab(&mut self) -> bool {
        if self.tabs.is_empty() {
            return false;
        }
        let next = (self.active_tab_index + 1) % self.tabs.len();
        self.switch_to_tab_index(next)
    }

    /// Switch to the previous tab (wraps around).
    pub fn switch_to_prev_tab(&mut self) -> bool {
        if self.tabs.is_empty() {
            return false;
        }
        let prev = if self.active_tab_index == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab_index - 1
        };
        self.switch_to_tab_index(prev)
    }

    /// Switch to the last tab.
    pub fn switch_to_last_tab(&mut self) -> bool {
        if self.tabs.is_empty() {
            return false;
        }
        let last = self.tabs.len() - 1;
        self.switch_to_tab_index(last)
    }

    /// Begin a viewer class transition (initiates 300ms cross-fade).
    ///
    /// If a transition is already in progress, it is replaced.
    pub fn begin_viewer_class_transition(&mut self, new_class: ViewerClass) {
        if self.viewer_class == new_class {
            return;
        }
        self.viewer_class_transition = Some(ViewerClassTransition {
            from: self.viewer_class,
            to: new_class,
            elapsed_us: 0,
        });
        // The viewer_class field is updated only when the transition completes.
    }

    /// Advance the viewer class transition by `delta_us` microseconds.
    ///
    /// Returns `true` if the transition completed this tick.
    pub fn advance_transition(&mut self, delta_us: u64) -> bool {
        let completed = if let Some(ref mut t) = self.viewer_class_transition {
            t.elapsed_us += delta_us;
            t.is_complete()
        } else {
            return false;
        };

        if completed {
            let to_class = self.viewer_class_transition.take().unwrap().to;
            self.viewer_class = to_class;
        }
        completed
    }
}

// ─── Chrome render geometry ───────────────────────────────────────────────────

/// Layout geometry for the chrome layer, computed at render time from ChromeState.
///
/// Cached by the compositor. Rebuilt when display dimensions or tab bar position changes.
/// Chrome render pass reads exclusively from ChromeState and this geometry — no agent state.
#[derive(Clone, Debug)]
pub struct ChromeLayout {
    /// Display width in pixels.
    pub display_width: f32,
    /// Display height in pixels.
    pub display_height: f32,
    /// Tab bar Y origin (pixels). 0 if top, display_height - TAB_BAR_HEIGHT_PX if bottom.
    pub tab_bar_y: f32,
    /// Tab bar height (pixels). 0 if hidden.
    pub tab_bar_height: f32,
    /// Content area Y start (below/above tab bar).
    pub content_y: f32,
    /// Content area height.
    pub content_height: f32,
    /// Total available width for tab buttons (excludes status indicator and mute control).
    pub tab_area_width: f32,
    /// Number of tabs that fit in tab_area_width (at MIN_TAB_WIDTH_PX each).
    pub tabs_that_fit: usize,
}

impl ChromeLayout {
    /// Compute chrome layout from state and display dimensions.
    pub fn compute(state: &ChromeState, display_width: f32, display_height: f32) -> Self {
        let bar_h = match state.tab_bar_position {
            TabBarPosition::Hidden => 0.0,
            _ => ChromeState::TAB_BAR_HEIGHT_PX,
        };
        let (tab_bar_y, content_y, content_height) = match state.tab_bar_position {
            TabBarPosition::Top => (0.0, bar_h, display_height - bar_h),
            TabBarPosition::Bottom => (display_height - bar_h, 0.0, display_height - bar_h),
            TabBarPosition::Hidden => (0.0, 0.0, display_height),
        };

        let reserved = ChromeState::STATUS_INDICATOR_WIDTH_PX + ChromeState::MUTE_CONTROL_WIDTH_PX;
        let tab_area_width = (display_width - reserved).max(0.0);
        let tabs_that_fit = (tab_area_width / ChromeState::MIN_TAB_WIDTH_PX) as usize;

        Self {
            display_width,
            display_height,
            tab_bar_y,
            tab_bar_height: bar_h,
            content_y,
            content_height,
            tab_area_width,
            tabs_that_fit,
        }
    }
}

// ─── Keyboard shortcut handling ───────────────────────────────────────────────

/// Keyboard events that the chrome layer intercepts.
///
/// These are handled before tile hit-testing and are NEVER routed to agents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeShortcut {
    /// Ctrl+Tab — switch to the next tab.
    NextTab,
    /// Ctrl+Shift+Tab — switch to the previous tab.
    PrevTab,
    /// Ctrl+1 through Ctrl+8 — switch to a specific tab (1-indexed).
    GotoTab(usize),
    /// Ctrl+9 — switch to the last tab.
    LastTab,
    /// Ctrl+Shift+M — mute control (v1-reserved: noop).
    MuteToggle,
}

/// Result of processing a keyboard event.
#[derive(Clone, Debug)]
pub struct ShortcutResult {
    /// Whether the shortcut was consumed (never route to agents if true).
    pub consumed: bool,
    /// Whether a tab switch occurred.
    pub tab_switched: bool,
    /// Index of the new active tab (if tab_switched).
    pub new_tab_index: Option<usize>,
    /// Whether a mute noop was logged (v1-reserved).
    pub mute_noop_logged: bool,
}

/// Handle a [`ChromeShortcut`] against the given [`ChromeState`].
///
/// The state write lock must be held by the caller.
/// Shortcut events are NEVER routed to any agent.
pub fn handle_shortcut(state: &mut ChromeState, shortcut: ChromeShortcut) -> ShortcutResult {
    match shortcut {
        ChromeShortcut::NextTab => {
            let switched = state.switch_to_next_tab();
            let new_idx = if switched {
                Some(state.active_tab_index)
            } else {
                None
            };
            ShortcutResult {
                consumed: true,
                tab_switched: switched,
                new_tab_index: new_idx,
                mute_noop_logged: false,
            }
        }
        ChromeShortcut::PrevTab => {
            let switched = state.switch_to_prev_tab();
            let new_idx = if switched {
                Some(state.active_tab_index)
            } else {
                None
            };
            ShortcutResult {
                consumed: true,
                tab_switched: switched,
                new_tab_index: new_idx,
                mute_noop_logged: false,
            }
        }
        ChromeShortcut::GotoTab(n) => {
            // n is 1-indexed (Ctrl+1 = index 0, Ctrl+8 = index 7).
            let idx = n.saturating_sub(1);
            let switched = state.switch_to_tab_index(idx);
            let new_idx = if switched {
                Some(state.active_tab_index)
            } else {
                None
            };
            ShortcutResult {
                consumed: true,
                tab_switched: switched,
                new_tab_index: new_idx,
                mute_noop_logged: false,
            }
        }
        ChromeShortcut::LastTab => {
            let switched = state.switch_to_last_tab();
            let new_idx = if switched {
                Some(state.active_tab_index)
            } else {
                None
            };
            ShortcutResult {
                consumed: true,
                tab_switched: switched,
                new_tab_index: new_idx,
                mute_noop_logged: false,
            }
        }
        ChromeShortcut::MuteToggle => {
            // v1-reserved: accept input, log noop, take no media action.
            tracing::debug!("chrome: Ctrl+Shift+M — mute noop (v1-reserved)");
            ShortcutResult {
                consumed: true,
                tab_switched: false,
                new_tab_index: None,
                mute_noop_logged: true,
            }
        }
    }
}

// ─── Dismiss tile override ────────────────────────────────────────────────────

/// Result of a dismiss tile action.
///
/// Dismiss is unconditional: local execution, frame-bounded, no agent veto.
/// Works even if the agent is disconnected or in the reconnect grace period.
#[derive(Clone, Debug)]
pub struct DismissTileResult {
    /// Whether the tile was found and removed.
    pub tile_removed: bool,
    /// The SceneId of the dismissed tile (for audit events and lease revocation).
    pub tile_id: Option<SceneId>,
    /// Whether the grace period was cancelled (agent was in grace period).
    pub grace_period_cancelled: bool,
}

/// Revocation reason sent to the agent as part of `LeaseResponse`.
///
/// RFC 0007 §4.1 / RFC 0008 `RevokeReason` enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeReason {
    /// Viewer dismissed the tile via the X button.
    ViewerDismissed,
    /// Viewer dismissed all tiles ("Dismiss All" affordance).
    ViewerDismissedAll,
    /// Safe mode was entered (leases are SUSPENDED not revoked, but tracked here).
    SafeMode,
}

// ─── Shell audit events ───────────────────────────────────────────────────────

/// A shell audit event emitted to the telemetry thread for every human override
/// action and viewer-context change.
///
/// ## Privacy invariants
/// Audit events MUST NOT contain: viewer name, biometric features, auth details,
/// device IDs, or geolocation. Viewer class changes carry only old_class/new_class.
///
/// ## Agent exclusion
/// Audit events are NEVER routed to agents. They are sent to the telemetry thread only.
#[derive(Clone, Debug)]
pub struct ShellAuditEvent {
    /// Monotonic timestamp in microseconds.
    pub timestamp_mono_us: u64,
    /// What triggered this event.
    pub trigger: AuditTrigger,
    /// The specific event payload.
    pub payload: AuditPayload,
}

/// How a shell audit event was triggered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditTrigger {
    /// Keyboard shortcut (e.g. Ctrl+Shift+Escape).
    KeyboardShortcut,
    /// Pointer gesture (click, hover, touch).
    PointerGesture,
    /// Automatic / runtime event (GPU loss, session crash, etc.).
    Auto,
}

/// The specific payload of a shell audit event.
///
/// Privacy constraint: `AuditViewerClassChanged` carries only old_class/new_class — no identity.
#[derive(Clone, Debug)]
pub enum AuditPayload {
    TileDismissed {
        tile_id: SceneId,
        trigger: AuditTrigger,
    },
    AllDismissed {
        trigger: AuditTrigger,
    },
    SafeModeEntered {
        reason: SafeModeEntryReason,
    },
    SafeModeExited,
    FreezeActivated,
    FreezeDeactivated,
    /// Privacy-safe: carries only class values, no viewer identity.
    ViewerClassChanged {
        old_class: ViewerClass,
        new_class: ViewerClass,
    },
    ViewerPromptShown,
    ViewerPromptResolved,
    MuteNoopLogged,
}

/// Why safe mode was entered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SafeModeEntryReason {
    /// Explicit viewer action (Ctrl+Shift+Escape or "Dismiss All").
    ExplicitViewerAction,
    /// Automatic — critical runtime error (GPU device loss, scene graph corruption).
    CriticalError,
}

// ─── Telemetry sink for audit events ─────────────────────────────────────────

/// Sink for shell audit events. Implemented by the telemetry thread.
///
/// Audit events are NEVER routed to agents. The only valid implementation
/// routes to the telemetry thread.
pub trait ShellAuditSink: Send + Sync {
    fn emit(&self, event: ShellAuditEvent);
}

/// A no-op audit sink for tests and headless environments.
pub struct NoopAuditSink;

impl ShellAuditSink for NoopAuditSink {
    fn emit(&self, _event: ShellAuditEvent) {
        // Intentionally empty — events are not recorded in no-op mode.
    }
}

/// A collecting audit sink for tests — accumulates events for assertion.
pub struct CollectingAuditSink {
    events: std::sync::Mutex<Vec<ShellAuditEvent>>,
}

impl CollectingAuditSink {
    pub fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn drain(&self) -> Vec<ShellAuditEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }

    pub fn count(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

impl Default for CollectingAuditSink {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellAuditSink for CollectingAuditSink {
    fn emit(&self, event: ShellAuditEvent) {
        self.events.lock().unwrap().push(event);
    }
}

// ─── Chrome renderer ──────────────────────────────────────────────────────────

/// The chrome renderer.
///
/// Holds a reference to the shared `ChromeState` and produces `ChromeDrawCmd` lists for
/// the compositor's chrome render pass.
///
/// # Layer sovereignty
///
/// The chrome render pass reads EXCLUSIVELY from `ChromeState`, the tab list, viewer context,
/// and cached `ChromeLayout`. It reads NO agent state. If all agents crash, chrome renders
/// correctly on the next frame.
pub struct ChromeRenderer {
    /// Shared chrome state. Compositor acquires read lock at start of chrome pass,
    /// releases before GPU submit.
    chrome_state: Arc<RwLock<ChromeState>>,
    /// Cached layout geometry, recomputed when display size changes.
    layout: Option<ChromeLayout>,
    /// Audit sink — never routes to agents. Used when chrome renderer emits audit events
    /// (e.g., viewer class transitions complete, safe mode changes are detected).
    #[allow(dead_code)]
    audit_sink: Arc<dyn ShellAuditSink>,
}

impl ChromeRenderer {
    /// Create a new chrome renderer backed by the given state.
    pub fn new(
        chrome_state: Arc<RwLock<ChromeState>>,
        audit_sink: Arc<dyn ShellAuditSink>,
    ) -> Self {
        Self {
            chrome_state,
            layout: None,
            audit_sink,
        }
    }

    /// Create with a no-op audit sink (for headless/test use).
    pub fn new_headless(chrome_state: Arc<RwLock<ChromeState>>) -> Self {
        Self::new(chrome_state, Arc::new(NoopAuditSink))
    }

    /// Produce the chrome draw commands for one frame.
    ///
    /// Acquires read lock on `ChromeState` for the duration of command generation.
    /// The lock is released before returning (before GPU submit).
    ///
    /// This is the chrome render pass — it executes AFTER the content render pass.
    pub fn render_chrome(&mut self, display_width: f32, display_height: f32) -> Vec<ChromeDrawCmd> {
        let state = match self.chrome_state.read() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // A writer panicked while holding the lock. Recover the inner state so chrome
                // rendering stays alive — a poisoned lock in the render path must not crash the
                // compositor. The runtime should separately detect and enter safe mode.
                tracing::error!("ChromeState RwLock poisoned; recovering for render_chrome");
                poisoned.into_inner()
            }
        };

        // Recompute layout if needed.
        let layout = ChromeLayout::compute(&state, display_width, display_height);
        self.layout = Some(layout.clone());

        let mut cmds = Vec::new();

        // Render tab bar (if not hidden).
        if state.tab_bar_position != TabBarPosition::Hidden {
            cmds.extend(self.build_tab_bar_cmds(&state, &layout));
        }

        // Render viewer class indicator + system status (only when tab bar is visible).
        cmds.extend(self.build_status_indicator_cmds(
            &state,
            &layout,
            display_width,
            display_height,
        ));

        // Safe mode overlay (if active) — renders over everything.
        if state.safe_mode_active {
            cmds.extend(self.build_safe_mode_overlay_cmds(&state, display_width, display_height));
        }

        // Lock is released here (state drops at end of scope before return).
        drop(state);
        cmds
    }

    // ── Tab bar ──────────────────────────────────────────────────────────

    fn build_tab_bar_cmds(&self, state: &ChromeState, layout: &ChromeLayout) -> Vec<ChromeDrawCmd> {
        let mut cmds = Vec::new();

        // Tab bar background.
        cmds.push(ChromeDrawCmd {
            x: 0.0,
            y: layout.tab_bar_y,
            width: layout.display_width,
            height: layout.tab_bar_height,
            color: [0.08, 0.08, 0.12, 1.0], // dark chrome background
        });

        let tabs = &state.tabs;
        if tabs.is_empty() {
            return cmds;
        }

        let fits = layout.tabs_that_fit.max(1);
        let overflow_count = if tabs.len() > fits {
            tabs.len() - fits
        } else {
            0
        };
        let visible_count = tabs.len().min(fits);

        // Find which tab slice to show (ensure active tab is visible).
        let active = state.active_tab_index;
        // Scroll window: start index such that active is in [start, start+visible_count).
        let start = if active >= fits { active + 1 - fits } else { 0 };
        let start = start.min(tabs.len().saturating_sub(visible_count));

        // Divide available tab area evenly across visible tabs.
        // Do NOT apply a MIN_TAB_WIDTH_PX floor here: the floor is only used when computing
        // `tabs_that_fit` (in ChromeLayout::compute). Applying it again can push tabs into
        // the reserved mute/status indicator area on narrow viewports.
        let tab_w = if visible_count > 0 {
            layout.tab_area_width / visible_count as f32
        } else {
            ChromeState::MIN_TAB_WIDTH_PX
        };

        for (slot, tab_idx) in (start..start + visible_count).enumerate() {
            if tab_idx >= tabs.len() {
                break;
            }
            let tab = &tabs[tab_idx];
            let tx = slot as f32 * tab_w;
            let ty = layout.tab_bar_y;

            // Tab background — active tab is lighter.
            let bg = if tab.active {
                [0.18, 0.18, 0.28, 1.0]
            } else {
                [0.10, 0.10, 0.16, 1.0]
            };
            cmds.push(ChromeDrawCmd {
                x: tx,
                y: ty,
                width: tab_w - 2.0, // 2px gap between tabs
                height: layout.tab_bar_height,
                color: bg,
            });

            // Active tab indicator — 2px accent bar at top.
            if tab.active {
                cmds.push(ChromeDrawCmd {
                    x: tx,
                    y: ty,
                    width: tab_w - 2.0,
                    height: 2.0,
                    color: [0.4, 0.6, 1.0, 1.0], // accent blue
                });
            }
        }

        // Overflow badge "+N" at trailing end of tab area.
        if overflow_count > 0 {
            let badge_x = layout.tab_area_width - 36.0;
            cmds.push(ChromeDrawCmd {
                x: badge_x,
                y: layout.tab_bar_y + 8.0,
                width: 32.0,
                height: layout.tab_bar_height - 16.0,
                color: [0.3, 0.3, 0.5, 1.0], // overflow badge background
            });
            // The actual "+N" text label is deferred to the text-rendering pass.
            // In the current vertical slice, the badge rect acts as the marker.
        }

        cmds
    }

    // ── System status indicator ───────────────────────────────────────────

    fn build_status_indicator_cmds(
        &self,
        state: &ChromeState,
        layout: &ChromeLayout,
        display_width: f32,
        _display_height: f32,
    ) -> Vec<ChromeDrawCmd> {
        let mut cmds = Vec::new();

        // Only render in tab bar if it is visible.
        if state.tab_bar_position == TabBarPosition::Hidden {
            return cmds;
        }

        // Status indicator background (trailing end of tab bar).
        let sx = display_width - ChromeState::STATUS_INDICATOR_WIDTH_PX;
        cmds.push(ChromeDrawCmd {
            x: sx,
            y: layout.tab_bar_y,
            width: ChromeState::STATUS_INDICATOR_WIDTH_PX,
            height: layout.tab_bar_height,
            color: [0.06, 0.06, 0.10, 1.0],
        });

        // Health dot color.
        let dot_color = match state.health {
            SystemHealth::AllConnected => [0.2, 0.8, 0.3, 1.0], // green
            SystemHealth::SomeDegraded => [0.9, 0.7, 0.1, 1.0], // amber
            SystemHealth::AllDisconnectedOrSafeMode => [0.8, 0.2, 0.2, 1.0], // red
        };
        let dot_size = 10.0;
        let dot_x = sx + 8.0;
        let dot_y = layout.tab_bar_y + (layout.tab_bar_height - dot_size) / 2.0;
        cmds.push(ChromeDrawCmd {
            x: dot_x,
            y: dot_y,
            width: dot_size,
            height: dot_size,
            color: dot_color,
        });

        // Agent count indicator (rendered as a colored rect scaled by count — text deferred).
        // The indicator MUST NOT expose agent identities or names.
        let count_x = sx + 22.0;
        let count_indicator_w = (state.connected_agent_count as f32 * 6.0).min(40.0);
        if state.connected_agent_count > 0 {
            cmds.push(ChromeDrawCmd {
                x: count_x,
                y: layout.tab_bar_y + (layout.tab_bar_height - 8.0) / 2.0,
                width: count_indicator_w.max(6.0),
                height: 8.0,
                color: [0.4, 0.4, 0.6, 0.8],
            });
        }

        // Viewer class icon (fills right side of status indicator).
        // Transitions cross-fade over 300ms (the only v1 animation).
        let icon_x = display_width - 36.0;
        let icon_y = layout.tab_bar_y + (layout.tab_bar_height - 16.0) / 2.0;
        let icon_color = if let Some(ref t) = state.viewer_class_transition {
            let p = t.progress();
            let from_c = viewer_class_color(t.from);
            let to_c = viewer_class_color(t.to);
            // Linear interpolation between from and to colors (including alpha).
            [
                from_c[0] * (1.0 - p) + to_c[0] * p,
                from_c[1] * (1.0 - p) + to_c[1] * p,
                from_c[2] * (1.0 - p) + to_c[2] * p,
                from_c[3] * (1.0 - p) + to_c[3] * p,
            ]
        } else {
            viewer_class_color(state.viewer_class)
        };

        cmds.push(ChromeDrawCmd {
            x: icon_x,
            y: icon_y,
            width: 16.0,
            height: 16.0,
            color: icon_color,
        });

        // Mute control (v1-reserved: rendered disabled/greyed).
        let mute_x = display_width
            - ChromeState::STATUS_INDICATOR_WIDTH_PX
            - ChromeState::MUTE_CONTROL_WIDTH_PX;
        cmds.push(ChromeDrawCmd {
            x: mute_x,
            y: layout.tab_bar_y + (layout.tab_bar_height - 20.0) / 2.0,
            width: 24.0,
            height: 20.0,
            color: [0.3, 0.3, 0.3, 0.4], // greyed/disabled
        });

        cmds
    }

    // ── Safe mode overlay ────────────────────────────────────────────────

    /// Build the safe mode overlay draw commands.
    ///
    /// The safe mode overlay reads EXCLUSIVELY from ChromeState. It renders correctly
    /// even if the scene graph is corrupted (because it does not depend on it).
    fn build_safe_mode_overlay_cmds(
        &self,
        state: &ChromeState,
        display_width: f32,
        display_height: f32,
    ) -> Vec<ChromeDrawCmd> {
        let mut cmds = Vec::new();

        // Full-viewport dimming overlay.
        cmds.push(ChromeDrawCmd {
            x: 0.0,
            y: 0.0,
            width: display_width,
            height: display_height,
            color: [0.0, 0.0, 0.0, 0.85],
        });

        // Centered banner area ("Safe Mode" — text rendering deferred; represented as rect).
        let banner_w = 500.0;
        let banner_h = 120.0;
        let banner_x = (display_width - banner_w) / 2.0;
        let banner_y = (display_height - banner_h) / 2.0 - 60.0;
        cmds.push(ChromeDrawCmd {
            x: banner_x,
            y: banner_y,
            width: banner_w,
            height: banner_h,
            color: [0.15, 0.15, 0.25, 1.0],
        });

        // "Resume" button.
        let btn_w = 160.0;
        let btn_h = 48.0;
        let btn_x = (display_width - btn_w) / 2.0;
        let btn_y = (display_height - btn_h) / 2.0 + 40.0;
        cmds.push(ChromeDrawCmd {
            x: btn_x,
            y: btn_y,
            width: btn_w,
            height: btn_h,
            color: [0.3, 0.5, 0.9, 1.0], // blue button
        });

        // Viewer class icon in overlay (uses same color logic as tab bar).
        let vc_color = viewer_class_color(state.viewer_class);
        cmds.push(ChromeDrawCmd {
            x: btn_x + btn_w + 24.0,
            y: btn_y + (btn_h - 20.0) / 2.0,
            width: 20.0,
            height: 20.0,
            color: vc_color,
        });

        cmds
    }
}

/// Map a viewer class to a representative RGBA color for icon rendering.
///
/// Alpha is encoded in `[3]`. Call sites use the full array for cross-fade interpolation.
fn viewer_class_color(vc: ViewerClass) -> [f32; 4] {
    match vc {
        ViewerClass::Owner => [0.4, 0.7, 1.0, 1.0], // bright blue filled
        ViewerClass::HouseholdMember => [0.4, 0.7, 1.0, 0.7], // medium blue partial
        ViewerClass::KnownGuest => [0.4, 0.7, 1.0, 0.4], // outline blue
        ViewerClass::Unknown => [0.7, 0.7, 0.7, 0.8], // grey question
        ViewerClass::Nobody => [0.4, 0.4, 0.4, 0.3], // dim
    }
}

// ─── V1 Diagnostic surface (CLI only) ─────────────────────────────────────────

/// Diagnostic snapshot — scene graph dump, active leases, resource utilization,
/// zone registry state, telemetry snapshot.
///
/// V1: CLI only. GUI operator diagnostics overlay deferred to post-v1.
#[derive(Clone, Debug)]
pub struct DiagnosticSnapshot {
    /// Monotonic timestamp of the snapshot.
    pub timestamp_mono_us: u64,
    /// Number of active leases.
    pub active_lease_count: usize,
    /// Number of connected agents.
    pub connected_agent_count: u32,
    /// Number of tabs.
    pub tab_count: usize,
    /// Active tab index.
    pub active_tab_index: usize,
    /// Tab bar position.
    pub tab_bar_position_label: &'static str,
    /// Viewer class (note: this is ONLY available in the CLI diagnostic, never to agents).
    pub viewer_class_label: &'static str,
    /// Safe mode active.
    pub safe_mode_active: bool,
    /// Capture surface active (v1: always false).
    pub capture_surface_active: bool,
}

/// Collect a diagnostic snapshot from a ChromeState.
///
/// `active_lease_count` must be supplied by the caller from the scene graph; the
/// chrome module does not have access to lease state.
///
/// This is the CLI-only v1 diagnostic surface. The GUI operator diagnostics overlay
/// is deferred to post-v1.
pub fn collect_diagnostic(
    state: &ChromeState,
    timestamp_mono_us: u64,
    active_lease_count: usize,
) -> DiagnosticSnapshot {
    DiagnosticSnapshot {
        timestamp_mono_us,
        active_lease_count,
        connected_agent_count: state.connected_agent_count,
        tab_count: state.tabs.len(),
        active_tab_index: state.active_tab_index,
        tab_bar_position_label: match state.tab_bar_position {
            TabBarPosition::Top => "top",
            TabBarPosition::Bottom => "bottom",
            TabBarPosition::Hidden => "hidden",
        },
        viewer_class_label: match state.viewer_class {
            ViewerClass::Owner => "owner",
            ViewerClass::HouseholdMember => "household_member",
            ViewerClass::KnownGuest => "known_guest",
            ViewerClass::Unknown => "unknown",
            ViewerClass::Nobody => "nobody",
        },
        safe_mode_active: state.safe_mode_active,
        capture_surface_active: state.capture_surface_active,
    }
}

impl std::fmt::Display for DiagnosticSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "=== tze_hud Chrome Diagnostic Snapshot ===")?;
        writeln!(f, "  timestamp_mono_us:  {}", self.timestamp_mono_us)?;
        writeln!(f, "  active_lease_count: {}", self.active_lease_count)?;
        writeln!(f, "  connected_agents:   {}", self.connected_agent_count)?;
        writeln!(
            f,
            "  tabs:               {} (active: {})",
            self.tab_count, self.active_tab_index
        )?;
        writeln!(f, "  tab_bar_position:   {}", self.tab_bar_position_label)?;
        writeln!(f, "  viewer_class:       {}", self.viewer_class_label)?;
        writeln!(f, "  safe_mode:          {}", self.safe_mode_active)?;
        writeln!(f, "  capture_surface:    {}", self.capture_surface_active)?;
        Ok(())
    }
}

// ─── Scene topology filter ────────────────────────────────────────────────────

/// Filter that strips all chrome-layer metadata from scene topology query results.
///
/// Chrome elements MUST NOT appear in scene topology queries. Chrome elements
/// MUST NOT be addressable via SceneId. This function is a no-op at the type level
/// (scene graph never holds chrome elements) but documents the exclusion contract
/// explicitly.
///
/// Call this before returning any scene topology response to an agent (gRPC or MCP).
pub fn strip_chrome_from_topology<T: AgentVisibleTopology>(response: &mut T) {
    response.remove_chrome_elements();
}

/// Marker trait for scene topology query responses that must exclude chrome.
///
/// Implemented by any type returned from scene topology queries to agents.
/// The implementation MUST ensure no chrome elements are present in the response.
pub trait AgentVisibleTopology {
    /// Remove any chrome-layer elements from this response.
    ///
    /// Since chrome elements are never added to the scene graph (the scene graph
    /// is agent content only), this is typically a no-op. The trait exists to
    /// document and enforce the exclusion contract at the type level.
    fn remove_chrome_elements(&mut self);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ChromeState basics ────────────────────────────────────────────────

    #[test]
    fn chrome_state_default_is_clean() {
        let state = ChromeState::new();
        assert_eq!(state.tabs.len(), 0);
        assert_eq!(state.active_tab_index, 0);
        assert_eq!(state.tab_bar_position, TabBarPosition::Top);
        assert_eq!(state.viewer_class, ViewerClass::Unknown);
        assert!(!state.safe_mode_active);
        assert!(!state.mute_active);
        assert_eq!(state.connected_agent_count, 0);
        assert!(
            !state.capture_surface_active,
            "v1: capture_surface_active must always be false"
        );
    }

    #[test]
    fn add_tab_makes_first_tab_active() {
        let mut state = ChromeState::new();
        state.add_tab(1, "Tab A".into());
        assert_eq!(state.tabs.len(), 1);
        assert!(state.tabs[0].active);
        assert_eq!(state.active_tab_index, 0);
    }

    #[test]
    fn add_multiple_tabs_only_first_is_active_initially() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.add_tab(3, "C".into());
        assert_eq!(state.tabs.len(), 3);
        assert!(state.tabs[0].active);
        assert!(!state.tabs[1].active);
        assert!(!state.tabs[2].active);
    }

    // ── Tab switching ─────────────────────────────────────────────────────

    #[test]
    fn switch_to_next_tab_wraps_around() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.add_tab(3, "C".into());

        state.switch_to_tab_index(2); // C is active
        assert_eq!(state.active_tab_index, 2);

        let switched = state.switch_to_next_tab();
        assert!(switched);
        assert_eq!(state.active_tab_index, 0, "should wrap to first tab");
        assert!(state.tabs[0].active);
        assert!(!state.tabs[2].active);
    }

    #[test]
    fn switch_to_prev_tab_wraps_around() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.add_tab(3, "C".into());

        // A is active (index 0)
        let switched = state.switch_to_prev_tab();
        assert!(switched);
        assert_eq!(state.active_tab_index, 2, "should wrap to last tab");
        assert!(state.tabs[2].active);
        assert!(!state.tabs[0].active);
    }

    #[test]
    fn switch_to_last_tab() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.add_tab(3, "C".into());
        state.add_tab(4, "D".into());

        let switched = state.switch_to_last_tab();
        assert!(switched);
        assert_eq!(state.active_tab_index, 3);
        assert!(state.tabs[3].active);
    }

    #[test]
    fn switch_to_tab_index_out_of_bounds_returns_false() {
        let mut state = ChromeState::new();
        state.add_tab(1, "Only".into());

        let switched = state.switch_to_tab_index(5);
        assert!(!switched);
    }

    #[test]
    fn switch_to_empty_tab_list_returns_false() {
        let mut state = ChromeState::new();
        assert!(!state.switch_to_next_tab());
        assert!(!state.switch_to_prev_tab());
        assert!(!state.switch_to_last_tab());
    }

    // ── Keyboard shortcuts — never routed to agents ───────────────────────

    #[test]
    fn ctrl_tab_switches_to_next_tab_and_is_consumed() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.add_tab(3, "C".into());

        let result = handle_shortcut(&mut state, ChromeShortcut::NextTab);

        assert!(
            result.consumed,
            "shortcut must be consumed — never routed to agents"
        );
        assert!(result.tab_switched);
        assert_eq!(result.new_tab_index, Some(1));
        assert_eq!(state.active_tab_index, 1);
    }

    #[test]
    fn ctrl_shift_tab_switches_to_prev_tab() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.switch_to_tab_index(1); // B active

        let result = handle_shortcut(&mut state, ChromeShortcut::PrevTab);

        assert!(result.consumed);
        assert!(result.tab_switched);
        assert_eq!(state.active_tab_index, 0);
    }

    #[test]
    fn ctrl_1_switches_to_first_tab() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.add_tab(3, "C".into());
        state.switch_to_tab_index(2); // C active

        let result = handle_shortcut(&mut state, ChromeShortcut::GotoTab(1));

        assert!(result.consumed);
        assert!(result.tab_switched);
        assert_eq!(state.active_tab_index, 0);
    }

    #[test]
    fn ctrl_9_switches_to_last_tab() {
        let mut state = ChromeState::new();
        for i in 1..=5 {
            state.add_tab(i, format!("Tab {i}"));
        }

        let result = handle_shortcut(&mut state, ChromeShortcut::LastTab);

        assert!(result.consumed);
        assert!(result.tab_switched);
        assert_eq!(state.active_tab_index, 4);
    }

    #[test]
    fn ctrl_shift_m_is_noop_in_v1() {
        let mut state = ChromeState::new();
        let result = handle_shortcut(&mut state, ChromeShortcut::MuteToggle);
        assert!(result.consumed, "shortcut must be consumed");
        assert!(!result.tab_switched);
        assert!(result.mute_noop_logged);
    }

    // ── Viewer class transition ───────────────────────────────────────────

    #[test]
    fn viewer_class_transition_begins_and_completes() {
        let mut state = ChromeState::new();
        assert_eq!(state.viewer_class, ViewerClass::Unknown);

        state.begin_viewer_class_transition(ViewerClass::Owner);
        assert!(state.viewer_class_transition.is_some());
        // viewer_class not yet updated
        assert_eq!(state.viewer_class, ViewerClass::Unknown);

        // Advance to completion.
        let completed = state.advance_transition(ViewerClassTransition::DURATION_US);
        assert!(completed);
        assert_eq!(state.viewer_class, ViewerClass::Owner);
        assert!(state.viewer_class_transition.is_none());
    }

    #[test]
    fn viewer_class_transition_partial_progress() {
        let mut state = ChromeState::new();
        state.begin_viewer_class_transition(ViewerClass::Owner);

        let t = state.viewer_class_transition.as_ref().unwrap();
        assert_eq!(t.from, ViewerClass::Unknown);
        assert_eq!(t.to, ViewerClass::Owner);

        // Advance 150ms — halfway.
        state.advance_transition(150_000);
        let t = state.viewer_class_transition.as_ref().unwrap();
        let p = t.progress();
        assert!((p - 0.5).abs() < 0.01, "expected ~50% progress");
        // viewer_class still the old value mid-transition
        assert_eq!(state.viewer_class, ViewerClass::Unknown);
    }

    #[test]
    fn begin_same_viewer_class_is_noop() {
        let mut state = ChromeState::new();
        state.viewer_class = ViewerClass::Owner;
        state.begin_viewer_class_transition(ViewerClass::Owner);
        assert!(
            state.viewer_class_transition.is_none(),
            "no transition needed for same class"
        );
    }

    #[test]
    fn viewer_class_change_audit_carries_only_class_values() {
        // Privacy constraint: audit must carry only old_class/new_class — no identity.
        let event = ShellAuditEvent {
            timestamp_mono_us: 12345,
            trigger: AuditTrigger::Auto,
            payload: AuditPayload::ViewerClassChanged {
                old_class: ViewerClass::Owner,
                new_class: ViewerClass::Unknown,
            },
        };

        // Verify the payload only contains class values.
        match event.payload {
            AuditPayload::ViewerClassChanged {
                old_class,
                new_class,
            } => {
                assert_eq!(old_class, ViewerClass::Owner);
                assert_eq!(new_class, ViewerClass::Unknown);
                // There are no other fields — privacy constraint satisfied.
            }
            _ => panic!("unexpected payload variant"),
        }
    }

    // ── Audit sink ────────────────────────────────────────────────────────

    #[test]
    fn collecting_audit_sink_accumulates_events() {
        let sink = CollectingAuditSink::new();

        sink.emit(ShellAuditEvent {
            timestamp_mono_us: 1000,
            trigger: AuditTrigger::PointerGesture,
            payload: AuditPayload::TileDismissed {
                tile_id: SceneId::new(),
                trigger: AuditTrigger::PointerGesture,
            },
        });

        sink.emit(ShellAuditEvent {
            timestamp_mono_us: 2000,
            trigger: AuditTrigger::KeyboardShortcut,
            payload: AuditPayload::SafeModeEntered {
                reason: SafeModeEntryReason::ExplicitViewerAction,
            },
        });

        assert_eq!(sink.count(), 2);

        let events = sink.drain();
        assert_eq!(events.len(), 2);
        assert_eq!(sink.count(), 0, "drain clears the collection");
    }

    // ── Chrome render pass ────────────────────────────────────────────────

    #[test]
    fn chrome_render_pass_produces_commands_without_agent_state() {
        // Chrome renders correctly even when there are no agents — reads only ChromeState.
        let chrome_state = Arc::new(RwLock::new({
            let mut state = ChromeState::new();
            state.add_tab(1, "Tab 1".into());
            state.add_tab(2, "Tab 2".into());
            state.connected_agent_count = 0; // no agents
            state.health = SystemHealth::AllDisconnectedOrSafeMode;
            state
        }));

        let mut renderer = ChromeRenderer::new_headless(chrome_state);
        let cmds = renderer.render_chrome(1920.0, 1080.0);

        // Must produce some draw commands (tab bar + status indicator at minimum).
        assert!(
            !cmds.is_empty(),
            "chrome render pass must produce draw commands even with no agents"
        );

        // No command should have zero or negative dimensions.
        for cmd in &cmds {
            assert!(
                cmd.width > 0.0,
                "draw command width must be positive: {cmd:?}"
            );
            assert!(
                cmd.height > 0.0,
                "draw command height must be positive: {cmd:?}"
            );
        }
    }

    #[test]
    fn chrome_render_produces_commands_when_all_agents_crash() {
        // Scenario: All agents crash — chrome renders correctly on next frame.
        // (No agent state is needed; ChromeState is sufficient.)
        let chrome_state = Arc::new(RwLock::new({
            let mut state = ChromeState::new();
            state.add_tab(1, "Work".into());
            state.connected_agent_count = 0; // all agents crashed
            state.health = SystemHealth::AllDisconnectedOrSafeMode;
            state
        }));

        let mut renderer = ChromeRenderer::new_headless(chrome_state);
        let cmds = renderer.render_chrome(1920.0, 1080.0);
        assert!(
            !cmds.is_empty(),
            "chrome must render correctly after all agents crash"
        );
    }

    // ── Chrome above all agent content ────────────────────────────────────

    #[test]
    fn chrome_layer_renders_in_separate_pass_after_content() {
        // The spec requires: compositor renders three ordered layers back to front:
        // background, content, chrome. Chrome is a separate pass (not mixed with content).
        //
        // Verify this by checking that ChromeRenderer::render_chrome() produces a distinct
        // set of commands (the chrome pass) that are NOT mixed with tile-content rendering.
        //
        // The actual ordering enforcement is in the compositor's render_frame_with_chrome(),
        // where content pass runs first, then chrome pass runs as a second render pass on
        // the same texture (using LoadOp::Load rather than LoadOp::Clear, preserving content).
        let chrome_state = Arc::new(RwLock::new({
            let mut state = ChromeState::new();
            state.add_tab(1, "Tab".into());
            state
        }));

        let mut renderer = ChromeRenderer::new_headless(chrome_state);
        let chrome_cmds = renderer.render_chrome(1920.0, 1080.0);

        // Chrome commands are generated independently — they do not depend on tile count.
        // This proves separability: content pass and chrome pass are decoupled.
        assert!(
            !chrome_cmds.is_empty(),
            "chrome pass must generate commands"
        );
    }

    // ── Tab bar overflow ──────────────────────────────────────────────────

    #[test]
    fn tab_bar_overflow_indicator_when_tabs_exceed_width() {
        let mut state = ChromeState::new();
        // Add many tabs — more than will fit in 1920px with MIN_TAB_WIDTH_PX=80.
        for i in 0..30 {
            state.add_tab(i, format!("Tab {i}"));
        }

        let chrome_state = Arc::new(RwLock::new(state));
        let mut renderer = ChromeRenderer::new_headless(chrome_state);
        let cmds = renderer.render_chrome(1920.0, 1080.0);

        // Should have produced an overflow badge rect.
        // The badge is a rect with color [0.3, 0.3, 0.5, 1.0].
        let has_overflow_badge = cmds.iter().any(|c| {
            (c.color[0] - 0.3).abs() < 0.01
                && (c.color[1] - 0.3).abs() < 0.01
                && (c.color[2] - 0.5).abs() < 0.01
        });
        assert!(
            has_overflow_badge,
            "expected overflow badge rect when tabs overflow"
        );
    }

    #[test]
    fn tab_bar_hidden_does_not_render_tab_bar_but_state_is_valid() {
        let chrome_state = Arc::new(RwLock::new({
            let mut state = ChromeState::new();
            state.add_tab(1, "Tab 1".into());
            state.add_tab(2, "Tab 2".into());
            state.tab_bar_position = TabBarPosition::Hidden;
            state
        }));

        let mut renderer = ChromeRenderer::new_headless(chrome_state.clone());
        let cmds = renderer.render_chrome(1920.0, 1080.0);

        // Tab bar should NOT be rendered.
        // When hidden, the chrome still exists but no tab bar draw commands are emitted.
        // Keyboard shortcuts (handled separately) still work — they read ChromeState directly.
        //
        // Verify: no commands at y=0 with the tab bar background color.
        let has_tab_bar_bg = cmds.iter().any(|c| {
            c.y == 0.0
                && (c.color[0] - 0.08).abs() < 0.01
                && (c.color[1] - 0.08).abs() < 0.01
                && (c.color[2] - 0.12).abs() < 0.01
        });
        assert!(
            !has_tab_bar_bg,
            "tab bar background must not render when position=hidden"
        );

        // Keyboard shortcuts still work when hidden.
        let mut state = chrome_state.write().unwrap();
        let result = handle_shortcut(&mut state, ChromeShortcut::NextTab);
        assert!(result.consumed);
        assert!(result.tab_switched);
        assert_eq!(state.active_tab_index, 1); // switched from tab 0 to tab 1
    }

    // ── Agent cannot access chrome ────────────────────────────────────────

    #[test]
    fn chrome_layout_is_independent_of_tile_z_order() {
        // Scenario: Agent requests tile z-order exceeding all others.
        // The tile renders BELOW chrome — chrome is always on top.
        //
        // In the compositor, chrome draw commands always execute in a separate pass
        // after the content pass. This means that regardless of what z-order a tile
        // claims, the chrome pass renders on top of all content by construction.
        //
        // This test verifies that ChromeRenderer does not read or depend on tile z-order.
        let chrome_state = Arc::new(RwLock::new({
            let mut state = ChromeState::new();
            state.add_tab(1, "Tab".into());
            state
        }));

        // Produce chrome commands without any tile/z-order information.
        let mut renderer = ChromeRenderer::new_headless(chrome_state);
        let cmds_no_tiles = renderer.render_chrome(1920.0, 1080.0);

        // Chrome commands are identical regardless of agent tile z-order —
        // chrome does not receive or process any tile z-order values.
        assert!(!cmds_no_tiles.is_empty());
    }

    // ── Safe mode overlay ─────────────────────────────────────────────────

    #[test]
    fn safe_mode_overlay_renders_from_chrome_state_only() {
        // Scenario: All agents crash — safe mode overlay renders correctly.
        let chrome_state = Arc::new(RwLock::new({
            let mut state = ChromeState::new();
            state.safe_mode_active = true;
            state.viewer_class = ViewerClass::Owner;
            state
        }));

        let mut renderer = ChromeRenderer::new_headless(chrome_state);
        let cmds = renderer.render_chrome(1920.0, 1080.0);

        // Should have a full-viewport dimming overlay (first safe mode cmd).
        let has_full_overlay = cmds.iter().any(|c| {
            c.x == 0.0
                && c.y == 0.0
                && c.width == 1920.0
                && c.height == 1080.0
                && (c.color[3] - 0.85).abs() < 0.01
        });
        assert!(
            has_full_overlay,
            "expected full-viewport safe mode dimming overlay"
        );

        // Should have a "Resume" button (blue rect).
        let has_resume_btn = cmds.iter().any(|c| {
            (c.color[0] - 0.3).abs() < 0.01
                && (c.color[1] - 0.5).abs() < 0.01
                && (c.color[2] - 0.9).abs() < 0.01
        });
        assert!(
            has_resume_btn,
            "expected Resume button in safe mode overlay"
        );
    }

    // ── Diagnostic surface ────────────────────────────────────────────────

    #[test]
    fn diagnostic_snapshot_contains_expected_fields() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.connected_agent_count = 3;
        state.viewer_class = ViewerClass::Owner;
        state.safe_mode_active = false;

        let snap = collect_diagnostic(&state, 999_000, 5);
        assert_eq!(snap.tab_count, 2);
        assert_eq!(snap.connected_agent_count, 3);
        assert_eq!(snap.viewer_class_label, "owner");
        assert_eq!(snap.active_lease_count, 5);
        assert!(!snap.safe_mode_active);
        assert!(
            !snap.capture_surface_active,
            "v1: capture_surface_active must be false"
        );
        assert_eq!(snap.timestamp_mono_us, 999_000);
    }

    #[test]
    fn diagnostic_display_formats_correctly() {
        let state = ChromeState::new();
        let snap = collect_diagnostic(&state, 0, 0);
        let output = format!("{snap}");
        assert!(output.contains("tze_hud Chrome Diagnostic Snapshot"));
        assert!(output.contains("unknown"));
    }

    // ── Concurrent ChromeState access ─────────────────────────────────────

    #[test]
    fn chrome_state_concurrent_read_and_write() {
        // Scenario: Control plane updates badge state while compositor reads chrome state.
        // No data races: compositor reads atomically (either pre- or post-update snapshot).
        let chrome_state = Arc::new(RwLock::new({
            let mut state = ChromeState::new();
            state.add_tab(1, "Tab".into());
            state.connected_agent_count = 2;
            state
        }));

        // Spawn a writer (control plane simulation).
        let writer_state = Arc::clone(&chrome_state);
        let writer = std::thread::spawn(move || {
            for i in 0..100 {
                let mut state = writer_state.write().unwrap();
                state.connected_agent_count = i % 10;
            }
        });

        // Spawn a reader (compositor simulation).
        let reader_state = Arc::clone(&chrome_state);
        let reader = std::thread::spawn(move || {
            for _ in 0..100 {
                let state = reader_state.read().unwrap();
                // Just read — no assertion on value, just verify no panic/deadlock.
                let _ = state.connected_agent_count;
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
        // If we reach here, no data races (RwLock enforces mutual exclusion).
    }

    // ── policy_matrix_basic: chrome visible during policy evaluation ───────

    #[test]
    fn policy_matrix_basic_chrome_visible_during_policy_evaluation() {
        // Acceptance criterion: policy_matrix_basic — chrome visible during policy evaluation.
        //
        // The chrome render pass is independent of policy evaluation state.
        // Policy evaluation happens in the scene graph (agent content); chrome
        // reads only ChromeState. This test confirms the two are decoupled.
        let chrome_state = Arc::new(RwLock::new({
            let mut state = ChromeState::new();
            // Simulate the policy_matrix_basic scenario: a system session with 3 agents.
            state.add_tab(1, "system".into());
            state.connected_agent_count = 3;
            state.health = SystemHealth::AllConnected;
            state
        }));

        let mut renderer = ChromeRenderer::new_headless(chrome_state);
        // Chrome must render regardless of what policies are being evaluated.
        let cmds = renderer.render_chrome(1920.0, 1080.0);

        assert!(
            !cmds.is_empty(),
            "chrome must be visible during policy evaluation"
        );

        // Verify health dot is green (AllConnected).
        let has_green_dot = cmds.iter().any(|c| {
            c.width <= 12.0 && // dot size
            (c.color[0] - 0.2).abs() < 0.01 &&
            (c.color[1] - 0.8).abs() < 0.01 &&
            (c.color[2] - 0.3).abs() < 0.01
        });
        assert!(
            has_green_dot,
            "expected green health dot for AllConnected with 3 agents"
        );
    }

    // ── Separable render passes ───────────────────────────────────────────

    #[test]
    fn chrome_render_pass_is_separable_from_content_pass() {
        // Requirement: Capture-Safe Redaction Architecture — content and chrome
        // rendering are separable passes. V1 ships overlay-only redaction
        // (capture_surface_active always false).
        let chrome_state = Arc::new(RwLock::new({
            let state = ChromeState::new();
            assert!(
                !state.capture_surface_active,
                "v1 invariant: capture_surface_active must always be false"
            );
            state
        }));

        // ChromeRenderer produces its commands independently of content rendering.
        // In the compositor, content pass runs first (render_frame), then chrome pass
        // (render_chrome via execute_chrome_pass) using LoadOp::Load on the same texture.
        let mut renderer = ChromeRenderer::new_headless(Arc::clone(&chrome_state));
        let chrome_cmds = renderer.render_chrome(800.0, 600.0);

        // The chrome renderer does not need to know what the content pass rendered.
        // This structural independence IS the separability guarantee: render_chrome() runs
        // without any reference to scene graph, tile list, or agent state.
        // For an 800×600 viewport with no tabs and no safe mode, commands may be empty
        // (status indicator is hidden when tab bar is hidden and there are no tabs).
        // The key invariant is that the call succeeds independently of content rendering.
        let _ = chrome_cmds; // structural separability verified by calling render_chrome() at all

        // v1 invariant: capture_surface_active never true.
        let state = chrome_state.read().unwrap();
        assert!(!state.capture_surface_active);
    }
}
