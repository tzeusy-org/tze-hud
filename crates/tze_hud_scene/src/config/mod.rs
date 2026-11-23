//! Configuration loader trait for v1.
//!
//! This module defines the configuration loader trait contract
//! and supporting types — no implementation is provided here.

pub mod agents;
pub mod hotkey;
pub use agents::{
    AgentDirectory, AgentIdentity, AuthRejection, DEFAULT_MCP_AGENT_ID, OPERATOR_ADMIN, PskDigest,
    SharedAgents, hash_psk,
};
pub use hotkey::{ChordRole, Hotkey, HotkeyKey, HotkeyParseError, HudChords};

// ─── Error Codes ─────────────────────────────────────────────────────────────

/// Stable configuration error codes.
///
/// Used when collecting structured configuration validation errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigErrorCode {
    ParseError,
    NoTabs,
    DuplicateTabName,
    MultipleDefaultTabs,
    UnknownProfile,
    UnknownZoneType,
    UnknownAllowEntry,
    /// `[agents]` in the config file; agents live in `agents.toml` (pairing).
    AgentsInConfigFile,
    /// `[display_profile]` in the config file; only the built-in profiles exist.
    DisplayProfileNotSupported,
    /// An `agents.toml` `psk_sha256` is not 64 hex characters.
    InvalidPskHash,
    /// Two `agents.toml` agents share one `psk_sha256`, so a PSK would not
    /// identify one agent.
    DuplicatePskHash,
    ConfigIncludesNotSupported,
    /// `[widget_bundles].paths` entry does not exist on disk.
    WidgetBundlePathNotFound,
    /// `[[tabs.widgets]]` entry references a widget type not loaded from any bundle.
    UnknownWidgetType,
    /// `[[tabs.widgets]]` `initial_params` fails schema validation.
    WidgetInvalidInitialParams,
    /// Two bundles declare the same widget type name.
    WidgetBundleDuplicateType,
    /// A key in `[design_tokens]` does not match the required pattern.
    InvalidTokenKey,
    /// A token value string could not be parsed into the expected format.
    TokenValueParseError,
    /// `[design_tokens] theme` names no built-in theme.
    UnknownTheme,
    Other(String),
}

/// A single structured validation error.
///
/// Used when collecting structured configuration validation errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub code: ConfigErrorCode,
    /// Dotted path to the offending field (e.g., `"runtime.profile"`).
    pub field_path: String,
    pub expected: String,
    pub got: String,
    /// Machine-readable correction suggestion.
    pub hint: String,
}

/// Parse error with line and column information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    /// 1-indexed line number of the error.
    pub line: u32,
    /// 1-indexed column number of the error.
    pub column: u32,
}

// ─── Built-in Profiles ────────────────────────────────────────────────────────

/// Default per-surface bound on the bytes a single uncached truncation may
/// shape before the compositor's viewport-adjacent-window fallback engages
///
/// This mirrors `tze_hud_compositor::overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES`:
/// the compositor's `TruncationCache` falls back to this same value when no
/// profile-supplied bound is applied. The 4096-byte default bounds work toward
/// the Stage-5 Layout Resolve target (< 1 ms); conformance still requires
/// measurement of the actual workload.
pub const DEFAULT_MAX_TRUNCATION_INPUT_BYTES: u32 = 4096;

/// A resolved display profile with its budget values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayProfile {
    pub name: String,
    pub max_tiles: u32,
    pub max_texture_mb: u32,
    /// Aggregate runtime-owned resident-memory ceiling in MiB.
    pub max_runtime_resident_mb: u32,
    /// Scene resource/image CPU and GPU residency ceiling in MiB.
    pub max_resource_resident_mb: u32,
    /// Retained runtime widget source residency ceiling in MiB.
    pub max_widget_asset_resident_mb: u32,
    /// Widget raster cache residency ceiling in MiB.
    pub max_widget_raster_cache_mb: u32,
    /// Font face, glyph, and atlas residency ceiling in MiB.
    pub max_font_resident_mb: u32,
    pub max_agents: u32,
    /// Maximum agent update rate in Hz (per-agent state-stream ceiling).
    pub max_agent_update_hz: u32,
    pub target_fps: u32,
    pub min_fps: u32,
    pub allow_background_zones: bool,
    pub allow_chrome_zones: bool,
    /// Per-surface bound on the bytes a single uncached truncation may shape
    /// before the compositor's viewport-adjacent-window fallback restricts the
    /// shaped input to a viewport-adjacent window of whole source lines
    ///
    /// Both built-in profiles use [`DEFAULT_MAX_TRUNCATION_INPUT_BYTES`].
    pub max_truncation_input_bytes: u32,
}

impl DisplayProfile {
    /// Looks up a built-in profile by its `[runtime].profile` name.
    pub fn builtin(name: &str) -> Option<Self> {
        match name {
            "full-display" => Some(Self::full_display()),
            "headless" => Some(Self::headless()),
            _ => None,
        }
    }

    /// Returns the `full-display` profile defaults.
    pub fn full_display() -> Self {
        DisplayProfile {
            name: "full-display".into(),
            max_tiles: 1024,
            max_texture_mb: 2048,
            max_runtime_resident_mb: 1024,
            max_resource_resident_mb: 512,
            max_widget_asset_resident_mb: 192,
            max_widget_raster_cache_mb: 256,
            max_font_resident_mb: 64,
            max_agents: 16,
            max_agent_update_hz: 60,
            target_fps: 60,
            min_fps: 30,
            allow_background_zones: true,
            allow_chrome_zones: true,
            max_truncation_input_bytes: DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
        }
    }

    /// Returns the `headless` profile defaults.
    pub fn headless() -> Self {
        DisplayProfile {
            name: "headless".into(),
            max_tiles: 256,
            max_texture_mb: 512,
            max_runtime_resident_mb: 512,
            max_resource_resident_mb: 256,
            max_widget_asset_resident_mb: 64,
            max_widget_raster_cache_mb: 128,
            max_font_resident_mb: 64,
            max_agents: 8,
            max_agent_update_hz: 60,
            target_fps: 60,
            min_fps: 1,
            allow_background_zones: false,
            allow_chrome_zones: false,
            max_truncation_input_bytes: DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
        }
    }
}

// ─── Resolved Config ──────────────────────────────────────────────────────────

/// A fully validated, frozen configuration.
///
/// Returned by `tze_hud_config::TzeHudConfig::freeze()`.
#[derive(Clone, Debug)]
pub struct ResolvedConfig {
    pub profile: DisplayProfile,
    pub tab_names: Vec<String>,
    /// Global chords: pause, panel and pairing (`[runtime].*_hotkey`).
    pub chords: HudChords,
    /// Sourced TOML file path.
    pub source_path: Option<String>,
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // These tests exercise DisplayProfile constants. Loader tests live in
    // tze_hud_config/src/tests.rs alongside TzeHudConfig.

    /// WHEN profile = "full-display" THEN correct budget values resolved.
    #[test]
    fn test_full_display_profile_budget_values() {
        let p = DisplayProfile::full_display();
        assert_eq!(p.max_tiles, 1024);
        assert_eq!(p.max_texture_mb, 2048);
        assert_eq!(p.max_agents, 16);
        assert_eq!(p.target_fps, 60);
        assert_eq!(p.min_fps, 30);
    }

    /// WHEN profile = "headless" THEN correct budget values resolved.
    #[test]
    fn test_headless_profile_budget_values() {
        let p = DisplayProfile::headless();
        assert_eq!(p.max_tiles, 256);
        assert_eq!(p.max_texture_mb, 512);
        assert_eq!(p.max_agents, 8);
        assert_eq!(p.max_agent_update_hz, 60);
        assert_eq!(p.target_fps, 60);
        assert_eq!(p.min_fps, 1);
    }

    #[test]
    fn built_in_resident_classes_exactly_partition_the_aggregate() {
        for profile in [DisplayProfile::full_display(), DisplayProfile::headless()] {
            let class_total = u64::from(profile.max_resource_resident_mb)
                + u64::from(profile.max_widget_asset_resident_mb)
                + u64::from(profile.max_widget_raster_cache_mb)
                + u64::from(profile.max_font_resident_mb);
            assert_eq!(
                class_total,
                u64::from(profile.max_runtime_resident_mb),
                "{} class ceilings must exactly partition the aggregate",
                profile.name
            );
        }
    }

    #[test]
    fn only_full_display_and_headless_are_built_in() {
        assert_eq!(
            DisplayProfile::builtin("headless"),
            Some(DisplayProfile::headless())
        );
        assert!(DisplayProfile::builtin("full-display").is_some());
        assert!(DisplayProfile::builtin("auto").is_none());
        assert!(DisplayProfile::builtin("custom").is_none());
    }
}
