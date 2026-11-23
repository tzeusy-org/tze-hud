//! Widget transitions: a publish with `transition_ms > 0` eases f32 and color
//! parameters, snaps enum parameters, and costs nothing once it has landed.
//!
//! Time is the scene's injected `TestClock`; nothing here sleeps.
//! Set `TZE_HUD_SKIP_GPU_TESTS=1` to skip; run with `HEADLESS_FORCE_SOFTWARE=1`.

use crate::common::{Gpu, publish_params, register_bundle, register_instance};
use serde_json::{Value, json};

use std::collections::HashMap;
use std::sync::Arc;

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{Rgba, WidgetParameterValue};
use tze_hud_scene::{DegradationLevel, TestClock};
use tze_hud_widget::loader::BundleScanResult;

const GAUGE: &str = "gauge-1";

struct Rig {
    gpu: Gpu,
    scene: SceneGraph,
    clock: Arc<TestClock>,
}

impl Rig {
    /// A gauge already on screen with `level = 0.0`, `color = black`,
    /// `severity = info`, published with no transition.
    async fn new() -> Option<Rig> {
        let mut gpu = Gpu::with_widgets(256, 256).await?;

        let clock = Arc::new(TestClock::new(1_000));
        let mut scene = SceneGraph::new_with_clock(256.0, 256.0, clock.clone());
        let tab = scene.create_tab("Main", 0).unwrap();
        let BundleScanResult::Ok(mut b) = bundle!("gauge", ["background.svg", "fill.svg"]) else {
            panic!("built-in gauge bundle failed to load");
        };
        // Synthetic model-color fixture: retain the generic color animation
        // seam without granting agents the shipped owner's fill_color setting.
        let color = b
            .definition
            .parameter_schema
            .iter_mut()
            .find(|p| p.name == "fill_color")
            .unwrap();
        color.name = "color".into();
        color.audience = tze_hud_scene::WidgetParamAudience::Model;
        for binding in b
            .definition
            .layers
            .iter_mut()
            .flat_map(|layer| &mut layer.bindings)
        {
            if binding.param == "fill_color" {
                binding.param = "color".into();
            }
        }
        let type_name = register_bundle(&mut gpu.compositor, &mut scene, BundleScanResult::Ok(b));
        register_instance(&mut scene, tab, &type_name, GAUGE, None, false);
        let mut rig = Rig { gpu, scene, clock };
        rig.publish(
            [
                ("level", WidgetParameterValue::F32(0.0)),
                (
                    "color",
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
        publish_params(&mut self.scene, GAUGE, "agent-a", ms, params);
    }

    fn render(&mut self) -> tze_hud_telemetry::FrameTelemetry {
        self.gpu.draw(&mut self.scene)
    }

    /// Advance the injected clock, render, and return the params last rasterized.
    fn step(&mut self, ms: u64) -> HashMap<String, WidgetParameterValue> {
        self.clock.advance(ms);
        self.render();
        self.rendered()
    }

    fn rendered(&self) -> HashMap<String, WidgetParameterValue> {
        let wr = self.gpu.compositor.widget_renderer().unwrap();
        wr.texture_entry(GAUGE)
            .unwrap()
            .last_rendered_params
            .clone()
    }

    fn assert_idle(&self) {
        assert!(!self.gpu.compositor.has_inflight_animation(&self.scene));
        assert!(self.gpu.compositor.next_animation_deadline().is_none());
    }

    fn widget_has_transition(&self) -> bool {
        self.gpu
            .compositor
            .widget_renderer()
            .unwrap()
            .has_active_transition()
    }

    fn active_duration(&self) -> u32 {
        self.scene.widget_registry.active_publishes[GAUGE]
            .last()
            .unwrap()
            .transition_ms
    }

    fn rasters(&self) -> u64 {
        self.gpu
            .compositor
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

const PSK: &str = "motion-test-key";

fn motion_config(ms: u32) -> tze_hud_mcp::McpConfig {
    let mut config = tze_hud_mcp::McpConfig::with_psk(PSK);
    config.widget_transition_ms = tze_hud_config::tokens::resolve_motion_duration_ms(
        &HashMap::from([("motion.state.ms".into(), ms.to_string())]),
        "motion.state.ms",
    );
    config
}

async fn widget_call(server: &tze_hud_mcp::McpServer, name: &str, params: Option<Value>) {
    let mut arguments = json!({"surface": format!("widget:{GAUGE}")});
    if let Some(params) = params {
        arguments["params"] = params;
    }
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    });
    let reply = server
        .dispatch(
            &request.to_string(),
            &tze_hud_mcp::CallerContext::with_bearer(PSK),
        )
        .await;
    let reply: Value = serde_json::from_str(&reply).expect("widget response is JSON");
    assert!(
        reply["error"].is_null()
            && reply["result"].is_object()
            && reply["result"]["isError"] != true,
        "{reply}"
    );
}

#[tokio::test]
async fn transition_stops_waking_after_completion() {
    let Some(mut rig) = Rig::new().await else {
        return;
    };
    rig.assert_idle();

    let shared = Arc::new(tokio::sync::Mutex::new(rig.scene.clone()));
    let server =
        tze_hud_mcp::McpServer::with_shared_scene(shared.clone()).with_config(motion_config(200));
    widget_call(
        &server,
        "hud_publish",
        Some(json!({
            "level": 1.0,
            "color": {"r": 1.0, "g": 0.5, "b": 0.0, "a": 1.0},
            "severity": "error",
            "label": "CPU",
        })),
    )
    .await;
    rig.scene = shared.lock().await.clone();
    assert_eq!(rig.active_duration(), 200);
    assert!(level(&rig.step(0)) < 0.01, "starts at the visible value");
    assert!(
        rig.gpu.compositor.next_animation_deadline().is_some(),
        "animating widget wakes the loop"
    );
    assert!(rig.gpu.compositor.has_inflight_animation(&rig.scene));
    assert_eq!(
        rig.rendered()["severity"],
        WidgetParameterValue::Enum("error".into())
    );
    assert_eq!(
        rig.rendered()["label"],
        WidgetParameterValue::String("CPU".into())
    );
    let midway = rig.step(100);
    assert!((level(&midway) - 0.5).abs() < 0.01);
    let WidgetParameterValue::Color(color) = midway["color"].clone() else {
        panic!("color must interpolate");
    };
    assert!((color.r - 0.5).abs() < 0.01 && (color.g - 0.25).abs() < 0.01 && color.b == 0.0);
    assert_eq!(
        midway["severity"],
        WidgetParameterValue::Enum("error".into())
    );
    assert_eq!(level(&rig.step(100)), 1.0, "final at 200 ms");
    let landed = rig.rasters();
    rig.assert_idle();
    assert_eq!(level(&rig.step(1_000)), 1.0);
    assert_eq!(
        rig.rasters(),
        landed,
        "no raster after the transition lands"
    );
    // The same MCP producer must preserve W1's zero-duration cancellation.
    widget_call(&server, "hud_publish", Some(json!({"level": 0.0}))).await;
    rig.scene = shared.lock().await.clone();
    rig.step(0);
    rig.step(100);
    assert!(rig.gpu.compositor.has_inflight_animation(&rig.scene));
    let server = server.with_config(motion_config(0));
    widget_call(&server, "hud_publish", Some(json!({"level": 0.0}))).await;
    rig.scene = shared.lock().await.clone();
    assert_eq!(level(&rig.step(0)), 0.0);
    rig.assert_idle();

    widget_call(&server, "hud_clear", None).await;
    {
        let mut scene = shared.lock().await;
        scene.clear_widget_for_publisher(GAUGE, "agent-a").unwrap();
        rig.scene = scene.clone();
    }
    rig.render();
    assert!(
        rig.gpu
            .compositor
            .widget_renderer()
            .unwrap()
            .texture_entry(GAUGE)
            .is_none()
    );
    assert!(!rig.gpu.compositor.has_inflight_animation(&rig.scene));
    let server = server.with_config(motion_config(200));
    widget_call(&server, "hud_publish", Some(json!({"level": 0.8}))).await;
    rig.scene = shared.lock().await.clone();
    assert_eq!(
        level(&rig.step(0)),
        0.8,
        "first appearance snaps despite positive duration"
    );
    rig.assert_idle();

    rig.gpu.compositor.degradation_level = DegradationLevel::Simplified;
    let before_snap = rig.rasters();
    widget_call(&server, "hud_publish", Some(json!({"level": 1.0}))).await;
    rig.scene = shared.lock().await.clone();
    assert_eq!(rig.active_duration(), 200);
    assert_eq!(
        level(&rig.step(0)),
        1.0,
        "simplified mode snaps on the first frame"
    );
    assert_eq!(rig.rasters(), before_snap + 1);
    rig.assert_idle();
    assert_eq!(level(&rig.step(1000)), 1.0);
    assert_eq!(
        rig.rasters(),
        before_snap + 1,
        "no further raster while idle"
    );
}

#[tokio::test]
async fn rewound_clock_still_lands_and_stops_waking() {
    let Some(mut rig) = Rig::new().await else {
        return;
    };
    rig.clock.advance(10_000);
    rig.publish([("level", WidgetParameterValue::F32(1.0))], 200);
    rig.step(0);
    rig.clock.set(1_000); // step far backward mid-transition
    rig.render();
    assert!(rig.widget_has_transition());
    assert_eq!(level(&rig.step(250)), 1.0);
    assert!(!rig.widget_has_transition());
    assert!(rig.gpu.compositor.next_animation_deadline().is_none());
}

#[tokio::test]
async fn retarget_mid_flight_is_continuous_and_uses_the_new_duration() {
    let Some(mut rig) = Rig::new().await else {
        return;
    };
    rig.publish([("level", WidgetParameterValue::F32(1.0))], 200);
    assert_eq!(rig.active_duration(), 200);
    rig.step(0);
    let before = level(&rig.step(100)); // ~0.5 on screen
    rig.publish([("level", WidgetParameterValue::F32(0.0))], 400);
    assert_eq!(rig.active_duration(), 400);
    let start = level(&rig.step(0));
    assert!(
        (start - before).abs() < 0.01,
        "continuous: {before} -> {start}"
    );
    let mid = level(&rig.step(200));
    assert!(
        (mid - before / 2.0).abs() < 0.01,
        "halfway at the new 400 ms, got {mid}"
    );
    assert_eq!(
        level(&rig.step(200)),
        0.0,
        "lands exactly on the new target"
    );

    // Duration zero must interrupt a running transition both when choosing a
    // different target and when choosing the same target as that old animation.
    for instant_target in [0.0, 1.0] {
        rig.publish([("level", WidgetParameterValue::F32(0.0))], 0);
        rig.step(0);
        rig.publish([("level", WidgetParameterValue::F32(1.0))], 200);
        rig.step(0);
        let halfway = level(&rig.step(100));
        assert!((halfway - 0.5).abs() < 0.01, "active halfway: {halfway}");
        assert!(rig.widget_has_transition());
        assert!(rig.gpu.compositor.has_inflight_animation(&rig.scene));
        assert!(rig.gpu.compositor.next_animation_deadline().is_some());

        rig.publish([("level", WidgetParameterValue::F32(instant_target))], 0);
        assert_eq!(
            level(&rig.step(0)),
            instant_target,
            "zero duration snaps without advancing the clock"
        );
        assert!(
            !rig.widget_has_transition(),
            "the old instance animation is cancelled"
        );
        assert!(!rig.gpu.compositor.has_inflight_animation(&rig.scene));
        assert!(rig.gpu.compositor.next_animation_deadline().is_none());
        let snapped_rasters = rig.rasters();
        assert_eq!(level(&rig.step(1_000)), instant_target);
        assert_eq!(
            rig.rasters(),
            snapped_rasters,
            "a later injected tick must not resume the old transition"
        );
        println!("instant retarget {instant_target}: cancelled, no deadline or later raster");
    }
}
