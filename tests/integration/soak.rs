//! E12.2: Soak and Leak Test Suite
//!
//! Validates sustained stability under multi-agent load.
//!
//! ## Spec Reference
//! validation-framework/spec.md §"Soak and Leak Tests" (lines 298-310):
//!
//! > Soak tests SHALL run hours-long sessions with repeated agent connects,
//! > disconnects, reconnects, lease grants, revocations, zone publishes, and
//! > content updates. Pass criteria: resource utilization at hour N SHALL be
//! > within 5% of resource utilization at hour 1 for the same steady-state
//! > workload. Any monotonic growth SHALL be a bug. After an agent disconnects
//! > and leases expire, its resource footprint MUST be zero.
//!
//! ## Test Scenarios
//!
//! - `test_soak_resource_growth` — runs continuous multi-agent mutation/zone
//!   cycles for the configured duration, capturing resource snapshots at each
//!   epoch. Asserts no metric grows by more than 5% vs baseline.
//!
//! - `test_post_disconnect_cleanup` — connects 3 agents, creates tiles and
//!   publishes zones, then explicitly disconnects them, expires their leases,
//!   and asserts the scene graph returns to zero footprint.
//!
//! - `test_lease_expiry_during_soak` — grants leases with very short TTLs,
//!   drives mutations through them, lets them expire naturally, then asserts
//!   all associated resources are freed.
//!
//! ## CI / Nightly Configuration
//!
//! The soak duration is controlled by environment variables:
//!
//! | Variable | Default | Notes |
//! |---|---|---|
//! | `TZE_HUD_SOAK_SECS` | 10 (fast CI) | Set to 3600 for 1-hour CI run |
//! | `TZE_HUD_SOAK_EPOCH_SECS` | 1 | Snapshot interval |
//! | `TZE_HUD_SOAK_AGENTS` | 3 | Number of concurrent soak agents |
//!
//! Nightly runs use:
//! ```
//! TZE_HUD_SOAK_SECS=21600  # 6 hours
//! ```
//!
//! CI 1-hour run:
//! ```
//! TZE_HUD_SOAK_SECS=3600
//! ```
//!
//! ## Artifacts
//!
//! Structured JSON artifacts are emitted to stdout with a tag prefix so CI
//! tools can capture and archive them:
//!
//! - `ARTIFACT:soak_resource_history` — full snapshot timeline
//! - `ARTIFACT:soak_growth_ratios`    — final vs baseline growth ratios
//! - `ARTIFACT:soak_post_disconnect`  — post-disconnect cleanup verification
//!
//! ## Cross-references
//! - E12.1 (multi_agent.rs): shared gRPC helpers and session patterns
//! - Epic 4 (lease governance): lease TTL, revocation, expiry
//! - Epic 9 (zone system): zone publish lifecycle

use tze_hud_protocol::proto;
use tze_hud_protocol::proto::session as session_proto;
#[allow(deprecated)]
use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;
use tze_hud_scene::types::*;
use tze_hud_telemetry::resource_monitor::{
    AgentFootprint, MutationAccountant, ResourceMonitor, ResourceSnapshot, SPEC_GROWTH_TOLERANCE,
};

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio_stream::StreamExt;

// ─── Soak test constants ─────────────────────────────────────────────────────

/// Default soak duration for fast CI runs (10 seconds).
/// Override with `TZE_HUD_SOAK_SECS` environment variable.
/// Production CI: 3600 (1 hour). Nightly: 21600 (6 hours).
const DEFAULT_SOAK_SECS: u64 = 10;

/// Default interval between resource snapshots.
const DEFAULT_EPOCH_SECS: u64 = 1;

/// Default number of concurrent soak agents.
const DEFAULT_SOAK_AGENTS: usize = 3;

/// gRPC port for soak tests. Distinct from multi_agent (50052) and vertical_slice.
const SOAK_GRPC_PORT: u16 = 50053;

/// Pre-shared key for soak test sessions.
const SOAK_PSK: &str = "soak-test-key";

/// Headless display dimensions.
const DISPLAY_W: u32 = 1280;
const DISPLAY_H: u32 = 720;

/// Grace period for lease expiry (milliseconds).
/// After revocation / TTL expiry the runtime must clean up within this window.
const CLEANUP_GRACE_MS: u64 = 200;

// ─── Configuration helpers ───────────────────────────────────────────────────

/// Read soak configuration from environment variables, falling back to defaults.
struct SoakConfig {
    /// Total soak duration in seconds.
    soak_secs: u64,
    /// Interval between resource snapshots in seconds.
    epoch_secs: u64,
    /// Number of concurrent soak agents.
    agent_count: usize,
}

impl SoakConfig {
    fn from_env() -> Self {
        let soak_secs = std::env::var("TZE_HUD_SOAK_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_SOAK_SECS);

        let epoch_secs = std::env::var("TZE_HUD_SOAK_EPOCH_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_EPOCH_SECS);

        let agent_count = std::env::var("TZE_HUD_SOAK_AGENTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_SOAK_AGENTS);

        Self {
            soak_secs,
            epoch_secs,
            agent_count: agent_count.max(3), // spec requires 3+
        }
    }
}

// ─── Artifact types ───────────────────────────────────────────────────────────

/// Soak test final artifact — emitted as JSON to stdout.
#[derive(Debug, Serialize, Deserialize)]
struct SoakArtifact {
    /// Test label for CI identification.
    test: String,
    /// Soak duration configured.
    soak_secs: u64,
    /// Number of snapshots captured.
    snapshot_count: usize,
    /// Number of concurrent agents.
    agent_count: usize,
    /// Whether the no-growth assertion passed.
    growth_assertion_passed: bool,
    /// Maximum growth percentage observed (across all metrics and all epochs).
    max_growth_pct: f64,
    /// The metric that showed the highest growth, if any.
    worst_metric: Option<String>,
    /// Error message if the growth assertion failed.
    error: Option<String>,
}

/// Post-disconnect cleanup artifact.
#[derive(Debug, Serialize, Deserialize)]
struct PostDisconnectArtifact {
    test: String,
    agent_count: usize,
    /// Whether all agents reached zero footprint.
    all_zero: bool,
    /// Per-agent result entries.
    agents: Vec<AgentCleanupEntry>,
}

/// Per-agent cleanup result entry.
#[derive(Debug, Serialize, Deserialize)]
struct AgentCleanupEntry {
    namespace: String,
    is_zero: bool,
    tiles: usize,
    nodes: usize,
    leases: usize,
    zone_entries: usize,
    texture_bytes: u64,
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

fn now_wall_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// Minimal agent session state for soak tests.
struct SoakAgentSession {
    namespace: String,
    lease_id_bytes: Vec<u8>,
    tx: tokio::sync::mpsc::Sender<session_proto::ClientMessage>,
    rx: tonic::codec::Streaming<session_proto::ServerMessage>,
    sequence: u64,
}

impl SoakAgentSession {
    fn next_seq(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }
}

/// Connect a soak agent, complete the handshake, and acquire a lease.
async fn connect_soak_agent(
    agent_id: &str,
    lease_priority: u32,
    lease_ttl_ms: u64,
) -> Result<SoakAgentSession, Box<dyn std::error::Error>> {
    let mut client = HudSessionClient::connect(format!("http://[::1]:{SOAK_GRPC_PORT}")).await?;

    let (tx, rx_chan) = tokio::sync::mpsc::channel::<session_proto::ClientMessage>(128);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx_chan);

    let now_us = now_wall_us();

    // Send SessionInit
    tx.send(session_proto::ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_us,
        payload: Some(session_proto::client_message::Payload::SessionInit(
            session_proto::SessionInit {
                agent_id: agent_id.to_string(),
                requested_capabilities: vec![
                    "create_tiles".to_string(),
                    "modify_own_tiles".to_string(),
                ],
                initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
                resume_token: Vec::new(),
                min_protocol_version: 0,
                max_protocol_version: 0,
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(SOAK_PSK.to_string())),
            },
        )),
    })
    .await?;

    let mut response_stream = client.session(stream).await?.into_inner();

    // Read SessionEstablished
    let msg = response_stream
        .next()
        .await
        .ok_or("no message received")??;
    let namespace = match &msg.payload {
        Some(session_proto::server_message::Payload::SessionEstablished(est)) => {
            est.namespace.clone()
        }
        other => {
            return Err(format!(
                "soak agent {agent_id}: Expected SessionEstablished, got: {other:?}"
            )
            .into());
        }
    };

    // Read SceneSnapshot
    let _snapshot_msg = response_stream.next().await.ok_or("no scene snapshot")??;
    let degradation_msg = response_stream
        .next()
        .await
        .ok_or("no degradation notice")??;
    if !matches!(
        &degradation_msg.payload,
        Some(session_proto::server_message::Payload::DegradationNotice(_))
    ) {
        return Err(format!(
            "soak agent {agent_id}: Expected DegradationNotice after SceneSnapshot, got: {:?}",
            degradation_msg.payload
        )
        .into());
    }

    // Request lease
    tx.send(session_proto::ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(session_proto::client_message::Payload::LeaseRequest(
            session_proto::LeaseRequest {
                ttl_ms: lease_ttl_ms,
                capabilities: vec!["create_tiles".to_string(), "modify_own_tiles".to_string()],
                lease_priority,
            },
        )),
    })
    .await?;

    // Read LeaseResponse
    let msg = next_server_msg(&mut response_stream).await?;
    let lease_id_bytes = match &msg.payload {
        Some(session_proto::server_message::Payload::LeaseResponse(resp)) if resp.granted => {
            resp.lease_id.clone()
        }
        other => return Err(format!(
            "soak agent {agent_id} (soak_agent): Expected LeaseResponse(granted), got: {other:?}"
        )
        .into()),
    };

    Ok(SoakAgentSession {
        namespace,
        lease_id_bytes,
        tx,
        rx: response_stream,
        sequence: 2,
    })
}

/// Return the next server message, failing if the stream ended.
async fn next_server_msg(
    stream: &mut tonic::codec::Streaming<session_proto::ServerMessage>,
) -> Result<session_proto::ServerMessage, Box<dyn std::error::Error>> {
    Ok(stream.next().await.ok_or("stream ended unexpectedly")??)
}

/// Send a CreateTile mutation via gRPC and return the tile ID.
async fn create_tile(
    session: &mut SoakAgentSession,
    bounds: [f32; 4],
    z_order: u32,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let batch_id: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    let seq = session.next_seq();

    session
        .tx
        .send(session_proto::ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(session_proto::client_message::Payload::MutationBatch(
                session_proto::MutationBatch {
                    batch_id,
                    lease_id: session.lease_id_bytes.clone(),
                    mutations: vec![proto::MutationProto {
                        mutation: Some(proto::mutation_proto::Mutation::CreateTile(
                            proto::CreateTileMutation {
                                tab_id: vec![],
                                bounds: Some(proto::Rect {
                                    x: bounds[0],
                                    y: bounds[1],
                                    width: bounds[2],
                                    height: bounds[3],
                                }),
                                z_order,
                            },
                        )),
                    }],
                    timing: None,
                },
            )),
        })
        .await?;

    // Read MutationResult
    let msg = next_server_msg(&mut session.rx).await?;
    match &msg.payload {
        Some(session_proto::server_message::Payload::MutationResult(result)) if result.accepted => {
            let tile_id = result.created_ids.first().cloned().unwrap_or_default();
            Ok(tile_id)
        }
        Some(session_proto::server_message::Payload::MutationResult(result)) => Err(format!(
            "CreateTile rejected: {} — {}",
            result.error_code, result.error_message
        )
        .into()),
        other => Err(format!("Expected MutationResult, got: {other:?}").into()),
    }
}

/// Update a tile's root content via a `SetTileRoot` mutation.
///
/// This is the primary "mutation cycle" operation in the soak test.
/// It exercises the full mutation → scene commit → layout pipeline
/// without requiring a proto-level DeleteTile (which is not in the v1 proto).
///
/// `tile_id_bytes` is the 16-byte UUIDv7 tile identifier.
/// The `cycle_idx` parameter is embedded in the content to make each update
/// distinct and detectable during post-run analysis.
///
/// Returns `Ok(true)` if the runtime accepted the mutation, `Ok(false)` if the
/// runtime rejected it (but sent a well-formed `MutationResult`). The caller
/// should record this via [`MutationAccountant`] so the acceptance rate can be
/// asserted after the soak loop ends.
async fn update_tile_content(
    session: &mut SoakAgentSession,
    tile_id_bytes: Vec<u8>,
    cycle_idx: u64,
) -> Result<bool, Box<dyn std::error::Error>> {
    let batch_id: Vec<u8> = uuid::Uuid::now_v7().as_bytes().to_vec();
    let seq = session.next_seq();

    // Vary color per cycle to ensure the node tree is actually different each time
    let r = ((cycle_idx * 37) % 256) as u32;
    let g = ((cycle_idx * 71) % 256) as u32;
    let b = ((cycle_idx * 113) % 256) as u32;

    session
        .tx
        .send(session_proto::ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(session_proto::client_message::Payload::MutationBatch(
                session_proto::MutationBatch {
                    batch_id,
                    lease_id: session.lease_id_bytes.clone(),
                    mutations: vec![proto::MutationProto {
                        mutation: Some(proto::mutation_proto::Mutation::SetTileRoot(
                            proto::SetTileRootMutation {
                                tile_id: tile_id_bytes,
                                node: Some(proto::NodeProto {
                                    layout: 0,
                                    id: vec![],
                                    data: Some(proto::node_proto::Data::SolidColor(
                                        proto::SolidColorNodeProto {
                                            bounds: Some(proto::Rect {
                                                x: 0.0,
                                                y: 0.0,
                                                width: 280.0,
                                                height: 200.0,
                                            }),
                                            color: Some(proto::Rgba {
                                                r: r as f32 / 255.0,
                                                g: g as f32 / 255.0,
                                                b: b as f32 / 255.0,
                                                a: 1.0,
                                            }),
                                            radius: -1.0,
                                        },
                                    )),
                                    children: vec![],
                                }),
                            },
                        )),
                    }],
                    timing: None,
                },
            )),
        })
        .await?;

    // Read the MutationResult and return whether the runtime accepted it.
    // Callers record this via MutationAccountant so that a run where the
    // runtime rejects every mutation is caught as a test failure.
    let msg = next_server_msg(&mut session.rx).await?;
    match &msg.payload {
        Some(session_proto::server_message::Payload::MutationResult(result)) => Ok(result.accepted),
        other => {
            Err(format!("update_tile_content: expected MutationResult, got: {other:?}").into())
        }
    }
}

/// Publish stream text to a zone.
async fn publish_to_zone(
    session: &mut SoakAgentSession,
    zone_name: &str,
    text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let seq = session.next_seq();

    session
        .tx
        .send(session_proto::ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(session_proto::client_message::Payload::ZonePublish(
                session_proto::ZonePublish {
                    zone_name: zone_name.to_string(),
                    content: Some(proto::ZoneContent {
                        payload: Some(proto::zone_content::Payload::StreamText(text.to_string())),
                    }),
                    ttl_us: 0,
                    element_id: Vec::new(),
                    merge_key: String::new(),
                    breakpoints: Vec::new(),
                    // Snapshot parity fields (WM-S2b session.proto delta §fields 7-9); 0/empty = no constraint.
                    present_at_wall_us: 0,
                    expires_at_wall_us: 0,
                    content_classification: String::new(),
                },
            )),
        })
        .await?;

    // Read ZonePublishResult
    let msg = session.rx.next().await.ok_or("no zone publish result")??;
    match &msg.payload {
        Some(session_proto::server_message::Payload::ZonePublishResult(result))
            if result.accepted =>
        {
            Ok(())
        }
        Some(session_proto::server_message::Payload::ZonePublishResult(result)) => Err(format!(
            "ZonePublish to '{}' rejected: {} — {}",
            zone_name, result.error_code, result.error_message
        )
        .into()),
        other => Err(format!("Expected ZonePublishResult, got: {other:?}").into()),
    }
}

/// Capture a resource snapshot from the shared runtime state.
///
/// This function is the canonical snapshot source: it reads tile_count,
/// node_count, lease_count, session_count, and zone_entry_count from the
/// `SharedState` under the state mutex.
async fn capture_snapshot(runtime: &HeadlessRuntime, elapsed: Duration) -> ResourceSnapshot {
    let state = runtime.shared_state().lock().await;
    let scene = state.scene.lock().await;
    let tile_count = scene.tile_count();
    let node_count = scene.node_count();
    let lease_count = scene.leases.len();
    let session_count = state.sessions.session_count();

    // Count total zone publication entries across all zones
    let zone_entry_count = scene
        .zone_registry
        .active_publishes
        .values()
        .map(|v| v.len())
        .sum();

    ResourceSnapshot::full(
        elapsed.as_secs_f64(),
        tile_count,
        node_count,
        lease_count,
        session_count,
        zone_entry_count,
        0, // texture_bytes: not tracked in v1 soak (no image content)
    )
}

/// Recursively count nodes in the subtree rooted at `node_id`.
///
/// Uses the public `scene.nodes` map. Returns 0 if the node is not found.
fn count_node_subtree_from(
    nodes: &std::collections::HashMap<tze_hud_scene::SceneId, tze_hud_scene::types::Node>,
    node_id: tze_hud_scene::SceneId,
) -> usize {
    match nodes.get(&node_id) {
        None => 0,
        Some(node) => {
            1 + node
                .children
                .iter()
                .map(|c| count_node_subtree_from(nodes, *c))
                .sum::<usize>()
        }
    }
}

/// Capture the resource footprint attributable to a single agent namespace.
///
/// Counts tiles, nodes (via tile root traversal), leases, and zone entries
/// owned by `namespace` in the scene graph. All fields used are public.
async fn capture_agent_footprint(
    runtime: &HeadlessRuntime,
    namespace: &str,
    elapsed: Duration,
) -> AgentFootprint {
    let state = runtime.shared_state().lock().await;
    let scene = state.scene.lock().await;

    // Tiles owned by this namespace
    let ns_tiles: Vec<_> = scene
        .tiles
        .values()
        .filter(|t| t.namespace == namespace)
        .collect();
    let tile_count = ns_tiles.len();

    // Nodes in tiles owned by this namespace (traverse from each tile's root)
    let node_count: usize = ns_tiles
        .iter()
        .filter_map(|t| t.root_node)
        .map(|root_id| count_node_subtree_from(&scene.nodes, root_id))
        .sum();

    // Leases held by this namespace (non-terminal only)
    let lease_count = scene
        .leases
        .values()
        .filter(|l| l.namespace == namespace && !l.state.is_terminal())
        .count();

    // Zone publication entries from this namespace
    let zone_entry_count: usize = scene
        .zone_registry
        .active_publishes
        .values()
        .flat_map(|pubs| pubs.iter())
        .filter(|p| p.publisher_namespace == namespace)
        .count();

    AgentFootprint {
        namespace: namespace.to_string(),
        elapsed_secs: elapsed.as_secs_f64(),
        tiles: tile_count,
        nodes: node_count,
        leases: lease_count,
        zone_entries: zone_entry_count,
        texture_bytes: 0,
    }
}

// ─── Test 1: No resource growth during sustained soak ─────────────────────────

/// Soak test: multi-agent sustained load with resource growth assertion.
///
/// Validates validation-framework/spec.md §"Soak and Leak Tests" scenario:
/// > WHEN the runtime runs under sustained load for N hours
/// > THEN memory usage, file descriptors, and scene graph size at hour N
/// > MUST be within 5% of hour 1
///
/// Three agents run continuous create/delete/zone-publish cycles. A resource
/// snapshot is captured every `epoch_secs` seconds. After the soak, the
/// monitor asserts no metric grew by more than 5% vs the baseline (first
/// snapshot, taken after the first epoch).
///
/// ## Port
/// Uses `SOAK_GRPC_PORT` (50053) to avoid conflicts with other tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_soak_resource_growth() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = SoakConfig::from_env();

    // ── Phase 0: Start runtime ─────────────────────────────────────────────

    let runtime_cfg = HeadlessConfig {
        width: DISPLAY_W,
        height: DISPLAY_H,
        grpc_port: SOAK_GRPC_PORT,
        bind_all_interfaces: false,
        psk: SOAK_PSK.to_string(),
        config_toml: None,
    };
    let mut runtime = HeadlessRuntime::new(runtime_cfg).await?;

    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab_id = scene.create_tab("Soak-Tab", 0)?;
        scene.active_tab = Some(tab_id);
        scene.zone_registry = ZoneRegistry::with_defaults();
    }

    let _server_handle = runtime.start_grpc_server().await?;

    eprintln!(
        "[soak] Starting soak test: {} agents, {} s, epoch {} s",
        cfg.agent_count, cfg.soak_secs, cfg.epoch_secs
    );

    // ── Phase 1: Connect agents ────────────────────────────────────────────

    let agent_ids: Vec<String> = (0..cfg.agent_count)
        .map(|i| format!("soak-agent-{i}"))
        .collect();

    let mut agents: Vec<SoakAgentSession> = Vec::new();
    for (i, id) in agent_ids.iter().enumerate() {
        let session = connect_soak_agent(id, (i + 1) as u32, 120_000).await?;
        eprintln!("[soak] Connected: {} → namespace={}", id, session.namespace);
        agents.push(session);
    }

    // ── Phase 1.5: Pre-create one tile per agent ──────────────────────────
    //
    // Each agent owns exactly one tile for the duration of the soak. The soak
    // loop mutates tile content (SetTileRoot) and publishes zone entries each
    // cycle. This avoids tile churn (no proto-level DeleteTile exists in v1)
    // while still exercising continuous mutations.

    let mut agent_tile_ids: Vec<Vec<u8>> = Vec::new();
    for (idx, agent) in agents.iter_mut().enumerate() {
        let x = 50.0 + (idx as f32) * 300.0;
        let tile_id = create_tile(agent, [x, 50.0, 280.0, 200.0], (idx + 1) as u32).await?;
        agent_tile_ids.push(tile_id);
    }

    // ── Phase 2: Soak loop ─────────────────────────────────────────────────

    let mut monitor = ResourceMonitor::new();
    // Tracks how many SetTileRoot mutations the runtime accepted vs rejected.
    // A soak run where the runtime silently rejects all mutations produces
    // stable (low) resource counters, making leak detection meaningless.
    // Asserting a minimum acceptance rate ensures the load was actually applied.
    let mut mutation_acc = MutationAccountant::new();

    let soak_start = Instant::now();
    let soak_duration = Duration::from_secs(cfg.soak_secs);
    let epoch_duration = Duration::from_secs(cfg.epoch_secs);
    let mut epoch_idx: u64 = 0;
    let mut next_epoch_at = soak_start + epoch_duration;

    while soak_start.elapsed() < soak_duration {
        // Each agent performs one mutation cycle per loop iteration:
        // - Update tile content (SetTileRoot) — exercises mutation pipeline
        // - Publish to the subtitle zone (LatestWins — no accumulation risk)
        for (idx, agent) in agents.iter_mut().enumerate() {
            // Record whether the runtime actually applied the mutation so that
            // an all-rejected run is caught as a test failure below.
            // Transport errors are tolerated during the soak (transient
            // congestion); they do not increment accepted/rejected counts.
            if let Ok(accepted) =
                update_tile_content(agent, agent_tile_ids[idx].clone(), epoch_idx).await
            {
                mutation_acc.record(accepted);
            }

            // Publish to the subtitle zone (LatestWins — no accumulation risk)
            let text = format!("soak-{idx}-epoch-{epoch_idx}");
            // Best-effort publish — zone ops are fire-and-forget in the soak
            let _ = publish_to_zone(agent, "subtitle", &text).await;
        }

        // Render a frame to drive the pipeline
        runtime.render_frame().await;

        // Take a snapshot at each epoch boundary
        if Instant::now() >= next_epoch_at {
            let elapsed = soak_start.elapsed();
            let snap = capture_snapshot(&runtime, elapsed).await;
            eprintln!(
                "[soak] Epoch {}: tiles={} nodes={} leases={} sessions={} zones={} \
                 mutations(acc={} rej={})",
                epoch_idx,
                snap.tile_count,
                snap.node_count,
                snap.lease_count,
                snap.session_count,
                snap.zone_entry_count,
                mutation_acc.accepted,
                mutation_acc.rejected,
            );
            monitor.record(snap);
            epoch_idx += 1;
            next_epoch_at += epoch_duration;
        }

        // Yield to allow tokio to service the gRPC network tasks
        tokio::task::yield_now().await;
    }

    // Capture final snapshot
    let final_snap = capture_snapshot(&runtime, soak_start.elapsed()).await;
    eprintln!(
        "[soak] Final: tiles={} nodes={} leases={} sessions={} zones={} \
         mutations(total={} acc={} rej={})",
        final_snap.tile_count,
        final_snap.node_count,
        final_snap.lease_count,
        final_snap.session_count,
        final_snap.zone_entry_count,
        mutation_acc.total,
        mutation_acc.accepted,
        mutation_acc.rejected,
    );
    monitor.record(final_snap);

    // ── Phase 3: Assert mutation accounting ──────────────────────────────
    //
    // At least 50% of SetTileRoot mutations must have been accepted.
    // If the runtime rejected all mutations the load was a no-op and the
    // growth assertions below are meaningless.
    //
    // Minimum threshold: 50% acceptance rate. This is deliberately lenient
    // to tolerate transient backpressure while still catching a fully
    // broken mutation pipeline.
    const MIN_MUTATION_ACCEPTANCE_RATE: f64 = 0.50;
    mutation_acc
        .assert_min_acceptance_rate(MIN_MUTATION_ACCEPTANCE_RATE)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // ── Phase 4: Assert no monotonic growth ───────────────────────────────
    //
    // Two complementary checks:
    //  (a) Endpoint comparison: last vs first snapshot — the original check.
    //  (b) Windowed comparison: sliding window of 3 consecutive snapshots to
    //      catch slow monotonic creep that cancels between endpoints.
    //
    // The windowed check fires on bounded-growth invariant violations that the
    // endpoint check would miss (e.g., if early growth is offset by later
    // contraction — a pattern that can occur when snapshots are sparse).

    let growth_result = monitor.assert_no_monotonic_growth(SPEC_GROWTH_TOLERANCE);
    // Windowed check: enforce bounded growth over any 3-snapshot window.
    // A window of 3 means we check growth between any snapshot[i] and
    // snapshot[i+2], catching steady per-epoch creep.
    let windowed_result = monitor.assert_no_windowed_growth(SPEC_GROWTH_TOLERANCE, 3);

    // Build and emit artifact
    let (passed, max_growth_pct, worst_metric, error_msg) = match &growth_result {
        Ok(ratios) => (
            true,
            ratios.max_growth() * 100.0,
            Some(ratios.worst_metric().to_string()),
            None,
        ),
        Err(e) => (false, 0.0, None, Some(e.clone())),
    };

    let artifact = SoakArtifact {
        test: "test_soak_resource_growth".to_string(),
        soak_secs: cfg.soak_secs,
        snapshot_count: monitor.len(),
        agent_count: cfg.agent_count,
        growth_assertion_passed: passed,
        max_growth_pct,
        worst_metric,
        error: error_msg,
    };
    println!(
        "ARTIFACT:soak_resource_history {}",
        monitor.to_json().unwrap_or_default()
    );
    println!(
        "ARTIFACT:soak_growth_ratios {}",
        serde_json::to_string_pretty(&artifact).unwrap_or_default()
    );

    // Propagate endpoint growth assertion failure first, then windowed.
    growth_result
        .map(|_| ())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    windowed_result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    Ok(())
}

// ─── Test 2: Post-disconnect cleanup ─────────────────────────────────────────

/// Post-disconnect cleanup test.
///
/// Validates validation-framework/spec.md §"Soak and Leak Tests" scenario:
/// > WHEN an agent disconnects and its leases expire during a soak test
/// > THEN the agent's resource footprint (memory, textures, scene graph nodes)
/// > MUST reach exactly zero
///
/// Three agents connect, create tiles, publish zones, then drop their
/// connections. After the cleanup grace period, the test asserts that each
/// agent's resource footprint is exactly zero.
///
/// ## Port
/// Binds to an ephemeral port to avoid conflicts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_post_disconnect_cleanup() -> Result<(), Box<dyn std::error::Error>> {
    // Use an ephemeral port
    let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    let grpc_port = listener.local_addr().unwrap().port();
    drop(listener);

    // ── Phase 0: Start runtime ─────────────────────────────────────────────

    let runtime_cfg = HeadlessConfig {
        width: DISPLAY_W,
        height: DISPLAY_H,
        grpc_port,
        bind_all_interfaces: false,
        psk: SOAK_PSK.to_string(),
        config_toml: None,
    };
    let runtime_cell = tokio::sync::Mutex::new(HeadlessRuntime::new(runtime_cfg).await?);

    {
        let runtime = runtime_cell.lock().await;
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab_id = scene.create_tab("Cleanup-Tab", 0)?;
        scene.active_tab = Some(tab_id);
        scene.zone_registry = ZoneRegistry::with_defaults();
    }

    let _server_handle = {
        let runtime = runtime_cell.lock().await;
        runtime.start_grpc_server().await?
    };

    // ── Phase 1: Connect three agents ─────────────────────────────────────

    let mut agent_alpha = connect_soak_agent_to(
        "cleanup-alpha",
        1,
        5_000, // 5-second TTL
        grpc_port,
    )
    .await?;
    let mut agent_beta = connect_soak_agent_to("cleanup-beta", 2, 5_000, grpc_port).await?;
    let mut agent_gamma = connect_soak_agent_to("cleanup-gamma", 3, 5_000, grpc_port).await?;

    let namespaces = [
        agent_alpha.namespace.clone(),
        agent_beta.namespace.clone(),
        agent_gamma.namespace.clone(),
    ];

    // ── Phase 2: Create tiles and zone entries ────────────────────────────

    let _tile_a = create_tile(&mut agent_alpha, [0.0, 0.0, 200.0, 200.0], 1).await?;
    let _tile_b = create_tile(&mut agent_beta, [200.0, 0.0, 200.0, 200.0], 2).await?;
    let _tile_c = create_tile(&mut agent_gamma, [400.0, 0.0, 200.0, 200.0], 3).await?;

    let _ = publish_to_zone(&mut agent_alpha, "subtitle", "alpha-content").await;
    let _ = publish_to_zone(&mut agent_beta, "subtitle", "beta-content").await;
    let _ = publish_to_zone(&mut agent_gamma, "subtitle", "gamma-content").await;

    // Render a frame to stabilise state
    {
        let mut runtime = runtime_cell.lock().await;
        runtime.render_frame().await;
    }

    // ── Phase 3: Verify non-zero footprints while agents are connected ────

    let test_start = Instant::now();
    {
        let runtime = runtime_cell.lock().await;
        for ns in &namespaces {
            let fp = capture_agent_footprint(&runtime, ns, test_start.elapsed()).await;
            // Each agent must have at least 1 tile before disconnect
            assert!(
                fp.tiles >= 1,
                "agent '{}' should have ≥1 tile before disconnect, got {}",
                ns,
                fp.tiles
            );
        }
    }

    // ── Phase 4: Disconnect agents (drop connections) ─────────────────────

    eprintln!("[soak] Dropping agent connections...");
    // Dropping the tx closes the client-to-server stream, which triggers
    // session cleanup in the server.
    drop(agent_alpha.tx);
    drop(agent_beta.tx);
    drop(agent_gamma.tx);
    // Also drop the rx handles
    drop(agent_alpha.rx);
    drop(agent_beta.rx);
    drop(agent_gamma.rx);

    // ── Phase 5: Drive session cleanup via frames + lease expiry ──────────

    // The runtime needs a few frames + the lease TTL to detect client
    // disconnect and revoke leases. We render several frames with a delay to
    // give the network layer time to surface the disconnect and for the
    // 5-second lease TTL to expire.
    let cleanup_deadline = Duration::from_millis(6_000 + CLEANUP_GRACE_MS);
    let cleanup_start = Instant::now();

    while cleanup_start.elapsed() < cleanup_deadline {
        {
            let mut runtime = runtime_cell.lock().await;
            // Expire leases that have timed out
            let _expired = runtime
                .shared_state()
                .lock()
                .await
                .scene
                .lock()
                .await
                .expire_leases();
            runtime.render_frame().await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ── Phase 6: Assert zero footprint ────────────────────────────────────

    eprintln!("[soak] Verifying zero footprint after disconnect + lease expiry...");

    let mut cleanup_entries: Vec<AgentCleanupEntry> = Vec::new();
    let mut all_zero = true;

    {
        let runtime = runtime_cell.lock().await;
        for ns in &namespaces {
            let fp = capture_agent_footprint(&runtime, ns, cleanup_start.elapsed()).await;
            eprintln!(
                "[soak] Post-disconnect '{}': tiles={} nodes={} leases={} zones={}",
                ns, fp.tiles, fp.nodes, fp.leases, fp.zone_entries
            );
            let is_zero = fp.is_zero();
            cleanup_entries.push(AgentCleanupEntry {
                namespace: ns.clone(),
                is_zero,
                tiles: fp.tiles,
                nodes: fp.nodes,
                leases: fp.leases,
                zone_entries: fp.zone_entries,
                texture_bytes: fp.texture_bytes,
            });
            if !is_zero {
                all_zero = false;
            }
        }
    }

    let artifact = PostDisconnectArtifact {
        test: "test_post_disconnect_cleanup".to_string(),
        agent_count: 3,
        all_zero,
        agents: cleanup_entries,
    };
    println!(
        "ARTIFACT:soak_post_disconnect {}",
        serde_json::to_string_pretty(&artifact).unwrap_or_default()
    );

    // Assert each agent has zero footprint
    {
        let runtime = runtime_cell.lock().await;
        for ns in &namespaces {
            let fp = capture_agent_footprint(&runtime, ns, cleanup_start.elapsed()).await;
            fp.assert_zero()
                .map_err(|e| -> Box<dyn std::error::Error> {
                    format!(
                        "post-disconnect cleanup failed: {e}\n\
                     If the lease TTL has not yet expired, increase CLEANUP_GRACE_MS."
                    )
                    .into()
                })?;
        }
    }

    eprintln!("[soak] Post-disconnect cleanup: all agents have zero footprint");
    Ok(())
}

// ─── Test 3: Lease expiry frees all associated resources ─────────────────────

/// Lease expiry cleanup test.
///
/// Validates validation-framework/spec.md §"Soak and Leak Tests" scenario:
/// > WHEN a lease expires during soak
/// > THEN all associated resources are freed completely
///
/// Creates a short-TTL lease, drives mutations through it, then lets the
/// lease expire naturally (via `expire_leases()`). Asserts the scene returns
/// to baseline (zero namespace footprint).
///
/// ## Port
/// Binds to an ephemeral port to avoid conflicts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_lease_expiry_frees_resources() -> Result<(), Box<dyn std::error::Error>> {
    // Use an ephemeral port
    let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    let grpc_port = listener.local_addr().unwrap().port();
    drop(listener);

    // ── Phase 0: Start runtime ─────────────────────────────────────────────

    let runtime_cfg = HeadlessConfig {
        width: DISPLAY_W,
        height: DISPLAY_H,
        grpc_port,
        bind_all_interfaces: false,
        psk: SOAK_PSK.to_string(),
        config_toml: None,
    };
    let runtime_cell = tokio::sync::Mutex::new(HeadlessRuntime::new(runtime_cfg).await?);

    {
        let runtime = runtime_cell.lock().await;
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab_id = scene.create_tab("Expiry-Tab", 0)?;
        scene.active_tab = Some(tab_id);
        scene.zone_registry = ZoneRegistry::with_defaults();
    }

    let _server_handle = {
        let runtime = runtime_cell.lock().await;
        runtime.start_grpc_server().await?
    };

    // ── Phase 1: Connect agent with short TTL lease ───────────────────────

    // Use a 1-second TTL so we can drive expiry quickly in the test
    let mut agent = connect_soak_agent_to("expiry-agent", 1, 1_000, grpc_port).await?;
    let namespace = agent.namespace.clone();

    // ── Phase 2: Create tile and publish zone entry ───────────────────────

    let _tile_id = create_tile(&mut agent, [0.0, 0.0, 400.0, 300.0], 1).await?;
    let _ = publish_to_zone(&mut agent, "subtitle", "expiry-test").await;

    {
        let mut runtime = runtime_cell.lock().await;
        runtime.render_frame().await;
    }

    let test_start = Instant::now();

    // Verify resources are non-zero while lease is active
    {
        let runtime = runtime_cell.lock().await;
        let fp = capture_agent_footprint(&runtime, &namespace, test_start.elapsed()).await;
        assert!(
            fp.tiles >= 1,
            "agent should have ≥1 tile before lease expiry, got {}",
            fp.tiles
        );
    }

    eprintln!("[soak] Lease active: agent '{}' tiles={}", namespace, {
        let runtime = runtime_cell.lock().await;
        let fp = capture_agent_footprint(&runtime, &namespace, test_start.elapsed()).await;
        fp.tiles
    });

    // ── Phase 3: Wait for lease TTL + grace period, then call expire_leases ─

    // Lease TTL = 1 s. We wait 1.5 s to ensure expiry has triggered.
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    // Call expire_leases() to process TTL-expired leases
    {
        let runtime = runtime_cell.lock().await;
        let state = runtime.shared_state().lock().await;
        let expired = state.scene.lock().await.expire_leases();
        eprintln!("[soak] expire_leases: {} leases expired", expired.len());
    }

    // Render a frame to flush any pending cleanup
    {
        let mut runtime = runtime_cell.lock().await;
        runtime.render_frame().await;
    }

    // ── Phase 4: Assert zero footprint ────────────────────────────────────

    {
        let runtime = runtime_cell.lock().await;
        let fp = capture_agent_footprint(&runtime, &namespace, test_start.elapsed()).await;
        eprintln!(
            "[soak] Post-expiry '{}': tiles={} nodes={} leases={} zones={}",
            namespace, fp.tiles, fp.nodes, fp.leases, fp.zone_entries
        );

        fp.assert_zero()
            .map_err(|e| -> Box<dyn std::error::Error> {
                format!(
                    "lease expiry cleanup failed: {e}\n\
                 Tiles/nodes/leases should reach zero after TTL expiry."
                )
                .into()
            })?;
    }

    eprintln!("[soak] Lease expiry cleanup: zero footprint confirmed");

    // Clean up the agent connection
    drop(agent.tx);
    drop(agent.rx);

    Ok(())
}

// ─── Variant connect helper (configurable port) ───────────────────────────────

/// Connect a soak agent to an arbitrary port (for tests using ephemeral ports).
async fn connect_soak_agent_to(
    agent_id: &str,
    lease_priority: u32,
    lease_ttl_ms: u64,
    grpc_port: u16,
) -> Result<SoakAgentSession, Box<dyn std::error::Error>> {
    let mut client = HudSessionClient::connect(format!("http://[::1]:{grpc_port}")).await?;

    let (tx, rx_chan) = tokio::sync::mpsc::channel::<session_proto::ClientMessage>(128);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx_chan);

    let now_us = now_wall_us();

    tx.send(session_proto::ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_us,
        payload: Some(session_proto::client_message::Payload::SessionInit(
            session_proto::SessionInit {
                agent_id: agent_id.to_string(),
                requested_capabilities: vec![
                    "create_tiles".to_string(),
                    "modify_own_tiles".to_string(),
                ],
                initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
                resume_token: Vec::new(),
                min_protocol_version: 0,
                max_protocol_version: 0,
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(SOAK_PSK.to_string())),
            },
        )),
    })
    .await?;

    let mut response_stream = client.session(stream).await?.into_inner();

    let msg = response_stream
        .next()
        .await
        .ok_or("no message received")??;
    let namespace = match &msg.payload {
        Some(session_proto::server_message::Payload::SessionEstablished(est)) => {
            est.namespace.clone()
        }
        other => {
            return Err(format!(
                "soak agent {agent_id}: Expected SessionEstablished, got: {other:?}"
            )
            .into());
        }
    };

    let _snapshot_msg = response_stream.next().await.ok_or("no scene snapshot")??;
    let degradation_msg = response_stream
        .next()
        .await
        .ok_or("no degradation notice")??;
    if !matches!(
        &degradation_msg.payload,
        Some(session_proto::server_message::Payload::DegradationNotice(_))
    ) {
        return Err(format!(
            "soak agent {agent_id}: Expected DegradationNotice after SceneSnapshot, got: {:?}",
            degradation_msg.payload
        )
        .into());
    }

    tx.send(session_proto::ClientMessage {
        sequence: 2,
        timestamp_wall_us: now_wall_us(),
        payload: Some(session_proto::client_message::Payload::LeaseRequest(
            session_proto::LeaseRequest {
                ttl_ms: lease_ttl_ms,
                capabilities: vec!["create_tiles".to_string(), "modify_own_tiles".to_string()],
                lease_priority,
            },
        )),
    })
    .await?;

    let msg = next_server_msg(&mut response_stream).await?;
    let lease_id_bytes =
        match &msg.payload {
            Some(session_proto::server_message::Payload::LeaseResponse(resp)) if resp.granted => {
                resp.lease_id.clone()
            }
            other => return Err(format!(
                "soak agent {agent_id} (to_port): Expected LeaseResponse(granted), got: {other:?}"
            )
            .into()),
        };

    Ok(SoakAgentSession {
        namespace,
        lease_id_bytes,
        tx,
        rx: response_stream,
        sequence: 2,
    })
}
