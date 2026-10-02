//! Disconnect detection, orphan handling, and reconnect-within-grace tests.
//!
//! Tests the full disconnect-to-cleanup pipeline for presence card tiles as
//! specified in OpenSpec task 6 (Disconnect and Orphan Handling Test, hud-apoe.4).
//!
//! **Test coverage:**
//! 1. Heartbeat timeout (15s = 3 × 5s interval) → lease ACTIVE → ORPHANED
//! 2. Disconnection badge appears on orphaned tile (visual_hint = DisconnectionBadge)
//! 3. Tile frozen during ORPHANED state: mutations from disconnected agent rejected
//! 4. Grace period (30s): ORPHANED → EXPIRED after 30s, tile removed from scene graph
//! 5. Reconnect within grace period: ORPHANED → ACTIVE, badge clears, updates resume
//! 6. Multi-agent isolation: agents 0 and 1 unaffected throughout
//!
//! All tests run at the **scene graph level** using a deterministic `TestClock`
//! so there is no real-time waiting. Time advances by direct `clock.advance(ms)` calls.
//!
//! ## References
//! - openspec/changes/exemplar-presence-card/tasks.md §6
//! - openspec/changes/exemplar-presence-card/spec.md
//! - crates/tze_hud_scene/src/lease/orphan.rs (GracePeriodTimer, TileVisualHint)
//! - crates/tze_hud_scene/src/graph.rs (disconnect_lease, reconnect_lease, expire_leases)

use std::sync::Arc;

use tze_hud_scene::{
    Clock, SceneGraph, SceneId, TestClock,
    lease::{LeaseState, ORPHAN_GRACE_PERIOD_MS, TileVisualHint},
    mutation::{MutationBatch, SceneMutation},
    types::{InputMode, Node, NodeData, Rect, Rgba, SolidColorNode},
};

// ─── Display and card constants (mirroring exemplar-presence-card spec) ──────

const DISPLAY_W: f32 = 1920.0;
const DISPLAY_H: f32 = 1080.0;
const CARD_W: f32 = 320.0;
const CARD_H: f32 = 112.0;
const BOTTOM_MARGIN: f32 = 24.0;
const LEFT_MARGIN: f32 = 24.0;
const CARD_GAP: f32 = 12.0;
const Z_ORDER_BASE: u32 = 100;

/// Heartbeat interval per spec (session-protocol/spec.md §1.1).
const HEARTBEAT_INTERVAL_MS: u64 = 5_000;

/// Orphan detection threshold: 3 missed heartbeats (lease-governance/spec.md lines 132-155).
const HEARTBEAT_MISSED_THRESHOLD: u64 = 3;

/// Heartbeat timeout that triggers ORPHANED: 3 × 5000ms = 15000ms.
const HEARTBEAT_TIMEOUT_MS: u64 = HEARTBEAT_INTERVAL_MS * HEARTBEAT_MISSED_THRESHOLD;

/// Reconnect grace period (ms) — imported from library to stay in sync with the implementation.
const GRACE_PERIOD_MS: u64 = ORPHAN_GRACE_PERIOD_MS;

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build a SceneGraph backed by a shared TestClock.
fn scene_with_clock() -> (SceneGraph, TestClock) {
    let clock = TestClock::new(1_000);
    let scene = SceneGraph::new_with_clock(DISPLAY_W, DISPLAY_H, Arc::new(clock.clone()));
    (scene, clock)
}

/// Compute the y-offset for a presence card given its agent index.
fn card_y_offset(agent_index: usize) -> f32 {
    DISPLAY_H
        - CARD_H * (agent_index as f32 + 1.0)
        - CARD_GAP * (agent_index as f32)
        - BOTTOM_MARGIN
}

/// Build a Rect for a presence card tile given agent index.
fn card_bounds(agent_index: usize) -> Rect {
    Rect::new(LEFT_MARGIN, card_y_offset(agent_index), CARD_W, CARD_H)
}

/// Build a minimal MutationBatch with the given mutations.
fn make_batch(
    agent_namespace: &str,
    lease_id: Option<SceneId>,
    mutations: Vec<SceneMutation>,
) -> MutationBatch {
    MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: agent_namespace.to_string(),
        mutations,
        timing_hints: None,
        lease_id,
    }
}

/// Build a minimal presence card root node (background SolidColor only).
///
/// These disconnect/reconnect tests exercise lease state-machine behavior, not node
/// content. A single root node (no children) is sufficient: the scene graph's
/// `insert_node_tree` only inserts the root; child IDs in `node.children` must be
/// inserted via separate `AddNode` mutations to become valid graph entries.
/// Including a `text_id` in children without a corresponding `AddNode` would create
/// a dangling reference detected by `check_node_child_consistency` invariants.
///
/// For full multi-node tree tests see `presence_card_tile.rs`.
fn make_card_root_node(_agent_name: &str) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba {
                r: 0.10,
                g: 0.14,
                b: 0.19,
                a: 0.72,
            },
            bounds: Rect::new(0.0, 0.0, CARD_W, CARD_H),
            radius: None,
        }),
    }
}

// ─── Setup helper ─────────────────────────────────────────────────────────────

/// Shared setup: create a tab and 3 agent leases each with a presence card tile.
///
/// Returns: (scene, clock, tab_id, [lease_id0, lease_id1, lease_id2], [tile_id0, tile_id1, tile_id2])
fn setup_three_agent_scene() -> (SceneGraph, TestClock, SceneId, [SceneId; 3], [SceneId; 3]) {
    let (mut scene, clock) = scene_with_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);

    let namespaces = ["agent-0", "agent-1", "agent-2"];
    let mut lease_ids = [SceneId::nil(); 3];
    let mut tile_ids = [SceneId::nil(); 3];

    for (i, ns) in namespaces.iter().enumerate() {
        let lease_id = scene.grant_lease(ns, 120_000);
        lease_ids[i] = lease_id;

        // Create tile
        let bounds = card_bounds(i);
        let z_order = Z_ORDER_BASE + i as u32;
        let batch = make_batch(
            ns,
            Some(lease_id),
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: ns.to_string(),
                lease_id,
                bounds,
                z_order,
            }],
        );
        let result = scene.apply_batch(&batch);
        assert!(result.applied, "CreateTile for agent {i} must be accepted");
        tile_ids[i] = result.created_ids[0];

        // Set tile content (root node)
        let root_node = make_card_root_node(ns);
        let set_root = make_batch(
            ns,
            Some(lease_id),
            vec![SceneMutation::SetTileRoot {
                tile_id: tile_ids[i],
                node: root_node,
                descendants: vec![],
            }],
        );
        let content_result = scene.apply_batch(&set_root);
        assert!(
            content_result.applied,
            "SetTileRoot for agent {i} must be accepted"
        );
    }

    (scene, clock, tab_id, lease_ids, tile_ids)
}

// ─── 1. Heartbeat timeout detection ──────────────────────────────────────────

/// WHEN heartbeat interval is 5000ms and missed_threshold is 3
/// THEN orphan detection timeout is 15000ms.
///
/// This test verifies the constants match the spec rather than testing live
/// heartbeat network behavior (which is the session layer's concern).
#[test]
fn heartbeat_timeout_is_three_intervals() {
    assert_eq!(HEARTBEAT_INTERVAL_MS, 5_000, "interval must be 5000ms");
    assert_eq!(HEARTBEAT_MISSED_THRESHOLD, 3, "threshold must be 3 misses");
    assert_eq!(
        HEARTBEAT_TIMEOUT_MS, 15_000,
        "timeout = 3 × 5000ms = 15000ms (lease-governance/spec.md lines 132-155)"
    );
}

/// WHEN an agent misses 2 heartbeats THEN the connection is NOT dead.
/// WHEN an agent misses 3 heartbeats THEN the connection IS dead.
///
/// Simulates the counter logic that the session layer maintains.
#[test]
fn heartbeat_missed_counter_triggers_at_threshold() {
    let mut missed: u64 = 0;

    // Miss 2 — not dead yet
    missed += 2;
    assert!(
        missed < HEARTBEAT_MISSED_THRESHOLD,
        "2 missed heartbeats must NOT trigger orphan detection"
    );

    // Miss one more — now at threshold
    missed += 1;
    assert_eq!(
        missed, HEARTBEAT_MISSED_THRESHOLD,
        "3 missed heartbeats must equal threshold"
    );
    assert!(
        missed >= HEARTBEAT_MISSED_THRESHOLD,
        "connection must be declared dead at threshold"
    );
}

/// WHEN the runtime detects heartbeat timeout (simulated as 15s elapsed without message)
/// THEN agent 2's lease transitions from ACTIVE to ORPHANED.
///
/// Scene-graph-level test: the session layer calls `disconnect_lease` after
/// timeout detection. We simulate that call directly.
#[test]
fn disconnect_transitions_lease_active_to_orphaned() {
    let (mut scene, clock, _tab_id, lease_ids, _tile_ids) = setup_three_agent_scene();

    // Verify initial state: all leases Active
    for (i, &lid) in lease_ids.iter().enumerate() {
        assert_eq!(
            scene.leases[&lid].state,
            LeaseState::Active,
            "agent {i} lease must be Active initially"
        );
    }

    // Advance clock by heartbeat timeout (3 × 5s = 15s) — connection declared dead
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();

    // Runtime calls disconnect_lease for agent 2 (the one that dropped)
    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect_lease must succeed from Active state");

    // Verify: agent 2 → ORPHANED
    assert_eq!(
        scene.leases[&lease_ids[2]].state,
        LeaseState::Orphaned,
        "agent 2 lease must transition to Orphaned after heartbeat timeout"
    );

    // Verify: agents 0 and 1 → still Active
    assert_eq!(
        scene.leases[&lease_ids[0]].state,
        LeaseState::Active,
        "agent 0 lease must remain Active"
    );
    assert_eq!(
        scene.leases[&lease_ids[1]].state,
        LeaseState::Active,
        "agent 1 lease must remain Active"
    );
}

/// WHEN agent 2's lease becomes ORPHANED
/// THEN disconnected_at_ms is recorded at the orphan timestamp.
#[test]
fn disconnect_records_disconnected_at_ms() {
    let (mut scene, clock, _tab_id, lease_ids, _tile_ids) = setup_three_agent_scene();

    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();

    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");

    let lease = &scene.leases[&lease_ids[2]];
    assert!(
        lease.disconnected_at_ms.is_some(),
        "disconnected_at_ms must be set after disconnect"
    );
    assert_eq!(
        lease.disconnected_at_ms,
        Some(now_ms),
        "disconnected_at_ms must equal the time of disconnect call"
    );
}

// ─── 2. Disconnection badge rendering ────────────────────────────────────────

/// WHEN agent 2's lease transitions to ORPHANED
/// THEN agent 2's tile visual_hint = DisconnectionBadge (within 1 frame of state change).
///
/// The scene graph sets the badge synchronously in `disconnect_lease`, so it
/// is guaranteed within the same call — which is within 1 frame of the state change.
#[test]
fn disconnection_badge_appears_on_orphaned_tile() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    // Verify initial state: no badges
    assert_eq!(
        scene.tiles[&tile_ids[2]].visual_hint,
        TileVisualHint::None,
        "agent 2 tile must have no visual hint initially"
    );

    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();

    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");

    // Badge must be set synchronously (= within same call = within 1 frame)
    assert_eq!(
        scene.tiles[&tile_ids[2]].visual_hint,
        TileVisualHint::DisconnectionBadge,
        "agent 2 tile must show DisconnectionBadge immediately after disconnect"
    );
}

/// WHEN agent 2's tile shows DisconnectionBadge
/// THEN agents 0 and 1 tiles remain with no badge.
#[test]
fn disconnection_badge_only_on_orphaned_agent_tile() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();

    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");

    // Agents 0 and 1: no badge
    assert_eq!(
        scene.tiles[&tile_ids[0]].visual_hint,
        TileVisualHint::None,
        "agent 0 tile must NOT show DisconnectionBadge"
    );
    assert_eq!(
        scene.tiles[&tile_ids[1]].visual_hint,
        TileVisualHint::None,
        "agent 1 tile must NOT show DisconnectionBadge"
    );
}

// ─── 3. Frozen tile: mutations rejected during ORPHANED ───────────────────────

/// WHEN agent 2's lease is ORPHANED
/// THEN mutations from agent 2 (using agent 2's lease_id) are rejected.
///
/// Spec: "No mutations from the disconnected agent are accepted during ORPHANED state."
#[test]
fn mutations_rejected_from_orphaned_agent() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();

    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");

    // Attempt a SetTileRoot mutation from agent 2 (ORPHANED lease)
    let updated_root = make_card_root_node("agent-2-updated");
    let mutation_batch = make_batch(
        "agent-2",
        Some(lease_ids[2]),
        vec![SceneMutation::SetTileRoot {
            tile_id: tile_ids[2],
            node: updated_root,
            descendants: vec![],
        }],
    );

    let result = scene.apply_batch(&mutation_batch);

    assert!(
        !result.applied,
        "SetTileRoot from ORPHANED agent must be rejected"
    );
    assert!(
        result.rejection.is_some(),
        "rejection reason must be present"
    );
}

/// WHEN agent 2's lease is ORPHANED
/// THEN UpdateTileOpacity mutation from agent 2 is also rejected.
#[test]
fn opacity_update_rejected_from_orphaned_agent() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();

    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");

    let mutation_batch = make_batch(
        "agent-2",
        Some(lease_ids[2]),
        vec![SceneMutation::UpdateTileOpacity {
            tile_id: tile_ids[2],
            opacity: 0.5,
        }],
    );

    let result = scene.apply_batch(&mutation_batch);
    assert!(
        !result.applied,
        "UpdateTileOpacity from ORPHANED agent must be rejected"
    );
}

/// WHEN agent 2's lease is ORPHANED
/// THEN agent 2's tile content remains at the last committed state.
///
/// The tile root node ID must not change after the rejected mutation.
#[test]
fn orphaned_tile_content_frozen_at_last_committed_state() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    // Record the tile's root node before disconnect
    let root_before = scene.tiles[&tile_ids[2]].root_node;

    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();

    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");

    // Try to update — must fail
    let updated_root = make_card_root_node("agent-2-new-content");
    let mutation_batch = make_batch(
        "agent-2",
        Some(lease_ids[2]),
        vec![SceneMutation::SetTileRoot {
            tile_id: tile_ids[2],
            node: updated_root,
            descendants: vec![],
        }],
    );
    let _ = scene.apply_batch(&mutation_batch);

    // Root node must be unchanged
    let root_after = scene.tiles[&tile_ids[2]].root_node;
    assert_eq!(
        root_before, root_after,
        "tile root node must be unchanged after rejected mutation from ORPHANED agent"
    );
}

/// WHEN agent 2's lease is ORPHANED
/// THEN agents 0 and 1 can still submit mutations successfully.
#[test]
fn active_agents_mutations_accepted_while_agent2_orphaned() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();

    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");

    // Agent 0: update opacity
    let batch0 = make_batch(
        "agent-0",
        Some(lease_ids[0]),
        vec![SceneMutation::UpdateTileOpacity {
            tile_id: tile_ids[0],
            opacity: 0.9,
        }],
    );
    let result0 = scene.apply_batch(&batch0);
    assert!(
        result0.applied,
        "agent 0 mutation must be accepted while agent 2 is ORPHANED"
    );

    // Agent 1: update content
    let updated_root_1 = make_card_root_node("agent-1-updated");
    let batch1 = make_batch(
        "agent-1",
        Some(lease_ids[1]),
        vec![SceneMutation::SetTileRoot {
            tile_id: tile_ids[1],
            node: updated_root_1,
            descendants: vec![],
        }],
    );
    let result1 = scene.apply_batch(&batch1);
    assert!(
        result1.applied,
        "agent 1 mutation must be accepted while agent 2 is ORPHANED"
    );
}

// ─── 4. Grace period (30s): ORPHANED → EXPIRED, tile removed ────────────────

/// WHEN agent 2's grace period of 30s has NOT elapsed
/// THEN agent 2's tile is still in the scene graph.
#[test]
fn tile_visible_during_grace_period() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");

    // Advance to 29s into grace — not yet expired
    clock.advance(29_000);

    // Tile must still be in the scene
    assert!(
        scene.tiles.contains_key(&tile_ids[2]),
        "agent 2 tile must remain in scene during grace period (before expiry)"
    );
    // Lease must still be ORPHANED
    assert_eq!(
        scene.leases[&lease_ids[2]].state,
        LeaseState::Orphaned,
        "lease must remain ORPHANED during grace period"
    );
}

/// WHEN agent 2's 30s grace period expires (no reconnect)
/// THEN `expire_leases()` transitions the lease to EXPIRED and removes agent 2's tile.
///
/// Acceptance criterion 4: grace period expiry → ORPHANED → EXPIRED, tile removed.
#[test]
fn grace_period_expiry_removes_tile_and_expires_lease() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    // Disconnect agent 2
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");

    assert_eq!(
        scene.tile_count(),
        3,
        "all 3 tiles must be present initially"
    );

    // Advance past grace period (30s + a little margin)
    clock.advance(GRACE_PERIOD_MS + 500);

    // Trigger expiry sweep
    let expiries = scene.expire_leases();

    // Exactly one lease must have been expired
    assert_eq!(
        expiries.len(),
        1,
        "exactly one lease must expire after grace period"
    );
    assert_eq!(
        expiries[0].lease_id, lease_ids[2],
        "the expired lease must be agent 2's lease"
    );
    assert_eq!(
        expiries[0].terminal_state,
        LeaseState::Expired,
        "terminal state must be Expired"
    );
    assert_eq!(
        expiries[0].removed_tiles,
        vec![tile_ids[2]],
        "agent 2's tile must be in removed_tiles"
    );

    // Lease in scene graph must be Expired
    assert_eq!(
        scene.leases[&lease_ids[2]].state,
        LeaseState::Expired,
        "lease must be in Expired state after grace period expiry"
    );

    // Tile must be removed from scene graph
    assert!(
        !scene.tiles.contains_key(&tile_ids[2]),
        "agent 2 tile must be removed from scene graph after grace period expiry"
    );

    // Total tile count: 2 remaining
    assert_eq!(
        scene.tile_count(),
        2,
        "scene must have exactly 2 tiles after agent 2's tile is removed"
    );
}

/// WHEN agent 2's tile is removed after grace expiry
/// THEN agents 0 and 1 tiles remain at their original positions.
///
/// Acceptance criterion 7: no automatic repositioning of remaining tiles.
#[test]
fn remaining_tiles_not_repositioned_after_agent2_removal() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    // Record positions of agents 0 and 1 before disconnect
    let bounds0_before = scene.tiles[&tile_ids[0]].bounds;
    let bounds1_before = scene.tiles[&tile_ids[1]].bounds;

    // Disconnect agent 2 and advance past grace
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");
    clock.advance(GRACE_PERIOD_MS + 500);
    scene.expire_leases();

    // Tiles 0 and 1 must still be present
    assert!(
        scene.tiles.contains_key(&tile_ids[0]),
        "agent 0 tile must still be in scene"
    );
    assert!(
        scene.tiles.contains_key(&tile_ids[1]),
        "agent 1 tile must still be in scene"
    );

    // Positions must not have changed (no auto-reposition)
    let bounds0_after = scene.tiles[&tile_ids[0]].bounds;
    let bounds1_after = scene.tiles[&tile_ids[1]].bounds;

    assert_eq!(
        bounds0_before, bounds0_after,
        "agent 0 tile bounds must be unchanged after agent 2 removal"
    );
    assert_eq!(
        bounds1_before, bounds1_after,
        "agent 1 tile bounds must be unchanged after agent 2 removal"
    );

    // Verify expected positions match spec (tab_height - 136 and tab_height - 260)
    let expected_y0 = DISPLAY_H - 136.0; // agent 0: y = tab_height - 136
    let expected_y1 = DISPLAY_H - 260.0; // agent 1: y = tab_height - 260
    assert_eq!(
        bounds0_after.y, expected_y0,
        "agent 0 tile y must be tab_height - 136 = {expected_y0}"
    );
    assert_eq!(
        bounds1_after.y, expected_y1,
        "agent 1 tile y must be tab_height - 260 = {expected_y1}"
    );
}

// ─── 5. Reconnect within grace period ────────────────────────────────────────

/// WHEN agent 2 reconnects within the 30s grace period
/// THEN the lease transitions ORPHANED → ACTIVE.
///
/// Acceptance criterion 5: reconnect within grace restores Active lease.
#[test]
fn reconnect_within_grace_restores_active_lease() {
    let (mut scene, clock, _tab_id, lease_ids, _tile_ids) = setup_three_agent_scene();

    // Disconnect agent 2 at T=15s (heartbeat timeout)
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let disconnect_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], disconnect_ms)
        .expect("disconnect must succeed");

    assert_eq!(
        scene.leases[&lease_ids[2]].state,
        LeaseState::Orphaned,
        "lease must be Orphaned after disconnect"
    );

    // Reconnect at T+20s (within 30s grace)
    clock.advance(20_000);
    let reconnect_ms = clock.now_millis();
    scene
        .reconnect_lease(&lease_ids[2], reconnect_ms)
        .expect("reconnect within grace must succeed");

    // Lease must be Active again
    assert_eq!(
        scene.leases[&lease_ids[2]].state,
        LeaseState::Active,
        "lease must transition to Active after reconnect within grace"
    );

    // disconnected_at_ms must be cleared
    assert!(
        scene.leases[&lease_ids[2]].disconnected_at_ms.is_none(),
        "disconnected_at_ms must be cleared after successful reconnect"
    );
}

/// WHEN agent 2 reconnects within grace
/// THEN the disconnection badge MUST clear (visual_hint = None).
///
/// Acceptance criterion 5: badge clears within 1 frame of reconnect.
/// The scene graph clears the badge synchronously in reconnect_lease.
#[test]
fn badge_clears_on_reconnect_within_grace() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    // Disconnect
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let disconnect_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], disconnect_ms)
        .expect("disconnect must succeed");

    assert_eq!(
        scene.tiles[&tile_ids[2]].visual_hint,
        TileVisualHint::DisconnectionBadge,
        "tile must show DisconnectionBadge after disconnect"
    );

    // Reconnect within grace
    clock.advance(20_000);
    let reconnect_ms = clock.now_millis();
    scene
        .reconnect_lease(&lease_ids[2], reconnect_ms)
        .expect("reconnect within grace must succeed");

    // Badge must be cleared synchronously
    assert_eq!(
        scene.tiles[&tile_ids[2]].visual_hint,
        TileVisualHint::None,
        "DisconnectionBadge must be cleared after reconnect within grace"
    );
}

/// WHEN agent 2 reconnects within grace
/// THEN agent 2 can immediately resume content updates.
///
/// Acceptance criterion 5: "Agent 2 can resume content updates immediately after reconnection."
#[test]
fn content_updates_resume_after_reconnect() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    // Disconnect agent 2
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let disconnect_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], disconnect_ms)
        .expect("disconnect must succeed");

    // Reconnect within grace
    clock.advance(20_000);
    let reconnect_ms = clock.now_millis();
    scene
        .reconnect_lease(&lease_ids[2], reconnect_ms)
        .expect("reconnect within grace must succeed");

    // Content update must now succeed
    let updated_root = make_card_root_node("agent-2-reconnected");
    let mutation_batch = make_batch(
        "agent-2",
        Some(lease_ids[2]),
        vec![SceneMutation::SetTileRoot {
            tile_id: tile_ids[2],
            node: updated_root,
            descendants: vec![],
        }],
    );
    let result = scene.apply_batch(&mutation_batch);

    assert!(
        result.applied,
        "agent 2 SetTileRoot must succeed after reconnect within grace"
    );
}

/// WHEN agent 2 attempts to reconnect AFTER the 30s grace period has expired
/// THEN reconnect must fail (grace period expired).
#[test]
fn reconnect_after_grace_fails() {
    let (mut scene, clock, _tab_id, lease_ids, _tile_ids) = setup_three_agent_scene();

    // Disconnect
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let disconnect_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], disconnect_ms)
        .expect("disconnect must succeed");

    // Advance past grace period (30s + margin)
    clock.advance(GRACE_PERIOD_MS + 1_000);
    let late_ms = clock.now_millis();

    // Reconnect attempt must fail
    let result = scene.reconnect_lease(&lease_ids[2], late_ms);
    assert!(
        result.is_err(),
        "reconnect after grace period must be rejected"
    );

    // Lease must still be Orphaned (expire_leases not called yet)
    assert_eq!(
        scene.leases[&lease_ids[2]].state,
        LeaseState::Orphaned,
        "lease remains Orphaned until expire_leases is called"
    );
}

// ─── 6. Multi-agent isolation ────────────────────────────────────────────────

/// WHEN agent 2 disconnects and its grace period expires
/// THEN agents 0 and 1 leases remain Active throughout.
///
/// Acceptance criterion 6: "Agents 0 and 1 unaffected throughout."
#[test]
fn agents_0_and_1_leases_stay_active_throughout_disconnect_cycle() {
    let (mut scene, clock, _tab_id, lease_ids, _tile_ids) = setup_three_agent_scene();

    // Step 1: Agent 2 disconnects at heartbeat timeout
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");

    // Agents 0 and 1 must be Active
    assert_eq!(
        scene.leases[&lease_ids[0]].state,
        LeaseState::Active,
        "agent 0 lease must be Active after agent 2 disconnect"
    );
    assert_eq!(
        scene.leases[&lease_ids[1]].state,
        LeaseState::Active,
        "agent 1 lease must be Active after agent 2 disconnect"
    );

    // Step 2: Grace period elapses (30s)
    clock.advance(GRACE_PERIOD_MS + 500);
    scene.expire_leases();

    // Agents 0 and 1 must still be Active after grace expiry + tile removal
    assert_eq!(
        scene.leases[&lease_ids[0]].state,
        LeaseState::Active,
        "agent 0 lease must be Active after agent 2 grace expiry"
    );
    assert_eq!(
        scene.leases[&lease_ids[1]].state,
        LeaseState::Active,
        "agent 1 lease must be Active after agent 2 grace expiry"
    );
}

/// WHEN agent 2 disconnects and its tile is removed
/// THEN agents 0 and 1 can continue submitting content updates without interference.
///
/// Acceptance criterion 6: "their leases remain ACTIVE, content updates continue succeeding."
#[test]
fn agents_0_and_1_updates_continue_after_agent2_removal() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    // Disconnect agent 2 and wait for grace expiry
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");
    clock.advance(GRACE_PERIOD_MS + 500);
    scene.expire_leases();

    // Agent 0 content update after agent 2's cleanup
    let updated_root0 = make_card_root_node("agent-0-post-cleanup");
    let batch0 = make_batch(
        "agent-0",
        Some(lease_ids[0]),
        vec![SceneMutation::SetTileRoot {
            tile_id: tile_ids[0],
            node: updated_root0,
            descendants: vec![],
        }],
    );
    let result0 = scene.apply_batch(&batch0);
    assert!(
        result0.applied,
        "agent 0 content update must succeed after agent 2 removal"
    );

    // Agent 1 content update after agent 2's cleanup
    let updated_root1 = make_card_root_node("agent-1-post-cleanup");
    let batch1 = make_batch(
        "agent-1",
        Some(lease_ids[1]),
        vec![SceneMutation::SetTileRoot {
            tile_id: tile_ids[1],
            node: updated_root1,
            descendants: vec![],
        }],
    );
    let result1 = scene.apply_batch(&batch1);
    assert!(
        result1.applied,
        "agent 1 content update must succeed after agent 2 removal"
    );
}

/// WHEN agent 2 disconnects and later its tile is removed via grace expiry
/// THEN agents 0 and 1 tiles are at y = tab_height - 136 and y = tab_height - 260.
///
/// Acceptance criterion 6 + 7: correct positions, no repositioning.
#[test]
fn agents_0_and_1_tiles_at_correct_positions_after_agent2_expiry() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    // Execute full disconnect cycle for agent 2
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let now_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], now_ms)
        .expect("disconnect must succeed");
    clock.advance(GRACE_PERIOD_MS + 500);
    scene.expire_leases();

    // Verify tile positions
    let bounds0 = scene.tiles[&tile_ids[0]].bounds;
    let bounds1 = scene.tiles[&tile_ids[1]].bounds;

    // Agent 0: y = tab_height - 136 (CARD_H=112 + BOTTOM_MARGIN=24 = 136)
    assert_eq!(
        bounds0.y,
        DISPLAY_H - 136.0,
        "agent 0 tile y must be tab_height - 136 (= {}) after agent 2 removal",
        DISPLAY_H - 136.0
    );

    // Agent 1: y = tab_height - 260 (2×CARD_H + GAP + BOTTOM_MARGIN = 224 + 12 + 24 = 260)
    assert_eq!(
        bounds1.y,
        DISPLAY_H - 260.0,
        "agent 1 tile y must be tab_height - 260 (= {}) after agent 2 removal",
        DISPLAY_H - 260.0
    );
}

// ─── 7. Full pipeline: combined scenario ────────────────────────────────────

/// Full pipeline test: exercises all 6 acceptance criteria in sequence.
///
/// Timeline:
/// - T=0 (+ 1s offset from TestClock): Setup — 3 agents, 3 tiles
/// - T+15s: Agent 2 heartbeat timeout → disconnect_lease → ORPHANED, badge appears
/// - T+15s: Verify badge, verify mutations from agent 2 rejected
/// - T+15s: Verify agents 0 and 1 still active, mutations still accepted
/// - T+35s: Reconnect at T+20s (within grace) — ORPHANED → ACTIVE, badge clears
/// - [Note: The reconnect sub-scenario and the expiry sub-scenario are tested separately
///   since they are mutually exclusive. This test covers the reconnect path.]
/// - Expiry path is covered by grace_period_expiry_removes_tile_and_expires_lease.
#[test]
fn full_pipeline_reconnect_within_grace_scenario() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    // --- Phase 1: Setup verified ---
    assert_eq!(scene.tile_count(), 3, "3 tiles must be present initially");
    for i in 0..3 {
        assert_eq!(
            scene.leases[&lease_ids[i]].state,
            LeaseState::Active,
            "all leases must be Active initially"
        );
        assert_eq!(
            scene.tiles[&tile_ids[i]].visual_hint,
            TileVisualHint::None,
            "no visual hints initially"
        );
    }

    // --- Phase 2: Heartbeat timeout → ORPHANED ---
    clock.advance(HEARTBEAT_TIMEOUT_MS); // +15s
    let disconnect_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], disconnect_ms)
        .expect("disconnect must succeed");

    // AC1: Lease is ORPHANED
    assert_eq!(
        scene.leases[&lease_ids[2]].state,
        LeaseState::Orphaned,
        "AC1: lease must be Orphaned after heartbeat timeout"
    );
    // AC2: Badge appears on agent 2's tile
    assert_eq!(
        scene.tiles[&tile_ids[2]].visual_hint,
        TileVisualHint::DisconnectionBadge,
        "AC2: DisconnectionBadge must appear on orphaned tile"
    );
    // AC3: Mutations from agent 2 rejected
    let rejected_batch = make_batch(
        "agent-2",
        Some(lease_ids[2]),
        vec![SceneMutation::UpdateTileOpacity {
            tile_id: tile_ids[2],
            opacity: 0.3,
        }],
    );
    let rejected_result = scene.apply_batch(&rejected_batch);
    assert!(
        !rejected_result.applied,
        "AC3: mutations from ORPHANED agent must be rejected"
    );
    // AC6: Agents 0 and 1 unaffected
    assert_eq!(
        scene.leases[&lease_ids[0]].state,
        LeaseState::Active,
        "AC6: agent 0 must remain Active"
    );
    assert_eq!(
        scene.leases[&lease_ids[1]].state,
        LeaseState::Active,
        "AC6: agent 1 must remain Active"
    );
    let ok_batch0 = make_batch(
        "agent-0",
        Some(lease_ids[0]),
        vec![SceneMutation::UpdateTileOpacity {
            tile_id: tile_ids[0],
            opacity: 1.0,
        }],
    );
    assert!(
        scene.apply_batch(&ok_batch0).applied,
        "AC6: agent 0 mutation must succeed during agent 2 orphan"
    );

    // --- Phase 3: Reconnect at T+20s (within 30s grace) ---
    clock.advance(20_000); // +20s from disconnect (total +35s from start)
    let reconnect_ms = clock.now_millis();
    scene
        .reconnect_lease(&lease_ids[2], reconnect_ms)
        .expect("reconnect within grace must succeed");

    // AC5: Lease back to Active
    assert_eq!(
        scene.leases[&lease_ids[2]].state,
        LeaseState::Active,
        "AC5: lease must be Active after reconnect within grace"
    );
    // AC5: Badge clears
    assert_eq!(
        scene.tiles[&tile_ids[2]].visual_hint,
        TileVisualHint::None,
        "AC5: DisconnectionBadge must be cleared after reconnect"
    );
    // AC5: Agent 2 can resume updates
    let resume_batch = make_batch(
        "agent-2",
        Some(lease_ids[2]),
        vec![SceneMutation::UpdateTileOpacity {
            tile_id: tile_ids[2],
            opacity: 1.0,
        }],
    );
    assert!(
        scene.apply_batch(&resume_batch).applied,
        "AC5: agent 2 mutation must succeed after reconnect"
    );
    // AC6: Agents 0 and 1 still active throughout
    assert_eq!(
        scene.leases[&lease_ids[0]].state,
        LeaseState::Active,
        "AC6: agent 0 must remain Active after agent 2 reconnect"
    );
    assert_eq!(
        scene.leases[&lease_ids[1]].state,
        LeaseState::Active,
        "AC6: agent 1 must remain Active after agent 2 reconnect"
    );
    // All 3 tiles remain
    assert_eq!(
        scene.tile_count(),
        3,
        "all 3 tiles must be present after agent 2 reconnect"
    );
}

/// Full pipeline test — expiry path.
///
/// Timeline:
/// - Setup: 3 agents, 3 tiles
/// - T+15s: Agent 2 disconnects → ORPHANED
/// - T+45s (+30s grace): expire_leases() → EXPIRED, agent 2 tile removed
/// - Agents 0 and 1 unaffected, tiles at original positions
#[test]
fn full_pipeline_grace_expiry_scenario() {
    let (mut scene, clock, _tab_id, lease_ids, tile_ids) = setup_three_agent_scene();

    // Phase 1: Disconnect agent 2
    clock.advance(HEARTBEAT_TIMEOUT_MS);
    let disconnect_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_ids[2], disconnect_ms)
        .expect("disconnect must succeed");

    // AC1 + AC2 + AC3 already covered by individual tests
    // Verify badge is present
    assert_eq!(
        scene.tiles[&tile_ids[2]].visual_hint,
        TileVisualHint::DisconnectionBadge
    );

    // Phase 2: Let grace expire without reconnect
    clock.advance(GRACE_PERIOD_MS + 1_000); // +31s
    let expiries = scene.expire_leases();

    // AC4: Grace period expiry → EXPIRED, tile removed
    assert_eq!(expiries.len(), 1, "AC4: one lease must expire");
    assert_eq!(
        expiries[0].terminal_state,
        LeaseState::Expired,
        "AC4: terminal state must be Expired"
    );
    assert!(
        !scene.tiles.contains_key(&tile_ids[2]),
        "AC4: agent 2 tile must be removed after grace expiry"
    );
    assert_eq!(scene.tile_count(), 2, "AC4: 2 tiles must remain");

    // AC6: Agents 0 and 1 unaffected
    assert_eq!(scene.leases[&lease_ids[0]].state, LeaseState::Active);
    assert_eq!(scene.leases[&lease_ids[1]].state, LeaseState::Active);
    assert!(scene.tiles.contains_key(&tile_ids[0]));
    assert!(scene.tiles.contains_key(&tile_ids[1]));

    // AC7: No repositioning — original positions preserved
    let bounds0 = scene.tiles[&tile_ids[0]].bounds;
    let bounds1 = scene.tiles[&tile_ids[1]].bounds;
    assert_eq!(bounds0.y, DISPLAY_H - 136.0, "agent 0 y must be unchanged");
    assert_eq!(bounds1.y, DISPLAY_H - 260.0, "agent 1 y must be unchanged");

    // Agents 0 and 1 still accept mutations
    let ok_batch0 = make_batch(
        "agent-0",
        Some(lease_ids[0]),
        vec![SceneMutation::UpdateTileInputMode {
            tile_id: tile_ids[0],
            input_mode: InputMode::Capture,
        }],
    );
    assert!(
        scene.apply_batch(&ok_batch0).applied,
        "agent 0 must continue accepting mutations after agent 2 cleanup"
    );
    let ok_batch1 = make_batch(
        "agent-1",
        Some(lease_ids[1]),
        vec![SceneMutation::UpdateTileInputMode {
            tile_id: tile_ids[1],
            input_mode: InputMode::Capture,
        }],
    );
    assert!(
        scene.apply_batch(&ok_batch1).applied,
        "agent 1 must continue accepting mutations after agent 2 cleanup"
    );
}
