//! TOML manifest deserialization for widget asset bundles.
//!
//! The `widget.toml` manifest format is defined in
//! widget-system/spec.md §Requirement: Widget Asset Bundle Format.
//!
//! # widget.toml Example
//!
//! ```toml
//! name = "gauge"
//! version = "1.0.0"
//! description = "Vertical fill gauge"
//!
//! [[parameter_schema]]
//! name = "level"
//! type = "f32"
//! default = 0.0
//!
//! [parameter_schema.constraints]
//! f32_min = 0.0
//! f32_max = 1.0
//!
//! [[parameter_schema]]
//! name = "label"
//! type = "string"
//! default = ""
//!
//! [[parameter_schema]]
//! name = "fill_color"
//! type = "color"
//! default = [0, 180, 255, 255]
//!
//! [[layers]]
//! svg_file = "background.svg"
//!
//! [[layers]]
//! svg_file = "fill.svg"
//!
//! [[layers.bindings]]
//! param = "level"
//! target_element = "bar"
//! target_attribute = "height"
//! mapping = "linear"
//! attr_min = 0.0
//! attr_max = 200.0
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ─── Raw manifest types ────────────────────────────────────────────────────────

/// The top-level `widget.toml` document.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawManifest {
    /// Kebab-case unique name, e.g. "gauge". Required.
    pub name: Option<String>,
    /// Semver version string, e.g. "1.0.0". Required.
    pub version: Option<String>,
    /// Human-readable description. Required.
    pub description: Option<String>,
    /// Ordered list of parameter declarations. Optional (empty widget if absent).
    #[serde(default, rename = "parameter_schema")]
    pub parameter_schema: Vec<RawParameterDeclaration>,
    /// Ordered list of SVG layers. Required (at least one layer).
    #[serde(default)]
    pub layers: Vec<RawLayer>,
    /// Optional default contention policy name.
    pub default_contention_policy: Option<String>,
    /// Optional default rendering policy name.
    pub default_rendering_policy: Option<String>,
    /// Optional runtime-managed hover behavior.
    #[serde(default)]
    pub hover_behavior: Option<RawHoverBehavior>,
}

/// A single parameter declaration in `[[parameter_schema]]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawParameterDeclaration {
    /// Parameter name. Required.
    pub name: Option<String>,
    /// Type string: "f32", "string", "color", or "enum". Required.
    #[serde(rename = "type")]
    pub param_type: Option<String>,
    /// Default value. Required. Encoding depends on `param_type`:
    /// - f32: TOML float
    /// - string: TOML string
    /// - color: TOML array of 4 integers [r, g, b, a], or a string holding
    ///   `#RRGGBB[AA]` or a `{{token.key}}` placeholder resolved at load
    /// - enum: TOML string (must be in allowed_values)
    pub default: Option<toml::Value>,
    /// Optional constraints table.
    pub constraints: Option<RawConstraints>,
}

/// Constraints for a parameter declaration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawConstraints {
    /// Minimum f32 value.
    pub f32_min: Option<f64>,
    /// Maximum f32 value.
    pub f32_max: Option<f64>,
    /// Max UTF-8 byte length for string parameters.
    pub string_max_bytes: Option<u32>,
    /// Allowed values for enum parameters.
    #[serde(default)]
    pub enum_allowed_values: Vec<String>,
}

/// A single entry in `[[layers]]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawLayer {
    /// Filename within the bundle directory, e.g. "fill.svg". Required.
    pub svg_file: Option<String>,
    /// Ordered list of parameter bindings for this layer.
    #[serde(default)]
    pub bindings: Vec<RawBinding>,
}

/// A single parameter-to-SVG-attribute binding in `[[layers.bindings]]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawBinding {
    /// Parameter name from the parameter schema. Required.
    pub param: Option<String>,
    /// SVG element ID in the layer's SVG file. Required.
    pub target_element: Option<String>,
    /// SVG attribute name, or the synthetic target `"text-content"`. Required.
    pub target_attribute: Option<String>,
    /// Mapping type string: "linear", "direct", or "discrete". Required.
    pub mapping: Option<String>,
    /// For "linear" mapping: minimum output attribute value.
    pub attr_min: Option<f64>,
    /// For "linear" mapping: maximum output attribute value.
    pub attr_max: Option<f64>,
    /// For "discrete" mapping: enum_value → attribute_value lookup table.
    /// Values may hold `{{token.key}}` placeholders, resolved at load.
    #[serde(default)]
    pub value_map: BTreeMap<String, String>,
}

/// Runtime-managed hover behavior declaration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawHoverBehavior {
    /// Widget-local trigger rectangle in normalized coordinates.
    pub trigger_rect: Option<RawNormalizedRect>,
    /// Delay before visibility param is switched to `visible_value`.
    pub delay_ms: Option<u32>,
    /// Parameter name to drive on hover transitions.
    pub visibility_param: Option<String>,
    /// Value written when hover is inactive.
    pub hidden_value: Option<f32>,
    /// Value written when hover dwell is satisfied.
    pub visible_value: Option<f32>,
}

/// Normalized rectangle in widget-local coordinates.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawNormalizedRect {
    pub x_pct: Option<f32>,
    pub y_pct: Option<f32>,
    pub width_pct: Option<f32>,
    pub height_pct: Option<f32>,
}
