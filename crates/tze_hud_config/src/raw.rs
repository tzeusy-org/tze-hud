//! Raw TOML-deserialisable structs.
//!
//! These are the intermediate representations produced by `toml::from_str`.
//! They mirror the configuration file structure exactly and are deliberately
//! permissive — all fields except the structurally-required ones are `Option`
//! so that we can collect all missing/invalid-value errors in the validation
//! phase rather than failing at deserialisation.
//!
//! All of these types derive `schemars::JsonSchema` so that the `--print-schema`
//! feature can generate a full JSON Schema from them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Helper: AnyValue for `includes` field ────────────────────────────────────

/// Wrapper that accepts any TOML value during deserialization.
///
/// Used exclusively for the `includes` field to detect its presence and
/// report a hard error (v1-reserved; post-v1 only).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AnyValue(pub toml::Value);

impl JsonSchema for AnyValue {
    fn schema_name() -> String {
        "AnyValue".to_string()
    }

    fn json_schema(_gen: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        // Represents any JSON value.
        schemars::schema::Schema::Bool(true)
    }
}

// ─── [runtime] ───────────────────────────────────────────────────────────────

/// `[runtime]` table — required.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawRuntime {
    /// Display profile name.  Must be present.
    pub profile: Option<String>,

    /// Write the JSON Schema at startup and continue running.
    #[serde(default)]
    pub emit_schema: bool,

    /// Virtual display width for headless mode (default 1920).
    pub headless_width: Option<u32>,

    /// Virtual display height for headless mode (default 1080).
    pub headless_height: Option<u32>,
}

// ─── [display_profile] ───────────────────────────────────────────────────────

/// `[display_profile]` table — optional; used for custom profiles that extend
/// a built-in.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawDisplayProfile {
    /// Built-in profile to extend (only `full-display`; NOT `headless`).
    pub extends: Option<String>,

    /// Override: max tiles.
    pub max_tiles: Option<u32>,
    /// Override: max texture memory in MiB.
    pub max_texture_mb: Option<u32>,
    /// Override: aggregate runtime-owned resident memory in MiB.
    pub max_runtime_resident_mb: Option<u32>,
    /// Override: scene resource/image resident memory in MiB.
    pub max_resource_resident_mb: Option<u32>,
    /// Override: retained widget source resident memory in MiB.
    pub max_widget_asset_resident_mb: Option<u32>,
    /// Override: widget raster cache resident memory in MiB.
    pub max_widget_raster_cache_mb: Option<u32>,
    /// Override: font resident memory in MiB.
    pub max_font_resident_mb: Option<u32>,
    /// Override: max simultaneous agents.
    pub max_agents: Option<u32>,
    /// Override: max media streams.
    pub max_media_streams: Option<u32>,
    /// Override: max agent update Hz.
    pub max_agent_update_hz: Option<u32>,
    /// Override: target FPS.
    pub target_fps: Option<u32>,
    /// Override: minimum FPS.
    pub min_fps: Option<u32>,
    /// Override: per-surface bound (bytes) on the input a single uncached
    /// truncation may shape before the compositor's viewport-adjacent-window
    /// fallback engages (spec.md §324/§331). Unset → the resolved profile keeps
    /// `DEFAULT_MAX_TRUNCATION_INPUT_BYTES` (4096).
    pub max_truncation_input_bytes: Option<u32>,

    /// Override: allow background zones.
    pub allow_background_zones: Option<bool>,
    /// Override: allow chrome zones.
    pub allow_chrome_zones: Option<bool>,
}

// ─── [[tabs]] ────────────────────────────────────────────────────────────────

/// A single entry in the `[[tabs]]` array.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawTab {
    /// Human-readable tab name.  Must be unique.
    pub name: Option<String>,

    /// Whether this is the default tab.
    #[serde(default)]
    pub default_tab: bool,

    /// Default tile layout for this tab.
    pub default_layout: Option<String>,

    /// Scene event name that triggers an automatic switch to this tab.
    /// Empty string = no auto-switch.
    pub tab_switch_on_event: Option<String>,

    /// Layout fractions.
    pub layout: Option<RawTabLayout>,

    /// Zone types active on this tab.
    ///
    /// Each entry must be either a built-in zone type (see
    /// `zones::BUILTIN_ZONE_TYPES`) or a custom type defined in the
    /// `[zones]` section.  An unknown name produces `CONFIG_UNKNOWN_ZONE_TYPE`.
    #[serde(default)]
    pub zones: Vec<String>,

    /// Widget instances declared on this tab.
    ///
    /// Each entry must reference a widget type loaded from a bundle in
    /// `[widget_bundles].paths`. Unknown widget types produce
    /// `CONFIG_UNKNOWN_WIDGET_TYPE`.
    #[serde(default)]
    pub widgets: Vec<RawTabWidget>,
}

/// Layout fractions within a tab.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawTabLayout {
    pub reserved_top_fraction: Option<f64>,
    pub reserved_bottom_fraction: Option<f64>,
    pub reserved_left_fraction: Option<f64>,
    pub reserved_right_fraction: Option<f64>,
}

// ─── [zones] ─────────────────────────────────────────────────────────────────

/// `[zones]` table — optional.  Custom zone type definitions.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawZones(pub HashMap<String, RawZoneType>);

/// A single custom zone type definition.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawZoneType {
    pub policy: Option<String>,
    pub layer: Option<String>,
}

// ─── [agents] ────────────────────────────────────────────────────────────────

/// `[agents]` table — optional.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawAgents {
    pub registered: Option<HashMap<String, RawRegisteredAgent>>,
    pub dynamic_policy: Option<RawDynamicPolicy>,
}

/// A single pre-registered agent entry under `[agents.registered.<name>]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawRegisteredAgent {
    pub capabilities: Option<Vec<String>>,
    pub auth_psk_env: Option<String>,
    pub max_tiles: Option<u32>,
    pub max_texture_mb: Option<u32>,
    pub max_update_hz: Option<u32>,
}

/// `[agents.dynamic_policy]` — optional.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawDynamicPolicy {
    #[serde(default)]
    pub allow_dynamic_agents: bool,
    pub default_capabilities: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub prompt_for_elevated_capabilities: bool,
    pub dynamic_presence_ceiling: Option<String>,
}

fn default_true() -> bool {
    true
}

// ─── [widget_bundles] ────────────────────────────────────────────────────────

/// `[widget_bundles]` table — optional.
///
/// Specifies directories to scan for widget asset bundles. Each directory is
/// scanned for immediate subdirectories containing `widget.toml` manifests.
/// Paths are resolved relative to the configuration file's parent directory.
///
/// Absence of this section means no widget types are loaded (empty registry).
/// This is valid — the runtime starts with an empty widget registry.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawWidgetBundles {
    /// Array of directory paths to scan for widget bundles.
    /// Each path is resolved relative to the config file's parent directory.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// `[widget_runtime_assets]` table — optional.
///
/// Configures the durable runtime widget SVG asset store used for assets
/// registered while the runtime is active.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawWidgetRuntimeAssets {
    /// Root directory for durable widget SVG blobs + metadata index.
    /// Relative paths are resolved against the config file parent directory.
    pub store_path: Option<String>,
    /// Global durable footprint ceiling in bytes.
    pub max_total_bytes: Option<u64>,
    /// Per-agent durable footprint ceiling in bytes.
    pub max_agent_bytes: Option<u64>,
}

/// A `[[tabs.widgets]]` entry declaring a widget instance on a tab.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawTabWidget {
    /// Widget type name (must match a loaded bundle's widget type name).
    pub widget_type: Option<String>,

    /// Optional instance ID. When multiple instances of the same widget type
    /// exist on a tab, `instance_id` disambiguates them. When absent, the
    /// `widget_type` name is used as the instance name.
    pub instance_id: Option<String>,

    /// Optional geometry override (overrides the widget type's default_geometry_policy).
    pub geometry: Option<RawWidgetGeometry>,

    /// Optional initial parameter values. Validated against the widget type's
    /// parameter schema at startup.
    ///
    /// Uses `AnyValue` wrappers to satisfy `JsonSchema` (same approach as `includes`).
    #[serde(default)]
    pub initial_params: HashMap<String, AnyValue>,

    /// Contention policy override. When absent, the widget type's
    /// default_contention_policy is used.
    pub contention: Option<String>,

    /// Auto-clear TTL in milliseconds. When set, the widget occupancy is
    /// automatically cleared after this duration.
    pub auto_clear_ms: Option<u64>,
}

/// Inline geometry override for a widget instance.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawWidgetGeometry {
    /// Absolute pixel x-coordinate (top-left origin).
    pub x: Option<f32>,
    /// Absolute pixel y-coordinate.
    pub y: Option<f32>,
    /// Width in pixels.
    pub width: Option<f32>,
    /// Height in pixels.
    pub height: Option<f32>,
    /// Fractional x-position (0.0–1.0, relative to display width).
    pub x_pct: Option<f32>,
    /// Fractional y-position.
    pub y_pct: Option<f32>,
    /// Fractional width.
    pub width_pct: Option<f32>,
    /// Fractional height.
    pub height_pct: Option<f32>,
}

// ─── [design_tokens] ─────────────────────────────────────────────────────────

/// `[design_tokens]` table — optional.
///
/// A flat key→value map of design tokens.  All keys must match
/// `^[a-z][a-z0-9]*(\.[a-z][a-z0-9_]*)*$`.  Values are opaque strings
/// that are parsed into typed values at runtime (color, numeric, font family,
/// or literal string).
///
/// Unknown keys (non-canonical) are accepted and passed through unchanged.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawDesignTokens(pub HashMap<String, String>);

// ─── Top-level document ──────────────────────────────────────────────────────

/// The top-level TOML document.
///
/// All sections are optional to allow maximum error collection; the validator
/// enforces required fields.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct RawConfig {
    /// Optional config schema version. Absent is treated as the current
    /// supported version (back-compatible for existing v1 configs); a value
    /// greater than the runtime's maximum supported version fails closed with
    /// `CONFIG_SCHEMA_VERSION_UNSUPPORTED` (configuration spec §Config Schema
    /// Version and Compatibility Policy).
    #[serde(default)]
    pub schema_version: Option<u32>,

    /// `includes` is v1-reserved.  Presence must produce a hard error.
    /// Accepts any value — detection of presence triggers the error in validation.
    pub includes: Option<AnyValue>,

    pub runtime: Option<RawRuntime>,
    pub display_profile: Option<RawDisplayProfile>,

    #[serde(default)]
    pub tabs: Vec<RawTab>,

    pub zones: Option<RawZones>,
    pub agents: Option<RawAgents>,
    /// Optional widget bundle directories to scan at startup.
    pub widget_bundles: Option<RawWidgetBundles>,
    /// Optional runtime widget asset store configuration.
    pub widget_runtime_assets: Option<RawWidgetRuntimeAssets>,
    /// Optional design token overrides.
    #[serde(default)]
    pub design_tokens: Option<RawDesignTokens>,
}
