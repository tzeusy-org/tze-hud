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
    use super::{is_valid_widget_type_id, validate_runtime_svg_registration};

    // Valid ids.
    #[test]
    fn valid_single_letter() {
        assert!(is_valid_widget_type_id("a"));
    }

    #[test]
    fn valid_simple_name() {
        assert!(is_valid_widget_type_id("gauge"));
    }

    #[test]
    fn valid_with_digits() {
        assert!(is_valid_widget_type_id("widget123"));
    }

    #[test]
    fn valid_with_hyphens() {
        assert!(is_valid_widget_type_id("my-widget"));
    }

    #[test]
    fn valid_complex_name() {
        assert!(is_valid_widget_type_id("a1b2-c3d4"));
    }

    #[test]
    fn valid_trailing_digit() {
        assert!(is_valid_widget_type_id("progress-bar2"));
    }

    // Invalid ids.
    #[test]
    fn invalid_empty() {
        assert!(!is_valid_widget_type_id(""));
    }

    #[test]
    fn invalid_starts_with_digit() {
        assert!(!is_valid_widget_type_id("1gauge"));
    }

    #[test]
    fn invalid_starts_with_hyphen() {
        assert!(!is_valid_widget_type_id("-gauge"));
    }

    #[test]
    fn invalid_uppercase() {
        assert!(!is_valid_widget_type_id("Gauge"));
    }

    #[test]
    fn invalid_all_uppercase() {
        assert!(!is_valid_widget_type_id("GAUGE"));
    }

    #[test]
    fn invalid_contains_uppercase() {
        assert!(!is_valid_widget_type_id("my-Gauge"));
    }

    #[test]
    fn invalid_space() {
        assert!(!is_valid_widget_type_id("my gauge"));
    }

    #[test]
    fn invalid_underscore() {
        assert!(!is_valid_widget_type_id("my_gauge"));
    }

    #[test]
    fn invalid_dot() {
        assert!(!is_valid_widget_type_id("my.gauge"));
    }

    #[test]
    fn invalid_slash() {
        assert!(!is_valid_widget_type_id("my/gauge"));
    }

    // ─── resolve_token_placeholders ──────────────────────────────────────────────

    use super::resolve_token_placeholders;
    use std::collections::HashMap;

    fn token_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Single placeholder is substituted with the token value.
    #[test]
    fn single_placeholder_substituted() {
        let tokens = token_map(&[("color.primary", "#ff0000")]);
        let input = r##"<rect fill="{{token.color.primary}}"/>"##;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, r##"<rect fill="#ff0000"/>"##);
    }

    /// Multiple placeholders in one attribute value are all substituted.
    #[test]
    fn multiple_placeholders_in_one_attribute() {
        let tokens = token_map(&[("fg", "white"), ("bg", "black")]);
        let input = r#"<text fill="{{token.fg}}" stroke="{{token.bg}}">x</text>"#;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, r#"<text fill="white" stroke="black">x</text>"#);
    }

    /// Escaped braces `\{\{` / `\}\}` are preserved as literal `{{` / `}}`.
    ///
    /// The spec escape format requires each brace to be individually escaped:
    /// `\{\{` (not `\{{`) and `\}\}` (not `\}}`).
    #[test]
    fn escaped_braces_preserved_as_literals() {
        let tokens = token_map(&[]);
        // Each brace is individually escaped: \{ \{ and \} \}
        let input = r"no placeholder \{\{ here \}\} either";
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, "no placeholder {{ here }} either");
    }

    /// An unresolved token (valid syntax, key absent from map) produces an error.
    #[test]
    fn unresolved_token_yields_error() {
        let tokens = token_map(&[]);
        let input = r#"<rect fill="{{token.missing.key}}"/>"#;
        let err = resolve_token_placeholders(input, &tokens).unwrap_err();
        assert_eq!(err, "missing.key");
    }

    /// Resolved values are never re-scanned (no recursive substitution).
    #[test]
    fn no_recursive_substitution() {
        // The value itself looks like a placeholder; it must NOT be re-resolved.
        let tokens = token_map(&[("a", "{{token.b}}"), ("b", "SHOULD_NOT_APPEAR")]);
        let input = r#"<text>{{token.a}}</text>"#;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        // The value "{{token.b}}" is inserted verbatim; it should NOT be expanded.
        assert_eq!(result, r#"<text>{{token.b}}</text>"#);
    }

    /// Placeholder inside a `<style>` block is resolved identically to any attribute.
    #[test]
    fn placeholder_inside_style_block() {
        let tokens = token_map(&[("color.accent", "blue")]);
        let input = "<style>.cls { fill: {{token.color.accent}}; }</style>";
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, "<style>.cls { fill: blue; }</style>");
    }

    /// `{{ token.key }}` with whitespace inside braces is NOT treated as a placeholder.
    #[test]
    fn whitespace_inside_braces_not_a_placeholder() {
        let tokens = token_map(&[("color.primary", "SHOULD_NOT_APPEAR")]);
        // The spec requires no whitespace inside braces.
        let input = r#"<rect fill="{{ token.color.primary }}"/>"#;
        // Should pass through unchanged (no match).
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, input);
    }

    /// A bare-form placeholder with a valid key that is absent from the map yields an error.
    ///
    /// Previously `{{other.key}}` was passed through unchanged because it lacked the
    /// `token.` prefix.  With bare-key support, ANY valid-syntax `{{key}}` is treated
    /// as a token placeholder and produces an error when the key is absent.
    #[test]
    fn bare_key_absent_from_map_yields_error() {
        let tokens = token_map(&[]);
        let input = "{{other.key}} stays put";
        let err = resolve_token_placeholders(input, &tokens).unwrap_err();
        assert_eq!(err, "other.key");
    }

    /// A bare `{{}}` (empty inner) is passed through unchanged.
    #[test]
    fn empty_braces_passed_through() {
        let tokens = token_map(&[]);
        let result = resolve_token_placeholders("{{}}", &tokens).unwrap();
        assert_eq!(result, "{{}}");
    }

    /// Unclosed `{{` is passed through unchanged without panicking.
    #[test]
    fn unclosed_braces_passed_through() {
        let tokens = token_map(&[]);
        let result = resolve_token_placeholders("{{ no close", &tokens).unwrap();
        assert_eq!(result, "{{ no close");
    }

    /// A key whose first segment contains an underscore is NOT a valid placeholder.
    ///
    /// The spec pattern `[a-z][a-z0-9]*(\.[a-z][a-z0-9_]*)*)` disallows
    /// underscores in the first segment; only subsequent segments permit them.
    #[test]
    fn underscore_in_first_segment_not_a_placeholder() {
        // `my_key` starts with a valid letter but the first segment contains `_`.
        let tokens = token_map(&[("my_key", "SHOULD_NOT_APPEAR")]);
        let input = "{{token.my_key}} stays put";
        // Should pass through unchanged — `my_key` fails the first-segment rule.
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, "{{token.my_key}} stays put");
    }

    /// When `{{token.<key>}}` has a `token.` prefix but the stripped key is invalid,
    /// the whole sequence is passed through — it is NOT re-interpreted as a bare
    /// key `token.<key>`.  The `token.` prefix has priority: once the prefix is
    /// detected, only the stripped part is validated, and bare-form fallback is
    /// suppressed.
    ///
    /// This means `{{token.foo_bar}}` and `{{token.my_key}}` are both passed through
    /// unchanged even though `token.foo_bar` and `token.my_key` would pass
    /// `is_valid_token_key` as bare keys.
    #[test]
    fn prefixed_form_with_invalid_stripped_key_does_not_fall_back_to_bare() {
        // `foo_bar` has underscore in its first (and only) segment — invalid as a
        // stripped key.  `token.foo_bar` would be a valid bare key, but the
        // `token.` prefix precludes bare-form fallback.
        let tokens = token_map(&[("token.foo_bar", "SHOULD_NOT_APPEAR")]);
        let input = "{{token.foo_bar}} stays put";
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, "{{token.foo_bar}} stays put");
    }

    /// Underscores in a subsequent (non-first) segment are valid.
    #[test]
    fn underscore_in_subsequent_segment_is_valid() {
        let tokens = token_map(&[("color.text_primary", "#00ff00")]);
        let input = r##"<rect fill="{{token.color.text_primary}}"/>"##;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, r##"<rect fill="#00ff00"/>"##);
    }

    /// Non-ASCII (multi-byte UTF-8) content outside placeholders is preserved intact.
    #[test]
    fn non_ascii_content_preserved() {
        let tokens = token_map(&[("color.primary", "red")]);
        // U+00E9 (é) is a 2-byte UTF-8 sequence.
        let input = "<!-- caf\u{00e9} -->{{token.color.primary}}";
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, "<!-- caf\u{00e9} -->red");
    }

    // ─── Bare-key form (no `token.` prefix) ──────────────────────────────────────

    /// Bare `{{key}}` form (spec-preferred) resolves directly against the token map.
    #[test]
    fn bare_key_single_placeholder_substituted() {
        let tokens = token_map(&[("color.text.primary", "#ffffff")]);
        let input = r##"<text fill="{{color.text.primary}}">hi</text>"##;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, r##"<text fill="#ffffff">hi</text>"##);
    }

    /// Bare-form and prefixed-form placeholders are resolved identically when they
    /// reference the same token key.
    #[test]
    fn bare_key_and_prefixed_key_equivalent() {
        let tokens = token_map(&[("color.primary", "#ff0000")]);
        let bare =
            resolve_token_placeholders(r##"<rect fill="{{color.primary}}"/>"##, &tokens).unwrap();
        let prefixed =
            resolve_token_placeholders(r##"<rect fill="{{token.color.primary}}"/>"##, &tokens)
                .unwrap();
        assert_eq!(bare, r##"<rect fill="#ff0000"/>"##);
        assert_eq!(prefixed, r##"<rect fill="#ff0000"/>"##);
    }

    /// Multiple bare-form placeholders in one SVG attribute are all substituted.
    #[test]
    fn bare_key_multiple_placeholders() {
        let tokens = token_map(&[("fg", "white"), ("bg", "black")]);
        let input = r#"<text fill="{{fg}}" stroke="{{bg}}">x</text>"#;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, r#"<text fill="white" stroke="black">x</text>"#);
    }

    /// Bare-form placeholder inside a `<style>` block is resolved identically.
    #[test]
    fn bare_key_inside_style_block() {
        let tokens = token_map(&[("color.accent", "blue")]);
        let input = "<style>.cls { fill: {{color.accent}}; }</style>";
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, "<style>.cls { fill: blue; }</style>");
    }

    /// A bare-form placeholder whose key has an underscore in the first segment is
    /// NOT a valid placeholder (passes through unchanged / treated as not a placeholder).
    ///
    /// Note: since an invalid-syntax inner text is passed through as `{{...}}`, the
    /// outer caller sees unchanged output rather than an error.
    #[test]
    fn bare_key_underscore_in_first_segment_not_a_placeholder() {
        let tokens = token_map(&[("my_key", "SHOULD_NOT_APPEAR")]);
        let input = "{{my_key}} stays put";
        // `my_key` fails the first-segment rule (underscore not allowed there),
        // so the `{{my_key}}` sequence is passed through literally.
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, "{{my_key}} stays put");
    }

    /// A mix of bare-form and prefixed-form placeholders in the same SVG are both resolved.
    #[test]
    fn bare_and_prefixed_mix_both_resolved() {
        let tokens = token_map(&[
            ("color.text.primary", "#ffffff"),
            ("color.backdrop.default", "#000000"),
        ]);
        let input = r##"fill="{{color.text.primary}}" stroke="{{token.color.backdrop.default}}""##;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, r##"fill="#ffffff" stroke="#000000""##);
    }

    /// Reference subtitle SVG uses bare-form token placeholders — all must resolve.
    ///
    /// This mirrors the real reference profile SVG pattern used in production.
    #[test]
    fn reference_profile_bare_tokens_resolve() {
        let tokens = token_map(&[
            ("color.text.primary", "#ffffff"),
            ("color.outline.default", "#000000"),
            ("typography.subtitle.family", "sans-serif"),
            ("typography.subtitle.size", "48"),
            ("typography.subtitle.weight", "700"),
            ("stroke.outline.width", "2"),
        ]);
        // Simulate a simplified version of the reference subtitle outlined-text SVG.
        let input = r#"<text
  font-family="{{typography.subtitle.family}}"
  font-size="{{typography.subtitle.size}}"
  font-weight="{{typography.subtitle.weight}}"
  fill="{{color.text.primary}}"
  stroke="{{color.outline.default}}"
  stroke-width="{{stroke.outline.width}}">Subtitle</text>"#;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert!(result.contains(r#"font-family="sans-serif""#));
        assert!(result.contains(r#"font-size="48""#));
        assert!(result.contains(r##"fill="#ffffff""##));
        assert!(result.contains(r##"stroke="#000000""##));
        assert!(!result.contains("{{"));
    }

    // ─── XML comment skipping ─────────────────────────────────────────────────────

    /// A token placeholder inside an XML comment is NOT resolved.
    ///
    /// SVG authors use `<!-- {{token.key}} -->` for documentation examples.
    /// The resolver must leave such regions verbatim to avoid spurious errors.
    #[test]
    fn token_inside_xml_comment_not_resolved() {
        // The token IS in the map, but the placeholder is inside a comment.
        let tokens = token_map(&[("color.primary", "#ff0000")]);
        let input = r#"<!-- use {{token.color.primary}} here -->"#;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        // Comment must be preserved verbatim — no substitution.
        assert_eq!(result, input);
    }

    /// A token placeholder outside an XML comment IS resolved normally.
    #[test]
    fn token_outside_xml_comment_is_resolved() {
        let tokens = token_map(&[("color.primary", "#ff0000")]);
        let input = r##"<!-- doc comment --><rect fill="{{token.color.primary}}"/>"##;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, r##"<!-- doc comment --><rect fill="#ff0000"/>"##);
    }

    /// Comment before AND after a live token: the comment is preserved; the
    /// token that precedes the second comment and the token that follows are
    /// both resolved.
    #[test]
    fn comment_between_live_tokens() {
        let tokens = token_map(&[("fg", "white"), ("bg", "black")]);
        let input =
            r#"<text fill="{{fg}}"><!-- note: {{bg}} is backdrop --></text><rect fill="{{bg}}"/>"#;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(
            result,
            r#"<text fill="white"><!-- note: {{bg}} is backdrop --></text><rect fill="black"/>"#
        );
    }

    /// Multi-line XML comment containing a token placeholder: the entire
    /// comment region (including newlines) is emitted verbatim.
    #[test]
    fn multiline_comment_with_token_not_resolved() {
        let tokens = token_map(&[("color.text.primary", "#ffffff")]);
        let input = "<!--\n  Example: fill=\"{{token.color.text.primary}}\"\n--><rect/>";
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(
            result,
            "<!--\n  Example: fill=\"{{token.color.text.primary}}\"\n--><rect/>"
        );
    }

    /// An unclosed XML comment (`<!--` with no `-->`) causes the rest of the
    /// input to be emitted verbatim — no token resolution, no panic.
    #[test]
    fn unclosed_xml_comment_emits_rest_verbatim() {
        let tokens = token_map(&[("color.primary", "#ff0000")]);
        let input = "<!-- unclosed {{token.color.primary}}";
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, input);
    }

    /// A bare-form token placeholder inside an XML comment is also NOT resolved.
    #[test]
    fn bare_token_inside_xml_comment_not_resolved() {
        let tokens = token_map(&[("color.text.primary", "#ffffff")]);
        let input = r#"<!-- e.g. {{color.text.primary}} -->"#;
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, input);
    }

    /// Multiple sequential XML comments, each with a token placeholder, are
    /// all emitted verbatim.
    #[test]
    fn multiple_xml_comments_none_resolved() {
        let tokens = token_map(&[("a", "AAA"), ("b", "BBB")]);
        let input = "<!-- {{a}} --><!-- {{b}} -->";
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(result, input);
    }

    /// Tokens before, inside, and after a comment: only the outside tokens
    /// are resolved; the inside one is preserved.
    #[test]
    fn tokens_before_inside_and_after_comment() {
        let tokens = token_map(&[
            ("color.text.primary", "#ffffff"),
            ("color.backdrop.default", "#000000"),
        ]);
        let input = concat!(
            r##"<rect fill="{{color.backdrop.default}}"/>"##,
            r##"<!-- doc: {{color.text.primary}} -->"##,
            r##"<text fill="{{color.text.primary}}">hi</text>"##,
        );
        let result = resolve_token_placeholders(input, &tokens).unwrap();
        assert_eq!(
            result,
            concat!(
                r##"<rect fill="#000000"/>"##,
                r##"<!-- doc: {{color.text.primary}} -->"##,
                r##"<text fill="#ffffff">hi</text>"##,
            )
        );
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
