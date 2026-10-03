//! Canonical app production-config boot gate.
//!
//! This test suite boots the runtime with the committed canonical app config:
//! `app/tze_hud_app/config/production.toml`.
//!
//! The gate is intentionally CI-visible:
//! - startup must succeed
//! - config-declared widget instances/types must be registered
//! - config `[design_tokens]` must be visible in zone policy
//!
//! If startup silently falls back to a default/headless policy, these assertions
//! fail even when runtime construction itself succeeds.

use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;

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
    let result = HeadlessRuntime::new(canonical_headless_config()).await;
    assert!(
        result.is_ok(),
        "runtime failed to start with app/tze_hud_app/config/production.toml: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn production_config_boots_with_builtin_widget_bundles() {
    let runtime = HeadlessRuntime::new(canonical_headless_config())
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
            scene.widget_registry.get_instance(instance).is_some(),
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

    // production.toml's [design_tokens] sets color.text.primary = #F5F7FA.
    // Verify the resolved zone policy reflects that override, not default fallback.
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
        245.0f32 / 255.0f32,
        247.0f32 / 255.0f32,
        250.0f32 / 255.0f32,
    );
    let eps = 1e-3f32;
    assert!(
        (text_color.r - expected.0).abs() < eps
            && (text_color.g - expected.1).abs() < eps
            && (text_color.b - expected.2).abs() < eps,
        "expected notification-area text_color to resolve to #F5F7FA from [design_tokens], got ({:.4}, {:.4}, {:.4})",
        text_color.r,
        text_color.g,
        text_color.b
    );
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
    let runtime = HeadlessRuntime::new(HeadlessConfig {
        grpc_port: port,
        agents: paired,
        ..canonical_headless_config()
    })
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
