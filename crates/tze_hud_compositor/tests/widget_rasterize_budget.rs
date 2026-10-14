//! Widget SVG rasterization correctness through the pure CPU path.
//!
//! The gauge fixtures check nonempty pixels and applied fill/text bindings.
//! These checks require no GPU device and make no wall-clock or upload claim.

use std::collections::HashMap;

use tze_hud_compositor::widget::rasterize_svg_layers;
use tze_hud_scene::types::{Rgba, WidgetBinding, WidgetBindingMapping, WidgetParameterValue};

// ─── Reference gauge SVG fixtures ────────────────────────────────────────────

const GAUGE_BACKGROUND_SVG: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 220" width="100" height="220">
  <rect id="frame" x="0" y="0" width="100" height="220" rx="6" ry="6"
        fill="#1a1a2e" stroke="#4a4a6a" stroke-width="2"/>
  <rect id="track" x="30" y="10" width="40" height="200" rx="3" ry="3"
        fill="#2a2a3e" stroke="#3a3a5a" stroke-width="1"/>
</svg>"##;

const GAUGE_FILL_SVG: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 220" width="100" height="220">
  <rect id="bar" x="30" y="10" width="40" height="0"
        fill="#00b4ff" rx="3" ry="3"/>
  <text id="label-text" x="50" y="215" text-anchor="middle"
        font-family="sans-serif" font-size="10" fill="#cccccc"></text>
  <circle id="indicator" cx="85" cy="15" r="6" fill="#00cc66"/>
</svg>"##;

// ─── Test helpers ─────────────────────────────────────────────────────────────

fn gauge_fill_bindings() -> Vec<WidgetBinding> {
    vec![
        WidgetBinding {
            param: "level".to_string(),
            target_element: "bar".to_string(),
            target_attribute: "height".to_string(),
            mapping: WidgetBindingMapping::Linear {
                attr_min: 0.0,
                attr_max: 200.0,
            },
        },
        WidgetBinding {
            param: "fill_color".to_string(),
            target_element: "bar".to_string(),
            target_attribute: "fill".to_string(),
            mapping: WidgetBindingMapping::Direct,
        },
        WidgetBinding {
            param: "label".to_string(),
            target_element: "label-text".to_string(),
            target_attribute: "text-content".to_string(),
            mapping: WidgetBindingMapping::Direct,
        },
        WidgetBinding {
            param: "severity".to_string(),
            target_element: "indicator".to_string(),
            target_attribute: "fill".to_string(),
            mapping: WidgetBindingMapping::Discrete {
                value_map: [
                    ("info".to_string(), "#00cc66".to_string()),
                    ("warning".to_string(), "#ffcc00".to_string()),
                    ("error".to_string(), "#ff3300".to_string()),
                ]
                .into_iter()
                .collect(),
            },
        },
    ]
}

fn gauge_params() -> HashMap<String, WidgetParameterValue> {
    [
        ("level".to_string(), WidgetParameterValue::F32(0.75)),
        (
            "fill_color".to_string(),
            WidgetParameterValue::Color(Rgba::new(0.0, 0.706, 1.0, 1.0)),
        ),
        (
            "label".to_string(),
            WidgetParameterValue::String("75%".to_string()),
        ),
        (
            "severity".to_string(),
            WidgetParameterValue::Enum("info".to_string()),
        ),
    ]
    .into_iter()
    .collect()
}

fn gauge_param_constraints() -> HashMap<String, (f32, f32)> {
    [("level".to_string(), (0.0f32, 1.0f32))]
        .into_iter()
        .collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// Verify rasterization produces non-trivial pixel output for the gauge fixture.
///
/// The background layer should fill the frame with a dark color — checks that
/// actual rendering occurred rather than an empty/zeroed pixmap.
#[test]
fn gauge_512x512_rasterize_produces_non_empty_pixels() {
    let params = gauge_params();
    let constraints = gauge_param_constraints();

    // Background layer only (no bindings).
    let layers: Vec<(&str, &[WidgetBinding])> = vec![(GAUGE_BACKGROUND_SVG, &[])];

    let pixmap = rasterize_svg_layers(&layers, &constraints, &params, 512, 512)
        .expect("rasterize_svg_layers must succeed for gauge background");

    // The background SVG fills with #1a1a2e — at least some pixels should be non-zero.
    let non_zero = pixmap.data().iter().any(|&b| b != 0);
    assert!(
        non_zero,
        "rasterized pixmap must contain non-zero pixels for the gauge background"
    );
}

/// Verify parameter bindings are applied: the fill bar height should reflect
/// the 'level' parameter via linear mapping.
///
/// At level=1.0, `height` should be 200 (full bar), so the fill layer produces
/// different pixels than at level=0.0 (empty bar).
#[test]
fn gauge_fill_bindings_are_applied() {
    let constraints = gauge_param_constraints();
    let bindings = gauge_fill_bindings();
    let layers: Vec<(&str, &[WidgetBinding])> = vec![(GAUGE_FILL_SVG, &bindings)];

    // level = 0.0 → bar height 0 (empty)
    let mut params_empty = gauge_params();
    params_empty.insert("level".to_string(), WidgetParameterValue::F32(0.0));
    let pixmap_empty = rasterize_svg_layers(&layers, &constraints, &params_empty, 512, 512)
        .expect("rasterize must succeed at level=0.0");

    // level = 1.0 → bar height 200 (full)
    let mut params_full = gauge_params();
    params_full.insert("level".to_string(), WidgetParameterValue::F32(1.0));
    let pixmap_full = rasterize_svg_layers(&layers, &constraints, &params_full, 512, 512)
        .expect("rasterize must succeed at level=1.0");

    // Pixmaps at different fill levels must differ.
    assert_ne!(
        pixmap_empty.data(),
        pixmap_full.data(),
        "fill pixmaps at level=0.0 and level=1.0 must differ (binding must be applied)"
    );
}

/// Verify direct `text-content` binding actually affects raster output.
///
/// This guards against regressions where SVG `<text>` rendering is disabled
/// (for example, missing resvg text/system-font features or empty fontdb).
#[test]
fn gauge_label_text_binding_changes_raster_output() {
    let constraints = gauge_param_constraints();
    let bindings = gauge_fill_bindings();
    let layers: Vec<(&str, &[WidgetBinding])> = vec![(GAUGE_FILL_SVG, &bindings)];

    let mut params_no_label = gauge_params();
    params_no_label.insert(
        "label".to_string(),
        WidgetParameterValue::String(String::new()),
    );
    let pixmap_no_label = rasterize_svg_layers(&layers, &constraints, &params_no_label, 256, 256)
        .expect("rasterize must succeed with empty label");

    let mut params_with_label = gauge_params();
    params_with_label.insert(
        "label".to_string(),
        WidgetParameterValue::String("ONLINE".to_string()),
    );
    let pixmap_with_label =
        rasterize_svg_layers(&layers, &constraints, &params_with_label, 256, 256)
            .expect("rasterize must succeed with non-empty label");

    if pixmap_no_label.data() == pixmap_with_label.data() {
        // CI images can lack a usable sans-serif system font. In that environment
        // text-content bindings cannot render glyphs, so this assertion would be
        // a false negative unrelated to widget binding logic.
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        let query = resvg::usvg::fontdb::Query {
            families: &[resvg::usvg::fontdb::Family::SansSerif],
            ..Default::default()
        };
        if db.query(&query).is_none() {
            eprintln!(
                "skipping text-content raster delta assertion: no sans-serif system font detected"
            );
            return;
        }
    }

    assert_ne!(
        pixmap_no_label.data(),
        pixmap_with_label.data(),
        "non-empty label must change raster output; if identical, text rendering is broken"
    );
}
