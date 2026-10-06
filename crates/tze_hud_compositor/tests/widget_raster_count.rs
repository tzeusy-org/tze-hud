//! Widget updates are proportional to change: publishing a parameter to one
//! widget re-rasterizes only that widget's SVG, not its neighbours.
//!
//! The proof counts rasterizations (per-instance counter on `WidgetRenderer`,
//! per-frame list on `FrameTelemetry::widget_rasterized`) rather than comparing
//! pixels. The frame itself is still re-presented in full (no damage tracking).
//!
//! Set `TZE_HUD_SKIP_GPU_TESTS=1` to skip; run with `HEADLESS_FORCE_SOFTWARE=1`.

use std::collections::HashMap;

use tze_hud_compositor::{Compositor, CompositorError, surface::HeadlessSurface};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{SceneId, WidgetInstance, WidgetParameterValue};
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

#[tokio::test]
async fn widget_param_update_rasterizes_only_that_instance() {
    if std::env::var("TZE_HUD_SKIP_GPU_TESTS").is_ok_and(|v| v.trim() == "1") {
        return;
    }
    let mut compositor = match Compositor::new_headless(512, 256).await {
        Ok(c) => c,
        Err(CompositorError::NoAdapter) => return,
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
        assert!(compositor.take_work_counts().is_none(), "single-frame drain");
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
        let work = compositor.take_work_counts().expect("actual unchanged frame");
        assert_eq!((work.layout, work.raster, work.upload), (0, 0, 0));
        assert_eq!(work.damage_px, 512 * 256);

        // A real raster admission failure must count the entered invocation,
        // while recording no RGBA submission. Keep the live per-instance and
        // FrameTelemetry attempt observations independent of upload success.
        let ledger = tze_hud_resource::ResidentLedger::new(tze_hud_resource::ResidentLedgerLimits {
            aggregate_bytes: 0,
            resource_bytes: 0,
            widget_source_bytes: 0,
            widget_raster_bytes: 0,
            font_bytes: 0,
        });
        compositor.set_resident_ledger(ledger.clone());
        publish(&mut scene, "main-gauge", "level", 0.5);
        let denied = render(&mut compositor, &mut scene, &mut surface);
        let denied_work = compositor.take_work_counts().expect("completed denied-raster frame");
        assert_eq!((denied_work.layout, denied_work.raster, denied_work.upload), (0, 1, 0));
        assert_eq!(count(&compositor, "main-gauge"), 3);
        assert_eq!(count(&compositor, "main-progress"), 1);
        assert_eq!(denied.widget_rasterized, ["main-gauge"]);
        assert_eq!(ledger.snapshot().class_denial_count, 1);
        assert!(compositor.take_work_counts().is_none());
        println!("observed widget frames: first raster/upload=2/2; changed=1/1; unchanged=0/0; admission-denied=1/0");
    }
}
