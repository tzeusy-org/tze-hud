//! Widget bundle and instance configuration validation.
//!
//! Implements the widget-system configuration requirements from
//! `configuration/spec.md §Requirement: Widget Bundle Configuration` and
//! `configuration/spec.md §Requirement: Widget Instance Configuration`.
//!
//! ## Error codes produced
//!
//! | Error code | Condition |
//! |---|---|
//! | `CONFIG_WIDGET_BUNDLE_PATH_NOT_FOUND` | `[widget_bundles].paths` entry does not exist |
//! | `CONFIG_WIDGET_BUNDLE_DUPLICATE_TYPE` | Two bundle dirs declare the same widget type name |
//! | `CONFIG_UNKNOWN_WIDGET_TYPE` | `[[tabs.widgets]]` references an unloaded widget type |
//! | `CONFIG_WIDGET_INVALID_INITIAL_PARAMS` | `initial_params` fail the widget type's parameter schema |
//!
//! ## Bundle path resolution
//!
//! All paths in `[widget_bundles].paths` are resolved relative to the
//! configuration file's parent directory when `config_parent` is `Some`.
//! When `config_parent` is `None` (e.g. config loaded from an inlined string),
//! absolute paths are still validated for existence; relative paths are
//! **skipped** because there is no meaningful base to resolve them against
//! (the runtime's `scan_bundle_dirs` handles absent directories gracefully).
//!
//! ## Spec references
//!
//! - configuration/spec.md §Widget Bundle Configuration
//! - configuration/spec.md §Widget Instance Configuration
//! - configuration/spec.md §Capability Vocabulary (publish_widget:)

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tze_hud_scene::config::{ConfigError, ConfigErrorCode};
use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, WidgetInstance, WidgetParamType, WidgetParameterValue,
};

use crate::raw::{AnyValue, RawConfig, RawTabWidget, RawWidgetGeometry};

// ─── Public API ────────────────────────────────────────────────────────────────

/// Information about a validated widget type (loaded from a bundle scan result).
///
/// Used during config validation to check:
/// - `[[tabs.widgets]]` type references
/// - `initial_params` schema conformance
#[derive(Debug, Clone)]
pub struct LoadedWidgetType {
    pub name: String,
    pub parameter_schema: Vec<tze_hud_scene::types::WidgetParameterDeclaration>,
    pub default_geometry_policy: GeometryPolicy,
    pub default_contention_policy: ContentionPolicy,
}

/// Validate `[widget_bundles]` bundle path existence and scan for loaded types.
///
/// Produces:
/// - `CONFIG_WIDGET_BUNDLE_PATH_NOT_FOUND` when a configured path doesn't exist.
/// - `CONFIG_WIDGET_BUNDLE_DUPLICATE_TYPE` when two bundles declare the same name.
///
/// Returns the set of loaded widget type names (from valid bundle dirs that were
/// already scanned by `tze_hud_widget::scan_bundle_dirs`).
///
/// # Arguments
///
/// - `raw`: The raw config document.
/// - `config_parent`: Parent directory of the config file (for relative path resolution).
///   When `Some`, relative paths in `[widget_bundles].paths` are resolved against this
///   directory and checked for existence. When `None` (e.g. config inlined via
///   `include_str!`), absolute paths are still validated but relative paths are skipped
///   because no meaningful base is available to resolve them against.
/// - `loaded_types`: Widget types already loaded by the bundle scanner (from the
///   `tze_hud_widget` crate). These are the types available for `[[tabs.widgets]]`
///   references. This is passed in to allow validation without re-scanning bundles
///   (separation of concerns: the runtime does the actual scan; config only validates).
/// - `errors`: Mutable error accumulator.
///
/// Returns the set of loaded widget type names for downstream reference validation.
pub fn validate_widget_bundles(
    raw: &RawConfig,
    config_parent: Option<&Path>,
    loaded_types: &[LoadedWidgetType],
    errors: &mut Vec<ConfigError>,
) -> HashSet<String> {
    // Build the known-types set from the already-loaded bundles.
    let known: HashSet<String> = loaded_types.iter().map(|t| t.name.clone()).collect();

    let Some(wb) = &raw.widget_bundles else {
        // Absent section: empty registry, not an error.
        return known;
    };

    // When config_parent is None (e.g. config loaded from an inlined string,
    // not a file), we cannot reliably resolve relative paths — the CWD at
    // validation time may be unrelated to where the config file would live.
    // In that case, skip the path-existence check for relative paths: the
    // runtime's scan_bundle_dirs already handles missing directories gracefully
    // (logs a warning and continues), so no hard error is needed here.
    let base = config_parent.unwrap_or_else(|| Path::new("."));

    for path_str in &wb.paths {
        // Skip existence check for relative paths when no config_parent is known.
        let p = Path::new(path_str);
        if config_parent.is_none() && !p.is_absolute() {
            continue;
        }

        let resolved = resolve_bundle_path(path_str, base);
        if !resolved.exists() {
            errors.push(ConfigError {
                code: ConfigErrorCode::WidgetBundlePathNotFound,
                field_path: "widget_bundles.paths".into(),
                expected: format!("an existing directory at {:?}", resolved.display()),
                got: format!("{path_str:?}"),
                hint: format!(
                    "widget bundle path {:?} (resolved to {:?}) does not exist; \
                     create the directory or remove the path from [widget_bundles].paths",
                    path_str,
                    resolved.display()
                ),
            });
        }
    }

    known
}

/// Validate per-tab `[[tabs.widgets]]` entries.
///
/// Produces:
/// - `CONFIG_UNKNOWN_WIDGET_TYPE` when a widget type is not in `known_types`.
/// - `CONFIG_WIDGET_INVALID_INITIAL_PARAMS` when `initial_params` fail the schema.
///
/// # Arguments
///
/// - `raw`: The raw config document.
/// - `known_types`: The set of widget type names loaded from bundles.
/// - `type_map`: A map from widget type name to its parameter schema (for `initial_params`
///   validation). May be empty if no bundles were loaded.
/// - `errors`: Mutable error accumulator.
pub fn validate_widget_instances(
    raw: &RawConfig,
    known_types: &HashSet<String>,
    type_map: &HashMap<String, LoadedWidgetType>,
    errors: &mut Vec<ConfigError>,
) {
    for (tab_idx, tab) in raw.tabs.iter().enumerate() {
        let tab_name = tab.name.as_deref().unwrap_or("<unnamed>");

        // Track instance names per tab to detect duplicate instance_name
        // (same type × no instance_id, or same instance_id used twice).
        let mut seen_instance_names: HashSet<String> = HashSet::new();

        for (widget_idx, raw_widget) in tab.widgets.iter().enumerate() {
            let field_prefix = format!("tabs[{tab_idx}].widgets[{widget_idx}]");

            // ── 1. widget_type must be present and non-empty ─────────────────
            let widget_type = match raw_widget.widget_type.as_deref().filter(|s| !s.is_empty()) {
                Some(t) => t,
                None => {
                    errors.push(ConfigError {
                        code: ConfigErrorCode::UnknownWidgetType,
                        field_path: format!("{field_prefix}.widget_type"),
                        expected: "non-empty widget type name".into(),
                        got: "missing or empty".into(),
                        hint: format!(
                            "tab {tab_name:?} widget [{widget_idx}] is missing 'widget_type'; \
                             specify the name of a widget type loaded from [widget_bundles].paths"
                        ),
                    });
                    continue;
                }
            };

            // ── 2. widget_type must reference a loaded bundle ────────────────
            if !known_types.is_empty() && !known_types.contains(widget_type) {
                errors.push(ConfigError {
                    code: ConfigErrorCode::UnknownWidgetType,
                    field_path: format!("{field_prefix}.widget_type"),
                    expected: format!(
                        "a widget type loaded from [widget_bundles].paths (loaded: {})",
                        sorted_names(known_types)
                    ),
                    got: format!("{widget_type:?}"),
                    hint: format!(
                        "tab {tab_name:?}: unknown widget type {widget_type:?}; \
                         add a bundle directory containing this type to [widget_bundles].paths \
                         or correct the type name"
                    ),
                });
                continue;
            }
            // If known_types is empty (no bundles configured), we still
            // validate shape but cannot reject unknown type names — the runtime
            // will reject them at startup when actually loading bundles.

            // ── 3. Compute instance_name; check for duplicates ───────────────
            let instance_name = raw_widget
                .instance_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(widget_type)
                .to_string();

            if seen_instance_names.contains(&instance_name) {
                errors.push(ConfigError {
                    code: ConfigErrorCode::Other("CONFIG_DUPLICATE_WIDGET_INSTANCE_NAME".into()),
                    field_path: format!("{field_prefix}.instance_id"),
                    expected: "unique instance name within the tab".into(),
                    got: format!("{instance_name:?}"),
                    hint: format!(
                        "tab {tab_name:?}: duplicate widget instance name {instance_name:?}; \
                         add a distinct 'instance_id' to each [[tabs.widgets]] entry with the \
                         same widget type"
                    ),
                });
                continue;
            }
            seen_instance_names.insert(instance_name.clone());

            // ── 4. Validate initial_params against schema ─────────────────────
            if !raw_widget.initial_params.is_empty() {
                if let Some(loaded) = type_map.get(widget_type) {
                    validate_initial_params(
                        &raw_widget.initial_params,
                        &loaded.parameter_schema,
                        &field_prefix,
                        widget_type,
                        tab_name,
                        errors,
                    );
                }
                // If no type_map entry, we skip param validation (bundles not yet loaded).
            }
        }
    }
}

/// Build a `WidgetInstance` from a validated `RawTabWidget` entry.
///
/// Called at runtime startup after bundles are loaded and config validation
/// passes. `tab_id` is the scene ID assigned to the owning tab.
pub fn build_widget_instance(
    raw_widget: &RawTabWidget,
    tab_id: tze_hud_scene::types::SceneId,
    type_map: &HashMap<String, LoadedWidgetType>,
) -> Option<WidgetInstance> {
    let widget_type = raw_widget.widget_type.as_deref()?.to_string();
    let loaded = type_map.get(&widget_type)?;

    let instance_name = raw_widget
        .instance_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&widget_type)
        .to_string();

    // Resolve geometry override.
    let geometry_override = raw_widget
        .geometry
        .as_ref()
        .and_then(parse_geometry_override);

    // Resolve contention override.
    let contention_override = raw_widget
        .contention
        .as_deref()
        .and_then(parse_contention_policy_str);

    // Build default current_params from schema defaults.
    let current_params: HashMap<String, WidgetParameterValue> = loaded
        .parameter_schema
        .iter()
        .map(|p| (p.name.clone(), p.default_value.clone()))
        .collect();

    // Apply initial_params overrides (already validated).
    let mut current_params = current_params;
    for (param_name, any_val) in &raw_widget.initial_params {
        if let Some(decl) = loaded
            .parameter_schema
            .iter()
            .find(|p| p.name == *param_name)
        {
            if let Some(value) = coerce_toml_to_widget_value(&any_val.0, decl.param_type) {
                current_params.insert(param_name.clone(), value);
            }
        }
    }

    Some(WidgetInstance {
        id: tze_hud_scene::types::SceneId::new(),
        widget_type_name: widget_type,
        tab_id,
        geometry_override,
        contention_override,
        instance_name,
        current_params,
        auto_clear_ms: raw_widget.auto_clear_ms.filter(|ms| *ms > 0),
    })
}

// ─── Private helpers ───────────────────────────────────────────────────────────

/// Resolve a widget bundle path string to an absolute `PathBuf`.
///
/// Absolute paths are returned unchanged. Relative paths are joined to
/// `config_parent` (typically the parent directory of the config file).
///
/// Exposed as `pub` so that `tze_hud_runtime::widget_startup` can reuse this
/// logic for step-1 path resolution without duplicating the implementation.
pub fn resolve_bundle_path(path_str: &str, config_parent: &Path) -> PathBuf {
    let p = Path::new(path_str);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        config_parent.join(p)
    }
}

fn sorted_names(set: &HashSet<String>) -> String {
    let mut v: Vec<&str> = set.iter().map(|s| s.as_str()).collect();
    v.sort_unstable();
    v.join(", ")
}

fn validate_initial_params(
    params: &HashMap<String, AnyValue>,
    schema: &[tze_hud_scene::types::WidgetParameterDeclaration],
    field_prefix: &str,
    widget_type: &str,
    tab_name: &str,
    errors: &mut Vec<ConfigError>,
) {
    let schema_map: HashMap<&str, &tze_hud_scene::types::WidgetParameterDeclaration> =
        schema.iter().map(|p| (p.name.as_str(), p)).collect();

    for (param_name, any_val) in params {
        let raw_val = &any_val.0;
        let field_path = format!("{field_prefix}.initial_params.{param_name}");

        // Unknown parameter.
        let decl = match schema_map.get(param_name.as_str()) {
            Some(d) => d,
            None => {
                errors.push(ConfigError {
                    code: ConfigErrorCode::WidgetInvalidInitialParams,
                    field_path,
                    expected: format!(
                        "a parameter defined in widget type {widget_type:?} schema ({})",
                        schema
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    got: format!("unknown parameter {param_name:?}"),
                    hint: format!(
                        "tab {tab_name:?}, widget {widget_type:?}: \
                         parameter {param_name:?} is not declared in the widget's schema"
                    ),
                });
                continue;
            }
        };

        // Type check.
        if coerce_toml_to_widget_value(raw_val, decl.param_type).is_none() {
            let type_name = match decl.param_type {
                WidgetParamType::F32 => "f32 (numeric)",
                WidgetParamType::String => "string",
                WidgetParamType::Color => "color (array of 4 integers [r, g, b, a])",
                WidgetParamType::Enum => "enum (string)",
            };
            errors.push(ConfigError {
                code: ConfigErrorCode::WidgetInvalidInitialParams,
                field_path,
                expected: format!("a value of type {type_name} for parameter {param_name:?}"),
                got: format!("{raw_val:?}"),
                hint: format!(
                    "tab {tab_name:?}, widget {widget_type:?}: \
                     parameter {param_name:?} expects type {type_name}; \
                     the provided initial_params value has the wrong type"
                ),
            });
            continue;
        }

        // Enum: check allowed_values if constraints are present.
        if decl.param_type == WidgetParamType::Enum {
            if let Some(constraints) = &decl.constraints {
                if !constraints.enum_allowed_values.is_empty() {
                    if let Some(s) = raw_val.as_str() {
                        if !constraints.enum_allowed_values.iter().any(|a| a == s) {
                            errors.push(ConfigError {
                                code: ConfigErrorCode::WidgetInvalidInitialParams,
                                field_path,
                                expected: format!("one of {:?}", constraints.enum_allowed_values),
                                got: format!("{s:?}"),
                                hint: format!(
                                    "tab {tab_name:?}, widget {widget_type:?}: \
                                     parameter {param_name:?} enum value {s:?} is not in \
                                     allowed_values {:?}",
                                    constraints.enum_allowed_values
                                ),
                            });
                        }
                    }
                }
            }
        }
    }
}

fn coerce_toml_to_widget_value(
    val: &toml::Value,
    param_type: WidgetParamType,
) -> Option<WidgetParameterValue> {
    match param_type {
        WidgetParamType::F32 => {
            let f = match val {
                toml::Value::Float(f) => *f as f32,
                toml::Value::Integer(i) => *i as f32,
                _ => return None,
            };
            // NaN/Inf are invalid.
            if !f.is_finite() {
                return None;
            }
            Some(WidgetParameterValue::F32(f))
        }
        WidgetParamType::String => {
            let s = val.as_str()?;
            Some(WidgetParameterValue::String(s.to_string()))
        }
        WidgetParamType::Color => {
            let arr = val.as_array()?;
            if arr.len() != 4 {
                return None;
            }
            let mut components = [0u8; 4];
            for (i, v) in arr.iter().enumerate() {
                let int = v.as_integer()?;
                if !(0..=255).contains(&int) {
                    return None;
                }
                components[i] = int as u8;
            }
            use tze_hud_scene::types::Rgba;
            Some(WidgetParameterValue::Color(Rgba::new(
                components[0] as f32 / 255.0,
                components[1] as f32 / 255.0,
                components[2] as f32 / 255.0,
                components[3] as f32 / 255.0,
            )))
        }
        WidgetParamType::Enum => {
            let s = val.as_str()?;
            Some(WidgetParameterValue::Enum(s.to_string()))
        }
    }
}

fn parse_geometry_override(raw: &RawWidgetGeometry) -> Option<GeometryPolicy> {
    // If relative fractions are set, use Relative.
    if let (Some(x_pct), Some(y_pct), Some(w_pct), Some(h_pct)) =
        (raw.x_pct, raw.y_pct, raw.width_pct, raw.height_pct)
    {
        return Some(GeometryPolicy::Relative {
            x_pct,
            y_pct,
            width_pct: w_pct,
            height_pct: h_pct,
        });
    }
    // If absolute position fields are set, map to Relative by normalizing
    // against a 1920×1080 reference resolution (best available without a
    // live display reference at config-parse time).
    // Note: GeometryPolicy only supports Relative and EdgeAnchored in v1.
    // Absolute pixel geometry is expressed as Relative fractions here.
    if let (Some(x), Some(y), Some(w), Some(h)) = (raw.x, raw.y, raw.width, raw.height) {
        // Normalize against full-display reference (1920×1080).
        const REF_W: f32 = 1920.0;
        const REF_H: f32 = 1080.0;
        return Some(GeometryPolicy::Relative {
            x_pct: x / REF_W,
            y_pct: y / REF_H,
            width_pct: w / REF_W,
            height_pct: h / REF_H,
        });
    }
    None
}

fn parse_contention_policy_str(s: &str) -> Option<ContentionPolicy> {
    match s {
        "LatestWins" | "latest_wins" => Some(ContentionPolicy::LatestWins),
        "Stack" | "stack" => Some(ContentionPolicy::Stack { max_depth: 8 }),
        "Replace" | "replace" => Some(ContentionPolicy::Replace),
        "MergeByKey" | "merge_by_key" => Some(ContentionPolicy::MergeByKey { max_keys: 16 }),
        _ => None,
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{AnyValue, RawConfig, RawTab, RawTabWidget};
    use tze_hud_scene::config::ConfigErrorCode;
    use tze_hud_scene::types::{
        GeometryPolicy, WidgetParamConstraints, WidgetParamType, WidgetParameterDeclaration,
        WidgetParameterValue,
    };

    fn make_decl(name: &str, ptype: WidgetParamType) -> WidgetParameterDeclaration {
        let default_value = match ptype {
            WidgetParamType::F32 => WidgetParameterValue::F32(0.0),
            WidgetParamType::String => WidgetParameterValue::String(String::new()),
            WidgetParamType::Color => {
                WidgetParameterValue::Color(tze_hud_scene::types::Rgba::new(0.0, 0.0, 0.0, 1.0))
            }
            WidgetParamType::Enum => WidgetParameterValue::Enum("info".to_string()),
        };
        WidgetParameterDeclaration {
            name: name.to_string(),
            param_type: ptype,
            default_value,
            constraints: None,
        }
    }

    fn make_gauge_type() -> LoadedWidgetType {
        LoadedWidgetType {
            name: "gauge".to_string(),
            parameter_schema: vec![
                make_decl("level", WidgetParamType::F32),
                make_decl("label", WidgetParamType::String),
            ],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 1.0,
                height_pct: 1.0,
            },
            default_contention_policy: ContentionPolicy::LatestWins,
        }
    }

    fn make_config_with_tab_widgets(widgets: Vec<RawTabWidget>) -> RawConfig {
        let mut raw = RawConfig::default();
        raw.tabs.push(RawTab {
            name: Some("Main".into()),
            widgets,
            ..Default::default()
        });
        raw
    }

    fn type_map_from(types: &[LoadedWidgetType]) -> HashMap<String, LoadedWidgetType> {
        types.iter().map(|t| (t.name.clone(), t.clone())).collect()
    }

    // ── validate_widget_bundles ───────────────────────────────────────────────

    /// WHEN [widget_bundles] is absent THEN no errors (empty registry is valid).
    #[test]
    fn absent_widget_bundles_is_valid() {
        let raw = RawConfig::default();
        let mut errors = Vec::new();
        let known = validate_widget_bundles(&raw, None, &[], &mut errors);
        assert!(
            errors.is_empty(),
            "absent [widget_bundles] should produce no errors"
        );
        assert!(known.is_empty());
    }

    /// WHEN [widget_bundles].paths contains a non-existent absolute path THEN error.
    #[test]
    fn nonexistent_bundle_path_produces_error() {
        let mut raw = RawConfig::default();
        raw.widget_bundles = Some(crate::raw::RawWidgetBundles {
            paths: vec!["/tmp/tze_hud_nonexistent_widget_dir_99999".into()],
        });
        let mut errors = Vec::new();
        validate_widget_bundles(&raw, None, &[], &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::WidgetBundlePathNotFound)),
            "nonexistent absolute path should produce CONFIG_WIDGET_BUNDLE_PATH_NOT_FOUND, got: {errors:?}"
        );
    }

    /// WHEN [widget_bundles].paths contains a relative path AND config_parent is None
    /// THEN no error is produced (path is unresolvable without a base; runtime handles it).
    ///
    /// Regression test for hud-hecz: production.toml uses `../../../assets/widget_bundles`
    /// which is relative to the config file's parent directory. When config is inlined via
    /// `include_str!` in tests, `config_parent` is `None`, so the path cannot be meaningfully
    /// resolved from CWD. The validator must skip the check rather than reporting a spurious
    /// CONFIG_WIDGET_BUNDLE_PATH_NOT_FOUND error that causes governance to fall back to guest
    /// policy (empty capabilities).
    #[test]
    fn relative_bundle_path_with_no_config_parent_skips_existence_check() {
        let mut raw = RawConfig::default();
        raw.widget_bundles = Some(crate::raw::RawWidgetBundles {
            paths: vec!["../../../assets/widget_bundles".into()],
        });
        let mut errors = Vec::new();
        validate_widget_bundles(&raw, None, &[], &mut errors);
        assert!(
            errors.is_empty(),
            "relative path with no config_parent should not produce errors (unresolvable base), \
             got: {errors:?}"
        );
    }

    /// WHEN [widget_bundles].paths contains a relative path AND config_parent IS provided
    /// THEN the path is resolved against config_parent and a non-existent path produces an error.
    ///
    /// Regression guard: the skip-when-None guard must not suppress errors when a valid base
    /// directory is available.
    #[test]
    fn relative_bundle_path_with_config_parent_is_still_validated() {
        let mut raw = RawConfig::default();
        raw.widget_bundles = Some(crate::raw::RawWidgetBundles {
            paths: vec!["nonexistent_subdir_tze_hud_99999".into()],
        });
        let mut errors = Vec::new();
        // Use a known-good base dir (e.g. "/tmp") so the relative path resolves
        // to /tmp/nonexistent_subdir_tze_hud_99999, which should not exist.
        let base = std::path::Path::new("/tmp");
        validate_widget_bundles(&raw, Some(base), &[], &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::WidgetBundlePathNotFound)),
            "relative path with config_parent provided should still produce \
             CONFIG_WIDGET_BUNDLE_PATH_NOT_FOUND for a non-existent path, got: {errors:?}"
        );
    }

    // ── validate_widget_instances ─────────────────────────────────────────────

    /// WHEN [[tabs.widgets]] type references an unknown type THEN CONFIG_UNKNOWN_WIDGET_TYPE.
    #[test]
    fn unknown_widget_type_produces_error() {
        let raw = make_config_with_tab_widgets(vec![RawTabWidget {
            widget_type: Some("speedometer".into()),
            ..Default::default()
        }]);
        let known: HashSet<String> = ["gauge".to_string()].into();
        let mut errors = Vec::new();
        validate_widget_instances(&raw, &known, &HashMap::new(), &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::UnknownWidgetType)),
            "unknown widget type should produce CONFIG_UNKNOWN_WIDGET_TYPE, got: {errors:?}"
        );
    }

    /// WHEN [[tabs.widgets]] type is known THEN no error.
    #[test]
    fn known_widget_type_accepted() {
        let raw = make_config_with_tab_widgets(vec![RawTabWidget {
            widget_type: Some("gauge".into()),
            ..Default::default()
        }]);
        let known: HashSet<String> = ["gauge".to_string()].into();
        let type_map = type_map_from(&[make_gauge_type()]);
        let mut errors = Vec::new();
        validate_widget_instances(&raw, &known, &type_map, &mut errors);
        assert!(
            errors.is_empty(),
            "known widget type should produce no errors, got: {errors:?}"
        );
    }

    /// WHEN initial_params type mismatch THEN CONFIG_WIDGET_INVALID_INITIAL_PARAMS.
    #[test]
    fn invalid_initial_params_type_mismatch_produces_error() {
        let mut params = HashMap::new();
        params.insert(
            "level".to_string(),
            AnyValue(toml::Value::String("not_a_number".into())),
        );
        let raw = make_config_with_tab_widgets(vec![RawTabWidget {
            widget_type: Some("gauge".into()),
            initial_params: params,
            ..Default::default()
        }]);
        let known: HashSet<String> = ["gauge".to_string()].into();
        let type_map = type_map_from(&[make_gauge_type()]);
        let mut errors = Vec::new();
        validate_widget_instances(&raw, &known, &type_map, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::WidgetInvalidInitialParams)),
            "type mismatch in initial_params should produce CONFIG_WIDGET_INVALID_INITIAL_PARAMS, got: {errors:?}"
        );
    }

    /// WHEN initial_params has unknown parameter name THEN error.
    #[test]
    fn initial_params_unknown_param_name_produces_error() {
        let mut params = HashMap::new();
        params.insert("bogus_param".to_string(), AnyValue(toml::Value::Float(1.0)));
        let raw = make_config_with_tab_widgets(vec![RawTabWidget {
            widget_type: Some("gauge".into()),
            initial_params: params,
            ..Default::default()
        }]);
        let known: HashSet<String> = ["gauge".to_string()].into();
        let type_map = type_map_from(&[make_gauge_type()]);
        let mut errors = Vec::new();
        validate_widget_instances(&raw, &known, &type_map, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::WidgetInvalidInitialParams)),
            "unknown param in initial_params should produce error, got: {errors:?}"
        );
    }

    /// WHEN valid initial_params provided THEN no error.
    #[test]
    fn valid_initial_params_accepted() {
        let mut params = HashMap::new();
        params.insert("level".to_string(), AnyValue(toml::Value::Float(0.75)));
        params.insert(
            "label".to_string(),
            AnyValue(toml::Value::String("CPU".into())),
        );
        let raw = make_config_with_tab_widgets(vec![RawTabWidget {
            widget_type: Some("gauge".into()),
            initial_params: params,
            ..Default::default()
        }]);
        let known: HashSet<String> = ["gauge".to_string()].into();
        let type_map = type_map_from(&[make_gauge_type()]);
        let mut errors = Vec::new();
        validate_widget_instances(&raw, &known, &type_map, &mut errors);
        assert!(
            errors.is_empty(),
            "valid initial_params should produce no errors, got: {errors:?}"
        );
    }

    /// WHEN two widget entries have same type and no instance_id THEN error.
    #[test]
    fn duplicate_instance_name_without_instance_id_produces_error() {
        let raw = make_config_with_tab_widgets(vec![
            RawTabWidget {
                widget_type: Some("gauge".into()),
                ..Default::default()
            },
            RawTabWidget {
                widget_type: Some("gauge".into()),
                ..Default::default()
            },
        ]);
        let known: HashSet<String> = ["gauge".to_string()].into();
        let type_map = type_map_from(&[make_gauge_type()]);
        let mut errors = Vec::new();
        validate_widget_instances(&raw, &known, &type_map, &mut errors);
        // Should produce a duplicate instance name error.
        assert!(
            !errors.is_empty(),
            "duplicate widget type without instance_id should produce error, got: {errors:?}"
        );
    }

    /// WHEN two widget entries have same type with distinct instance_ids THEN no error.
    #[test]
    fn two_instances_with_distinct_ids_accepted() {
        let raw = make_config_with_tab_widgets(vec![
            RawTabWidget {
                widget_type: Some("gauge".into()),
                instance_id: Some("cpu_gauge".into()),
                ..Default::default()
            },
            RawTabWidget {
                widget_type: Some("gauge".into()),
                instance_id: Some("mem_gauge".into()),
                ..Default::default()
            },
        ]);
        let known: HashSet<String> = ["gauge".to_string()].into();
        let type_map = type_map_from(&[make_gauge_type()]);
        let mut errors = Vec::new();
        validate_widget_instances(&raw, &known, &type_map, &mut errors);
        assert!(
            errors.is_empty(),
            "distinct instance_ids should be accepted, got: {errors:?}"
        );
    }

    /// WHEN enum initial_param not in allowed_values THEN error.
    #[test]
    fn initial_params_enum_out_of_allowed_values_produces_error() {
        let severity_decl = WidgetParameterDeclaration {
            name: "severity".to_string(),
            param_type: WidgetParamType::Enum,
            default_value: WidgetParameterValue::Enum("info".to_string()),
            constraints: Some(WidgetParamConstraints {
                enum_allowed_values: vec!["info".into(), "warning".into(), "error".into()],
                ..Default::default()
            }),
        };
        let alert_type = LoadedWidgetType {
            name: "alert".to_string(),
            parameter_schema: vec![severity_decl],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 1.0,
                height_pct: 1.0,
            },
            default_contention_policy: ContentionPolicy::LatestWins,
        };
        let mut params = HashMap::new();
        params.insert(
            "severity".to_string(),
            AnyValue(toml::Value::String("critical".into())),
        );
        let raw = make_config_with_tab_widgets(vec![RawTabWidget {
            widget_type: Some("alert".into()),
            initial_params: params,
            ..Default::default()
        }]);
        let known: HashSet<String> = ["alert".to_string()].into();
        let type_map = type_map_from(&[alert_type]);
        let mut errors = Vec::new();
        validate_widget_instances(&raw, &known, &type_map, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::WidgetInvalidInitialParams)),
            "enum out of allowed_values should produce CONFIG_WIDGET_INVALID_INITIAL_PARAMS, got: {errors:?}"
        );
    }

    // ── coerce_toml_to_widget_value ───────────────────────────────────────────

    #[test]
    fn coerce_f32_from_integer() {
        let val = toml::Value::Integer(42);
        let result = coerce_toml_to_widget_value(&val, WidgetParamType::F32);
        assert!(matches!(result, Some(WidgetParameterValue::F32(f)) if (f - 42.0).abs() < 1e-6));
    }

    #[test]
    fn coerce_f32_from_float() {
        let val = toml::Value::Float(0.5);
        let result = coerce_toml_to_widget_value(&val, WidgetParamType::F32);
        assert!(matches!(result, Some(WidgetParameterValue::F32(f)) if (f - 0.5).abs() < 1e-6));
    }

    #[test]
    fn coerce_f32_from_string_rejected() {
        let val = toml::Value::String("bad".into());
        let result = coerce_toml_to_widget_value(&val, WidgetParamType::F32);
        assert!(result.is_none());
    }

    #[test]
    fn coerce_color_from_array() {
        let val = toml::Value::Array(vec![
            toml::Value::Integer(255),
            toml::Value::Integer(128),
            toml::Value::Integer(0),
            toml::Value::Integer(255),
        ]);
        let result = coerce_toml_to_widget_value(&val, WidgetParamType::Color);
        assert!(result.is_some());
    }

    #[test]
    fn coerce_color_from_wrong_length_rejected() {
        let val = toml::Value::Array(vec![toml::Value::Integer(255), toml::Value::Integer(128)]);
        let result = coerce_toml_to_widget_value(&val, WidgetParamType::Color);
        assert!(result.is_none());
    }
}
