//! # tze_hud_config
//!
//! TOML configuration loading and validation for tze_hud.
//!
//! This crate provides `TzeHudConfig`, the concrete implementation of the
//! `ConfigLoader` trait defined in `tze_hud_scene::config`.
//!
//! ## Scope
//!
//! ### rig-j90m (TOML schema and file loading)
//! - TOML Configuration Format (v1-mandatory)
//! - Configuration File Resolution Order (v1-mandatory)
//! - Minimal Valid Configuration (v1-mandatory)
//! - Structured Validation Error Collection (v1-mandatory)
//! - Tab Configuration Validation (v1-mandatory)
//! - Reserved Fraction Validation (v1-mandatory)
//! - FPS Range Validation (v1-mandatory)
//! - Degradation Threshold Ordering (v1-mandatory)
//! - Scene Event Naming Convention (v1-mandatory)
//! - Schema Export (v1-mandatory)
//! - Layered Config Composition guard (v1-reserved: hard error)
//!
//! ### rig-umgy (Display profile resolution)
//! - Display Profile full-display (v1-mandatory)
//! - Display Profile headless (v1-mandatory)
//! - Profile Auto-Detection (v1-mandatory)
//! - Profile Budget Escalation Prevention (v1-mandatory)
//! - Profile Extends Conflict Detection (v1-mandatory)
//! - Headless Virtual Display (v1-mandatory)
//!
//! ### rig-mop4 (Zone registry, agent registration, hot-reload)
//! - Zone Registry Configuration (v1-mandatory)
//! - Agent Registration with Per-Agent Budget Overrides (v1-mandatory)
//! - Per-agent PSK (`psk_env`) and allow lists
//! - Configuration Reload (v1-mandatory)

pub mod agents;
pub mod allow;
pub mod loader;
pub mod policy_builder;
pub mod portal_tokens;
pub mod profile;
pub mod raw;
pub mod reload;
pub mod resolver;
pub mod runtime_widget_assets;
pub mod schema;
#[cfg(test)]
mod tests;
pub mod tokens;
pub mod widgets;
pub mod zones;

pub use agents::{
    AuthEnvWarning, resolve_agent_psks, resolve_agent_psks_with_lookup, validate_agents,
};
pub use allow::{allow_to_permissions, validate_allow_entry};
pub use loader::TzeHudConfig;
pub use policy_builder::{
    apply_token_defaults_for_zone, build_all_effective_policies, build_effective_policy,
    tile_placement_from_tokens,
};
pub use portal_tokens::{
    PORTAL_TOKEN_COLLAPSED_BACKGROUND, PORTAL_TOKEN_COLLAPSED_FONT_SIZE,
    PORTAL_TOKEN_COLLAPSED_TEXT_COLOR, PORTAL_TOKEN_COMPOSER_AT_CAPACITY_COLOR,
    PORTAL_TOKEN_COMPOSER_BACKGROUND, PORTAL_TOKEN_COMPOSER_CARET_COLOR,
    PORTAL_TOKEN_COMPOSER_FONT_SIZE, PORTAL_TOKEN_COMPOSER_PLACEHOLDER_COLOR,
    PORTAL_TOKEN_COMPOSER_SELECTION_COLOR, PORTAL_TOKEN_COMPOSER_TEXT_COLOR,
    PORTAL_TOKEN_DIVIDER_COLOR, PORTAL_TOKEN_FOCUS_RING_COLOR, PORTAL_TOKEN_FOCUS_RING_WIDTH_PX,
    PORTAL_TOKEN_FRAME_BACKGROUND, PORTAL_TOKEN_FRAME_BORDER_COLOR, PORTAL_TOKEN_FRAME_OPACITY,
    PORTAL_TOKEN_HEADER_FONT_SIZE, PORTAL_TOKEN_HEADER_TEXT_COLOR,
    PORTAL_TOKEN_LIFECYCLE_ACTIVE_COLOR, PORTAL_TOKEN_LIFECYCLE_ATTACHED_COLOR,
    PORTAL_TOKEN_LIFECYCLE_ATTENTION_COLOR, PORTAL_TOKEN_LIFECYCLE_INACTIVE_COLOR,
    PORTAL_TOKEN_SCROLL_INDICATOR_COLOR, PORTAL_TOKEN_SCROLL_INDICATOR_MIN_HEIGHT_PX,
    PORTAL_TOKEN_SCROLL_INDICATOR_WIDTH_PX, PORTAL_TOKEN_SPACING_CONTENT_INSET_PX,
    PORTAL_TOKEN_SPACING_HEADER_HEIGHT_PX, PORTAL_TOKEN_SPACING_SECTION_GAP_PX,
    PORTAL_TOKEN_TIMESTAMP_COLOR, PORTAL_TOKEN_TIMESTAMP_GRANULARITY,
    PORTAL_TOKEN_TRANSCRIPT_BACKGROUND, PORTAL_TOKEN_TRANSCRIPT_FONT_SIZE,
    PORTAL_TOKEN_TRANSCRIPT_MAX_MEASURE_PX, PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR,
    PORTAL_TOKEN_TRANSITION_IN_MS, PORTAL_TOKEN_TRANSITION_OUT_MS,
    PORTAL_TOKEN_WINDOW_MIN_HEIGHT_PX, PORTAL_TOKEN_WINDOW_MIN_WIDTH_PX,
    PORTAL_TOKEN_WINDOW_RESIZE_AFFORDANCE_PX, PORTAL_TOKEN_WINDOW_RESIZE_GRIP_COLOR,
    PORTAL_TOKEN_WINDOW_RESIZE_GRIP_HOVER_COLOR, PORTAL_TOKEN_WINDOW_RESIZE_GRIP_SIZE_PX,
    PORTAL_TOKEN_WINDOW_RESIZE_STEP_PX, PortalPartTokens, TimestampGranularity,
    resolve_portal_token_strings, resolve_portal_tokens,
};
pub use profile::{
    AutoDetectResult, HeadlessSignal, auto_detect_profile, resolve_headless_dimensions,
    resolve_profile, validate_display_profile,
};
pub use reload::{
    FROZEN_SECTIONS, FieldClassification, HotReloadableConfig, SighupHandler,
    check_frozen_section_changes, reload_config, section_classification,
};
pub use resolver::resolve_config_path;
pub use runtime_widget_assets::{
    DEFAULT_MAX_AGENT_BYTES as DEFAULT_WIDGET_RUNTIME_MAX_AGENT_BYTES,
    DEFAULT_MAX_TOTAL_BYTES as DEFAULT_WIDGET_RUNTIME_MAX_TOTAL_BYTES,
    RuntimeWidgetAssetStoreConfig, resolve_runtime_widget_asset_store, resolve_store_path,
};
pub use schema::print_schema;
pub use widgets::{
    LoadedWidgetType, build_widget_instance, validate_widget_bundles, validate_widget_instances,
};
pub use zones::{BUILTIN_ZONE_TYPES, is_known_zone_type, validate_zone_type_ref, validate_zones};
