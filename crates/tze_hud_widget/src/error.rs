//! Structured error codes for the widget asset bundle system.
//!
//! Each variant maps 1-to-1 with a spec-defined error code from
//! widget-system/spec.md §Requirement: Widget Asset Bundle Format and
//! §Requirement: SVG Layer Parameter Bindings.

use thiserror::Error;

/// Errors produced by the widget asset bundle loader.
///
/// Source: widget-system/spec.md §Requirement: Widget Asset Bundle Format,
///         §Requirement: SVG Layer Parameter Bindings.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BundleError {
    /// The bundle directory does not contain a `widget.toml` manifest.
    ///
    /// Wire code: `WIDGET_BUNDLE_NO_MANIFEST`
    #[error("WIDGET_BUNDLE_NO_MANIFEST: {path}: no widget.toml found")]
    NoManifest { path: String },

    /// The `widget.toml` file cannot be parsed or is missing required fields.
    ///
    /// Wire code: `WIDGET_BUNDLE_INVALID_MANIFEST`
    #[error("WIDGET_BUNDLE_INVALID_MANIFEST: {path}: {detail}")]
    InvalidManifest { path: String, detail: String },

    /// The widget type name in `widget.toml` does not conform to the required
    /// format `[a-z][a-z0-9-]*`.
    ///
    /// Wire code: `WIDGET_BUNDLE_INVALID_NAME`
    #[error(
        "WIDGET_BUNDLE_INVALID_NAME: {path}: widget type name '{name}' is invalid (must match [a-z][a-z0-9-]*)"
    )]
    InvalidName { path: String, name: String },

    /// A widget type name is declared in two different loaded bundles.
    ///
    /// Wire code: `WIDGET_BUNDLE_DUPLICATE_TYPE`
    #[error(
        "WIDGET_BUNDLE_DUPLICATE_TYPE: widget type '{name}' already registered (from {existing_path}), rejected from {new_path}"
    )]
    DuplicateType {
        name: String,
        existing_path: String,
        new_path: String,
    },

    /// A layer's SVG file is referenced in the manifest but not present in the bundle directory.
    ///
    /// Wire code: `WIDGET_BUNDLE_MISSING_SVG`
    #[error("WIDGET_BUNDLE_MISSING_SVG: {path}: SVG file '{svg_file}' not found in bundle")]
    MissingSvg { path: String, svg_file: String },

    /// A layer's SVG file exists but fails to parse as valid SVG.
    ///
    /// Wire code: `WIDGET_BUNDLE_SVG_PARSE_ERROR`
    #[error("WIDGET_BUNDLE_SVG_PARSE_ERROR: {path}: SVG file '{svg_file}': {detail}")]
    SvgParseError {
        path: String,
        svg_file: String,
        detail: String,
    },

    /// A parameter binding references a nonexistent parameter name, a nonexistent
    /// SVG element ID, or uses an incompatible mapping type for the parameter type.
    ///
    /// Wire code: `WIDGET_BINDING_UNRESOLVABLE`
    #[error("WIDGET_BINDING_UNRESOLVABLE: {path}: {detail}")]
    BindingUnresolvable { path: String, detail: String },

    /// An SVG file contains a `{{token.key}}` placeholder that was not present in
    /// the supplied token map.
    ///
    /// Wire code: `WIDGET_BUNDLE_UNRESOLVED_TOKEN`
    #[error(
        "WIDGET_BUNDLE_UNRESOLVED_TOKEN: {path}: SVG file '{svg_file}': unresolved token '{{{{token.{token_key}}}}}'"
    )]
    UnresolvedToken {
        path: String,
        svg_file: String,
        token_key: String,
    },
}

impl BundleError {
    /// Stable wire code string (used in protobuf / log `error_code` fields).
    pub fn wire_code(&self) -> &'static str {
        match self {
            BundleError::NoManifest { .. } => "WIDGET_BUNDLE_NO_MANIFEST",
            BundleError::InvalidManifest { .. } => "WIDGET_BUNDLE_INVALID_MANIFEST",
            BundleError::InvalidName { .. } => "WIDGET_BUNDLE_INVALID_NAME",
            BundleError::DuplicateType { .. } => "WIDGET_BUNDLE_DUPLICATE_TYPE",
            BundleError::MissingSvg { .. } => "WIDGET_BUNDLE_MISSING_SVG",
            BundleError::SvgParseError { .. } => "WIDGET_BUNDLE_SVG_PARSE_ERROR",
            BundleError::BindingUnresolvable { .. } => "WIDGET_BINDING_UNRESOLVABLE",
            BundleError::UnresolvedToken { .. } => "WIDGET_BUNDLE_UNRESOLVED_TOKEN",
        }
    }
}
