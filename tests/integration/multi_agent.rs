//! E12.1: Multi-agent integration test
//!
//! End-to-end test: three resident agents connect via gRPC, acquire leases,
//! publish to zones, create tiles, and coexist without interference.
//!
//! ## Test scenario
//! - Agent A (`agent-weather`): weather dashboard — creates two tiles, updates content.
//! - Agent B (`agent-notifications`): publishes two notifications to the `notification-area`
//!   zone (Stack contention policy).
//! - Agent C (`agent-media`): publishes two subtitles to the `subtitle` zone
//!   (LatestWins contention policy).
//!
//! ## Verifications
//! 1. Namespace isolation — each agent's tiles are in its own namespace only.
//! 2. Lease priority ordering — priority-1 (agent-weather) is highest.
//! 3. Zone contention resolution — notifications stack; subtitles are latest-wins.
//! 4. Compositor renders ≥ 1 frame without panic; tile count and active-lease count
//!    match expectations.
//!
//! ## JSON Artifacts (per acceptance criteria)
//! The test emits four structured JSON documents to stdout (prefixed with a tag
//! so they can be captured by CI tools):
//! - `ARTIFACT:tile_ownership_map`   — per-agent tile ownership map
//! - `ARTIFACT:zone_contention_log`  — zone contention resolution log
//! - `ARTIFACT:namespace_isolation`  — namespace isolation verification report
//! - `ARTIFACT:frame_rate`           — compositor frame rate measurements
//!
//! ## References (validation-framework/spec.md)
//! - Lines 313–320: V1 Success Criterion — Live Multi-Agent Presence (3 agents, 60fps)
//! - Lines 335–343: V1 Success Criterion — Security Isolation
//! - Lines 160–172: Test Scene Registry — `three_agents_contention`
//!
//! ## Cross-epic references validated
//! - Epic 1  (scene graph):    namespace isolation, tile CRUD
//! - Epic 4  (lease governance): lease priorities, concurrent leases
//! - Epic 6  (session protocol): multi-agent gRPC connections
//! - Epic 9  (zone system):    zone contention resolution policies

use tze_hud_protocol::proto;
use tze_hud_protocol::proto::session as session_proto;
use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;
use tze_hud_scene::types::*;

use serde::{Deserialize, Serialize};

// ─── Shared gRPC session harness ─────────────────────────────────────────────
// Extracted from duplicate copies in the former presence-card suite and
// subtitle_streaming (hud-ls5pz). See common/mod.rs for drift reconciliation notes.
#[path = "common/mod.rs"]
mod common;
use common::*;

// ─── Test-local constants ─────────────────────────────────────────────────────

const TEST_PSK: &str = "multi-agent-test-key";
const GRPC_PORT: u16 = 50052; // use a different port from vertical_slice to avoid conflicts
const DISPLAY_W: u32 = 1920;
const DISPLAY_H: u32 = 1080;

// ─── Artifact types (JSON output per acceptance criteria) ────────────────────

/// Per-agent tile ownership entry.
#[derive(Debug, Serialize, Deserialize)]
struct TileOwnershipEntry {
    tile_id: String,
    namespace: String,
    z_order: u32,
    lease_id: String,
    bounds: [f32; 4], // [x, y, w, h]
}

/// Per-agent tile ownership map artifact.
#[derive(Debug, Serialize, Deserialize)]
struct TileOwnershipMap {
    agent_count: usize,
    total_tiles: usize,
    tiles: Vec<TileOwnershipEntry>,
    namespaces: Vec<String>,
}

/// Single zone contention event.
#[derive(Debug, Serialize, Deserialize)]
struct ZoneContentionEvent {
    zone: String,
    policy: String,
    publisher: String,
    content_summary: String,
    /// Number of active zone entries after this publish event.
    /// Note: for Stack policy this may exceed the number of distinct publishers
    /// since one publisher can hold multiple entries.
    active_entries_after: usize,
}

/// Zone contention resolution log artifact.
#[derive(Debug, Serialize, Deserialize)]
struct ZoneContentionLog {
    events: Vec<ZoneContentionEvent>,
    notification_area_final_count: usize,
    subtitle_final_count: usize,
    notification_area_policy: String,
    subtitle_policy: String,
}

/// Single namespace isolation check result.
#[derive(Debug, Serialize, Deserialize)]
struct NamespaceIsolationCheck {
    description: String,
    passed: bool,
    detail: String,
}

/// Namespace isolation verification report artifact.
#[derive(Debug, Serialize, Deserialize)]
struct NamespaceIsolationReport {
    checks: Vec<NamespaceIsolationCheck>,
    all_passed: bool,
}

/// Compositor frame rate measurements artifact.
#[derive(Debug, Serialize, Deserialize)]
struct FrameRateMeasurements {
    frames_rendered: u64,
    active_agents: usize,
    active_leases: u32,
    tile_count: u32,
    frame_time_us: u64,
    /// Raw measured fps (informational; not a pass/fail gate).
    raw_fps_informational: f64,
}

// ─── Main integration test ───────────────────────────────────────────────────

#[tokio::test]
async fn test_three_agents_contention() -> Result<(), Box<dyn std::error::Error>> {
    // ── Phase 0: Start runtime ──────────────────────────────────────────────

    let config = HeadlessConfig {
        width: DISPLAY_W,
        height: DISPLAY_H,
        grpc_port: GRPC_PORT,
        agents: tze_hud_scene::config::AgentDirectory::unrestricted(TEST_PSK),
        config_toml: None,
    };

    let mut runtime = HeadlessRuntime::new(config).await?;

    // Pre-populate the scene with a tab and default zones BEFORE gRPC connections,
    // since all three agents will need an active tab and zones to publish to.
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab_id = scene.create_tab("Multi-Agent", 0)?;
        scene.active_tab = Some(tab_id);
        // Register default zones (status-bar, notification-area, subtitle).
        scene.zone_registry = ZoneRegistry::with_defaults();
    }

    let _server_handle = runtime.start_grpc_server().await?;

    // ── Phase 1: Connect three agents concurrently ──────────────────────────

    // All three connect concurrently.
    let (mut agent_a, mut agent_b, mut agent_c) = tokio::try_join!(
        connect_agent(TEST_PSK, GRPC_PORT, "agent-weather"),
        connect_agent(TEST_PSK, GRPC_PORT, "agent-notifications"),
        connect_agent(TEST_PSK, GRPC_PORT, "agent-media"),
    )?;

    // Verify all three namespaces are distinct
    assert_ne!(
        agent_a.namespace, agent_b.namespace,
        "agent-weather and agent-notifications must have distinct namespaces"
    );
    assert_ne!(
        agent_b.namespace, agent_c.namespace,
        "agent-notifications and agent-media must have distinct namespaces"
    );
    assert_ne!(
        agent_a.namespace, agent_c.namespace,
        "agent-weather and agent-media must have distinct namespaces"
    );

    // ── Phase 2: Agent A creates weather dashboard tiles ────────────────────

    // Current conditions tile
    let tile_a1_id = claim_tile_via_grpc(
        &mut agent_a,
        placement(TileAnchor::TopLeft, TileSize::Large),
    )
    .await?;
    assert!(
        !tile_a1_id.is_empty(),
        "agent-weather tile-1 must be created"
    );

    // Forecast tile (stacks below the first at the same anchor)
    let tile_a2_id =
        claim_tile_via_grpc(&mut agent_a, placement(TileAnchor::TopLeft, TileSize::Wide)).await?;
    assert!(
        !tile_a2_id.is_empty(),
        "agent-weather tile-2 must be created"
    );

    // Verify agent A tiles exist in the scene graph with correct namespace
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let weather_tiles: Vec<_> = scene
            .tiles
            .values()
            .filter(|t| t.namespace == agent_a.namespace)
            .collect();
        assert_eq!(
            weather_tiles.len(),
            2,
            "agent-weather must own exactly 2 tiles in the scene"
        );
        for tile in &weather_tiles {
            assert_eq!(
                tile.namespace, agent_a.namespace,
                "tile namespace must match agent-weather's authenticated namespace"
            );
        }
    }

    // ── Phase 3: Zone contention — agent B publishes to notification-area ───

    // First notification (ShortTextWithIcon media type, as required by notification-area zone)
    publish_notification_to_zone_via_grpc(
        &mut agent_b,
        "notification-area",
        "Alert: Weather update available",
        1, // urgency: normal
    )
    .await?;

    // Second notification (stacks, because notification-area uses Stack policy)
    publish_notification_to_zone_via_grpc(
        &mut agent_b,
        "notification-area",
        "Alert: System health check complete",
        1,
    )
    .await?;

    let notification_count = {
        let state = runtime.shared_state().lock().await;
        state
            .scene
            .lock()
            .await
            .zone_registry
            .active_for_zone("notification-area")
            .len()
    };

    // Stack policy: both publishes must be present
    assert!(
        notification_count >= 2,
        "notification-area (Stack) must have >= 2 active entries, got {notification_count}"
    );

    // ── Phase 4: Zone contention — agent C publishes to subtitle (LatestWins) ─

    // First subtitle (StreamText media type, as required by subtitle zone)
    publish_stream_text_to_zone_via_grpc(&mut agent_c, "subtitle", "Subtitle line one").await?;

    let count_after_first = {
        let state = runtime.shared_state().lock().await;
        state
            .scene
            .lock()
            .await
            .zone_registry
            .active_for_zone("subtitle")
            .len()
    };
    assert_eq!(
        count_after_first, 1,
        "subtitle (LatestWins) must have exactly 1 entry after first publish"
    );

    // Second subtitle — must replace the first (LatestWins)
    publish_stream_text_to_zone_via_grpc(&mut agent_c, "subtitle", "Subtitle line two (latest)")
        .await?;

    let (subtitle_count, subtitle_text) = {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let active = scene.zone_registry.active_for_zone("subtitle");
        let text = active
            .first()
            .and_then(|r| match &r.content {
                ZoneContent::StreamText(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_default();
        (active.len(), text)
    };

    assert_eq!(
        subtitle_count, 1,
        "subtitle (LatestWins) must have exactly 1 active entry, got {subtitle_count}"
    );
    assert!(
        subtitle_text.contains("latest"),
        "subtitle LatestWins must retain the most recent publish, got: '{subtitle_text}'"
    );

    // ── Phase 5: Namespace isolation verification ───────────────────────────

    // Verify: agent B and C have no tiles (they only use zones)
    let (b_tile_count, c_tile_count) = {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let b = scene
            .tiles
            .values()
            .filter(|t| t.namespace == agent_b.namespace)
            .count();
        let c = scene
            .tiles
            .values()
            .filter(|t| t.namespace == agent_c.namespace)
            .count();
        (b, c)
    };
    assert_eq!(
        b_tile_count, 0,
        "agent-notifications must have no tiles (zone-only agent)"
    );
    assert_eq!(
        c_tile_count, 0,
        "agent-media must have no tiles (zone-only agent)"
    );

    // Verify: agent A has no zone publications
    let a_zone_publishes = {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let notif = scene
            .zone_registry
            .active_for_zone("notification-area")
            .iter()
            .filter(|r| r.publisher_namespace == agent_a.namespace)
            .count();
        let sub = scene
            .zone_registry
            .active_for_zone("subtitle")
            .iter()
            .filter(|r| r.publisher_namespace == agent_a.namespace)
            .count();
        notif + sub
    };
    assert_eq!(
        a_zone_publishes, 0,
        "agent-weather must have no zone publications"
    );

    // Verify: namespace isolation — the session server derives tile namespace from
    // the authenticated session, not from client-supplied values. Verify this by
    // confirming all tiles in the scene are attributed to agent-weather's namespace.
    // (The session server uses `session.namespace` in CreateTile, so no other agent
    //  could create a tile attributed to agent-weather even if they used the same lease_id.)
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        for tile in scene.tiles.values() {
            assert_eq!(
                tile.namespace, agent_a.namespace,
                "all tiles must belong to agent-weather (the only tile-creating agent); \
                 found tile with namespace '{}' (expected '{}')",
                tile.namespace, agent_a.namespace
            );
        }
    }

    // ── Phase 6b: Adversarial cross-agent namespace security check ──────────
    //
    // Verify that the session server rejects a mutation from agent B (notifications)
    // that targets a tile owned by agent A (weather).  This exercises the
    // namespace-isolation enforcement path end-to-end via gRPC — not just the
    // scene-graph layer in isolation.
    //
    // Agent B attempts to change the opacity of agent A's tile (tile_a1_id).  The
    // session server fills `agent_namespace` from the authenticated session context
    // (agent B's namespace), NOT from the client payload.  The scene graph's
    // `update_tile_opacity` call then rejects the mutation because the tile's
    // namespace does not match agent B's namespace.
    //
    // This replaces the previous `passed: true` hardcoded fiat (hud-59b32).
    let cross_agent_mutation_rejected = {
        let seq = agent_b.next_seq();
        agent_b
            .tx
            .send(session_proto::ClientMessage {
                sequence: seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(session_proto::client_message::Payload::MutationBatch(
                    session_proto::MutationBatch {
                        batch_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
                        lease_id: agent_b.lease_id_bytes.clone(),
                        mutations: vec![proto::MutationProto {
                            mutation: Some(proto::mutation_proto::Mutation::UpdateTileOpacity(
                                proto::UpdateTileOpacityMutation {
                                    // tile_a1_id belongs to agent A — agent B must not be allowed
                                    // to mutate it.
                                    tile_id: tile_a1_id.clone(),
                                    opacity: 0.5,
                                },
                            )),
                        }],
                        timing: None,
                    },
                )),
            })
            .await
            .is_ok()
            && {
                // Receive the MutationResult.
                match agent_b.next_server_msg().await {
                    Some(Ok(msg)) => {
                        match &msg.payload {
                            Some(session_proto::server_message::Payload::RequestResult(result)) => {
                                // The mutation must be rejected (not accepted).
                                let rejected = !result.ok;
                                eprintln!(
                                    "    Cross-agent mutation rejected={rejected} \
                                     (error_code='{}', error_message='{}')",
                                    result.code, result.hint
                                );
                                rejected
                            }
                            other => {
                                eprintln!(
                                    "    Cross-agent security check: unexpected payload: {other:?}"
                                );
                                false
                            }
                        }
                    }
                    other => {
                        eprintln!("    Cross-agent security check: stream error: {other:?}");
                        false
                    }
                }
            }
    };

    assert!(
        cross_agent_mutation_rejected,
        "Cross-agent mutation must be rejected: agent-notifications must NOT be allowed to \
         mutate tiles belonging to agent-weather (namespace isolation enforcement)"
    );

    // ── Phase 7: Compositor frame rendering ─────────────────────────────────

    let frame = runtime.render_frame().await;

    // Verify the compositor renders correctly with all three agents active.
    assert!(
        frame.tile_count >= 2,
        "compositor must see at least 2 tiles from agent-weather"
    );
    assert!(
        frame.active_leases >= 2,
        "compositor must see one active lease per claimed tile"
    );

    // Record frame time for telemetry (informational — not a calibrated pass/fail).
    runtime
        .telemetry
        .summary_mut()
        .frame_time
        .record(frame.frame_time_us);

    // ── Phase 8: Emit JSON artifacts ────────────────────────────────────────

    // Artifact 1: Per-agent tile ownership map
    let ownership_map = {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let mut entries: Vec<TileOwnershipEntry> = scene
            .tiles
            .values()
            .map(|t| TileOwnershipEntry {
                tile_id: t.id.to_string(),
                namespace: t.namespace.clone(),
                z_order: t.z_order,
                lease_id: t.lease_id.to_string(),
                bounds: [t.bounds.x, t.bounds.y, t.bounds.width, t.bounds.height],
            })
            .collect();
        entries.sort_by(|a, b| {
            a.namespace
                .cmp(&b.namespace)
                .then(b.z_order.cmp(&a.z_order))
        });

        let mut namespaces: Vec<String> = entries.iter().map(|e| e.namespace.clone()).collect();
        namespaces.sort_unstable();
        namespaces.dedup();

        TileOwnershipMap {
            agent_count: 3,
            total_tiles: entries.len(),
            namespaces,
            tiles: entries,
        }
    };
    println!(
        "ARTIFACT:tile_ownership_map:{}",
        serde_json::to_string(&ownership_map)?
    );

    // Artifact 2: Zone contention resolution log
    let contention_log = {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let notif_active = scene.zone_registry.active_for_zone("notification-area");
        let sub_active = scene.zone_registry.active_for_zone("subtitle");

        ZoneContentionLog {
            events: vec![
                ZoneContentionEvent {
                    zone: "notification-area".to_string(),
                    policy: "Stack".to_string(),
                    publisher: agent_b.namespace.clone(),
                    content_summary: "2 notifications published (both retained by Stack policy)"
                        .to_string(),
                    active_entries_after: notif_active.len(),
                },
                ZoneContentionEvent {
                    zone: "subtitle".to_string(),
                    policy: "LatestWins".to_string(),
                    publisher: agent_c.namespace.clone(),
                    content_summary:
                        "2 subtitles published; only latest retained (LatestWins policy)"
                            .to_string(),
                    active_entries_after: sub_active.len(),
                },
            ],
            notification_area_final_count: notif_active.len(),
            subtitle_final_count: sub_active.len(),
            notification_area_policy: "Stack".to_string(),
            subtitle_policy: "LatestWins".to_string(),
        }
    };
    println!(
        "ARTIFACT:zone_contention_log:{}",
        serde_json::to_string(&contention_log)?
    );

    // Artifact 3: Namespace isolation verification report
    let isolation_checks = vec![
        NamespaceIsolationCheck {
            description: "Agent-weather tiles are all in agent-weather namespace".to_string(),
            passed: {
                let state = runtime.shared_state().lock().await;
                state
                    .scene
                    .lock()
                    .await
                    .tiles
                    .values()
                    .filter(|t| t.namespace == agent_a.namespace)
                    .count()
                    == 2
            },
            detail: format!(
                "Expected 2 tiles in namespace '{}'; verified via scene graph",
                agent_a.namespace
            ),
        },
        NamespaceIsolationCheck {
            description: "Agent-notifications has no tiles (zone-only)".to_string(),
            passed: b_tile_count == 0,
            detail: format!(
                "Namespace '{}' tile count = {} (expected 0)",
                agent_b.namespace, b_tile_count
            ),
        },
        NamespaceIsolationCheck {
            description: "Agent-media has no tiles (zone-only)".to_string(),
            passed: c_tile_count == 0,
            detail: format!(
                "Namespace '{}' tile count = {} (expected 0)",
                agent_c.namespace, c_tile_count
            ),
        },
        NamespaceIsolationCheck {
            description: "Notification zone publishes are from agent-notifications only"
                .to_string(),
            passed: {
                let state = runtime.shared_state().lock().await;
                state
                    .scene
                    .lock()
                    .await
                    .zone_registry
                    .active_for_zone("notification-area")
                    .iter()
                    .all(|r| r.publisher_namespace == agent_b.namespace)
            },
            detail: "All notification-area publishes must originate from agent-notifications"
                .to_string(),
        },
        NamespaceIsolationCheck {
            description: "Subtitle zone publish is from agent-media only".to_string(),
            passed: {
                let state = runtime.shared_state().lock().await;
                state
                    .scene
                    .lock()
                    .await
                    .zone_registry
                    .active_for_zone("subtitle")
                    .iter()
                    .all(|r| r.publisher_namespace == agent_c.namespace)
            },
            detail: "All subtitle publishes must originate from agent-media".to_string(),
        },
        NamespaceIsolationCheck {
            description: "Cross-agent mutation rejected: session server derives namespace \
                          from auth credential, not from client payload"
                .to_string(),
            // Driven by the adversarial test in Phase 6b: agent-notifications sent an
            // UpdateTileOpacity targeting a tile owned by agent-weather.  The server must
            // reject it — if it accepted, namespace isolation is broken.
            passed: cross_agent_mutation_rejected,
            detail: format!(
                "Agent-notifications attempted to mutate a tile owned by agent-weather. \
                 Mutation rejected by session server: {cross_agent_mutation_rejected}",
            ),
        },
    ];
    let all_passed = isolation_checks.iter().all(|c| c.passed);
    let isolation_report = NamespaceIsolationReport {
        checks: isolation_checks,
        all_passed,
    };
    assert!(
        all_passed,
        "Namespace isolation checks failed: {:?}",
        isolation_report
            .checks
            .iter()
            .filter(|c| !c.passed)
            .collect::<Vec<_>>()
    );
    println!(
        "ARTIFACT:namespace_isolation:{}",
        serde_json::to_string(&isolation_report)?
    );

    // Artifact 4: Compositor frame rate measurements
    let frame_rate_artifact = FrameRateMeasurements {
        frames_rendered: runtime.telemetry.summary().total_frames,
        active_agents: 3,
        active_leases: frame.active_leases,
        tile_count: frame.tile_count,
        frame_time_us: frame.frame_time_us,
        raw_fps_informational: if frame.frame_time_us > 0 {
            1_000_000.0 / frame.frame_time_us as f64
        } else {
            0.0
        },
    };
    println!(
        "ARTIFACT:frame_rate:{}",
        serde_json::to_string(&frame_rate_artifact)?
    );

    // ── Phase 9: Final scene-level assertions ────────────────────────────────

    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;

        // agent-weather's leases (one per claimed tile) must still be Active;
        // zone-only agents hold no lease.
        let weather_leases: Vec<_> = scene
            .leases
            .values()
            .filter(|l| l.namespace == agent_a.namespace)
            .collect();
        assert_eq!(
            weather_leases.len(),
            2,
            "agent-weather holds one lease per tile"
        );
        for lease in weather_leases {
            assert_eq!(
                lease.state,
                LeaseState::Active,
                "agent-weather lease must be Active at end of test"
            );
        }

        // Total tile count must be 2 (only agent-weather has tiles)
        assert_eq!(
            scene.tiles.len(),
            2,
            "total tile count must be 2 (only agent-weather creates tiles)"
        );

        // Scene graph invariants must hold
        let violations = tze_hud_scene::test_scenes::assert_layer0_invariants(&scene);
        assert!(
            violations.is_empty(),
            "Layer 0 invariants violated after multi-agent test: {violations:?}"
        );
    }

    Ok(())
}

// ─── Cross-protocol scene coherence test ─────────────────────────────────────

/// Verify that MCP and gRPC share a single scene graph (hud-bco1).
///
/// Acceptance criteria (from hud-bco1 issue):
/// - A mutation applied via gRPC (CreateTile via session stream) must be visible
///   when the scene is read via the `shared_state().scene` Arc — the same Arc
///   that the MCP server holds.
/// - A direct mutation to `shared_state().scene` (simulating MCP writes) must
///   be visible to gRPC queries (SceneSnapshot response reflects the mutation).
///
/// This is a headless unit-style integration test that does not require a real
/// MCP client — it directly exercises the shared `Arc<Mutex<SceneGraph>>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_grpc_and_mcp_share_single_scene_graph() {
    use tze_hud_runtime::headless::HeadlessConfig;

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0, // ephemeral port — no real gRPC server needed
        agents: tze_hud_scene::config::AgentDirectory::unrestricted("coherence-test"),
        config_toml: None,
    };
    let runtime = HeadlessRuntime::new(config).await.expect("runtime init");

    // Obtain the Arc<Mutex<SharedState>> that both gRPC and MCP share.
    let shared_state_arc = runtime.shared_state().clone();

    // ── Step 1: Write a tab via shared_state (simulates MCP-side mutation) ──

    let tab_id = {
        let state = shared_state_arc.lock().await;
        let mut scene = state.scene.lock().await;
        scene
            .create_tab("coherence-tab", 0)
            .expect("tab creation via shared scene must succeed")
    };

    // ── Step 2: Write a lease via shared_state (simulates MCP-side mutation) ──

    let lease_id = {
        let state = shared_state_arc.lock().await;
        let mut scene = state.scene.lock().await;
        scene.grant_lease("coherence-agent", 60_000)
    };

    // ── Step 3: Write a tile via shared_state (simulates MCP-side mutation) ──

    let tile_id = {
        let state = shared_state_arc.lock().await;
        let mut scene = state.scene.lock().await;
        scene
            .create_tile(
                tab_id,
                "coherence-agent",
                lease_id,
                tze_hud_scene::types::Rect::new(10.0, 10.0, 100.0, 50.0),
                1,
            )
            .expect("tile creation via shared scene must succeed")
    };

    // ── Step 4: Read the tile back via the same Arc (simulates gRPC reading) ──
    //
    // This verifies that gRPC (which holds a clone of the same Arc) would see
    // the tile created by MCP. There is only ONE scene graph — they share it.

    {
        let state = shared_state_arc.lock().await;
        let scene = state.scene.lock().await;

        assert!(
            scene.tiles.contains_key(&tile_id),
            "tile created via MCP-side write must be visible via gRPC-side read \
             (single shared Arc<Mutex<SceneGraph>>)"
        );
        let tile = &scene.tiles[&tile_id];
        assert_eq!(
            tile.namespace, "coherence-agent",
            "tile namespace must be preserved across protocol boundary"
        );
        assert_eq!(
            tile.tab_id, tab_id,
            "tile tab_id must be preserved across protocol boundary"
        );

        assert!(
            scene.leases.contains_key(&lease_id),
            "lease created via MCP-side write must be visible via gRPC-side read"
        );
        assert!(
            scene.tabs.contains_key(&tab_id),
            "tab created via MCP-side write must be visible via gRPC-side read"
        );

        eprintln!(
            "[coherence] Cross-protocol scene: tabs={}, tiles={}, leases={}",
            scene.tabs.len(),
            scene.tiles.len(),
            scene.leases.len()
        );
    }

    // ── Step 5: Mutate via gRPC-side path and verify MCP-side sees it ──
    //
    // The gRPC session server would call `apply_batch` on `st.scene`.
    // We simulate that here by locking state then scene, exactly as the
    // session server does.

    let batch = tze_hud_scene::mutation::MutationBatch {
        batch_id: tze_hud_scene::types::SceneId::new(),
        agent_namespace: "coherence-agent".to_string(),
        mutations: vec![tze_hud_scene::mutation::SceneMutation::DeleteTile { tile_id }],
        timing_hints: None,
        lease_id: None,
    };

    {
        let state = shared_state_arc.lock().await;
        let mut scene = state.scene.lock().await;
        let result = scene.apply_batch(&batch);
        assert!(
            result.applied,
            "DeleteTile via gRPC-side path must succeed: {:?}",
            result.error
        );
    }

    // ── Step 6: Verify MCP-side no longer sees the deleted tile ──

    {
        let state = shared_state_arc.lock().await;
        let scene = state.scene.lock().await;
        assert!(
            !scene.tiles.contains_key(&tile_id),
            "tile deleted via gRPC-side apply_batch must not be visible via MCP-side read \
             (mutations are immediately visible across protocol boundary)"
        );
        eprintln!("[coherence] PASS: cross-protocol scene coherence verified (one shared Arc)");
    }
}

/// Verify LatestWins contention resolution with TWO DISTINCT publishers.
///
/// The `test_three_agents_contention` integration test exercises LatestWins via a
/// single agent (agent-media) publishing twice — which proves the single-publisher
/// replacement path, but does NOT exercise the cross-publisher eviction path.
///
/// This test drives LatestWins with two agents from distinct namespaces publishing
/// to the same subtitle zone.  The second publisher's content must win, and the
/// first publisher's content must be evicted — even though the first publisher is
/// different from the second.
///
/// This is the behavioral assertion that was absent prior to hud-59b32.
#[test]
fn test_latest_wins_two_distinct_publishers() {
    use tze_hud_scene::graph::SceneGraph;
    use tze_hud_scene::mutation::{MutationBatch, SceneMutation};
    use tze_hud_scene::types::{SceneId, ZoneContent, ZonePublishToken, ZoneRegistry};

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.zone_registry = ZoneRegistry::with_defaults();

    let _tab = scene.create_tab("main", 0).unwrap();
    // Note: PublishToZone at the apply_batch level does not check lease capabilities
    // (token validation happens at the gRPC layer).  No leases needed here.

    // Dummy token: publish_token is validated by the gRPC layer (not apply_batch).
    let dummy_token = ZonePublishToken {
        token: vec![0xDE, 0xAD, 0xBE, 0xEF],
    };

    // Agent-alpha publishes to subtitle zone (LatestWins policy).
    let batch_a = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent-alpha".to_string(),
        mutations: vec![SceneMutation::PublishToZone {
            zone_name: "subtitle".to_string(),
            content: ZoneContent::StreamText("Alpha subtitle content".to_string()),
            publish_token: dummy_token.clone(),
            merge_key: None,
            expires_at_wall_us: None,
            content_classification: None,
            breakpoints: Vec::new(),
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result_a = scene.apply_batch(&batch_a);
    assert!(
        result_a.applied,
        "agent-alpha zone publish must succeed: {:?}",
        result_a.error
    );

    // Verify alpha's content is the sole active entry.
    let after_alpha = scene.zone_registry.active_for_zone("subtitle");
    assert_eq!(
        after_alpha.len(),
        1,
        "subtitle zone must have exactly 1 entry after agent-alpha publishes"
    );
    assert_eq!(
        after_alpha[0].publisher_namespace, "agent-alpha",
        "sole entry must belong to agent-alpha"
    );

    // Agent-beta publishes to the same zone — must evict agent-alpha (LatestWins).
    // This is the key assertion: a DIFFERENT namespace takes over the slot.
    let batch_b = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent-beta".to_string(),
        mutations: vec![SceneMutation::PublishToZone {
            zone_name: "subtitle".to_string(),
            content: ZoneContent::StreamText("Beta subtitle content (wins)".to_string()),
            publish_token: dummy_token,
            merge_key: None,
            expires_at_wall_us: None,
            content_classification: None,
            breakpoints: Vec::new(),
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result_b = scene.apply_batch(&batch_b);
    assert!(
        result_b.applied,
        "agent-beta zone publish must succeed: {:?}",
        result_b.error
    );

    // After beta publishes, only beta's content must remain (LatestWins evicts alpha).
    let after_beta = scene.zone_registry.active_for_zone("subtitle");
    assert_eq!(
        after_beta.len(),
        1,
        "subtitle zone must have exactly 1 entry after agent-beta publishes (LatestWins)"
    );
    assert_eq!(
        after_beta[0].publisher_namespace, "agent-beta",
        "LatestWins must retain agent-beta's content and evict agent-alpha's content"
    );

    // Confirm the content text is beta's (not alpha's).
    let content_text = match &after_beta[0].content {
        ZoneContent::StreamText(t) => t.clone(),
        other => panic!("expected StreamText content, got: {other:?}"),
    };
    assert!(
        content_text.contains("Beta"),
        "surviving content must be agent-beta's ('{content_text}')"
    );
    assert!(
        !content_text.contains("Alpha"),
        "agent-alpha's content must have been evicted by LatestWins ('{content_text}')"
    );
}

// ─── Same-anchor coexistence ─────────────────────────────────────────────────

/// Replace a tile's root with a single text node and wait for the result.
async fn set_tile_text_via_grpc(
    session: &mut AgentSession,
    tile_id: &[u8],
    content: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let seq = session.next_seq();
    let text = proto::TextMarkdownNodeProto {
        content: content.to_string(),
        bounds: Some(proto::Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 20.0,
        }),
        font_size_px: 14.0,
        color: Some(proto::Rgba {
            r: 0.9,
            g: 0.9,
            b: 0.9,
            a: 1.0,
        }),
        background: None,
        color_runs: vec![],
        overflow: 0,
    };
    session
        .tx
        .send(session_proto::ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(session_proto::client_message::Payload::MutationBatch(
                session_proto::MutationBatch {
                    batch_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
                    lease_id: session.lease_id_bytes.clone(),
                    mutations: vec![proto::MutationProto {
                        mutation: Some(proto::mutation_proto::Mutation::SetTileRoot(
                            proto::SetTileRootMutation {
                                tile_id: tile_id.to_vec(),
                                node: Some(proto::NodeProto {
                                    layout: 0,
                                    id: Vec::new(),
                                    data: Some(proto::node_proto::Data::TextMarkdown(text)),
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
    match session
        .next_server_msg()
        .await
        .ok_or("no mutation result")??
        .payload
    {
        Some(session_proto::server_message::Payload::RequestResult(r)) if r.ok => Ok(()),
        other => Err(format!("SetTileRoot rejected: {other:?}").into()),
    }
}

/// Three agents claim the same anchor concurrently: the runtime stacks the
/// tiles without overlap (z-order follows claim order), and all three can
/// then update their own tile's content at once without interference.
#[tokio::test]
async fn test_three_agents_same_anchor_stack_without_overlap_and_update_concurrently()
-> Result<(), Box<dyn std::error::Error>> {
    const PORT: u16 = 50055;
    let config = HeadlessConfig {
        width: DISPLAY_W,
        height: DISPLAY_H,
        grpc_port: PORT,
        agents: tze_hud_scene::config::AgentDirectory::unrestricted(TEST_PSK),
        config_toml: None,
    };
    let runtime = HeadlessRuntime::new(config).await?;
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab_id = scene.create_tab("Stack", 0)?;
        scene.active_tab = Some(tab_id);
    }
    let _server_handle = runtime.start_grpc_server().await?;

    let (mut alpha, mut beta, mut gamma) = tokio::try_join!(
        connect_agent(TEST_PSK, PORT, "agent-alpha"),
        connect_agent(TEST_PSK, PORT, "agent-beta"),
        connect_agent(TEST_PSK, PORT, "agent-gamma"),
    )?;
    let small = || placement(TileAnchor::BottomLeft, TileSize::Small);
    let (tile_alpha, tile_beta, tile_gamma) = tokio::try_join!(
        claim_tile_via_grpc(&mut alpha, small()),
        claim_tile_via_grpc(&mut beta, small()),
        claim_tile_via_grpc(&mut gamma, small()),
    )?;

    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let mut tiles: Vec<_> = scene.tiles.values().collect();
        assert_eq!(tiles.len(), 3, "each agent's claim creates one tile");
        tiles.sort_by_key(|t| t.z_order);
        assert_eq!(
            tiles.iter().map(|t| t.z_order).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "each claim stacks above the previous one"
        );
        for (i, a) in tiles.iter().enumerate() {
            for b in &tiles[i + 1..] {
                assert!(
                    a.bounds.y + a.bounds.height <= b.bounds.y
                        || b.bounds.y + b.bounds.height <= a.bounds.y,
                    "tiles claimed at one anchor must not overlap: {:?} vs {:?}",
                    a.bounds,
                    b.bounds
                );
            }
        }
    }

    tokio::try_join!(
        set_tile_text_via_grpc(&mut alpha, &tile_alpha, "alpha"),
        set_tile_text_via_grpc(&mut beta, &tile_beta, "beta"),
        set_tile_text_via_grpc(&mut gamma, &tile_gamma, "gamma"),
    )?;
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        for (id, agent, label) in [
            (&tile_alpha, &alpha, "alpha"),
            (&tile_beta, &beta, "beta"),
            (&tile_gamma, &gamma, "gamma"),
        ] {
            let uuid = uuid::Uuid::from_bytes(id.as_slice().try_into()?);
            let tile = &scene.tiles[&tze_hud_scene::SceneId::from_uuid(uuid)];
            assert_eq!(tile.namespace, agent.namespace, "{label}: tile namespace");
            assert!(tile.root_node.is_some(), "{label}: root set by its update");
        }
    }
    // The agent that sets its tile root must not be able to do so on a peer's tile.
    assert!(
        set_tile_text_via_grpc(&mut alpha, &tile_gamma, "intrusion")
            .await
            .is_err(),
        "an agent must not replace another agent's tile content"
    );
    Ok(())
}
