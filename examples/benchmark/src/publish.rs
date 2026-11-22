//! Publish-path benchmark scenarios: what agents actually send.
//!
//! Each scenario starts the runtime's real MCP HTTP server over a headless
//! runtime's shared scene and, once per frame, makes one `hud_publish` call
//! over loopback HTTP the way an agent does: a notification, a widget
//! parameter update, or a portal append. The call's round trip (request
//! written to reply read, so it includes admission and the scene commit) is
//! the `input_to_scene_commit` sample; the frame rendered after it gives
//! `frame_time`, and call start to frame end gives `input_to_next_present`.
//! A refused call is an invariant violation, so a broken publish fails the
//! zero-counter gate rather than looking fast.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::info;

use tze_hud_input::{PointerEvent, PointerEventKind};
use tze_hud_runtime::headless::{HeadlessConfig, HeadlessRuntime};
use tze_hud_runtime::portal::{PortalHandle, PortalHub};
use tze_hud_runtime::threads::{ShutdownReason, ShutdownToken};
use tze_hud_runtime::{McpServerConfig, start_mcp_http_server_with_render_wake};
use tze_hud_scene::types::{
    ContentionPolicy, GeometryPolicy, HitResult, RenderingPolicy, SceneId, WidgetDefinition,
    WidgetInstance, WidgetParamConstraints, WidgetParamType, WidgetParameterDeclaration,
    WidgetParameterValue, ZoneInteractionKind,
};
use tze_hud_telemetry::SessionSummary;

use crate::ScenarioResult;

const PSK: &str = "benchmark-publish-psk";
const WIDTH: u32 = crate::BENCHMARK_WIDTH;
const HEIGHT: u32 = crate::BENCHMARK_HEIGHT;
/// Frames rendered before the timed window (not recorded).
const WARMUP_FRAMES: u64 = 5;

/// The three agent publishes the gate times.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Publish {
    Notification,
    WidgetParams,
    PortalAppend,
}

impl Publish {
    pub(crate) fn session_name(self) -> &'static str {
        match self {
            Self::Notification => "notification_publish",
            Self::WidgetParams => "widget_param_update",
            Self::PortalAppend => "portal_append",
        }
    }

    /// The `hud_publish` arguments for frame `frame`.
    fn arguments(self, frame: u64) -> Value {
        match self {
            Self::Notification => json!({
                "surface": "zone:notification-area",
                "key": "benchmark",
                "content": {
                    "title": "Build",
                    "body": format!("Step {frame} passed."),
                    "actions": [{"label": "Open", "callback_id": "open"}],
                },
            }),
            Self::WidgetParams => json!({
                "surface": "widget:gauge",
                "params": {"level": (frame % 101) as f64 / 100.0},
            }),
            Self::PortalAppend => json!({
                "surface": "portal:benchmark",
                "content": format!("step {frame} passed\n"),
            }),
        }
    }
}

fn server_config() -> McpServerConfig {
    McpServerConfig {
        startup: None,
        text_fit: None,
        publication_defaults: Default::default(),
        widget_transition_ms: tze_hud_config::tokens::resolve_motion_duration_ms(
            &Default::default(),
            "motion.state.ms",
        ),
        bind_addrs: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)],
        late_tailnet_port: None,
        agents: tze_hud_scene::config::AgentDirectory::unrestricted(PSK).shared(),
        presents: None,
        surfaces: None,
        capture: None,
        restart: None,
        update: None,
        bind_gate: None,
        pairing: None,
        appearance: None,
    }
}

/// The default zones plus one `gauge` widget instance to update.
async fn prepare_scene(runtime: &HeadlessRuntime) {
    let state = runtime.shared_state().lock().await;
    let mut scene = state.scene.lock().await;
    scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
    let tab_id = scene.create_tab("Benchmark", 0).expect("create tab");
    scene.widget_registry.register_definition(WidgetDefinition {
        id: "gauge".to_string(),
        name: "Gauge".to_string(),
        description: "Benchmark gauge".to_string(),
        parameter_schema: vec![WidgetParameterDeclaration {
            audience: Default::default(),
            name: "level".to_string(),
            param_type: WidgetParamType::F32,
            default_value: WidgetParameterValue::F32(0.0),
            constraints: Some(WidgetParamConstraints {
                f32_min: Some(0.0),
                f32_max: Some(1.0),
                ..Default::default()
            }),
        }],
        layers: vec![],
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.2,
            height_pct: 0.1,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::LatestWins,
        max_publishers: WidgetDefinition::default_max_publishers(),
        ephemeral: false,
        hover_behavior: None,
    });
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "gauge".to_string(),
        current_params: HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.0))]),
        auto_clear_ms: None,
    });
}

/// One `hud_publish` over loopback HTTP, as an agent sends it. Returns
/// whether the runtime accepted it.
async fn hud_publish(address: SocketAddr, arguments: &Value) -> bool {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "hud_publish", "arguments": arguments}})
    .to_string();
    let request = format!(
        "POST /mcp HTTP/1.0\r\nAuthorization: Bearer {PSK}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to the MCP listener");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send hud_publish");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("read hud_publish reply");
    let raw = String::from_utf8_lossy(&raw);
    let Some((_, body)) = raw.split_once("\r\n\r\n") else {
        return false;
    };
    serde_json::from_str::<Value>(body).is_ok_and(|reply| {
        reply.get("error").is_none() && reply["result"]["isError"].as_bool() != Some(true)
    })
}

/// Hover the notification's action, then move off it, through the runtime's
/// input processor: the local feedback the owner sees before any agent does.
/// Records the on-card event's local acknowledgement time.
async fn record_hover_ack(runtime: &mut HeadlessRuntime, summary: &mut SessionSummary) {
    let scene_arc = runtime.shared_state().lock().await.scene.clone();
    let mut scene = scene_arc.lock().await;
    runtime
        .compositor
        .populate_zone_hit_regions(&mut scene, WIDTH as f32, HEIGHT as f32);
    let Some(action) = scene
        .overlay
        .zone_hit_regions
        .iter()
        .find(|region| matches!(region.kind, ZoneInteractionKind::Action { .. }))
        .map(|region| region.bounds)
    else {
        return;
    };
    for (x, y) in [
        (
            action.x + action.width / 2.0,
            action.y + action.height / 2.0,
        ),
        (1.0, 1.0),
    ] {
        let result = runtime.input_processor.process(
            &PointerEvent {
                x,
                y,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );
        if matches!(result.hit, HitResult::ZoneInteraction { .. }) {
            summary.input_to_local_ack.record(result.local_ack_us);
        }
    }
}

/// Run one publish scenario for `frame_count` frames.
pub(crate) async fn run(publish: Publish, frame_count: u64) -> ScenarioResult {
    let name = publish.session_name();
    info!("Running {name} scenario ({frame_count} frames)");
    let mut runtime = HeadlessRuntime::new(HeadlessConfig {
        width: WIDTH,
        height: HEIGHT,
        grpc_port: 0,
        agents: tze_hud_scene::config::AgentDirectory::unrestricted(PSK),
        config_toml: Some(String::new()),
    })
    .await
    .expect("HeadlessRuntime::new failed");
    prepare_scene(&runtime).await;
    let scene = runtime.shared_state().lock().await.scene.clone();
    let portals = PortalHandle::new(
        PortalHub::default(),
        Arc::new(tze_hud_scene::SystemClock::new()),
    );
    let shutdown = ShutdownToken::new();
    let (server, addresses) = start_mcp_http_server_with_render_wake(
        scene,
        server_config(),
        shutdown.clone(),
        Some(portals),
        Default::default(),
        Default::default(),
        Default::default(),
    )
    .await
    .expect("start the MCP listener");
    let address = addresses[0];

    for frame in 0..WARMUP_FRAMES {
        hud_publish(address, &publish.arguments(frame)).await;
        runtime.render_frame().await;
    }

    let session_start = Instant::now();
    let mut summary = SessionSummary::new();
    for frame in 0..frame_count {
        let arguments = publish.arguments(WARMUP_FRAMES + frame);
        let start = Instant::now();
        let accepted = hud_publish(address, &arguments).await;
        summary
            .input_to_scene_commit
            .record(start.elapsed().as_micros() as u64);
        let mut telemetry = runtime.render_frame().await;
        summary
            .input_to_next_present
            .record(start.elapsed().as_micros() as u64);
        if matches!(publish, Publish::Notification) {
            record_hover_ack(&mut runtime, &mut summary).await;
        }
        telemetry.invariant_violations_this_frame = u32::from(!accepted);
        // Calls are sequential: nothing races the frame for the scene lock.
        telemetry.scene_lock_miss_count = 0;
        summary.record_frame(telemetry.frame_time_us, telemetry.tile_count);
        summary.record_frame_correctness(&telemetry);
    }
    summary.elapsed_us = session_start.elapsed().as_micros() as u64;
    summary.finalize();

    shutdown.trigger(ShutdownReason::Clean);
    let _ = server.await;
    ScenarioResult {
        name: name.to_string(),
        summary,
    }
}
