//! Canonical app production-config boot gate.
//!
//! This test suite boots the runtime with the committed canonical app config:
//! `app/tze_hud_app/config/production.toml`.
//!
//! The gate is intentionally CI-visible:
//! - startup must succeed
//! - config-declared widget instances/types must be registered
//! - the config-selected theme (`[design_tokens] theme`) must be visible in zone policy
//!
//! If startup silently falls back to a default/headless policy, these assertions
//! fail even when runtime construction itself succeeds.
//!
//! GPU initialization shares the test-only async gate within this binary. The
//! guard is released before assertions or client traffic. Run this suite with
//! `just canonical-app-boot`, which selects the installed llvmpipe ICD.

#[path = "../../../crates/tze_hud_runtime/src/test_support.rs"]
mod gpu_init;

use std::collections::HashMap;

use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;
use tze_hud_scene::types::WidgetParameterValue;

const PRODUCTION_CONFIG: &str = include_str!("../config/production.toml");
fn canonical_headless_config() -> HeadlessConfig {
    HeadlessConfig {
        width: 320,
        height: 240,
        grpc_port: 0,
        agents: tze_hud_scene::config::AgentDirectory::unrestricted(
            "canonical-app-production-boot-test",
        ),
        config_toml: Some(PRODUCTION_CONFIG.to_string()),
    }
}

#[tokio::test]
async fn canonical_app_production_config_boot_succeeds() {
    let result =
        gpu_init::serialized_headless_init(HeadlessRuntime::new(canonical_headless_config())).await;
    assert!(
        result.is_ok(),
        "runtime failed to start with app/tze_hud_app/config/production.toml: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn production_config_boots_with_builtin_widget_bundles() {
    let mut runtime =
        gpu_init::serialized_headless_init(HeadlessRuntime::new(canonical_headless_config()))
            .await
            .expect("runtime must start with canonical app production config");

    let scene_handle = {
        let state = runtime.shared_state().lock().await;
        state.scene.clone()
    };
    let scene = scene_handle.lock().await;

    // Config declares three concrete widget instances on the Main tab. If startup
    // fell back to defaults, these instances are absent.
    for instance in ["main-gauge", "main-progress", "main-status"] {
        assert!(
            scene.widget_registry.instances.contains_key(instance),
            "expected widget instance `{instance}` from canonical app config; startup likely fell back"
        );
    }

    // The corresponding widget types must also be loaded.
    for widget_type in ["gauge", "progress-bar", "status-indicator"] {
        assert!(
            scene.widget_registry.get_definition(widget_type).is_some(),
            "expected widget type `{widget_type}` from widget bundles; startup likely fell back"
        );
    }

    // production.toml selects the tonal-glass theme, which sets
    // color.text.primary = #E8EBF0. Verify the resolved zone policy reflects
    // the theme, not the canonical fallback.
    let notification_zone = scene
        .zone_registry
        .zones
        .get("notification-area")
        .expect("notification-area zone must be present");
    let text_color = notification_zone
        .rendering_policy
        .text_color
        .expect("notification-area text_color must be populated");

    let expected = (
        232.0f32 / 255.0f32,
        235.0f32 / 255.0f32,
        240.0f32 / 255.0f32,
    );
    let eps = 1e-3f32;
    assert!(
        (text_color.r - expected.0).abs() < eps
            && (text_color.g - expected.1).abs() < eps
            && (text_color.b - expected.2).abs() < eps,
        "expected notification-area text_color to resolve to #E8EBF0 from the tonal-glass theme, got ({:.4}, {:.4}, {:.4})",
        text_color.r,
        text_color.g,
        text_color.b
    );
    drop(scene);

    // Use the registered production widgets and the real scene publication path.
    {
        let mut scene = scene_handle.lock().await;
        for (instance, parameter) in [("main-gauge", "level"), ("main-progress", "progress")] {
            scene
                .publish_to_widget_for_lease(
                    instance,
                    HashMap::from([(parameter.to_string(), WidgetParameterValue::F32(0.25))]),
                    "raster-telemetry-fixture",
                    None,
                    0,
                    None,
                    None,
                )
                .expect("registered widget publication must succeed");
        }
    }
    let first = runtime.render_frame().await;
    let mut first_names = first.widget_rasterized.clone();
    first_names.sort();
    assert_eq!(first_names, ["main-gauge", "main-progress"]);
    let first_record = runtime
        .telemetry
        .records()
        .last()
        .expect("first frame record");
    assert_eq!(first_record.frame_number, first.frame_number);
    assert_eq!(first_record.widget_rasterized, first.widget_rasterized);

    // Changing only the gauge must preserve that exact same-frame observation.
    {
        let mut scene = scene_handle.lock().await;
        scene
            .publish_to_widget_for_lease(
                "main-gauge",
                HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.75))]),
                "raster-telemetry-fixture",
                None,
                0,
                None,
                None,
            )
            .expect("gauge update must succeed");
    }
    let update = runtime.render_frame().await;
    assert_eq!(update.widget_rasterized, ["main-gauge"]);
    let update_record = runtime
        .telemetry
        .records()
        .last()
        .expect("updated frame record");
    assert_eq!(update_record.frame_number, update.frame_number);
    assert_eq!(update_record.widget_rasterized, update.widget_rasterized);

    // An actually rendered idle frame must not carry the previous raster list.
    let idle = runtime.render_frame().await;
    assert!(idle.widget_rasterized.is_empty());
    let idle_record = runtime
        .telemetry
        .records()
        .last()
        .expect("idle frame record");
    assert_eq!(idle_record.frame_number, idle.frame_number);
    assert_eq!(idle_record.widget_rasterized, idle.widget_rasterized);
}

/// Only agents paired in `agents.toml` may connect: an unpaired PSK is
/// rejected at the handshake, whatever id it claims.
#[tokio::test]
async fn production_config_rejects_unpaired_psk() {
    use tokio_stream::StreamExt;
    use tze_hud_protocol::proto::session::client_message::Payload as Request;
    use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
    use tze_hud_protocol::proto::session::server_message::Payload as Reply;
    use tze_hud_protocol::proto::session::{ClientMessage, SessionInit};

    let port = std::net::TcpListener::bind("[::1]:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let paired = tze_hud_config::AgentsFile::default()
        .with_agent("paired-agent", "paired-key", &["tiles"])
        .directory()
        .expect("paired agent directory");
    let runtime = gpu_init::serialized_headless_init(HeadlessRuntime::new(HeadlessConfig {
        grpc_port: port,
        agents: paired,
        ..canonical_headless_config()
    }))
    .await
    .expect("runtime must start with canonical app production config");
    let _server = runtime.start_grpc_server().await.expect("gRPC server");

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: 0,
        payload: Some(Request::SessionInit(SessionInit {
            agent_id: "paired-agent".to_string(),
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: Some(tze_hud_protocol::auth::psk_credential("unpaired-key")),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let mut replies = HudSessionClient::connect(format!("http://[::1]:{port}"))
        .await
        .expect("connect")
        .session(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .expect("open session stream")
        .into_inner();

    let first = replies
        .next()
        .await
        .expect("a reply")
        .expect("no stream error");
    match first.payload {
        Some(Reply::SessionError(e)) => assert_eq!(e.code, "AUTH_FAILED"),
        other => panic!("expected SessionError(AUTH_FAILED), got {other:?}"),
    }
}
