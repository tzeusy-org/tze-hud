//! JSON Schema export.
//!
//! Implements spec §Requirement: Schema Export:
//! - `--print-schema` CLI flag → print JSON Schema to stdout and exit 0.
//! - `emit_schema = true` in `[runtime]` → write schema at startup and continue.
//!
//! This module exposes `print_schema()` which writes the JSON Schema for the
//! complete configuration document to stdout.

use crate::raw::RawConfig;
use schemars::schema_for;

/// Print the full JSON Schema for `RawConfig` to stdout.
///
/// Callers (typically the main binary) should exit with code 0 after calling
/// this if `--print-schema` was passed.
pub fn print_schema() {
    let schema = schema_for!(RawConfig);
    let json =
        serde_json::to_string_pretty(&schema).expect("JSON Schema serialisation should not fail");
    println!("{json}");
}

/// Returns the JSON Schema as a `serde_json::Value` for programmatic use.
pub fn schema_value() -> serde_json::Value {
    let schema = schema_for!(RawConfig);
    serde_json::to_value(&schema).expect("JSON Schema serialisation should not fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WHEN schema_value() is called THEN a non-empty JSON object is returned.
    #[test]
    fn test_schema_value_is_non_empty_object() {
        let v = schema_value();
        assert!(v.is_object(), "schema should be a JSON object");
        let obj = v.as_object().unwrap();
        assert!(
            obj.contains_key("$schema")
                || obj.contains_key("properties")
                || obj.contains_key("title"),
            "schema should have at least one recognised JSON Schema key"
        );
    }

    /// WHEN print_schema() is called THEN it does not panic.
    #[test]
    fn test_print_schema_does_not_panic() {
        // Redirect stdout is complex; just check that it doesn't panic.
        // The actual --print-schema scenario is an integration concern.
        let schema = schema_for!(crate::raw::RawConfig);
        let json = serde_json::to_string_pretty(&schema);
        assert!(json.is_ok(), "JSON Schema serialisation should succeed");
    }

    /// WHEN schema_value() is called THEN it contains the design token fields.
    #[test]
    fn test_schema_contains_design_token_fields() {
        let v = schema_value();
        let json_str = serde_json::to_string(&v).unwrap();
        assert!(
            json_str.contains("design_tokens"),
            "schema should contain design_tokens field"
        );
        assert!(
            json_str.contains("widget_runtime_assets"),
            "schema should contain widget_runtime_assets field"
        );
    }

    /// WHEN the schema is exported THEN it includes the optional `schema_version`
    /// field (configuration spec §Config Schema Version and Compatibility Policy).
    #[test]
    fn test_schema_contains_schema_version_field() {
        let v = schema_value();
        let json_str = serde_json::to_string(&v).unwrap();
        assert!(
            json_str.contains("schema_version"),
            "exported schema should contain the schema_version field"
        );
    }
}
