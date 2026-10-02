//! # Vertical Slice Example — v1 Canonical Conformance Reference
//!
//! This binary is the **primary reference** for how an agent interacts with
//! the tze_hud runtime. Every interaction pattern demonstrated here follows
//! the v1 spec directly and is linked to the relevant spec requirement.
//!
//! **Phase 1** — Session + Lease
//!   - Session init with canonical capability names (`create_tiles`,
//!     `modify_own_tiles`, `access_input_events`, `read_scene_topology`)
//!   - Capability negotiation: shows granted vs denied capabilities
//!   - Mandatory subscription categories: `LEASE_CHANGES`, `SCENE_TOPOLOGY`,
//!     `ZONE_EVENTS`
//!   - Lease acquisition with priority (spec §Priority Assignment)
//!   - Structured error handling: capability denied, budget exceeded
//!
//! **Phase 2** — Scene Setup
//!   - Tab creation and zone registry initialization
//!   - Tile creation and mutation via resident capabilities
//!   - Zone publishing via `publish_to_zone` — the LLM-first surface
//!     (spec §Zone Publishing, the preferred way for LLMs to surface content)
//!
//! **Phase 3** — Input Loop: pointer events, hit-test, local ack, agent dispatch
//!
//! **Phase 4** — Telemetry: frame metrics, telemetry frame over session stream
//!
//! **Phase 5** — Safe Mode: suspend all leases, verify mutation rejection, resume
//!
//! **Phase 6** — Graceful Shutdown: SessionClose, cleanup verification
//!
//! ## Config flow (production — default)
//!
//! The headless path loads `config/production.toml` (embedded at compile time).
//! This enforces capability governance: only the registered agent
//! (`vertical-slice-agent`) may connect, with the capability set declared in
//! that file.  Unknown agents receive guest policy (no capabilities).
//!
//! The production config is the **default documented path** — it requires no
//! feature flags at build time.
//!
//! Config schema: `configuration/spec.md` §Capability Vocabulary (lines 149-164).
//! The agent must appear in `[agents.registered]`; unknown agents get guest
//! policy (no capabilities).
//!
//! ## Dev mode (opt-in, test/dev only)
//!
//! Pass `--dev` to bypass config governance: all capabilities are granted to any
//! agent (`fallback_unrestricted = true`).  This also requires the `dev-mode`
//! Cargo feature; production builds without it will refuse `--dev` with an error.
//!
//! **NEVER use `--dev` in production deployments.**
//!
//! ## Spec references
//!
//! - `session-protocol/spec.md` §Requirement: MCP Bridge Guest Tools (lines 487-502)
//! - `configuration/spec.md` §Requirement: Capability Vocabulary (lines 149-164)
//! - `validation-framework/spec.md`: "Tests SHALL read like usage examples" (line 398)
//!
//! Run headless (production config — default, no feature flags):
//!   cargo run -p vertical_slice -- --headless
//!
//! Run headless (dev mode — unrestricted caps, TEST/DEV ONLY):
//!   cargo run -p vertical_slice --features dev-mode -- --headless --dev
//!
//! Run windowed (production config via env vars or defaults):
//!   cargo run -p vertical_slice

use tze_hud_protocol::proto::session as session_proto;
#[allow(deprecated)]
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;
use tze_hud_runtime::window::{WindowConfig, WindowMode};
use tze_hud_runtime::windowed::{WindowedConfig, WindowedRuntime};

use tze_hud_scene::mutation::{MutationBatch as SceneMutationBatch, SceneMutation};
use tze_hud_scene::types::*;

/// Embedded production config — parsed at runtime so capability governance is
/// always active by default.  The file lives at `config/production.toml`
/// relative to this source file and is baked into the binary at compile time.
const PRODUCTION_CONFIG: &str = include_str!("../config/production.toml");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let headless = args.iter().any(|a| a == "--headless");
    // `--dev` activates dev mode: all capabilities granted to any agent.
    // Requires the `dev-mode` Cargo feature.  TEST/DEV ONLY — never use in production.
    let dev_mode = args.iter().any(|a| a == "--dev");

    if headless {
        // Run headless inside a Tokio runtime.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(run_headless(dev_mode))
    } else {
        run_windowed()
    }
}

/// Run the windowed display runtime.
///
/// Creates a `WindowedRuntime` and runs the winit event loop on the main thread.
/// This call blocks until the window is closed.
///
/// For production deployments, configure agent capabilities via the runtime's
/// config TOML (see `WindowedConfig` docs and `config/production.toml` for the
/// headless equivalent schema).  The windowed runtime does not embed a default
/// config in this example; extend `WindowedConfig` with a `config_toml` field
/// to enforce capability governance in windowed mode.
///
/// Per spec §Main Thread Responsibilities (line 33): "The main thread MUST run
/// the winit event loop." Winit requires the event loop to run on the main thread,
/// hence this function is called directly from `main()` without a Tokio wrapper.
fn run_windowed() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== tze_hud windowed runtime ===");
    println!("Close the window to exit.");

    let mode = match std::env::var("TZE_HUD_WINDOW_MODE")
        .unwrap_or_else(|_| "fullscreen".to_string())
        .to_lowercase()
        .as_str()
    {
        "overlay" => WindowMode::Overlay,
        _ => WindowMode::Fullscreen,
    };

    let width = std::env::var("TZE_HUD_WINDOW_WIDTH")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(800);

    let height = std::env::var("TZE_HUD_WINDOW_HEIGHT")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(600);

    println!("Window mode: {mode}, size: {width}x{height}");

    // overlay_auto_size: true when in overlay mode and no explicit dimensions
    // were given (env vars default to explicit fallbacks, so we detect the
    // "default" case by checking if the env vars are absent).
    let overlay_auto_size = mode == WindowMode::Overlay
        && std::env::var("TZE_HUD_WINDOW_WIDTH").is_err()
        && std::env::var("TZE_HUD_WINDOW_HEIGHT").is_err();

    let config = WindowedConfig {
        window: WindowConfig {
            mode,
            width,
            height,
            title: "tze_hud — vertical slice".to_string(),
        },
        overlay_auto_size,
        grpc_port: 0, // Disabled for the standalone windowed demo.
        mcp_port: 0,  // Disabled for the standalone windowed demo.
        psk: "vertical-slice-key".to_string(),
        projection_operator_authority: None,
        target_fps: 60,
        config_toml: None,      // No configuration file for the standalone demo.
        config_file_path: None, // No config file path needed when config_toml is None.
        debug_zones: false,     // Render zone boundaries — disabled for the demo.
        monitor_index: None,    // Use primary monitor.
        benchmark: None,        // Demo mode is unbounded until the window closes.
        quiescent_efficiency: None, // Demo mode never self-terminates for CI evidence.
        bind_all_interfaces: false, // Demo binds loopback only (ports are 0 anyway).
        resident_grpc_portal: None, // Resident gRPC portal bridge OFF for the demo (hud-x2e2v).
    };

    let runtime = WindowedRuntime::new(config);
    runtime.run()
}

fn now_wall_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Run the headless runtime.
///
/// # Production path (default, `dev_mode = false`)
///
/// Loads `config/production.toml` (embedded at compile time via `include_str!`).
/// Only the registered agent (`vertical-slice-agent`) may connect, with the
/// capability set declared in that file.  No Cargo feature flags are required.
///
///   cargo run -p vertical_slice -- --headless
///
/// # Dev mode (opt-in, `dev_mode = true`, TEST/DEV ONLY)
///
/// Bypasses config governance: all capabilities granted to any agent
/// (`fallback_unrestricted = true`).  Requires the `dev-mode` Cargo feature.
/// Pass `--dev` on the command line to activate.
///
///   cargo run -p vertical_slice --features dev-mode -- --headless --dev
///
/// **NEVER use dev mode in production deployments.**
async fn run_headless(dev_mode: bool) -> Result<(), Box<dyn std::error::Error>> {
    if dev_mode {
        println!("=== tze_hud vertical slice (DEV MODE — unrestricted caps, TEST/DEV ONLY) ===\n");
        println!("WARNING: --dev bypasses capability governance. Do NOT use in production.\n");
    } else {
        println!("=== tze_hud vertical slice (full contract path, production config) ===\n");
    }

    // ─── Initialize runtime ────────────────────────────────────────────────
    //
    // Production path (default): load config/production.toml, which enforces
    // capability governance via [agents.registered.vertical-slice-agent].
    //
    // Dev mode (--dev, TEST/DEV ONLY): config_toml = None activates unrestricted
    // capability grants.  Requires --features dev-mode at build time.
    let config_toml = if dev_mode {
        None // dev-mode: requires --features dev-mode at build time
    } else {
        Some(PRODUCTION_CONFIG.to_string())
    };

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 50051,
        // This example is a demo entrypoint where external agents connect;
        // opt in to all-interfaces binding so connections from outside loopback work.
        bind_all_interfaces: true,
        psk: "vertical-slice-key".to_string(),
        config_toml,
    };

    let mut runtime = HeadlessRuntime::new(config).await?;
    let _server = runtime.start_grpc_server().await?;
    println!("Runtime initialized: 800x600, gRPC on [::]:50051\n");

    // ─────────────────────────────────────────────────────────────────────────
    // PHASE 1: Session + Lease (streaming)
    // ─────────────────────────────────────────────────────────────────────────
    println!("=== Phase 1: Session Handshake + Lease Acquisition ===\n");

    let mut session_client = HudSessionClient::connect("http://[::1]:50051").await?;

    let (tx, rx) = tokio::sync::mpsc::channel::<session_proto::ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    let now_us = now_wall_us();

    // Send SessionInit with canonical capability names and mandatory subscriptions.
    //
    // Canonical capability vocabulary (configuration/spec.md §Capability Vocabulary,
    // lines 149-164). Legacy names like `create_tile` or `receive_input` are rejected
    // with CONFIG_UNKNOWN_CAPABILITY — always use the canonical plural forms.
    //
    // Mandatory subscription categories (session-protocol/spec.md §Subscriptions):
    // - LEASE_CHANGES: always delivered regardless of capability gating; demonstrates
    //   spec requirement that agents MUST subscribe to lease state changes.
    // - SCENE_TOPOLOGY: requires `read_scene_topology` capability.
    // - ZONE_EVENTS: requires any `publish_zone:<zone>` capability; not open to all.
    tx.send(session_proto::ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_us,
        payload: Some(session_proto::client_message::Payload::SessionInit(
            session_proto::SessionInit {
                agent_id: "vertical-slice-agent".to_string(),
                // Canonical v1 capability names. The runtime validates these against
                // the canonical vocabulary; non-canonical names are rejected with a
                // CONFIG_UNKNOWN_CAPABILITY error and a hint pointing to the canonical
                // replacement (see the structured error handling demo below).
                requested_capabilities: vec![
                    "create_tiles".to_string(),            // create tiles in leased area
                    "modify_own_tiles".to_string(),        // mutate tiles owned by this agent
                    "access_input_events".to_string(),     // receive pointer / keyboard events
                    "read_scene_topology".to_string(),     // required to subscribe SCENE_TOPOLOGY
                    "publish_zone:status-bar".to_string(), // required to subscribe ZONE_EVENTS
                ],
                // LEASE_CHANGES is mandatory (always active). Listing it in
                // initial_subscriptions is spec-compliant and demonstrates
                // that agents should explicitly declare their intent.
                initial_subscriptions: vec![
                    "SCENE_TOPOLOGY".to_string(), // requires read_scene_topology
                    "LEASE_CHANGES".to_string(),  // mandatory: always active
                    "ZONE_EVENTS".to_string(),    // requires publish_zone:<zone> capability
                ],
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(session_proto::AuthCredential {
                    credential: Some(session_proto::auth_credential::Credential::PreSharedKey(
                        session_proto::PreSharedKeyCredential {
                            key: "vertical-slice-key".to_string(),
                        },
                    )),
                }),
            },
        )),
    })
    .await?;

    let mut response_stream = session_client.session(stream).await?.into_inner();

    // Read SessionEstablished — capability negotiation result.
    //
    // The runtime intersects the agent's requested capabilities with what its
    // authorization policy allows.  With the production config (default),
    // only the registered agent's declared capabilities are granted; anything
    // else is denied.  In dev mode (--dev, TEST/DEV ONLY), all canonical
    // capabilities are granted to any agent (fallback_unrestricted = true).
    //
    // `granted_capabilities` lists what was actually granted — agents MUST
    // only exercise capabilities present in this list (spec §Capability Gating).
    // `active_subscriptions` lists which subscription categories are live.
    use tokio_stream::StreamExt;
    let msg = response_stream.next().await.unwrap()?;
    let namespace = match &msg.payload {
        Some(session_proto::server_message::Payload::SessionEstablished(established)) => {
            println!("  Session established:");
            println!("    namespace             = {}", established.namespace);
            println!(
                "    heartbeat_ms          = {}",
                established.heartbeat_interval_ms
            );
            println!(
                "    granted_capabilities  = {:?}",
                established.granted_capabilities
            );
            println!(
                "    active_subscriptions  = {:?}",
                established.active_subscriptions
            );

            // Capability negotiation: verify the expected capabilities were granted.
            // In dev mode all requested caps are granted; with a restricted config
            // only the registered set would be granted and others would be absent.
            let granted = &established.granted_capabilities;
            assert!(
                granted.contains(&"create_tiles".to_string()),
                "create_tiles must be granted"
            );
            assert!(
                granted.contains(&"modify_own_tiles".to_string()),
                "modify_own_tiles must be granted"
            );
            assert!(
                granted.contains(&"access_input_events".to_string()),
                "access_input_events must be granted"
            );
            assert!(
                granted.contains(&"read_scene_topology".to_string()),
                "read_scene_topology must be granted (needed for SCENE_TOPOLOGY subscription)"
            );
            assert!(
                granted.contains(&"publish_zone:status-bar".to_string()),
                "publish_zone:status-bar must be granted (needed for ZONE_EVENTS subscription)"
            );
            println!("  Capability negotiation: all 5 requested capabilities granted.");

            // Subscription negotiation:
            // - LEASE_CHANGES: mandatory, always active regardless of capabilities
            // - SCENE_TOPOLOGY: active because agent has read_scene_topology
            // - ZONE_EVENTS: active because agent has publish_zone:status-bar
            let subs = &established.active_subscriptions;
            assert!(
                subs.contains(&"LEASE_CHANGES".to_string()),
                "LEASE_CHANGES must be active (mandatory category)"
            );
            assert!(
                subs.contains(&"SCENE_TOPOLOGY".to_string()),
                "SCENE_TOPOLOGY must be active (agent has read_scene_topology)"
            );
            assert!(
                subs.contains(&"ZONE_EVENTS".to_string()),
                "ZONE_EVENTS must be active (agent has publish_zone:status-bar)"
            );
            println!(
                "  Subscription negotiation: LEASE_CHANGES + SCENE_TOPOLOGY + ZONE_EVENTS active."
            );

            established.namespace.clone()
        }
        other => {
            return Err(format!("Expected SessionEstablished, got: {other:?}").into());
        }
    };

    // Read SceneSnapshot
    let msg = response_stream.next().await.unwrap()?;
    match &msg.payload {
        Some(session_proto::server_message::Payload::SceneSnapshot(snapshot)) => {
            println!(
                "  Scene snapshot: sequence={}, json_len={}, checksum={}",
                snapshot.sequence,
                snapshot.snapshot_json.len(),
                &snapshot.blake3_checksum[..8.min(snapshot.blake3_checksum.len())]
            );
        }
        other => {
            return Err(format!("Expected SceneSnapshot, got: {other:?}").into());
        }
    }

    // Read the current degradation policy before transactional responses. The
    // server sends this baseline for every new session and resume.
    let msg = response_stream.next().await.unwrap()?;
    match &msg.payload {
        Some(session_proto::server_message::Payload::DegradationNotice(notice)) => {
            println!("  Degradation baseline: level={}", notice.level);
        }
        other => {
            return Err(
                format!("Expected DegradationNotice after SceneSnapshot, got: {other:?}").into(),
            );
        }
    }

    // Request lease with priority.
    //
    // `lease_priority` controls arbitration when the runtime must shed leases under
    // resource pressure (spec §Priority Assignment). Priority 2 is the default;
    // priority 1 (high) requires the `lease:priority:1` capability.
    // Capabilities listed here are scoped to this lease — the agent can only
    // exercise these capabilities while the lease is active.
    tx.send(session_proto::ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(session_proto::client_message::Payload::LeaseRequest(
            session_proto::LeaseRequest {
                ttl_ms: 60_000,
                capabilities: vec![
                    "create_tiles".to_string(),
                    "modify_own_tiles".to_string(),
                    "access_input_events".to_string(),
                    "read_scene_topology".to_string(),
                    "publish_zone:status-bar".to_string(),
                ],
                lease_priority: 2, // default priority; 1=high requires lease:priority:1 cap
            },
        )),
    })
    .await?;

    let msg = response_stream.next().await.unwrap()?;
    let _lease_id_bytes = match &msg.payload {
        Some(session_proto::server_message::Payload::LeaseResponse(resp)) if resp.granted => {
            println!(
                "  Lease granted: ttl={}ms, priority={}",
                resp.granted_ttl_ms, resp.granted_priority
            );
            resp.lease_id.clone()
        }
        Some(session_proto::server_message::Payload::LeaseResponse(resp)) => {
            // Lease denied — structured error:
            //   deny_code:   machine-readable error code (e.g. CONFIG_UNKNOWN_CAPABILITY)
            //   deny_reason: human-readable explanation
            // See the structured error handling demo below for how to handle this.
            return Err(format!(
                "Lease denied: code={}, reason={}",
                resp.deny_code, resp.deny_reason
            )
            .into());
        }
        other => {
            return Err(format!("Expected LeaseResponse, got: {other:?}").into());
        }
    };

    // Heartbeat round-trip
    let hb_mono = 999_000u64;
    tx.send(session_proto::ClientMessage {
        sequence: 3,
        timestamp_wall_us: now_wall_us(),
        payload: Some(session_proto::client_message::Payload::Heartbeat(
            session_proto::Heartbeat {
                timestamp_mono_us: hb_mono,
            },
        )),
    })
    .await?;

    let msg = response_stream.next().await.unwrap()?;
    match &msg.payload {
        Some(session_proto::server_message::Payload::Heartbeat(hb)) => {
            assert_eq!(hb.timestamp_mono_us, hb_mono, "heartbeat echo mismatch");
            println!("  Heartbeat echoed: mono_us={}", hb.timestamp_mono_us);
        }
        other => {
            return Err(format!("Expected Heartbeat, got: {other:?}").into());
        }
    }

    println!("\n  Phase 1 PASSED: session established, lease active, heartbeat verified.\n");

    // ─────────────────────────────────────────────────────────────────────────
    // PHASE 1.5: Structured Error Handling
    //
    // The runtime returns structured errors in two forms:
    //
    // 1. `LeaseResponse { granted: false, deny_code, deny_reason }` — when a
    //    LeaseRequest is denied. `deny_code` is a machine-readable constant:
    //    - CONFIG_UNKNOWN_CAPABILITY — one or more capability names are not in
    //      the canonical vocabulary (legacy names like `create_tile` trigger this)
    //    - CAPABILITY_DENIED — agent's policy doesn't permit the capability
    //
    // 2. `RuntimeError { error_code, message, hint }` — advisory errors sent
    //    alongside a LeaseResponse denial when the server wants to give the
    //    agent actionable hints (e.g. the canonical replacement for a legacy name).
    //
    // This section demonstrates the capability-denied and budget-exceeded paths
    // using direct scene graph calls (no second gRPC session needed).
    // ─────────────────────────────────────────────────────────────────────────
    println!("=== Phase 1.5: Structured Error Handling Demo ===\n");

    // 1. Capability denied: demonstrate that requesting an unknown capability
    //    name returns CONFIG_UNKNOWN_CAPABILITY via the scene graph.
    //    (Over gRPC this would be a LeaseResponse denial.)
    {
        use tze_hud_protocol::auth::validate_canonical_capabilities;
        let result = validate_canonical_capabilities(&[
            "create_tile".to_string(),   // legacy — rejected
            "receive_input".to_string(), // legacy — rejected
        ]);
        let unknowns = result.expect_err("legacy names must be rejected");
        println!("  Capability denied (CONFIG_UNKNOWN_CAPABILITY):");
        for u in &unknowns {
            println!("    unknown={:?}  hint={:?}", u.unknown, u.hint);
        }
        assert!(
            unknowns.iter().any(|u| u.hint.contains("create_tiles")),
            "hint must point to create_tiles for legacy create_tile"
        );
        assert!(
            unknowns
                .iter()
                .any(|u| u.hint.contains("access_input_events")),
            "hint must point to access_input_events for legacy receive_input"
        );
        println!("  CONFIG_UNKNOWN_CAPABILITY validated: hints contain canonical replacements.");
    }

    // 2. Budget exceeded: demonstrate that mutation batches are rejected
    //    when an agent's tile budget is exhausted.
    //    Over gRPC this produces MutationResult { applied: false } with an error.
    {
        use tze_hud_scene::mutation::{MutationBatch as DemoBatch, SceneMutation as DemoMutation};

        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let demo_tab = scene.create_tab("BudgetDemo", 1).unwrap();
        let demo_lease = scene.grant_lease(
            "budget-demo-agent",
            5_000,
            vec![tze_hud_scene::types::Capability::CreateTiles],
        );
        // Shrink the budget to 2 tiles for demonstration purposes.
        scene
            .leases
            .get_mut(&demo_lease)
            .unwrap()
            .resource_budget
            .max_tiles = 2;

        // First two tiles succeed via apply_batch (within budget).
        for i in 0..2u32 {
            let batch = DemoBatch {
                batch_id: SceneId::new(),
                agent_namespace: "budget-demo-agent".to_string(),
                mutations: vec![DemoMutation::CreateTile {
                    tab_id: demo_tab,
                    namespace: "budget-demo-agent".to_string(),
                    lease_id: demo_lease,
                    bounds: tze_hud_scene::types::Rect::new(i as f32 * 100.0, 400.0, 90.0, 50.0),
                    z_order: i + 10,
                }],
                timing_hints: None,
                lease_id: Some(demo_lease),
            };
            let result = scene.apply_batch(&batch);
            assert!(result.applied, "tile {i} within budget should succeed");
        }

        // Third tile exceeds budget — apply_batch returns a structured rejection.
        let over_batch = DemoBatch {
            batch_id: SceneId::new(),
            agent_namespace: "budget-demo-agent".to_string(),
            mutations: vec![DemoMutation::CreateTile {
                tab_id: demo_tab,
                namespace: "budget-demo-agent".to_string(),
                lease_id: demo_lease,
                bounds: tze_hud_scene::types::Rect::new(200.0, 400.0, 90.0, 50.0),
                z_order: 12,
            }],
            timing_hints: None,
            lease_id: Some(demo_lease),
        };
        let result = scene.apply_batch(&over_batch);
        assert!(
            !result.applied,
            "third tile must be rejected (budget exceeded)"
        );
        let err_msg = result
            .error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_default();
        println!("  Budget exceeded (MUTATION_REJECTED):");
        println!("    error: {err_msg}");
        assert!(
            err_msg.contains("tiles") || err_msg.contains("budget"),
            "error must reference tile budget: {err_msg}"
        );

        // Clean up the demo tab and lease.
        // Use delete_tab (public API) rather than tabs.remove: it removes tiles
        // belonging to the tab, handles active_tab fallback, and bumps version.
        scene.revoke_lease(demo_lease).ok();
        scene.delete_tab(demo_tab).ok();
        println!("  Budget enforcement validated: tile budget rejection confirmed.");
    }

    println!(
        "\n  Phase 1.5 PASSED: structured error handling validated (capability denied + budget exceeded).\n"
    );

    // ─────────────────────────────────────────────────────────────────────────
    // PHASE 2: Scene Setup — tab, tiles, zone publish
    // ─────────────────────────────────────────────────────────────────────────
    println!("=== Phase 2: Scene Setup (tab + tiles + zone publish) ===\n");

    // Create a tab and register zones directly on the scene graph.
    // (The streaming session shares state via Arc<Mutex<SharedState>>.)
    let (tab_id, lease_id) = {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab_id = scene.create_tab("Main", 0).unwrap();
        println!("  Tab created: id={tab_id}");

        // Register the default zones
        scene.zone_registry = ZoneRegistry::with_defaults();
        let zone_count = scene.zone_registry.all_zones().len();
        println!(
            "  Registered {zone_count} default zones (status-bar, notification-area, subtitle)"
        );

        // We already have a lease from Phase 1 -- find it by namespace
        let lease_id = scene
            .leases
            .values()
            .find(|l| l.namespace == namespace && l.is_active())
            .map(|l| l.id)
            .expect("should have an active lease from Phase 1");
        println!("  Using lease: {lease_id}");

        (tab_id, lease_id)
    };

    // Create text tile via scene graph
    let _text_tile_id = {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tile_id = scene
            .create_tile(
                tab_id,
                &namespace,
                lease_id,
                Rect::new(50.0, 50.0, 350.0, 250.0),
                1,
            )
            .unwrap();

        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "Hello, tze_hud! Status display tile.".to_string(),
                        bounds: Rect::new(0.0, 0.0, 350.0, 250.0),
                        font_size_px: 24.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: Some(Rgba::new(0.1, 0.15, 0.3, 1.0)),
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
            .unwrap();

        println!("  Text tile created: id={tile_id}");
        tile_id
    };

    // Create hit region tile via scene graph
    let (_hit_tile_id, _hr_node_id) = {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tile_id = scene
            .create_tile(
                tab_id,
                &namespace,
                lease_id,
                Rect::new(450.0, 50.0, 300.0, 250.0),
                2,
            )
            .unwrap();

        let node_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: node_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(25.0, 25.0, 250.0, 200.0),
                        interaction_id: "demo-button".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        println!("  Hit region tile created: id={tile_id}, node={node_id}");
        (tile_id, node_id)
    };

    // Zone publishing via publish_to_zone — the LLM-first surface.
    //
    // `publish_to_zone` is the preferred way for LLMs to surface content. Zones are
    // named slots on the display (e.g. "status-bar", "notification-area", "subtitle")
    // with defined layout semantics. LLMs publish into zones; the runtime composits
    // the final display. This is distinct from direct tile creation (which gives more
    // control but requires a lease and explicit tile management).
    //
    // publish_to_zone arguments:
    //   zone_name         — must match a registered zone
    //   content           — ZoneContent variant matching the zone's accepted types
    //   publisher_ns      — the publishing agent's namespace
    //   merge_key         — optional; same key = replace existing publish (MergeByKey)
    //   expires_at_wall_us— optional; publication expires at this wall-clock time
    //   content_class     — optional; content classification for privacy/redaction

    // Publish to status-bar zone (MergeByKey: same key replaces, does not accumulate)
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let mut entries = std::collections::HashMap::new();
        entries.insert("agent".to_string(), "vertical-slice-agent".to_string());
        entries.insert("status".to_string(), "running".to_string());

        scene
            .publish_to_zone(
                "status-bar",
                ZoneContent::StatusBar(StatusBarPayload { entries }),
                &namespace,
                Some("agent-status".to_string()), // merge_key: subsequent publishes with same key replace this one
                None,                             // no expiry
                None,                             // no content classification
            )
            .unwrap();
        println!("  Published to status-bar zone (MergeByKey, key=agent-status)");

        // Verify the publish is active — confirms zone routing is working.
        let active = scene.zone_registry.active_for_zone("status-bar");
        assert!(
            !active.is_empty(),
            "status-bar should have active publishes"
        );
        println!("  status-bar active publishes: {}", active.len());
    }

    // Publish to notification-area zone (no merge key — each publish is independent)
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: "Vertical slice started".to_string(),
                    icon: "info".to_string(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                &namespace,
                None, // no merge key — stacks alongside other notifications
                None,
                None,
            )
            .unwrap();
        println!("  Published notification to notification-area zone");
    }

    // Verify scene state
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        assert_eq!(scene.tiles.len(), 2, "expected 2 tiles");
        assert_eq!(scene.tabs.len(), 1, "expected 1 tab");
        let active_leases: usize = scene.leases.values().filter(|l| l.is_active()).count();
        assert!(active_leases >= 1, "expected at least 1 active lease");
        println!(
            "  Scene verified: {} tabs, {} tiles, {} active leases",
            scene.tabs.len(),
            scene.tiles.len(),
            active_leases
        );
    }

    println!("\n  Phase 2 PASSED: scene populated with tab, tiles, and zone content.\n");

    // ─────────────────────────────────────────────────────────────────────────
    // PHASE 3: Input Loop — pointer events, hit-test, dispatch
    // ─────────────────────────────────────────────────────────────────────────
    println!("=== Phase 3: Input Loop (pointer events + hit-test + dispatch) ===\n");

    // Hover over the hit region
    let hover_result = {
        let state_arc = runtime.shared_state().clone();
        let state = state_arc.lock().await;
        let mut scene = state.scene.lock().await;
        runtime.input_processor.process(
            &tze_hud_input::PointerEvent {
                x: 550.0,
                y: 150.0,
                kind: tze_hud_input::PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        )
    };
    assert!(
        matches!(hover_result.hit, tze_hud_scene::HitResult::NodeHit { .. }),
        "hover should hit the tile"
    );
    assert_eq!(hover_result.interaction_id, Some("demo-button".to_string()));
    assert!(
        hover_result.dispatch.is_some(),
        "should dispatch PointerEnter"
    );
    let dispatch = hover_result.dispatch.as_ref().unwrap();
    assert_eq!(
        dispatch.kind,
        tze_hud_input::AgentDispatchKind::PointerEnter
    );
    assert_eq!(dispatch.interaction_id, "demo-button");
    println!("  Hover: hit=NodeHit, interaction_id=demo-button, dispatch=PointerEnter");

    // Move within the hit region (should produce PointerMove)
    let move_result = {
        let state_arc = runtime.shared_state().clone();
        let state = state_arc.lock().await;
        let mut scene = state.scene.lock().await;
        runtime.input_processor.process(
            &tze_hud_input::PointerEvent {
                x: 560.0,
                y: 160.0,
                kind: tze_hud_input::PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        )
    };
    assert!(move_result.dispatch.is_some());
    assert_eq!(
        move_result.dispatch.as_ref().unwrap().kind,
        tze_hud_input::AgentDispatchKind::PointerMove
    );
    println!(
        "  Move: dispatch=PointerMove, local_coords=({:.1},{:.1})",
        move_result.dispatch.as_ref().unwrap().local_x,
        move_result.dispatch.as_ref().unwrap().local_y
    );

    // Press on hit region
    let press_result = {
        let state_arc = runtime.shared_state().clone();
        let state = state_arc.lock().await;
        let mut scene = state.scene.lock().await;
        runtime.input_processor.process(
            &tze_hud_input::PointerEvent {
                x: 550.0,
                y: 150.0,
                kind: tze_hud_input::PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        )
    };
    assert!(press_result.dispatch.is_some());
    assert_eq!(
        press_result.dispatch.as_ref().unwrap().kind,
        tze_hud_input::AgentDispatchKind::PointerDown
    );
    println!(
        "  Press: local_ack={}us, hit_test={}us, dispatch=PointerDown",
        press_result.local_ack_us, press_result.hit_test_us
    );

    // Verify local ack is within 4ms budget
    assert!(
        press_result.local_ack_us < 4_000,
        "local_ack_us={}us exceeds 4ms budget",
        press_result.local_ack_us
    );
    println!(
        "  Budget check: local_ack={}us < 4000us PASSED",
        press_result.local_ack_us
    );

    // Release (activate)
    let release_result = {
        let state_arc = runtime.shared_state().clone();
        let state = state_arc.lock().await;
        let mut scene = state.scene.lock().await;
        runtime.input_processor.process(
            &tze_hud_input::PointerEvent {
                x: 550.0,
                y: 150.0,
                kind: tze_hud_input::PointerEventKind::Up,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        )
    };
    assert!(
        release_result.activated,
        "press+release on same node should activate"
    );
    assert!(release_result.dispatch.is_some());
    assert_eq!(
        release_result.dispatch.as_ref().unwrap().kind,
        tze_hud_input::AgentDispatchKind::Activated
    );
    assert_eq!(
        release_result.dispatch.as_ref().unwrap().interaction_id,
        "demo-button"
    );
    println!("  Release: activated=true, dispatch=Activated(demo-button)");

    // Record latencies
    runtime
        .telemetry
        .summary_mut()
        .input_to_local_ack
        .record(press_result.local_ack_us);
    runtime
        .telemetry
        .summary_mut()
        .hit_test_latency
        .record(press_result.hit_test_us);

    println!("\n  Phase 3 PASSED: input pipeline exercised, latency budgets met.\n");

    // ─────────────────────────────────────────────────────────────────────────
    // PHASE 4: Telemetry — frame metrics + stream telemetry
    // ─────────────────────────────────────────────────────────────────────────
    println!("=== Phase 4: Telemetry (frame metrics + stream telemetry) ===\n");

    // Render a frame and collect telemetry
    let frame_telemetry = runtime.render_frame().await;
    println!("  Frame rendered:");
    println!("    frame_time     = {}us", frame_telemetry.frame_time_us);
    println!("    tiles          = {}", frame_telemetry.tile_count);
    println!("    nodes          = {}", frame_telemetry.node_count);
    println!("    active_leases  = {}", frame_telemetry.active_leases);
    println!(
        "    render_encode  = {}us",
        frame_telemetry.stage6_render_encode_us
    );
    println!(
        "    gpu_submit     = {}us",
        frame_telemetry.stage7_gpu_submit_us
    );

    assert!(
        frame_telemetry.tile_count >= 2,
        "expected at least 2 tiles in frame"
    );
    assert!(
        frame_telemetry.node_count >= 2,
        "expected at least 2 nodes in frame"
    );

    // Pixel readback to verify rendering
    let pixels = runtime.read_pixels();
    assert_eq!(pixels.len(), 800 * 600 * 4, "pixel buffer size mismatch");
    println!("  Pixel readback: {} bytes (800x600 RGBA)", pixels.len());

    // Emit session summary JSON
    let summary = runtime.telemetry.summary();
    println!("  Session summary:");
    println!("    total_frames = {}", summary.total_frames);
    if let Some(p50) = summary.frame_time.p50() {
        println!("    frame_time p50 = {p50}us");
    }
    if let Some(p99) = summary.frame_time.p99() {
        println!("    frame_time p99 = {p99}us");
    }
    if let Some(ack) = summary.input_to_local_ack.p99() {
        println!("    input_to_local_ack p99 = {ack}us (budget: 4000us)");
    }
    if let Some(ht) = summary.hit_test_latency.p99() {
        println!("    hit_test p99 = {ht}us (budget: 100us)");
    }

    let json = runtime.telemetry.emit_json()?;
    println!("  Telemetry JSON emitted: {} bytes", json.len());

    println!("\n  Phase 4 PASSED: frame rendered, telemetry collected and streamed.\n");

    // ─────────────────────────────────────────────────────────────────────────
    // PHASE 5: Safe Mode — suspend/resume all leases, mutation rejection
    // ─────────────────────────────────────────────────────────────────────────
    println!("=== Phase 5: Safe Mode (suspend + mutation rejection + resume) ===\n");

    // Verify the lease is active before suspension
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let lease = scene.leases.get(&lease_id).unwrap();
        assert_eq!(
            lease.state,
            LeaseState::Active,
            "lease should be Active before suspension"
        );
        println!("  Pre-suspend: lease state={:?}", lease.state);
    }

    // Suspend all leases (safe mode entry)
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let now = now_ms();
        scene.suspend_all_leases(now);

        // Verify all leases are suspended
        let suspended_count = scene
            .leases
            .values()
            .filter(|l| l.state == LeaseState::Suspended)
            .count();
        println!("  Suspended {suspended_count} lease(s) (safe mode entry)");
        assert!(suspended_count >= 1, "at least 1 lease should be suspended");

        let lease = scene.leases.get(&lease_id).unwrap();
        assert_eq!(lease.state, LeaseState::Suspended);
        assert!(
            lease.suspended_at_ms.is_some(),
            "should track suspension time"
        );
        assert!(
            lease.ttl_remaining_at_suspend_ms.is_some(),
            "should track remaining TTL"
        );
        println!(
            "  Lease state: {:?}, suspended_at={:?}, ttl_remaining={:?}ms",
            lease.state, lease.suspended_at_ms, lease.ttl_remaining_at_suspend_ms
        );
    }

    // Attempt mutations during suspension -- should be rejected
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let batch = SceneMutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: namespace.clone(),
            mutations: vec![SceneMutation::CreateTile {
                tab_id,
                namespace: namespace.clone(),
                lease_id,
                bounds: Rect::new(100.0, 350.0, 200.0, 100.0),
                z_order: 3,
            }],
            timing_hints: None,
            lease_id: None,
        };

        let result = scene.apply_batch(&batch);
        assert!(
            !result.applied,
            "mutations should be rejected during suspension"
        );
        let error_msg = result
            .error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_default();
        println!("  Mutation during suspension: rejected=true");
        println!("    error: {error_msg}");
        assert!(
            error_msg.contains("Suspended"),
            "error should mention Suspended state"
        );
    }

    // Tiles should still be present (state preserved, only mutations blocked)
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        assert_eq!(
            scene.tiles.len(),
            2,
            "tiles should be preserved during suspension"
        );
        println!(
            "  Tiles preserved during suspension: count={}",
            scene.tiles.len()
        );
    }

    // Render during suspension -- should still produce a frame (display frozen state)
    let suspended_frame = runtime.render_frame().await;
    println!(
        "  Frame during suspension: tiles={}, nodes={}",
        suspended_frame.tile_count, suspended_frame.node_count
    );
    assert!(
        suspended_frame.tile_count >= 2,
        "tiles should still render during suspension"
    );

    // Resume all leases (safe mode exit)
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let now = now_ms();
        scene.resume_all_leases(now);

        let active_count = scene.leases.values().filter(|l| l.is_active()).count();
        println!("  Resumed {active_count} lease(s) (safe mode exit)");
        assert!(
            active_count >= 1,
            "at least 1 lease should be active after resume"
        );

        let lease = scene.leases.get(&lease_id).unwrap();
        assert_eq!(
            lease.state,
            LeaseState::Active,
            "lease should be Active after resume"
        );
        assert!(
            lease.suspended_at_ms.is_none(),
            "suspension timestamp should be cleared"
        );
        println!("  Lease state after resume: {:?}", lease.state);
    }

    // Verify mutations work again after resume
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let batch = SceneMutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: namespace.clone(),
            mutations: vec![SceneMutation::CreateTile {
                tab_id,
                namespace: namespace.clone(),
                lease_id,
                bounds: Rect::new(100.0, 350.0, 200.0, 100.0),
                z_order: 3,
            }],
            timing_hints: None,
            lease_id: None,
        };

        let result = scene.apply_batch(&batch);
        assert!(result.applied, "mutations should succeed after resume");
        let new_tile = result.created_ids[0];
        println!("  Post-resume mutation: tile created id={new_tile}");

        // Clean up via DeleteTile mutation
        let delete_batch = SceneMutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: namespace.clone(),
            mutations: vec![SceneMutation::DeleteTile { tile_id: new_tile }],
            timing_hints: None,
            lease_id: None,
        };
        let del_result = scene.apply_batch(&delete_batch);
        assert!(del_result.applied, "delete tile should succeed");
        println!("  Cleanup: deleted post-resume tile");
    }

    println!("\n  Phase 5 PASSED: safe mode suspend/resume verified, mutation gating works.\n");

    // ─────────────────────────────────────────────────────────────────────────
    // PHASE 6: Graceful Shutdown
    // ─────────────────────────────────────────────────────────────────────────
    println!("=== Phase 6: Graceful Shutdown ===\n");

    // Send SessionClose
    tx.send(session_proto::ClientMessage {
        sequence: 5,
        timestamp_wall_us: now_wall_us(),
        payload: Some(session_proto::client_message::Payload::SessionClose(
            session_proto::SessionClose {
                reason: "Vertical slice complete".to_string(),
            },
        )),
    })
    .await?;
    println!("  SessionClose sent: reason='Vertical slice complete'");

    // Drop the sender to close the stream
    drop(tx);

    // Verify the stream closes gracefully
    let next =
        tokio::time::timeout(tokio::time::Duration::from_secs(2), response_stream.next()).await;

    match next {
        Ok(None) => println!("  Stream closed gracefully (server-side cleanup)"),
        Ok(Some(_)) => println!("  Stream received final message before close"),
        Err(_) => println!("  Stream timed out (expected -- server-side cleanup async)"),
    }

    // Final state verification
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        println!("  Final scene state:");
        println!("    tabs   = {}", scene.tabs.len());
        println!("    tiles  = {}", scene.tiles.len());
        println!(
            "    leases = {} total ({} active)",
            scene.leases.len(),
            scene.leases.values().filter(|l| l.is_active()).count()
        );
        println!(
            "    zones  = {} registered, {} with active publishes",
            scene.zone_registry.zones.len(),
            scene.zone_registry.active_publishes.len()
        );
        println!("    version = {}", scene.version);
    }

    println!("\n  Phase 6 PASSED: graceful shutdown complete.\n");

    // ─── Final Summary ─────────────────────────────────────────────────────
    println!("===================================================");
    println!("  VERTICAL SLICE COMPLETE — ALL PHASES PASSED");
    println!();
    println!("  Phase 1:   Session init (canonical caps + mandatory subs)");
    println!("  Phase 1.5: Structured error handling (cap denied + budget)");
    println!("  Phase 2:   Scene setup (tab + tiles + zone publish)");
    println!("  Phase 3:   Input loop (pointer events + hit-test + dispatch)");
    println!("  Phase 4:   Telemetry (frame metrics + stream telemetry)");
    println!("  Phase 5:   Safe mode (suspend + rejection + resume)");
    println!("  Phase 6:   Graceful shutdown");
    println!("===================================================");

    Ok(())
}

// ─── Test module ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::Instant;
    use tze_hud_input::{AgentDispatchKind, InputProcessor, PointerEvent, PointerEventKind};
    use tze_hud_protocol::proto::session as session_proto;
    use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
    use tze_hud_runtime::HeadlessRuntime;
    use tze_hud_runtime::headless::HeadlessConfig;
    use tze_hud_scene::graph::SceneGraph;
    use tze_hud_scene::mutation::{MutationBatch as SceneMutationBatch, SceneMutation};
    use tze_hud_scene::types::*;
    use tze_hud_telemetry::LatencyBucket;

    fn now_wall_us() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    // ─── Helpers ────────────────────────────────────────────────────────────

    /// Create a minimal scene with one tab and one active lease using canonical
    /// v1 capability variants. Tests that use this helper read like usage
    /// examples for the spec (validation-framework/spec.md line 398).
    fn setup_scene_with_lease() -> (SceneGraph, SceneId, SceneId) {
        let mut scene = SceneGraph::new(800.0, 600.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "test-agent",
            60_000,
            // Canonical v1 capability variants (not the legacy CreateTile / ReceiveInput).
            vec![
                Capability::CreateTiles,
                Capability::ModifyOwnTiles,
                Capability::AccessInputEvents,
            ],
        );
        (scene, tab_id, lease_id)
    }

    fn add_hit_region_tile(
        scene: &mut SceneGraph,
        tab_id: SceneId,
        lease_id: SceneId,
        tile_bounds: Rect,
        hr_bounds: Rect,
        interaction_id: &str,
        z_order: u32,
    ) -> (SceneId, SceneId) {
        let tile_id = scene
            .create_tile(tab_id, "test-agent", lease_id, tile_bounds, z_order)
            .unwrap();
        let node_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: node_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: hr_bounds,
                        interaction_id: interaction_id.to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();
        (tile_id, node_id)
    }

    // ─── Phase 1 tests: handshake timing ────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_handshake_completes_within_budget() {
        // Budget: handshake should complete in under 5000ms
        // Bind to port 0 to get an ephemeral port, then release before tonic binds.
        let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
        let free_port = listener.local_addr().unwrap().port();
        drop(listener);

        // Use an explicit config that registers "test-agent" with create_tiles.
        // This mirrors the production path: capability governance is always active.
        // (config_toml: None requires the dev-mode feature or cfg(test) in the
        // runtime crate itself — not available when the runtime is a dependency.)
        let toml = r#"
[runtime]
profile = "headless"

[[tabs]]
name = "Main"
default_tab = true

[agents.registered.test-agent]
capabilities = ["create_tiles"]
"#;

        let config = HeadlessConfig {
            width: 320,
            height: 240,
            grpc_port: free_port,
            bind_all_interfaces: false,
            psk: "test-key".to_string(),
            config_toml: Some(toml.to_string()),
        };
        let runtime = HeadlessRuntime::new(config).await.unwrap();
        let _server = runtime.start_grpc_server().await.unwrap();

        let start = Instant::now();

        let mut client = HudSessionClient::connect(format!("http://[::1]:{free_port}"))
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel::<session_proto::ClientMessage>(16);
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        tx.send(session_proto::ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(session_proto::client_message::Payload::SessionInit(
                session_proto::SessionInit {
                    agent_id: "test-agent".to_string(),
                    requested_capabilities: vec!["create_tiles".to_string()],
                    initial_subscriptions: vec![],
                    resume_token: Vec::new(),
                    min_protocol_version: 1000,
                    max_protocol_version: 1001,
                    auth_credential: Some(session_proto::AuthCredential {
                        credential: Some(session_proto::auth_credential::Credential::PreSharedKey(
                            session_proto::PreSharedKeyCredential {
                                key: "test-key".to_string(),
                            },
                        )),
                    }),
                },
            )),
        })
        .await
        .unwrap();

        let mut response = client.session(stream).await.unwrap().into_inner();

        // SessionEstablished
        use tokio_stream::StreamExt;
        let msg = response.next().await.unwrap().unwrap();
        assert!(matches!(
            msg.payload,
            Some(session_proto::server_message::Payload::SessionEstablished(
                _
            ))
        ));

        // SceneSnapshot
        let msg = response.next().await.unwrap().unwrap();
        assert!(matches!(
            msg.payload,
            Some(session_proto::server_message::Payload::SceneSnapshot(_))
        ));

        // Current degradation baseline
        let msg = response.next().await.unwrap().unwrap();
        assert!(matches!(
            msg.payload,
            Some(session_proto::server_message::Payload::DegradationNotice(_))
        ));

        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 5000,
            "handshake took {}ms, budget is 5000ms",
            elapsed.as_millis()
        );
    }

    // ─── Phase 2 tests: lease state transitions ─────────────────────────────

    #[test]
    fn test_lease_state_transitions_end_to_end() {
        let (mut scene, _tab_id, lease_id) = setup_scene_with_lease();

        // Active -> Suspended
        let now = now_ms();
        scene.suspend_lease(&lease_id, now).unwrap();
        assert_eq!(scene.leases[&lease_id].state, LeaseState::Suspended);

        // Suspended -> Active (resume)
        let now = now_ms();
        scene.resume_lease(&lease_id, now).unwrap();
        assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);

        // Active -> Orphaned (disconnect)
        let now = now_ms();
        scene.disconnect_lease(&lease_id, now).unwrap();
        assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);

        // Orphaned -> Active (reconnect)
        let now = now_ms();
        scene.reconnect_lease(&lease_id, now).unwrap();
        assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);

        // Active -> Revoked
        scene.revoke_lease(lease_id).unwrap();
        assert_eq!(scene.leases[&lease_id].state, LeaseState::Revoked);

        // Cannot transition from terminal state
        assert!(scene.suspend_lease(&lease_id, now_ms()).is_err());
    }

    // ─── Phase 3 tests: hit-test + dispatch ─────────────────────────────────

    #[test]
    fn test_hit_test_returns_correct_tile_and_node() {
        let (mut scene, tab_id, lease_id) = setup_scene_with_lease();

        let (tile_id, node_id) = add_hit_region_tile(
            &mut scene,
            tab_id,
            lease_id,
            Rect::new(100.0, 100.0, 300.0, 200.0),
            Rect::new(0.0, 0.0, 300.0, 200.0),
            "button-1",
            1,
        );

        let mut processor = InputProcessor::new();

        // Hit inside
        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );
        assert_eq!(
            result.hit,
            tze_hud_scene::HitResult::NodeHit {
                tile_id,
                node_id,
                interaction_id: "button-1".to_string(),
            }
        );
        assert_eq!(result.interaction_id, Some("button-1".to_string()));

        // Hit outside
        let result = processor.process(
            &PointerEvent {
                x: 10.0,
                y: 10.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );
        assert!(result.hit.is_none());
    }

    #[test]
    fn test_agent_dispatch_contains_correct_interaction_id() {
        let (mut scene, tab_id, lease_id) = setup_scene_with_lease();

        add_hit_region_tile(
            &mut scene,
            tab_id,
            lease_id,
            Rect::new(100.0, 100.0, 200.0, 200.0),
            Rect::new(0.0, 0.0, 200.0, 200.0),
            "my-button",
            1,
        );

        let mut processor = InputProcessor::new();

        // Press on button
        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 200.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        let dispatch = result.dispatch.expect("should have dispatch");
        assert_eq!(dispatch.kind, AgentDispatchKind::PointerDown);
        assert_eq!(dispatch.interaction_id, "my-button");
        assert_eq!(dispatch.namespace, "test-agent");

        // Release on button (activate)
        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 200.0,
                kind: PointerEventKind::Up,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        assert!(result.activated);
        let dispatch = result.dispatch.expect("should have dispatch");
        assert_eq!(dispatch.kind, AgentDispatchKind::Activated);
        assert_eq!(dispatch.interaction_id, "my-button");
    }

    #[test]
    fn test_local_ack_within_4ms_budget() {
        let (mut scene, tab_id, lease_id) = setup_scene_with_lease();

        add_hit_region_tile(
            &mut scene,
            tab_id,
            lease_id,
            Rect::new(100.0, 100.0, 200.0, 200.0),
            Rect::new(0.0, 0.0, 200.0, 200.0),
            "btn",
            1,
        );

        let mut processor = InputProcessor::new();
        let mut bucket = LatencyBucket::new("local_ack");

        for _ in 0..30 {
            let result = processor.process(
                &PointerEvent {
                    x: 200.0,
                    y: 200.0,
                    kind: PointerEventKind::Down,
                    device_id: 0,
                    timestamp: None,
                },
                &mut scene,
            );
            bucket.record(result.local_ack_us);
        }

        bucket
            .assert_p99_under(4_000)
            .expect("local_ack p99 should be under 4ms");
    }

    // ─── Phase 5 tests: safe mode ───────────────────────────────────────────

    #[test]
    fn test_safe_mode_suspends_and_resumes() {
        let (mut scene, tab_id, lease_id) = setup_scene_with_lease();

        // Create a tile while active
        let tile_id = scene
            .create_tile(
                tab_id,
                "test-agent",
                lease_id,
                Rect::new(10.0, 10.0, 100.0, 100.0),
                1,
            )
            .unwrap();

        // Suspend all
        scene.suspend_all_leases(now_ms());
        assert_eq!(scene.leases[&lease_id].state, LeaseState::Suspended);

        // Tile still exists
        assert!(scene.tiles.contains_key(&tile_id));

        // Resume all
        scene.resume_all_leases(now_ms());
        assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);

        // Tile still exists
        assert!(scene.tiles.contains_key(&tile_id));
    }

    #[test]
    fn test_safe_mode_rejects_mutations() {
        let (mut scene, tab_id, lease_id) = setup_scene_with_lease();

        // Suspend
        scene.suspend_all_leases(now_ms());

        // Try to create a tile -- should fail
        let batch = SceneMutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: "test-agent".to_string(),
            mutations: vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "test-agent".to_string(),
                lease_id,
                bounds: Rect::new(10.0, 10.0, 100.0, 100.0),
                z_order: 1,
            }],
            timing_hints: None,
            lease_id: None,
        };

        let result = scene.apply_batch(&batch);
        assert!(
            !result.applied,
            "mutations should be rejected during suspension"
        );

        let error_msg = result
            .error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            error_msg.contains("Suspended"),
            "error should mention Suspended state, got: {error_msg}"
        );
    }

    #[test]
    fn test_safe_mode_mutations_succeed_after_resume() {
        let (mut scene, tab_id, lease_id) = setup_scene_with_lease();

        // Suspend then resume
        scene.suspend_all_leases(now_ms());
        scene.resume_all_leases(now_ms());

        // Mutations should work now
        let batch = SceneMutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: "test-agent".to_string(),
            mutations: vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "test-agent".to_string(),
                lease_id,
                bounds: Rect::new(10.0, 10.0, 100.0, 100.0),
                z_order: 1,
            }],
            timing_hints: None,
            lease_id: None,
        };

        let result = scene.apply_batch(&batch);
        assert!(result.applied, "mutations should succeed after resume");
        assert_eq!(result.created_ids.len(), 1);
    }

    // ─── Budget enforcement test ────────────────────────────────────────────

    #[test]
    fn test_budget_enforcement_rejects_over_limit() {
        let (mut scene, tab_id, lease_id) = setup_scene_with_lease();

        // Default budget allows max 8 tiles per lease.
        // Create 8 tiles to fill the budget.
        for i in 0..8u32 {
            let batch = SceneMutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "test-agent".to_string(),
                mutations: vec![SceneMutation::CreateTile {
                    tab_id,
                    namespace: "test-agent".to_string(),
                    lease_id,
                    bounds: Rect::new(i as f32 * 90.0, 10.0, 80.0, 60.0),
                    z_order: i + 1,
                }],
                timing_hints: None,
                lease_id: None,
            };
            let result = scene.apply_batch(&batch);
            assert!(result.applied, "tile {i} should be within budget");
        }

        // 9th tile should exceed budget
        let batch = SceneMutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: "test-agent".to_string(),
            mutations: vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "test-agent".to_string(),
                lease_id,
                bounds: Rect::new(0.0, 100.0, 80.0, 60.0),
                z_order: 9,
            }],
            timing_hints: None,
            lease_id: None,
        };
        let result = scene.apply_batch(&batch);
        assert!(!result.applied, "9th tile should exceed budget");

        let error_msg = result
            .error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            error_msg.contains("tiles") || error_msg.contains("budget"),
            "error should reference tile budget, got: {error_msg}"
        );
    }

    // ─── Zone publish test ──────────────────────────────────────────────────

    #[test]
    fn test_zone_publish_and_query() {
        let (mut scene, _tab_id, _lease_id) = setup_scene_with_lease();

        // Register default zones
        scene.zone_registry = ZoneRegistry::with_defaults();

        // Publish to status-bar
        let mut entries = std::collections::HashMap::new();
        entries.insert("key".to_string(), "value".to_string());
        scene
            .publish_to_zone(
                "status-bar",
                ZoneContent::StatusBar(StatusBarPayload { entries }),
                "test-agent",
                Some("test-key".to_string()),
                None,
                None,
            )
            .unwrap();

        let active = scene.zone_registry.active_for_zone("status-bar");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].publisher_namespace, "test-agent");

        // Publish again with same merge key -- should replace
        let mut entries2 = std::collections::HashMap::new();
        entries2.insert("key".to_string(), "updated".to_string());
        scene
            .publish_to_zone(
                "status-bar",
                ZoneContent::StatusBar(StatusBarPayload { entries: entries2 }),
                "test-agent",
                Some("test-key".to_string()),
                None,
                None,
            )
            .unwrap();

        let active = scene.zone_registry.active_for_zone("status-bar");
        assert_eq!(active.len(), 1, "MergeByKey should replace, not accumulate");
    }

    // ─── Zone not found test ────────────────────────────────────────────────

    #[test]
    fn test_zone_publish_fails_for_unknown_zone() {
        let (mut scene, _tab_id, _lease_id) = setup_scene_with_lease();

        let result = scene.publish_to_zone(
            "nonexistent-zone",
            ZoneContent::StreamText("hello".to_string()),
            "test-agent",
            None,
            None,
            None,
        );

        assert!(result.is_err(), "should fail for unknown zone");
    }

    // ─── Structured error handling tests ────────────────────────────────────

    /// GIVEN an agent requests a legacy (non-canonical) capability name
    /// WHEN the runtime validates the SessionInit or LeaseRequest
    /// THEN it returns CONFIG_UNKNOWN_CAPABILITY with a canonical hint
    ///
    /// (configuration/spec.md §Capability Vocabulary, lines 149-164)
    #[test]
    fn test_legacy_capability_names_rejected_with_hint() {
        use tze_hud_protocol::auth::validate_canonical_capabilities;

        // Legacy names must be rejected — agents that copy old examples will
        // get clear feedback pointing to the canonical replacement.
        let result = validate_canonical_capabilities(&[
            "create_tile".to_string(),   // legacy: should be create_tiles
            "receive_input".to_string(), // legacy: should be access_input_events
        ]);

        let unknowns =
            result.expect_err("legacy names must be rejected with CONFIG_UNKNOWN_CAPABILITY");
        assert_eq!(
            unknowns.len(),
            2,
            "both legacy names must be reported (collect-all, not fail-fast)"
        );

        let create_unknown = unknowns
            .iter()
            .find(|u| u.unknown == "create_tile")
            .expect("create_tile must appear in unknowns");
        assert!(
            create_unknown.hint.contains("create_tiles"),
            "hint must point to canonical create_tiles: {:?}",
            create_unknown.hint
        );

        let receive_unknown = unknowns
            .iter()
            .find(|u| u.unknown == "receive_input")
            .expect("receive_input must appear in unknowns");
        assert!(
            receive_unknown.hint.contains("access_input_events"),
            "hint must point to canonical access_input_events: {:?}",
            receive_unknown.hint
        );
    }

    /// GIVEN an agent's canonical capability request
    /// WHEN the runtime validates it
    /// THEN all canonical names are accepted without error
    #[test]
    fn test_canonical_capability_names_accepted() {
        use tze_hud_protocol::auth::validate_canonical_capabilities;

        let result = validate_canonical_capabilities(&[
            "create_tiles".to_string(),
            "modify_own_tiles".to_string(),
            "access_input_events".to_string(),
            "read_scene_topology".to_string(),
        ]);

        assert!(
            result.is_ok(),
            "all canonical v1 capability names must be accepted without error"
        );
    }

    /// GIVEN an agent has a limited tile budget (max_tiles = 2)
    /// WHEN the agent submits a mutation batch that would exceed the budget
    /// THEN the batch is rejected with a structured budget-exceeded error
    ///
    /// (session-protocol/spec.md §Resource Budget Enforcement)
    /// Budget enforcement runs in apply_batch, which is the path used for both
    /// gRPC MutationBatch messages and internal scene mutations.
    #[test]
    fn test_budget_exceeded_returns_structured_error() {
        let (mut scene, tab_id, lease_id) = setup_scene_with_lease();

        // Constrain the lease to 2 tiles to make the budget easy to exhaust.
        scene
            .leases
            .get_mut(&lease_id)
            .unwrap()
            .resource_budget
            .max_tiles = 2;

        // First two batches succeed (within budget).
        for i in 0..2u32 {
            let batch = SceneMutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "test-agent".to_string(),
                mutations: vec![SceneMutation::CreateTile {
                    tab_id,
                    namespace: "test-agent".to_string(),
                    lease_id,
                    bounds: Rect::new(i as f32 * 100.0, 10.0, 90.0, 50.0),
                    z_order: i + 1,
                }],
                timing_hints: None,
                lease_id: Some(lease_id),
            };
            let result = scene.apply_batch(&batch);
            assert!(result.applied, "tile {i} within budget must succeed");
        }

        // Third batch exceeds budget — must return applied=false with a structured error.
        let over_budget = SceneMutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: "test-agent".to_string(),
            mutations: vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "test-agent".to_string(),
                lease_id,
                bounds: Rect::new(200.0, 10.0, 90.0, 50.0),
                z_order: 3,
            }],
            timing_hints: None,
            lease_id: Some(lease_id),
        };
        let result = scene.apply_batch(&over_budget);
        assert!(
            !result.applied,
            "third tile must be rejected (budget exceeded)"
        );

        // Error message must reference the budget constraint so that the caller
        // can surface actionable feedback to the agent.
        let err_msg = result
            .error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err_msg.contains("tiles") || err_msg.contains("budget"),
            "error must reference tile budget: {err_msg}"
        );
    }

    /// GIVEN an agent subscribes to SCENE_TOPOLOGY and ZONE_EVENTS without the required capabilities
    /// WHEN the runtime processes the SessionInit
    /// THEN both gated subscriptions are denied while LEASE_CHANGES remains active (mandatory)
    ///
    /// Subscription gating (session-protocol/spec.md §Subscription Categories):
    /// - SCENE_TOPOLOGY requires `read_scene_topology` capability
    /// - ZONE_EVENTS requires any `publish_zone:<zone>` capability
    /// - LEASE_CHANGES is mandatory: always active regardless of capabilities
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_gated_subscriptions_denied_without_required_capabilities() {
        // Use config_toml to restrict the agent's capabilities.
        // Without this, dev mode (config_toml = None) grants all capabilities.
        let toml = r#"
[runtime]
profile = "headless"

[[tabs]]
name = "Main"
default_tab = true

[agents.dynamic_policy]
allow_dynamic_agents = false

[agents.registered.restricted-agent]
capabilities = ["create_tiles", "modify_own_tiles"]
"#;
        // Bind to port 0 to get an ephemeral port, then release the listener
        // before tonic binds. Avoids hardcoded ports that may conflict in CI.
        let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
        let free_port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = HeadlessConfig {
            width: 320,
            height: 240,
            grpc_port: free_port,
            bind_all_interfaces: false,
            psk: "test-key".to_string(),
            config_toml: Some(toml.to_string()),
        };
        let runtime = HeadlessRuntime::new(config).await.unwrap();
        let _server = runtime.start_grpc_server().await.unwrap();

        let mut client = HudSessionClient::connect(format!("http://[::1]:{free_port}"))
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel::<session_proto::ClientMessage>(16);
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        tx.send(session_proto::ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(session_proto::client_message::Payload::SessionInit(
                session_proto::SessionInit {
                    agent_id: "restricted-agent".to_string(),
                    // Request SCENE_TOPOLOGY and ZONE_EVENTS without the required capabilities.
                    // This demonstrates the subscription gating behaviour: the agent will
                    // receive LEASE_CHANGES (mandatory) but not the gated categories.
                    requested_capabilities: vec![
                        "create_tiles".to_string(),
                        "modify_own_tiles".to_string(),
                        // Intentionally omit read_scene_topology (needed for SCENE_TOPOLOGY)
                        // Intentionally omit publish_zone:* (needed for ZONE_EVENTS)
                    ],
                    initial_subscriptions: vec![
                        "SCENE_TOPOLOGY".to_string(), // gated: denied (no read_scene_topology)
                        "LEASE_CHANGES".to_string(),  // mandatory: always active
                        "ZONE_EVENTS".to_string(),    // gated: denied (no publish_zone:*)
                    ],
                    resume_token: Vec::new(),
                    min_protocol_version: 1000,
                    max_protocol_version: 1001,
                    auth_credential: Some(session_proto::AuthCredential {
                        credential: Some(session_proto::auth_credential::Credential::PreSharedKey(
                            session_proto::PreSharedKeyCredential {
                                key: "test-key".to_string(),
                            },
                        )),
                    }),
                },
            )),
        })
        .await
        .unwrap();

        let mut response = client.session(stream).await.unwrap().into_inner();
        use tokio_stream::StreamExt;
        let msg = response.next().await.unwrap().unwrap();

        match &msg.payload {
            Some(session_proto::server_message::Payload::SessionEstablished(established)) => {
                // SCENE_TOPOLOGY must be absent: agent lacks read_scene_topology.
                assert!(
                    !established
                        .active_subscriptions
                        .contains(&"SCENE_TOPOLOGY".to_string()),
                    "SCENE_TOPOLOGY must be denied without read_scene_topology capability; \
                     active: {:?}",
                    established.active_subscriptions
                );
                // ZONE_EVENTS must be absent: agent lacks any publish_zone:* capability.
                assert!(
                    !established
                        .active_subscriptions
                        .contains(&"ZONE_EVENTS".to_string()),
                    "ZONE_EVENTS must be denied without publish_zone:* capability; \
                     active: {:?}",
                    established.active_subscriptions
                );
                // LEASE_CHANGES must be active: it is mandatory regardless of capabilities.
                assert!(
                    established
                        .active_subscriptions
                        .contains(&"LEASE_CHANGES".to_string()),
                    "LEASE_CHANGES must always be active (mandatory category); \
                     active: {:?}",
                    established.active_subscriptions
                );
            }
            other => panic!("Expected SessionEstablished, got: {other:?}"),
        }
    }
}
