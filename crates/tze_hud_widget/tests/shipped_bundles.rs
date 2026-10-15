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

/// Every token the shipped SVGs and manifests reference (Tonal Glass values).
fn shipped_tokens() -> HashMap<String, String> {
    [
        ("color.surface.container.high", "#222833"),
        ("color.surface.container.highest", "#2A313D"),
        ("color.on_surface", "#E8EBF0"),
        ("color.on_surface.variant", "#A9B1BE"),
        ("color.outline", "#5A6475"),
        ("color.outline.variant", "#343B47"),
        ("color.primary", "#8AB4FF"),
        ("color.success", "#7BD88F"),
        ("color.caution", "#F2C14E"),
        ("color.error", "#FF8A80"),
        ("opacity.surface", "0.92"),
        ("state.hover.opacity", "0.08"),
        ("stroke.border.width", "1"),
        ("shape.s", "8"),
        ("shape.m", "12"),
        ("type.title.m.family", "IBM Plex Sans"),
        ("type.title.m.size", "16"),
        ("type.title.m.weight", "600"),
        ("type.body.m.family", "IBM Plex Sans"),
        ("type.body.m.size", "14"),
        ("type.readout.family", "IBM Plex Mono"),
        ("type.readout.size", "14"),
        ("type.readout.weight", "500"),
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
                ("severity", "indicator", "stroke"),
                ("tooltip_visible", "tooltip-group", "opacity"),
                ("readout", "tooltip-readout", "text-content"),
            ],
        ),
        (
            "progress-bar",
            &["progress", "label", "fill_color", "tooltip_visible"],
            &["track.svg", "fill.svg"],
            &[
                ("progress", "fill-bar", "width"),
                ("progress", "readout-value", "text-content"),
                ("fill_color", "fill-bar", "fill"),
                ("label", "label-text", "text-content"),
                ("tooltip_visible", "tooltip-group", "opacity"),
            ],
        ),
        (
            "status-indicator",
            &["status", "theme", "label", "reason", "tooltip_visible"],
            &["indicator.svg"],
            &[
                ("status", "system-fill", "fill"),
                ("theme", "system-group", "opacity"),
                ("label", "tooltip-label", "text-content"),
                ("reason", "tooltip-reason", "text-content"),
                ("tooltip_visible", "tooltip-group", "opacity"),
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

/// The hover tooltips all reveal after the same short delay.
#[test]
fn shipped_hover_tooltips_share_the_gauge_delay() {
    for name in ["gauge", "progress-bar", "status-indicator"] {
        let hover = load_shipped(name)
            .definition
            .hover_behavior
            .unwrap_or_else(|| panic!("{name}: no hover behavior"));
        assert_eq!(hover.delay_ms, 180, "{name}");
        assert_eq!(hover.visibility_param, "tooltip_visible", "{name}");
    }
}

/// Manifest color values name design tokens: discrete value maps and color
/// defaults resolve against the token map at load.
#[test]
fn shipped_manifest_colors_resolve_from_tokens() {
    let tokens = shipped_tokens();
    let gauge = load_shipped("gauge").definition;
    let severity = gauge
        .layers
        .iter()
        .flat_map(|l| &l.bindings)
        .find(|b| b.param == "severity")
        .unwrap();
    let WidgetBindingMapping::Discrete { value_map } = &severity.mapping else {
        panic!("severity must be a discrete binding");
    };
    assert_eq!(value_map["info"], tokens["color.outline.variant"]);
    assert_eq!(value_map["warning"], tokens["color.caution"]);
    assert_eq!(value_map["error"], tokens["color.error"]);

    for name in ["gauge", "progress-bar"] {
        let fill = load_shipped(name)
            .definition
            .parameter_schema
            .into_iter()
            .find(|p| p.name == "fill_color")
            .unwrap();
        assert_eq!(
            fill.default_value,
            WidgetParameterValue::Color(tze_hud_scene::Rgba::new(
                138.0 / 255.0,
                180.0 / 255.0,
                1.0,
                1.0
            )),
            "{name}: fill_color defaults to color.primary"
        );
    }

    let mut other = tokens.clone();
    other.insert("color.primary".into(), "#00FF0080".into());
    let BundleScanResult::Ok(bundle) = load_bundle_dir_with_tokens(&shipped_path("gauge"), &other)
    else {
        panic!("gauge failed to load with another primary");
    };
    let fill = bundle
        .definition
        .parameter_schema
        .into_iter()
        .find(|p| p.name == "fill_color")
        .unwrap();
    assert_eq!(
        fill.default_value,
        WidgetParameterValue::Color(tze_hud_scene::Rgba::new(0.0, 1.0, 0.0, 128.0 / 255.0))
    );
}

#[test]
fn status_indicator_status_maps_each_value_to_its_color() {
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
    let want: std::collections::BTreeMap<String, String> = [
        ("online", "#7BD88F"),
        ("away", "#F2C14E"),
        ("busy", "#FF8A80"),
        ("offline", "#5A6475"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(value_map, &want);
}

/// Shipped SVGs and manifests take every color from design tokens.
#[test]
fn shipped_bundles_have_no_hardcoded_hex_colors() {
    for (bundle, files) in [
        ("gauge", &["background.svg", "fill.svg", "widget.toml"][..]),
        (
            "progress-bar",
            &["track.svg", "fill.svg", "widget.toml"][..],
        ),
        ("status-indicator", &["indicator.svg", "widget.toml"][..]),
    ] {
        for file in files {
            let raw = std::fs::read_to_string(shipped_path(bundle).join(file)).unwrap();
            // Drop XML and TOML comments; they may mention colors in prose.
            let mut text = String::new();
            let mut rest = raw.as_str();
            while let Some(i) = rest.find("<!--") {
                text.push_str(&rest[..i]);
                rest = rest[i..].split_once("-->").map_or("", |(_, r)| r);
            }
            text.push_str(rest);
            if file.ends_with(".toml") {
                text = text
                    .lines()
                    .filter(|line| !line.trim_start().starts_with('#'))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            for (i, _) in text.match_indices('#') {
                let digits = text[i + 1..]
                    .chars()
                    .take_while(char::is_ascii_hexdigit)
                    .count();
                assert!(
                    !(3..=8).contains(&digits),
                    "{bundle}/{file}: hardcoded hex literal near {:?}",
                    &text[i..(i + 1 + digits)]
                );
            }
        }
    }
}

// ─── Publishing to shipped widgets ────────────────────────────────────────────

#[test]
fn shipped_widgets_reject_out_of_range_f32() {
    for (widget, param, published, accepted) in [
        ("gauge", "level", 1.5, false),
        ("gauge", "level", -0.5, false),
        ("gauge", "level", 0.4, true),
        ("gauge", "level", 0.0, true),
        ("gauge", "level", 1.0, true),
        ("progress-bar", "progress", 2.0, false),
    ] {
        let mut scene = scene_with(widget);
        let result = publish(
            &mut scene,
            widget,
            &[(param, WidgetParameterValue::F32(published))],
        );
        assert_eq!(
            result.is_ok(),
            accepted,
            "{widget}.{param}={published}: {result:?}"
        );
        let pubs = scene.widget_registry.active_for_widget(widget);
        if !accepted {
            assert!(pubs.is_empty());
            continue;
        }
        match pubs[0].params.get(param) {
            Some(WidgetParameterValue::F32(got)) => {
                assert!(
                    (got - published).abs() < 1e-6,
                    "{widget}.{param}={published}: stored {got}"
                )
            }
            other => panic!("{widget}.{param}: {other:?}"),
        }
        let color = scene.widget_registry.definitions[widget]
            .parameter_schema
            .iter()
            .find(|p| p.name == "fill_color")
            .unwrap()
            .default_value
            .clone();
        assert_eq!(
            color,
            WidgetParameterValue::Color(tze_hud_scene::Rgba::new(
                138.0 / 255.0,
                180.0 / 255.0,
                1.0,
                1.0
            )),
            "token color default must be normalized"
        );
        publish(&mut scene, widget, &[("fill_color", color.clone())]).unwrap();
        assert_eq!(
            scene.widget_registry.instances[widget].current_params["fill_color"],
            color
        );
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

    // Manifest placeholders (a color default, a discrete value) fail the same way.
    let color_default = manifest(
        "t",
        "[[parameter_schema]]\nname = \"c\"\ntype = \"color\"\ndefault = \"{{token.color.y}}\"\n\
         [[layers]]\nsvg_file = \"fill.svg\"\n",
    );
    let value_map = manifest(
        "t",
        "[[parameter_schema]]\nname = \"sev\"\ntype = \"enum\"\ndefault = \"info\"\n\
         [parameter_schema.constraints]\nenum_allowed_values = [\"info\"]\n\n\
         [[layers]]\nsvg_file = \"fill.svg\"\n\n[[layers.bindings]]\nparam = \"sev\"\n\
         target_element = \"bar\"\ntarget_attribute = \"fill\"\nmapping = \"discrete\"\n\n\
         [layers.bindings.value_map]\ninfo = \"{{token.color.y}}\"\n",
    );
    for m in [color_default, value_map] {
        let err = expect_err(load_files(
            &[("widget.toml", &m), ("fill.svg", SVG)],
            &HashMap::new(),
        ));
        assert!(
            matches!(&err, BundleError::UnresolvedToken { token_key, svg_file, .. }
                if token_key == "color.y" && svg_file == "widget.toml"),
            "{err:?}"
        );
    }
    // A color default that resolves to something other than #RRGGBB[AA].
    let not_a_color = manifest(
        "t",
        "[[parameter_schema]]\nname = \"c\"\ntype = \"color\"\ndefault = \"blue\"\n\
         [[layers]]\nsvg_file = \"fill.svg\"\n",
    );
    let err = expect_err(load_files(
        &[("widget.toml", &not_a_color), ("fill.svg", SVG)],
        &HashMap::new(),
    ));
    assert_eq!(err.wire_code(), "WIDGET_BUNDLE_INVALID_MANIFEST");
}
