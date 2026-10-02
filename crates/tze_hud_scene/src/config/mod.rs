//! Configuration loader trait for v1.
//!
//! Encodes the configuration specification from
//! `configuration/spec.md §Requirement: TOML Configuration Format`
//! and related requirements.  This module defines **only** the trait contract
//! and supporting types — no implementation is provided here.

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
    UnknownCapability,
    InvalidEventName,
    AgentBudgetExceedsProfile,
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
    ///
    /// Per-agent `max_update_hz` MUST NOT exceed this value.
    /// Violations produce `CONFIG_AGENT_BUDGET_EXCEEDS_PROFILE`.
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

/// Validated optional per-agent budget overrides retained at config freeze.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegisteredAgentBudgetOverrides {
    pub max_tiles: Option<u32>,
    pub max_texture_mb: Option<u32>,
    pub max_update_hz: Option<u32>,
}

/// A fully validated, frozen configuration.
///
/// Returned by `ConfigLoader::freeze()`.
#[derive(Clone, Debug)]
pub struct ResolvedConfig {
    pub profile: DisplayProfile,
    pub tab_names: Vec<String>,
    pub agent_capabilities: std::collections::HashMap<String, Vec<String>>,
    /// Per-agent budget overrides keyed by registered agent name.
    pub agent_budget_overrides: std::collections::HashMap<String, RegisteredAgentBudgetOverrides>,
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
/// - Validate capability names against the canonical v1 vocabulary.
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

    /// Returns `true` if the capability name is in the canonical v1 vocabulary.
    ///
    /// From spec §Requirement: Capability Vocabulary.
    fn is_known_capability(name: &str) -> bool
    where
        Self: Sized;
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Canonical v1 capability names (from spec §Requirement: Capability Vocabulary).
pub const CANONICAL_CAPABILITIES: &[&str] = &[
    "create_tiles",
    "modify_own_tiles",
    "manage_tabs",
    "upload_resource",
    "register_widget_asset",
    "read_scene_topology",
    "access_input_events",
    "read_telemetry",
    "resident_mcp",
    // Parameterized — wildcard or specific zone.
    "publish_zone:*",
    // Parameterized — wildcard or specific widget.
    "publish_widget:*",
    // "publish_zone:<zone_name>", "publish_widget:<widget_name>", and
    // "lease:priority:<N>" are validated by prefix pattern, not exact match.
];

/// Returns `true` if `name` is a valid v1 capability per the canonical vocabulary.
///
/// Parameterized forms are validated:
/// - `publish_zone:<name>`: suffix must be non-empty.
/// - `lease:priority:<N>`: suffix must be a non-empty numeric value.
pub fn is_canonical_capability(name: &str) -> bool {
    // Exact matches.
    if CANONICAL_CAPABILITIES.contains(&name) {
        return true;
    }
    // Parameterized forms with validation.
    if let Some(zone_name) = name.strip_prefix("publish_zone:") {
        return !zone_name.is_empty();
    }
    if let Some(widget_name) = name.strip_prefix("publish_widget:") {
        return !widget_name.is_empty();
    }
    if let Some(priority_str) = name.strip_prefix("lease:priority:") {
        if priority_str.is_empty() {
            return false;
        }
        return priority_str.parse::<u32>().is_ok();
    }
    false
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // These tests exercise logic that lives in this module (capability validation
    // and DisplayProfile constants).  ConfigLoader conformance tests live in
    // tze_hud_config/src/tests.rs alongside the TzeHudConfig implementation.

    /// WHEN valid canonical capability names used THEN no error.
    #[test]
    fn test_canonical_capability_names_valid() {
        assert!(is_canonical_capability("create_tiles"));
        assert!(is_canonical_capability("read_scene_topology"));
        assert!(is_canonical_capability("access_input_events"));
        assert!(is_canonical_capability("register_widget_asset"));
        assert!(is_canonical_capability("publish_zone:subtitle"));
        assert!(is_canonical_capability("lease:priority:1"));
        assert!(is_canonical_capability("resident_mcp"));
    }

    /// WHEN non-canonical capability names used THEN validation rejects them.
    #[test]
    fn test_non_canonical_capability_names_invalid() {
        // Pre-Round-14 names.
        assert!(
            !is_canonical_capability("read_scene"),
            "read_scene is pre-Round-14"
        );
        assert!(
            !is_canonical_capability("receive_input"),
            "receive_input is pre-Round-14"
        );
        assert!(
            !is_canonical_capability("zone_publish:subtitle"),
            "zone_publish is pre-Round-14"
        );
        // Wrong case / format.
        assert!(
            !is_canonical_capability("CREATE_TILE"),
            "uppercase not allowed"
        );
        assert!(
            !is_canonical_capability("create-tiles"),
            "kebab-case not allowed"
        );
    }

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
