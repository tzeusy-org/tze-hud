//! Built-in dock countdown only. Custom SVGs never enter this display layout.
//!
//! All fitting probes and the final pixels use the same widget fonts/usvg
//! shaper. The scene retains the full label and the shared absolute deadline.

use super::{
    HashMap, WidgetBinding, WidgetParameterValue, apply_svg_attribute, bound_widget_svg,
    escape_text, find_start_tag, parse_f32_attr, parse_tag_attrs, widget_usvg_options,
};
use resvg::usvg::tiny_skia_path::Rect;
use unicode_segmentation::UnicodeSegmentation;

pub(super) struct PreparedCountdown {
    pub svg: String,
    pub clipped: bool,
    pub prefix_chars: usize,
}

#[derive(Clone, Copy)]
struct Line {
    top: f32,
    height: f32,
}

/// Shape a fixed font-metric probe, not the varying label's ink. This keeps
/// baselines still as digits and labels change, including font fallback.
fn baseline(svg: &str, id: &str, line: Line) -> Option<f32> {
    let probe = apply_svg_attribute(svg, id, "text-content", "Mg");
    let (metrics, _) = text_bounds(&probe, id)?;
    Some(line.top + (line.height - metrics.height()) * 0.5 - metrics.top())
}

// SVG's metric/advance box is deliberately separate from flattened glyph ink.
fn text_bounds(svg: &str, id: &str) -> Option<(Rect, Rect)> {
    let tree = resvg::usvg::Tree::from_str(svg, &widget_usvg_options()).ok()?;
    let resvg::usvg::Node::Text(text) = tree.node_by_id(id)? else {
        return None;
    };
    // usvg's flattened glyph group remains in text-local coordinates; apply
    // the text's absolute transform once to compare ink and advance on canvas.
    let ink = text
        .flattened()
        .bounding_box()
        .transform(text.abs_transform())?;
    Some((text.abs_bounding_box(), ink))
}

#[cfg(test)]
fn ink_bounds(svg: &str, id: &str) -> Option<Rect> {
    text_bounds(svg, id).map(|(_, ink)| ink)
}

fn fits_label(svg: &str, label: &str, inset: f32, right: f32, line: Line) -> bool {
    // XML/CSS collapses these characters to spaces, then drops an empty text
    // run. No glyph node is expected; absence must not invent an ellipsis.
    if label.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '\n')) {
        return true;
    }
    let probe = apply_svg_attribute(svg, "label", "text-content", &escape_text(label));
    let Some((metrics, bounds)) = text_bounds(&probe, "label") else {
        return false;
    };
    // Ink, not the canvas-clipped raster: an overhanging or accented glyph
    // cannot appear to fit just because the texture edge cut it away.
    metrics.right() <= right
        && bounds.left() >= inset
        && bounds.right() <= right
        && bounds.top() >= line.top
        && bounds.bottom() <= line.top + line.height
}

/// The visible label, its prefix length and whether it was clipped; `None`
/// when not even an ellipsis fits, so a misfit is not searched every tick.
type LabelFit = Option<(String, usize, bool)>;

/// Bounded: a few dock countdowns, each with one label at a time.
const LABEL_FIT_CAPACITY: usize = 64;

thread_local! {
    static LABEL_FITS: std::cell::RefCell<HashMap<(u64, String), LabelFit>> =
        std::cell::RefCell::new(HashMap::new());
}

/// The shaping inputs: the widget font set and the resolved template.
fn fit_key(svg: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    super::widget_fonts().id.hash(&mut hasher);
    svg.hash(&mut hasher);
    hasher.finish()
}

fn remember_fit(key: (u64, String), fit: LabelFit) -> LabelFit {
    LABEL_FITS.with(|fits| {
        let mut fits = fits.borrow_mut();
        if fits.len() >= LABEL_FIT_CAPACITY {
            fits.clear();
        }
        fits.insert(key, fit.clone());
    });
    fit
}

/// The dock artwork's own size and line metrics (dock.svg), validated so the
/// two text lines fit inside it.
struct Geometry {
    inset: f32,
    body_height: f32,
    readout_height: f32,
    width: f32,
    height: f32,
}

fn geometry(svg: &str) -> Option<Geometry> {
    let attrs = parse_tag_attrs(find_start_tag(svg, "svg")?);
    let get = |name| parse_f32_attr(&attrs, name);
    let g = Geometry {
        inset: get("data-inset")?,
        body_height: get("data-body-line-height")?,
        readout_height: get("data-readout-line-height")?,
        width: get("width")?,
        height: get("height")?,
    };
    let positive = [g.inset, g.body_height, g.readout_height, g.width, g.height]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0);
    // A warning readout reserves one line-height for its mark inside the insets.
    (positive
        && g.inset * 2.0 + g.readout_height < g.width
        && g.body_height + g.readout_height <= g.height)
        .then_some(g)
}

/// Whether the resolved dock countdown draws in its row, so a token override
/// that would hide it is a configuration error instead. It lays out a label
/// at the 24-byte admission bound that cannot fit, so drawing it needs at
/// least the ellipsis every label can fall back to. The geometry check keeps
/// room for the warning mark a long readout reserves.
pub fn compact_countdown_fits(svg: &str, bindings: &[WidgetBinding]) -> bool {
    let probe = HashMap::from([
        (
            "label".to_owned(),
            WidgetParameterValue::String("W".repeat(24)),
        ),
        (
            "severity".to_owned(),
            WidgetParameterValue::Enum("warning".into()),
        ),
    ]);
    geometry(svg).is_some() && prepare(svg, bindings, &HashMap::new(), &probe).is_some()
}

pub(super) fn prepare(
    svg: &str,
    bindings: &[WidgetBinding],
    constraints: &HashMap<String, (f32, f32)>,
    params: &HashMap<String, WidgetParameterValue>,
) -> Option<PreparedCountdown> {
    let Geometry {
        inset,
        body_height,
        readout_height,
        width,
        height,
    } = geometry(svg)?;
    let top = (height - body_height - readout_height) * 0.5;
    let body = Line {
        top,
        height: body_height,
    };
    let readout = Line {
        top: top + body_height,
        height: readout_height,
    };
    // Free-form label text may itself contain an ordinary id='value' or
    // id='error' substring. Complete all raw authored-ID rewrites before
    // inserting that text; shaping probes use parsed XML IDs afterwards.
    let mut non_label_params = params.clone();
    non_label_params.remove("label");
    let mut displayed = bound_widget_svg(svg, bindings, constraints, &non_label_params);
    for (id, line) in [("label", body), ("value", readout)] {
        let y = baseline(&displayed, id, line)?;
        displayed = apply_svg_attribute(&displayed, id, "x", &inset.to_string());
        displayed = apply_svg_attribute(&displayed, id, "y", &y.to_string());
    }
    // One line-height is reserved at the end; the actual mark is smaller.
    let mark_x = width - inset - readout_height * 0.5;
    let mark_y = readout.top + readout_height * 0.5;
    let transform = format!(
        "translate({mark_x} {mark_y}) scale({})",
        readout_height / 20.0
    );
    for id in ["warning", "error"] {
        displayed = apply_svg_attribute(&displayed, id, "transform", &transform);
    }
    // Ordinary countdown forms are at most seven bytes. Only an extended
    // wall-clock rollback readout needs this extra shaping probe; compress it
    // horizontally only if its full advance or ink crosses the reserved edge.
    if matches!(params.get("ends_in_ms"), Some(WidgetParameterValue::String(value)) if value.len() > 7)
    {
        let reserve = match params.get("severity") {
            Some(WidgetParameterValue::Enum(severity))
                if severity == "warning" || severity == "error" =>
            {
                readout_height
            }
            _ => 0.0,
        };
        let end = width - inset - reserve;
        let (logical, ink) = text_bounds(&displayed, "value")?;
        let rightmost = logical.right().max(ink.right());
        if rightmost > end {
            let scale = (end - inset) / (rightmost - inset);
            if !scale.is_finite() || scale <= 0.0 {
                return None;
            }
            let transform = format!(
                "translate({inset} 0) scale({scale} 1) translate({} 0)",
                -inset
            );
            displayed = apply_svg_attribute(&displayed, "value", "transform", &transform);
        }
    }
    let label = match params.get("label") {
        Some(WidgetParameterValue::String(label)) => label.as_str(),
        _ => "",
    };
    let right = width - inset;
    // The label's fit depends only on the fonts, the resolved template and
    // the label: the readout and severity bind other elements. Reuse it
    // across ticks.
    let key = (fit_key(svg), label.to_owned());
    let cached = LABEL_FITS.with(|fits| fits.borrow().get(&key).cloned());
    let (visible, prefix, clipped) = if let Some(fit) = cached {
        fit?
    } else if fits_label(&displayed, label, inset, right, body) {
        remember_fit(key, Some((label.to_owned(), label.len(), false)))?
    } else {
        // Labels have an existing 24-byte admission bound. Check complete
        // shaped prefix+ellipsis candidates, without assuming monotone widths
        // across ligatures or splitting a combining/ZWJ grapheme cluster.
        let mut fitted = None;
        for end in std::iter::once(0).chain(label.grapheme_indices(true).map(|(i, _)| i).skip(1)) {
            let candidate = format!("{}…", &label[..end]);
            if fits_label(&displayed, &candidate, inset, right, body) {
                fitted = Some((candidate, end));
            }
        }
        let fit = fitted.map(|(visible, prefix)| (visible, prefix, true));
        remember_fit(key, fit)?
    };
    displayed = apply_svg_attribute(&displayed, "label", "text-content", &escape_text(&visible));
    Some(PreparedCountdown {
        svg: displayed,
        clipped,
        prefix_chars: label[..prefix].chars().count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{
        WidgetRenderPlan, rasterize_single_svg_layer, rasterize_widget_render_plan,
        set_widget_fonts, text_mask_alpha_bounds,
    };
    use tze_hud_scene::widget_deadline::{WidgetDeadline, widget_params_at};
    use tze_hud_widget::loader::{BundleScanResult, LoadedBundle, load_bundle_from_files};

    fn bundle(theme: &str, overrides: &[(&str, &str)]) -> LoadedBundle {
        let mut selection = HashMap::from([("theme".to_owned(), theme.to_owned())]);
        selection.extend(overrides.iter().map(|(k, v)| ((*k).into(), (*v).into())));
        let tokens = tze_hud_config::themes::resolve_config_tokens(&selection);
        let (fonts, resolved) =
            crate::fonts::build_font_system(&crate::fonts::FontConfig::from_token_map(&tokens));
        set_widget_fonts(crate::fonts::loaded_font_files(fonts.db()), &resolved);
        let BundleScanResult::Ok(bundle) = load_bundle_from_files(
            "builtin:countdown",
            &[
                (
                    "widget.toml",
                    include_bytes!("../../../../assets/widget_bundles/countdown/widget.toml"),
                ),
                (
                    "card.svg",
                    include_bytes!("../../../../assets/widget_bundles/countdown/card.svg"),
                ),
                (
                    "dock.svg",
                    include_bytes!("../../../../assets/widget_bundles/countdown/dock.svg"),
                ),
            ],
            &tokens,
        ) else {
            panic!("shipped compact countdown must load");
        };
        bundle
    }

    fn params(label: &str, severity: &str, hours: u64) -> HashMap<String, WidgetParameterValue> {
        let mut deadline = WidgetDeadline::relative(86_400_000);
        deadline.snapshot(1_000_000 + (hours.saturating_sub(24)) * 3_600_000_000);
        // The admitted duration remains 24 hours. Rewinding the wall to1s
        // legitimately exposes a longer existing formatted readout.
        widget_params_at(
            &HashMap::from([
                ("label".into(), WidgetParameterValue::String(label.into())),
                (
                    "severity".into(),
                    WidgetParameterValue::Enum(severity.into()),
                ),
                (
                    "ends_in_ms".into(),
                    WidgetParameterValue::Deadline(deadline),
                ),
            ]),
            1_000_000,
        )
    }

    fn prepare_bundle(
        bundle: &LoadedBundle,
        params: &HashMap<String, WidgetParameterValue>,
    ) -> PreparedCountdown {
        let dock = bundle.dock.as_ref().unwrap();
        let layer = &dock.layers[0];
        prepare(
            std::str::from_utf8(&bundle.svg_contents[&layer.svg_file]).unwrap(),
            &layer.bindings,
            &HashMap::new(),
            params,
        )
        .unwrap()
    }

    fn text(svg: &str, id: &str) -> String {
        let tree = resvg::usvg::Tree::from_str(svg, &widget_usvg_options()).unwrap();
        let resvg::usvg::Node::Text(node) = tree.node_by_id(id).unwrap() else {
            panic!("text node");
        };
        node.chunks().iter().map(|chunk| chunk.text()).collect()
    }

    fn isolate(svg: &str, visible: Option<&str>) -> String {
        let mut isolated = svg.to_owned();
        for (tag, id) in [
            ("rect", "card"),
            ("text", "label"),
            ("text", "value"),
            ("path", "warning"),
            ("path", "error"),
        ] {
            if visible == Some(id) {
                continue;
            }
            // Match only an actual authored start tag, never ID-like prose in
            // label text. Hide the whole nonselected element, including stroke;
            // the selected element's fill-opacity/opacity stays byte-exact.
            let start = isolated
                .find(&format!("<{tag} id=\"{id}\""))
                .expect("authored element");
            let end = start + super::super::find_tag_end(&isolated[start..]).unwrap();
            isolated.insert_str(end, " display=\"none\"");
        }
        isolated
    }

    fn backdrop(svg: &str) -> String {
        isolate(svg, Some("card"))
    }

    fn luminance(rgb: [f64; 3]) -> f64 {
        let decode = |v: f64| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * decode(rgb[0]) + 0.7152 * decode(rgb[1]) + 0.0722 * decode(rgb[2])
    }

    // The existing widget path uploads premultiplied RGBA8 sRGB then blends
    // over the desktop in linear light. This is independent of SVG layout.
    fn over(pixel: tiny_skia::PremultipliedColorU8, ground: [u8; 3]) -> [f64; 3] {
        let decode = |v: f64| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let encode = |v: f64| {
            if v <= 0.0031308 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            }
        };
        let source = [pixel.red(), pixel.green(), pixel.blue()];
        std::array::from_fn(|i| {
            encode(
                (decode(f64::from(source[i]) / 255.0)
                    + decode(f64::from(ground[i]) / 255.0)
                        * (1.0 - f64::from(pixel.alpha()) / 255.0))
                    .clamp(0.0, 1.0),
            )
        })
    }

    // Opt-in only: the coordinator supplies one private output directory.
    // Ordinary tests/CI write nothing. Fixed fixture identities bound filenames.
    fn capture_path(stem: &str, extension: &str) -> Option<std::path::PathBuf> {
        let root = std::env::var_os("TZE_HUD_COMPACT_COUNTDOWN_CAPTURE_DIR")?;
        let root = std::path::PathBuf::from(root);
        std::fs::create_dir_all(&root).unwrap();
        let name: String = stem
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Some(root.join(format!("{name}.{extension}")))
    }

    fn capture_png(stem: &str, pixels: &tiny_skia::Pixmap) {
        if let Some(path) = capture_path(stem, "png") {
            pixels.save_png(path).unwrap();
        }
    }

    fn capture_json(stem: &str, value: serde_json::Value) {
        if let Some(path) = capture_path(stem, "json") {
            std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        }
    }

    fn capture_composed(stem: &str, pixels: &tiny_skia::Pixmap, ground: [u8; 3]) {
        if std::env::var_os("TZE_HUD_COMPACT_COUNTDOWN_CAPTURE_DIR").is_none() {
            return;
        }
        let mut composed = tiny_skia::Pixmap::new(pixels.width(), pixels.height()).unwrap();
        for (dst, &src) in composed.pixels_mut().iter_mut().zip(pixels.pixels()) {
            let rgb = over(src, ground).map(|c| (c * 255.0).round() as u8);
            *dst = tiny_skia::PremultipliedColorU8::from_rgba(rgb[0], rgb[1], rgb[2], 255).unwrap();
        }
        capture_png(stem, &composed);
    }

    fn contrast(
        actual: &tiny_skia::Pixmap,
        clean: &tiny_skia::Pixmap,
        mask: &tiny_skia::Pixmap,
        ground: [u8; 3],
        threshold: f64,
        glyph: bool,
        context: &str,
    ) {
        let peak = mask.pixels().iter().map(|p| p.alpha()).max().unwrap_or(0);
        // Small hinted glyphs need not contain an almost-opaque pixel. Sample
        // their actual high-coverage band, never below 75% absolute coverage.
        // Severity marks still require the original near-opaque core.
        let absolute_floor = if glyph { 192 } else { 250 };
        let relative_floor = (u16::from(peak) * 9).div_ceil(10) as u8;
        let effective_floor = if glyph {
            absolute_floor.max(relative_floor)
        } else {
            absolute_floor
        };
        let core: Vec<_> = mask
            .pixels()
            .iter()
            .enumerate()
            .filter(|(_, p)| p.alpha() >= effective_floor)
            .map(|(i, _)| i)
            .collect();
        let minimum = core
            .iter()
            .map(|&i| {
                let a = luminance(over(actual.pixels()[i], ground));
                let b = luminance(over(clean.pixels()[i], ground));
                (a.max(b) + 0.05) / (a.min(b) + 0.05)
            })
            .fold(f64::INFINITY, f64::min);
        capture_json(
            &format!("{context}-ground{ground:?}"),
            serde_json::json!({
                "case": context, "ground_srgb": ground, "glyph":glyph,
                "core_absolute_minimum_alpha":absolute_floor, "core_peak_relative_minimum_alpha":relative_floor,
                "core_minimum_alpha":effective_floor, "core_pixels":core.len(), "maximum_mask_alpha":peak,
                "mask_bounds":text_mask_alpha_bounds(mask.as_ref()),
                "minimum_composed_contrast":minimum.is_finite().then_some(minimum), "required_contrast":threshold,
                "composition":"premultiplied RGBA8 sRGB texture, linear-light source-over"
            }),
        );
        assert!(
            !core.is_empty(),
            "{context}: no genuinely filled core pixels"
        );
        eprintln!(
            "[DOCK-COUNTDOWN-CONTRAST] {context} ground={ground:?} core={} peak={peak}/255 absolute_floor={absolute_floor} relative_floor={relative_floor} coverage>={effective_floor}/255 min={minimum:.4}",
            core.len()
        );
        assert!(
            minimum >= threshold,
            "{context}: {minimum:.4} < {threshold} on {ground:?}"
        );
    }

    #[test]
    fn compact_countdown_actual_rasters_preserve_graphemes_readout_and_contrast() {
        let labels = [
            "Cutover".to_owned(),
            "W".repeat(24),
            format!("{}{}{}", "W".repeat(8), "W\u{301}".repeat(4), "W".repeat(4)),
        ];
        assert_eq!(labels[2].len(), 24);
        for theme in ["tonal-glass", "classic", "blueprint"] {
            let bundle = bundle(theme, &[]);
            let alternate = bundle.dock.as_ref().unwrap();
            let layer = &alternate.layers[0];
            let source = std::str::from_utf8(&bundle.svg_contents[&layer.svg_file]).unwrap();
            let mut plan = WidgetRenderPlan::compile(&[(source, &layer.bindings)]);
            plan.builtin_countdown_layout = true;
            let line_height = if theme == "blueprint" { 18.0 } else { 20.0 };
            let top = (52.0 - 2.0 * line_height) * 0.5;
            for severity in ["info", "warning", "error"] {
                for (index, label) in labels.iter().enumerate() {
                    let values = params(label, severity, if index == 2 { 100 } else { 24 });
                    let authoritative = values.clone();
                    let prepared = prepare_bundle(&bundle, &values);
                    assert_eq!(values, authoritative, "display never edits semantic params");
                    let shown = text(&prepared.svg, "label");
                    assert_eq!(prepared.clipped, index != 0);
                    if prepared.clipped {
                        let prefix = shown.strip_suffix('…').expect("visible end ellipsis");
                        assert!(label.starts_with(prefix));
                        assert!(label.grapheme_indices(true).any(|(i, _)| i == prefix.len()));
                        assert_eq!(prepared.prefix_chars, prefix.chars().count());
                    } else {
                        assert_eq!(shown, *label);
                    }
                    let readout = if index == 2 { "100 h 00" } else { "24 h 00" };
                    assert_eq!(text(&prepared.svg, "value"), readout);
                    let fitted = crate::widget::compact_countdown_clipped(
                        &[(source, &layer.bindings)],
                        &HashMap::new(),
                        &values,
                    );
                    assert_eq!(
                        fitted,
                        if prepared.clipped {
                            vec![("label".into(), prepared.prefix_chars)]
                        } else {
                            Vec::new()
                        }
                    );
                    for (id, y, right) in [
                        ("label", top, 152.0),
                        (
                            "value",
                            top + line_height,
                            152.0 - if severity == "info" { 0.0 } else { line_height },
                        ),
                    ] {
                        let ink = ink_bounds(&prepared.svg, id).unwrap();
                        assert!(
                            ink.left() >= 8.0 && ink.right() <= right,
                            "{theme}/{severity}/{readout}/{id}: {ink:?}"
                        );
                        assert!(
                            ink.top() >= y && ink.bottom() <= y + line_height,
                            "{theme}/{id}: {ink:?}"
                        );
                    }
                    for (width, height) in [(160, 52), (120, 39)] {
                        let actual = rasterize_widget_render_plan(
                            &plan,
                            &HashMap::new(),
                            &values,
                            width,
                            height,
                        )
                        .unwrap();
                        let clean =
                            rasterize_single_svg_layer(&backdrop(&prepared.svg), width, height)
                                .unwrap();
                        let case = format!("{theme}-{severity}-label{index}-{width}x{height}");
                        capture_png(&format!("{case}-raw"), &actual);
                        capture_png(&format!("{case}-backdrop"), &clean);
                        for ground in [[255, 255, 255], [63, 63, 89]] {
                            capture_composed(&format!("{case}-ground{ground:?}"), &actual, ground);
                        }
                        capture_json(
                            &case,
                            serde_json::json!({"theme":theme, "severity":severity, "size":[width,height], "semantic_label":label, "visible_label":shown, "readout":readout, "prefix_chars":prepared.prefix_chars, "clipped":prepared.clipped, "body_line_top":top, "line_height":line_height}),
                        );
                        let scale = width as f32 / 160.0;
                        for (id, required) in [("label", 4.5), ("value", 4.5), (severity, 3.0)] {
                            if id == "info" {
                                continue;
                            }
                            let mask = rasterize_single_svg_layer(
                                &isolate(&prepared.svg, Some(id)),
                                width,
                                height,
                            )
                            .unwrap();
                            capture_png(&format!("{case}-{id}-mask"), &mask);
                            let (left, upper, right, bottom) =
                                text_mask_alpha_bounds(mask.as_ref()).expect("real glyph/mark ink");
                            assert!(
                                left as f32 >= (8.0 * scale).floor()
                                    && right as f32 <= (152.0 * scale).ceil()
                            );
                            let start = if id == "label" {
                                top
                            } else {
                                top + line_height
                            };
                            assert!(
                                upper as f32 >= (start * scale).floor()
                                    && bottom as f32 <= ((start + line_height) * scale).ceil(),
                                "{theme}/{id}/{width}: {upper}..{bottom}"
                            );
                            let context = format!(
                                "{theme}/{severity}/label{index}/{readout}/{width}x{height}/{id}"
                            );
                            for ground in [[255, 255, 255], [63, 63, 89]] {
                                contrast(
                                    &actual,
                                    &clean,
                                    &mask,
                                    ground,
                                    required,
                                    matches!(id, "label" | "value"),
                                    &context,
                                );
                            }
                        }
                        if severity == "info" {
                            // No colored mark anywhere in its reserved end region.
                            for y in ((top + line_height) * scale).ceil() as u32
                                ..((top + 2.0 * line_height) * scale).floor() as u32
                            {
                                for x in ((152.0 - line_height) * scale).ceil() as u32
                                    ..(152.0 * scale).floor() as u32
                                {
                                    let i = (y * width + x) as usize;
                                    assert_eq!(actual.pixels()[i], clean.pixels()[i]);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn compact_countdown_uses_realized_geometry_and_keeps_every_normal_hour_value() {
        for theme in ["tonal-glass", "classic", "blueprint"] {
            let bundle = bundle(
                theme,
                &[
                    ("space.s", "10"),
                    ("type.body.m.line_height", "22"),
                    ("type.readout.line_height", "24"),
                ],
            );
            let prepared = prepare_bundle(&bundle, &params("Cutover", "error", 24));
            assert!(!prepared.svg.contains("translate(10 0) scale("));
            let rollback = prepare_bundle(&bundle, &params("Cutover", "error", 100));
            assert_eq!(text(&rollback.svg, "value"), "100 h 00");
            assert!(!rollback.svg.contains("translate(10 0) scale("));
            let label = ink_bounds(&prepared.svg, "label").unwrap();
            let readout = ink_bounds(&prepared.svg, "value").unwrap();
            assert!(label.left() >= 10.0 && label.top() >= 3.0 && label.bottom() <= 25.0);
            assert!(readout.left() >= 10.0 && readout.top() >= 25.0 && readout.bottom() <= 49.0);
            for minutes in 60..=1_440 {
                let value = format!("{} h {:02}", minutes / 60, minutes % 60);
                let svg = apply_svg_attribute(&prepared.svg, "value", "text-content", &value);
                let (metrics, ink) = text_bounds(&svg, "value").unwrap();
                assert!(
                    metrics.right().max(ink.right()) <= 160.0 - 10.0 - 24.0,
                    "{theme}/{value}: {ink:?}"
                );
            }
        }
    }
    /// The admitted 24h duration does not bound strings after wall rollback.
    /// Keep the representational maximum complete inside the actual mark lane.
    #[test]
    fn compact_countdown_characterizes_representational_maximum_without_clamping() {
        let deadline = WidgetDeadline {
            ends_in_ms: 86_400_000,
            at_wall_us: Some(u64::MAX),
            counts_up: false,
        };
        let formatted = deadline.display_at(0);
        assert_eq!(formatted, "5124095576 h 02");
        for theme in ["tonal-glass", "classic", "blueprint"] {
            let bundle = bundle(theme, &[]);
            let alternate = bundle.dock.as_ref().unwrap();
            let layer = &alternate.layers[0];
            let source = std::str::from_utf8(&bundle.svg_contents[&layer.svg_file]).unwrap();
            let mut plan = WidgetRenderPlan::compile(&[(source, &layer.bindings)]);
            plan.builtin_countdown_layout = true;
            let line_height = if theme == "blueprint" { 18.0 } else { 20.0 };
            let line_top = 26.0;
            for severity in ["error", "warning", "info"] {
                let values = widget_params_at(
                    &HashMap::from([
                        (
                            "label".into(),
                            WidgetParameterValue::String("Clock rollback".into()),
                        ),
                        (
                            "severity".into(),
                            WidgetParameterValue::Enum(severity.into()),
                        ),
                        (
                            "ends_in_ms".into(),
                            WidgetParameterValue::Deadline(deadline.clone()),
                        ),
                    ]),
                    0,
                );
                let prepared = prepare_bundle(&bundle, &values);
                let normal = prepare_bundle(&bundle, &params("Clock rollback", severity, 24));
                assert_eq!(
                    text(&prepared.svg, "value"),
                    formatted,
                    "no clamp, shortening or substituted formatter"
                );
                let (logical, ink) = text_bounds(&prepared.svg, "value").unwrap();
                let (normal_logical, _) = text_bounds(&normal.svg, "value").unwrap();
                let limit = 152.0 - if severity == "info" { 0.0 } else { line_height };
                // X fitting cannot change the baseline or font's vertical metrics.
                assert_eq!(
                    (logical.top(), logical.bottom()),
                    (normal_logical.top(), normal_logical.bottom())
                );
                assert!(logical.left() >= 8.0 && ink.left() >= 8.0);
                assert!(ink.top() >= line_top && ink.bottom() <= line_top + line_height);
                for (width, height) in [(160, 52), (120, 39)] {
                    let raster = rasterize_widget_render_plan(
                        &plan,
                        &HashMap::new(),
                        &values,
                        width,
                        height,
                    )
                    .unwrap();
                    let mask = rasterize_single_svg_layer(
                        &isolate(&prepared.svg, Some("value")),
                        width,
                        height,
                    )
                    .unwrap();
                    let clean = rasterize_single_svg_layer(&backdrop(&prepared.svg), width, height)
                        .unwrap();
                    let bounds = text_mask_alpha_bounds(mask.as_ref())
                        .expect("complete nonempty readout ink");
                    let scale = width as f32 / 160.0;
                    // Preserve the original error capture filenames for comparison.
                    let context = if severity == "error" {
                        format!("{theme}-representational-max-{width}x{height}")
                    } else {
                        format!("{theme}-representational-max-{severity}-{width}x{height}")
                    };
                    let overflow = logical.right().max(ink.right()) > limit;
                    let audit = serde_json::json!({"case":context,"formatted":formatted,"severity":severity,"absolute_deadline_us":u64::MAX,"wall_now_us":0,"size":[width,height],"reserved_readout_right":limit,"logical_bounds":[logical.left(),logical.top(),logical.right(),logical.bottom()],"ink_bounds":[ink.left(),ink.top(),ink.right(),ink.bottom()],"rendered_mask_bounds":bounds,"reserved_line_overflow":overflow});
                    eprintln!("[DOCK-COUNTDOWN-MAXIMUM] {audit}");
                    if severity == "error" {
                        capture_png(&format!("{context}-raw"), &raster);
                        capture_png(&format!("{context}-readout-mask"), &mask);
                        for ground in [[255, 255, 255], [63, 63, 89]] {
                            capture_composed(
                                &format!("{context}-ground{ground:?}"),
                                &raster,
                                ground,
                            );
                        }
                        capture_json(&context, audit);
                    }
                    assert!(!overflow, "{context}: {logical:?} / {ink:?} cross {limit}");
                    assert!(
                        bounds.0 as f32 >= (8.0 * scale).floor()
                            && bounds.2 as f32 <= (limit * scale).ceil()
                    );
                    assert!(
                        bounds.1 as f32 >= (line_top * scale).floor()
                            && bounds.3 as f32 <= ((line_top + line_height) * scale).ceil()
                    );
                    for ground in [[255, 255, 255], [63, 63, 89]] {
                        contrast(
                            &raster,
                            &clean,
                            &mask,
                            ground,
                            4.5,
                            true,
                            &format!("{context}/value"),
                        );
                    }
                    for mark in ["warning", "error"] {
                        let mark_mask = rasterize_single_svg_layer(
                            &isolate(&prepared.svg, Some(mark)),
                            width,
                            height,
                        )
                        .unwrap();
                        let normal_mark = rasterize_single_svg_layer(
                            &isolate(&normal.svg, Some(mark)),
                            width,
                            height,
                        )
                        .unwrap();
                        assert_eq!(
                            mark_mask.data(),
                            normal_mark.data(),
                            "{context}: {mark} geometry/ink changed"
                        );
                        assert!(
                            mask.pixels()
                                .iter()
                                .zip(mark_mask.pixels())
                                .all(|(text, mark)| text.alpha() == 0 || mark.alpha() == 0),
                            "{context}: readout overlaps {mark}"
                        );
                        if severity == mark {
                            for ground in [[255, 255, 255], [63, 63, 89]] {
                                contrast(
                                    &raster,
                                    &clean,
                                    &mark_mask,
                                    ground,
                                    3.0,
                                    false,
                                    &format!("{context}/{mark}"),
                                );
                            }
                        } else {
                            assert!(
                                text_mask_alpha_bounds(mark_mask.as_ref()).is_none(),
                                "{context}: unexpected {mark}"
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn compact_reference_isolation_preserves_authored_alpha_and_excludes_border() {
        let bundle = bundle("tonal-glass", &[]);
        let prepared = prepare_bundle(&bundle, &params("Cutover", "error", 24));
        let actual = rasterize_single_svg_layer(&prepared.svg, 160, 52).unwrap();
        let clean = rasterize_single_svg_layer(&backdrop(&prepared.svg), 160, 52).unwrap();
        // The glyph-free interior retains the authored translucent surface,
        // while its real outline remains present in the clean reference.
        let interior = (24 * 160 + 80) as usize;
        assert_eq!(clean.pixels()[interior], actual.pixels()[interior]);
        assert_eq!(
            clean.pixels()[interior].alpha(),
            (0.92_f32 * 255.0).round() as u8
        );
        assert!(clean.pixels()[interior].alpha() < 255);
        for id in ["label", "value", "error"] {
            let mask =
                rasterize_single_svg_layer(&isolate(&prepared.svg, Some(id)), 160, 52).unwrap();
            assert!(text_mask_alpha_bounds(mask.as_ref()).is_some());
            for (x, y) in [(80, 0), (0, 24), (159, 24), (80, 51), (80, 24)] {
                assert_eq!(
                    mask.pixels()[(y * 160 + x) as usize].alpha(),
                    0,
                    "{id}: background/border leaked at{x},{y}"
                );
            }
        }
    }

    #[test]
    fn compact_identifier_like_labels_cannot_rewrite_readout_or_marks() {
        let bundle = bundle("tonal-glass", &[]);
        for severity in ["warning", "error"] {
            let plain = prepare_bundle(&bundle, &params("Cutover", severity, 24));
            for label in [
                "id='value'",
                "id='warning'",
                "id='error'",
                "id=\"value\"",
                "id=\"warning\"",
                "id=\"error\"",
            ] {
                assert!(label.len() <= 24);
                let values = params(label, severity, 24);
                let original = values.clone();
                let prepared = prepare_bundle(&bundle, &values);
                assert_eq!(values, original);
                assert_eq!(text(&prepared.svg, "value"), "24 h 00");
                assert_eq!(text(&prepared.svg, "label"), label);
                assert!(!prepared.clipped);
                for id in ["value", severity] {
                    let expected =
                        rasterize_single_svg_layer(&isolate(&plain.svg, Some(id)), 160, 52)
                            .unwrap();
                    let actual =
                        rasterize_single_svg_layer(&isolate(&prepared.svg, Some(id)), 160, 52)
                            .unwrap();
                    assert_eq!(
                        actual.data(),
                        expected.data(),
                        "{label}: {id} must retain its actual geometry/pixels"
                    );
                    assert!(text_mask_alpha_bounds(actual.as_ref()).is_some());
                }
            }
        }
    }

    #[test]
    fn compact_label_fit_is_shaped_once_per_label_across_readout_ticks() {
        let bundle = bundle("tonal-glass", &[]);
        let label = "Deploy the staging cluster";
        LABEL_FITS.with(|fits| fits.borrow_mut().clear());
        let first = prepare_bundle(&bundle, &params(label, "info", 24));
        assert!(first.clipped, "the long label needs fitting");
        let fits = || LABEL_FITS.with(|fits| fits.borrow().len());
        assert_eq!(fits(), 1);
        for hours in [25, 26] {
            let tick = prepare_bundle(&bundle, &params(label, "warning", hours));
            assert_eq!(text(&tick.svg, "label"), text(&first.svg, "label"));
            assert_eq!(
                (tick.clipped, tick.prefix_chars),
                (first.clipped, first.prefix_chars)
            );
            assert_ne!(text(&tick.svg, "value"), text(&first.svg, "value"));
        }
        assert_eq!(fits(), 1, "readout and severity ticks reuse the fit");
        prepare_bundle(&bundle, &params("Deploy", "info", 24));
        assert_eq!(fits(), 2, "a new label is fitted on its own");
    }

    #[test]
    fn compact_css_collapsed_blank_labels_have_no_false_clipped_result() {
        let bundle = bundle("tonal-glass", &[]);
        let empty = prepare_bundle(&bundle, &params("", "error", 24));
        let reference = rasterize_single_svg_layer(&empty.svg, 160, 52).unwrap();
        let dock = bundle.dock.as_ref().unwrap();
        let layer = &dock.layers[0];
        let source = std::str::from_utf8(&bundle.svg_contents[&layer.svg_file]).unwrap();
        for label in ["", " ", " \t\r\n "] {
            let values = params(label, "error", 24);
            let original = values.clone();
            let prepared = prepare_bundle(&bundle, &values);
            assert_eq!(
                values, original,
                "full semantic whitespace value remains unchanged"
            );
            assert!(!prepared.clipped);
            assert_eq!(prepared.prefix_chars, label.chars().count());
            assert!(
                crate::widget::compact_countdown_clipped(
                    &[(source, &layer.bindings)],
                    &HashMap::new(),
                    &values
                )
                .is_empty()
            );
            let rendered = rasterize_single_svg_layer(&prepared.svg, 160, 52).unwrap();
            assert_eq!(
                rendered.data(),
                reference.data(),
                "whitespace paints no ellipsis or glyph"
            );
            let mask = rasterize_single_svg_layer(&isolate(&prepared.svg, Some("label")), 160, 52)
                .unwrap();
            assert!(text_mask_alpha_bounds(mask.as_ref()).is_none());
        }
    }
}
