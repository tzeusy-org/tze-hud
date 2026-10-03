//! # Production Config Boot Test
//!
//! Boots the headless runtime with the committed production config
//! (`config/production.toml`) and verifies:
//!
//! 1. Startup succeeds without error — the config is valid and parseable.
//! 2. The runtime initialises the full pipeline (scene, compositor, telemetry).
//! 3. An unpaired PSK connecting over gRPC is rejected with `AUTH_FAILED`.
//! 4. The paired agent (`vertical-slice-agent`, `config/agents.toml`)
//!    receives its declared `allow` permissions.
//!
//! ## Why this test exists
//!
//! The default example path (`cargo run -p vertical_slice -- --headless`)
//! loads `config/production.toml` at runtime.  If the config is malformed
//! or the runtime's config parsing regresses, the binary silently falls back
//! to guest policy.  This test makes that failure explicit and CI-visible.
//!
//! ## Spec reference
//!
//! - `configuration/spec.md` §Requirement: Capability Vocabulary (lines 149-164)
//! - `session-protocol/spec.md` §Requirement: Session Establishment (lines 87-112)
//! - `heart-and-soul/architecture.md` §Sovereignty by Mechanism
//!
//! ## Dev-mode note
//!
//! This test uses `config_toml: Some(PRODUCTION_CONFIG)` — it does NOT rely on
//! `config_toml: None` (dev-mode unrestricted bypass).  The `dev-mode` feature
//! is compiled into `vertical_slice` for other test infrastructure, but this
//! test exercises the production code path where governance is enforced by config.
//!
//! Run:
//!   cargo test -p vertical_slice --test production_boot -- --nocapture

use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;

/// The production config is embedded at compile time from the committed file.
/// If the file is missing or malformed, this const will cause a compile error —
/// which is intentional (the config must always be present and syntactically valid).
const PRODUCTION_CONFIG: &str = include_str!("../config/production.toml");

/// The paired agents that ship beside the production config.
const PRODUCTION_AGENTS: &str = include_str!("../config/agents.toml");

/// The demo PSK whose SHA-256 `config/agents.toml` stores.
const AGENT_PSK: &str = "vertical-slice-key";

fn production_agents() -> tze_hud_scene::config::AgentDirectory {
    tze_hud_config::AgentsFile::parse(PRODUCTION_AGENTS)
        .and_then(|file| file.directory())
        .expect("config/agents.toml must be valid")
}

/// Test-scoped writable directory for runtime widget asset store probes.
struct RuntimeWidgetAssetStoreTestDir {
    root: std::path::PathBuf,
    store_path: std::path::PathBuf,
}

impl RuntimeWidgetAssetStoreTestDir {
    fn create() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "tze_hud_production_boot_widget_store_{}_{}",
            std::process::id(),
            nanos
        ));
        let store_path = root.join("runtime_widget_assets");
        std::fs::create_dir_all(&store_path)
            .expect("test runtime widget asset store dir must be creatable");
        Self { root, store_path }
    }
}

impl Drop for RuntimeWidgetAssetStoreTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Inject or override `[widget_runtime_assets].store_path` with a writable
/// test-local directory so boot tests don't depend on CI cache env wiring.
fn with_writable_widget_store(base_toml: &str) -> (String, RuntimeWidgetAssetStoreTestDir) {
    let temp_store = RuntimeWidgetAssetStoreTestDir::create();
    let escaped_store_path = temp_store
        .store_path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");

    let mut out = String::with_capacity(base_toml.len() + escaped_store_path.len() + 96);
    let mut in_widget_runtime_assets = false;
    let mut saw_section = false;
    let mut saw_store_path = false;

    for line in base_toml.lines() {
        let trimmed = line.trim();
        let is_header = trimmed.starts_with('[') && trimmed.ends_with(']');
        if is_header {
            if in_widget_runtime_assets && !saw_store_path {
                out.push_str(&format!("store_path = \"{escaped_store_path}\"\n"));
            }
            in_widget_runtime_assets = trimmed == "[widget_runtime_assets]";
            if in_widget_runtime_assets {
                saw_section = true;
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if in_widget_runtime_assets && trimmed.starts_with("store_path") {
            out.push_str(&format!("store_path = \"{escaped_store_path}\"\n"));
            saw_store_path = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }

    if in_widget_runtime_assets && !saw_store_path {
        out.push_str(&format!("store_path = \"{escaped_store_path}\"\n"));
    }

    if !saw_section {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n[widget_runtime_assets]\n");
        out.push_str(&format!("store_path = \"{escaped_store_path}\"\n"));
    }

    (out, temp_store)
}

/// Boot the runtime with the committed production config and verify startup
/// succeeds.
///
/// This is the most fundamental CI gate: if `production.toml` is malformed or
/// the runtime's config parsing regresses, this test fails immediately.
#[tokio::test]
async fn production_config_boot_succeeds() {
    let (config_toml, _store_dir) = with_writable_widget_store(PRODUCTION_CONFIG);
    let config = HeadlessConfig {
        width: 320,
        height: 240,
        grpc_port: 0, // No gRPC server — pure boot test.
        agents: production_agents(),
        config_toml: Some(config_toml),
    };

    let result = HeadlessRuntime::new(config).await;
    assert!(
        result.is_ok(),
        "Runtime failed to start with production config: {:?}",
        result.err()
    );

    println!("PASS: runtime booted with production.toml");
}

/// Verify that the production config correctly parses and registers the
/// `vertical-slice-agent` with its declared capability set.
///
/// This test boots the runtime, starts the gRPC server on an ephemeral port,
/// connects as the registered agent, and asserts that the granted capabilities
/// match the config file declaration.
#[tokio::test]
async fn production_config_grants_registered_agent_capabilities() {
    use tokio_stream::StreamExt;
    use tze_hud_protocol::proto::session as session_proto;
    use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;

    let (config_toml, _store_dir) = with_writable_widget_store(PRODUCTION_CONFIG);
    // Ephemeral port to avoid port conflicts in parallel CI.
    let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    let free_port = listener.local_addr().unwrap().port();
    drop(listener);

    let config = HeadlessConfig {
        width: 320,
        height: 240,
        grpc_port: free_port,
        agents: production_agents(),
        config_toml: Some(config_toml),
    };

    let runtime = HeadlessRuntime::new(config)
        .await
        .expect("runtime must start with production config");
    let _server = runtime
        .start_grpc_server()
        .await
        .expect("gRPC server must start");

    // Connect as the registered agent declared in production.toml.
    let mut client = HudSessionClient::connect(format!("http://[::1]:{free_port}"))
        .await
        .expect("must connect to gRPC server");

    let (tx, rx) = tokio::sync::mpsc::channel::<session_proto::ClientMessage>(16);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;

    // Send SessionInit as the registered agent with the canonical capability set.
    // These are exactly the capabilities declared in production.toml.
    tx.send(session_proto::ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_us,
        payload: Some(session_proto::client_message::Payload::SessionInit(
            session_proto::SessionInit {
                agent_id: "vertical-slice-agent".to_string(),
                initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(AGENT_PSK)),
            },
        )),
    })
    .await
    .unwrap();

    let mut response = client
        .session(stream)
        .await
        .expect("must open session stream")
        .into_inner();

    let msg = response
        .next()
        .await
        .expect("must receive SessionEstablished")
        .expect("must not error");

    match &msg.payload {
        Some(session_proto::server_message::Payload::SessionEstablished(established)) => {
            // The configured agent's allow list grants read_scene_topology,
            // so the gated SCENE_TOPOLOGY subscription is activated. If the
            // config were not loaded the agent would get nothing and this fails.
            assert!(
                established
                    .active_subscriptions
                    .contains(&"SCENE_TOPOLOGY".to_string()),
                "configured agent must get SCENE_TOPOLOGY, got: {:?}",
                established.active_subscriptions
            );
            println!("PASS: configured agent received its allow-list permissions");
        }
        other => {
            panic!("Expected SessionEstablished, got: {other:?}");
        }
    }
}

/// Verify that an unpaired PSK is rejected at the handshake.
///
/// This is the sovereignty-by-mechanism gate: only agents paired in
/// `agents.toml` may connect, whatever id or subscriptions they request.
#[tokio::test]
async fn production_config_rejects_unpaired_psk() {
    use tokio_stream::StreamExt;
    use tze_hud_protocol::proto::session as session_proto;
    use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;

    let (config_toml, _store_dir) = with_writable_widget_store(PRODUCTION_CONFIG);
    // Ephemeral port.
    let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    let free_port = listener.local_addr().unwrap().port();
    drop(listener);

    let config = HeadlessConfig {
        width: 320,
        height: 240,
        grpc_port: free_port,
        agents: production_agents(),
        config_toml: Some(config_toml),
    };

    let runtime = HeadlessRuntime::new(config)
        .await
        .expect("runtime must start with production config");
    let _server = runtime
        .start_grpc_server()
        .await
        .expect("gRPC server must start");

    let mut client = HudSessionClient::connect(format!("http://[::1]:{free_port}"))
        .await
        .expect("must connect to gRPC server");

    let (tx, rx) = tokio::sync::mpsc::channel::<session_proto::ClientMessage>(16);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;

    // A PSK not paired in agents.toml — must be rejected.
    tx.send(session_proto::ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_us,
        payload: Some(session_proto::client_message::Payload::SessionInit(
            session_proto::SessionInit {
                agent_id: "unknown-rogue-agent".to_string(),
                // Requests all capabilities — must receive none.
                initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(tze_hud_protocol::auth::psk_credential("unpaired-rogue-key")),
            },
        )),
    })
    .await
    .unwrap();

    let mut response = client
        .session(stream)
        .await
        .expect("must open session stream")
        .into_inner();

    let msg = response
        .next()
        .await
        .expect("must receive response")
        .expect("must not error");

    match &msg.payload {
        Some(session_proto::server_message::Payload::SessionError(error)) => {
            assert_eq!(error.code, "AUTH_FAILED");
            println!("PASS: unpaired PSK rejected");
        }
        other => {
            panic!("Expected SessionError(AUTH_FAILED), got: {other:?}");
        }
    }
}
