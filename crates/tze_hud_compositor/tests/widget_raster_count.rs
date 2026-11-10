//! Widget updates are proportional to change: publishing a parameter to one
//! widget re-rasterizes only that widget's SVG, not its neighbours.
//!
//! The proof counts rasterizations (per-instance counter on `WidgetRenderer`,
//! per-frame list on `FrameTelemetry::widget_rasterized`). The shipped status
//! indicator also checks independent badge/tooltip pixel regions. The frame
//! itself is still re-presented in full (no damage tracking).
//!
//! Set `TZE_HUD_SKIP_GPU_TESTS=1` to skip; run with `HEADLESS_FORCE_SOFTWARE=1`.

use std::collections::HashMap;

use tze_hud_compositor::{Compositor, CompositorError, surface::HeadlessSurface};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{GeometryPolicy, SceneId, WidgetInstance, WidgetParameterValue};
use tze_hud_telemetry::FrameTelemetry;
use tze_hud_widget::loader::{BundleScanResult, load_bundle_from_files};

macro_rules! bundle {
    ($name:literal, [$($file:literal),*]) => {
        load_bundle_from_files(
            $name,
            &[
                ("widget.toml", include_bytes!(concat!("../../../assets/widget_bundles/", $name, "/widget.toml")) as &[u8]),
                $(($file, include_bytes!(concat!("../../../assets/widget_bundles/", $name, "/", $file)) as &[u8])),*
            ],
            &tokens(),
        )
    };
}

/// The canonical design tokens the built-in bundles resolve against.
fn tokens() -> HashMap<String, String> {
    tze_hud_config::tokens::resolve_tokens(&Default::default(), &Default::default())
}

/// `#RRGGBB` token value as bytes.
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

fn render(c: &mut Compositor, scene: &mut SceneGraph, s: &mut HeadlessSurface) -> FrameTelemetry {
    c.prime_markdown_cache(scene);
    c.render_frame_headless(scene, s)
}

fn publish(scene: &mut SceneGraph, instance: &str, param: &str, v: f32) {
    let params = HashMap::from([(param.to_string(), WidgetParameterValue::F32(v))]);
    scene
        .publish_to_widget(instance, params, "agent-a", None, 0, None)
        .unwrap();
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
            (x..x + width).map(move |x| {
                let offset = (((origin.1 + y) * canvas_width + origin.0 + x) * 4) as usize;
                pixels[offset..offset + 4].try_into().unwrap()
            })
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

#[tokio::test]
async fn widget_param_update_rasterizes_only_that_instance() {
    let require_gpu = std::env::var("TZE_HUD_REQUIRE_GPU").is_ok_and(|v| v.trim() == "1");
    if std::env::var("TZE_HUD_SKIP_GPU_TESTS").is_ok_and(|v| v.trim() == "1") {
        assert!(
            !require_gpu,
            "TZE_HUD_REQUIRE_GPU=1 forbids skipping widget raster assertions"
        );
        eprintln!("SKIPPED: widget_raster_count TZE_HUD_SKIP_GPU_TESTS=1");
        return;
    }
    let compositor_init = {
        let _guard = crate::common::lock_gpu_init().await;
        Compositor::new_headless(512, 256).await
    };
    let mut compositor = match compositor_init {
        Ok(c) => c,
        Err(CompositorError::NoAdapter) => {
            assert!(
                !require_gpu,
                "TZE_HUD_REQUIRE_GPU=1 but no adapter for widget raster assertions"
            );
            eprintln!("SKIPPED: widget_raster_count no GPU adapter");
            return;
        }
        Err(e) => panic!("unexpected compositor error: {e}"),
    };
    let mut surface = HeadlessSurface::new(&compositor.device, 512, 256);
    compositor.init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

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
        let BundleScanResult::Ok(b) = bundle else {
            panic!("built-in bundle for {instance} failed to load");
        };
        let type_name = b.definition.id.clone();
        for (file, bytes) in b.svg_contents {
            compositor
                .widget_renderer_mut()
                .unwrap()
                .register_svg(&type_name, &file, bytes);
        }
        scene.widget_registry.register_definition(b.definition);
        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: type_name,
            tab_id: tab,
            geometry_override: None,
            contention_override: None,
            instance_name: instance.to_string(),
            current_params: HashMap::new(),
            auto_clear_ms: None,
        });
    }

    // Both widgets get their first raster.
    publish(&mut scene, "main-gauge", "level", 0.25);
    publish(&mut scene, "main-progress", "progress", 0.25);
    let first = render(&mut compositor, &mut scene, &mut surface);
    let count = |c: &Compositor, n: &str| c.widget_renderer().unwrap().raster_count(n);
    assert_eq!(
        (
            count(&compositor, "main-gauge"),
            count(&compositor, "main-progress")
        ),
        (1, 1)
    );
    assert_eq!(first.widget_rasterized.len(), 2);
    #[cfg(feature = "dev-mode")]
    {
        let work = compositor.take_work_counts().expect("first widget frame");
        assert_eq!((work.layout, work.raster, work.upload), (0, 2, 2));
    }

    // Publish to the gauge only: gauge +1, progress +0.
    publish(&mut scene, "main-gauge", "level", 0.75);
    let update = render(&mut compositor, &mut scene, &mut surface);
    assert_eq!(count(&compositor, "main-gauge"), 2);
    assert_eq!(count(&compositor, "main-progress"), 1);
    assert_eq!(update.widget_rasterized, ["main-gauge"]);
    #[cfg(feature = "dev-mode")]
    {
        let work = compositor.take_work_counts().expect("changed widget frame");
        assert_eq!((work.layout, work.raster, work.upload), (0, 1, 1));
        assert!(
            compositor.take_work_counts().is_none(),
            "single-frame drain"
        );
        // Nothing called the renderer after that drain: there is no synthetic
        // observation of an idle frame, even though the last image persists.
        assert!(compositor.take_work_counts().is_none(), "no new frame");
    }

    // Idle frame: nothing is re-rasterized.
    let idle = render(&mut compositor, &mut scene, &mut surface);
    assert_eq!(count(&compositor, "main-gauge"), 2);
    assert_eq!(count(&compositor, "main-progress"), 1);
    assert!(idle.widget_rasterized.is_empty());
    #[cfg(feature = "dev-mode")]
    {
        // This explicitly invoked frame still presents the full image, despite
        // no new shaping/raster/upload work. It is distinct from no render.
        let work = compositor
            .take_work_counts()
            .expect("actual unchanged frame");
        assert_eq!((work.layout, work.raster, work.upload), (0, 0, 0));

        // A real raster admission failure must count the entered invocation,
        // while recording no RGBA submission. Keep the live per-instance and
        // FrameTelemetry attempt observations independent of upload success.
        let ledger =
            tze_hud_resource::ResidentLedger::new(tze_hud_resource::ResidentLedgerLimits {
                aggregate_bytes: 0,
                resource_bytes: 0,
                widget_source_bytes: 0,
                widget_raster_bytes: 0,
                font_bytes: 0,
            });
        compositor.set_resident_ledger(ledger.clone());
        publish(&mut scene, "main-gauge", "level", 0.5);
        let denied = render(&mut compositor, &mut scene, &mut surface);
        let denied_work = compositor
            .take_work_counts()
            .expect("completed denied-raster frame");
        assert_eq!(
            (denied_work.layout, denied_work.raster, denied_work.upload),
            (0, 1, 0)
        );
        assert_eq!(count(&compositor, "main-gauge"), 3);
        assert_eq!(count(&compositor, "main-progress"), 1);
        assert_eq!(denied.widget_rasterized, ["main-gauge"]);
        assert_eq!(ledger.snapshot().class_denial_count, 1);
        assert!(compositor.take_work_counts().is_none());
        println!(
            "observed widget frames: first raster/upload=2/2; changed=1/1; unchanged=0/0; admission-denied=1/0"
        );
    }

    // The original observations, including zero-quota denial, are complete.
    // Reset only this fixture's ledger/renderer; the status section has its own
    // bounded admission and fresh texture/raster-count baseline on the same GPU.
    compositor.set_resident_ledger(tze_hud_resource::ResidentLedger::new(
        tze_hud_resource::ResidentLedgerLimits {
            aggregate_bytes: 2 * 1024 * 1024,
            resource_bytes: 0,
            widget_source_bytes: 1024 * 1024,
            widget_raster_bytes: 1024 * 1024,
            font_bytes: 0,
        },
    ));
    compositor.init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut status_scene = SceneGraph::new(512.0, 256.0);
    let tab = status_scene.create_tab("Status", 0).unwrap();
    let BundleScanResult::Ok(status_bundle) = bundle!("status-indicator", ["indicator.svg"]) else {
        panic!("shipped status-indicator bundle failed to load");
    };
    let type_name = status_bundle.definition.id.clone();
    for (file, bytes) in status_bundle.svg_contents {
        compositor
            .widget_renderer_mut()
            .unwrap()
            .register_svg(&type_name, &file, bytes);
    }
    status_scene
        .widget_registry
        .register_definition(status_bundle.definition);
    status_scene
        .widget_registry
        .register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: type_name,
            tab_id: tab,
            geometry_override: Some(GeometryPolicy::Relative {
                x_pct: 32.0 / 512.0,
                y_pct: 32.0 / 256.0,
                width_pct: 320.0 / 512.0,
                height_pct: 124.0 / 256.0,
            }),
            contention_override: None,
            instance_name: "status-matrix".into(),
            current_params: HashMap::new(),
            auto_clear_ms: None,
        });
    let GeometryPolicy::Relative {
        x_pct,
        y_pct,
        width_pct,
        height_pct,
    } = status_scene
        .widget_registry
        .resolve_geometry_policy_for_instance("status-matrix", None)
        .unwrap()
    else {
        panic!("status fixture must resolve relative geometry");
    };
    assert_eq!(
        (
            compositor.width as f32 * width_pct,
            compositor.height as f32 * height_pct
        ),
        (320.0, 124.0)
    );
    let origin = (
        (compositor.width as f32 * x_pct) as u32,
        (compositor.height as f32 * y_pct) as u32,
    );
    let region = |pixels: &[u8], local| widget_region(pixels, 512, origin, local);
    let clean = render(&mut compositor, &mut status_scene, &mut surface);
    assert!(clean.widget_rasterized.is_empty());
    let baseline = surface.read_pixels(&compositor.device);
    #[cfg(feature = "dev-mode")]
    {
        let work = compositor
            .take_work_counts()
            .expect("clean status baseline");
        assert_eq!((work.layout, work.raster, work.upload), (0, 0, 0));
    }
    let set_status = |scene: &mut SceneGraph,
                      theme: &str,
                      status: &str,
                      label: &str,
                      reason: &str,
                      tooltip: f32| {
        let definition = &scene.widget_registry.definitions["status-indicator"];
        let configured = configured_instance(
            definition,
            tab,
            "status-matrix",
            serde_json::json!({"theme":theme}),
        );
        // Preserve the geometry and model publication while applying the owner
        // setting produced by the config builder.
        scene
            .widget_registry
            .instances
            .get_mut("status-matrix")
            .unwrap()
            .current_params
            .insert("theme".into(), configured.current_params["theme"].clone());
        scene
            .set_widget_param_local(
                "status-matrix",
                "tooltip_visible",
                WidgetParameterValue::F32(tooltip),
            )
            .unwrap();
        scene
            .publish_to_widget(
                "status-matrix",
                HashMap::from([
                    ("status".into(), WidgetParameterValue::Enum(status.into())),
                    ("label".into(), WidgetParameterValue::String(label.into())),
                    ("reason".into(), WidgetParameterValue::String(reason.into())),
                ]),
                "agent-a",
                None,
                0,
                None,
            )
            .unwrap();
    };
    // Contract literals come from the shipped SVG/TOML, not the rasterizer.
    // Badge tile: x 280..320, y 0..40, surface.container.high at
    // opacity.surface. The minimal dot (r=7) is centred at (300, 20); the
    // system/friendly tonal disc (r=13, fill-opacity 0.22) is sampled above
    // its glyph.
    let tile = (token_rgb("color.surface.container.high"), 0.92);
    let panel_roi = (24, 52, 260, 3);
    let label_roi = (32, 63, 200, 18);
    let reason_roi = (16, 92, 280, 16);
    let statuses = ["online", "away", "busy", "offline"];
    // status -> color role, from the shipped value maps.
    let fills = [
        "color.success",
        "color.caution",
        "color.error",
        "color.outline",
    ]
    .map(token_rgb);
    let mut glyphs = HashMap::new();
    for (theme, badge_roi, fill_opacity) in [
        ("minimal", (300, 20, 1, 1), 1.0),
        ("system", (300, 10, 1, 1), 0.22),
        ("friendly", (300, 10, 1, 1), 0.22),
    ] {
        let mut previous_badge = None;
        for (status, fill) in statuses.into_iter().zip(fills) {
            let before = count(&compositor, "status-matrix");
            set_status(
                &mut status_scene,
                theme,
                status,
                "READY",
                "ALL SYSTEMS GO",
                0.0,
            );
            let frame = render(&mut compositor, &mut status_scene, &mut surface);
            assert_eq!(frame.widget_rasterized, ["status-matrix"]);
            assert_eq!(count(&compositor, "status-matrix"), before + 1);
            #[cfg(feature = "dev-mode")]
            {
                let work = compositor.take_work_counts().expect("status raster/upload");
                assert_eq!((work.layout, work.raster, work.upload), (0, 1, 1));
            }
            let pixels = surface.read_pixels(&compositor.device);
            let badge = region(&pixels, badge_roi);
            if status == "online" {
                glyphs.insert(theme, region(&pixels, (286, 6, 28, 28)));
            }
            assert_eq!(
                status_scene.widget_registry.instances["status-matrix"].current_params["theme"],
                WidgetParameterValue::Enum(theme.into())
            );
            let background = region(&baseline, badge_roi)[0];
            assert_eq!(background[3], 255);
            let expected = expected_badge_pixel(&[tile, (fill, fill_opacity)], background);
            assert!(badge[0] != background, "invisible {theme}/{status} badge");
            assert_eq!(badge[0][3], 255, "opaque readback is not group opacity");
            for channel in 0..3 {
                assert!(
                    badge[0][channel].abs_diff(expected[channel]) <= 4,
                    "{theme}/{status} fill channel {channel}: actual {:?}, expected {expected:?}",
                    badge[0]
                );
            }
            if let Some(previous) = &previous_badge {
                assert!(
                    badge != *previous,
                    "status change retained stale badge pixels"
                );
            }
            previous_badge = Some(badge.clone());
            for roi in [panel_roi, label_roi, reason_roi] {
                assert!(
                    region(&pixels, roi) == region(&baseline, roi),
                    "hidden tooltip leaked into its local region"
                );
            }
            let idle = render(&mut compositor, &mut status_scene, &mut surface);
            assert!(idle.widget_rasterized.is_empty());
            assert_eq!(count(&compositor, "status-matrix"), before + 1);
            #[cfg(feature = "dev-mode")]
            {
                let work = compositor
                    .take_work_counts()
                    .expect("unchanged status frame");
                assert_eq!((work.layout, work.raster, work.upload), (0, 0, 0));
                assert!(compositor.take_work_counts().is_none());
            }
            assert!(
                surface.read_pixels(&compositor.device) == pixels,
                "unchanged {theme}/{status} frame changed pixels"
            );
            println!(
                "status-indicator {theme}/{status}: badge {:?}, expected {expected:?}; raster/upload=1/1, unchanged=0/0",
                badge[0]
            );
        }
    }

    assert_ne!(
        glyphs["system"], glyphs["friendly"],
        "owner friendly theme must draw a distinct glyph across model publishes"
    );
    let hidden = surface.read_pixels(&compositor.device);
    let badge_roi = (280, 0, 40, 40);
    let mut tooltip_frames = Vec::new();
    for (label, reason, visible) in [
        ("READY", "ALL SYSTEMS GO", 1.0),
        ("PAUSED", "ALL SYSTEMS GO", 1.0),
        ("PAUSED", "AWAITING OPERATOR", 1.0),
        ("PAUSED", "AWAITING OPERATOR", 0.0),
    ] {
        let before = count(&compositor, "status-matrix");
        set_status(
            &mut status_scene,
            "friendly",
            "offline",
            label,
            reason,
            visible,
        );
        let frame = render(&mut compositor, &mut status_scene, &mut surface);
        assert_eq!(frame.widget_rasterized, ["status-matrix"]);
        assert_eq!(count(&compositor, "status-matrix"), before + 1);
        #[cfg(feature = "dev-mode")]
        {
            let work = compositor.take_work_counts().expect("tooltip update");
            assert_eq!((work.layout, work.raster, work.upload), (0, 1, 1));
        }
        let pixels = surface.read_pixels(&compositor.device);
        assert!(
            region(&pixels, badge_roi) == region(&hidden, badge_roi),
            "tooltip changed badge"
        );
        let idle = render(&mut compositor, &mut status_scene, &mut surface);
        assert!(idle.widget_rasterized.is_empty());
        assert_eq!(count(&compositor, "status-matrix"), before + 1);
        #[cfg(feature = "dev-mode")]
        {
            let work = compositor
                .take_work_counts()
                .expect("unchanged tooltip frame");
            assert_eq!((work.layout, work.raster, work.upload), (0, 0, 0));
            assert!(compositor.take_work_counts().is_none());
        }
        assert!(
            surface.read_pixels(&compositor.device) == pixels,
            "unchanged tooltip frame changed pixels"
        );
        tooltip_frames.push(pixels);
    }
    // Report the existing isolation predicates before a foreground failure can
    // stop their later assertions. These diagnostics do not change the oracle.
    println!(
        "tooltip binding isolation: label_changed={} reason_stable_on_label={} reason_changed={} label_stable_on_reason={}",
        region(&tooltip_frames[0], label_roi) != region(&tooltip_frames[1], label_roi),
        region(&tooltip_frames[0], reason_roi) == region(&tooltip_frames[1], reason_roi),
        region(&tooltip_frames[1], reason_roi) != region(&tooltip_frames[2], reason_roi),
        region(&tooltip_frames[1], label_roi) == region(&tooltip_frames[2], label_roi),
    );
    for (case, pixels) in tooltip_frames[..3].iter().enumerate() {
        assert!(
            region(pixels, panel_roi) != region(&baseline, panel_roi),
            "tooltip panel missing"
        );
        for (name, roi) in [("label", label_roi), ("reason", reason_roi)] {
            let roi_pixels = region(pixels, roi);
            let peak_pixel = roi_pixels
                .iter()
                .max_by_key(|p| p[..3].iter().copied().min().unwrap_or(0))
                .copied()
                .unwrap_or([0; 4]);
            let peak_min_rgb = peak_pixel[..3].iter().copied().min().unwrap_or(0);
            let channel_max: [u8; 3] = std::array::from_fn(|channel| {
                roi_pixels.iter().map(|p| p[channel]).max().unwrap_or(0)
            });
            // The shipped opaque panel color; all shown cases bind
            // tooltip_visible=1, so its interior has no text coverage.
            let expected_panel = expected_badge_pixel(
                &[(token_rgb("color.surface.container.highest"), 1.0)],
                region(&baseline, roi)[0],
            );
            let maximum_panel_contrast = roi_pixels
                .iter()
                .map(|p| {
                    (0..3)
                        .map(|channel| p[channel].abs_diff(expected_panel[channel]))
                        .max()
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            let bright_pixels = roi_pixels
                .iter()
                .filter(|p| p[..3].iter().all(|v| *v > 180))
                .count();
            let case_name = ["ready", "label-paused", "reason-awaiting-operator"][case];
            println!(
                "tooltip diagnostic case={case_name} roi={name} local_bounds={roi:?} origin={origin:?} peak_min_rgb={peak_min_rgb} peak_pixel={peak_pixel:?} channel_max={channel_max:?} foreground_gt180_count={bright_pixels} expected_panel={expected_panel:?} maximum_panel_contrast={maximum_panel_contrast} observed_panel_sample={:?}",
                region(pixels, panel_roi)[0],
            );
            assert!(
                region(pixels, roi).iter().any(|p| (0..3).all(
                    |channel| u16::from(p[channel]) >= u16::from(expected_panel[channel]) + 64
                )),
                "tooltip text region contains no foreground"
            );
        }
    }
    assert!(
        region(&tooltip_frames[0], label_roi) != region(&tooltip_frames[1], label_roi),
        "label binding did not change label pixels"
    );
    assert!(
        region(&tooltip_frames[0], reason_roi) == region(&tooltip_frames[1], reason_roi),
        "label update changed reason pixels"
    );
    assert!(
        region(&tooltip_frames[1], reason_roi) != region(&tooltip_frames[2], reason_roi),
        "reason binding did not change reason pixels"
    );
    assert!(
        region(&tooltip_frames[1], label_roi) == region(&tooltip_frames[2], label_roi),
        "reason update changed label pixels"
    );
    assert!(
        tooltip_frames[3] == hidden,
        "hidden tooltip retained stale pixels"
    );
    status_scene
        .clear_widget_for_publisher("status-matrix", "agent-a")
        .unwrap();
    let cleared = render(&mut compositor, &mut status_scene, &mut surface);
    assert!(cleared.widget_rasterized.is_empty());
    assert!(
        surface.read_pixels(&compositor.device) == baseline,
        "clear retained stale status texture"
    );
    #[cfg(feature = "dev-mode")]
    {
        let work = compositor.take_work_counts().expect("cleared status frame");
        assert_eq!((work.layout, work.raster, work.upload), (0, 0, 0));
    }
    println!(
        "status-indicator: 12 local theme/status cells, separate label/reason controls, hidden/clear baselines, adapter {:?}",
        compositor.adapter_info()
    );
    // Shipped severity fills, including return-to-info after warning/error.
    // Fixed logical geometry makes the sampled bar interior independent of
    // display-relative layout; all expected colors come from theme tokens.
    for (loaded, level_name, width, height, sample) in [
        (
            bundle!("gauge", ["background.svg", "fill.svg"]),
            "level",
            72.0,
            224.0,
            (36, 100),
        ),
        (
            bundle!("progress-bar", ["track.svg", "fill.svg"]),
            "progress",
            360.0,
            52.0,
            (100, 37),
        ),
    ] {
        let BundleScanResult::Ok(bundle) = loaded else {
            panic!("severity bundle");
        };
        let name = bundle.definition.id.clone();
        for (file, bytes) in bundle.svg_contents {
            compositor
                .widget_renderer_mut()
                .unwrap()
                .register_svg(&name, &file, bytes);
        }
        let mut severity_frames: HashMap<&str, Vec<u8>> = HashMap::new();
        for owner in [false, true] {
            let mut scene = SceneGraph::new(512.0, 256.0);
            let tab = scene.create_tab("Severity", 0).unwrap();
            let initial = if owner {
                serde_json::json!({"fill_color":[12,210,123,255]})
            } else {
                serde_json::json!({})
            };
            let mut instance = configured_instance(&bundle.definition, tab, &name, initial);
            instance.geometry_override = Some(GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: width / 512.0,
                height_pct: height / 256.0,
            });
            scene
                .widget_registry
                .register_definition(bundle.definition.clone());
            scene.widget_registry.register_instance(instance);
            for severity in ["info", "warning", "error", "info"] {
                scene
                    .publish_to_widget(
                        &name,
                        HashMap::from([
                            (level_name.into(), WidgetParameterValue::F32(0.75)),
                            (
                                "severity".into(),
                                WidgetParameterValue::Enum(severity.into()),
                            ),
                        ]),
                        "model",
                        None,
                        0,
                        None,
                    )
                    .unwrap();
                render(&mut compositor, &mut scene, &mut surface);
                let pixels = surface.read_pixels(&compositor.device);
                if severity != "info" {
                    if owner {
                        let baseline = &severity_frames[severity];
                        let changed = pixels
                            .chunks_exact(4)
                            .zip(baseline.chunks_exact(4))
                            .filter(|(actual, expected)| actual != expected)
                            .count();
                        assert_eq!(
                            changed, 0,
                            "{name}/{severity}: owner info fill altered non-info pixels, including rounded edges"
                        );
                    } else {
                        severity_frames.insert(severity, pixels.clone());
                    }
                }
                let actual = widget_region(&pixels, 512, (0, 0), (sample.0, sample.1, 1, 1))[0];
                let expected = match severity {
                    "warning" => token_rgb("color.caution"),
                    "error" => token_rgb("color.error"),
                    _ if owner => [12, 210, 123],
                    _ => token_rgb("color.primary"),
                };
                for channel in 0..3 {
                    assert!(
                        actual[channel].abs_diff(expected[channel]) <= 2,
                        "{name} owner={owner} severity={severity}: {actual:?} vs {expected:?}"
                    );
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

    if std::env::var("TZE_HUD_SKIP_GPU_TESTS").is_ok_and(|v| v.trim() == "1") {
        eprintln!("SKIPPED: hidden widgets TZE_HUD_SKIP_GPU_TESTS=1");
        return;
    }
    let compositor_init = {
        let _guard = crate::common::lock_gpu_init().await;
        Compositor::new_headless(512, 256).await
    };
    let mut compositor = match compositor_init {
        Ok(c) => c,
        Err(CompositorError::NoAdapter) => {
            assert!(
                !std::env::var("TZE_HUD_REQUIRE_GPU").is_ok_and(|v| v.trim() == "1"),
                "TZE_HUD_REQUIRE_GPU=1 but no adapter"
            );
            return;
        }
        Err(e) => panic!("unexpected compositor error: {e}"),
    };
    let mut surface = HeadlessSurface::new(&compositor.device, 512, 256);
    compositor.init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(512.0, 256.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let BundleScanResult::Ok(b) = bundle!("gauge", ["background.svg", "fill.svg"]) else {
        panic!("gauge bundle failed to load");
    };
    let type_name = b.definition.id.clone();
    for (file, bytes) in b.svg_contents {
        compositor
            .widget_renderer_mut()
            .unwrap()
            .register_svg(&type_name, &file, bytes);
    }
    scene.widget_registry.register_definition(b.definition);
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: type_name,
        tab_id: tab,
        geometry_override: None,
        contention_override: None,
        instance_name: "main-gauge".to_string(),
        current_params: HashMap::new(),
        auto_clear_ms: None,
    });
    let count = |c: &Compositor| c.widget_renderer().unwrap().raster_count("main-gauge");
    // The frame with no published widget: what "nothing drawn" looks like.
    render(&mut compositor, &mut scene, &mut surface);
    let empty = surface.read_pixels(&compositor.device);

    // What the newest value (0.75) looks like when published directly.
    publish(&mut scene, "main-gauge", "level", 0.75);
    render(&mut compositor, &mut scene, &mut surface);
    let at_075 = surface.read_pixels(&compositor.device);
    assert!(at_075 != empty, "the gauge draws");

    publish(&mut scene, "main-gauge", "level", 0.25);
    render(&mut compositor, &mut scene, &mut surface);
    assert_eq!(count(&compositor), 2);

    scene.set_owner_hidden(SurfaceClasses {
        widgets: true,
        ..SurfaceClasses::NONE
    });
    // Published with a transition while hidden.
    let params = HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.75))]);
    scene
        .publish_to_widget("main-gauge", params, "agent-a", None, 500, None)
        .unwrap();
    let hidden = render(&mut compositor, &mut scene, &mut surface);
    assert_eq!(count(&compositor), 2, "no raster while hidden");
    assert!(hidden.widget_rasterized.is_empty());
    assert!(
        surface.read_pixels(&compositor.device) == empty,
        "nothing drawn while hidden"
    );

    scene.set_owner_hidden(SurfaceClasses::NONE);
    let shown = render(&mut compositor, &mut scene, &mut surface);
    assert_eq!(count(&compositor), 3, "one raster of the newest params");
    assert_eq!(shown.widget_rasterized, ["main-gauge"]);
    assert!(
        !compositor
            .widget_renderer()
            .unwrap()
            .has_active_transition(),
        "showing snaps: no transition from the value drawn before hiding"
    );
    // Raster rounding varies by one step; a transition starting from the
    // pre-hide 0.25 would differ by whole fill colors.
    let again = surface.read_pixels(&compositor.device);
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

/// The owner resizes a widget (control panel layout): it re-rasters once at
/// the new size; moving it without resizing re-rasters nothing.
#[tokio::test]
async fn resizing_a_widget_rerasters_it_and_moving_does_not() {
    if std::env::var("TZE_HUD_SKIP_GPU_TESTS").is_ok_and(|v| v.trim() == "1") {
        return;
    }
    let compositor_init = {
        let _guard = crate::common::lock_gpu_init().await;
        Compositor::new_headless(512, 256).await
    };
    let mut compositor = match compositor_init {
        Ok(c) => c,
        Err(CompositorError::NoAdapter) => {
            assert!(
                !std::env::var("TZE_HUD_REQUIRE_GPU").is_ok_and(|v| v.trim() == "1"),
                "TZE_HUD_REQUIRE_GPU=1 but no adapter"
            );
            return;
        }
        Err(e) => panic!("unexpected compositor error: {e}"),
    };
    let mut surface = HeadlessSurface::new(&compositor.device, 512, 256);
    compositor.init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(512.0, 256.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let BundleScanResult::Ok(b) = bundle!("gauge", ["background.svg", "fill.svg"]) else {
        panic!("built-in gauge bundle failed to load");
    };
    let type_name = b.definition.id.clone();
    for (file, bytes) in b.svg_contents {
        compositor
            .widget_renderer_mut()
            .unwrap()
            .register_svg(&type_name, &file, bytes);
    }
    scene.widget_registry.register_definition(b.definition);
    let at = |x_pct, width_pct| GeometryPolicy::Relative {
        x_pct,
        y_pct: 0.1,
        width_pct,
        height_pct: 0.5,
    };
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: type_name,
        tab_id: tab,
        geometry_override: Some(at(0.1, 0.25)),
        contention_override: None,
        instance_name: "g".to_string(),
        current_params: HashMap::new(),
        auto_clear_ms: None,
    });
    publish(&mut scene, "g", "level", 0.5);
    let count = |c: &Compositor| c.widget_renderer().unwrap().raster_count("g");
    let size = |c: &Compositor| {
        let e = c.widget_renderer().unwrap().texture_entry("g").unwrap();
        (e.width, e.height)
    };
    render(&mut compositor, &mut scene, &mut surface);
    assert_eq!((count(&compositor), size(&compositor)), (1, (128, 128)));

    assert!(scene.set_widget_geometry("g", at(0.5, 0.25)));
    let moved = render(&mut compositor, &mut scene, &mut surface);
    assert!(
        moved.widget_rasterized.is_empty(),
        "a move keeps the raster"
    );
    assert_eq!(count(&compositor), 1);

    assert!(scene.set_widget_geometry("g", at(0.5, 0.5)));
    let resized = render(&mut compositor, &mut scene, &mut surface);
    assert_eq!(resized.widget_rasterized, ["g"]);
    assert_eq!((count(&compositor), size(&compositor)), (2, (256, 128)));
    assert!(!scene.set_widget_geometry("missing", at(0.0, 0.1)));
}

/// A theme change re-registers a widget type's SVGs with new token values:
/// its instances re-raster once; the same bytes again cost nothing.
#[tokio::test]
async fn a_restyled_widget_type_rerasters_once() {
    if std::env::var("TZE_HUD_SKIP_GPU_TESTS").is_ok_and(|v| v.trim() == "1") {
        return;
    }
    let compositor_init = {
        let _guard = crate::common::lock_gpu_init().await;
        Compositor::new_headless(512, 256).await
    };
    let mut compositor = match compositor_init {
        Ok(c) => c,
        Err(CompositorError::NoAdapter) => {
            assert!(
                !std::env::var("TZE_HUD_REQUIRE_GPU").is_ok_and(|v| v.trim() == "1"),
                "TZE_HUD_REQUIRE_GPU=1 but no adapter"
            );
            return;
        }
        Err(e) => panic!("unexpected compositor error: {e}"),
    };
    let mut surface = HeadlessSurface::new(&compositor.device, 512, 256);
    compositor.init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = SceneGraph::new(512.0, 256.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let BundleScanResult::Ok(b) = bundle!("gauge", ["background.svg", "fill.svg"]) else {
        panic!("built-in gauge bundle failed to load");
    };
    let type_name = b.definition.id.clone();
    let svgs = b.svg_contents.clone();
    let register = |c: &mut Compositor, svgs: &HashMap<String, Vec<u8>>| {
        for (file, bytes) in svgs {
            c.widget_renderer_mut()
                .unwrap()
                .register_svg(&type_name, file, bytes.clone());
        }
    };
    register(&mut compositor, &svgs);
    scene.widget_registry.register_definition(b.definition);
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: type_name.clone(),
        tab_id: tab,
        geometry_override: None,
        contention_override: None,
        instance_name: "g".to_string(),
        current_params: HashMap::new(),
        auto_clear_ms: None,
    });
    publish(&mut scene, "g", "level", 0.5);
    render(&mut compositor, &mut scene, &mut surface);
    let count = |c: &Compositor| c.widget_renderer().unwrap().raster_count("g");
    assert_eq!(count(&compositor), 1);

    register(&mut compositor, &svgs);
    let same = render(&mut compositor, &mut scene, &mut surface);
    assert!(
        same.widget_rasterized.is_empty(),
        "same bytes keep the raster"
    );

    let restyled: HashMap<String, Vec<u8>> = svgs
        .iter()
        .map(|(file, bytes)| {
            let mut bytes = bytes.clone();
            bytes.extend_from_slice(b"<!-- restyled -->");
            (file.clone(), bytes)
        })
        .collect();
    register(&mut compositor, &restyled);
    let frame = render(&mut compositor, &mut scene, &mut surface);
    assert_eq!(frame.widget_rasterized, ["g"]);
    let next = render(&mut compositor, &mut scene, &mut surface);
    assert!(next.widget_rasterized.is_empty(), "once");
}

/// Render gauges sharing one box (each `(instance, level)`) in a fresh scene
/// and read the frame back.
fn overlapping_gauges_frame(
    compositor: &mut Compositor,
    surface: &mut HeadlessSurface,
    gauges: &[(&str, f32)],
) -> Vec<u8> {
    let mut scene = SceneGraph::new(512.0, 256.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let BundleScanResult::Ok(b) = bundle!("gauge", ["background.svg", "fill.svg"]) else {
        panic!("built-in gauge bundle failed to load");
    };
    let type_name = b.definition.id.clone();
    for (file, bytes) in b.svg_contents {
        compositor
            .widget_renderer_mut()
            .unwrap()
            .register_svg(&type_name, &file, bytes);
    }
    scene.widget_registry.register_definition(b.definition);
    for &(instance, level) in gauges {
        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: type_name.clone(),
            tab_id: tab,
            geometry_override: Some(GeometryPolicy::Relative {
                x_pct: 0.1,
                y_pct: 0.1,
                width_pct: 0.25,
                height_pct: 0.5,
            }),
            contention_override: None,
            instance_name: instance.to_string(),
            current_params: HashMap::new(),
            auto_clear_ms: None,
        });
        publish(&mut scene, instance, "level", level);
    }
    render(compositor, &mut scene, surface);
    surface.read_pixels(&compositor.device)
}

/// Every widget shares one z-order, so overlapping widgets stack by instance
/// name on every frame and every run, independent of the instance map's
/// iteration order (a `HashMap`, reseeded per scene).
#[tokio::test]
async fn overlapping_widgets_stack_by_instance_name() {
    if std::env::var("TZE_HUD_SKIP_GPU_TESTS").is_ok_and(|v| v.trim() == "1") {
        return;
    }
    let compositor_init = {
        let _guard = crate::common::lock_gpu_init().await;
        Compositor::new_headless(512, 256).await
    };
    let mut compositor = match compositor_init {
        Ok(c) => c,
        Err(CompositorError::NoAdapter) => {
            assert!(
                !std::env::var("TZE_HUD_REQUIRE_GPU").is_ok_and(|v| v.trim() == "1"),
                "TZE_HUD_REQUIRE_GPU=1 but no adapter"
            );
            return;
        }
        Err(e) => panic!("unexpected compositor error: {e}"),
    };
    let mut surface = HeadlessSurface::new(&compositor.device, 512, 256);
    compositor.init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut frame =
        |gauges: &[(&str, f32)]| overlapping_gauges_frame(&mut compositor, &mut surface, gauges);
    let full = frame(&[("b", 1.0)]);
    let empty = frame(&[("b", 0.0)]);
    // The full bar's interior: pixels where a full and an empty gauge differ,
    // as do all four neighbours (no antialiased edge). Bar and empty track
    // are opaque there, so only the top widget decides the colour.
    let (w, h) = (512usize, 256usize);
    let differs = |x: usize, y: usize| {
        let i = (y * w + x) * 4;
        full[i..i + 4] != empty[i..i + 4]
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
    for gauges in [[("a", 0.0), ("b", 1.0)], [("b", 1.0), ("a", 0.0)]] {
        assert!(
            shows(&frame(&gauges), &full, &empty),
            "b draws over a: {gauges:?}"
        );
    }
    for gauges in [[("a", 1.0), ("b", 0.0)], [("b", 0.0), ("a", 1.0)]] {
        assert!(
            shows(&frame(&gauges), &empty, &full),
            "b draws over a: {gauges:?}"
        );
    }
}

/// Real shipped bundle pixels, semantic color tokens and countdown raster work.
/// TestClock advances wall time; no host timer or synthetic SVG is the oracle.
#[tokio::test]
async fn countdown_kpi_and_stepper_render_their_shipped_contracts() {
    use std::sync::Arc;
    use tze_hud_scene::{Clock, TestClock, WidgetDeadline};
    let require_gpu = std::env::var("TZE_HUD_REQUIRE_GPU").is_ok_and(|v| v.trim() == "1");
    if std::env::var("TZE_HUD_SKIP_GPU_TESTS").is_ok_and(|v| v.trim() == "1") {
        assert!(!require_gpu, "required GPU bundle proof cannot skip");
        return;
    }
    let mut compositor = match crate::common::new_headless_serialized(360, 124).await {
        Ok(compositor) => compositor,
        Err(CompositorError::NoAdapter) if !require_gpu => return,
        Err(error) => panic!("bundle compositor: {error}"),
    };
    compositor.init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut surface = HeadlessSurface::new(&compositor.device, 360, 124);
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = SceneGraph::new_with_clock(360.0, 124.0, clock.clone());
    let tab = scene.create_tab("Bundles", 0).unwrap();
    for loaded in [
        bundle!("countdown", ["card.svg"]),
        bundle!("kpi", ["card.svg"]),
        bundle!("stepper", ["card.svg"]),
    ] {
        let BundleScanResult::Ok(bundle) = loaded else {
            panic!("shipped bundle must load")
        };
        let name = bundle.definition.id.clone();
        for (file, bytes) in bundle.svg_contents {
            compositor
                .widget_renderer_mut()
                .unwrap()
                .register_svg(&name, &file, bytes);
        }
        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: name.clone(),
            tab_id: tab,
            geometry_override: Some(GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 1.0,
                height_pct: 1.0,
            }),
            contention_override: None,
            instance_name: name,
            current_params: bundle
                .definition
                .parameter_schema
                .iter()
                .map(|p| (p.name.clone(), p.default_value.clone()))
                .collect(),
            auto_clear_ms: None,
        });
        scene.widget_registry.register_definition(bundle.definition);
    }
    let publish = |scene: &mut SceneGraph, name, params| {
        scene
            .publish_to_widget(name, params, "agent", None, 200, None)
            .unwrap();
    };
    let string = |value: &str| WidgetParameterValue::String(value.into());
    let enumeration = |value: &str| WidgetParameterValue::Enum(value.into());
    let color_at = |pixels: &[u8], x, y, token| {
        let actual = widget_region(pixels, 360, (0, 0), (x, y, 1, 1))[0];
        let expected = token_rgb(token);
        for channel in 0..3 {
            assert!(
                actual[channel].abs_diff(expected[channel]) <= 4,
                "{token} at {x},{y}: {actual:?} != {expected:?}"
            );
        }
    };
    publish(
        &mut scene,
        "stepper",
        HashMap::from([
            ("step".into(), WidgetParameterValue::F32(2.0)),
            ("total".into(), WidgetParameterValue::F32(5.0)),
            ("state".into(), enumeration("blocked")),
        ]),
    );
    render(&mut compositor, &mut scene, &mut surface);
    let blocked = surface.read_pixels(&compositor.device);
    color_at(&blocked, 20, 54, "color.primary");
    color_at(&blocked, 49, 54, "color.caution");
    color_at(&blocked, 78, 54, "color.outline.variant");
    for (step, filled) in [(2.6, 2), (7.0, 5)] {
        publish(
            &mut scene,
            "stepper",
            HashMap::from([("step".into(), WidgetParameterValue::F32(step))]),
        );
        // A numeric transition uses injected monotonic time and then stops.
        render(&mut compositor, &mut scene, &mut surface);
        clock.advance(201);
        render(&mut compositor, &mut scene, &mut surface);
        let pixels = surface.read_pixels(&compositor.device);
        for dot in 1..=5 {
            color_at(
                &pixels,
                20 + (dot - 1) * 29,
                54,
                if dot < filled {
                    "color.primary"
                } else if dot == filled {
                    "color.caution"
                } else {
                    "color.outline.variant"
                },
            );
        }
        if filled == 2 {
            assert_eq!(
                pixels, blocked,
                "fractional step must floor, including antialiased edges"
            );
        }
    }
    scene
        .clear_widget_for_publisher("stepper", "agent")
        .unwrap();
    publish(
        &mut scene,
        "kpi",
        HashMap::from([
            ("value".into(), string("3 fail")),
            ("label".into(), string("Tests")),
            ("trend".into(), enumeration("down")),
            ("severity".into(), enumeration("error")),
        ]),
    );
    render(&mut compositor, &mut scene, &mut surface);
    let failure = surface.read_pixels(&compositor.device);
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
        HashMap::from([
            ("value".into(), string("")),
            ("trend".into(), enumeration("up")),
        ]),
    );
    render(&mut compositor, &mut scene, &mut surface);
    let changed = surface.read_pixels(&compositor.device);
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
    publish(
        &mut scene,
        "countdown",
        HashMap::from([
            (
                "ends_in_ms".into(),
                WidgetParameterValue::Deadline(WidgetDeadline::relative(300_000)),
            ),
            ("severity".into(), enumeration("error")),
        ]),
    );
    assert!(compositor.widget_deadline_needs_render(&scene));
    render(&mut compositor, &mut scene, &mut surface);
    let count = |c: &Compositor| c.widget_renderer().unwrap().raster_count("countdown");
    let first = count(&compositor);
    let five = surface.read_pixels(&compositor.device);
    let value = &compositor
        .widget_renderer()
        .unwrap()
        .texture_entry("countdown")
        .unwrap()
        .last_rendered_params["ends_in_ms"];
    assert_eq!(*value, string("5 min"));
    assert!(
        !compositor.has_inflight_animation(&scene),
        "clock text must not create frame-rate animation"
    );
    clock.advance(59_999);
    assert!(!compositor.widget_deadline_needs_render(&scene));
    render(&mut compositor, &mut scene, &mut surface);
    assert_eq!(count(&compositor), first);
    clock.advance(1);
    assert!(compositor.widget_deadline_needs_render(&scene));
    render(&mut compositor, &mut scene, &mut surface);
    assert_eq!(count(&compositor), first + 1);
    assert!(!compositor.has_inflight_animation(&scene));
    scene.pause_widget_ttls();
    clock.advance(240_000);
    assert_eq!(compositor.next_widget_deadline_wall_us(&scene), None);
    assert!(!compositor.widget_deadline_needs_render(&scene));
    render(&mut compositor, &mut scene, &mut surface);
    assert_eq!(count(&compositor), first + 1);
    scene.resume_widget_ttls();
    assert!(compositor.widget_deadline_needs_render(&scene));
    render(&mut compositor, &mut scene, &mut surface);
    let entry = compositor
        .widget_renderer()
        .unwrap()
        .texture_entry("countdown")
        .unwrap();
    assert_eq!(entry.last_rendered_params["ends_in_ms"], string("0:00"));
    assert_eq!(entry.last_rendered_params["severity"], enumeration("error"));
    assert_eq!(compositor.next_widget_deadline_wall_us(&scene), None);
    assert_ne!(five, surface.read_pixels(&compositor.device));
    assert_eq!(clock.now_us(), scene.now_wall_us());
}
