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
    BundleScanResult, LoadedBundle, load_bundle_dir, load_bundle_dir_with_tokens,
};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn shipped_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/widget_bundles")
        .join(name)
}

/// Every token the shipped SVGs and manifests reference (Tonal Glass values).
fn shipped_tokens() -> HashMap<String, String> {
    tze_hud_config::themes::resolve_config_tokens(&Default::default())
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
        auto_clear_ms: None,
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

#[test]
fn dock_alternate_uses_the_shared_schema_without_changing_default_assets() {
    use tze_hud_widget::loader::validate_runtime_svg_registration;

    let default_manifest = manifest(
        "alternate-test",
        &format!(
            "{LEVEL_PARAM}[[layers]]\nsvg_file = 'default.svg'\n\
             [[layers.bindings]]\nparam = 'level'\ntarget_element = 'bar'\n\
             target_attribute = 'height'\nmapping = 'linear'\nattr_max = 20\n"
        ),
    );
    let default_svg = r#"<svg viewBox="0 0 360 124"><rect id="bar" height="20"/></svg>"#;
    let alternate = "[[dock.layers]]\nsvg_file = 'dock.svg'\n\
                     [[dock.layers.bindings]]\nparam = 'level'\ntarget_element = 'bar'\n\
                     target_attribute = 'height'\nmapping = 'linear'\nattr_max = 20\n";
    let dock_svg =
        r#"<svg viewBox="0 0 160 {{token.dock.row.height}}"><rect id="bar" height="20"/></svg>"#;
    let load = |extra: &str, svg: Option<&str>| {
        let text = format!("{default_manifest}{extra}");
        let mut files = vec![
            ("widget.toml", text.as_str()),
            ("default.svg", default_svg),
            ("other.svg", default_svg),
        ];
        if let Some(svg) = svg {
            files.push(("dock.svg", svg));
        }
        load_files(&files, &shipped_tokens())
    };
    let BundleScanResult::Ok(default) = load("", None) else {
        panic!("default fixture failed");
    };
    assert!(default.dock.is_none());
    let BundleScanResult::Ok(bundle) = load(alternate, Some(dock_svg)) else {
        panic!("alternate fixture failed");
    };
    assert_eq!(bundle.definition, default.definition);
    assert_eq!(
        bundle.svg_contents["default.svg"],
        default.svg_contents["default.svg"]
    );
    let dock = bundle.dock.as_ref().unwrap();
    assert_eq!((dock.width, dock.height), (160.0, 52.0));
    assert_eq!(
        dock.layers[0].bindings,
        bundle.definition.layers[0].bindings
    );
    assert!(dock.hover_behavior.is_none());
    assert!(String::from_utf8_lossy(&bundle.svg_contents["dock.svg"]).contains("160 52"));
    validate_runtime_svg_registration(
        &bundle.definition,
        "alternate-test",
        "default.svg",
        default_svg.as_bytes(),
        &shipped_tokens(),
    )
    .unwrap();
    assert!(matches!(
        validate_runtime_svg_registration(
            &bundle.definition,
            "alternate-test",
            "dock.svg",
            dock_svg.as_bytes(),
            &shipped_tokens(),
        ),
        Err(BundleError::BindingUnresolvable { .. })
    ));

    // These are alternate-only failures: default assets stay valid in every case.
    for (extra, svg, code) in [
        (alternate.to_owned(), None, "WIDGET_BUNDLE_MISSING_SVG"),
        (
            alternate.to_owned(),
            Some("not SVG"),
            "WIDGET_BUNDLE_SVG_PARSE_ERROR",
        ),
        (
            alternate.to_owned(),
            Some(r#"<svg viewBox="0 0 160 52"><rect id="other"/></svg>"#),
            "WIDGET_BINDING_UNRESOLVABLE",
        ),
        (
            alternate.replace("param = 'level'", "param = 'unknown'"),
            Some(dock_svg),
            "WIDGET_BINDING_UNRESOLVABLE",
        ),
        (
            alternate.replace("mapping = 'linear'", "mapping = 'discrete'"),
            Some(dock_svg),
            "WIDGET_BINDING_UNRESOLVABLE",
        ),
        (
            alternate.to_owned(),
            Some(r#"<svg viewBox="0 0 160 {{token.missing}}"><rect id="bar"/></svg>"#),
            "WIDGET_BUNDLE_UNRESOLVED_TOKEN",
        ),
        (
            alternate.replace("dock.svg", "default.svg"),
            Some(dock_svg),
            "WIDGET_BUNDLE_INVALID_MANIFEST",
        ),
        (
            "[dock]\nlayers = []\n".into(),
            None,
            "WIDGET_BUNDLE_INVALID_MANIFEST",
        ),
        (
            format!("{alternate}[[dock.parameter_schema]]\nname='second-schema'\n"),
            Some(dock_svg),
            "WIDGET_BUNDLE_INVALID_MANIFEST",
        ),
        (
            format!("{alternate}[[dock.layers]]\nsvg_file='other.svg'\n"),
            Some(dock_svg),
            "WIDGET_BUNDLE_INVALID_MANIFEST",
        ),
        (
            format!(
                "{alternate}[dock.hover_behavior]\nvisibility_param='unknown'\n\
                 [dock.hover_behavior.trigger_rect]\nx_pct=0\ny_pct=0\nwidth_pct=1\nheight_pct=1\n"
            ),
            Some(dock_svg),
            "WIDGET_BUNDLE_INVALID_MANIFEST",
        ),
    ] {
        assert_eq!(expect_err(load(&extra, svg)).wire_code(), code);
    }

    // Eligibility sees actual authored dimensions, never the resource estimate's
    // fallback, integer rounding or maximum-raster clamp.
    for (dimensions, expected) in [
        ("viewBox='-2 3 160 52.5'", Some((160.0, 52.5))),
        ("width='160px' height='52.5px'", Some((160.0, 52.5))),
        ("viewBox='0 0 160 100000'", Some((160.0, 100000.0))),
        ("viewBox='0 0 160 0'", None),
        ("viewBox='0 0 160 NaN'", None),
        ("viewBox='0 0 160 -1' width='160' height='52'", None),
        ("width='160' height='100%'", None),
        ("", None),
    ] {
        let svg = format!("<svg {dimensions}><rect id='bar'/></svg>");
        match (load(alternate, Some(&svg)), expected) {
            (BundleScanResult::Ok(bundle), Some(size)) => {
                let dock = bundle.dock.unwrap();
                assert_eq!((dock.width, dock.height), size);
                let errors = tze_hud_config::dock::DockConfig {
                    accepts: vec!["alternate-test".into()],
                    ..Default::default()
                }
                .validate_bundles(52.0, |_| Some(dock.height));
                assert_eq!(errors.is_empty(), dock.height <= 52.0 * (4.0 / 3.0));
            }
            (BundleScanResult::Err(BundleError::SvgParseError { .. }), None) => {}
            (result, expected) => panic!("{dimensions}: {result:?}, expected {expected:?}"),
        }
    }
}

#[test]
fn parameter_audience_defaults_and_invalid_values_are_explicit() {
    use tze_hud_scene::WidgetParamAudience;
    for (name, expected) in [
        (None, Some(WidgetParamAudience::Model)),
        (Some("model"), Some(WidgetParamAudience::Model)),
        (Some("owner"), Some(WidgetParamAudience::Owner)),
        (Some("runtime"), Some(WidgetParamAudience::Runtime)),
        (Some("everyone"), None),
    ] {
        let audience = name
            .map(|name| format!("audience = \"{name}\"\n"))
            .unwrap_or_default();
        let text = manifest("audience-test", &format!("{LEVEL_PARAM}{audience}"));
        let result = load_files(&[("widget.toml", &text)], &HashMap::new());
        match (result, expected) {
            (BundleScanResult::Ok(bundle), Some(expected)) => {
                assert_eq!(bundle.definition.parameter_schema[0].audience, expected)
            }
            (BundleScanResult::Err(error), None) => {
                let text = error.to_string();
                for required in ["audience-test", "level", "model", "owner", "runtime"] {
                    assert!(text.contains(required), "{text}");
                }
            }
            other => panic!("unexpected audience result: {other:?}"),
        }
    }
}

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
                ("level", "arc", "stroke-dashoffset"),
                ("level", "percent-value", "text-content"),
                ("fill_color", "arc", "stroke"),
                ("label", "label-text", "text-content"),
                ("severity", "severity-arc", "stroke"),
                ("severity", "card", "stroke"),
                ("tooltip_visible", "label-group", "opacity"),
                ("tooltip_visible", "tooltip-group", "opacity"),
                ("readout", "tooltip-readout", "text-content"),
            ],
        ),
        (
            "progress-bar",
            &[
                "progress",
                "label",
                "fill_color",
                "tooltip_visible",
                "severity",
            ],
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
        (
            "countdown",
            &["label", "ends_in_ms", "severity", "tooltip_visible"],
            &["card.svg"],
            &[
                ("ends_in_ms", "value", "text-content"),
                ("severity", "value", "fill"),
                ("tooltip_visible", "tooltip", "opacity"),
            ],
        ),
        (
            "elapsed",
            &["label", "started_ms_ago", "severity", "tooltip_visible"],
            &["card.svg"],
            &[
                ("started_ms_ago", "value", "text-content"),
                ("severity", "value", "fill"),
                ("tooltip_visible", "tooltip", "opacity"),
            ],
        ),
        (
            "kpi",
            &["label", "value", "trend", "severity", "tooltip_visible"],
            &["card.svg"],
            &[
                ("value", "value", "text-content"),
                ("trend", "trend-down", "opacity"),
                ("severity", "glyph-down", "stroke"),
            ],
        ),
        (
            "stepper",
            &["label", "step", "total", "state", "tooltip_visible"],
            &["card.svg"],
            &[
                ("step", "filled-2", "opacity"),
                ("total", "total-5", "opacity"),
                ("state", "current-a-2", "fill"),
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
    for name in [
        "gauge",
        "progress-bar",
        "status-indicator",
        "countdown",
        "elapsed",
        "kpi",
        "stepper",
    ] {
        let err = expect_err(load_bundle_dir(&shipped_path(name)));
        assert_eq!(err.wire_code(), "WIDGET_BUNDLE_UNRESOLVED_TOKEN", "{name}");
    }
}

/// The hover tooltips all reveal after the same short delay.
#[test]
fn shipped_hover_tooltips_share_the_gauge_delay() {
    for name in [
        "gauge",
        "progress-bar",
        "status-indicator",
        "countdown",
        "elapsed",
        "kpi",
        "stepper",
    ] {
        let hover = load_shipped(name)
            .definition
            .hover_behavior
            .unwrap_or_else(|| panic!("{name}: no hover behavior"));
        assert_eq!(hover.delay_ms, 180, "{name}");
        assert_eq!(hover.visibility_param, "tooltip_visible", "{name}");
    }
    let mut tokens = shipped_tokens();
    tokens.insert("status.hover.height_pct".into(), "0.25".into());
    let BundleScanResult::Ok(bundle) =
        load_bundle_dir_with_tokens(&shipped_path("status-indicator"), &tokens)
    else {
        panic!("owner token must resolve the status hover rectangle");
    };
    assert_eq!(
        bundle
            .definition
            .hover_behavior
            .unwrap()
            .trigger_rect
            .height_pct,
        0.25
    );
    for invalid in ["nan", "inf", "text", "1e99", "0", "-0.1", "1.1"] {
        tokens.insert("status.hover.height_pct".into(), invalid.into());
        assert!(
            matches!(
                load_bundle_dir_with_tokens(&shipped_path("status-indicator"), &tokens),
                BundleScanResult::Err(_)
            ),
            "invalid hover rectangle {invalid}"
        );
    }
}

/// Manifest color values name design tokens: discrete value maps and color
/// defaults resolve against the token map at load.
#[test]
fn shipped_manifest_styling_resolves_from_tokens() {
    let tokens = shipped_tokens();
    let gauge = load_shipped("gauge").definition;
    // (target, info role): severity colors the hairline and the arc.
    for (target, info) in [
        ("card", "color.outline.variant"),
        ("severity-arc", "color.primary"),
    ] {
        let severity = gauge
            .layers
            .iter()
            .flat_map(|l| &l.bindings)
            .find(|b| b.param == "severity" && b.target_element == target)
            .unwrap();
        let WidgetBindingMapping::Discrete { value_map } = &severity.mapping else {
            panic!("severity must be a discrete binding");
        };
        assert_eq!(value_map["info"], tokens[info], "{target}");
        assert_eq!(value_map["warning"], tokens["color.caution"], "{target}");
        assert_eq!(value_map["error"], tokens["color.error"], "{target}");
    }

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

    // Numeric style fields resolve once against the same owner token map as
    // SVG geometry, with finite/range validation before they reach rendering.
    let mut numeric = tokens.clone();
    numeric.insert("gauge.ring.circumference".into(), "111.25".into());
    numeric.insert("gauge.hover.delay_ms".into(), "371".into());
    let BundleScanResult::Ok(restyled) =
        load_bundle_dir_with_tokens(&shipped_path("gauge"), &numeric)
    else {
        panic!("numeric design tokens must load");
    };
    assert_eq!(restyled.definition.hover_behavior.unwrap().delay_ms, 371);
    for binding in restyled
        .definition
        .layers
        .iter()
        .flat_map(|l| &l.bindings)
        .filter(|b| b.param == "level" && b.target_attribute == "stroke-dashoffset")
    {
        assert_eq!(
            binding.mapping,
            WidgetBindingMapping::Linear {
                attr_min: 111.25,
                attr_max: 0.0
            }
        );
    }
    for (key, values) in [
        (
            "gauge.ring.circumference",
            &["nan", "inf", "red", "1e99"][..],
        ),
        (
            "gauge.hover.delay_ms",
            &["-1", "1.5", "4294967296", "nan"][..],
        ),
    ] {
        for value in values {
            let mut bad = tokens.clone();
            bad.insert(key.into(), (*value).into());
            let error = expect_err(load_bundle_dir_with_tokens(&shipped_path("gauge"), &bad));
            assert_eq!(
                error.wire_code(),
                "WIDGET_BUNDLE_INVALID_MANIFEST",
                "{key}={value}"
            );
        }
    }
    let mut missing = tokens.clone();
    missing.remove("gauge.ring.circumference");
    assert_eq!(
        expect_err(load_bundle_dir_with_tokens(
            &shipped_path("gauge"),
            &missing
        ))
        .wire_code(),
        "WIDGET_BUNDLE_UNRESOLVED_TOKEN"
    );

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
        ("busy", "#F2C14E"),
        ("offline", "#A9B1BE"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(value_map, &want);
}

/// Shipped SVGs and manifests take every color from design tokens.
#[test]
fn shipped_bundles_have_no_hardcoded_colors_or_restyled_opacity() {
    for (bundle, files) in [
        ("gauge", &["background.svg", "fill.svg", "widget.toml"][..]),
        (
            "progress-bar",
            &["track.svg", "fill.svg", "widget.toml"][..],
        ),
        ("status-indicator", &["indicator.svg", "widget.toml"][..]),
        ("countdown", &["card.svg", "widget.toml"][..]),
        ("elapsed", &["card.svg", "widget.toml"][..]),
        ("kpi", &["card.svg", "widget.toml"][..]),
        ("stepper", &["card.svg", "widget.toml"][..]),
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
            if matches!(bundle, "gauge" | "progress-bar" | "status-indicator")
                && file.ends_with(".svg")
            {
                for attribute in ["opacity", "fill-opacity", "stroke-opacity"] {
                    let marker = format!("{attribute}=\"");
                    for rest in text.split(&marker).skip(1) {
                        assert!(
                            rest.starts_with("{{token."),
                            "{bundle}/{file}: literal {attribute}"
                        );
                    }
                }
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

    let record = &scene.widget_registry.active_for_widget(w)[0];
    assert_eq!(
        record.params["label"],
        str_val("B"),
        "shipped bundle retains omitted model fields in its record"
    );
    let tab = scene.widget_registry.instances[w].tab_id;
    let occupancy = scene.widget_registry.get_occupancy(w, tab).unwrap();
    assert_eq!(occupancy.effective_params["label"], str_val("B"));
    assert_eq!(occupancy.effective_params["status"], enum_val("away"));
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

    let (results, warnings) = tze_hud_widget::loader::scan_bundle_dirs_with_diagnostics(
        &[
            root.path().to_path_buf(),
            root.path().join("private-missing-root"),
        ],
        &HashMap::new(),
    );
    assert_eq!(warnings.items().len(), 3);
    assert!(
        warnings
            .items()
            .iter()
            .any(|w| w.code == "WIDGET_BUNDLE_ROOT_UNREADABLE")
    );
    assert!(
        warnings
            .items()
            .iter()
            .all(|w| w.subject.as_deref().unwrap().starts_with("root:"))
    );
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

#[test]
fn deadline_defaults_and_threshold_bindings_admit_only_their_typed_contract() {
    use tze_hud_scene::{WidgetDeadline, WidgetParamType};
    for (value, accepted) in [
        ("0", true),
        ("86400000", true),
        ("-1", false),
        ("86400001", false),
        ("1.5", false),
        ("\"1000\"", false),
    ] {
        let source = manifest(
            "deadline",
            &format!(
                "[[parameter_schema]]\nname = \"ends_in_ms\"\ntype = \"deadline\"\ndefault = {value}\n"
            ),
        );
        match load_files(&[("widget.toml", &source)], &HashMap::new()) {
            BundleScanResult::Ok(bundle) if accepted => {
                let declaration = &bundle.definition.parameter_schema[0];
                assert_eq!(declaration.param_type, WidgetParamType::Deadline);
                assert_eq!(
                    declaration.default_value,
                    WidgetParameterValue::Deadline(WidgetDeadline::relative(
                        value.parse().unwrap()
                    ))
                );
            }
            BundleScanResult::Err(error) if !accepted => {
                assert!(error.to_string().contains("deadline<=24h"), "{error}")
            }
            other => panic!("{value}: {other:?}"),
        }
    }
    for (kind, threshold, outputs, attribute, accepted) in [
        ("f32", "2", "", "opacity", true),
        (
            "f32",
            "2",
            "attr_min = 1.0\nattr_max = 0.0\n",
            "opacity",
            true,
        ),
        ("f32", "nan", "", "opacity", false),
        ("f32", "2", "attr_max = 1.1\n", "opacity", false),
        ("f32", "2", "", "width", false),
        ("deadline", "2", "", "text-content", false),
    ] {
        let default = if kind == "f32" { "0.0" } else { "0" };
        let source = manifest(
            "threshold",
            &format!(
                "[[parameter_schema]]\nname = \"level\"\ntype = \"{kind}\"\ndefault = {default}\n[[layers]]\nsvg_file = \"test.svg\"\n[[layers.bindings]]\nparam = \"level\"\ntarget_element = \"bar\"\ntarget_attribute = \"{attribute}\"\nmapping = \"threshold\"\nthreshold = {threshold}\n{outputs}"
            ),
        );
        let result = load_files(
            &[("widget.toml", &source), ("test.svg", SVG)],
            &HashMap::new(),
        );
        assert_eq!(
            matches!(result, BundleScanResult::Ok(_)),
            accepted,
            "{source}: {result:?}"
        );
    }
}

/// Stepper counts are whole numbers: `2` and `2.0` are the same count, a
/// fraction is refused (never floored), and every refusal names the integer
/// bound. A step above total is still accepted (it fills every dot).
#[test]
fn stepper_counts_are_whole_numbers() {
    use tze_hud_scene::validation::ValidationError;
    for (param, value, accepted) in [
        ("step", 2.0, true),
        ("step", 0.0, true),
        ("step", 12.0, true),
        ("total", 1.0, true),
        ("step", 2.5, false),
        ("step", 2.000_001, false),
        ("step", -1.0, false),
        ("step", 13.0, false),
        ("total", 0.0, false),
        ("total", 4.5, false),
    ] {
        let mut scene = scene_with("stepper");
        let result = publish(
            &mut scene,
            "stepper",
            &[(param, WidgetParameterValue::F32(value))],
        );
        if accepted {
            assert!(result.is_ok(), "{param}={value}: {result:?}");
            continue;
        }
        let range = if param == "step" { "0..12" } else { "1..12" };
        match result {
            Err(ValidationError::WidgetParameterTypeMismatch { expected, .. }) => {
                assert_eq!(
                    expected,
                    format!("an integer in {range}"),
                    "{param}={value}"
                )
            }
            other => panic!("{param}={value}: {other:?}"),
        }
        assert!(
            scene
                .widget_registry
                .active_for_widget("stepper")
                .is_empty()
        );
    }
    let mut scene = scene_with("stepper");
    publish(
        &mut scene,
        "stepper",
        &[
            ("step", WidgetParameterValue::F32(7.0)),
            ("total", WidgetParameterValue::F32(5.0)),
        ],
    )
    .unwrap();
}

/// `f32_integer` needs an f32 with whole bounds and default; the load error
/// names the parameter.
#[test]
fn integer_constraint_needs_whole_bounds_and_default() {
    for (kind, default, bounds, accepted) in [
        ("f32", "2.0", "f32_min = 0.0\nf32_max = 12.0\n", true),
        ("f32", "2", "f32_min = 0\nf32_max = 12\n", true),
        ("f32", "2.5", "f32_min = 0.0\nf32_max = 12.0\n", false),
        ("f32", "2.0", "f32_min = 0.5\nf32_max = 12.0\n", false),
        ("f32", "2.0", "f32_min = 0.0\nf32_max = 11.5\n", false),
        ("string", "\"a\"", "", false),
    ] {
        let source = manifest(
            "counts",
            &format!(
                "[[parameter_schema]]\nname = \"count\"\ntype = \"{kind}\"\ndefault = {default}\n[parameter_schema.constraints]\n{bounds}f32_integer = true\n"
            ),
        );
        match load_files(&[("widget.toml", &source)], &HashMap::new()) {
            BundleScanResult::Ok(bundle) if accepted => {
                let c = bundle.definition.parameter_schema[0]
                    .constraints
                    .clone()
                    .unwrap();
                assert!(c.f32_integer);
            }
            BundleScanResult::Err(error) if !accepted => {
                let text = error.to_string();
                assert!(
                    text.contains("'count'") && text.contains("f32_integer"),
                    "{text}"
                );
            }
            other => panic!("{kind} {default} {bounds}: {other:?}"),
        }
    }
}

/// The elapsed bundle's start counts up; `deadline_counts` accepts only
/// "up" or "down", and only on a deadline.
#[test]
fn elapsed_counts_up_and_deadline_counts_is_checked() {
    use tze_hud_scene::WidgetParamType;
    let bundle = load_shipped("elapsed");
    let started = bundle
        .definition
        .parameter_schema
        .iter()
        .find(|p| p.name == "started_ms_ago")
        .unwrap();
    assert_eq!(started.param_type, WidgetParamType::Deadline);
    assert!(started.deadline_counts_up());
    assert!(started.value_hint().starts_with("elapsed<=24h"));
    for (kind, default, counts, accepted) in [
        ("deadline", "0", "up", true),
        ("deadline", "0", "down", true),
        ("deadline", "0", "sideways", false),
        ("f32", "0.0", "up", false),
    ] {
        let source = manifest(
            "timer",
            &format!(
                "[[parameter_schema]]\nname = \"since\"\ntype = \"{kind}\"\ndefault = {default}\n[parameter_schema.constraints]\ndeadline_counts = \"{counts}\"\n"
            ),
        );
        let result = load_files(&[("widget.toml", &source)], &HashMap::new());
        assert_eq!(
            matches!(result, BundleScanResult::Ok(_)),
            accepted,
            "{source}: {result:?}"
        );
        if let BundleScanResult::Err(error) = result {
            assert!(error.to_string().contains("'since'"), "{error}");
        }
    }
}
