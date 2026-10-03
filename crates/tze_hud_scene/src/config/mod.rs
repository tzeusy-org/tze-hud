//! Configuration loader trait for v1.
//!
//! Encodes the configuration specification from
//! `configuration/spec.md §Requirement: TOML Configuration Format`
//! and related requirements.  This module defines **only** the trait contract
//! and supporting types — no implementation is provided here.

pub mod agents;
pub use agents::{
    AgentDirectory, AgentIdentity, AuthRejection, DEFAULT_MCP_AGENT_ID, PskDigest, SharedAgents,
    hash_psk,
};

// ─── Error Codes ─────────────────────────────────────────────────────────────

/// Stable configuration error codes.
///
/// From spec §Requirement: Structured Validation Error Collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigErrorCode {
    ParseError,
    NoTabs,
    DuplicateTabName,
    MultipleDefaultTabs,
    UnknownLayout,
    UnknownProfile,
    HeadlessNotExtendable,
    ProfileExtendsConflictsWithProfile,
    ProfileBudgetEscalation,
    /// Resident-memory class ceilings do not fit within the aggregate ceiling.
    ProfileResidentBudgetInvalid,
    ProfileCapabilityEscalation,
    UnknownZoneType,
    UnknownAllowEntry,
    InvalidEventName,
    /// `[agents]` in the config file; agents live in `agents.toml` (pairing).
    AgentsInConfigFile,
    /// An `agents.toml` `psk_sha256` is not 64 hex characters.
    InvalidPskHash,
    InvalidReservedFraction,
    InvalidFpsRange,
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
    Other(String),
}

/// A single structured validation error.
///
/// From spec §Requirement: Structured Validation Error Collection.
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
/// (spec.md §324/§331).
///
/// This mirrors `tze_hud_compositor::overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES`:
/// the compositor's `TruncationCache` falls back to this same value when no
/// profile-supplied bound is applied, so an unset `[display_profile]` preserves
/// the historical 4096-byte behaviour. The value sits well below the ~8 KiB
/// point where the `overflow_truncate` benchmark first exceeds the Stage-5
/// Layout Resolve budget (< 1 ms).
pub const DEFAULT_MAX_TRUNCATION_INPUT_BYTES: u32 = 4096;

/// A resolved display profile with its budget values.
///
/// From spec §Requirement: Display Profile full-display and related.
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
    /// (spec.md §324/§331).
    ///
    /// Operators tune this per surface via `[display_profile]
    /// max_truncation_input_bytes`: lower it on constrained hosts to keep a
    /// single uncached truncation inside the Stage-5 Layout Resolve budget, or
    /// raise it on capable hosts that can afford shaping a larger committed
    /// transcript. Defaults to [`DEFAULT_MAX_TRUNCATION_INPUT_BYTES`].
    pub max_truncation_input_bytes: u32,
}

impl DisplayProfile {
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
/// Returned by `ConfigLoader::freeze()`.
#[derive(Clone, Debug)]
pub struct ResolvedConfig {
    pub profile: DisplayProfile,
    pub tab_names: Vec<String>,
    /// Sourced TOML file path.
    pub source_path: Option<String>,
}

// ─── ConfigLoader Trait ───────────────────────────────────────────────────────

/// Trait encoding the configuration loading and validation contract.
///
/// Implementations must:
/// - Accept only TOML with parse errors including line/column.
/// - Search configuration file chain (CLI → env → cwd → XDG) in order.
/// - Enforce built-in profile budget values exactly.
/// - Prevent budget escalation in custom profiles.
/// - Reject `[agents]` (agents live in `agents.toml`).
/// - Collect ALL validation errors before reporting.
/// - Reject `includes` fields (post-v1 reserved).
pub trait ConfigLoader {
    /// Parse a TOML configuration string.
    ///
    /// Returns `Err(ParseError)` with line and column if the TOML is invalid.
    fn parse(toml_src: &str) -> Result<Self, ParseError>
    where
        Self: Sized;

    /// Apply normalisation rules (resolve profile, fill defaults).
    fn normalize(&mut self);

    /// Validate all fields.  Returns ALL validation errors (never stops at first).
    fn validate(&self) -> Vec<ConfigError>;

    /// Freeze the validated config into a `ResolvedConfig`.
    ///
    /// Returns `Err` (with all errors) if validation fails.
    fn freeze(self) -> Result<ResolvedConfig, Vec<ConfigError>>;

    /// Resolve the file path to use according to the search chain:
    /// (1) `cli_path`, (2) `$TZE_HUD_CONFIG` env, (3) `./tze_hud.toml`,
    /// (4) XDG config.
    ///
    /// Returns `Ok(path)` for the first path found; returns `Err(searched_paths)` when
    /// no file is found, where `searched_paths` is the ordered list of paths that were tried.
    fn resolve_config_path(cli_path: Option<&str>) -> Result<String, Vec<String>>
    where
        Self: Sized;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // These tests exercise DisplayProfile constants.  ConfigLoader conformance tests live in
    // tze_hud_config/src/tests.rs alongside the TzeHudConfig implementation.

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
}
