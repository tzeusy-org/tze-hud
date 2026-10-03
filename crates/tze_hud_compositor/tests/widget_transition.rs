//! Widget transitions: a publish with `transition_ms > 0` eases f32 and color
//! parameters, snaps enum parameters, and costs nothing once it has landed.
//!
//! Time is the scene's injected `TestClock`; nothing here sleeps.
//! Set `TZE_HUD_SKIP_GPU_TESTS=1` to skip; run with `HEADLESS_FORCE_SOFTWARE=1`.

use std::collections::HashMap;
use std::sync::Arc;

use tze_hud_compositor::{Compositor, CompositorError, surface::HeadlessSurface};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{Rgba, SceneId, WidgetInstance, WidgetParameterValue};
use tze_hud_scene::{DegradationLevel, TestClock};
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

const GAUGE: &str = "gauge-1";

struct Rig {
    compositor: Compositor,
    surface: HeadlessSurface,
    scene: SceneGraph,
    clock: Arc<TestClock>,
}

impl Rig {
    /// A gauge already on screen with `level = 0.0`, `fill_color = black`,
    /// `severity = info`, published with no transition.
    async fn new() -> Option<Rig> {
        if std::env::var("TZE_HUD_SKIP_GPU_TESTS").is_ok_and(|v| v.trim() == "1") {
            return None;
        }
        let mut compositor = match Compositor::new_headless(256, 256).await {
            Ok(c) => c,
            Err(CompositorError::NoAdapter) => return None,
            Err(e) => panic!("unexpected compositor error: {e}"),
        };
        let surface = HeadlessSurface::new(&compositor.device, 256, 256);
        compositor.init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

        let clock = Arc::new(TestClock::new(1_000));
        let mut scene = SceneGraph::new_with_clock(256.0, 256.0, clock.clone());
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
        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: type_name,
            tab_id: tab,
            geometry_override: None,
            contention_override: None,
            instance_name: GAUGE.to_string(),
            current_params: HashMap::new(),
        });
        let mut rig = Rig {
            compositor,
            surface,
            scene,
            clock,
        };
        rig.publish(
            [
                ("level", WidgetParameterValue::F32(0.0)),
                (
                    "fill_color",
                    WidgetParameterValue::Color(Rgba::new(0.0, 0.0, 0.0, 1.0)),
                ),
                ("severity", WidgetParameterValue::Enum("info".into())),
            ],
            0,
        );
        rig.render();
        Some(rig)
    }

    fn publish<const N: usize>(&mut self, params: [(&str, WidgetParameterValue); N], ms: u32) {
        let params = params
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        self.scene
            .publish_to_widget(GAUGE, params, "agent-a", None, ms, None)
            .unwrap();
    }

    fn render(&mut self) -> tze_hud_telemetry::FrameTelemetry {
        self.compositor.prime_markdown_cache(&self.scene);
        self.compositor
            .render_frame_headless(&mut self.scene, &self.surface)
    }

    /// Advance the injected clock, render, and return the params last rasterized.
    fn step(&mut self, ms: u64) -> HashMap<String, WidgetParameterValue> {
        self.clock.advance(ms);
        self.render();
        self.rendered()
    }

    fn rendered(&self) -> HashMap<String, WidgetParameterValue> {
        let wr = self.compositor.widget_renderer().unwrap();
        wr.texture_entry(GAUGE)
            .unwrap()
            .last_rendered_params
            .clone()
    }

    fn rasters(&self) -> u64 {
        self.compositor
            .widget_renderer()
            .unwrap()
            .raster_count(GAUGE)
    }
}

fn level(p: &HashMap<String, WidgetParameterValue>) -> f32 {
    match p["level"] {
        WidgetParameterValue::F32(v) => v,
        _ => panic!("level is not f32"),
    }
}

#[tokio::test]
async fn f32_param_eases_over_transition_ms() {
    let Some(mut rig) = Rig::new().await else {
        return;
    };
    rig.publish([("level", WidgetParameterValue::F32(1.0))], 200);
    assert!(
        level(&rig.step(0)) < 0.01,
        "starts from the on-screen value"
    );
    let mid = level(&rig.step(100));
    assert!(
        (mid - 0.5).abs() < 0.01,
        "intermediate at 100 ms, got {mid}"
    );
    assert_eq!(level(&rig.step(100)), 1.0, "final at >= 200 ms");
}

#[tokio::test]
async fn color_param_eases_component_wise() {
    let Some(mut rig) = Rig::new().await else {
        return;
    };
    rig.publish(
        [(
            "fill_color",
            WidgetParameterValue::Color(Rgba::new(1.0, 0.5, 0.0, 1.0)),
        )],
        200,
    );
    rig.step(0);
    let WidgetParameterValue::Color(mid) = rig.step(100)["fill_color"].clone() else {
        panic!("fill_color is not a color");
    };
    assert!((mid.r - 0.5).abs() < 0.01 && (mid.g - 0.25).abs() < 0.01 && mid.b == 0.0);
}

#[tokio::test]
async fn enum_param_snaps_while_f32_eases() {
    let Some(mut rig) = Rig::new().await else {
        return;
    };
    rig.publish(
        [
            ("severity", WidgetParameterValue::Enum("error".into())),
            ("level", WidgetParameterValue::F32(1.0)),
        ],
        200,
    );
    let p = rig.step(100);
    assert_eq!(p["severity"], WidgetParameterValue::Enum("error".into()));
    assert!(level(&p) < 0.9);
}

#[tokio::test]
async fn degraded_mode_snaps_widget_transition() {
    let Some(mut rig) = Rig::new().await else {
        return;
    };
    rig.compositor.degradation_level = DegradationLevel::Simplified;
    rig.publish([("level", WidgetParameterValue::F32(1.0))], 200);
    assert_eq!(level(&rig.step(0)), 1.0, "snaps with no time elapsed");
    assert_eq!(
        rig.rasters(),
        2,
        "one raster for the change, not one per tick"
    );
}

#[tokio::test]
async fn transition_stops_waking_after_completion() {
    let Some(mut rig) = Rig::new().await else {
        return;
    };
    assert!(
        rig.compositor.next_animation_deadline().is_none(),
        "idle widget schedules nothing"
    );

    rig.publish([("level", WidgetParameterValue::F32(1.0))], 200);
    rig.step(0);
    assert!(
        rig.compositor.next_animation_deadline().is_some(),
        "animating widget wakes the loop"
    );
    assert!(rig.compositor.has_inflight_animation(&rig.scene));

    rig.step(250);
    let landed = rig.rasters();
    assert!(rig.compositor.next_animation_deadline().is_none());
    assert!(!rig.compositor.has_inflight_animation(&rig.scene));
    assert!(rig.step(1000).contains_key("level"));
    assert_eq!(
        rig.rasters(),
        landed,
        "no raster after the transition lands"
    );
}
