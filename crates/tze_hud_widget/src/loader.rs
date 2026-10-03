//! Widget asset bundle directory scanner and loader.
//!
//! # Bundle Layout
//!
//! A valid bundle is a directory with:
//! - `widget.toml` — the manifest (required)
//! - One or more `.svg` files referenced by the manifest (required for each layer)
//!
//! # Bundle Scan Algorithm
//!
//! 1. For each configured bundle path, enumerate immediate subdirectories.
//! 2. For each subdirectory, attempt to load a bundle.
//! 3. If loading fails, log the structured error and continue (do not abort).
//! 4. If loading succeeds but the widget type name duplicates an already-loaded
//!    bundle, reject the new bundle with `WIDGET_BUNDLE_DUPLICATE_TYPE`.
//!
//! Source: widget-system/spec.md §Requirement: Widget Asset Bundle Format,
//!         §Requirement: SVG Layer Parameter Bindings.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tze_hud_resource::validation::parse_svg_dimensions;
use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetBinding, WidgetBindingMapping,
    WidgetDefinition, WidgetHoverBehavior, WidgetNormalizedRect, WidgetParamConstraints,
    WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue, WidgetSvgLayer,
};

use crate::error::BundleError;
use crate::manifest::{
    RawBinding, RawHoverBehavior, RawManifest, RawNormalizedRect, RawParameterDeclaration,
};
use crate::svg_ids::collect_svg_element_ids;

// ─── Bundle loader ─────────────────────────────────────────────────────────────

/// Result of loading a single widget asset bundle.
#[derive(Clone, Debug)]
pub struct LoadedBundle {
    /// The widget type definition, ready to register into WidgetRegistry.
    pub definition: WidgetDefinition,
    /// Raw SVG bytes keyed by filename within the bundle directory.
    /// These can be uploaded to the resource store as IMAGE_SVG resources.
    pub svg_contents: HashMap<String, Vec<u8>>,
}

/// Outcome of scanning a bundle directory: either a successful load or a
/// structured error (the error is logged but does not abort scanning).
///
/// `LoadedBundle` is intentionally large — it holds in-memory SVG bytes keyed
/// by filename. Bundle scanning is a startup-time, non-hot-path operation, so
/// the extra stack size is acceptable here. Boxing would require call-site
/// updates across the workspace.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum BundleScanResult {
    Ok(LoadedBundle),
    Err(BundleError),
}

/// Scan one or more bundle root directories and load all valid widget bundles.
///
/// For each root path, every immediate subdirectory is treated as a potential
/// bundle.  Failed bundles are returned as `BundleScanResult::Err` entries and
/// logged at `WARN` level; they do not prevent other bundles from loading.
///
/// Duplicate widget type names across bundles produce a
/// `WIDGET_BUNDLE_DUPLICATE_TYPE` error for the second bundle.
///
/// # Arguments
///
/// - `bundle_roots`: directories to scan; each immediate subdirectory is a
///   potential bundle.
/// - `tokens`: design-token map used to resolve `{{key}}` / `{{token.key}}`
///   placeholders in SVG files.  Pass an empty map when no token substitution
///   is needed.
///
/// Source: widget-system/spec.md §Requirement: Widget Asset Bundle Format.
pub fn scan_bundle_dirs(
    bundle_roots: &[PathBuf],
    tokens: &HashMap<String, String>,
) -> Vec<BundleScanResult> {
    let mut results: Vec<BundleScanResult> = Vec::new();
    // Track registered names to detect duplicates.
    let mut registered: HashMap<String, PathBuf> = HashMap::new();

    for root in bundle_roots {
        let read_dir = match std::fs::read_dir(root) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(
                    path = %root.display(),
                    error = %e,
                    "widget bundle root not readable, skipping"
                );
                continue;
            }
        };

        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue; // skip non-directory entries
            }

            let result = load_bundle_dir_with_tokens(&path, tokens);
            match &result {
                BundleScanResult::Ok(bundle) => {
                    let name = bundle.definition.id.clone();
                    if let Some(existing) = registered.get(&name) {
                        let err = BundleError::DuplicateType {
                            name: name.clone(),
                            existing_path: existing.display().to_string(),
                            new_path: path.display().to_string(),
                        };
                        tracing::warn!(wire_code = err.wire_code(), "{}", err);
                        results.push(BundleScanResult::Err(err));
                        continue;
                    }
                    registered.insert(name, path.clone());
                    tracing::info!(
                        widget_name = bundle.definition.id,
                        path = %path.display(),
                        "loaded widget bundle"
                    );
                }
                BundleScanResult::Err(err) => {
                    tracing::warn!(
                        wire_code = err.wire_code(),
                        path = %path.display(),
                        "{}",
                        err
                    );
                }
            }
            results.push(result);
        }
    }

    results
}

/// Load a single bundle directory with no token substitution.
///
/// Returns `BundleScanResult::Ok` on success, or `BundleScanResult::Err` with
/// the first structural error encountered.  A rejected bundle does not prevent
/// other bundles from loading.
pub fn load_bundle_dir(dir: &Path) -> BundleScanResult {
    load_bundle_dir_with_tokens(dir, &HashMap::new())
}

/// Load a single bundle directory, substituting design-token placeholders in
/// SVG files using the supplied `tokens` map.
///
/// Returns `BundleScanResult::Ok` on success, or `BundleScanResult::Err` with
/// the first structural error encountered.  A rejected bundle does not prevent
/// other bundles from loading.
pub fn load_bundle_dir_with_tokens(
    dir: &Path,
    tokens: &HashMap<String, String>,
) -> BundleScanResult {
    let path_str = dir.display().to_string();
    match load_bundle_dir_inner(dir, &path_str, tokens) {
        Ok(bundle) => BundleScanResult::Ok(bundle),
        Err(e) => BundleScanResult::Err(e),
    }
}

/// Validate a runtime-registered SVG layer against an existing widget type definition.
///
/// This enforces the same structural checks used by startup bundle loading:
/// token substitution, SVG parse validation, and binding target resolution.
///
/// Returns the resolved SVG bytes (post token substitution) on success.
///
/// Source: widget-system/spec.md §Requirement: Widget Asset Bundle Format,
///         §Requirement: Runtime Widget SVG Registration.
pub fn validate_runtime_svg_registration(
    definition: &WidgetDefinition,
    widget_type_id: &str,
    svg_filename: &str,
    svg_bytes: &[u8],
    tokens: &HashMap<String, String>,
) -> Result<Vec<u8>, BundleError> {
    let path_str = format!("runtime:{widget_type_id}");
    if definition.id != widget_type_id {
        return Err(BundleError::BindingUnresolvable {
            path: path_str,
            detail: format!(
                "runtime registration widget_type_id '{widget_type_id}' does not match definition id '{}'",
                definition.id
            ),
        });
    }

    let layer = definition
        .layers
        .iter()
        .find(|l| l.svg_file == svg_filename)
        .ok_or_else(|| BundleError::BindingUnresolvable {
            path: format!("runtime:{widget_type_id}"),
            detail: format!(
                "runtime registration references unknown svg_filename '{svg_filename}' for widget type '{widget_type_id}'"
            ),
        })?;

    validate_svg_layer(&path_str, svg_filename, svg_bytes, tokens, &layer.bindings)
}

/// Load a bundle from in-memory files (`(filename, bytes)`), e.g. bundles
/// embedded in the executable. Same validation as [`load_bundle_dir_with_tokens`];
/// `label` stands in for the directory path in error messages.
pub fn load_bundle_from_files(
    label: &str,
    files: &[(&str, &[u8])],
    tokens: &HashMap<String, String>,
) -> BundleScanResult {
    let read = |name: &str| {
        files
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, b)| b.to_vec())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    };
    match load_bundle_inner(&read, label, tokens) {
        Ok(bundle) => BundleScanResult::Ok(bundle),
        Err(e) => BundleScanResult::Err(e),
    }
}

fn load_bundle_dir_inner(
    dir: &Path,
    path_str: &str,
    tokens: &HashMap<String, String>,
) -> Result<LoadedBundle, BundleError> {
    let read = |name: &str| std::fs::read(dir.join(name));
    load_bundle_inner(&read, path_str, tokens)
}

fn load_bundle_inner(
    read: &dyn Fn(&str) -> std::io::Result<Vec<u8>>,
    path_str: &str,
    tokens: &HashMap<String, String>,
) -> Result<LoadedBundle, BundleError> {
    // Steps 1-2: Read and parse widget.toml.
    let manifest_bytes = read("widget.toml").map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BundleError::NoManifest {
                path: path_str.to_string(),
            }
        } else {
            BundleError::InvalidManifest {
                path: path_str.to_string(),
                detail: format!("cannot read widget.toml: {e}"),
            }
        }
    })?;
    let toml_str = String::from_utf8(manifest_bytes).map_err(|e| BundleError::InvalidManifest {
        path: path_str.to_string(),
        detail: format!("cannot read widget.toml: {e}"),
    })?;

    let raw: RawManifest = toml::from_str(&toml_str).map_err(|e| BundleError::InvalidManifest {
        path: path_str.to_string(),
        detail: format!("TOML parse error: {e}"),
    })?;

    // Step 3: Validate required manifest fields.
    let name = raw
        .name
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| BundleError::InvalidManifest {
            path: path_str.to_string(),
            detail: "missing required field 'name'".to_string(),
        })?;

    // Step 3a: Validate widget type id format: [a-z][a-z0-9-]*
    if !is_valid_widget_type_id(name) {
        return Err(BundleError::InvalidName {
            path: path_str.to_string(),
            name: name.to_string(),
        });
    }

    let version = raw
        .version
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| BundleError::InvalidManifest {
            path: path_str.to_string(),
            detail: "missing required field 'version'".to_string(),
        })?;

    let description = raw.description.as_deref().unwrap_or("").to_string();

    // Step 4: Parse parameter schema.
    let parameter_schema = parse_parameter_schema(&raw.parameter_schema, path_str)?;

    // Build a set of parameter names for binding validation.
    let param_names: HashSet<&str> = parameter_schema.iter().map(|p| p.name.as_str()).collect();
    // Build a map from param name to type for mapping validation.
    let param_types: HashMap<&str, WidgetParamType> = parameter_schema
        .iter()
        .map(|p| (p.name.as_str(), p.param_type))
        .collect();
    // Build a map from param name to enum_allowed_values for discrete binding validation.
    let param_enum_values: HashMap<&str, &[String]> = parameter_schema
        .iter()
        .map(|p| {
            let allowed: &[String] = p
                .constraints
                .as_ref()
                .map(|c| c.enum_allowed_values.as_slice())
                .unwrap_or(&[]);
            (p.name.as_str(), allowed)
        })
        .collect();
    let binding_validation_context = BindingValidationContext {
        param_names: &param_names,
        param_types: &param_types,
        param_enum_values: &param_enum_values,
    };

    // Step 5: Parse optional runtime hover behavior.
    let hover_behavior =
        parse_hover_behavior(raw.hover_behavior.as_ref(), &parameter_schema, path_str)?;

    // Step 6: Load SVG files and resolve bindings.
    let mut svg_contents: HashMap<String, Vec<u8>> = HashMap::new();
    let mut layers: Vec<WidgetSvgLayer> = Vec::new();

    for raw_layer in &raw.layers {
        let svg_file = raw_layer
            .svg_file
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BundleError::InvalidManifest {
                path: path_str.to_string(),
                detail: "a layer entry is missing required field 'svg_file'".to_string(),
            })?;

        // Step 5a/5b: Read the SVG (missing file is a distinct error).
        let svg_bytes = read(svg_file).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                BundleError::MissingSvg {
                    path: path_str.to_string(),
                    svg_file: svg_file.to_string(),
                }
            } else {
                BundleError::SvgParseError {
                    path: path_str.to_string(),
                    svg_file: svg_file.to_string(),
                    detail: format!("cannot read file: {e}"),
                }
            }
        })?;
        // Step 5b-post/5c/5d: validate SVG + binding targets.
        let (resolved_svg, bindings) = validate_svg_layer_and_manifest_bindings(
            path_str,
            svg_file,
            &svg_bytes,
            tokens,
            &raw_layer.bindings,
            &binding_validation_context,
        )?;

        // Store the resolved SVG text (post-substitution) as bytes.
        svg_contents.insert(svg_file.to_string(), resolved_svg);
        layers.push(WidgetSvgLayer {
            svg_file: svg_file.to_string(),
            bindings,
        });
    }

    // Step 7: Build WidgetDefinition.
    let contention_policy =
        parse_contention_policy(raw.default_contention_policy.as_deref(), path_str)?;
    let rendering_policy =
        parse_rendering_policy(raw.default_rendering_policy.as_deref(), path_str)?;

    // Default geometry: full display area (100% × 100% at origin).
    // Widget instances will override this via config.
    let default_geometry = GeometryPolicy::Relative {
        x_pct: 0.0,
        y_pct: 0.0,
        width_pct: 1.0,
        height_pct: 1.0,
    };

    let definition = WidgetDefinition {
        id: name.to_string(),
        name: name.to_string(),
        description,
        parameter_schema,
        layers,
        default_geometry_policy: default_geometry,
        default_rendering_policy: rendering_policy,
        default_contention_policy: contention_policy,
        max_publishers: WidgetDefinition::default_max_publishers(),
        ephemeral: false,
        hover_behavior,
    };

    tracing::debug!(
        widget_name = definition.id,
        version = version,
        svg_files = svg_contents.len(),
        "widget bundle loaded successfully"
    );

    Ok(LoadedBundle {
        definition,
        svg_contents,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_svg_layer_and_manifest_bindings(
    path_str: &str,
    svg_file: &str,
    svg_bytes: &[u8],
    tokens: &HashMap<String, String>,
    raw_bindings: &[RawBinding],
    binding_validation_context: &BindingValidationContext<'_>,
) -> Result<(Vec<u8>, Vec<WidgetBinding>), BundleError> {
    let resolved_svg = validate_svg_layer(path_str, svg_file, svg_bytes, tokens, &[])?;

    let svg_text = std::str::from_utf8(&resolved_svg).map_err(|e| BundleError::SvgParseError {
        path: path_str.to_string(),
        svg_file: svg_file.to_string(),
        detail: format!("file is not valid UTF-8: {e}"),
    })?;

    let element_ids =
        collect_svg_element_ids(svg_text).map_err(|e| BundleError::SvgParseError {
            path: path_str.to_string(),
            svg_file: svg_file.to_string(),
            detail: e,
        })?;

    let bindings = resolve_bindings(
        raw_bindings,
        svg_file,
        &element_ids,
        binding_validation_context.param_names,
        binding_validation_context.param_types,
        binding_validation_context.param_enum_values,
        path_str,
    )?;

    Ok((resolved_svg, bindings))
}

struct BindingValidationContext<'a> {
    param_names: &'a HashSet<&'a str>,
    param_types: &'a HashMap<&'a str, WidgetParamType>,
    param_enum_values: &'a HashMap<&'a str, &'a [String]>,
}

fn validate_svg_layer(
    path_str: &str,
    svg_file: &str,
    svg_bytes: &[u8],
    tokens: &HashMap<String, String>,
    expected_bindings: &[WidgetBinding],
) -> Result<Vec<u8>, BundleError> {
    let svg_text = std::str::from_utf8(svg_bytes).map_err(|e| BundleError::SvgParseError {
        path: path_str.to_string(),
        svg_file: svg_file.to_string(),
        detail: format!("file is not valid UTF-8: {e}"),
    })?;

    let svg_text_resolved = resolve_token_placeholders(svg_text, tokens).map_err(|key| {
        BundleError::UnresolvedToken {
            path: path_str.to_string(),
            svg_file: svg_file.to_string(),
            token_key: key,
        }
    })?;
    let svg_text = svg_text_resolved.as_str();

    parse_svg_dimensions(svg_text).map_err(|e| BundleError::SvgParseError {
        path: path_str.to_string(),
        svg_file: svg_file.to_string(),
        detail: e.to_string(),
    })?;

    if !expected_bindings.is_empty() {
        let element_ids =
            collect_svg_element_ids(svg_text).map_err(|e| BundleError::SvgParseError {
                path: path_str.to_string(),
                svg_file: svg_file.to_string(),
                detail: e,
            })?;
        for binding in expected_bindings {
            if !element_ids.contains(&binding.target_element) {
                return Err(BundleError::BindingUnresolvable {
                    path: path_str.to_string(),
                    detail: format!(
                        "layer '{svg_file}': runtime SVG missing bound target element id '{}'",
                        binding.target_element
                    ),
                });
            }
        }
    }

    Ok(svg_text.as_bytes().to_vec())
}

// ─── Parameter schema parsing ─────────────────────────────────────────────────

fn parse_parameter_schema(
    raw: &[RawParameterDeclaration],
    path_str: &str,
) -> Result<Vec<WidgetParameterDeclaration>, BundleError> {
    let mut params = Vec::new();
    for raw_param in raw {
        let name = raw_param
            .name
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BundleError::InvalidManifest {
                path: path_str.to_string(),
                detail: "parameter_schema entry missing required field 'name'".to_string(),
            })?;

        let type_str = raw_param
            .param_type
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BundleError::InvalidManifest {
                path: path_str.to_string(),
                detail: format!("parameter '{name}' missing required field 'type'"),
            })?;

        let param_type = parse_param_type(type_str).ok_or_else(|| {
            BundleError::InvalidManifest {
                path: path_str.to_string(),
                detail: format!(
                    "parameter '{name}': unknown type '{type_str}' (must be f32, string, color, or enum)"
                ),
            }
        })?;

        let default_value =
            parse_default_value(raw_param.default.as_ref(), param_type, name, path_str)?;

        let constraints = raw_param.constraints.as_ref().map(|c| {
            let mut wc = WidgetParamConstraints::default();
            if let Some(v) = c.f32_min {
                wc.f32_min = Some(v as f32);
            }
            if let Some(v) = c.f32_max {
                wc.f32_max = Some(v as f32);
            }
            if let Some(v) = c.string_max_bytes {
                wc.string_max_bytes = Some(v);
            }
            if !c.enum_allowed_values.is_empty() {
                wc.enum_allowed_values = c.enum_allowed_values.clone();
            }
            wc
        });

        params.push(WidgetParameterDeclaration {
            name: name.to_string(),
            param_type,
            default_value,
            constraints,
        });
    }
    Ok(params)
}

fn parse_param_type(s: &str) -> Option<WidgetParamType> {
    match s {
        "f32" => Some(WidgetParamType::F32),
        "string" => Some(WidgetParamType::String),
        "color" => Some(WidgetParamType::Color),
        "enum" => Some(WidgetParamType::Enum),
        _ => None,
    }
}

fn parse_default_value(
    raw: Option<&toml::Value>,
    param_type: WidgetParamType,
    name: &str,
    path_str: &str,
) -> Result<WidgetParameterValue, BundleError> {
    let raw = raw.ok_or_else(|| BundleError::InvalidManifest {
        path: path_str.to_string(),
        detail: format!("parameter '{name}' missing required field 'default'"),
    })?;

    let type_err = || BundleError::InvalidManifest {
        path: path_str.to_string(),
        detail: format!(
            "parameter '{name}': 'default' value type mismatch for type {param_type:?}"
        ),
    };

    match param_type {
        WidgetParamType::F32 => {
            let v = match raw {
                toml::Value::Float(f) => *f as f32,
                toml::Value::Integer(i) => *i as f32,
                _ => return Err(type_err()),
            };
            Ok(WidgetParameterValue::F32(v))
        }
        WidgetParamType::String => {
            let s = raw.as_str().ok_or_else(type_err)?;
            Ok(WidgetParameterValue::String(s.to_string()))
        }
        WidgetParamType::Color => {
            // Expect array of 4 integers [r, g, b, a].
            let arr = raw.as_array().ok_or_else(type_err)?;
            if arr.len() != 4 {
                return Err(BundleError::InvalidManifest {
                    path: path_str.to_string(),
                    detail: format!(
                        "parameter '{name}': color default must be [r, g, b, a] (4 integers)"
                    ),
                });
            }
            let mut components = [0u8; 4];
            for (i, v) in arr.iter().enumerate() {
                let int = v.as_integer().ok_or_else(|| BundleError::InvalidManifest {
                    path: path_str.to_string(),
                    detail: format!("parameter '{name}': color component {i} must be an integer"),
                })?;
                if !(0..=255).contains(&int) {
                    return Err(BundleError::InvalidManifest {
                        path: path_str.to_string(),
                        detail: format!(
                            "parameter '{name}': color component {i} value {int} out of range [0, 255]"
                        ),
                    });
                }
                components[i] = int as u8;
            }
            // WidgetParameterValue::Color uses Rgba with f32 [0.0, 1.0] components.
            use tze_hud_scene::types::Rgba;
            Ok(WidgetParameterValue::Color(Rgba::new(
                components[0] as f32 / 255.0,
                components[1] as f32 / 255.0,
                components[2] as f32 / 255.0,
                components[3] as f32 / 255.0,
            )))
        }
        WidgetParamType::Enum => {
            let s = raw.as_str().ok_or_else(type_err)?;
            Ok(WidgetParameterValue::Enum(s.to_string()))
        }
    }
}

fn parse_hover_behavior(
    raw: Option<&RawHoverBehavior>,
    parameter_schema: &[WidgetParameterDeclaration],
    path_str: &str,
) -> Result<Option<WidgetHoverBehavior>, BundleError> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    let trigger_rect_raw =
        raw.trigger_rect
            .as_ref()
            .ok_or_else(|| BundleError::InvalidManifest {
                path: path_str.to_string(),
                detail: "hover_behavior is missing required table 'trigger_rect'".to_string(),
            })?;
    let trigger_rect = parse_normalized_rect(trigger_rect_raw, path_str)?;

    let visibility_param = raw
        .visibility_param
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| BundleError::InvalidManifest {
            path: path_str.to_string(),
            detail: "hover_behavior is missing required field 'visibility_param'".to_string(),
        })?;

    let decl = parameter_schema
        .iter()
        .find(|p| p.name == visibility_param)
        .ok_or_else(|| BundleError::InvalidManifest {
            path: path_str.to_string(),
            detail: format!(
                "hover_behavior.visibility_param '{visibility_param}' is not in parameter_schema"
            ),
        })?;
    if decl.param_type != WidgetParamType::F32 {
        return Err(BundleError::InvalidManifest {
            path: path_str.to_string(),
            detail: format!(
                "hover_behavior.visibility_param '{visibility_param}' must be type 'f32', got {:?}",
                decl.param_type
            ),
        });
    }

    let delay_ms = raw.delay_ms.unwrap_or(3_000);
    let hidden_value = raw.hidden_value.unwrap_or(0.0);
    let visible_value = raw.visible_value.unwrap_or(1.0);
    if !hidden_value.is_finite() || !visible_value.is_finite() {
        return Err(BundleError::InvalidManifest {
            path: path_str.to_string(),
            detail: "hover_behavior hidden_value/visible_value must be finite f32".to_string(),
        });
    }

    Ok(Some(WidgetHoverBehavior {
        trigger_rect,
        delay_ms,
        visibility_param: visibility_param.to_string(),
        hidden_value,
        visible_value,
    }))
}

fn parse_normalized_rect(
    raw: &RawNormalizedRect,
    path_str: &str,
) -> Result<WidgetNormalizedRect, BundleError> {
    let x_pct = raw
        .x_pct
        .ok_or_else(|| invalid_hover_rect(path_str, "x_pct missing"))?;
    let y_pct = raw
        .y_pct
        .ok_or_else(|| invalid_hover_rect(path_str, "y_pct missing"))?;
    let width_pct = raw
        .width_pct
        .ok_or_else(|| invalid_hover_rect(path_str, "width_pct missing"))?;
    let height_pct = raw
        .height_pct
        .ok_or_else(|| invalid_hover_rect(path_str, "height_pct missing"))?;

    let all = [x_pct, y_pct, width_pct, height_pct];
    if all.iter().any(|v| !v.is_finite()) {
        return Err(invalid_hover_rect(
            path_str,
            "all values must be finite f32",
        ));
    }
    if x_pct < 0.0 || y_pct < 0.0 || width_pct <= 0.0 || height_pct <= 0.0 {
        return Err(invalid_hover_rect(
            path_str,
            "x_pct/y_pct must be >= 0 and width_pct/height_pct must be > 0",
        ));
    }
    if x_pct + width_pct > 1.0 || y_pct + height_pct > 1.0 {
        return Err(invalid_hover_rect(
            path_str,
            "trigger_rect must stay within normalized [0,1] bounds",
        ));
    }

    Ok(WidgetNormalizedRect {
        x_pct,
        y_pct,
        width_pct,
        height_pct,
    })
}

fn invalid_hover_rect(path_str: &str, detail: &str) -> BundleError {
    BundleError::InvalidManifest {
        path: path_str.to_string(),
        detail: format!("hover_behavior.trigger_rect invalid: {detail}"),
    }
}

// ─── Binding resolution ────────────────────────────────────────────────────────

/// Resolve and validate all bindings for a single layer.
///
/// Source: widget-system/spec.md §Requirement: SVG Layer Parameter Bindings.
fn resolve_bindings(
    raw_bindings: &[RawBinding],
    svg_file: &str,
    element_ids: &HashSet<String>,
    param_names: &HashSet<&str>,
    param_types: &HashMap<&str, WidgetParamType>,
    param_enum_values: &HashMap<&str, &[String]>,
    path_str: &str,
) -> Result<Vec<WidgetBinding>, BundleError> {
    let mut bindings = Vec::new();

    for raw_b in raw_bindings {
        let param = raw_b
            .param
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BundleError::InvalidManifest {
                path: path_str.to_string(),
                detail: format!("layer '{svg_file}': binding missing required field 'param'"),
            })?;

        let target_element = raw_b
            .target_element
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BundleError::InvalidManifest {
                path: path_str.to_string(),
                detail: format!(
                    "layer '{svg_file}': binding for param '{param}' missing 'target_element'"
                ),
            })?;

        let target_attribute = raw_b
            .target_attribute
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BundleError::InvalidManifest {
                path: path_str.to_string(),
                detail: format!(
                    "layer '{svg_file}': binding for param '{param}' missing 'target_attribute'"
                ),
            })?;

        let mapping_str = raw_b
            .mapping
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BundleError::InvalidManifest {
                path: path_str.to_string(),
                detail: format!(
                    "layer '{svg_file}': binding for param '{param}' missing 'mapping'"
                ),
            })?;

        // Validate: param name must exist in the parameter schema.
        if !param_names.contains(param) {
            return Err(BundleError::BindingUnresolvable {
                path: path_str.to_string(),
                detail: format!(
                    "layer '{svg_file}': binding references nonexistent parameter '{param}'"
                ),
            });
        }

        // Validate: target_element must exist in the SVG (except for text-content,
        // where any element with an id is valid — we still require the element exists).
        if !element_ids.contains(target_element) {
            return Err(BundleError::BindingUnresolvable {
                path: path_str.to_string(),
                detail: format!(
                    "layer '{svg_file}': binding references nonexistent SVG element id '{target_element}'"
                ),
            });
        }

        let param_type = *param_types.get(param).unwrap(); // checked above
        let enum_allowed = *param_enum_values.get(param).unwrap(); // checked above

        // Validate and parse the mapping.
        let mapping = parse_binding_mapping(
            mapping_str,
            raw_b,
            param,
            param_type,
            enum_allowed,
            svg_file,
            path_str,
        )?;

        bindings.push(WidgetBinding {
            param: param.to_string(),
            target_element: target_element.to_string(),
            target_attribute: target_attribute.to_string(),
            mapping,
        });
    }

    Ok(bindings)
}

/// Parse and validate a binding mapping.
///
/// Validates that the mapping type is compatible with the parameter type:
/// - `linear` is only valid for f32 parameters.
/// - `direct` is valid for string and color parameters.
/// - `discrete` is only valid for enum parameters.
///
/// For `discrete` mappings, also validates that `value_map` exactly covers
/// `enum_allowed_values`: every allowed enum value must have an entry, and no
/// extra entries beyond the allowed values may be present.
fn parse_binding_mapping(
    mapping_str: &str,
    raw_b: &RawBinding,
    param: &str,
    param_type: WidgetParamType,
    enum_allowed: &[String],
    svg_file: &str,
    path_str: &str,
) -> Result<WidgetBindingMapping, BundleError> {
    match mapping_str {
        "linear" => {
            if param_type != WidgetParamType::F32 {
                return Err(BundleError::BindingUnresolvable {
                    path: path_str.to_string(),
                    detail: format!(
                        "layer '{svg_file}': binding param '{param}' uses 'linear' mapping but type is {param_type:?} (linear is only valid for f32)"
                    ),
                });
            }
            let attr_min = raw_b.attr_min.unwrap_or(0.0) as f32;
            let attr_max = raw_b.attr_max.unwrap_or(1.0) as f32;
            Ok(WidgetBindingMapping::Linear { attr_min, attr_max })
        }
        "direct" => {
            if param_type != WidgetParamType::String && param_type != WidgetParamType::Color {
                return Err(BundleError::BindingUnresolvable {
                    path: path_str.to_string(),
                    detail: format!(
                        "layer '{svg_file}': binding param '{param}' uses 'direct' mapping but type is {param_type:?} (direct is only valid for string and color)"
                    ),
                });
            }
            Ok(WidgetBindingMapping::Direct)
        }
        "discrete" => {
            if param_type != WidgetParamType::Enum {
                return Err(BundleError::BindingUnresolvable {
                    path: path_str.to_string(),
                    detail: format!(
                        "layer '{svg_file}': binding param '{param}' uses 'discrete' mapping but type is {param_type:?} (discrete is only valid for enum)"
                    ),
                });
            }

            // Validate that value_map covers all enum_allowed_values (no missing entries).
            let missing: Vec<&str> = enum_allowed
                .iter()
                .filter(|v| !raw_b.value_map.contains_key(v.as_str()))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                return Err(BundleError::BindingUnresolvable {
                    path: path_str.to_string(),
                    detail: format!(
                        "layer '{svg_file}': discrete binding for param '{param}' is missing value_map entries for enum values: {missing:?}"
                    ),
                });
            }

            // Validate that value_map has no extra entries beyond enum_allowed_values.
            let extra: Vec<&str> = raw_b
                .value_map
                .keys()
                .filter(|k| !enum_allowed.contains(*k))
                .map(String::as_str)
                .collect();
            if !extra.is_empty() {
                return Err(BundleError::BindingUnresolvable {
                    path: path_str.to_string(),
                    detail: format!(
                        "layer '{svg_file}': discrete binding for param '{param}' has value_map entries not in enum_allowed_values: {extra:?}"
                    ),
                });
            }

            Ok(WidgetBindingMapping::Discrete {
                value_map: raw_b.value_map.clone(),
            })
        }
        other => Err(BundleError::BindingUnresolvable {
            path: path_str.to_string(),
            detail: format!(
                "layer '{svg_file}': binding param '{param}' has unknown mapping type '{other}'"
            ),
        }),
    }
}

// ─── Widget type id validation ────────────────────────────────────────────────

/// Returns `true` if `id` conforms to the widget type id format: `[a-z][a-z0-9-]*`.
///
/// The id must:
/// - start with a lowercase ASCII letter (`a`–`z`),
/// - contain only lowercase ASCII letters, ASCII digits, or hyphens (`-`).
///
/// Source: scene-graph/spec.md §Widget Type Identifier.
pub(crate) fn is_valid_widget_type_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        // Must start with a lowercase letter.
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    // Remaining characters must be lowercase letters, digits, or hyphens.
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ─── Policy helpers ────────────────────────────────────────────────────────────

fn parse_contention_policy(
    s: Option<&str>,
    _path_str: &str,
) -> Result<ContentionPolicy, BundleError> {
    Ok(match s {
        None | Some("LatestWins") | Some("latest_wins") => ContentionPolicy::LatestWins,
        Some("Stack") | Some("stack") => ContentionPolicy::Stack { max_depth: 8 },
        Some("Replace") | Some("replace") => ContentionPolicy::Replace,
        _ => ContentionPolicy::LatestWins,
    })
}

fn parse_rendering_policy(
    s: Option<&str>,
    _path_str: &str,
) -> Result<RenderingPolicy, BundleError> {
    // Default rendering policy when not specified.
    let _ = s;
    Ok(RenderingPolicy::default())
}

// ─── Token placeholder resolution ────────────────────────────────────────────
// Delegated to `tze_hud_scene::svg_tokens` so the compositor can share the
// same implementation without a circular dependency.

pub(crate) use tze_hud_scene::resolve_token_placeholders;

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        is_valid_widget_type_id, resolve_token_placeholders, validate_runtime_svg_registration,
    };
    use std::collections::HashMap;

    #[test]
    fn widget_type_id_accepts_only_lowercase_kebab() {
        for ok in ["a", "gauge", "widget123", "my-widget", "a1b2-c3d4"] {
            assert!(is_valid_widget_type_id(ok), "{ok:?} should be valid");
        }
        for bad in [
            "", "1gauge", "-gauge", "Gauge", "my-Gauge", "my gauge", "my_gauge", "my.gauge",
            "my/gauge",
        ] {
            assert!(!is_valid_widget_type_id(bad), "{bad:?} should be invalid");
        }
    }

    fn token_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// (case, tokens, input, expected output). Covers prefixed and bare forms,
    /// escapes, XML-comment skipping, no recursive substitution, and every
    /// near-miss that must pass through verbatim instead of erroring.
    #[test]
    fn placeholder_resolution_table() {
        type Case = (
            &'static str,
            &'static [(&'static str, &'static str)],
            &'static str,
            &'static str,
        );
        let cases: &[Case] = &[
            (
                "prefixed",
                &[("color.primary", "#f00")],
                "{{token.color.primary}}",
                "#f00",
            ),
            (
                "bare",
                &[("color.primary", "#f00")],
                "{{color.primary}}",
                "#f00",
            ),
            (
                "multiple, mixed forms",
                &[("fg", "white"), ("bg", "black")],
                r#"<t fill="{{fg}}" stroke="{{token.bg}}"/>"#,
                r#"<t fill="white" stroke="black"/>"#,
            ),
            (
                "inside style block",
                &[("c.a", "blue")],
                "<style>.x { fill: {{c.a}}; }</style>",
                "<style>.x { fill: blue; }</style>",
            ),
            (
                "no recursive substitution",
                &[("a", "{{token.b}}"), ("b", "BAD")],
                "{{token.a}}",
                "{{token.b}}",
            ),
            (
                "underscore allowed after first segment",
                &[("color.text_primary", "#0f0")],
                "{{token.color.text_primary}}",
                "#0f0",
            ),
            (
                "non-ascii preserved",
                &[("c", "red")],
                "<!-- caf\u{e9} -->{{c}}",
                "<!-- caf\u{e9} -->red",
            ),
            (
                "escaped braces become literals",
                &[],
                r"\{\{ x \}\}",
                "{{ x }}",
            ),
            (
                "whitespace inside braces",
                &[("k", "BAD")],
                "{{ token.k }}",
                "{{ token.k }}",
            ),
            ("empty braces", &[], "{{}}", "{{}}"),
            ("unclosed braces", &[], "{{ no close", "{{ no close"),
            (
                "underscore in first segment",
                &[("my_key", "BAD")],
                "{{my_key}}",
                "{{my_key}}",
            ),
            (
                "prefixed invalid key does not fall back to bare",
                &[("token.foo_bar", "BAD")],
                "{{token.foo_bar}}",
                "{{token.foo_bar}}",
            ),
            (
                "comment skipped, live tokens around it resolved",
                &[("fg", "white"), ("bg", "black")],
                "{{fg}}<!-- {{bg}} -->{{bg}}",
                "white<!-- {{bg}} -->black",
            ),
            (
                "multi-line comment verbatim",
                &[("k", "BAD")],
                "<!--\n {{token.k}}\n--><r/>",
                "<!--\n {{token.k}}\n--><r/>",
            ),
            (
                "unclosed comment swallows rest",
                &[("k", "BAD")],
                "<!-- {{k}}",
                "<!-- {{k}}",
            ),
            (
                "sequential comments",
                &[("a", "A"), ("b", "B")],
                "<!-- {{a}} --><!-- {{b}} -->",
                "<!-- {{a}} --><!-- {{b}} -->",
            ),
        ];
        for (name, tokens, input, want) in cases {
            let got = resolve_token_placeholders(input, &token_map(tokens))
                .unwrap_or_else(|k| panic!("{name}: unexpected unresolved token {k:?}"));
            assert_eq!(&got, want, "{name}");
        }
    }

    /// A syntactically valid placeholder whose key is absent reports that key
    /// (prefixed or bare form), so authors see which token is missing.
    #[test]
    fn unresolved_placeholder_reports_the_missing_key() {
        for (input, key) in [
            (r#"<rect fill="{{token.missing.key}}"/>"#, "missing.key"),
            ("{{other.key}} stays put", "other.key"),
        ] {
            let err = resolve_token_placeholders(input, &HashMap::new()).unwrap_err();
            assert_eq!(err, key, "{input}");
        }
    }

    #[test]
    fn runtime_registration_validates_same_binding_targets_as_startup() {
        use tze_hud_scene::types::{
            ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetBinding, WidgetBindingMapping,
            WidgetDefinition, WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue,
            WidgetSvgLayer,
        };

        let def = WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "runtime test".to_string(),
            parameter_schema: vec![WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            }],
            layers: vec![WidgetSvgLayer {
                svg_file: "fill.svg".to_string(),
                bindings: vec![WidgetBinding {
                    param: "level".to_string(),
                    target_element: "bar".to_string(),
                    target_attribute: "height".to_string(),
                    mapping: WidgetBindingMapping::Linear {
                        attr_min: 0.0,
                        attr_max: 100.0,
                    },
                }],
            }],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 1.0,
                height_pct: 1.0,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false,
            hover_behavior: None,
        };

        let bad_svg =
            br#"<svg viewBox="0 0 100 100"><rect id="missing-bar" width="10" height="20"/></svg>"#;
        let err =
            validate_runtime_svg_registration(&def, "gauge", "fill.svg", bad_svg, &HashMap::new())
                .expect_err("runtime registration must reject unresolved binding targets");
        assert_eq!(err.wire_code(), "WIDGET_BINDING_UNRESOLVABLE");
    }

    #[test]
    fn runtime_registration_accepts_valid_svg_and_resolves_tokens() {
        use tze_hud_scene::types::{
            ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetBinding, WidgetBindingMapping,
            WidgetDefinition, WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue,
            WidgetSvgLayer,
        };

        let def = WidgetDefinition {
            id: "status".to_string(),
            name: "Status".to_string(),
            description: "runtime test".to_string(),
            parameter_schema: vec![WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            }],
            layers: vec![WidgetSvgLayer {
                svg_file: "layer.svg".to_string(),
                bindings: vec![WidgetBinding {
                    param: "level".to_string(),
                    target_element: "bar".to_string(),
                    target_attribute: "height".to_string(),
                    mapping: WidgetBindingMapping::Linear {
                        attr_min: 0.0,
                        attr_max: 100.0,
                    },
                }],
            }],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 1.0,
                height_pct: 1.0,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false,
            hover_behavior: None,
        };

        let svg = br#"<svg viewBox="0 0 100 100"><rect id="bar" fill="{{color.text.primary}}" width="10" height="20"/></svg>"#;
        let tokens = token_map(&[("color.text.primary", "#ffffff")]);
        let resolved = validate_runtime_svg_registration(&def, "status", "layer.svg", svg, &tokens)
            .expect("runtime registration should accept valid compatible SVG");
        let resolved_text = String::from_utf8(resolved).expect("resolved svg must be utf-8");
        assert!(resolved_text.contains("fill=\"#ffffff\""));
    }
}
