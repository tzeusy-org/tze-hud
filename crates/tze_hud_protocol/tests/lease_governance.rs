//! Lease governance lifecycle and namespace isolation tests.
//!
//! Implements test coverage for `hud-i6yd.7`: Wire the full lease governance
//! state machine for the dashboard tile.
//!
//! Covers tasks.md §10 (Lease Governance Lifecycle) and §11 (Namespace Isolation)
//! plus the following spec.md requirements:
//! - Requirement: Lease Request With AutoRenew — Scenario: Lease auto-renews at 75% TTL
//! - Requirement: Lease Orphan Handling on Disconnect (3 scenarios)
//! - Requirement: Lease Expiry Without Renewal Removes Tile (2 scenarios)
//! - Namespace isolation enforcement
//!
//! All tests are headless Layer 0: no display server or GPU required.
//!
//! ## Test scenarios
//!
//! 1. Auto-renewal fires at 75% TTL (45s for 60s lease) — agent receives
//!    `LeaseResponse { granted: true }` with an updated expiry.
//! 2. Agent disconnect → lease transitions ACTIVE→ORPHANED, tile frozen with
//!    `TileVisualHint::DisconnectionBadge` (must happen within 1 frame, i.e.
//!    synchronously from the scene graph's perspective).
//! 3. Agent reconnects within 30-second grace → lease transitions
//!    ORPHANED→ACTIVE, badge clears, agent can immediately submit mutations.
//! 4. Grace period expiry (no reconnect in 30s) → lease transitions
//!    ORPHANED→EXPIRED, tile and all nodes removed from scene graph.
//! 5. Explicit `LeaseRelease` → lease transitions ACTIVE→RELEASED, tile
//!    removed cleanly (via `revoke_lease` on the scene graph).
//! 6. Resource cleanup: on lease expiry, icon image resource `ref_count` drops;
//!    modelled using `SceneGraph::expire_leases` and ref-count assertions.
//! 7. Namespace isolation: second agent session cannot mutate dashboard tile
//!    (rejected with `NamespaceMismatch`).
//! 8. Namespace isolation: dashboard agent cannot mutate tiles owned by a
//!    different namespace (also `NamespaceMismatch`).

use std::sync::Arc;
use tze_hud_scene::clock::TestClock;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::lease::TileVisualHint;
use tze_hud_scene::types::{InputMode, LeaseState, Rect, SceneId};
use tze_hud_scene::validation::ValidationError;
// Clock trait must be in scope for `now_millis()` to resolve via Deref on Arc<TestClock>.
use tze_hud_scene::Clock;

// ─── Test helpers ─────────────────────────────────────────────────────────────

/// Build a `SceneGraph` with a `TestClock` at `start_ms`.
fn make_scene(start_ms: u64) -> (SceneGraph, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(start_ms));
    let scene = SceneGraph::new_with_clock(800.0, 600.0, clock.clone());
    (scene, clock)
}

/// Create a tab and set it active, then return the tab ID.
fn setup_active_tab(scene: &mut SceneGraph) -> SceneId {
    let tab_id = scene.create_tab("Test Tab", 0).unwrap();
    scene.switch_active_tab(tab_id).unwrap();
    tab_id
}

/// Grant a 60-second lease in namespace `ns` and return its ID.
fn grant_lease(scene: &mut SceneGraph, ns: &str) -> SceneId {
    scene.grant_lease(ns, 60_000)
}

/// Create a tile for `ns` under `lease_id` and return its ID.
fn create_tile(scene: &mut SceneGraph, tab_id: SceneId, ns: &str, lease_id: SceneId) -> SceneId {
    scene
        .create_tile_checked(
            tab_id,
            ns,
            lease_id,
            Rect::new(50.0, 50.0, 400.0, 300.0),
            100,
        )
        .expect("tile creation must succeed")
}

// ─── Scenario 10.1: Auto-renewal at 75% TTL ───────────────────────────────────

/// spec.md §Requirement: Lease Request With AutoRenew — Scenario: Lease auto-renews at 75% TTL
///
/// WHEN a 60-second lease has been active for 45 seconds (75% of 60s TTL)
/// THEN the TTL state machine's `poll()` returns `TtlCheck::AutoRenewDue`
/// AND after calling `reset_renewal_window` the remaining TTL is reset to the
///     fresh window.
#[test]
fn auto_renewal_fires_at_75_percent_ttl() {
    use tze_hud_scene::clock::TestClock;
    use tze_hud_scene::lease::{
        RenewalPolicy,
        ttl::{TtlCheck, TtlState},
    };

    let clock = TestClock::new(0);
    let mut ttl = TtlState::new_activated(60_000, RenewalPolicy::AutoRenew, clock.clone());

    // Just before 75% threshold (44_999 ms < 45_000 ms) — no renewal yet.
    clock.advance(44_999);
    assert_eq!(
        ttl.poll(),
        TtlCheck::Ok,
        "poll must return Ok before 75% threshold"
    );

    // Step over the threshold to exactly 45_000 ms (75% of 60_000 ms).
    clock.advance(1); // total = 45_000 ms = 75%
    assert_eq!(
        ttl.poll(),
        TtlCheck::AutoRenewDue,
        "poll must return AutoRenewDue at 75% TTL elapsed"
    );

    // Simulate the session layer renewing the lease.
    ttl.reset_renewal_window(60_000);

    // After reset, poll at 0 ms elapsed — should be Ok (not fire again immediately).
    assert_eq!(
        ttl.poll(),
        TtlCheck::Ok,
        "poll must not fire again immediately after renewal reset"
    );

    // Advance to 75% of the fresh window (45_000 ms).
    clock.advance(45_001); // 45_001 ms past reset → ≥ 75%
    assert_eq!(
        ttl.poll(),
        TtlCheck::AutoRenewDue,
        "poll must fire again at 75% of the renewed TTL window"
    );
}

/// Auto-renewal timer arms at lease activation.
///
/// Verifies the `AutoRenewalArm::Armed` state after a fresh lease with
/// `AutoRenew` policy — prerequisite for the 75% trigger.
#[test]
fn auto_renewal_arm_armed_at_activation() {
    use tze_hud_scene::clock::TestClock;
    use tze_hud_scene::lease::{
        RenewalPolicy,
        ttl::{AutoRenewalArm, TtlState},
    };

    let clock = TestClock::new(0);
    let ttl = TtlState::new_activated(60_000, RenewalPolicy::AutoRenew, clock);
    assert_eq!(
        ttl.auto_renewal_arm(),
        AutoRenewalArm::Armed,
        "AutoRenew lease must start with the renewal timer Armed"
    );
}

// ─── Scenario 10.2: Disconnect → ORPHANED with badge ─────────────────────────

/// spec.md §Lease Orphan Handling on Disconnect — Scenario: Disconnection triggers orphan state and badge
///
/// WHEN the agent's gRPC stream disconnects unexpectedly
/// THEN the lease SHALL transition to ORPHANED
/// AND the tile SHALL be frozen at its last state
/// AND a disconnection badge SHALL appear (within 1 frame — synchronously in scene graph).
#[test]
fn disconnect_transitions_to_orphaned_and_sets_disconnection_badge() {
    let (mut scene, clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    // Grant lease and create tile.
    let lease_id = grant_lease(&mut scene, "dashboard-agent");
    let tile_id = create_tile(&mut scene, tab_id, "dashboard-agent", lease_id);

    // Verify initial state: ACTIVE, no badge.
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Active,
        "lease must start as ACTIVE"
    );
    assert_eq!(
        scene.tiles[&tile_id].visual_hint,
        TileVisualHint::None,
        "tile must have no visual hint initially"
    );

    // Simulate agent disconnect at t=5_000ms (5 seconds after activation).
    clock.advance(5_000);
    let now_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_id, now_ms)
        .expect("disconnect must succeed from ACTIVE state");

    // Lease transitions to ORPHANED.
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Orphaned,
        "lease must transition to ORPHANED on disconnect"
    );

    // Tile receives DisconnectionBadge (within 1 frame = synchronous in the scene graph).
    assert_eq!(
        scene.tiles[&tile_id].visual_hint,
        TileVisualHint::DisconnectionBadge,
        "tile must have DisconnectionBadge after disconnect"
    );

    // Tile still exists (frozen at last state — not removed during grace period).
    assert!(
        scene.tiles.contains_key(&tile_id),
        "tile must still exist during orphan grace period"
    );
}

/// Multiple tiles owned by the same lease all receive the disconnection badge.
#[test]
fn disconnect_badges_all_owned_tiles() {
    let (mut scene, clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let lease_id = grant_lease(&mut scene, "multi-tile-agent");

    // Create two tiles under the same lease.
    let tile_a = scene
        .create_tile_checked(
            tab_id,
            "multi-tile-agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 150.0),
            10,
        )
        .unwrap();
    let tile_b = scene
        .create_tile_checked(
            tab_id,
            "multi-tile-agent",
            lease_id,
            Rect::new(200.0, 0.0, 200.0, 150.0),
            11,
        )
        .unwrap();

    clock.advance(2_000);
    let now_ms = clock.now_millis();
    scene.disconnect_lease(&lease_id, now_ms).unwrap();

    assert_eq!(
        scene.tiles[&tile_a].visual_hint,
        TileVisualHint::DisconnectionBadge
    );
    assert_eq!(
        scene.tiles[&tile_b].visual_hint,
        TileVisualHint::DisconnectionBadge
    );
}

// ─── Scenario 10.3: Reconnect within grace period restores ACTIVE ─────────────

/// spec.md §Lease Orphan Handling on Disconnect — Scenario: Reconnection within grace period restores tile
///
/// WHEN the agent reconnects within 30 seconds of disconnection
/// THEN the lease SHALL transition back to ACTIVE
/// AND the disconnection badge SHALL clear (within 1 frame — synchronously)
/// AND the agent CAN immediately submit mutations.
#[test]
fn reconnect_within_grace_period_restores_active_and_clears_badge() {
    let (mut scene, clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let lease_id = grant_lease(&mut scene, "dashboard-agent");
    let tile_id = create_tile(&mut scene, tab_id, "dashboard-agent", lease_id);

    // Disconnect at t=5_000ms.
    clock.advance(5_000);
    let disconnect_ms = clock.now_millis();
    scene.disconnect_lease(&lease_id, disconnect_ms).unwrap();

    // Reconnect at t=5_000 + 15_000 = 20_000ms (within 30s grace).
    clock.advance(15_000); // 15s after disconnect = well within 30s grace
    let reconnect_ms = clock.now_millis();
    scene
        .reconnect_lease(&lease_id, reconnect_ms)
        .expect("reconnect must succeed within grace period");

    // Lease transitions back to ACTIVE.
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Active,
        "lease must transition back to ACTIVE on reconnect within grace"
    );

    // Badge clears (within 1 frame = synchronous).
    assert_eq!(
        scene.tiles[&tile_id].visual_hint,
        TileVisualHint::None,
        "disconnection badge must clear on reconnect"
    );

    // Agent can submit mutations again — create a second tile to verify the lease is operational.
    let tile_b = scene.create_tile_checked(
        tab_id,
        "dashboard-agent",
        lease_id,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        50,
    );
    assert!(
        tile_b.is_ok(),
        "agent must be able to submit mutations after reconnect within grace period"
    );
}

/// Reconnect at the boundary of the grace period (just before expiry) is still accepted.
#[test]
fn reconnect_at_grace_period_boundary_is_accepted() {
    let (mut scene, clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);
    let _ = tab_id; // tab needed only for tab setup

    let lease_id = grant_lease(&mut scene, "boundary-agent");
    clock.advance(1_000);
    let disconnect_ms = clock.now_millis();
    scene.disconnect_lease(&lease_id, disconnect_ms).unwrap();

    // Reconnect at grace_period - 1 ms (29_999 ms after disconnect).
    clock.advance(29_999);
    let reconnect_ms = clock.now_millis();
    let result = scene.reconnect_lease(&lease_id, reconnect_ms);
    assert!(
        result.is_ok(),
        "reconnect must succeed at grace_period - 1ms: {result:?}"
    );
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Active,
        "lease must be ACTIVE after reconnect at boundary"
    );
}

// ─── Scenario 10.4: Grace period expiry removes tile ─────────────────────────

/// spec.md §Lease Orphan Handling on Disconnect — Scenario: Grace period expiry removes tile
///
/// WHEN the agent fails to reconnect within 30 seconds
/// THEN the lease SHALL transition to EXPIRED
/// AND the dashboard tile (and all its nodes) SHALL be removed from the scene graph.
#[test]
fn grace_period_expiry_removes_tile_and_nodes() {
    let (mut scene, clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let lease_id = grant_lease(&mut scene, "dashboard-agent");
    let tile_id = create_tile(&mut scene, tab_id, "dashboard-agent", lease_id);

    // Verify tile exists.
    assert!(scene.tiles.contains_key(&tile_id));

    // Disconnect at t=1_000ms.
    clock.advance(1_000);
    let disconnect_ms = clock.now_millis();
    scene.disconnect_lease(&lease_id, disconnect_ms).unwrap();

    // Advance past 30-second grace period (31_000ms after disconnect).
    // Total time: 1_000 + 31_000 = 32_000ms — still within 60s TTL.
    clock.advance(31_000);

    // expire_leases sweeps grace-expired orphaned leases.
    let expiries = scene.expire_leases();

    // At least one expiry must have occurred for this lease.
    assert!(
        expiries.iter().any(|e| e.lease_id == lease_id),
        "expire_leases must return an expiry for the grace-expired orphaned lease"
    );

    // Lease state must be EXPIRED or REVOKED (terminal).
    // The lease entry remains in the map after expiry until GC; use expect() to
    // make a missing entry detectable rather than silently passing.
    let lease = scene
        .leases
        .get(&lease_id)
        .expect("lease must still be present in the map after grace expiry");
    assert!(
        lease.state == LeaseState::Expired || lease.state == LeaseState::Revoked,
        "lease must be in a terminal state after grace expiry, got {:?}",
        lease.state
    );

    // Tile must have been removed from the scene graph.
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "tile must be removed from scene graph after grace period expiry"
    );
}

/// Grace expiry also removes all nodes that were part of the tile.
#[test]
fn grace_period_expiry_removes_all_tile_nodes() {
    use tze_hud_scene::types::NodeData;

    let (mut scene, clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let lease_id = grant_lease(&mut scene, "node-agent");
    let tile_id = create_tile(&mut scene, tab_id, "node-agent", lease_id);

    // Add a child node to the tile.
    let node = tze_hud_scene::types::Node {
        layout: Default::default(),
        id: SceneId::new(),
        data: NodeData::SolidColor(tze_hud_scene::types::SolidColorNode {
            color: tze_hud_scene::types::Rgba {
                r: 0.07,
                g: 0.07,
                b: 0.07,
                a: 0.9,
            },
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            radius: None,
        }),
        children: vec![],
    };
    let node_id = node.id;
    scene
        .set_tile_root_checked(tile_id, node, "node-agent")
        .expect("set_tile_root must succeed with active lease");

    assert!(
        scene.nodes.contains_key(&node_id),
        "node must exist before expiry"
    );

    // Disconnect and advance past grace period.
    clock.advance(500);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();
    clock.advance(31_000);
    scene.expire_leases();

    // Node must be gone.
    assert!(
        !scene.nodes.contains_key(&node_id),
        "all tile nodes must be removed after grace period expiry"
    );
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "tile must also be gone after grace period expiry"
    );
}

// ─── Scenario 10.5: Explicit LeaseRelease removes tile cleanly ───────────────

/// spec.md §Requirement: Lease Expiry Without Renewal Removes Tile —
/// Scenario: Explicit LeaseRelease transitions to RELEASED and removes tile.
///
/// WHEN the agent sends a `LeaseRelease` (mapped to `revoke_lease` in the scene graph)
/// THEN the tile SHALL be removed cleanly from the scene graph.
#[test]
fn explicit_lease_release_removes_tile_cleanly() {
    let (mut scene, _clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let lease_id = grant_lease(&mut scene, "dashboard-agent");
    let tile_id = create_tile(&mut scene, tab_id, "dashboard-agent", lease_id);

    // Verify tile exists before release.
    assert!(
        scene.tiles.contains_key(&tile_id),
        "tile must exist before LeaseRelease"
    );

    // Simulate explicit LeaseRelease — session server calls `revoke_lease`.
    scene
        .revoke_lease(lease_id)
        .expect("revoke_lease must succeed from ACTIVE state");

    // Tile must be removed.
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "tile must be removed after explicit LeaseRelease"
    );

    // Lease must be in a terminal state.
    // The lease entry remains in the map after revocation until GC; use expect() to
    // make a missing entry detectable rather than silently passing.
    let lease = scene
        .leases
        .get(&lease_id)
        .expect("lease must still be present in the map after revoke_lease");
    assert!(
        lease.state.is_terminal(),
        "lease must be in terminal state after release, got {:?}",
        lease.state
    );
}

/// Explicit release also removes all nodes that were part of the tile.
#[test]
fn explicit_lease_release_removes_all_nodes() {
    use tze_hud_scene::types::NodeData;

    let (mut scene, _clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let lease_id = grant_lease(&mut scene, "release-agent");
    let tile_id = create_tile(&mut scene, tab_id, "release-agent", lease_id);

    let node = tze_hud_scene::types::Node {
        layout: Default::default(),
        id: SceneId::new(),
        data: NodeData::SolidColor(tze_hud_scene::types::SolidColorNode {
            color: tze_hud_scene::types::Rgba {
                r: 0.1,
                g: 0.1,
                b: 0.1,
                a: 1.0,
            },
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            radius: None,
        }),
        children: vec![],
    };
    let node_id = node.id;
    scene
        .set_tile_root_checked(tile_id, node, "release-agent")
        .unwrap();

    assert!(scene.nodes.contains_key(&node_id));

    scene.revoke_lease(lease_id).unwrap();

    assert!(
        !scene.nodes.contains_key(&node_id),
        "all tile nodes must be removed after explicit lease release"
    );
}

// ─── Scenario 10.6: Resource cleanup on expiry ───────────────────────────────

/// spec.md §Requirement: Lease Expiry Without Renewal Removes Tile —
/// Scenario: Resources freed after expiry
///
/// WHEN the lease expires and the tile is removed
/// THEN the icon image resource ref_count SHALL drop.
///
/// This test models the ref-count contract using `SceneGraph`'s resource usage
/// tracking (which counts texture bytes / nodes, not a separate ResourceStore).
/// A StaticImageNode with a `resource_id` causes the lease's texture budget to
/// be consumed; after expiry the tile removal drops those bytes.
///
/// Because the runtime's `ResourceStore` (ref-counting) is a separate trait with
/// no in-tree concrete implementation yet (see resource/mod.rs test harness), we
/// test the scene-layer resource tracking that is implemented: `ResourceUsage`
/// (tile count, node count, texture_bytes) which drops on tile removal.
#[test]
fn resource_ref_count_drops_after_lease_expiry() {
    use tze_hud_scene::types::{ImageFitMode, NodeData, ResourceId, StaticImageNode};

    let (mut scene, clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let lease_id = grant_lease(&mut scene, "resource-agent");
    let tile_id = create_tile(&mut scene, tab_id, "resource-agent", lease_id);

    // Set a tile root with a StaticImageNode referencing a resource.
    // The resource_id is a placeholder (48×48 PNG; 48×48×4 = 9216 bytes decoded).
    let resource_id = ResourceId::from_bytes([0xAB; 32]);
    scene.register_resource(resource_id);
    let node = tze_hud_scene::types::Node {
        layout: Default::default(),
        id: SceneId::new(),
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 48,
            height: 48,
            bounds: Rect::new(16.0, 16.0, 48.0, 48.0),
            fit_mode: ImageFitMode::Contain,
            decoded_bytes: 9216, // 48×48×4 RGBA8
        }),
        children: vec![],
    };
    scene
        .set_tile_root_checked(tile_id, node, "resource-agent")
        .unwrap();

    // Resource usage before expiry: 1 tile, 1 node, some texture bytes.
    let usage_before = scene.lease_resource_usage(&lease_id);
    assert_eq!(usage_before.tiles, 1, "lease should reference 1 tile");
    assert!(
        usage_before.texture_bytes > 0,
        "lease should have texture bytes from StaticImageNode"
    );

    // Disconnect and expire past grace.
    clock.advance(500);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();
    clock.advance(31_000); // past 30s grace
    scene.expire_leases();

    // Tile must be gone.
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "tile must be removed after expiry"
    );

    // Resource usage after expiry: tile is gone so tile count = 0.
    // (If the lease entry is still in the map, its usage drops to 0.)
    let usage_after = scene.lease_resource_usage(&lease_id);
    assert_eq!(
        usage_after.tiles, 0,
        "lease resource usage (tiles) must drop to 0 after tile removal"
    );
    assert_eq!(
        usage_after.texture_bytes, 0,
        "lease resource usage (texture bytes) must drop to 0 after tile removal"
    );
}

/// TTL expiry without reconnect also removes tile and drops resource usage.
#[test]
fn ttl_expiry_without_renewal_removes_tile() {
    use tze_hud_scene::types::NodeData;

    let (mut scene, clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let lease_id = grant_lease(&mut scene, "ttl-agent");
    let tile_id = create_tile(&mut scene, tab_id, "ttl-agent", lease_id);

    let node = tze_hud_scene::types::Node {
        layout: Default::default(),
        id: SceneId::new(),
        data: NodeData::SolidColor(tze_hud_scene::types::SolidColorNode {
            color: tze_hud_scene::types::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            radius: None,
        }),
        children: vec![],
    };
    scene
        .set_tile_root_checked(tile_id, node, "ttl-agent")
        .unwrap();

    // Advance past the full 60-second TTL (no disconnect, no renewal).
    clock.advance(61_000);
    let expiries = scene.expire_leases();

    assert!(
        expiries.iter().any(|e| e.lease_id == lease_id),
        "expire_leases must return an expiry for the TTL-elapsed lease"
    );
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "tile must be removed after TTL expiry without renewal"
    );
}

// ─── Scenario 11.1: Cross-namespace mutation rejection ───────────────────────

/// spec.md §Requirement: Full Lifecycle User-Test Scenario —
/// Scenario: Namespace isolation during lifecycle
///
/// WHEN a second agent session attempts to mutate the dashboard tile
/// THEN the scene graph SHALL reject the mutation with `NamespaceMismatch`.
///
/// This test directly exercises the scene graph's namespace isolation enforcement
/// in `get_tile_lease_checked` / `set_tile_root_checked` / `update_tile_opacity`.
#[test]
fn second_agent_cannot_mutate_dashboard_tile() {
    use tze_hud_scene::types::NodeData;

    let (mut scene, _clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    // Dashboard agent creates its tile.
    let dashboard_lease = grant_lease(&mut scene, "dashboard-agent");
    let tile_id = create_tile(&mut scene, tab_id, "dashboard-agent", dashboard_lease);

    // Second agent has its own lease.
    let second_lease = scene.grant_lease("intruder-agent", 60_000);
    let _ = second_lease; // has a lease but for a different namespace

    // Attempt: intruder-agent tries to set_tile_root on dashboard-agent's tile.
    let node = tze_hud_scene::types::Node {
        layout: Default::default(),
        id: SceneId::new(),
        data: NodeData::SolidColor(tze_hud_scene::types::SolidColorNode {
            color: tze_hud_scene::types::Rgba {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            radius: None,
        }),
        children: vec![],
    };
    let result = scene.set_tile_root_checked(tile_id, node, "intruder-agent");
    assert!(
        matches!(result, Err(ValidationError::NamespaceMismatch { .. })),
        "second agent must be rejected with NamespaceMismatch when mutating dashboard tile, \
         got: {result:?}"
    );
}

/// Second agent also cannot delete the dashboard tile (via `delete_tile`).
#[test]
fn second_agent_cannot_delete_dashboard_tile() {
    let (mut scene, _clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let dashboard_lease = grant_lease(&mut scene, "dashboard-agent");
    let tile_id = create_tile(&mut scene, tab_id, "dashboard-agent", dashboard_lease);

    // Intruder attempts to delete the tile through a namespace-checked path.
    // `update_tile_opacity` enforces namespace isolation and is a representative
    // example of a mutation that requires namespace ownership.
    let result = scene.update_tile_opacity(tile_id, 0.5, "intruder-agent");
    assert!(
        matches!(result, Err(ValidationError::NamespaceMismatch { .. })),
        "second agent must be rejected with NamespaceMismatch when updating dashboard tile opacity, \
         got: {result:?}"
    );
}

/// Cross-namespace CreateTile: agent cannot create a tile in another namespace.
///
/// When an agent submits `CreateTile` with their lease_id but specifies a
/// namespace that differs from the lease's namespace, the scene graph rejects
/// with `NamespaceMismatch`.
#[test]
fn agent_cannot_create_tile_in_foreign_namespace() {
    let (mut scene, _clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    // dashboard-agent has a lease registered under "dashboard-agent".
    let dashboard_lease = grant_lease(&mut scene, "dashboard-agent");

    // Attempt to create a tile in "other-agent"'s namespace using the dashboard lease.
    // The scene graph checks that the caller's stated namespace matches the lease namespace.
    let result = scene.create_tile_checked(
        tab_id,
        "other-agent", // claiming to be a different namespace
        dashboard_lease,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        50,
    );
    assert!(
        matches!(result, Err(ValidationError::NamespaceMismatch { .. })),
        "agent must not create tile in foreign namespace, got: {result:?}"
    );
}

// ─── Scenario 11.2: Dashboard agent cannot mutate other namespace tiles ───────

/// spec.md §11.2: dashboard agent cannot mutate tiles owned by another namespace.
///
/// WHEN the dashboard agent tries to mutate a tile owned by another agent's
/// namespace THEN the scene graph SHALL reject with `NamespaceMismatch`.
#[test]
fn dashboard_agent_cannot_mutate_other_namespace_tiles() {
    use tze_hud_scene::types::NodeData;

    let (mut scene, _clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    // "Other agent" creates its tile.
    let other_lease = scene.grant_lease("other-agent", 60_000);
    let other_tile = create_tile(&mut scene, tab_id, "other-agent", other_lease);

    // Dashboard agent has its own lease.
    let _dashboard_lease = grant_lease(&mut scene, "dashboard-agent");

    // Dashboard agent attempts to mutate other-agent's tile.
    let node = tze_hud_scene::types::Node {
        layout: Default::default(),
        id: SceneId::new(),
        data: NodeData::SolidColor(tze_hud_scene::types::SolidColorNode {
            color: tze_hud_scene::types::Rgba {
                r: 0.0,
                g: 1.0,
                b: 0.0,
                a: 1.0,
            },
            bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
            radius: None,
        }),
        children: vec![],
    };
    let result = scene.set_tile_root_checked(other_tile, node, "dashboard-agent");
    assert!(
        matches!(result, Err(ValidationError::NamespaceMismatch { .. })),
        "dashboard agent must be rejected with NamespaceMismatch when mutating other agent's tile, \
         got: {result:?}"
    );
}

/// Dashboard agent cannot update opacity of another agent's tile.
#[test]
fn dashboard_agent_cannot_update_opacity_of_other_tile() {
    let (mut scene, _clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let other_lease = scene.grant_lease("other-agent", 60_000);
    let other_tile = create_tile(&mut scene, tab_id, "other-agent", other_lease);

    let _dashboard_lease = grant_lease(&mut scene, "dashboard-agent");

    let result = scene.update_tile_opacity(other_tile, 0.5, "dashboard-agent");
    assert!(
        matches!(result, Err(ValidationError::NamespaceMismatch { .. })),
        "dashboard agent must be rejected when updating opacity of another agent's tile, \
         got: {result:?}"
    );
}

/// Dashboard agent cannot update input mode of another agent's tile.
#[test]
fn dashboard_agent_cannot_update_input_mode_of_other_tile() {
    let (mut scene, _clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let other_lease = scene.grant_lease("other-agent", 60_000);
    let other_tile = create_tile(&mut scene, tab_id, "other-agent", other_lease);

    let _dashboard_lease = grant_lease(&mut scene, "dashboard-agent");

    let result = scene.update_tile_input_mode(other_tile, InputMode::Capture, "dashboard-agent");
    assert!(
        matches!(result, Err(ValidationError::NamespaceMismatch { .. })),
        "dashboard agent must be rejected when updating input mode of another agent's tile, \
         got: {result:?}"
    );
}

// ─── Miscellaneous lifecycle edge cases ───────────────────────────────────────

/// TTL continues running while lease is ORPHANED.
///
/// Spec line 133: "TTL continues running during orphan state."
/// If the lease TTL elapses while orphaned (even before grace expires),
/// `expire_leases` must still clean up the tile.
#[test]
fn ttl_expires_while_orphaned_removes_tile() {
    // Use a short 5-second TTL to keep the test fast.
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(800.0, 600.0, clock.clone());
    let tab_id = scene.create_tab("Tab", 0).unwrap();
    scene.switch_active_tab(tab_id).unwrap();

    // Short 5s TTL lease.
    let lease_id = scene.grant_lease("short-ttl-agent", 5_000);
    let tile_id = create_tile(&mut scene, tab_id, "short-ttl-agent", lease_id);

    // Disconnect at t=2_000ms (2s into the 5s TTL).
    clock.advance(2_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);

    // Advance to t=6_000ms — TTL has elapsed (5s), grace still running.
    clock.advance(4_000); // total 6_000ms > 5_000ms TTL
    let expiries = scene.expire_leases();

    assert!(
        expiries.iter().any(|e| e.lease_id == lease_id),
        "TTL expiry while orphaned must trigger expire_leases"
    );
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "tile must be removed when TTL expires while lease is ORPHANED"
    );
}

/// Reconnect after grace period expiry fails.
///
/// Once the grace period has elapsed, `reconnect_lease` must not succeed.
#[test]
fn reconnect_after_grace_period_fails() {
    let (mut scene, clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);
    let _ = tab_id;

    let lease_id = grant_lease(&mut scene, "late-agent");
    clock.advance(1_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();

    // Advance past the 30-second grace period.
    clock.advance(31_000);

    // Attempt to reconnect — must fail.
    let result = scene.reconnect_lease(&lease_id, clock.now_millis());
    assert!(
        result.is_err(),
        "reconnect after grace expiry must fail: {result:?}"
    );
}

/// Disconnect from non-Active state is rejected.
#[test]
fn disconnect_from_non_active_state_is_rejected() {
    let (mut scene, clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);
    let _ = tab_id;

    let lease_id = grant_lease(&mut scene, "test-agent");
    clock.advance(1_000);
    let now_ms = clock.now_millis();

    // First disconnect succeeds.
    scene.disconnect_lease(&lease_id, now_ms).unwrap();
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);

    // Second disconnect from ORPHANED must fail.
    clock.advance(100);
    let result = scene.disconnect_lease(&lease_id, clock.now_millis());
    assert!(
        result.is_err(),
        "disconnect from ORPHANED state must fail (must be ACTIVE)"
    );
}

/// After explicit lease release, a mutation against the old lease is rejected.
#[test]
fn mutation_after_release_is_rejected() {
    use tze_hud_scene::types::NodeData;

    let (mut scene, _clock) = make_scene(0);
    let tab_id = setup_active_tab(&mut scene);

    let lease_id = grant_lease(&mut scene, "release-agent");
    let tile_id = create_tile(&mut scene, tab_id, "release-agent", lease_id);

    // Release the lease.
    scene.revoke_lease(lease_id).unwrap();

    // Attempt to mutate the tile after release — should fail.
    let node = tze_hud_scene::types::Node {
        layout: Default::default(),
        id: SceneId::new(),
        data: NodeData::SolidColor(tze_hud_scene::types::SolidColorNode {
            color: tze_hud_scene::types::Rgba {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            radius: None,
        }),
        children: vec![],
    };
    // set_tile_root_checked uses tile lookup which checks namespace isolation.
    // The tile itself no longer exists, so expect NotFound or similar.
    let result = scene.set_tile_root_checked(tile_id, node, "release-agent");
    assert!(
        result.is_err(),
        "mutation after lease release must be rejected (tile no longer exists)"
    );
}
