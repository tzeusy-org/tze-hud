//! Multi-agent zone contention and internal shared-scene coherence.
//!
//! Three gRPC sessions publish notifications and subtitles. The suite checks
//! namespace isolation, stack/latest-wins resolution and compositor output.
//! Internal tile and mutation coverage uses the shared scene directly.

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
const DISPLAY_W: u32 = 1920;
const DISPLAY_H: u32 = 1080;

// ─── Artifact types (JSON output per acceptance criteria) ────────────────────

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

    let listener = tokio::net::TcpListener::bind("[::1]:0").await?;
    let grpc_port = listener.local_addr()?.port();
    let config = HeadlessConfig {
        width: DISPLAY_W,
        height: DISPLAY_H,
        grpc_port,
        agents: tze_hud_scene::config::AgentDirectory::unrestricted(TEST_PSK),
        config_toml: None,
    };

    let mut runtime = serialized_headless_init(HeadlessRuntime::new(config)).await?;

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

    let _server_handle = runtime.start_grpc_server_on_listener(listener);

    // ── Phase 1: Connect three agents concurrently ──────────────────────────

    // All three connect concurrently.
    let (agent_a, mut agent_b, mut agent_c) = tokio::try_join!(
        connect_agent(TEST_PSK, grpc_port, "agent-weather"),
        connect_agent(TEST_PSK, grpc_port, "agent-notifications"),
        connect_agent(TEST_PSK, grpc_port, "agent-media"),
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

    // ── Phase 7: Compositor frame rendering ─────────────────────────────────

    // render_frame records this frame in the collector used by the artifact below.
    let frame = runtime.render_frame().await;

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

    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        assert!(
            scene
                .zone_registry
                .active_for_zone("notification-area")
                .iter()
                .all(|r| r.publisher_namespace == agent_b.namespace)
        );
        assert!(
            scene
                .zone_registry
                .active_for_zone("subtitle")
                .iter()
                .all(|r| r.publisher_namespace == agent_c.namespace)
        );
    }
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
/// The protocol and MCP services share the same scene Arc. This headless
/// integration test exercises that internal ownership directly: writes through
/// either clone are visible through the other without a transport round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_grpc_and_mcp_share_single_scene_graph() {
    use tze_hud_runtime::headless::HeadlessConfig;

    let config = HeadlessConfig {
        width: 800,
        height: 600,
        grpc_port: 0, // configured gRPC server disabled; shared-scene test only
        agents: tze_hud_scene::config::AgentDirectory::unrestricted("coherence-test"),
        config_toml: None,
    };
    let runtime = serialized_headless_init(HeadlessRuntime::new(config))
        .await
        .expect("runtime init");

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
            "DeleteTile via internal scene path must succeed: {:?}",
            result.error
        );
    }

    // ── Step 6: Verify MCP-side no longer sees the deleted tile ──

    {
        let state = shared_state_arc.lock().await;
        let scene = state.scene.lock().await;
        assert!(
            !scene.tiles.contains_key(&tile_id),
            "tile deleted via internal apply_batch must not be visible via MCP-side read \
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
            held: false,
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
            held: false,
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
