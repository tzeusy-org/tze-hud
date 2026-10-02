//! # Production Config Boot Test
//!
//! Boots the headless runtime with the committed production config
//! (`config/production.toml`) and verifies:
//!
//! 1. Startup succeeds without error — the config is valid and parseable.
//! 2. The runtime initialises the full pipeline (scene, compositor, telemetry).
//! 3. An unregistered agent connecting over gRPC receives **guest policy**
//!    (zero capabilities granted) — sovereignty-by-mechanism is active.
//! 4. The registered agent (`vertical-slice-agent`) receives its declared
//!    capabilities from the config file.
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
        bind_all_interfaces: false,
        psk: "production-boot-test".to_string(),
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
        bind_all_interfaces: false,
        psk: "production-boot-test".to_string(),
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
                requested_capabilities: vec![
                    "create_tiles".to_string(),
                    "modify_own_tiles".to_string(),
                    "access_input_events".to_string(),
                    "read_scene_topology".to_string(),
                    "publish_zone:status-bar".to_string(),
                ],
                initial_subscriptions: vec!["LEASE_CHANGES".to_string()],
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(
                    "production-boot-test".to_string(),
                )),
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
            let granted = &established.granted_capabilities;
            println!("Granted capabilities: {granted:?}");

            // The registered agent must receive all 5 declared capabilities.
            // If governance is broken (e.g., config not loaded), the agent
            // gets guest policy (empty capabilities), and this fails.
            assert!(
                granted.contains(&"create_tiles".to_string()),
                "expected create_tiles in granted capabilities, got: {granted:?}"
            );
            assert!(
                granted.contains(&"modify_own_tiles".to_string()),
                "expected modify_own_tiles in granted capabilities, got: {granted:?}"
            );
            assert!(
                granted.contains(&"access_input_events".to_string()),
                "expected access_input_events in granted capabilities, got: {granted:?}"
            );
            assert!(
                granted.contains(&"read_scene_topology".to_string()),
                "expected read_scene_topology in granted capabilities, got: {granted:?}"
            );
            assert!(
                granted.contains(&"publish_zone:status-bar".to_string()),
                "expected publish_zone:status-bar in granted capabilities, got: {granted:?}"
            );
            println!("PASS: registered agent received all 5 declared capabilities");
        }
        other => {
            panic!("Expected SessionEstablished, got: {other:?}");
        }
    }
}

/// Verify that an unregistered agent receives guest policy (no capabilities).
///
/// This is the sovereignty-by-mechanism gate: agents not declared in the config
/// must never receive capabilities, regardless of what they request.
#[tokio::test]
async fn production_config_denies_unregistered_agent() {
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
        bind_all_interfaces: false,
        psk: "production-boot-test".to_string(),
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

    // An agent not declared in production.toml — must receive guest policy.
    tx.send(session_proto::ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_us,
        payload: Some(session_proto::client_message::Payload::SessionInit(
            session_proto::SessionInit {
                agent_id: "unknown-rogue-agent".to_string(),
                // Requests all capabilities — must receive none.
                requested_capabilities: vec![
                    "create_tiles".to_string(),
                    "modify_own_tiles".to_string(),
                    "access_input_events".to_string(),
                    "read_scene_topology".to_string(),
                    "publish_zone:status-bar".to_string(),
                ],
                initial_subscriptions: vec!["LEASE_CHANGES".to_string()],
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(
                    "production-boot-test".to_string(),
                )),
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
        Some(session_proto::server_message::Payload::SessionEstablished(established)) => {
            let granted = &established.granted_capabilities;
            println!("Granted capabilities for unregistered agent: {granted:?}");

            // Guest policy: no capabilities should be granted.
            assert!(
                granted.is_empty(),
                "unregistered agent must receive no capabilities (guest policy), \
                 but got: {granted:?}"
            );
            println!(
                "PASS: unregistered agent received empty capabilities (guest policy enforced)"
            );
        }
        other => {
            panic!("Expected SessionEstablished, got: {other:?}");
        }
    }
}
