//! # tze_hud_config
//!
//! TOML configuration loading and validation for tze_hud.
//!
//! This crate provides `TzeHudConfig`, which parses, validates and freezes a
//! configuration into `tze_hud_scene::config::ResolvedConfig`.
//!
//! Covers TOML parsing and validation, file resolution, the built-in display
//! profiles (`full-display`, `headless`), display zone assignments, widget
//! bundles, design tokens, and paired agents (`agents.toml`).

pub mod agents_file;
pub mod allow;
pub mod config_write;
pub mod displays;
pub mod loader;
pub mod policy_builder;
pub mod portal_tokens;
pub mod raw;
mod resolver;
pub mod runtime_widget_assets;
#[cfg(test)]
mod tests;
pub mod themes;
pub mod tokens;
mod validate;
pub mod widgets;

pub use agents_file::{
    AgentChange, AgentRecord, AgentsFile, AgentsFileError, Edited, agents_path_for,
};
pub use allow::Grant;
pub use config_write::{
    ConfigWriteError, RuntimeUpdate, TokenUpdate, WidgetUpdate, check_token_update,
    set_design_tokens, set_runtime, set_widget, set_zone_display,
};
pub use loader::{
    DEFAULT_FPS, DEFAULT_GRPC_PORT, DEFAULT_LOG_LEVEL, DEFAULT_MCP_PORT, LOG_LEVELS, TzeHudConfig,
    runtime_section,
};
pub use policy_builder::{apply_token_defaults_for_zone, build_effective_policy};
pub use portal_tokens::{
    PORTAL_TOKEN_COLLAPSED_FONT_SIZE, PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR, PortalPartTokens,
    TimestampGranularity, resolve_portal_tokens,
};
pub use resolver::resolve_config_path;
pub use runtime_widget_assets::{
    RuntimeWidgetAssetStoreConfig, resolve_runtime_widget_asset_store,
};
pub use tze_hud_scene::{DEFAULT_ANSWER_WINDOW_MS, MAX_ANSWER_WINDOW_MS};
pub use validate::validate_config;
pub use widgets::{LoadedWidgetType, build_widget_instance};
