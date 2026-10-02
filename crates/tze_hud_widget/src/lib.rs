//! # tze_hud_widget
//!
//! Widget asset bundle loader and IMAGE_SVG resource support for tze_hud.
//!
//! ## Responsibilities
//!
//! This crate implements the widget asset bundle system:
//!
//! - **Bundle directory scanner**: scans configured directories for subdirectories
//!   containing `widget.toml` manifests.
//! - **Manifest parser**: deserializes `widget.toml` into `WidgetDefinition` scene
//!   types, validating all required fields, parameter schema, layer references, and
//!   parameter bindings.
//! - **SVG file loading and validation**: each referenced SVG file is read,
//!   validated as well-formed XML with an `<svg>` root element, and its element IDs
//!   are collected for binding resolution.
//! - **Parameter binding resolution**: validates that each binding references an
//!   existing parameter name, an existing SVG element ID, and a compatible mapping
//!   type for the parameter type.
//! - **Structured error codes**: each failure produces a named `BundleError` with
//!   a wire code string matching the spec.
//!
//! ## Relationship to tze_hud_resource
//!
//! The `tze_hud_resource` crate provides `ResourceType::ImageSvg` and the upload
//! validation logic (`parse_svg_dimensions`).  This crate uses those for SVG
//! validation at bundle load time, but does NOT upload SVG files to the resource
//! store — that is the responsibility of the runtime startup code.
//!
//! ## Spec references
//!
//! - widget-system/spec.md §Requirement: Widget Asset Bundle Format
//! - widget-system/spec.md §Requirement: SVG Layer Parameter Bindings
//! - resource-store/spec.md §Requirement: V1 Resource Type Enumeration (IMAGE_SVG)
//! - resource-store/spec.md §Requirement: SVG Resource Budget Accounting
//!
//! ## Module structure
//!
//! | Module | Contents |
//! |---|---|
//! | [`error`] | Structured bundle error codes |
//! | [`manifest`] | TOML manifest deserialization types |
//! | [`svg_ids`] | SVG element ID scanner for binding resolution |
//! | [`loader`] | Bundle directory scanner and WidgetDefinition builder |

pub mod error;
pub mod loader;
pub mod manifest;
pub mod runtime_registration;
pub mod svg_ids;

pub use error::BundleError;
pub use loader::{
    BundleScanResult, LoadedBundle, load_bundle_dir, load_bundle_dir_with_tokens, scan_bundle_dirs,
};
pub use runtime_registration::{RuntimeWidgetAssetError, register_runtime_widget_svg_asset};
