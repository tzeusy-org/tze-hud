//! Widget registry startup integration.
//!
//! This module handles widget system initialization at runtime startup:
//!
//! 1. **Bundle scanning**: scans configured `[widget_bundles].paths` for valid
//!    widget asset bundles.
//! 2. **Definition registration**: loads `WidgetDefinition` entries into the
//!    scene graph's `WidgetRegistry`.
//! 3. **Instance creation**: creates `WidgetInstance` entries for each
//!    `[[tabs.widgets]]` declaration in the config, bound to the appropriate
//!    tab scene IDs.
//!
//! ## Built-in bundles
//!
//! The three shipped bundles (gauge, progress-bar, status-indicator) are
//! embedded in the binary and always registered. `[widget_bundles].paths` is
//! optional; an on-disk bundle with the same type name overrides the built-in.
//!
//! ## Bundle errors
//!
//! Per widget-system/spec.md §Widget Asset Bundle Format: "A rejected bundle
//! MUST NOT prevent other valid bundles from loading; the runtime SHALL log
//! the error and continue."  Bundle load errors are logged at WARN level and
//! do not abort startup.
//!
//! ## Spec references
//!
//! - widget-system/spec.md §Requirement: Widget Registry
//! - widget-system/spec.md §Requirement: Widget Asset Bundle Format
//! - widget-system/spec.md §Requirement: Widget Instance Lifecycle
//! - widget-system/spec.md §Requirement: Widget Contention and Governance
//! - configuration/spec.md §Requirement: Widget Bundle Configuration
//! - configuration/spec.md §Requirement: Widget Instance Configuration

use std::collections::HashMap;
use std::path::Path;

use tze_hud_config::raw::RawConfig;
use tze_hud_config::widgets::{LoadedWidgetType, build_widget_instance, resolve_bundle_path};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::SceneId;
use tze_hud_widget::loader::{BundleScanResult, load_bundle_from_files, scan_bundle_dirs};

// ─── Built-in bundles ─────────────────────────────────────────────────────────

macro_rules! builtin_bundle {
    ($name:literal, [$($file:literal),*]) => {
        (
            $name,
            &[
                ("widget.toml", include_bytes!(concat!("../../../assets/widget_bundles/", $name, "/widget.toml")) as &[u8]),
                $(($file, include_bytes!(concat!("../../../assets/widget_bundles/", $name, "/", $file)) as &[u8])),*
            ] as BundleFiles,
        )
    };
}

/// Widget bundles compiled into the executable so a lone `tze_hud.exe` exposes
/// `gauge`, `progress-bar` and `status-indicator` with no asset files beside it.
type BundleFiles = &'static [(&'static str, &'static [u8])];

const BUILTIN_BUNDLES: &[(&str, BundleFiles)] = &[
    builtin_bundle!("gauge", ["background.svg", "fill.svg"]),
    builtin_bundle!("progress-bar", ["track.svg", "fill.svg"]),
    builtin_bundle!("status-indicator", ["indicator.svg"]),
];

// ─── Public API ───────────────────────────────────────────────────────────────

/// Initialize the widget registry in `scene` from the raw config.
///
/// Called at runtime startup after zone registry initialization.  This function:
///
/// 1. Resolves `[widget_bundles].paths` relative to `config_parent`.
/// 2. Calls `scan_bundle_dirs` to load all valid bundles (with token substitution).
/// 3. Registers each `WidgetDefinition` in `scene.widget_registry`.
/// 4. Pre-creates tabs from config if they do not exist in the scene graph yet
///    (needed to bind widget instances to tab IDs).
/// 5. Creates `WidgetInstance` records for each `[[tabs.widgets]]` entry.
///
/// Errors are logged at WARN level but never abort startup (spec §Widget
/// Asset Bundle Format).
///
/// # Arguments
///
/// - `scene`: The mutable scene graph to populate.
/// - `raw`: Raw (already-validated) config document.
/// - `config_parent`: Parent directory of the config file (for path resolution).
///   Pass `None` to resolve paths relative to the current working directory.
/// - `tab_name_to_id`: Map from tab name string to its `SceneId` in the scene graph.
///   Used to bind widget instances to tabs. When a tab name from config is not
///   found in this map, the function will attempt to pre-create the tab.
/// - `token_map`: Global design token map for `{{token.key}}` placeholder resolution
///   in widget SVG files. Pass an empty map when no design tokens are configured.
///   Per component-shape-language/spec.md §SVG Token Placeholder Resolution: global
///   bundles resolve against the global token map.
///
/// SVG asset from a loaded widget bundle: `(widget_type_id, svg_filename, svg_bytes)`.
pub type WidgetSvgAsset = (String, String, Vec<u8>);

pub fn init_widget_registry(
    scene: &mut SceneGraph,
    raw: &RawConfig,
    config_parent: Option<&Path>,
    tab_name_to_id: &HashMap<String, SceneId>,
    token_map: &HashMap<String, String>,
) -> Vec<WidgetSvgAsset> {
    // Step 1: Resolve on-disk bundle roots (optional) relative to `config_parent`.
    let base = config_parent.unwrap_or_else(|| Path::new("."));
    let bundle_roots: Vec<std::path::PathBuf> = raw
        .widget_bundles
        .iter()
        .flat_map(|wb| wb.paths.iter())
        .map(|p| resolve_bundle_path(p, base))
        .collect();

    // Step 2: Scan on-disk bundles with token substitution. Per
    // component-shape-language/spec.md §SVG Token Placeholder Resolution: global
    // bundles resolve {{token.key}} placeholders against the global token map.
    let mut scan_results = scan_bundle_dirs(&bundle_roots, token_map);

    // Built-in bundles are defaults: an on-disk bundle with the same type name
    // overrides the embedded one.
    for (name, files) in BUILTIN_BUNDLES {
        let overridden = scan_results.iter().any(|r| match r {
            BundleScanResult::Ok(b) => b.definition.id == *name,
            BundleScanResult::Err(_) => false,
        });
        if overridden {
            tracing::info!(
                widget_name = *name,
                "widget_startup: on-disk bundle overrides built-in"
            );
            continue;
        }
        scan_results.push(load_bundle_from_files(
            &format!("builtin:{name}"),
            files,
            token_map,
        ));
    }

    // Step 3: Register each valid WidgetDefinition.
    // Track registered names to detect cross-dir duplicates (scan_bundle_dirs
    // already handles within-dir duplicates, but we re-check here for safety).
    let mut registered_names: HashMap<String, ()> = HashMap::new();
    let mut type_map: HashMap<String, LoadedWidgetType> = HashMap::new();
    let mut svg_assets: Vec<WidgetSvgAsset> = Vec::new();

    for result in scan_results {
        match result {
            BundleScanResult::Ok(bundle) => {
                let name = bundle.definition.id.clone();
                if registered_names.contains_key(&name) {
                    tracing::warn!(
                        widget_name = %name,
                        "widget_startup: duplicate widget type name across bundle roots; \
                         skipping second occurrence"
                    );
                    continue;
                }
                registered_names.insert(name.clone(), ());

                // Build the LoadedWidgetType entry for instance creation.
                let loaded = LoadedWidgetType {
                    name: name.clone(),
                    parameter_schema: bundle.definition.parameter_schema.clone(),
                    default_geometry_policy: bundle.definition.default_geometry_policy,
                    default_contention_policy: bundle.definition.default_contention_policy,
                };
                type_map.insert(name.clone(), loaded);

                // Collect SVG bytes for compositor registration.
                for (svg_filename, svg_bytes) in &bundle.svg_contents {
                    svg_assets.push((name.clone(), svg_filename.clone(), svg_bytes.clone()));
                }

                // Register the WidgetDefinition in the scene graph.
                tracing::info!(
                    widget_name = %name,
                    svg_count = bundle.svg_contents.len(),
                    "widget_startup: registered widget type"
                );
                scene.widget_registry.register_definition(bundle.definition);
            }
            BundleScanResult::Err(err) => {
                // Spec: rejected bundle MUST NOT prevent other bundles from loading.
                tracing::warn!(
                    wire_code = err.wire_code(),
                    error = %err,
                    "widget_startup: bundle load error (skipping)"
                );
            }
        }
    }

    tracing::info!(
        widget_types = scene.widget_registry.definitions.len(),
        "widget_startup: widget type registration complete"
    );

    // Step 4: Create widget instances from [[tabs.widgets]] configuration.
    //
    // We need tab SceneIds to bind instances. Widget instances are declared
    // against named tabs from [[tabs]] in config. If the tab already exists in
    // the scene graph (from the caller's tab_name_to_id map), use its ID.
    // Otherwise, pre-create the tab so widget instances can be bound.
    let mut effective_tab_map: HashMap<String, SceneId> = tab_name_to_id.clone();
    let mut total_instances = 0usize;

    for (tab_idx, tab) in raw.tabs.iter().enumerate() {
        let tab_name = tab.name.as_deref().unwrap_or("<unnamed>");
        let tab_id = if let Some(&id) = effective_tab_map.get(tab_name) {
            id
        } else if tab.widgets.iter().any(|w| {
            w.widget_type
                .as_deref()
                .map(|t| type_map.contains_key(t))
                .unwrap_or(false)
        }) {
            // Pre-create the tab so widget instances can reference it.
            match scene.create_tab(tab_name, tab_idx as u32) {
                Ok(id) => {
                    tracing::debug!(
                        tab_name = tab_name,
                        "widget_startup: pre-created tab for widget instance binding"
                    );
                    effective_tab_map.insert(tab_name.to_string(), id);
                    id
                }
                Err(e) => {
                    tracing::warn!(
                        tab_name = tab_name,
                        error = %e,
                        "widget_startup: could not pre-create tab; skipping widget instances"
                    );
                    continue;
                }
            }
        } else {
            // No widgets need this tab; skip instance creation.
            continue;
        };

        for (widget_idx, raw_widget) in tab.widgets.iter().enumerate() {
            let widget_type = match raw_widget.widget_type.as_deref() {
                Some(t) if !t.is_empty() => t,
                _ => {
                    tracing::warn!(
                        tab_name = tab_name,
                        widget_idx = widget_idx,
                        "widget_startup: widget entry missing widget_type; skipping"
                    );
                    continue;
                }
            };

            // Check that the type is registered.
            if !type_map.contains_key(widget_type) {
                tracing::warn!(
                    tab_name = tab_name,
                    widget_type = widget_type,
                    "widget_startup: widget type not loaded; skipping instance creation"
                );
                continue;
            }

            // Build and register the widget instance.
            if let Some(instance) = build_widget_instance(raw_widget, tab_id, &type_map) {
                tracing::info!(
                    tab_name = tab_name,
                    instance_name = %instance.instance_name,
                    widget_type = %instance.widget_type_name,
                    "widget_startup: created widget instance"
                );
                scene.widget_registry.register_instance(instance);
                total_instances += 1;
            } else {
                tracing::warn!(
                    tab_name = tab_name,
                    widget_type = widget_type,
                    widget_idx = widget_idx,
                    "widget_startup: failed to build widget instance"
                );
            }
        }
    }

    tracing::info!(
        widget_instances = total_instances,
        svg_assets = svg_assets.len(),
        "widget_startup: widget instance creation complete"
    );

    svg_assets
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_config::raw::{RawConfig, RawWidgetBundles};
    use tze_hud_scene::graph::SceneGraph;

    fn default_tokens() -> HashMap<String, String> {
        use tze_hud_config::tokens::resolve_tokens;
        resolve_tokens(&Default::default(), &Default::default())
    }

    fn registered_types(scene: &SceneGraph) -> Vec<String> {
        let mut v: Vec<String> = scene.widget_registry.definitions.keys().cloned().collect();
        v.sort();
        v
    }

    /// WHEN no [widget_bundles] section exists THEN the three built-in bundles
    /// are still registered, and a missing on-disk root does not break startup.
    #[test]
    fn builtin_bundles_register_without_any_paths() {
        let expected = ["gauge", "progress-bar", "status-indicator"];
        for raw in [
            RawConfig::default(),
            RawConfig {
                widget_bundles: Some(RawWidgetBundles {
                    paths: vec!["/tmp/tze_hud_nonexistent_widget_dir_99999_a1b2c3".into()],
                }),
                ..RawConfig::default()
            },
        ] {
            let mut scene = SceneGraph::new(1920.0, 1080.0);
            init_widget_registry(&mut scene, &raw, None, &HashMap::new(), &default_tokens());
            assert_eq!(registered_types(&scene), expected);
        }
    }

    /// WHEN an on-disk bundle has the same type name as a built-in THEN the
    /// on-disk bundle wins and the type is registered once.
    #[test]
    fn on_disk_bundle_overrides_builtin_by_type_name() {
        let root = std::env::temp_dir().join(format!("tze_hud_override_{}", std::process::id()));
        let bundle = root.join("my-gauge");
        std::fs::create_dir_all(&bundle).unwrap();
        for (file, bytes) in BUILTIN_BUNDLES
            .iter()
            .find(|(n, _)| *n == "gauge")
            .unwrap()
            .1
        {
            let mut bytes = bytes.to_vec();
            if *file == "widget.toml" {
                let text = String::from_utf8(bytes).unwrap();
                bytes = text
                    .replace("Vertical fill gauge:", "OVERRIDDEN gauge:")
                    .into_bytes();
            }
            std::fs::write(bundle.join(file), bytes).unwrap();
        }

        let raw = RawConfig {
            widget_bundles: Some(RawWidgetBundles {
                paths: vec![root.to_string_lossy().into_owned()],
            }),
            ..RawConfig::default()
        };
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        init_widget_registry(&mut scene, &raw, None, &HashMap::new(), &default_tokens());
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(
            registered_types(&scene),
            ["gauge", "progress-bar", "status-indicator"]
        );
        let gauge = scene.widget_registry.get_definition("gauge").unwrap();
        assert!(
            gauge.description.starts_with("OVERRIDDEN"),
            "{}",
            gauge.description
        );
    }
}
