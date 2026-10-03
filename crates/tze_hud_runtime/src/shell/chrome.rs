//! Chrome state — runtime-owned shell state that agents can never see or address.
//!
//! [`ChromeState`] holds the tab slots, the safe-mode flag and the agent count.
//! The windowed runtime reads it for the safe-mode overlay and mutates it only
//! from the keyboard shortcut path (`handle_shortcut`) and the safe-mode functions.
//! Chrome elements never appear in scene topology and are not addressable via
//! `SceneId`; the overlay itself is drawn by the compositor's windowed frame.

use tze_hud_scene::types::SceneId;

// ─── Tab bar position ────────────────────────────────────────────────────────

/// Where the tab bar renders (reported by the diagnostic snapshot).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabBarPosition {
    #[default]
    Top,
    Bottom,
    Hidden,
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

// ─── ChromeState ─────────────────────────────────────────────────────────────

/// The authoritative runtime-owned shell state.
///
/// Protected by `Arc<RwLock<ChromeState>>`; writers hold the lock only for
/// short-lived updates.
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
    /// Whether safe mode is currently active.
    pub safe_mode_active: bool,
    /// Number of currently connected agents (for system status indicator).
    pub connected_agent_count: u32,
    /// Capture surface active (v1-reserved: always false — overlay-only redaction).
    pub capture_surface_active: bool,
}

impl ChromeState {
    /// Create a new ChromeState with default configuration.
    pub fn new() -> Self {
        Self::default()
    }

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
}

// ─── Keyboard shortcut handling ───────────────────────────────────────────────

/// Keyboard events that the chrome layer intercepts.
///
/// These are handled before tile hit-testing and are NEVER routed to agents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeShortcut {
    /// Ctrl+Tab — switch to the next tab.
    Next,
    /// Ctrl+Shift+Tab — switch to the previous tab.
    Prev,
    /// Ctrl+1 through Ctrl+8 — switch to a specific tab (1-indexed).
    Goto(usize),
    /// Ctrl+9 — switch to the last tab.
    Last,
}

/// Result of processing a keyboard event.
#[derive(Clone, Debug)]
pub struct ShortcutResult {
    /// Whether the shortcut was consumed (never route to agents if true).
    pub consumed: bool,
    /// Index of the new active tab, if a tab switch occurred.
    pub new_tab_index: Option<usize>,
}

/// Handle a [`ChromeShortcut`] against the given [`ChromeState`].
///
/// The state write lock must be held by the caller.
/// Shortcut events are NEVER routed to any agent.
pub fn handle_shortcut(state: &mut ChromeState, shortcut: ChromeShortcut) -> ShortcutResult {
    let switched = match shortcut {
        ChromeShortcut::Next => state.switch_to_next_tab(),
        ChromeShortcut::Prev => state.switch_to_prev_tab(),
        // n is 1-indexed (Ctrl+1 = index 0, Ctrl+8 = index 7).
        ChromeShortcut::Goto(n) => state.switch_to_tab_index(n.saturating_sub(1)),
        ChromeShortcut::Last => state.switch_to_last_tab(),
    };
    ShortcutResult {
        consumed: true,
        new_tab_index: switched.then_some(state.active_tab_index),
    }
}

// ─── Dismiss tile override ────────────────────────────────────────────────────

/// Result of a dismiss tile action.
///
/// Dismiss is unconditional: local execution, frame-bounded, no agent veto.
/// Works even if the agent is disconnected or in the reconnect grace period.
#[derive(Clone, Debug)]
pub struct DismissTileResult {
    /// The terminal lease transition. `Some` when the tile's lease was
    /// reclaimed; forward it to the owning session
    /// (`Reclaimed{OVERRIDE}`). `None` when the tile or lease was already gone.
    pub expiry: Option<tze_hud_scene::types::LeaseExpiry>,
}

/// Viewer dismiss of a tile (hover close button): reclaim its lease now.
///
/// The single entry for the human override on a tile. It runs on the input
/// thread's own scene lock, so the tile is gone before the next frame and no
/// agent round trip is involved. The caller publishes `expiry` after releasing
/// the lock. Portal tiles go through
/// `InProcessPortalDriver::viewer_dismiss_tile`, which also drops the
/// projection.
pub fn dismiss_tile(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    tile_id: SceneId,
) -> DismissTileResult {
    DismissTileResult {
        expiry: scene.viewer_dismiss_tile(tile_id),
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
        writeln!(f, "  safe_mode:          {}", self.safe_mode_active)?;
        writeln!(f, "  capture_surface:    {}", self.capture_surface_active)?;
        Ok(())
    }
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
        assert!(!state.safe_mode_active);
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

        let result = handle_shortcut(&mut state, ChromeShortcut::Next);

        assert!(
            result.consumed,
            "shortcut must be consumed — never routed to agents"
        );
        assert_eq!(result.new_tab_index, Some(1));
        assert_eq!(state.active_tab_index, 1);
    }

    #[test]
    fn ctrl_shift_tab_switches_to_prev_tab() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.switch_to_tab_index(1); // B active

        let result = handle_shortcut(&mut state, ChromeShortcut::Prev);

        assert!(result.consumed);
        assert_eq!(state.active_tab_index, 0);
    }

    #[test]
    fn ctrl_1_switches_to_first_tab() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.add_tab(3, "C".into());
        state.switch_to_tab_index(2); // C active

        let result = handle_shortcut(&mut state, ChromeShortcut::Goto(1));

        assert!(result.consumed);
        assert_eq!(state.active_tab_index, 0);
    }

    #[test]
    fn ctrl_9_switches_to_last_tab() {
        let mut state = ChromeState::new();
        for i in 1..=5 {
            state.add_tab(i, format!("Tab {i}"));
        }

        let result = handle_shortcut(&mut state, ChromeShortcut::Last);

        assert!(result.consumed);
        assert_eq!(state.active_tab_index, 4);
    }

    // ── Diagnostic surface ────────────────────────────────────────────────

    #[test]
    fn diagnostic_snapshot_contains_expected_fields() {
        let mut state = ChromeState::new();
        state.add_tab(1, "A".into());
        state.add_tab(2, "B".into());
        state.connected_agent_count = 3;
        state.safe_mode_active = false;

        let snap = collect_diagnostic(&state, 999_000, 5);
        assert_eq!(snap.tab_count, 2);
        assert_eq!(snap.connected_agent_count, 3);
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
        assert!(output.contains("tab_bar_position:   top"));
    }
}
