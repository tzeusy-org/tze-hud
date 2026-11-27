//! Widget updates are proportional to change: publishing a parameter to one
//! widget re-rasterizes only that widget's SVG, not its neighbours.
//!
//! The proof counts rasterizations (per-instance counter on `WidgetRenderer`,
//! per-frame list on `FrameTelemetry::widget_rasterized`). The shipped status
//! indicator also checks independent badge/tooltip pixel regions. The frame
//! itself is still re-presented in full (no damage tracking).
//!
//! Set `TZE_HUD_SKIP_GPU_TESTS=1` to skip; run with `HEADLESS_FORCE_SOFTWARE=1`.

use crate::common::{
    Gpu, pixel_at, publish_params, register_bundle, register_instance, relative, tokens,
};
use std::collections::HashMap;

use tze_hud_compositor::Compositor;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    GeometryPolicy, SceneId, WidgetInstance, WidgetParameterValue as Value,
};
use tze_hud_widget::loader::BundleScanResult;

fn token_rgb(key: &str) -> [u8; 3] {
    let value = &tokens()[key];
    let hex = value.strip_prefix('#').expect("hex color token");
    std::array::from_fn(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
}

/// Use the real config builder for owner overrides rather than agent publishes.
fn configured_instance(
    definition: &tze_hud_scene::WidgetDefinition,
    tab: SceneId,
    name: &str,
    initial_params: serde_json::Value,
) -> WidgetInstance {
    let types = HashMap::from([(
        definition.id.clone(),
        tze_hud_config::widgets::LoadedWidgetType {
            name: definition.id.clone(),
            parameter_schema: definition.parameter_schema.clone(),
            default_geometry_policy: definition.default_geometry_policy,
            default_contention_policy: definition.default_contention_policy,
        },
    )]);
    let raw = serde_json::from_value(serde_json::json!({
        "widget_type": definition.id, "instance_id": name, "initial_params": initial_params,
    }))
    .unwrap();
    tze_hud_config::widgets::build_widget_instance(&raw, tab, &types).unwrap()
}

fn raster_count(c: &Compositor, instance: &str) -> u64 {
    c.widget_renderer().unwrap().raster_count(instance)
}

#[track_caller]
fn assert_color(
    actual: [u8; 4],
    expected: [u8; 3],
    tolerance: u8,
    context: impl std::fmt::Display,
) {
    assert!(
        actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.abs_diff(expected) <= tolerance),
        "{context}: {actual:?} vs {expected:?}"
    );
}

fn publish(scene: &mut SceneGraph, instance: &str, param: &'static str, value: f32) {
    publish_params(scene, instance, "agent-a", 0, [(param, Value::F32(value))]);
}

fn string(value: impl Into<String>) -> Value {
    Value::String(value.into())
}

fn enumeration(value: &str) -> Value {
    Value::Enum(value.into())
}

fn widget_region(
    pixels: &[u8],
    canvas_width: u32,
    origin: (u32, u32),
    local: (u32, u32, u32, u32),
) -> Vec<[u8; 4]> {
    let (x, y, width, height) = local;
    (y..y + height)
        .flat_map(|y| {
            (x..x + width).map(move |x| pixel_at(pixels, canvas_width, origin.0 + x, origin.1 + y))
        })
        .collect()
}

/// Independent color arithmetic: each shipped layer (fill, opacity), lowest
/// first, is composited source-over into a premultiplied RGBA8 sRGB raster,
/// which is then blended over the clean frame in linear light. No widget
/// renderer or SVG rasterizer is an oracle.
fn expected_badge_pixel(layers: &[([u8; 3], f64)], background: [u8; 4]) -> [u8; 4] {
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
    let mut premultiplied = [0.0f64; 3];
    let mut alpha = 0.0f64;
    for (fill, opacity) in layers {
        let a = (opacity * 255.0).round() / 255.0;
        for channel in 0..3 {
            premultiplied[channel] =
                (f64::from(fill[channel]) * a + premultiplied[channel] * (1.0 - a)).round();
        }
        alpha = a + alpha * (1.0 - a);
    }
    let mut expected = [0, 0, 0, 255];
    for channel in 0..3 {
        let source = premultiplied[channel] / 255.0;
        let base = f64::from(background[channel]) / 255.0;
        expected[channel] =
            (encode(decode(source) + decode(base) * (1.0 - alpha)) * 255.0).round() as u8;
    }
    expected
}

fn gauge_scene(compositor: &mut Compositor) -> (SceneGraph, SceneId, String) {
    let mut scene = SceneGraph::new(512.0, 256.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let name = register_bundle(
        compositor,
        &mut scene,
        bundle!("gauge", ["background.svg", "fill.svg"]),
    );
    (scene, tab, name)
}

fn widget_ledger(bytes: u64) -> tze_hud_resource::ResidentLedger {
    use tze_hud_resource::{ResidentLedger, ResidentLedgerLimits};
    ResidentLedger::new(ResidentLedgerLimits {
        aggregate_bytes: 2 * bytes,
        widget_source_bytes: bytes,
        widget_raster_bytes: bytes,
        resource_bytes: 0,
        font_bytes: 0,
    })
}

const FULL_FRAME: GeometryPolicy = relative(0.0, 0.0, 1.0, 1.0);

#[cfg(feature = "dev-mode")]
#[track_caller]
fn assert_widget_work(c: &mut Compositor, raster: u64, upload: u64) {
    let work = c.take_work_counts().expect("completed widget frame");
    assert_eq!((work.layout, work.raster, work.upload), (0, raster, upload));
    assert!(c.take_work_counts().is_none(), "single-frame drain");
}

#[tokio::test]
async fn dock_presentation_selects_and_retires_its_own_raster() {
    use std::num::NonZeroU64;
    use tze_hud_scene::dock::{DockLayout, DockMetrics, DockPresentation, WidgetBundleOrigin};
    use tze_hud_scene::types::{Rect, WidgetSvgLayer};

    let Some(mut gpu) = Gpu::with_widgets(512, 256).await else {
        return;
    };
    let ledger = widget_ledger(1024 * 1024);
    gpu.compositor.set_resident_ledger(ledger.clone());
    let mut scene = SceneGraph::new_with_clock(512.0, 256.0, Arc::new(TestClock::new(1_000)));
    let tab = scene.create_tab("Main", 0).unwrap();
    let solid = |color| {
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 64"><rect width="128" height="64" fill="{color}"/></svg>"#)
            .into_bytes()
    };
    let layer = |file: &str| WidgetSvgLayer {
        svg_file: file.into(),
        bindings: Vec::new(),
    };
    let BundleScanResult::Ok(mut bundle) = bundle!("gauge", ["background.svg", "fill.svg"]) else {
        panic!("gauge fixture");
    };
    bundle.definition.layers = vec![layer("default.svg")];
    bundle.svg_contents = HashMap::from([
        ("default.svg".into(), solid("#ff0000")),
        ("dock.svg".into(), solid("#0000ff")),
    ]);
    let origin = WidgetBundleOrigin::embedded("gauge-test");
    let alternate = DockPresentation::new(
        vec![layer("dock.svg")],
        128.0,
        64.0,
        None,
        &bundle.svg_contents,
    );
    let kind = register_bundle(
        &mut gpu.compositor,
        &mut scene,
        BundleScanResult::Ok(bundle),
    );
    scene
        .widget_registry
        .set_dock_presentation(&kind, origin.clone(), Some(alternate));
    register_instance(
        &mut scene,
        tab,
        &kind,
        "standalone",
        Some(relative(0.125, 0.25, 0.25, 0.25)),
        true,
    );
    publish(&mut scene, "standalone", "level", 0.25);
    let layout = DockLayout::new(
        Rect::new(0.0, 0.0, 512.0, 256.0),
        1,
        DockMetrics {
            row_height: 64.0,
            caption_width: 32.0,
            gap: 8.0,
        },
        [2.0],
    );
    scene.configure_dock(vec![kind.clone()], layout, NonZeroU64::new(10_000).unwrap());
    let scope = "agent-b";
    let params = |level| HashMap::from([("level".into(), Value::F32(level))]);
    scene
        .publish_to_dock(&kind, params(0.25), scope, None, 0, 0)
        .unwrap();
    let dock = scene
        .dock_instance(scope, &kind)
        .unwrap()
        .instance_name
        .clone();
    // Equal pixel dimensions expose variant selection independently of dock layout.
    assert!(scene.set_widget_geometry(&dock, relative(0.625, 0.25, 0.25, 0.25)));
    let (standalone_x, dock_x, y) = (128, 384, 96);
    let initial = gpu.render(&mut scene);
    initial.expect(standalone_x, y, [255, 0, 0, 255], 2, "default gauge");
    initial.expect(dock_x, y, [0, 0, 255, 255], 2, "dock gauge");
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 2, 2);
    let raster_bytes = 128 * 64 * 4;
    assert_eq!(ledger.snapshot().widget_raster_bytes, 2 * raster_bytes);

    // Change only the descriptor and leave the registered bytes stale. Failed
    // admission must not draw the old dock pixels or the ordinary gauge plan.
    let green = solid("#00ff00");
    let alternate = DockPresentation::new(
        vec![layer("dock.svg")],
        128.0,
        64.0,
        None,
        &HashMap::from([("dock.svg".into(), green.clone())]),
    );
    scene
        .widget_registry
        .set_dock_presentation(&kind, origin, Some(alternate));
    assert_eq!(gpu.draw(&mut scene).widget_rasterized, [dock.as_str()]);
    assert!(
        gpu.compositor
            .widget_renderer()
            .unwrap()
            .texture_entry(&dock)
            .is_none()
    );
    assert_eq!(ledger.snapshot().widget_raster_bytes, raster_bytes);
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 1, 0);

    gpu.compositor
        .widget_renderer_mut()
        .unwrap()
        .register_svg(&kind, "dock.svg", green);
    let current = gpu.render(&mut scene);
    current.expect(
        standalone_x,
        y,
        [255, 0, 0, 255],
        2,
        "default plan after restyle",
    );
    current.expect(
        dock_x,
        y,
        [0, 255, 0, 255],
        2,
        "current dock plan at the same size",
    );
    assert_eq!(raster_count(&gpu.compositor, "standalone"), 1);
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 1, 1);
    assert!(gpu.draw(&mut scene).widget_rasterized.is_empty());
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 0, 0);
    assert_eq!(ledger.snapshot().widget_raster_bytes, 2 * raster_bytes);

    scene
        .publish_to_dock(&kind, params(0.75), scope, None, 2_000, 0)
        .unwrap();
    gpu.draw(&mut scene);
    assert!(
        gpu.compositor
            .widget_renderer()
            .unwrap()
            .has_active_transition()
    );
    scene.set_owner_hidden(SurfaceClasses {
        widgets: true,
        ..SurfaceClasses::NONE
    });
    scene.release_scope(scope);
    assert!(gpu.draw(&mut scene).widget_rasterized.is_empty());
    let renderer = gpu.compositor.widget_renderer().unwrap();
    assert!(renderer.texture_entry(&dock).is_none());
    assert_eq!(renderer.raster_count(&dock), 0);
    assert!(!renderer.has_active_transition());
    assert_eq!(renderer.raster_count("standalone"), 1);
    assert_eq!(ledger.snapshot().widget_raster_bytes, raster_bytes);
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 0, 0);
}

/// The actual embedded compact source shares the standalone timer and keeps
/// steady frames and neighbouring instances free of raster/upload work.
#[tokio::test]
async fn builtin_compact_countdown_keeps_standalone_pixels_and_timer_cadence() {
    use std::num::NonZeroU64;
    use tze_hud_scene::dock::{DockLayout, DockMetrics, DockPresentation, WidgetBundleOrigin};
    use tze_hud_scene::types::Rect;

    let Some(mut gpu) = Gpu::with_widgets(640, 248).await else {
        return;
    };
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = SceneGraph::new_with_clock(640.0, 248.0, clock.clone());
    let tab = scene.create_tab("Main", 0).unwrap();
    let BundleScanResult::Ok(bundle) = bundle!("countdown", ["card.svg", "dock.svg"]) else {
        panic!("countdown");
    };
    let alternate = bundle.dock.as_ref().unwrap();
    let presentation = DockPresentation::new(
        alternate.layers.clone(),
        alternate.width,
        alternate.height,
        alternate.hover_behavior.clone(),
        &bundle.svg_contents,
    )
    .with_builtin_countdown_layout();
    let kind = register_bundle(
        &mut gpu.compositor,
        &mut scene,
        BundleScanResult::Ok(bundle),
    );
    scene.widget_registry.set_dock_presentation(
        &kind,
        WidgetBundleOrigin::embedded("builtin:countdown"),
        Some(presentation),
    );
    register_instance(
        &mut scene,
        tab,
        &kind,
        "standalone",
        Some(relative(0.0, 0.0, 360.0 / 640.0, 0.5)),
        true,
    );
    let values = |label: &str, ms| {
        [
            ("label", string(label)),
            ("severity", enumeration("error")),
            ("ends_in_ms", Value::Deadline(WidgetDeadline::relative(ms))),
        ]
    };
    let dock_values = |label: &str, ms| -> HashMap<String, Value> {
        values(label, ms)
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect()
    };
    publish_params(
        &mut scene,
        "standalone",
        "configured",
        0,
        HashMap::from(values("Cutover", 120_000)),
    );
    let standalone = gpu.render_pixels(&mut scene);
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 1, 1);
    scene.configure_dock(
        vec![kind.clone()],
        DockLayout::new(
            Rect::new(0.0, 0.0, 640.0, 248.0),
            2,
            DockMetrics {
                row_height: 52.0,
                caption_width: 96.0,
                gap: 8.0,
            },
            [160.0 / 52.0],
        ),
        NonZeroU64::new(10_000).unwrap(),
    );
    for (scope, label, ms, y) in [
        ("agent-a", "Cutover", 120_000, 24.0),
        ("agent-b", "Peer", 300_000, 120.0),
    ] {
        scene
            .publish_to_dock(&kind, dock_values(label, ms), scope, None, 0, 0)
            .unwrap();
        let name = scene
            .dock_instance(scope, &kind)
            .unwrap()
            .instance_name
            .clone();
        assert!(scene.set_widget_geometry(
            &name,
            relative(400.0 / 640.0, y / 248.0, 160.0 / 640.0, 52.0 / 248.0)
        ));
    }
    let a = scene
        .dock_instance("agent-a", &kind)
        .unwrap()
        .instance_name
        .clone();
    let b = scene
        .dock_instance("agent-b", &kind)
        .unwrap()
        .instance_name
        .clone();
    let combined = gpu.render_pixels(&mut scene);
    assert_eq!(
        widget_region(&standalone, 640, (0, 0), (0, 0, 360, 124)),
        widget_region(&combined, 640, (0, 0), (0, 0, 360, 124)),
        "adding dock artwork must preserve every standalone pixel"
    );
    assert_color(
        pixel_at(&combined, 640, 542, 60),
        token_rgb("color.error"),
        4,
        "compact filled error diamond",
    );
    assert_eq!(
        rendered_timer_value(&gpu.compositor, "standalone", "ends_in_ms"),
        string("2 min")
    );
    assert_eq!(
        rendered_timer_value(&gpu.compositor, &a, "ends_in_ms"),
        string("2 min")
    );
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 2, 2);
    assert_eq!(
        [
            raster_count(&gpu.compositor, "standalone"),
            raster_count(&gpu.compositor, &a),
            raster_count(&gpu.compositor, &b)
        ],
        [1, 1, 1]
    );
    assert!(
        !gpu.compositor.widget_deadline_needs_render(&scene),
        "identical projected timer values have no frame debt"
    );
    assert!(
        gpu.draw(&mut scene).widget_rasterized.is_empty(),
        "identical steady frame"
    );
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 0, 0);
    clock.advance(1_000);
    assert!(gpu.compositor.widget_deadline_needs_render(&scene));
    gpu.draw(&mut scene);
    for name in ["standalone", a.as_str()] {
        assert_eq!(
            rendered_timer_value(&gpu.compositor, name, "ends_in_ms"),
            string("1:59")
        );
        assert_eq!(raster_count(&gpu.compositor, name), 2);
    }
    assert_eq!(raster_count(&gpu.compositor, &b), 1);
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 2, 2);
    scene
        .publish_to_dock(
            &kind,
            dock_values("Release", 119_000),
            "agent-a",
            None,
            0,
            0,
        )
        .unwrap();
    gpu.draw(&mut scene);
    assert_eq!(raster_count(&gpu.compositor, &a), 3);
    assert_eq!(raster_count(&gpu.compositor, &b), 1);
    assert_eq!(raster_count(&gpu.compositor, "standalone"), 2);
    assert_eq!(
        scene.widget_registry.instances[&a].current_params["label"],
        string("Release")
    );
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 1, 1);
    scene.release_scope("agent-a");
    gpu.draw(&mut scene);
    assert!(
        gpu.compositor
            .widget_renderer()
            .unwrap()
            .texture_entry(&a)
            .is_none()
    );
    assert!(
        gpu.compositor
            .widget_renderer()
            .unwrap()
            .texture_entry(&b)
            .is_some()
    );
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 0, 0);
    scene.set_owner_hidden(SurfaceClasses {
        widgets: true,
        ..SurfaceClasses::NONE
    });
    clock.advance(5_000);
    assert!(!gpu.compositor.widget_deadline_needs_render(&scene));
    assert_eq!(gpu.compositor.next_widget_deadline_wall_us(&scene), None);
    assert_eq!(scene.next_widget_deadline_wall_us(), None);
    assert!(gpu.draw(&mut scene).widget_rasterized.is_empty());
    assert_eq!(raster_count(&gpu.compositor, "standalone"), 2);
    assert_eq!(raster_count(&gpu.compositor, &b), 1);
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 0, 0);
}

#[tokio::test]
async fn dock_draws_only_on_its_accepted_display_and_parks_overflow() {
    use std::num::NonZeroU64;
    use tze_hud_compositor::renderer::frame::WindowedFrameBuild;
    use tze_hud_compositor::{DisplayLayout, DisplayRect, FrameTarget};
    use tze_hud_scene::Rect;
    use tze_hud_scene::dock::{DockLayout, DockMetrics, DockProjection};

    let Some(mut gpu) = Gpu::with_widgets(512, 256).await else {
        return;
    };
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = SceneGraph::new_with_clock(512.0, 256.0, clock.clone());
    let tab = scene.create_tab("Main", 0).unwrap();
    let gauge = register_bundle(
        &mut gpu.compositor,
        &mut scene,
        bundle!("gauge", ["background.svg", "fill.svg"]),
    );
    let card_svg = |color: &str| {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="52" viewBox="0 0 160 52">
                <rect width="160" height="52" fill="{color}"/>
                <text id="value" x="22" y="34" font-family="sans-serif" font-size="24">0:00</text>
                <g id="tooltip" opacity="0"><rect x="140" y="10" width="10" height="10"/></g>
            </svg>"#,
        )
        .into_bytes()
    };
    let BundleScanResult::Ok(mut bundle) = bundle!("countdown", ["card.svg", "dock.svg"]) else {
        panic!("countdown fixture");
    };
    // Exercise same-size default-layer updates while hidden, independent of
    // alternate descriptor changes. The adjacent fixture covers compact art.
    bundle.svg_contents = HashMap::from([("card.svg".into(), card_svg("#ff0000"))]);
    let countdown = register_bundle(
        &mut gpu.compositor,
        &mut scene,
        BundleScanResult::Ok(bundle),
    );
    register_instance(
        &mut scene,
        tab,
        &gauge,
        "standalone",
        Some(relative(0.125, 0.125, 0.25, 0.25)),
        true,
    );
    publish(&mut scene, "standalone", "level", 0.25);
    let primary = DisplayRect {
        name: "PRIMARY".into(),
        rect: Rect::new(0.0, 0.0, 512.0, 256.0),
        primary: true,
    };
    let secondary = DisplayRect {
        name: "DOCK DISPLAY".into(),
        rect: Rect::new(-960.0, 83.0, 960.0, 600.0),
        primary: false,
    };
    let apply_displays = |compositor: &mut Compositor, display: DisplayRect| {
        compositor.set_display_layout(DisplayLayout::new(
            vec![
                primary.clone(),
                DisplayRect {
                    name: "MIRROR".into(),
                    ..display.clone()
                },
                display,
            ],
            &Default::default(),
        ));
    };
    apply_displays(&mut gpu.compositor, secondary.clone());
    let primary_target = FrameTarget::primary(512, 256);
    let secondary_target = FrameTarget {
        x: secondary.rect.x,
        y: secondary.rect.y,
        width: 960,
        height: 600,
        primary: false,
    };
    let accepts = [gauge.clone(), countdown.clone()];
    let project = |scene: &mut SceneGraph, display: &DisplayRect, rows| {
        let aspect_ratios = vec![(gauge.clone(), 2.0), (countdown.clone(), 160.0 / 52.0)];
        assert!(scene.set_dock_projection(
            &accepts,
            Some(DockProjection {
                target: Some(display.name.clone()),
                layout: DockLayout::new(
                    display.rect,
                    rows,
                    DockMetrics {
                        row_height: 52.0,
                        caption_width: 32.0,
                        gap: 8.0
                    },
                    aspect_ratios.iter().map(|(_, ratio)| *ratio),
                ),
                pixels_per_dp: 1.0,
                aspect_ratios,
            }),
            NonZeroU64::new(10_000).unwrap(),
        ));
    };
    let build = |c: &mut Compositor, scene: &mut SceneGraph| {
        c.prime_markdown_cache(scene);
        c.build_windowed_frame(scene, 512, 256)
    };
    let capture = |c: &mut Compositor,
                   frame: &WindowedFrameBuild,
                   target: &FrameTarget,
                   display: Option<&str>| {
        c.capture_windowed_display(frame, target, display, wgpu::TextureFormat::Rgba8UnormSrgb)
            .unwrap()
            .rgba
    };
    let signature = |c: &Compositor, frame: &WindowedFrameBuild| {
        c.display_frame_signature(frame, &secondary_target, Some(&secondary.name))
    };
    let first = build(&mut gpu.compositor, &mut scene);
    let primary_before = capture(&mut gpu.compositor, &first, &primary_target, None);

    project(&mut scene, &secondary, 2);
    scene
        .publish_to_dock(&gauge, HashMap::new(), "first", None, 0, 0)
        .unwrap();
    scene
        .publish_to_dock(
            &countdown,
            HashMap::from([(
                "ends_in_ms".into(),
                Value::Deadline(WidgetDeadline::relative(120_000)),
            )]),
            "overflow",
            None,
            2_000,
            0,
        )
        .unwrap();
    let bottom = scene
        .dock_instance("overflow", &countdown)
        .unwrap()
        .instance_name
        .clone();
    let bounds = |scene: &SceneGraph| {
        scene
            .overlay
            .widget_geometry
            .iter()
            .find(|item| item.instance_name == bottom)
            .map(|item| item.bounds)
    };
    let original_expiry = scene.widget_registry.active_publishes[&bottom][0].expires_at_wall_us;
    let initial = build(&mut gpu.compositor, &mut scene);
    assert_eq!(bounds(&scene), Some(Rect::new(-872.0, 215.0, 160.0, 52.0)));
    let entry = gpu
        .compositor
        .widget_renderer()
        .unwrap()
        .texture_entry(&bottom)
        .unwrap();
    assert_eq!((entry.width, entry.height), (160, 52));
    assert_eq!(
        capture(&mut gpu.compositor, &initial, &primary_target, None),
        primary_before
    );
    let mirrored = capture(
        &mut gpu.compositor,
        &initial,
        &secondary_target,
        Some("MIRROR"),
    );
    assert_eq!(
        mirrored,
        capture(&mut gpu.compositor, &initial, &secondary_target, None)
    );
    let selected = |c: &mut Compositor, frame: &WindowedFrameBuild| {
        capture(c, frame, &secondary_target, Some(&secondary.name))
    };
    let initial_pixels = selected(&mut gpu.compositor, &initial);
    assert_color(
        pixel_at(&initial_pixels, 960, 90, 134),
        [255, 0, 0],
        2,
        "dock card",
    );
    assert_ne!(initial_pixels, mirrored);
    let smaller = FrameTarget {
        width: 950,
        ..secondary_target
    };
    assert_eq!(
        gpu.compositor
            .display_frame_signature(&initial, &smaller, Some(&secondary.name)),
        gpu.compositor.frame_signature(&initial, &smaller),
    );
    let initial_signature = signature(&gpu.compositor, &initial);
    let surface = tze_hud_compositor::HeadlessSurface::new(&gpu.compositor.device, 960, 600);
    let present = gpu.compositor.present_windowed_display(
        &initial,
        &secondary_target,
        Some(&secondary.name),
        &surface,
    );
    assert!(present.gpu_submitted && present.signature_current);

    publish(&mut scene, "standalone", "level", 0.75);
    let ordinary_change = build(&mut gpu.compositor, &mut scene);
    assert_ne!(
        capture(&mut gpu.compositor, &ordinary_change, &primary_target, None),
        primary_before
    );
    assert_eq!(
        signature(&gpu.compositor, &ordinary_change),
        initial_signature
    );
    let mut equivalent_svg = card_svg("#ff0000");
    equivalent_svg.extend_from_slice(b"<!-- equivalent raster -->");
    gpu.compositor.widget_renderer_mut().unwrap().register_svg(
        &countdown,
        "card.svg",
        equivalent_svg,
    );
    let same_pixels = build(&mut gpu.compositor, &mut scene);
    assert_eq!(raster_count(&gpu.compositor, &bottom), 2);
    assert_eq!(signature(&gpu.compositor, &same_pixels), initial_signature);
    assert_eq!(selected(&mut gpu.compositor, &same_pixels), initial_pixels);

    clock.advance(1_000);
    assert!(gpu.compositor.widget_deadline_needs_render(&scene));
    let due = build(&mut gpu.compositor, &mut scene);
    let due_pixels = selected(&mut gpu.compositor, &due);
    assert_eq!(
        rendered_timer_value(&gpu.compositor, &bottom, "ends_in_ms"),
        string("1:59")
    );
    assert_ne!(signature(&gpu.compositor, &due), initial_signature);
    assert_ne!(
        widget_region(&due_pixels, 960, (88, 132), (20, 8, 112, 35)),
        widget_region(&initial_pixels, 960, (88, 132), (20, 8, 112, 35)),
    );
    let due_signature = signature(&gpu.compositor, &due);
    let steady = build(&mut gpu.compositor, &mut scene);
    assert_eq!(raster_count(&gpu.compositor, &bottom), 3);
    assert_eq!(signature(&gpu.compositor, &steady), due_signature);

    scene
        .set_widget_param_local(&bottom, "tooltip_visible", Value::F32(1.0))
        .unwrap();
    build(&mut gpu.compositor, &mut scene);
    assert!(
        gpu.compositor
            .widget_renderer()
            .unwrap()
            .has_active_transition()
    );
    let before_overflow = raster_count(&gpu.compositor, &bottom);

    project(&mut scene, &secondary, 1);
    let overflow = build(&mut gpu.compositor, &mut scene);
    let renderer = gpu.compositor.widget_renderer().unwrap();
    assert!(renderer.texture_entry(&bottom).is_none());
    assert!(!renderer.has_active_transition());
    assert_eq!(bounds(&scene), None);
    assert_eq!(scene.next_widget_deadline_wall_us(), None);
    assert!(!gpu.compositor.widget_deadline_needs_render(&scene));
    assert_eq!(
        scene.widget_registry.active_publishes[&bottom][0].expires_at_wall_us,
        original_expiry
    );
    let parked_pixels = selected(&mut gpu.compositor, &overflow);
    assert_eq!(
        pixel_at(&parked_pixels, 960, 90, 134),
        pixel_at(&mirrored, 960, 90, 134)
    );

    clock.advance(1_000);
    gpu.compositor.widget_renderer_mut().unwrap().register_svg(
        &countdown,
        "card.svg",
        card_svg("#0000ff"),
    );
    let restyled_hidden = build(&mut gpu.compositor, &mut scene);
    assert_eq!(raster_count(&gpu.compositor, &bottom), before_overflow);
    assert_eq!(
        selected(&mut gpu.compositor, &restyled_hidden),
        parked_pixels
    );

    project(&mut scene, &secondary, 2);
    let returned = build(&mut gpu.compositor, &mut scene);
    assert_eq!(raster_count(&gpu.compositor, &bottom), before_overflow + 1);
    assert_eq!(bounds(&scene), Some(Rect::new(-872.0, 215.0, 160.0, 52.0)));
    assert_color(
        pixel_at(&selected(&mut gpu.compositor, &returned), 960, 90, 134),
        [0, 0, 255],
        2,
        "restored card reflects current art",
    );
    assert_eq!(
        rendered_timer_value(&gpu.compositor, &bottom, "ends_in_ms"),
        string("1:58")
    );

    let mut moved = secondary.clone();
    moved.rect.y += 40.0;
    apply_displays(&mut gpu.compositor, moved.clone());
    let stale = build(&mut gpu.compositor, &mut scene);
    assert_eq!(bounds(&scene), None);
    assert!(!gpu.compositor.widget_deadline_needs_render(&scene));
    assert_eq!(gpu.compositor.next_widget_deadline_wall_us(&scene), None);
    assert_eq!(raster_count(&gpu.compositor, &bottom), before_overflow + 1);
    let moved_target = FrameTarget {
        y: moved.rect.y,
        ..secondary_target
    };
    assert_eq!(
        gpu.compositor
            .display_frame_signature(&stale, &moved_target, Some(&moved.name)),
        gpu.compositor.frame_signature(&stale, &moved_target),
    );

    project(&mut scene, &primary, 2);
    let direct = gpu.render(&mut scene);
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 2, 2);
    let bounds = bounds(&scene).unwrap();
    assert!(bounds.x >= 0.0 && bounds.y > 0.0);
    direct.expect(
        bounds.x as u32 + 3,
        bounds.y as u32 + 3,
        [0, 0, 255, 255],
        2,
        "direct primary dock",
    );
    assert!(gpu.draw(&mut scene).widget_rasterized.is_empty());
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 0, 0);

    clock.advance(8_000);
    assert_eq!(scene.drain_expired_widget_publications(), 2);
    assert!(scene.dock_instance("overflow", &countdown).is_none());
    assert!(gpu.draw(&mut scene).widget_rasterized.is_empty());
}

#[tokio::test]
async fn widget_param_update_rasterizes_only_that_instance() {
    let Some(mut gpu) = Gpu::with_widgets(512, 256).await else {
        return;
    };

    let mut scene = SceneGraph::new(512.0, 256.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    for (bundle, instance) in [
        (
            bundle!("gauge", ["background.svg", "fill.svg"]),
            "main-gauge",
        ),
        (
            bundle!("progress-bar", ["track.svg", "fill.svg"]),
            "main-progress",
        ),
    ] {
        let name = register_bundle(&mut gpu.compositor, &mut scene, bundle);
        register_instance(&mut scene, tab, &name, instance, None, false);
    }

    let counts = |c: &Compositor| {
        (
            raster_count(c, "main-gauge"),
            raster_count(c, "main-progress"),
        )
    };
    publish(&mut scene, "main-gauge", "level", 0.25);
    publish(&mut scene, "main-progress", "progress", 0.25);
    for (level_update, expected, rasterized) in [
        (None, (1, 1), &["main-gauge", "main-progress"][..]),
        (Some(0.75), (2, 1), &["main-gauge"][..]),
        (None, (2, 1), &[][..]),
    ] {
        if let Some(level) = level_update {
            publish(&mut scene, "main-gauge", "level", level);
        }
        let mut frame = gpu.draw(&mut scene);
        frame.widget_rasterized.sort_unstable();
        assert_eq!(counts(&gpu.compositor), expected);
        assert_eq!(frame.widget_rasterized, rasterized);
        #[cfg(feature = "dev-mode")]
        assert_widget_work(
            &mut gpu.compositor,
            rasterized.len() as u64,
            rasterized.len() as u64,
        );
    }
    #[cfg(feature = "dev-mode")]
    {
        // A real raster admission failure must count the entered invocation,
        // while recording no RGBA submission. Keep the live per-instance and
        // FrameTelemetry attempt observations independent of upload success.
        let ledger = widget_ledger(0);
        gpu.compositor.set_resident_ledger(ledger.clone());
        publish(&mut scene, "main-gauge", "level", 0.5);
        let denied = gpu.draw(&mut scene);
        assert_widget_work(&mut gpu.compositor, 1, 0);
        assert_eq!(counts(&gpu.compositor), (3, 1));
        assert_eq!(denied.widget_rasterized, ["main-gauge"]);
        assert_eq!(ledger.snapshot().class_denial_count, 1);
        assert!(gpu.compositor.take_work_counts().is_none());
    }

    // The original observations, including zero-quota denial, are complete.
    // Reset only this fixture's ledger/renderer; the status section has its own
    // bounded admission and fresh texture/raster-count baseline on the same GPU.
    gpu.compositor
        .set_resident_ledger(widget_ledger(1024 * 1024));
    gpu.compositor
        .init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut status_scene = SceneGraph::new(512.0, 256.0);
    let tab = status_scene.create_tab("Status", 0).unwrap();
    let name = register_bundle(
        &mut gpu.compositor,
        &mut status_scene,
        bundle!("status-indicator", ["indicator.svg"]),
    );
    register_instance(
        &mut status_scene,
        tab,
        &name,
        "status-matrix",
        Some(relative(
            32.0 / 512.0,
            32.0 / 256.0,
            320.0 / 512.0,
            124.0 / 256.0,
        )),
        false,
    );
    let region = |pixels: &[u8], local| widget_region(pixels, 512, (32, 32), local);
    let clean = gpu.draw(&mut status_scene);
    assert!(clean.widget_rasterized.is_empty());
    let baseline = gpu.pixels();
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 0, 0);
    let draw_status =
        |gpu: &mut Gpu,
         scene: &mut SceneGraph,
         (status, label, reason, tooltip): (&str, &str, &str, f32)| {
            scene
                .set_widget_param_local("status-matrix", "tooltip_visible", Value::F32(tooltip))
                .unwrap();
            publish_params(
                scene,
                "status-matrix",
                "agent-a",
                0,
                [
                    ("status", enumeration(status)),
                    ("label", string(label)),
                    ("reason", string(reason)),
                ],
            );
            let before = raster_count(&gpu.compositor, "status-matrix");
            assert_eq!(gpu.draw(scene).widget_rasterized, ["status-matrix"]);
            #[cfg(feature = "dev-mode")]
            assert_widget_work(&mut gpu.compositor, 1, 1);
            assert_eq!(raster_count(&gpu.compositor, "status-matrix"), before + 1);
            gpu.pixels()
        };
    let tile = (token_rgb("color.surface.container.low"), 0.92);
    let tooltip_roi = (0, 48, 320, 76);
    let badge_roi = (0, 0, 100, 40);
    let pill_reason = (106, 8, 200, 24);
    let glyph_roi = (7, 10, 20, 20);
    let [panel, label, reason] = [(24, 52, 260, 3), (32, 63, 200, 18), (16, 92, 280, 16)];
    let badge_rgb = |at, layers: &[([u8; 3], f64)]| {
        expected_badge_pixel(layers, region(&baseline, at)[0])[..3]
            .try_into()
            .unwrap()
    };

    // The bundle tests check every discrete mapping. Give each actual theme
    // one independent authored pixel oracle and compare the same status glyph
    // across two themes. The gesture suite owns pointer dwell and leave.
    let [_minimal, system, friendly, mut previous] = [
        ("minimal", "away", "color.caution", (17, 20, 1, 1), 1.0),
        ("system", "online", "color.success", (17, 12, 1, 1), 0.22),
        ("friendly", "online", "color.success", (17, 12, 1, 1), 0.22),
        (
            "friendly",
            "offline",
            "color.on_surface.variant",
            (17, 12, 1, 1),
            0.22,
        ),
    ]
    .map(|(theme, status, color, marker, opacity)| {
        let instance = configured_instance(
            &status_scene.widget_registry.definitions["status-indicator"],
            tab,
            "status-matrix",
            serde_json::json!({"theme": theme}),
        );
        status_scene
            .widget_registry
            .instances
            .get_mut("status-matrix")
            .unwrap()
            .current_params
            .insert("theme".into(), instance.current_params["theme"].clone());
        let frame = draw_status(
            &mut gpu,
            &mut status_scene,
            (status, "READY", "ALL SYSTEMS GO", 0.0),
        );
        for sample in [(35, 5, 1, 1), (285, 5, 1, 1)] {
            assert_color(
                region(&frame, sample)[0],
                badge_rgb(sample, &[tile]),
                2,
                theme,
            );
        }
        let actual = region(&frame, marker)[0];
        assert_eq!(actual[3], 255);
        assert_color(
            actual,
            badge_rgb(marker, &[tile, (token_rgb(color), opacity)]),
            4,
            status,
        );
        assert_eq!(region(&frame, tooltip_roi), region(&baseline, tooltip_roi));
        frame
    });
    assert_ne!(
        region(&system, glyph_roi),
        region(&friendly, glyph_roi),
        "owner theme produces a distinct glyph"
    );
    let idle_rasters = raster_count(&gpu.compositor, "status-matrix");
    assert!(gpu.draw(&mut status_scene).widget_rasterized.is_empty());
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 0, 0);
    assert_eq!(raster_count(&gpu.compositor, "status-matrix"), idle_rasters);
    assert_eq!(gpu.pixels(), previous);

    let badge = region(&previous, badge_roi);
    let panel_color = (token_rgb("color.surface.container.highest"), 1.0);
    for (caption, message, visible, changes) in [
        ("READY", "ALL SYSTEMS GO", 1.0, [true, true]),
        ("PAUSED", "ALL SYSTEMS GO", 1.0, [true, false]),
        ("PAUSED", "AWAITING OPERATOR", 1.0, [false, true]),
        ("PAUSED", "AWAITING OPERATOR", 0.0, [true, true]),
    ] {
        let frame = draw_status(
            &mut gpu,
            &mut status_scene,
            ("offline", caption, message, visible),
        );
        assert_eq!(region(&frame, badge_roi), badge, "tooltip preserves badge");
        assert_eq!(
            [label, reason].map(|roi| region(&frame, roi) != region(&previous, roi)),
            changes
        );
        if visible == 0.0 {
            assert_eq!(region(&frame, tooltip_roi), region(&baseline, tooltip_roi));
            assert_eq!(region(&frame, pill_reason), region(&previous, pill_reason));
        } else {
            assert_ne!(region(&frame, panel), region(&baseline, panel));
            for roi in [label, reason] {
                let background = badge_rgb(roi, &[panel_color]);
                assert!(
                    region(&frame, roi).iter().any(|pixel| {
                        pixel[..3]
                            .iter()
                            .zip(background)
                            .all(|(got, base)| i16::from(*got) - i16::from(base) >= 64)
                    }),
                    "missing readable glyphs in {roi:?}"
                );
            }
        }
        previous = frame;
    }
    let long = draw_status(
        &mut gpu,
        &mut status_scene,
        ("offline", "PAUSED", &"W".repeat(160), 0.0),
    );
    assert_ne!(region(&long, pill_reason), region(&previous, pill_reason));
    let inset = (307, 8, 12, 24);
    assert_eq!(
        region(&long, inset),
        region(&previous, inset),
        "reason stays inside its clip"
    );
    status_scene
        .clear_widget_for_publisher("status-matrix", "agent-a")
        .unwrap();
    assert!(gpu.draw(&mut status_scene).widget_rasterized.is_empty());
    assert_eq!(gpu.pixels(), baseline, "clear removes the status texture");
    #[cfg(feature = "dev-mode")]
    assert_widget_work(&mut gpu.compositor, 0, 0);

    assert_shipped_severity_pixels(&mut gpu);
}

fn assert_shipped_severity_pixels(gpu: &mut Gpu) {
    for (bundle, level, width, height, sample) in [
        (
            bundle!("gauge", ["background.svg", "fill.svg"]),
            "level",
            128.0,
            132.0,
            (98, 56),
        ),
        (
            bundle!("progress-bar", ["track.svg", "fill.svg"]),
            "progress",
            320.0,
            50.0,
            (100, 37),
        ),
    ] {
        let mut scene = SceneGraph::new(512.0, 256.0);
        let name = register_bundle(&mut gpu.compositor, &mut scene, bundle);
        let tab = scene.create_tab("Severity", 0).unwrap();
        let definition = scene.widget_registry.definitions[&name].clone();
        let mut paint = |scene: &mut SceneGraph, params: &[(&'static str, Value)]| {
            publish_params(scene, &name, "model", 0, params.iter().cloned());
            gpu.render_pixels(scene)
        };
        let mut default_severity_frames = HashMap::new();
        for owner in [false, true] {
            let (initial, fill) = if owner {
                (
                    serde_json::json!({ "fill_color": [12, 210, 123, 255] }),
                    [12, 210, 123],
                )
            } else {
                (serde_json::json!({}), token_rgb("color.primary"))
            };
            let mut scene = scene.clone();
            let mut instance = configured_instance(&definition, tab, &name, initial);
            instance.geometry_override = Some(relative(0.0, 0.0, width / 512.0, height / 256.0));
            scene.widget_registry.register_instance(instance);
            if level == "progress" {
                let at = |pixels: &[u8], roi| widget_region(pixels, 512, (0, 0), roi);
                let track = token_rgb("color.outline.variant");
                let mut last_readout = Vec::new();
                for (progress, fill_to) in [(0.0, 15), (0.5, 160), (1.0, 305)] {
                    let pixels = paint(
                        &mut scene,
                        &[(level, Value::F32(progress)), ("label", string(""))],
                    );
                    for x in [18, 155, 165, 301] {
                        let expected = if x < fill_to { fill } else { track };
                        assert_color(
                            pixel_at(&pixels, 512, x, 37),
                            expected,
                            2,
                            format_args!("progress={progress}, x={x}"),
                        );
                    }
                    let text = at(&pixels, (270, 10, 35, 16));
                    assert!(
                        text.iter()
                            .any(|p| p[..3].iter().all(|&channel| channel > 128))
                    );
                    assert_ne!(text, last_readout, "readout updates when progress changes");
                    last_readout = text;
                    let long = paint(&mut scene, &[("label", string("W".repeat(32)))]);
                    for (roi, stable) in [
                        ((270, 10, 50, 16), true),  // Includes the percentage and right inset.
                        ((320, 0, 192, 50), true),  // Outside the progress card.
                        ((15, 10, 220, 16), false), // Visible label text.
                    ] {
                        assert_eq!(
                            at(&long, roi) == at(&pixels, roi),
                            stable,
                            "long label at {roi:?}, progress={progress}, owner={owner}"
                        );
                    }
                }
            }
            let mut info_frame = None;
            for (severity, expected) in [
                ("info", fill),
                ("warning", token_rgb("color.caution")),
                ("error", token_rgb("color.error")),
                ("info", fill),
            ] {
                let pixels = paint(
                    &mut scene,
                    &[
                        (level, Value::F32(0.75)),
                        ("severity", enumeration(severity)),
                        ("label", string("")),
                    ],
                );
                assert_color(
                    pixel_at(&pixels, 512, sample.0, sample.1),
                    expected,
                    2,
                    format_args!("owner={owner} severity={severity}"),
                );
                if severity == "info" {
                    if let Some(first) = &info_frame {
                        assert!(pixels == *first, "{name}: info frame must return unchanged");
                    } else {
                        info_frame = Some(pixels);
                    }
                } else if owner {
                    assert!(
                        pixels == default_severity_frames[severity],
                        "{name}/{severity}: owner info fill changed non-info pixels, including rounded edges"
                    );
                } else {
                    default_severity_frames.insert(severity, pixels);
                }
            }
        }
    }
}

/// Owner hiding (control panel, CP-6): while widgets are hidden a publish
/// costs no raster or upload and nothing is drawn; showing them again
/// rasterizes the newest parameters once and draws them directly, even when
/// they were published with a transition.
#[tokio::test]
async fn hidden_widgets_do_no_raster_work_and_draw_nothing() {
    use tze_hud_scene::graph::SurfaceClasses;

    let Some(mut gpu) = Gpu::with_widgets(512, 256).await else {
        return;
    };
    let (mut scene, tab, name) = gauge_scene(&mut gpu.compositor);
    register_instance(&mut scene, tab, &name, "main-gauge", None, false);
    let count = |c: &Compositor| raster_count(c, "main-gauge");
    // The frame with no published widget: what "nothing drawn" looks like.
    let empty = gpu.render_pixels(&mut scene);

    // What the newest value (0.75) looks like when published directly.
    publish(&mut scene, "main-gauge", "level", 0.75);
    let at_075 = gpu.render_pixels(&mut scene);
    assert!(at_075 != empty, "the gauge draws");

    publish(&mut scene, "main-gauge", "level", 0.25);
    gpu.draw(&mut scene);
    assert_eq!(count(&gpu.compositor), 2);

    scene.set_owner_hidden(SurfaceClasses {
        widgets: true,
        ..SurfaceClasses::NONE
    });
    // Published with a transition while hidden.
    publish_params(
        &mut scene,
        "main-gauge",
        "agent-a",
        500,
        [("level", Value::F32(0.75))],
    );
    let hidden = gpu.draw(&mut scene);
    assert_eq!(count(&gpu.compositor), 2, "no raster while hidden");
    assert!(hidden.widget_rasterized.is_empty());
    assert!(gpu.pixels() == empty, "nothing drawn while hidden");

    scene.set_owner_hidden(SurfaceClasses::NONE);
    let shown = gpu.draw(&mut scene);
    assert_eq!(count(&gpu.compositor), 3, "one raster of the newest params");
    assert_eq!(shown.widget_rasterized, ["main-gauge"]);
    assert!(
        !gpu.compositor
            .widget_renderer()
            .unwrap()
            .has_active_transition(),
        "showing snaps: no transition from the value drawn before hiding"
    );
    // Raster rounding varies by one step; a transition starting from the
    // pre-hide 0.25 would differ by whole fill colors.
    let again = gpu.pixels();
    let max_step = again
        .iter()
        .zip(&at_075)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        max_step <= 2,
        "drawn again at the newest value (max channel step {max_step})"
    );
}

#[tokio::test]
async fn widget_raster_is_invalidated_by_size_and_source_changes() {
    let Some(mut gpu) = Gpu::with_widgets(512, 256).await else {
        return;
    };
    let BundleScanResult::Ok(bundle) = bundle!("gauge", ["background.svg", "fill.svg"]) else {
        panic!("built-in gauge bundle failed to load");
    };
    let mut scene = SceneGraph::new(512.0, 256.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let mut source = bundle.svg_contents["fill.svg"].clone();
    let name = register_bundle(
        &mut gpu.compositor,
        &mut scene,
        BundleScanResult::Ok(bundle),
    );
    let at = |x_pct, width_pct| relative(x_pct, 0.1, width_pct, 0.5);
    register_instance(&mut scene, tab, &name, "g", Some(at(0.1, 0.25)), false);
    publish(&mut scene, "g", "level", 0.5);
    let check = |gpu: &mut Gpu, scene: &mut SceneGraph, (rastered, count, width)| {
        let frame = gpu.draw(scene);
        let c = &gpu.compositor;
        let expected: &[&str] = if rastered { &["g"] } else { &[] };
        assert_eq!(frame.widget_rasterized, expected);
        let texture = c.widget_renderer().unwrap().texture_entry("g").unwrap();
        assert_eq!(
            (raster_count(c, "g"), texture.width, texture.height),
            (count, width, 128)
        );
    };

    check(&mut gpu, &mut scene, (true, 1, 128));
    assert!(scene.set_widget_geometry("g", at(0.5, 0.25)));
    check(&mut gpu, &mut scene, (false, 1, 128));
    assert!(scene.set_widget_geometry("g", at(0.5, 0.5)));
    check(&mut gpu, &mut scene, (true, 2, 256));
    assert!(!scene.set_widget_geometry("missing", at(0.0, 0.1)));

    let register = |c: &mut Compositor, bytes| {
        c.widget_renderer_mut()
            .unwrap()
            .register_svg(&name, "fill.svg", bytes);
    };
    register(&mut gpu.compositor, source.clone());
    check(&mut gpu, &mut scene, (false, 2, 256));
    source.extend_from_slice(b"<!-- restyled -->");
    register(&mut gpu.compositor, source);
    check(&mut gpu, &mut scene, (true, 3, 256));
    check(&mut gpu, &mut scene, (false, 3, 256));
}

/// Render gauges sharing one box (each `(instance, level)`) in a fresh scene
/// and read the frame back.
fn overlapping_gauges_frame(gpu: &mut Gpu, gauges: &[(&str, f32)]) -> Vec<u8> {
    let (mut scene, tab, name) = gauge_scene(&mut gpu.compositor);
    let at = relative(0.1, 0.1, 0.25, 0.5);
    for &(instance, level) in gauges {
        register_instance(&mut scene, tab, &name, instance, Some(at), false);
        publish(&mut scene, instance, "level", level);
    }
    gpu.render_pixels(&mut scene)
}

/// Every widget shares one z-order, so overlapping widgets stack by instance
/// name on every frame and every run, independent of the instance map's
/// iteration order (a `HashMap`, reseeded per scene).
#[tokio::test]
async fn overlapping_widgets_stack_by_instance_name() {
    let Some(mut gpu) = Gpu::with_widgets(512, 256).await else {
        return;
    };
    let mut frame = |gauges: &[(&str, f32)]| overlapping_gauges_frame(&mut gpu, gauges);
    let full = frame(&[("b", 1.0)]);
    let empty = frame(&[("b", 0.0)]);
    // Probe the ring's solid foreground/track interiors, with a clear
    // reference contrast through all four neighbours. Tiny differences from
    // antialiased percent glyphs cannot decide which widget is on top.
    let (w, h) = (512usize, 256usize);
    let differs = |x: usize, y: usize| {
        let i = (y * w + x) * 4;
        full[i..i + 3]
            .iter()
            .zip(&empty[i..i + 3])
            .any(|(a, b)| a.abs_diff(*b) >= 64)
    };
    let probe: Vec<usize> = (1..h - 1)
        .flat_map(|y| (1..w - 1).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            differs(x, y)
                && differs(x - 1, y)
                && differs(x + 1, y)
                && differs(x, y - 1)
                && differs(x, y + 1)
        })
        .map(|(x, y)| (y * w + x) * 4)
        .collect();
    assert!(!probe.is_empty(), "a full and an empty gauge must differ");
    // Each probe pixel must be nearer the expected gauge's colour than the
    // other's: a rounded track corner blends slightly differently over one
    // card than over two, but full and empty differ by ~80 per channel.
    let distance = |a: &[u8], b: &[u8]| -> u32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| u32::from(x.abs_diff(*y)))
            .sum()
    };
    let shows = |frame: &[u8], expected: &[u8], other: &[u8]| {
        probe.iter().all(|&i| {
            let px = &frame[i..i + 4];
            distance(px, &expected[i..i + 4]) < distance(px, &other[i..i + 4])
        })
    };
    for (b_level, expected, other) in [(1.0, &full, &empty), (0.0, &empty, &full)] {
        let mut gauges = [("a", 1.0 - b_level), ("b", b_level)];
        for _ in 0..2 {
            assert!(
                shows(&frame(&gauges), expected, other),
                "b draws over a: {gauges:?}"
            );
            gauges.reverse();
        }
    }
}

use std::sync::Arc;
use tze_hud_scene::{TestClock, WidgetDeadline, graph::SurfaceClasses};
use tze_hud_telemetry::FrameTelemetry;

fn timer_scene() -> (SceneGraph, Arc<TestClock>, SceneId) {
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = SceneGraph::new_with_clock(360.0, 124.0, clock.clone());
    let tab = scene.create_tab("Bundles", 0).unwrap();
    (scene, clock, tab)
}

fn rendered_timer_value(c: &Compositor, widget: &str, parameter: &str) -> Value {
    let renderer = c.widget_renderer().unwrap();
    let entry = renderer.texture_entry(widget).unwrap();
    entry.last_rendered_params[parameter].clone()
}

#[track_caller]
fn draw_deadline_frame(gpu: &mut Gpu, scene: &mut SceneGraph, due: bool) -> FrameTelemetry {
    assert_eq!(gpu.compositor.widget_deadline_needs_render(scene), due);
    gpu.draw(scene)
}

/// Real shipped bundle pixels, semantic color tokens and countdown raster work.
/// TestClock advances wall time; no host timer or synthetic SVG is the oracle.
fn assert_countdown_kpi_and_stepper_contracts(gpu: &mut Gpu) {
    let (mut scene, clock, tab) = timer_scene();
    for loaded in [
        bundle!("countdown", ["card.svg", "dock.svg"]),
        bundle!("kpi", ["card.svg"]),
        bundle!("stepper", ["card.svg"]),
    ] {
        let name = register_bundle(&mut gpu.compositor, &mut scene, loaded);
        register_instance(&mut scene, tab, &name, &name, Some(FULL_FRAME), true);
    }
    let publish = |scene: &mut SceneGraph, name, params: &[(&'static str, Value)]| {
        publish_params(scene, name, "agent", 200, params.iter().cloned());
    };
    let dot_color = |pixels: &[u8], dot: u32, token| {
        assert_color(
            pixel_at(pixels, 360, 20 + dot * 29, 54),
            token_rgb(token),
            4,
            format_args!("{token} at step {dot}"),
        );
    };
    publish(
        &mut scene,
        "stepper",
        &[
            ("step", Value::F32(2.0)),
            ("total", Value::F32(5.0)),
            ("state", enumeration("blocked")),
        ],
    );
    let blocked = gpu.render_pixels(&mut scene);
    for (step, active) in [(2.0, 1), (7.0, 4)] {
        publish(&mut scene, "stepper", &[("step", Value::F32(step))]);
        gpu.draw(&mut scene);
        clock.advance(201);
        let pixels = gpu.render_pixels(&mut scene);
        if active == 1 {
            assert!(
                pixels == blocked,
                "republishing the same whole step changes no pixel"
            );
        }
        for dot in 0..5 {
            dot_color(
                &pixels,
                dot,
                match dot.cmp(&active) {
                    std::cmp::Ordering::Less => "color.primary",
                    std::cmp::Ordering::Equal => "color.caution",
                    std::cmp::Ordering::Greater => "color.outline.variant",
                },
            );
        }
    }
    for (state, token) in [("active", "color.primary"), ("done", "color.success")] {
        publish(&mut scene, "stepper", &[("state", enumeration(state))]);
        let pixels = gpu.render_pixels(&mut scene);
        dot_color(&pixels, 4, token);
    }
    scene
        .clear_widget_for_publisher("stepper", "agent")
        .unwrap();
    publish(
        &mut scene,
        "kpi",
        &[
            ("value", string("3 fail")),
            ("label", string("Tests")),
            ("trend", enumeration("down")),
            ("severity", enumeration("error")),
        ],
    );
    let failure = gpu.render_pixels(&mut scene);
    let glyph = widget_region(&failure, 360, (0, 0), (310, 40, 20, 22));
    let error = token_rgb("color.error");
    assert!(
        glyph
            .iter()
            .any(|pixel| (0..3).all(|c| pixel[c].abs_diff(error[c]) <= 5)),
        "down path must use error token"
    );
    publish(
        &mut scene,
        "kpi",
        &[("value", string("")), ("trend", enumeration("up"))],
    );
    let changed = gpu.render_pixels(&mut scene);
    assert_ne!(
        widget_region(&failure, 360, (0, 0), (16, 40, 160, 24)),
        widget_region(&changed, 360, (0, 0), (16, 40, 160, 24)),
        "3 fail must produce text pixels"
    );
    assert_ne!(
        glyph,
        widget_region(&changed, 360, (0, 0), (310, 40, 20, 22)),
        "down differs from up"
    );
    scene.clear_widget_for_publisher("kpi", "agent").unwrap();
    let clean = gpu.render_pixels(&mut scene);
    let clear_pixel = pixel_at(&clean, 360, 0, 0);
    let remaining = |ms| ("ends_in_ms", Value::Deadline(WidgetDeadline::relative(ms)));
    publish(
        &mut scene,
        "countdown",
        &[remaining(300_000), ("severity", enumeration("error"))],
    );
    assert!(gpu.compositor.widget_deadline_needs_render(&scene));
    let five = gpu.render_pixels(&mut scene);
    let rendered_text = |c: &Compositor| rendered_timer_value(c, "countdown", "ends_in_ms");
    let count = |c: &Compositor| raster_count(c, "countdown");
    assert_eq!(rendered_text(&gpu.compositor), string("5 min"));
    let initial_rasters = count(&gpu.compositor);
    assert!(!gpu.compositor.has_inflight_animation(&scene));
    clock.advance(59_999);
    let early = draw_deadline_frame(gpu, &mut scene, false);
    assert!(early.widget_rasterized.is_empty());
    assert_eq!(count(&gpu.compositor), initial_rasters);
    clock.advance(1);
    let due = draw_deadline_frame(gpu, &mut scene, true);
    assert_eq!(due.widget_rasterized, ["countdown"]);
    assert_eq!(rendered_text(&gpu.compositor), string("4 min"));
    assert_eq!(count(&gpu.compositor), initial_rasters + 1);

    // The windowed idle-gate harness separately covers pause and resume.
    scene.set_owner_hidden(SurfaceClasses {
        widgets: true,
        ..SurfaceClasses::NONE
    });
    clock.advance(60_000);
    assert_eq!(gpu.compositor.next_widget_deadline_wall_us(&scene), None);
    let hidden = draw_deadline_frame(gpu, &mut scene, false);
    assert!(hidden.widget_rasterized.is_empty());
    assert_eq!(count(&gpu.compositor), initial_rasters + 1);

    scene.set_owner_hidden(SurfaceClasses::NONE);
    let shown = draw_deadline_frame(gpu, &mut scene, true);
    assert_eq!(shown.widget_rasterized, ["countdown"]);
    assert_eq!(rendered_text(&gpu.compositor), string("3 min"));
    assert_eq!(count(&gpu.compositor), initial_rasters + 2);
    assert!(!gpu.compositor.has_inflight_animation(&scene));

    clock.advance(180_000);
    let zero = gpu.render_pixels(&mut scene);
    assert_eq!(rendered_text(&gpu.compositor), string("0:00"));
    assert_eq!(gpu.compositor.next_widget_deadline_wall_us(&scene), None);
    assert_ne!(five, zero);
    // Small text may have no fully opaque glyph pixel. Its brightest red
    // pixel must still match the error token at nontrivial mask coverage,
    // independently composited over the shipped card and clean frame.
    let readout = widget_region(&zero, 360, (0, 0), (16, 40, 160, 24));
    let actual = readout.iter().max_by_key(|pixel| pixel[0]).unwrap();
    let tile = (token_rgb("color.surface.container.high"), 0.92);
    assert!(
        (32..=255).any(|coverage| {
            let expected =
                expected_badge_pixel(&[tile, (error, f64::from(coverage) / 255.0)], clear_pixel);
            (0..3).all(|c| actual[c].abs_diff(expected[c]) <= 4)
        }),
        "zero retains its severity at antialiased coverage: {actual:?}"
    );
    publish(&mut scene, "countdown", &[remaining(90_000)]);
    let next_deadline = gpu.compositor.next_widget_deadline_wall_us(&scene);
    assert!(next_deadline.is_some());
    let instances = &mut scene.widget_registry.instances;
    let countdown = instances.get_mut("countdown").unwrap();
    countdown.geometry_override = Some(relative(0.0, 0.0, 0.0, 1.0));
    assert_eq!(gpu.compositor.next_widget_deadline_wall_us(&scene), None);
    assert!(!gpu.compositor.widget_deadline_needs_render(&scene));
    gpu.draw(&mut scene);
    let renderer = gpu.compositor.widget_renderer().unwrap();
    assert!(renderer.texture_entry("countdown").is_none());
}

/// The elapsed timer rasterizes once per display change and never between
/// (per second, then per minute), and its longest strings fit the card.
#[tokio::test]
async fn elapsed_rasterizes_once_per_display_change_and_fits_its_longest_strings() {
    let Some(mut gpu) = Gpu::with_widgets(360, 124).await else {
        return;
    };
    assert_countdown_kpi_and_stepper_contracts(&mut gpu);
    gpu.compositor
        .init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (mut scene, clock, tab) = timer_scene();
    let elapsed = bundle!("elapsed", ["card.svg"]);
    let name = register_bundle(&mut gpu.compositor, &mut scene, elapsed);
    register_instance(&mut scene, tab, &name, &name, Some(FULL_FRAME), true);
    let start = |scene: &mut SceneGraph, ago_ms| {
        publish_params(
            scene,
            "elapsed",
            "agent",
            0,
            [
                (
                    "started_ms_ago",
                    Value::Deadline(WidgetDeadline::relative(ago_ms)),
                ),
                ("label", string("Tests")),
            ],
        );
    };
    let count = |c: &Compositor| raster_count(c, "elapsed");
    let shown = |c: &Compositor| rendered_timer_value(c, "elapsed", "started_ms_ago");
    for (start_ms, before, after, boundary) in [
        (0, "0:00", "0:01", 1_000),
        (180_000, "3 min", "4 min", 60_000),
    ] {
        start(&mut scene, start_ms);
        gpu.draw(&mut scene);
        assert_eq!(shown(&gpu.compositor), string(before));
        assert!(!gpu.compositor.has_inflight_animation(&scene));
        let before_count = count(&gpu.compositor);
        clock.advance(boundary - 1);
        draw_deadline_frame(&mut gpu, &mut scene, false);
        assert_eq!(count(&gpu.compositor), before_count);
        clock.advance(1);
        draw_deadline_frame(&mut gpu, &mut scene, true);
        assert_eq!(
            (count(&gpu.compositor), shown(&gpu.compositor)),
            (before_count + 1, string(after))
        );
    }
    // The longest strings stay inside the card with its 16 px inset.
    for (ago_ms, expected) in [(86_340_000, "23 h 59"), (86_400_000, "1 d 00 h")] {
        start(&mut scene, ago_ms);
        gpu.draw(&mut scene);
        assert_eq!(shown(&gpu.compositor), string(expected));
    }
    clock.advance(99 * 24 * 3_600_000);
    gpu.draw(&mut scene);
    assert_eq!(shown(&gpu.compositor), string("99 d 23 h"));
    assert_eq!(
        gpu.compositor.next_widget_deadline_wall_us(&scene),
        None,
        "held"
    );
    let pixels = gpu.pixels();
    let primary = token_rgb("color.primary");
    let ink = |x: u32| {
        widget_region(&pixels, 360, (0, 0), (x, 40, 1, 24))
            .iter()
            .any(|p| (0..3).all(|c| p[c].abs_diff(primary[c]) <= 40))
    };
    let columns: Vec<u32> = (0..360).filter(|x| ink(*x)).collect();
    let (left, right) = (columns[0], *columns.last().unwrap());
    assert!(
        left >= 16 && right < 360 - 16,
        "value ink spans {left}..={right}"
    );
    assert!(
        right - left > 40,
        "the whole string is drawn: {left}..={right}"
    );
}

#[tokio::test]
async fn dock_session_caption_follows_only_its_accepted_display() {
    use std::num::NonZeroU64;
    use std::sync::Arc;
    use tze_hud_compositor::renderer::frame::WindowedFrameBuild;
    use tze_hud_compositor::{DisplayLayout, DisplayRect, FrameTarget, HeadlessSurface};
    use tze_hud_scene::dock::{DockLayout, DockMetrics, DockProjection};
    use tze_hud_scene::scope::scoped_namespace;
    use tze_hud_scene::{Rect, TestClock};

    let Some(mut gpu) = Gpu::with_widgets(480, 256).await else {
        return;
    };
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut caption_tokens = tokens();
    caption_tokens.insert("type.label.m.family".into(), "monospace".into());
    caption_tokens.insert("font.mono".into(), "IBM Plex Mono".into());
    caption_tokens.insert("color.on_surface.variant".into(), "#00FF00".into());
    gpu.compositor.set_token_map(caption_tokens);
    gpu.compositor.init_text_renderer(format);
    let mut scene = SceneGraph::new_with_clock(480.0, 256.0, Arc::new(TestClock::new(1_000)));
    scene.create_tab("Main", 0).unwrap();
    let gauge = register_bundle(
        &mut gpu.compositor,
        &mut scene,
        bundle!("gauge", ["background.svg", "fill.svg"]),
    );
    let primary_target = FrameTarget::primary(480, 256);
    let primary = DisplayRect {
        name: "PRIMARY".into(),
        rect: Rect::new(0.0, 0.0, 480.0, 256.0),
        primary: true,
    };
    let selected = DisplayRect {
        name: "DOCK DISPLAY".into(),
        rect: Rect::new(-480.0, 83.0, 480.0, 256.0),
        primary: false,
    };
    let selected_target = FrameTarget {
        x: -480.0,
        y: 83.0,
        primary: false,
        ..primary_target
    };
    gpu.compositor.set_display_layout(DisplayLayout::new(
        vec![
            primary.clone(),
            DisplayRect {
                name: "MIRROR".into(),
                ..selected.clone()
            },
            selected.clone(),
        ],
        &Default::default(),
    ));
    let project = |scene: &mut SceneGraph, display: &DisplayRect| {
        let layout = DockLayout::new(
            display.rect,
            2,
            DockMetrics {
                row_height: 52.0,
                caption_width: 112.0,
                gap: 8.0,
            },
            [2.0],
        );
        assert_eq!(layout.scale, 0.75);
        assert_eq!(layout.caption(0).width, 84.0);
        assert!(scene.set_dock_projection(
            std::slice::from_ref(&gauge),
            Some(DockProjection {
                target: Some(display.name.clone()),
                layout,
                pixels_per_dp: 1.5,
                aspect_ratios: vec![(gauge.clone(), 2.0)],
            }),
            NonZeroU64::new(10_000).unwrap(),
        ));
    };
    project(&mut scene, &selected);
    let scoped = scoped_namespace("agent", "session");
    let lease = scene.grant_lease(&scoped, 10_000);
    for namespace in [scoped.as_str(), "agent"] {
        scene
            .publish_to_dock(&gauge, HashMap::new(), namespace, None, 0, 0)
            .unwrap();
    }
    scene.set_session_label("agent", Some("Unscoped".into()));
    assert_eq!(scene.dock_caption("agent"), None);
    let instance = scene
        .dock_instance(&scoped, &gauge)
        .unwrap()
        .instance_name
        .clone();
    let build = |compositor: &mut Compositor, scene: &mut SceneGraph| {
        compositor.prime_markdown_cache(scene);
        compositor.prime_truncation_cache(scene);
        compositor.build_windowed_frame(scene, 480, 256)
    };
    let capture = |compositor: &mut Compositor,
                   frame: &WindowedFrameBuild,
                   target: &FrameTarget,
                   display: Option<&str>| {
        compositor
            .capture_windowed_display(frame, target, display, format)
            .unwrap()
            .rgba
    };
    let named = |compositor: &mut Compositor, frame: &WindowedFrameBuild| {
        capture(compositor, frame, &selected_target, Some(&selected.name))
    };
    // Use the same Scene commit as MCP notification relabel before the frame.
    let caption = |scene: &mut SceneGraph, label: String| {
        scene.renew_lease(lease, 10_000).unwrap();
        scene.set_session_label(&scoped, Some(label));
    };
    let blank = build(&mut gpu.compositor, &mut scene);
    let blank_pixels = named(&mut gpu.compositor, &blank);
    let mirror_pixels = capture(
        &mut gpu.compositor,
        &blank,
        &selected_target,
        Some("MIRROR"),
    );
    let headless_without_caption = gpu.render_pixels(&mut scene);

    caption(&mut scene, "hud".into());
    let short = build(&mut gpu.compositor, &mut scene);
    let short_pixels = named(&mut gpu.compositor, &short);
    caption(&mut scene, "hud (2)".into());
    let suffixed = build(&mut gpu.compositor, &mut scene);
    let suffixed_pixels = named(&mut gpu.compositor, &suffixed);
    assert_eq!(scene.dock_caption(&scoped), Some("hud (2)"));
    assert_ne!(short_pixels, suffixed_pixels, "the suffix draws");
    let diff: Vec<_> = suffixed_pixels
        .chunks_exact(4)
        .zip(blank_pixels.chunks_exact(4))
        .enumerate()
        .filter(|(_, (label, blank))| label != blank)
        .collect();
    assert!(diff.len() >= 20);
    let mut green_cores = 0;
    let mut fringe = None;
    for (index, (pixel, blank)) in diff {
        let (x, y) = (index % 480, index / 480);
        assert!(
            (24..108).contains(&x) && (30..70).contains(&y),
            "caption outside its reserved box at ({x}, {y}): {pixel:?} over {blank:?}"
        );
        // A low-coverage green glyph can still look blue over the clear color.
        assert!(
            pixel[1].saturating_add(1) >= blank[1]
                && pixel[0] <= blank[0].saturating_add(1)
                && pixel[2] <= blank[2].saturating_add(1),
            "secondary foreground moved away from green at ({x}, {y}): {pixel:?} over {blank:?}"
        );
        if pixel[1] >= blank[1].saturating_add(48) && pixel[1] > pixel[0].max(pixel[2]) {
            green_cores += 1;
        }
        if pixel[1] <= pixel[0].max(pixel[2]) {
            fringe.get_or_insert((x, y, pixel, blank));
        }
    }
    assert!(
        green_cores >= 8,
        "secondary foreground has only {green_cores} strong green pixels; fringe {fringe:?}"
    );
    if let Some((x, y, pixel, blank)) = fringe {
        eprintln!("caption fringe at ({x}, {y}): {pixel:?} over {blank:?}");
    }
    assert_ne!(
        gpu.compositor
            .display_frame_signature(&blank, &selected_target, Some(&selected.name)),
        gpu.compositor
            .display_frame_signature(&suffixed, &selected_target, Some(&selected.name)),
        "caption-only frame identity"
    );
    assert_eq!(
        gpu.compositor
            .display_frame_signature(&blank, &selected_target, Some("MIRROR")),
        gpu.compositor
            .display_frame_signature(&suffixed, &selected_target, Some("MIRROR"))
    );
    assert_eq!(
        capture(
            &mut gpu.compositor,
            &suffixed,
            &selected_target,
            Some("MIRROR")
        ),
        mirror_pixels
    );
    assert_eq!(
        capture(
            &mut gpu.compositor,
            &suffixed,
            &primary_target,
            Some("PRIMARY")
        ),
        capture(
            &mut gpu.compositor,
            &blank,
            &primary_target,
            Some("PRIMARY")
        )
    );
    assert_eq!(gpu.render_pixels(&mut scene), headless_without_caption);

    let long = format!("{}B", "A\u{0301}".repeat(10));
    caption(&mut scene, long.clone());
    let overflow = build(&mut gpu.compositor, &mut scene);
    let overflow_pixels = named(&mut gpu.compositor, &overflow);
    assert_eq!(scene.dock_caption(&scoped), Some(long.as_str()));
    let surface = HeadlessSurface::new(&gpu.compositor.device, 480, 256);
    let present = gpu.compositor.present_windowed_display(
        &overflow,
        &selected_target,
        Some(&selected.name),
        &surface,
    );
    assert!(present.gpu_submitted && present.signature_current);
    assert!(present.telemetry.widget_rasterized.is_empty());
    assert_eq!(raster_count(&gpu.compositor, &instance), 1);

    // The bundled monospace face advances 0.6 em per accented grapheme or
    // ellipsis. At 12 dp * 1.5 * 0.75, ten cells fit 84 px and eleven do not.
    caption(&mut scene, format!("{}…", "A\u{0301}".repeat(9)));
    let explicit = build(&mut gpu.compositor, &mut scene);
    assert_eq!(named(&mut gpu.compositor, &explicit), overflow_pixels);
    assert_ne!(overflow_pixels, suffixed_pixels);

    project(&mut scene, &primary);
    assert_eq!(
        widget_region(
            &gpu.render_pixels(&mut scene),
            480,
            (0, 0),
            (24, 30, 84, 40)
        ),
        widget_region(&overflow_pixels, 480, (0, 0), (24, 30, 84, 40)),
        "the direct headless path draws the same local caption when it owns the dock"
    );
}
