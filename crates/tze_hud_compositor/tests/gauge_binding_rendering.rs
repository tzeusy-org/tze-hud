//! The shipped gauge’s bindings and rendered behavior.

use std::collections::HashMap;

use tze_hud_compositor::widget::{apply_svg_attribute, resolve_binding_value};
use tze_hud_scene::types::{Rgba, WidgetBinding, WidgetParameterValue};
use tze_hud_widget::loader::{BundleScanResult, LoadedBundle};

use crate::common::{tokens, widget_defaults};

/// The shipped bundle's own layers, token-resolved, as the compositor
/// rasterizes them, with the definition's defaults as the base params.
fn shipped_ring() -> LoadedBundle {
    let loaded = bundle!("gauge", ["background.svg", "fill.svg"]);
    let BundleScanResult::Ok(bundle) = loaded else {
        panic!("shipped gauge: {loaded:?}");
    };
    bundle
}

fn rgb(hex: &str) -> [u8; 3] {
    let v = u32::from_str_radix(&hex[1..7], 16).unwrap();
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// Rasterize the ring 1:1 (128x132 viewBox) with `overrides` on the defaults.
fn render_ring(
    bundle: &LoadedBundle,
    overrides: &[(&str, WidgetParameterValue)],
) -> tiny_skia::Pixmap {
    render_ring_at(bundle, overrides, (128, 132))
}

fn render_ring_at(
    bundle: &LoadedBundle,
    overrides: &[(&str, WidgetParameterValue)],
    (width, height): (u32, u32),
) -> tiny_skia::Pixmap {
    let mut params = widget_defaults(&bundle.definition);
    params.extend(
        overrides
            .iter()
            .map(|(name, value)| ((*name).into(), value.clone())),
    );
    let refs: Vec<(&str, &[WidgetBinding])> = bundle
        .definition
        .layers
        .iter()
        .map(|layer| {
            (
                std::str::from_utf8(&bundle.svg_contents[&layer.svg_file]).unwrap(),
                layer.bindings.as_slice(),
            )
        })
        .collect();
    let constraints = bundle
        .definition
        .parameter_schema
        .iter()
        .filter_map(|p| {
            let c = p.constraints.as_ref()?;
            Some((p.name.clone(), (c.f32_min?, c.f32_max?)))
        })
        .collect();
    tze_hud_compositor::widget::rasterize_svg_layers(&refs, &constraints, &params, width, height)
        .expect("shipped gauge rasterizes")
}

fn pixel_rgb(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 3] {
    let pixel = pixmap.pixel(x, y).unwrap().demultiply();
    [pixel.red(), pixel.green(), pixel.blue()]
}

/// Fraction of the ring, sampled clockwise from 12 o'clock at the stroke's
/// centre line, drawn in `color` (within 8 per channel).
fn arc_coverage(pixmap: &tiny_skia::Pixmap, color: [u8; 3]) -> f32 {
    const SAMPLES: usize = 360;
    let hits = (0..SAMPLES)
        .filter(|i| {
            let a = (*i as f32 + 0.5) / SAMPLES as f32 * std::f32::consts::TAU;
            let (x, y) = (64.0 + 34.0 * a.sin(), 56.0 - 34.0 * a.cos());
            pixel_rgb(pixmap, x as u32, y as u32)
                .iter()
                .zip(color)
                .all(|(a, b)| a.abs_diff(b) <= 8)
        })
        .count();
    hits as f32 / SAMPLES as f32
}

fn region_differs(
    a: &tiny_skia::Pixmap,
    b: &tiny_skia::Pixmap,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> bool {
    (y..y + h).any(|py| (x..x + w).any(|px| a.pixel(px, py) != b.pixel(px, py)))
}

/// The arc covers the level's share of the ring, clockwise from 12
/// o'clock (round caps add up to ~3% at each end); level 0 draws none.
#[test]
fn shipped_gauge_ring_raster_contract() {
    let tokens = tokens();
    let bundle = shipped_ring();
    let scaled = render_ring_at(&bundle, &[], (512, 512));
    assert_eq!((scaled.width(), scaled.height()), (512, 512));
    assert!(scaled.data().iter().any(|&byte| byte != 0));
    shipped_gauge_ring_arc_tracks_level(&bundle, rgb(&tokens["color.primary"]));
    shipped_gauge_ring_colors_by_severity(&bundle, &tokens);
    shipped_gauge_ring_percent_label_and_hover_readout(&bundle);
}

#[test]
fn shipped_gauge_resolves_levels_and_applies_direct_label_and_color_bindings() {
    let bundle = shipped_ring();
    let layer = bundle
        .definition
        .layers
        .iter()
        .find(|layer| layer.svg_file == "fill.svg")
        .unwrap();
    let source = std::str::from_utf8(&bundle.svg_contents[&layer.svg_file]).unwrap();
    let binding = |element: &str, attribute: &str| {
        layer
            .bindings
            .iter()
            .find(|binding| {
                binding.target_element == element && binding.target_attribute == attribute
            })
            .unwrap()
    };
    let resolve = |binding: &WidgetBinding, value| {
        let params = [(binding.param.clone(), value)].into();
        resolve_binding_value(binding, &params, &HashMap::new()).unwrap()
    };

    let percent = binding("percent-value", "text-content");
    for (level, expected) in [(0.0, "0"), (0.25, "25"), (0.5, "50"), (1.0, "100")] {
        assert_eq!(resolve(percent, WidgetParameterValue::F32(level)), expected);
    }

    let label = binding("label-text", "text-content");
    let mut labeled = source.to_owned();
    for expected in ["CPU Load", "Memory Usage 87%", ""] {
        let value = resolve(label, WidgetParameterValue::String(expected.into()));
        assert_eq!(value, expected);
        labeled = apply_svg_attribute(&labeled, "label-text", "text-content", &value);
        let (_, tag) = labeled.split_once(r#"id="label-text""#).unwrap();
        let (_, content) = tag.split_once('>').unwrap();
        assert_eq!(content.split_once("</text>").unwrap().0, expected);
    }

    let stroke = binding("arc", "stroke");
    for (color, expected) in [
        (Rgba::new(1.0, 0.0, 0.0, 1.0), "#ff0000"),
        (Rgba::new(0.0, 0.0, 1.0, 1.0), "#0000ff"),
        (
            Rgba::new(0.0, 1.0, 0.0, 128.0 / 255.0),
            "rgba(0,255,0,0.502)",
        ),
    ] {
        let value = resolve(stroke, WidgetParameterValue::Color(color));
        assert_eq!(value, expected);
        let svg = apply_svg_attribute(source, "arc", "stroke", &value);
        let (_, circle) = svg.split_once(r#"id="arc""#).unwrap();
        let (attributes, _) = circle.split_once('>').unwrap();
        assert!(attributes.contains(&format!(r#"stroke="{expected}""#)));
    }
}

fn shipped_gauge_ring_arc_tracks_level(bundle: &LoadedBundle, primary: [u8; 3]) {
    let mut last = -1.0;
    for level in [0.0f32, 0.1, 0.25, 0.5, 0.72, 0.95, 1.0] {
        let pixmap = render_ring(bundle, &[("level", WidgetParameterValue::F32(level))]);
        let covered = arc_coverage(&pixmap, primary);
        assert!(
            (covered - level).abs() <= 0.04,
            "level {level}: arc covers {covered} of the ring"
        );
        if level == 0.0 || level == 1.0 {
            assert_eq!(
                covered, level,
                "empty/full endpoints cover exactly their share"
            );
        }
        // A quarter fills the top-right quadrant, clockwise from 12 o'clock.
        if level == 0.25 {
            assert_eq!(pixel_rgb(&pixmap, 88, 32), primary);
            assert_ne!(pixel_rgb(&pixmap, 40, 32), primary);
        }
        assert!(covered >= last, "arc coverage must grow with level");
        last = covered;
    }
}

/// Info uses the owner's fill_color (default color.primary); warning and
/// error use their roles regardless of fill_color, on the arc and on the
/// card's hairline (info: color.outline.variant).
fn shipped_gauge_ring_colors_by_severity(bundle: &LoadedBundle, tokens: &HashMap<String, String>) {
    let orange = WidgetParameterValue::Color(Rgba::new(1.0, 165.0 / 255.0, 0.0, 1.0));
    let caution = rgb(&tokens["color.caution"]);
    let error = rgb(&tokens["color.error"]);
    for (severity, expected, edge) in [
        ("info", [255, 165, 0], rgb(&tokens["color.outline.variant"])),
        ("warning", caution, caution),
        ("error", error, error),
    ] {
        let pixmap = render_ring(
            bundle,
            &[
                ("level", WidgetParameterValue::F32(0.5)),
                ("fill_color", orange.clone()),
                ("severity", WidgetParameterValue::Enum(severity.into())),
            ],
        );
        let covered = arc_coverage(&pixmap, expected);
        assert!(
            (covered - 0.5).abs() <= 0.04,
            "{severity}: arc in {expected:?} covers {covered}"
        );
        // The card's top hairline (y 0.5, 1 wide) carries the severity too.
        assert_eq!(pixel_rgb(&pixmap, 64, 0), edge, "{severity}: hairline");
    }
}

/// The centre shows the level as a percent; the label sits below the
/// ring, and hover swaps it for the readout over a state layer.
fn shipped_gauge_ring_percent_label_and_hover_readout(bundle: &LoadedBundle) {
    let render = |overrides: &[(&str, WidgetParameterValue)]| render_ring(bundle, overrides);
    let label = |s: &str| ("label", WidgetParameterValue::String(s.into()));
    let readout = (
        "readout",
        WidgetParameterValue::String("42/100 (42%)".into()),
    );
    let hover = |v: f32| ("tooltip_visible", WidgetParameterValue::F32(v));

    // Percent: inside the ring (x 36..92, y 46..66) only.
    let a = render(&[label("Build")]);
    let b = render(&[label("Build"), readout.clone()]);
    let c = render(&[("level", WidgetParameterValue::F32(0.07)), label("Build")]);
    assert!(region_differs(&a, &c, 36, 46, 56, 20));
    assert!(
        !region_differs(&a, &c, 0, 104, 128, 16),
        "level leaves the caption alone"
    );

    // Label in the caption row; the readout is hidden until hover.
    assert!(region_differs(&a, &render(&[]), 8, 104, 112, 16));
    assert!(a.data() == b.data(), "readout is hover-only");

    // Hover: the readout replaces the label and the card lifts.
    let hovered = render(&[label("Build"), readout, hover(1.0)]);
    assert!(region_differs(&b, &hovered, 8, 104, 112, 16));
    assert!(
        region_differs(&b, &hovered, 4, 4, 8, 8),
        "state layer covers the card"
    );
    let hover_label_only = render(&[label("Build"), hover(1.0)]);
    let hover_nothing = render(&[hover(1.0)]);
    assert!(
        !region_differs(&hover_label_only, &hover_nothing, 0, 104, 128, 16),
        "hover hides the label"
    );
}
