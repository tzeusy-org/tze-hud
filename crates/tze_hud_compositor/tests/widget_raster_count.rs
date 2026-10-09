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

/// The design tokens the built-in gauge and progress-bar SVGs reference.
fn tokens() -> HashMap<String, String> {
    [
        ("border.radius.large", "8"),
        ("border.radius.medium", "4"),
        ("color.backdrop.default", "#101018"),
        ("color.border.default", "#4A4A6A"),
        ("color.outline.default", "#000000"),
        ("color.severity.info", "#4A9EFF"),
        ("color.text.accent", "#4A9EFF"),
        ("color.text.primary", "#FFFFFF"),
        ("color.text.secondary", "#CCCCCC"),
        ("stroke.border.width", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
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

/// Independent color arithmetic: the shipped opaque fill is premultiplied by
/// its group's opacity in an RGBA8 sRGB raster, then blended over the clean
/// frame in linear light. No widget renderer or SVG rasterizer is an oracle.
fn expected_badge_pixel(fill: [u8; 3], opacity: f64, background: [u8; 4]) -> [u8; 4] {
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
    let alpha = (opacity * 255.0).round() / 255.0;
    let mut expected = [0, 0, 0, 255];
    for channel in 0..3 {
        let source = (f64::from(fill[channel]) * alpha).round() / 255.0;
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
    let mut compositor = match Compositor::new_headless(512, 256).await {
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
        assert_eq!(work.damage_px, 512 * 256);
        assert!(work.full_frame);
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
        assert_eq!(work.damage_px, work.pixels_damaged);
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
        assert_eq!(work.damage_px, 512 * 256);

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
                width_pct: 252.0 / 512.0,
                height_pct: 96.0 / 256.0,
            }),
            contention_override: None,
            instance_name: "status-matrix".into(),
            current_params: HashMap::new(),
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
        (252.0, 96.0)
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
        scene
            .publish_to_widget(
                "status-matrix",
                HashMap::from([
                    ("theme".into(), WidgetParameterValue::Enum(theme.into())),
                    ("status".into(), WidgetParameterValue::Enum(status.into())),
                    ("label".into(), WidgetParameterValue::String(label.into())),
                    ("reason".into(), WidgetParameterValue::String(reason.into())),
                    ("tooltip_visible".into(), WidgetParameterValue::F32(tooltip)),
                ]),
                "agent-a",
                None,
                0,
                None,
            )
            .unwrap();
    };
    // Contract literals come from the shipped TOML, not the rasterizer. This
    // badge interior is above/left of the glyphs and inside every fill shape.
    let badge_roi = (228, 7, 1, 1);
    let panel_roi = (24, 32, 180, 3);
    let label_roi = (58, 34, 152, 14);
    let reason_roi = (18, 53, 204, 14);
    let statuses = ["online", "away", "busy", "offline"];
    let mut previous_badge = None;
    for (theme, opacity, fills) in [
        (
            "minimal",
            0.8,
            [
                [0x22, 0xC5, 0x5E],
                [0xA3, 0xA3, 0xA3],
                [0xF5, 0x9E, 0x0B],
                [0x6B, 0x72, 0x80],
            ],
        ),
        (
            "system",
            0.82,
            [
                [0x4F, 0xB5, 0x43],
                [0xD9, 0x77, 0x06],
                [0xDC, 0x26, 0x26],
                [0x6B, 0x72, 0x80],
            ],
        ),
        (
            "friendly",
            0.82,
            [
                [0x7D, 0xD3, 0xA7],
                [0xC4, 0xB5, 0xFD],
                [0xEA, 0x58, 0x0C],
                [0x94, 0xA3, 0xB8],
            ],
        ),
    ] {
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
            let background = region(&baseline, badge_roi)[0];
            assert_eq!(background[3], 255);
            let expected = expected_badge_pixel(fill, opacity, background);
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

    let hidden = surface.read_pixels(&compositor.device);
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
            // Independent shipped friendly-panel literal; all shown cases bind
            // tooltip_visible=1, so its opaque interior has no text coverage.
            let expected_panel =
                expected_badge_pixel([0x12, 0x20, 0x18], 1.0, region(&baseline, roi)[0]);
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
}
