//! Behavior of the built-in widget bundles (`assets/widget_bundles/`) as the
//! loader and scene see them, plus one test per `BundleError` wire code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, RenderingPolicy, SceneId, WidgetBindingMapping,
    WidgetInstance, WidgetParameterValue,
};
use tze_hud_scene::{SceneGraph, validation::ValidationError};
use tze_hud_widget::error::BundleError;
use tze_hud_widget::loader::{
    BundleScanResult, LoadedBundle, load_bundle_dir, load_bundle_dir_with_tokens, scan_bundle_dirs,
};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn shipped_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/widget_bundles")
        .join(name)
}

/// Every token the shipped SVGs reference.
fn shipped_tokens() -> HashMap<String, String> {
    [
        ("color.backdrop.default", "#000000"),
        ("color.border.default", "#333333"),
        ("color.outline.default", "#000000"),
        ("color.severity.info", "#4A9EFF"),
        ("color.text.accent", "#4A9EFF"),
        ("color.text.primary", "#FFFFFF"),
        ("color.text.secondary", "#B0B0B0"),
        ("border.radius.medium", "8"),
        ("border.radius.large", "16"),
        ("stroke.border.width", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn load_shipped(name: &str) -> LoadedBundle {
    match load_bundle_dir_with_tokens(&shipped_path(name), &shipped_tokens()) {
        BundleScanResult::Ok(b) => b,
        BundleScanResult::Err(e) => panic!("shipped bundle {name} failed to load: {e}"),
    }
}

/// A scene with one shipped widget registered and instantiated under its own name.
fn scene_with(name: &str) -> SceneGraph {
    let mut definition = load_shipped(name).definition;
    definition.default_contention_policy = ContentionPolicy::LatestWins;
    definition.default_rendering_policy = RenderingPolicy::default();
    definition.default_geometry_policy = GeometryPolicy::Relative {
        x_pct: 0.0,
        y_pct: 0.0,
        width_pct: 0.25,
        height_pct: 0.25,
    };
    let current_params = definition
        .parameter_schema
        .iter()
        .map(|p| (p.name.clone(), p.default_value.clone()))
        .collect();

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.widget_registry.register_definition(definition);
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: name.to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: name.to_string(),
        current_params,
    });
    scene
}

fn publish(
    scene: &mut SceneGraph,
    widget: &str,
    params: &[(&str, WidgetParameterValue)],
) -> Result<(), ValidationError> {
    let params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    scene
        .publish_to_widget(widget, params, "agent.test", None, 0, None)
        .map(|_| ())
}

fn enum_val(s: &str) -> WidgetParameterValue {
    WidgetParameterValue::Enum(s.to_string())
}

fn str_val(s: &str) -> WidgetParameterValue {
    WidgetParameterValue::String(s.to_string())
}

/// Load a throwaway bundle made of `files` (name, contents) in a temp dir.
fn load_files(files: &[(&str, &str)], tokens: &HashMap<String, String>) -> BundleScanResult {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    load_bundle_dir_with_tokens(dir.path(), tokens)
}

fn expect_err(result: BundleScanResult) -> BundleError {
    match result {
        BundleScanResult::Err(e) => e,
        BundleScanResult::Ok(_) => panic!("bundle unexpectedly loaded"),
    }
}

const SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect id="bar"/></svg>"#;

fn manifest(name: &str, extra: &str) -> String {
    format!("name = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"t\"\n{extra}")
}

const LEVEL_PARAM: &str = "[[parameter_schema]]\nname = \"level\"\ntype = \"f32\"\ndefault = 0.0\n";

// ─── Shipped bundles load and bind ────────────────────────────────────────────

type BindingRow = (&'static str, &'static str, &'static str);
/// (bundle, parameters, svg layers, bindings each user-visible parameter must drive)
type BundleRow = (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
    &'static [BindingRow],
);

#[test]
fn shipped_bundles_load_and_bind() {
    // (bundle, parameters, svg layers, a binding each user-visible parameter must drive)
    let table: &[BundleRow] = &[
        (
            "gauge",
            &[
                "level",
                "label",
                "fill_color",
                "severity",
                "tooltip_visible",
                "readout",
            ],
            &["background.svg", "fill.svg"],
            &[
                ("level", "bar", "height"),
                ("fill_color", "bar", "fill"),
                ("label", "label-text", "text-content"),
                ("severity", "indicator", "fill"),
                ("readout", "tooltip-readout", "text-content"),
            ],
        ),
        (
            "progress-bar",
            &["progress", "label", "fill_color"],
            &["track.svg", "fill.svg"],
            &[
                ("progress", "fill-bar", "width"),
                ("fill_color", "fill-bar", "fill"),
                ("label", "label-text", "text-content"),
            ],
        ),
        (
            "status-indicator",
            &["status", "theme", "label", "reason", "tooltip_visible"],
            &["indicator.svg"],
            &[
                ("status", "system-fill", "fill"),
                ("theme", "system-group", "opacity"),
                ("label", "label-text", "text-content"),
                ("reason", "tooltip-reason", "text-content"),
            ],
        ),
    ];

    for (name, params, layers, bindings) in table {
        let bundle = load_shipped(name);
        let def = &bundle.definition;
        assert_eq!(def.id, *name);

        let got_params: Vec<_> = def
            .parameter_schema
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(&got_params, params, "{name}: parameters");
        let got_layers: Vec<_> = def.layers.iter().map(|l| l.svg_file.as_str()).collect();
        assert_eq!(&got_layers, layers, "{name}: layers");

        for (param, element, attr) in *bindings {
            assert!(
                def.layers.iter().flat_map(|l| &l.bindings).any(|b| {
                    b.param == *param && b.target_element == *element && b.target_attribute == *attr
                }),
                "{name}: no binding {param} -> {element}.{attr}"
            );
        }

        // Tokens are substituted into the bytes the compositor will rasterize.
        for (file, bytes) in &bundle.svg_contents {
            let text = String::from_utf8_lossy(bytes);
            assert!(
                !text.contains("=\"{{"),
                "{name}/{file}: unresolved placeholder"
            );
        }
    }
}

#[test]
fn shipped_bundles_need_their_tokens() {
    for name in ["gauge", "progress-bar", "status-indicator"] {
        let err = expect_err(load_bundle_dir(&shipped_path(name)));
        assert_eq!(err.wire_code(), "WIDGET_BUNDLE_UNRESOLVED_TOKEN", "{name}");
    }
}

#[test]
fn status_indicator_status_maps_each_value_to_its_own_color() {
    let def = load_shipped("status-indicator").definition;
    let binding = def
        .layers
        .iter()
        .flat_map(|l| &l.bindings)
        .find(|b| b.param == "status" && b.target_element == "system-fill")
        .unwrap();
    let WidgetBindingMapping::Discrete { value_map } = &binding.mapping else {
        panic!("status must be a discrete binding");
    };
    let mut colors: Vec<_> = ["online", "away", "busy", "offline"]
        .iter()
        .map(|s| value_map[*s].clone())
        .collect();
    colors.sort();
    colors.dedup();
    assert_eq!(colors.len(), 4, "each status needs a distinct color");
}

// ─── Publishing to shipped widgets ────────────────────────────────────────────

#[test]
fn shipped_widgets_clamp_out_of_range_f32() {
    // (widget, param, published, stored)
    for (widget, param, published, stored) in [
        ("gauge", "level", 1.5, 1.0),
        ("gauge", "level", -0.5, 0.0),
        ("gauge", "level", 0.4, 0.4),
        ("progress-bar", "progress", 2.0, 1.0),
    ] {
        let mut scene = scene_with(widget);
        publish(
            &mut scene,
            widget,
            &[(param, WidgetParameterValue::F32(published))],
        )
        .unwrap();
        let pubs = scene.widget_registry.active_for_widget(widget);
        match pubs[0].params.get(param) {
            Some(WidgetParameterValue::F32(got)) => {
                assert!(
                    (got - stored).abs() < 1e-6,
                    "{widget}.{param}={published}: stored {got}"
                )
            }
            other => panic!("{widget}.{param}: {other:?}"),
        }
    }
}

#[test]
fn shipped_widgets_reject_invalid_params() {
    use WidgetParameterValue::F32;
    let table = [
        ("gauge", "level", F32(f32::NAN)),
        ("gauge", "level", F32(f32::INFINITY)),
        ("gauge", "level", str_val("high")),
        ("gauge", "nonexistent", F32(0.5)),
        ("gauge", "severity", enum_val("Info")),
        ("status-indicator", "status", enum_val("invisible")),
        (
            "status-indicator",
            "label",
            str_val("a label well over sixteen bytes"),
        ),
    ];
    for (widget, param, value) in table {
        let mut scene = scene_with(widget);
        assert!(
            publish(&mut scene, widget, &[(param, value.clone())]).is_err(),
            "{widget}.{param}={value:?} should be rejected"
        );
    }
}

#[test]
fn invalid_param_rejects_the_whole_publish() {
    let mut scene = scene_with("gauge");
    let result = publish(
        &mut scene,
        "gauge",
        &[
            ("level", WidgetParameterValue::F32(0.5)),
            ("severity", enum_val("CRITICAL")),
        ],
    );
    assert!(result.is_err());
    assert!(scene.widget_registry.active_for_widget("gauge").is_empty());
}

#[test]
fn status_indicator_latest_publisher_wins_and_partial_updates_keep_other_params() {
    let mut scene = scene_with("status-indicator");
    let w = "status-indicator";
    publish(
        &mut scene,
        w,
        &[("status", enum_val("online")), ("label", str_val("A"))],
    )
    .unwrap();
    publish(
        &mut scene,
        w,
        &[("status", enum_val("busy")), ("label", str_val("B"))],
    )
    .unwrap();
    publish(&mut scene, w, &[("status", enum_val("away"))]).unwrap();

    let params = &scene.widget_registry.instances[w].current_params;
    assert_eq!(params["status"], enum_val("away"));
    assert_eq!(
        params["label"],
        str_val("B"),
        "unpublished params are retained"
    );
    assert_eq!(scene.widget_registry.active_for_widget(w).len(), 1);
}

// ─── BundleError wire codes (one test per code) ───────────────────────────────

#[test]
fn wire_code_no_manifest() {
    let err = expect_err(load_files(&[("fill.svg", SVG)], &HashMap::new()));
    assert!(matches!(err, BundleError::NoManifest { .. }));
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_NO_MANIFEST");
}

#[test]
fn wire_code_invalid_manifest() {
    // Rows: unparseable TOML, and well-formed TOML missing the required name.
    for body in ["this is = not [valid", "version = \"1.0.0\"\n"] {
        let err = expect_err(load_files(&[("widget.toml", body)], &HashMap::new()));
        assert!(
            matches!(err, BundleError::InvalidManifest { .. }),
            "{body}: {err:?}"
        );
        assert_eq!(err.wire_code(), "WIDGET_BUNDLE_INVALID_MANIFEST");
    }
}

#[test]
fn wire_code_invalid_name() {
    for name in ["Gauge", "1gauge", "my_gauge", "my gauge", "gauge!"] {
        let files = [(
            "widget.toml",
            manifest(name, "[[layers]]\nsvg_file = \"fill.svg\"\n"),
        )];
        let err = expect_err(load_files(
            &[(files[0].0, &files[0].1), ("fill.svg", SVG)],
            &HashMap::new(),
        ));
        assert!(
            matches!(err, BundleError::InvalidName { .. }),
            "{name}: {err:?}"
        );
        assert_eq!(err.wire_code(), "WIDGET_BUNDLE_INVALID_NAME");
    }
}

#[test]
fn wire_code_duplicate_type_and_siblings_still_load() {
    let root = tempfile::tempdir().unwrap();
    let m = manifest("dup", "[[layers]]\nsvg_file = \"fill.svg\"\n");
    for dir in ["a", "b"] {
        let d = root.path().join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("widget.toml"), &m).unwrap();
        std::fs::write(d.join("fill.svg"), SVG).unwrap();
    }
    // A broken sibling must not stop the rest from loading.
    std::fs::create_dir_all(root.path().join("c")).unwrap();

    let results = scan_bundle_dirs(&[root.path().to_path_buf()], &HashMap::new());
    let ok = results
        .iter()
        .filter(|r| matches!(r, BundleScanResult::Ok(_)))
        .count();
    let codes: Vec<_> = results
        .iter()
        .filter_map(|r| match r {
            BundleScanResult::Err(e) => Some(e.wire_code()),
            _ => None,
        })
        .collect();
    assert_eq!(ok, 1);
    assert!(codes.contains(&"WIDGET_BUNDLE_DUPLICATE_TYPE"), "{codes:?}");
    assert!(codes.contains(&"WIDGET_BUNDLE_NO_MANIFEST"), "{codes:?}");
}

#[test]
fn wire_code_missing_svg() {
    let m = manifest("t", "[[layers]]\nsvg_file = \"absent.svg\"\n");
    let err = expect_err(load_files(&[("widget.toml", &m)], &HashMap::new()));
    assert!(matches!(&err, BundleError::MissingSvg { svg_file, .. } if svg_file == "absent.svg"));
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_MISSING_SVG");
}

#[test]
fn wire_code_svg_parse_error() {
    let m = manifest("t", "[[layers]]\nsvg_file = \"fill.svg\"\n");
    // Malformed XML, and well-formed XML whose root is not <svg>.
    for body in ["<svg><unclosed></svg>", "<html></html>"] {
        let err = expect_err(load_files(
            &[("widget.toml", &m), ("fill.svg", body)],
            &HashMap::new(),
        ));
        assert!(
            matches!(err, BundleError::SvgParseError { .. }),
            "{body}: {err:?}"
        );
        assert_eq!(err.wire_code(), "WIDGET_BUNDLE_SVG_PARSE_ERROR");
    }
}

#[test]
fn wire_code_binding_unresolvable() {
    let binding = |param: &str, element: &str, mapping: &str| {
        manifest(
            "t",
            &format!(
                "{LEVEL_PARAM}\n[[layers]]\nsvg_file = \"fill.svg\"\n\n[[layers.bindings]]\n\
                 param = \"{param}\"\ntarget_element = \"{element}\"\ntarget_attribute = \"height\"\n\
                 mapping = \"{mapping}\"\n"
            ),
        )
    };
    let linear = "attr_min = 0.0\nattr_max = 1.0\n";
    let cases = [
        // (manifest, substring the detail must name)
        (binding("nope", "bar", "linear") + linear, "nope"),
        (
            binding("level", "no-such-id", "linear") + linear,
            "no-such-id",
        ),
        // Direct mapping is not valid for an f32 parameter.
        (binding("level", "bar", "direct"), "level"),
    ];
    for (m, mention) in cases {
        let err = expect_err(load_files(
            &[("widget.toml", &m), ("fill.svg", SVG)],
            &HashMap::new(),
        ));
        match &err {
            BundleError::BindingUnresolvable { detail, .. } => {
                assert!(detail.contains(mention), "{detail}")
            }
            other => panic!("expected BindingUnresolvable, got {other:?}"),
        }
        assert_eq!(err.wire_code(), "WIDGET_BINDING_UNRESOLVABLE");
    }
}

#[test]
fn discrete_binding_must_cover_exactly_the_enum_values() {
    let m = |entries: &str| {
        manifest(
            "t",
            &format!(
                "[[parameter_schema]]\nname = \"sev\"\ntype = \"enum\"\ndefault = \"info\"\n\
                 [parameter_schema.constraints]\nenum_allowed_values = [\"info\", \"error\"]\n\n\
                 [[layers]]\nsvg_file = \"fill.svg\"\n\n[[layers.bindings]]\nparam = \"sev\"\n\
                 target_element = \"bar\"\ntarget_attribute = \"fill\"\nmapping = \"discrete\"\n\n\
                 [layers.bindings.value_map]\n{entries}"
            ),
        )
    };
    let full = "info = \"a\"\nerror = \"b\"\n";
    let ok = load_files(
        &[("widget.toml", &m(full)), ("fill.svg", SVG)],
        &HashMap::new(),
    );
    assert!(matches!(ok, BundleScanResult::Ok(_)));
    for entries in [
        "info = \"a\"\n",
        "info = \"a\"\nerror = \"b\"\nextra = \"c\"\n",
    ] {
        let err = expect_err(load_files(
            &[("widget.toml", &m(entries)), ("fill.svg", SVG)],
            &HashMap::new(),
        ));
        assert_eq!(err.wire_code(), "WIDGET_BINDING_UNRESOLVABLE", "{entries}");
    }
}

#[test]
fn wire_code_unresolved_token() {
    let m = manifest("t", "[[layers]]\nsvg_file = \"fill.svg\"\n");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect id="bar" fill="{{token.color.x}}"/></svg>"#;
    let err = expect_err(load_files(
        &[("widget.toml", &m), ("fill.svg", svg)],
        &HashMap::new(),
    ));
    assert!(
        matches!(&err, BundleError::UnresolvedToken { token_key, .. } if token_key == "color.x")
    );
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_UNRESOLVED_TOKEN");
}
