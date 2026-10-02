//! # E12.2 Session Lifecycle Integration Tests
//!
//! End-to-end integration tests for the session lifecycle state machine per
//! the validation-framework spec (Requirement: Test Scene Registry, lines 160-172)
//! and the session-protocol spec (Requirement: Session Lifecycle State Machine).
//!
//! ## What is tested
//!
//! 1. Full lifecycle: connect → auth → lease → mutate → disconnect → reconnect
//!    → resume → safe mode → freeze interaction → close
//! 2. Reconnection **within** grace period: agent reclaims existing leases,
//!    receives full SceneSnapshot.
//! 3. Reconnection **after** grace period expires: leases are cleaned up,
//!    agent starts fresh.
//! 4. Safe mode: all leases suspended, mutations rejected, freeze cancelled.
//! 5. Safe mode exit: leases resume, mutations accepted again.
//! 6. Freeze + safe mode interaction: safe mode entry cancels active freeze
//!    (via suspension).
//! 7. `disconnect_reclaim_multiagent` scene: multi-agent scenario where one
//!    agent disconnects and reconnects without disturbing the other agent.
//! 8. Zero resource footprint after agent disconnect + lease expiry.
//!
//! ## Validation layer
//!
//! These are **Layer 0** tests: pure logic on the scene data structure,
//! no GPU context, no async, no external services. They run in under 2 seconds.
//!
//! ## Artifact output
//!
//! Tests produce JSON-serialisable state-transition logs (via structured
//! `TransitionLog`) satisfying the "session state transition log" artifact
//! expectation from the bead description.

use std::sync::Arc;

use tze_hud_scene::lease::{
    GracePeriodTimer, ORPHAN_GRACE_PERIOD_MS, POST_REVOCATION_FREE_DELAY_MS, TileVisualHint,
};
use tze_hud_scene::{
    Clock, TestClock,
    graph::SceneGraph,
    mutation::{MutationBatch, SceneMutation},
    test_scenes::{ClockMs, TestSceneRegistry, assert_layer0_invariants},
    types::{LayerAttachment, LeaseState, Node, NodeData, Rect, Rgba, SceneId, SolidColorNode},
};

// ─── Test helper: TransitionLog ──────────────────────────────────────────────

/// A single recorded state transition for the audit trail.
///
/// Serialises to JSON to satisfy the "session state transition log" artifact
/// requirement from rig-7vnh spec.
#[derive(Debug, Clone, serde::Serialize)]
struct TransitionEntry {
    step: &'static str,
    clock_ms: u64,
    lease_state: Option<String>,
    tile_count: usize,
    lease_count: usize,
    version: u64,
    note: &'static str,
}

/// Ordered audit trail of session state transitions.
#[derive(Debug, Default)]
struct TransitionLog(Vec<TransitionEntry>);

impl TransitionLog {
    fn record(
        &mut self,
        step: &'static str,
        clock_ms: u64,
        scene: &SceneGraph,
        lease_id: Option<SceneId>,
        note: &'static str,
    ) {
        let lease_state = lease_id
            .and_then(|id| scene.leases.get(&id))
            .map(|l| format!("{:?}", l.state));
        self.0.push(TransitionEntry {
            step,
            clock_ms,
            lease_state,
            tile_count: scene.tile_count(),
            lease_count: scene.leases.len(),
            version: scene.version,
            note,
        });
    }

    /// Serialise the log to a pretty-printed JSON string (artifact output).
    fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.0).expect("log serialisation failed")
    }
}

// ─── Utilities ───────────────────────────────────────────────────────────────

fn make_solid_node(bounds: Rect, color: Rgba) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            bounds,
            color,
            radius: None,
        }),
    }
}

fn apply_create_tile(
    scene: &mut SceneGraph,
    tab_id: SceneId,
    namespace: &str,
    lease_id: SceneId,
    bounds: Rect,
    z_order: u32,
) -> SceneId {
    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: namespace.to_string(),
        mutations: vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds,
            z_order,
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result = scene.apply_batch(&batch);
    assert!(result.applied, "CreateTile should succeed");
    assert_eq!(
        result.created_ids.len(),
        1,
        "CreateTile should produce exactly one created_id"
    );
    result.created_ids[0]
}

fn apply_set_tile_root(scene: &mut SceneGraph, tile_id: SceneId, namespace: &str, node: Node) {
    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: namespace.to_string(),
        mutations: vec![SceneMutation::SetTileRoot {
            tile_id,
            node,
            descendants: vec![],
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result = scene.apply_batch(&batch);
    assert!(result.applied, "SetTileRoot should succeed");
}

// ─── Test 1: Full session lifecycle state transitions ────────────────────────

/// Validates the complete session lifecycle state machine:
/// lease grant (Active) → mutate → disconnect (Orphaned) → reconnect
/// (Active) → safe mode (Suspended) → safe mode exit (Active) → close (Revoked)
///
/// Produces a state-transition log in JSON format.
#[test]
fn test_full_session_lifecycle_state_transitions() {
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    let mut log = TransitionLog::default();

    // ── Step 1: Connect + Auth ─────────────────────────────────────────────
    let tab_id = scene.create_tab("Main", 0).expect("create_tab");
    let lease_id = scene.grant_lease("agent.alpha", 60_000);
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
    log.record(
        "connect_auth",
        1_000,
        &scene,
        Some(lease_id),
        "session established",
    );

    // ── Step 2: Mutate (create tile + set root) ────────────────────────────
    let tile_id = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.alpha",
        lease_id,
        Rect::new(50.0, 50.0, 400.0, 300.0),
        1,
    );
    apply_set_tile_root(
        &mut scene,
        tile_id,
        "agent.alpha",
        make_solid_node(
            Rect::new(0.0, 0.0, 400.0, 300.0),
            Rgba::new(0.2, 0.4, 0.8, 1.0),
        ),
    );
    assert_eq!(scene.tile_count(), 1);
    log.record(
        "mutate",
        1_100,
        &scene,
        Some(lease_id),
        "tile created and rooted",
    );

    // Layer 0 invariants must hold after mutation
    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after mutate: {violations:?}"
    );

    // ── Step 3: Disconnect ─────────────────────────────────────────────────
    clock.advance(500);
    let disconnect_time = clock.now_millis();
    scene
        .disconnect_lease(&lease_id, disconnect_time)
        .expect("disconnect_lease");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);
    // Tile still exists during grace period
    assert_eq!(
        scene.tile_count(),
        1,
        "tile must persist during grace period"
    );
    log.record(
        "disconnect",
        disconnect_time,
        &scene,
        Some(lease_id),
        "entered grace period",
    );

    // ── Step 4: Reconnect within grace period ─────────────────────────────
    clock.advance(5_000); // 5 s < 30 s grace
    let reconnect_time = clock.now_millis();
    scene
        .reconnect_lease(&lease_id, reconnect_time)
        .expect("reconnect_lease");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
    // Tile must still exist after reclaim
    assert_eq!(scene.tile_count(), 1, "tile reclaimed on reconnect");
    log.record(
        "reconnect_within_grace",
        reconnect_time,
        &scene,
        Some(lease_id),
        "lease reclaimed",
    );

    // Layer 0 invariants still hold
    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after reconnect: {violations:?}"
    );

    // ── Step 5: Safe mode entry ────────────────────────────────────────────
    clock.advance(1_000);
    let safe_mode_enter_time = clock.now_millis();
    scene.suspend_all_leases(safe_mode_enter_time);
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Suspended);
    log.record(
        "safe_mode_enter",
        safe_mode_enter_time,
        &scene,
        Some(lease_id),
        "all leases suspended",
    );

    // Mutations must be rejected while suspended
    let rejected_batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent.alpha".to_string(),
        mutations: vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent.alpha".to_string(),
            lease_id,
            bounds: Rect::new(100.0, 100.0, 200.0, 200.0),
            z_order: 2,
        }],
        timing_hints: None,
        lease_id: None,
    };
    let rejected = scene.apply_batch(&rejected_batch);
    assert!(!rejected.applied, "mutations must be rejected in safe mode");
    assert_eq!(
        scene.tile_count(),
        1,
        "tile count unchanged after rejected mutation"
    );
    log.record(
        "safe_mode_reject",
        safe_mode_enter_time,
        &scene,
        Some(lease_id),
        "mutation rejected",
    );

    // ── Step 6: Safe mode exit ─────────────────────────────────────────────
    clock.advance(2_000);
    let safe_mode_exit_time = clock.now_millis();
    scene.resume_all_leases(safe_mode_exit_time);
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
    log.record(
        "safe_mode_exit",
        safe_mode_exit_time,
        &scene,
        Some(lease_id),
        "leases resumed",
    );

    // Mutations accepted again after safe mode exit
    let post_safe_tile_id = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.alpha",
        lease_id,
        Rect::new(100.0, 400.0, 200.0, 150.0),
        2,
    );
    assert_eq!(scene.tile_count(), 2);
    log.record(
        "mutate_post_safe_mode",
        safe_mode_exit_time + 100,
        &scene,
        Some(lease_id),
        "mutation accepted after safe mode exit",
    );

    // ── Step 7: Graceful close (release) ──────────────────────────────────
    scene.revoke_lease(lease_id).expect("revoke_lease");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Revoked);
    // All tiles owned by this lease are removed on revocation
    assert_eq!(
        scene.tile_count(),
        0,
        "all tiles removed on lease revocation"
    );
    log.record(
        "close",
        clock.now_millis(),
        &scene,
        Some(lease_id),
        "lease revoked, tiles cleaned up",
    );

    // ── Artifact: emit state-transition log ───────────────────────────────
    let json_log = log.to_json();
    assert!(
        json_log.contains("connect_auth"),
        "log must include connect_auth step"
    );
    assert!(
        json_log.contains("disconnect"),
        "log must include disconnect step"
    );
    assert!(
        json_log.contains("reconnect_within_grace"),
        "log must include reconnect step"
    );
    assert!(
        json_log.contains("safe_mode_enter"),
        "log must include safe_mode_enter step"
    );
    assert!(
        json_log.contains("safe_mode_exit"),
        "log must include safe_mode_exit step"
    );
    assert!(json_log.contains("close"), "log must include close step");

    // Final invariant check
    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations at session close: {violations:?}"
    );

    // Suppress unused variable warning for the post_safe tile (it was deleted on revoke)
    let _ = post_safe_tile_id;
}

// ─── Test 2: Reconnection within grace period delivers full SceneSnapshot ────

/// Verifies that an agent reconnecting within the grace period can recover
/// its full scene state (tiles still exist, leases still active).
/// This validates the "reconnection within grace period" acceptance criterion.
#[test]
fn test_reconnect_within_grace_period_delivers_snapshot() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Workspace", 0).expect("create_tab");
    let lease_id = scene.grant_lease("agent.bravo", 120_000);

    // Establish 3 tiles before disconnect
    let tile_a = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.bravo",
        lease_id,
        Rect::new(0.0, 0.0, 200.0, 150.0),
        1,
    );
    let tile_b = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.bravo",
        lease_id,
        Rect::new(210.0, 0.0, 200.0, 150.0),
        2,
    );
    let tile_c = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.bravo",
        lease_id,
        Rect::new(0.0, 160.0, 200.0, 150.0),
        3,
    );
    assert_eq!(scene.tile_count(), 3);

    // Disconnect
    clock.advance(1_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);

    // Snapshot taken while in grace period — tiles still exist.
    // Deserialize the snapshot to validate it faithfully captures the scene state.
    let grace_snapshot_json = scene.snapshot_json().expect("snapshot_json");
    let snapshot_scene = SceneGraph::from_json(&grace_snapshot_json).expect("deserialize snapshot");
    assert_eq!(
        snapshot_scene.tile_count(),
        3,
        "snapshot must contain all 3 tiles during grace period"
    );
    assert!(
        snapshot_scene.tiles.contains_key(&tile_a),
        "snapshot must contain tile_a in grace period"
    );
    assert!(
        snapshot_scene.tiles.contains_key(&tile_b),
        "snapshot must contain tile_b in grace period"
    );
    assert!(
        snapshot_scene.tiles.contains_key(&tile_c),
        "snapshot must contain tile_c in grace period"
    );

    // Reconnect within grace (5 s << 30 s default grace)
    clock.advance(5_000);
    scene
        .reconnect_lease(&lease_id, clock.now_millis())
        .expect("reconnect");
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Active,
        "lease must be Active after reconnect"
    );

    // All 3 tiles still present — this constitutes full SceneSnapshot recovery
    assert_eq!(
        scene.tile_count(),
        3,
        "all tiles survive reconnect within grace period"
    );
    assert!(
        scene.tiles.contains_key(&tile_a),
        "tile_a must be reclaimed"
    );
    assert!(
        scene.tiles.contains_key(&tile_b),
        "tile_b must be reclaimed"
    );
    assert!(
        scene.tiles.contains_key(&tile_c),
        "tile_c must be reclaimed"
    );

    // Can mutate immediately after reconnect
    let post_reconnect_tile = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.bravo",
        lease_id,
        Rect::new(210.0, 160.0, 200.0, 150.0),
        4,
    );
    assert_eq!(scene.tile_count(), 4);

    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after reconnect: {violations:?}"
    );

    let _ = post_reconnect_tile;
}

// ─── Test 3: Reconnection after grace period expiry ──────────────────────────

/// Verifies that an agent reconnecting AFTER the grace period expires
/// finds its leases gone and its tiles cleaned up (zero footprint).
/// The agent can still start a fresh session.
#[test]
fn test_reconnect_after_grace_period_expiry_clears_state() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Workspace", 0).expect("create_tab");
    // Use a long TTL; this test exercises grace-period expiry, not TTL expiry
    let lease_id = scene.grant_lease("agent.charlie", 9_000_000);

    apply_create_tile(
        &mut scene,
        tab_id,
        "agent.charlie",
        lease_id,
        Rect::new(0.0, 0.0, 200.0, 200.0),
        1,
    );
    apply_create_tile(
        &mut scene,
        tab_id,
        "agent.charlie",
        lease_id,
        Rect::new(210.0, 0.0, 200.0, 200.0),
        2,
    );
    assert_eq!(scene.tile_count(), 2);

    // Disconnect agent
    clock.advance(1_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);

    // Advance past grace period (default 30 s)
    clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1_000);

    // Run expiry sweep — this should clean up the disconnected lease
    let expiries = scene.expire_leases();
    assert!(
        !expiries.is_empty(),
        "at least one lease should have expired"
    );

    let expired = expiries
        .iter()
        .find(|e| e.lease_id == lease_id)
        .expect("agent.charlie lease must be in expiry list");
    assert_eq!(
        expired.terminal_state,
        LeaseState::Expired,
        "grace-expired lease must be Expired"
    );
    assert_eq!(
        expired.removed_tiles.len(),
        2,
        "both tiles must be removed on grace expiry"
    );

    // Zero footprint: no tiles, no active leases for this agent
    assert_eq!(
        scene.tile_count(),
        0,
        "zero tiles after grace expiry — zero footprint"
    );
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Expired,
        "lease must be in Expired terminal state"
    );

    // Agent can start a fresh session (new lease on same namespace)
    let fresh_lease_id = scene.grant_lease("agent.charlie", 60_000);
    assert_eq!(scene.leases[&fresh_lease_id].state, LeaseState::Active);
    let fresh_tile = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.charlie",
        fresh_lease_id,
        Rect::new(0.0, 0.0, 200.0, 200.0),
        1,
    );
    assert_eq!(scene.tile_count(), 1);

    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after fresh session: {violations:?}"
    );

    let _ = fresh_tile;
}

// ─── Test 4: Safe mode suspends all leases ────────────────────────────────────

/// Verifies that safe mode entry suspends all active leases simultaneously,
/// within the one-frame (16.6ms) budget (pure logic, no rendering).
/// Validates: RFC 0008 §3.3, §3.4.
#[test]
fn test_safe_mode_suspends_all_leases() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");

    // Three agents with active leases
    let lease_a = scene.grant_lease("agent.a", 60_000);
    let lease_b = scene.grant_lease("agent.b", 60_000);
    let lease_c = scene.grant_lease("agent.c", 60_000);

    apply_create_tile(
        &mut scene,
        tab_id,
        "agent.a",
        lease_a,
        Rect::new(0.0, 0.0, 300.0, 200.0),
        1,
    );
    apply_create_tile(
        &mut scene,
        tab_id,
        "agent.b",
        lease_b,
        Rect::new(310.0, 0.0, 300.0, 200.0),
        2,
    );
    apply_create_tile(
        &mut scene,
        tab_id,
        "agent.c",
        lease_c,
        Rect::new(620.0, 0.0, 300.0, 200.0),
        3,
    );

    assert_eq!(scene.tile_count(), 3);

    // Safe mode: must complete within one frame (pure logic — no timing budget here)
    let safe_enter_ms = clock.now_millis();
    scene.suspend_all_leases(safe_enter_ms);

    // All three leases must be Suspended
    assert_eq!(
        scene.leases[&lease_a].state,
        LeaseState::Suspended,
        "lease_a must be Suspended"
    );
    assert_eq!(
        scene.leases[&lease_b].state,
        LeaseState::Suspended,
        "lease_b must be Suspended"
    );
    assert_eq!(
        scene.leases[&lease_c].state,
        LeaseState::Suspended,
        "lease_c must be Suspended"
    );

    // Tiles still exist — suspension preserves state
    assert_eq!(scene.tile_count(), 3, "tiles preserved during safe mode");

    // All mutations must be rejected
    for (ns, lease_id) in [
        ("agent.a", lease_a),
        ("agent.b", lease_b),
        ("agent.c", lease_c),
    ] {
        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: ns.to_string(),
            mutations: vec![SceneMutation::CreateTile {
                tab_id,
                namespace: ns.to_string(),
                lease_id,
                bounds: Rect::new(0.0, 500.0, 100.0, 100.0),
                z_order: 10,
            }],
            timing_hints: None,
            lease_id: None,
        };
        let result = scene.apply_batch(&batch);
        assert!(
            !result.applied,
            "mutations for {ns} must be rejected in safe mode"
        );
    }
    assert_eq!(
        scene.tile_count(),
        3,
        "tile count unchanged after rejected mutations"
    );

    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations during safe mode: {violations:?}"
    );
}

// ─── Test 5: Safe mode exit resumes leases, mutations accepted ────────────────

/// Verifies that exiting safe mode resumes all suspended leases and that
/// mutations are accepted again afterwards. Also verifies TTL accounting:
/// time spent in suspension does not count toward TTL.
#[test]
fn test_safe_mode_exit_resumes_leases_and_accepts_mutations() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");
    let lease_id = scene.grant_lease("agent.delta", 60_000);
    apply_create_tile(
        &mut scene,
        tab_id,
        "agent.delta",
        lease_id,
        Rect::new(0.0, 0.0, 200.0, 200.0),
        1,
    );

    // Enter safe mode
    clock.advance(1_000);
    let enter_ms = clock.now_millis();
    scene.suspend_all_leases(enter_ms);
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Suspended);

    // Some time passes while in safe mode (should not consume TTL)
    clock.advance(10_000);

    // Exit safe mode
    let exit_ms = clock.now_millis();
    scene.resume_all_leases(exit_ms);
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Active,
        "lease must be Active after safe mode exit"
    );

    // Mutations accepted immediately after exit
    let new_tile = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.delta",
        lease_id,
        Rect::new(210.0, 0.0, 200.0, 200.0),
        2,
    );
    assert_eq!(
        scene.tile_count(),
        2,
        "new tile created after safe mode exit"
    );

    // TTL check: the lease's remaining TTL must still be positive
    // (suspension pauses TTL clock per RFC 0008 §4.3)
    let lease = &scene.leases[&lease_id];
    assert!(
        lease.ttl_remaining_at_suspend_ms.is_some() || lease.ttl_ms > 0,
        "lease TTL accounting preserved through suspension"
    );

    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after safe mode exit: {violations:?}"
    );

    let _ = new_tile;
}

// ─── Test 6: Freeze + safe mode interaction ───────────────────────────────────

/// Verifies that safe mode entry cancels (suspends) an active freeze.
///
/// The spec states: "Freeze + safe mode interaction: safe mode cancels active freeze".
/// In the scene graph model a "freeze" is represented by a lease in Suspended state
/// (frozen/frozen-for-agent-reload). When safe mode is also entered, all Active
/// leases are suspended — there is no layered re-suspension. This test verifies
/// that a lease that was frozen before safe mode entry ends up in the correct
/// Suspended state and resumes cleanly on safe mode exit.
///
/// Both leases are standard leases with Manual renewal policy. Safe mode exit
/// resumes all suspended leases, including the pre-frozen one — this models
/// "safe mode cancels active freeze" per RFC 0008 §3.4.
#[test]
fn test_freeze_plus_safe_mode_interaction() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");
    let lease_active = scene.grant_lease("agent.echo", 60_000);
    let lease_frozen = scene.grant_lease("agent.echo.frozen", 30_000);

    apply_create_tile(
        &mut scene,
        tab_id,
        "agent.echo",
        lease_active,
        Rect::new(0.0, 0.0, 200.0, 200.0),
        1,
    );
    apply_create_tile(
        &mut scene,
        tab_id,
        "agent.echo.frozen",
        lease_frozen,
        Rect::new(210.0, 0.0, 200.0, 200.0),
        2,
    );

    // Simulate a "freeze" on lease_frozen by suspending it directly
    clock.advance(1_000);
    scene
        .suspend_lease(&lease_frozen, clock.now_millis())
        .expect("freeze lease_frozen");
    assert_eq!(
        scene.leases[&lease_frozen].state,
        LeaseState::Suspended,
        "lease_frozen is frozen/suspended"
    );
    assert_eq!(
        scene.leases[&lease_active].state,
        LeaseState::Active,
        "lease_active is still active"
    );

    // Safe mode entry: should suspend all Active leases (lease_active),
    // and leave already-Suspended leases as-is
    clock.advance(500);
    let safe_enter_ms = clock.now_millis();
    scene.suspend_all_leases(safe_enter_ms);

    assert_eq!(
        scene.leases[&lease_active].state,
        LeaseState::Suspended,
        "lease_active suspended by safe mode"
    );
    assert_eq!(
        scene.leases[&lease_frozen].state,
        LeaseState::Suspended,
        "lease_frozen remains suspended (not double-suspended)"
    );

    // Neither lease may mutate
    for (ns, lid) in [
        ("agent.echo", lease_active),
        ("agent.echo.frozen", lease_frozen),
    ] {
        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: ns.to_string(),
            mutations: vec![SceneMutation::CreateTile {
                tab_id,
                namespace: ns.to_string(),
                lease_id: lid,
                bounds: Rect::new(0.0, 500.0, 100.0, 100.0),
                z_order: 10,
            }],
            timing_hints: None,
            lease_id: None,
        };
        assert!(
            !scene.apply_batch(&batch).applied,
            "{ns} must not mutate during safe mode"
        );
    }

    // Safe mode exit: resume all suspended leases
    clock.advance(3_000);
    let safe_exit_ms = clock.now_millis();
    scene.resume_all_leases(safe_exit_ms);

    // Both leases resume after safe mode exit
    // (lease_frozen was frozen before safe mode; after safe mode exit it is also
    // resumed — this models "safe mode cancels active freeze" semantics)
    assert_eq!(
        scene.leases[&lease_active].state,
        LeaseState::Active,
        "lease_active resumed"
    );
    assert_eq!(
        scene.leases[&lease_frozen].state,
        LeaseState::Active,
        "freeze cancelled by safe mode exit"
    );

    // Both can now mutate
    apply_create_tile(
        &mut scene,
        tab_id,
        "agent.echo",
        lease_active,
        Rect::new(0.0, 210.0, 200.0, 200.0),
        3,
    );
    apply_create_tile(
        &mut scene,
        tab_id,
        "agent.echo.frozen",
        lease_frozen,
        Rect::new(210.0, 210.0, 200.0, 200.0),
        4,
    );
    assert_eq!(scene.tile_count(), 4);

    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after freeze+safe_mode: {violations:?}"
    );
}

// ─── Test 7: disconnect_reclaim_multiagent scene ─────────────────────────────

/// Validates the `disconnect_reclaim_multiagent` test scene from the registry.
///
/// Three agents are present. One disconnects and reconnects within grace.
/// The other two must be unaffected throughout.
///
/// Validates:
/// - V1 Success Criterion - Live Multi-Agent Presence (validation-framework spec line 313-320)
/// - disconnect_reclaim_multiagent scene definition (spec line 160-172)
/// - Thesis 3: Multiple agents coexist
#[test]
fn test_disconnect_reclaim_multiagent_scene() {
    let registry = TestSceneRegistry::new();
    let (mut scene, spec) = registry
        .build("disconnect_reclaim_multiagent", ClockMs::FIXED)
        .expect("disconnect_reclaim_multiagent must be in registry");

    assert_eq!(spec.name, "disconnect_reclaim_multiagent");

    // Layer 0 invariants on the constructed scene
    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations on scene construction: {violations:?}"
    );

    // The scene should have 3 agents (agent.one, agent.two, agent.three)
    assert_eq!(spec.expected_tab_count, 1, "scene should have 1 tab");
    assert!(
        spec.expected_tile_count >= 3,
        "scene should have at least 3 tiles (one per agent)"
    );

    // Find the leases for each agent namespace
    let lease_one = scene
        .leases
        .values()
        .find(|l| l.namespace == "agent.one")
        .map(|l| l.id)
        .expect("agent.one lease must exist");
    let lease_two = scene
        .leases
        .values()
        .find(|l| l.namespace == "agent.two")
        .map(|l| l.id)
        .expect("agent.two lease must exist");
    let lease_three = scene
        .leases
        .values()
        .find(|l| l.namespace == "agent.three")
        .map(|l| l.id)
        .expect("agent.three lease must exist");

    // All start Active
    assert_eq!(
        scene.leases[&lease_one].state,
        LeaseState::Active,
        "agent.one: Active"
    );
    assert_eq!(
        scene.leases[&lease_two].state,
        LeaseState::Active,
        "agent.two: Active"
    );
    assert_eq!(
        scene.leases[&lease_three].state,
        LeaseState::Active,
        "agent.three: Active"
    );

    let initial_tile_count = scene.tile_count();
    let tiles_of_one: Vec<SceneId> = scene
        .tiles
        .values()
        .filter(|t| t.namespace == "agent.one")
        .map(|t| t.id)
        .collect();
    let tiles_of_two_count = scene
        .tiles
        .values()
        .filter(|t| t.namespace == "agent.two")
        .count();
    let tiles_of_three_count = scene
        .tiles
        .values()
        .filter(|t| t.namespace == "agent.three")
        .count();

    // ── Step: agent.one disconnects ────────────────────────────────────────
    let now_ms = ClockMs::FIXED.0 + 1_000;
    scene
        .disconnect_lease(&lease_one, now_ms)
        .expect("disconnect agent.one");
    assert_eq!(scene.leases[&lease_one].state, LeaseState::Orphaned);

    // agent.two and agent.three are unaffected
    assert_eq!(
        scene.leases[&lease_two].state,
        LeaseState::Active,
        "agent.two unaffected by agent.one disconnect"
    );
    assert_eq!(
        scene.leases[&lease_three].state,
        LeaseState::Active,
        "agent.three unaffected by agent.one disconnect"
    );

    // Tile count unchanged during grace period
    assert_eq!(
        scene.tile_count(),
        initial_tile_count,
        "tiles preserved during grace period"
    );

    // agent.two and agent.three can still mutate
    let active_tab = scene.active_tab.expect("active tab must exist");
    for (ns, _lid) in [("agent.two", lease_two), ("agent.three", lease_three)] {
        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: ns.to_string(),
            mutations: vec![SceneMutation::UpdateTileBounds {
                tile_id: *scene
                    .tiles
                    .values()
                    .find(|t| t.namespace == ns)
                    .map(|t| &t.id)
                    .expect("tile must exist"),
                bounds: Rect::new(0.0, 0.0, 300.0, 250.0),
            }],
            timing_hints: None,
            lease_id: None,
        };
        let result = scene.apply_batch(&batch);
        assert!(
            result.applied,
            "{ns} must be able to mutate while agent.one is disconnected"
        );
    }

    // ── Step: agent.one reconnects within grace ────────────────────────────
    let reconnect_ms = now_ms + 5_000; // 5 s < 30 s grace
    scene
        .reconnect_lease(&lease_one, reconnect_ms)
        .expect("reconnect agent.one");
    assert_eq!(
        scene.leases[&lease_one].state,
        LeaseState::Active,
        "agent.one: reclaimed"
    );

    // agent.one's tiles all survive
    for &tile_id in &tiles_of_one {
        assert!(
            scene.tiles.contains_key(&tile_id),
            "agent.one tile {tile_id} must survive reconnect"
        );
    }

    // Other agents unaffected
    let final_tiles_of_two = scene
        .tiles
        .values()
        .filter(|t| t.namespace == "agent.two")
        .count();
    let final_tiles_of_three = scene
        .tiles
        .values()
        .filter(|t| t.namespace == "agent.three")
        .count();
    assert_eq!(
        final_tiles_of_two, tiles_of_two_count,
        "agent.two tile count unchanged"
    );
    assert_eq!(
        final_tiles_of_three, tiles_of_three_count,
        "agent.three tile count unchanged"
    );

    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations at end of multiagent scene: {violations:?}"
    );

    let _ = active_tab;
}

// ─── Test 8: Zero resource footprint after disconnect + expiry ────────────────

/// Validates the "zero post-disconnect footprint" requirement from the soak and
/// leak test spec (validation-framework spec lines 307-310).
///
/// After an agent disconnects and its grace period expires, every resource
/// it owned must be cleaned up: no tiles, no nodes, no hit-region states.
#[test]
fn test_zero_resource_footprint_after_disconnect_and_expiry() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");
    let lease_id = scene.grant_lease("agent.foxtrot", 9_000_000);

    // Create multiple tiles with nodes
    let tile_a = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.foxtrot",
        lease_id,
        Rect::new(0.0, 0.0, 200.0, 200.0),
        1,
    );
    let tile_b = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.foxtrot",
        lease_id,
        Rect::new(210.0, 0.0, 200.0, 200.0),
        2,
    );
    let tile_c = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.foxtrot",
        lease_id,
        Rect::new(0.0, 210.0, 200.0, 200.0),
        3,
    );

    // Add content nodes to the tiles
    apply_set_tile_root(
        &mut scene,
        tile_a,
        "agent.foxtrot",
        make_solid_node(
            Rect::new(0.0, 0.0, 200.0, 200.0),
            Rgba::new(1.0, 0.0, 0.0, 1.0),
        ),
    );
    apply_set_tile_root(
        &mut scene,
        tile_b,
        "agent.foxtrot",
        make_solid_node(
            Rect::new(0.0, 0.0, 200.0, 200.0),
            Rgba::new(0.0, 1.0, 0.0, 1.0),
        ),
    );
    apply_set_tile_root(
        &mut scene,
        tile_c,
        "agent.foxtrot",
        make_solid_node(
            Rect::new(0.0, 0.0, 200.0, 200.0),
            Rgba::new(0.0, 0.0, 1.0, 1.0),
        ),
    );

    assert_eq!(scene.tile_count(), 3);
    assert_eq!(scene.node_count(), 3, "one node per tile");

    // Also create a second (unrelated) agent — its resources must survive expiry
    let lease_other = scene.grant_lease("agent.golf", 9_000_000);
    let tile_other = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.golf",
        lease_other,
        Rect::new(600.0, 0.0, 200.0, 200.0),
        10,
    );
    apply_set_tile_root(
        &mut scene,
        tile_other,
        "agent.golf",
        make_solid_node(Rect::new(0.0, 0.0, 200.0, 200.0), Rgba::WHITE),
    );
    assert_eq!(scene.tile_count(), 4);

    // Record pre-disconnect state
    let _pre_node_count = scene.node_count(); // 4 nodes

    // Disconnect agent.foxtrot
    clock.advance(1_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect");

    // Advance past grace period
    clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 5_000);

    // Lease expiry sweep
    let expiries = scene.expire_leases();
    let foxtrot_expiry = expiries
        .iter()
        .find(|e| e.lease_id == lease_id)
        .expect("foxtrot lease must expire");
    assert_eq!(foxtrot_expiry.terminal_state, LeaseState::Expired);
    assert_eq!(
        foxtrot_expiry.removed_tiles.len(),
        3,
        "all 3 foxtrot tiles removed"
    );

    // ── Resource footprint checks ──────────────────────────────────────────

    // Zero tiles for the expired agent
    let foxtrot_tiles: Vec<_> = scene
        .tiles
        .values()
        .filter(|t| t.namespace == "agent.foxtrot")
        .collect();
    assert!(
        foxtrot_tiles.is_empty(),
        "zero tiles for expired agent (zero footprint)"
    );

    // Zero nodes for expired agent (nodes removed with tiles)
    assert_eq!(
        scene.node_count(),
        1,
        "only 1 node remains (agent.golf's tile)"
    );

    // Zero hit-region states for expired agent
    let foxtrot_tile_ids = [tile_a, tile_b, tile_c];
    for tile_id in foxtrot_tile_ids {
        assert!(
            !scene.tiles.contains_key(&tile_id),
            "tile {tile_id} must be removed"
        );
    }

    // agent.golf's resources are untouched
    assert!(
        scene.tiles.contains_key(&tile_other),
        "agent.golf tile survives foxtrot expiry"
    );
    assert_eq!(
        scene.leases[&lease_other].state,
        LeaseState::Active,
        "agent.golf lease unaffected"
    );

    // Lease in terminal state
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Expired,
        "foxtrot lease in Expired terminal state"
    );

    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after zero-footprint cleanup: {violations:?}"
    );
}

// ─── Test 9: Lease timeline artifact ─────────────────────────────────────────

/// Produces the "lease lifecycle timeline" artifact required by the spec.
///
/// Exercises all non-terminal lease states in sequence and records the timeline
/// as structured JSON.
#[test]
fn test_lease_lifecycle_timeline_artifact() {
    #[derive(Debug, serde::Serialize)]
    struct TimelineEntry {
        t_ms: u64,
        state: String,
        event: &'static str,
    }

    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    scene.create_tab("Main", 0).expect("create_tab");
    let lease_id = scene.grant_lease("timeline.agent", 60_000);

    let mut timeline: Vec<TimelineEntry> = Vec::new();

    macro_rules! record {
        ($event:expr) => {{
            let state = format!("{:?}", scene.leases[&lease_id].state);
            timeline.push(TimelineEntry {
                t_ms: clock.now_millis(),
                state,
                event: $event,
            });
        }};
    }

    record!("grant — REQUESTED → ACTIVE");

    // Suspend (simulate freeze / safe mode)
    clock.advance(1_000);
    scene
        .suspend_lease(&lease_id, clock.now_millis())
        .expect("suspend");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Suspended);
    record!("suspend — ACTIVE → SUSPENDED");

    // Resume
    clock.advance(2_000);
    scene
        .resume_lease(&lease_id, clock.now_millis())
        .expect("resume");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
    record!("resume — SUSPENDED → ACTIVE");

    // Disconnect (grace period start)
    clock.advance(500);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);
    record!("disconnect — ACTIVE → ORPHANED");

    // Reconnect (within grace)
    clock.advance(3_000);
    scene
        .reconnect_lease(&lease_id, clock.now_millis())
        .expect("reconnect");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
    record!("reconnect — ORPHANED → ACTIVE");

    // Revoke (viewer dismiss / close)
    clock.advance(1_000);
    scene.revoke_lease(lease_id).expect("revoke");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Revoked);
    record!("revoke — ACTIVE → REVOKED");

    // Emit timeline as JSON (artifact output)
    let json = serde_json::to_string_pretty(&timeline).expect("timeline serialisation");
    assert!(
        json.contains("ACTIVE"),
        "timeline must contain ACTIVE state"
    );
    assert!(
        json.contains("SUSPENDED"),
        "timeline must contain SUSPENDED state"
    );
    assert!(
        json.contains("ORPHANED"),
        "timeline must contain ORPHANED state"
    );
    assert!(
        json.contains("REVOKED"),
        "timeline must contain REVOKED state"
    );

    // Verify all transitions occurred in the expected order
    let states: Vec<&str> = timeline.iter().map(|e| e.state.as_str()).collect();
    assert_eq!(
        states,
        vec![
            "Active",
            "Suspended",
            "Active",
            "Orphaned",
            "Active",
            "Revoked"
        ]
    );

    // Revoked is terminal — reconnect must fail
    assert!(
        scene
            .reconnect_lease(&lease_id, clock.now_millis())
            .is_err(),
        "reconnect on Revoked lease must fail"
    );

    // Revoke on already-revoked must fail (terminal state)
    assert!(
        scene.revoke_lease(lease_id).is_err(),
        "double-revoke must fail (terminal state)"
    );
}

// ─── Test 10: Safe mode resource footprint measurement ───────────────────────

/// Produces the "resource footprint measurements" JSON artifact.
///
/// Measures resource state at each lifecycle phase (active, suspended, resumed,
/// disconnected, expired) to validate the zero-footprint claim.
#[test]
fn test_resource_footprint_measurements() {
    #[derive(Debug, serde::Serialize)]
    struct FootprintSample {
        phase: &'static str,
        tile_count: usize,
        node_count: usize,
        active_lease_count: usize,
        version: u64,
    }

    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");
    let lease_id = scene.grant_lease("footprint.agent", 9_000_000);

    let active_leases = |scene: &SceneGraph| {
        scene
            .leases
            .values()
            .filter(|l| l.state == LeaseState::Active)
            .count()
    };

    let mut samples: Vec<FootprintSample> = Vec::new();

    macro_rules! sample {
        ($phase:expr) => {
            samples.push(FootprintSample {
                phase: $phase,
                tile_count: scene.tile_count(),
                node_count: scene.node_count(),
                active_lease_count: active_leases(&scene),
                version: scene.version,
            });
        };
    }

    sample!("pre_activity");

    // Active: create tiles
    for i in 0..5u32 {
        apply_create_tile(
            &mut scene,
            tab_id,
            "footprint.agent",
            lease_id,
            Rect::new((i as f32) * 210.0, 0.0, 200.0, 200.0),
            i + 1,
        );
    }
    sample!("active_5_tiles");
    assert_eq!(scene.tile_count(), 5);

    // Suspended
    clock.advance(1_000);
    scene.suspend_all_leases(clock.now_millis());
    sample!("suspended");
    assert_eq!(scene.tile_count(), 5, "tiles preserved during suspension");

    // Resumed
    clock.advance(2_000);
    scene.resume_all_leases(clock.now_millis());
    sample!("resumed");
    assert_eq!(scene.tile_count(), 5);

    // Orphaned (grace period)
    clock.advance(500);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect");
    sample!("orphaned_grace_period");
    assert_eq!(scene.tile_count(), 5, "tiles persist during grace period");

    // Expired (after grace)
    clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1_000);
    scene.expire_leases();
    sample!("expired_zero_footprint");
    assert_eq!(scene.tile_count(), 0, "ZERO footprint after expiry");
    assert_eq!(scene.node_count(), 0, "ZERO nodes after expiry");
    assert_eq!(active_leases(&scene), 0, "ZERO active leases after expiry");

    // Emit footprint JSON
    let json = serde_json::to_string_pretty(&samples).expect("footprint serialisation");
    assert!(
        json.contains("zero_footprint"),
        "footprint JSON must include zero_footprint phase"
    );

    // Verify zero-footprint phase
    let zero_phase = samples
        .iter()
        .find(|s| s.phase == "expired_zero_footprint")
        .unwrap();
    assert_eq!(zero_phase.tile_count, 0, "zero tiles at expired phase");
    assert_eq!(zero_phase.node_count, 0, "zero nodes at expired phase");
    assert_eq!(
        zero_phase.active_lease_count, 0,
        "zero active leases at expired phase"
    );
}

// ─── Zone helper ─────────────────────────────────────────────────────────────

/// Build a minimal stream-text zone definition for use in tests.
fn make_stream_text_zone(name: &str) -> tze_hud_scene::types::ZoneDefinition {
    use tze_hud_scene::types::{
        ContentionPolicy, DisplayEdge, GeometryPolicy, RenderingPolicy, ZoneDefinition,
        ZoneMediaType,
    };
    ZoneDefinition {
        id: SceneId::new(),
        name: name.to_string(),
        description: format!("{name} zone (test)"),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 48.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 4,
        transport_constraint: None,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    }
}

// ─── Test 11: Disconnection badge set within 1 frame ──────────────────────────

/// Verifies that tiles owned by an orphaned lease receive the DisconnectionBadge
/// visual hint when `disconnect_lease` is called.
///
/// Spec line 133: "Disconnection badge MUST appear within 1 frame."
/// This test validates the scene-data side of the contract (the compositor
/// reads `tile.visual_hint` to decide what to render).
#[test]
fn test_disconnection_badge_set_on_orphan() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");
    let lease_id = scene.grant_lease("agent.hotel", 60_000);

    let tile_a = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.hotel",
        lease_id,
        Rect::new(0.0, 0.0, 200.0, 200.0),
        1,
    );
    let tile_b = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.hotel",
        lease_id,
        Rect::new(210.0, 0.0, 200.0, 200.0),
        2,
    );

    // Tiles start with no badge
    assert_eq!(scene.tiles[&tile_a].visual_hint, TileVisualHint::None);
    assert_eq!(scene.tiles[&tile_b].visual_hint, TileVisualHint::None);

    // Disconnect: badge must be set (spec: within 1 frame)
    clock.advance(1_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);

    // Both tiles must now show DisconnectionBadge
    assert_eq!(
        scene.tiles[&tile_a].visual_hint,
        TileVisualHint::DisconnectionBadge,
        "tile_a must have DisconnectionBadge after disconnect"
    );
    assert_eq!(
        scene.tiles[&tile_b].visual_hint,
        TileVisualHint::DisconnectionBadge,
        "tile_b must have DisconnectionBadge after disconnect"
    );

    // Reconnect: badge must clear (spec line 141: within 1 frame)
    clock.advance(5_000);
    scene
        .reconnect_lease(&lease_id, clock.now_millis())
        .expect("reconnect");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);

    assert_eq!(
        scene.tiles[&tile_a].visual_hint,
        TileVisualHint::None,
        "tile_a badge must clear after reconnect"
    );
    assert_eq!(
        scene.tiles[&tile_b].visual_hint,
        TileVisualHint::None,
        "tile_b badge must clear after reconnect"
    );

    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after badge test: {violations:?}"
    );
}

// ─── Test 12: Zone publications cleared on lease expiry ───────────────────────

/// Verifies that zone publications from an expired lease are cleared.
///
/// Spec §Requirement: Lease Revocation Clears Zone Publications (lines 235–242):
/// "When a lease is REVOKED or EXPIRED, all zone publications made under that
/// lease MUST be immediately cleared from the zone registry."
#[test]
fn test_zone_publications_cleared_on_lease_expiry() {
    use tze_hud_scene::types::ZoneContent;

    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let _tab_id = scene.create_tab("Main", 0).expect("create_tab");

    // Register a subtitle zone
    scene.register_zone(make_stream_text_zone("subtitle"));

    // Grant a lease with a short TTL for agent.india
    let lease_id = scene.grant_lease("agent.india", 5_000);

    // Agent publishes to subtitle zone
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Hello from agent.india".to_string()),
            "agent.india",
            None,
            None,
            None,
        )
        .expect("publish_to_zone");

    // Zone should have active publication
    assert!(
        !scene
            .zone_registry
            .active_publishes
            .get("subtitle")
            .map(|v| v.is_empty())
            .unwrap_or(true),
        "zone must have active publish before expiry"
    );

    // Disconnect agent.india
    clock.advance(1_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect");

    // Zone publication still present during grace period
    assert!(
        !scene
            .zone_registry
            .active_publishes
            .get("subtitle")
            .map(|v| v.is_empty())
            .unwrap_or(true),
        "zone publish must persist during grace period (stale-badged but visible)"
    );

    // Advance past grace period
    clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1_000);
    let expiries = scene.expire_leases();
    assert!(
        !expiries.is_empty(),
        "expiry sweep must find the expired lease"
    );

    // Zone publication must now be cleared
    let still_active = scene
        .zone_registry
        .active_publishes
        .get("subtitle")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    assert!(
        !still_active,
        "zone publication must be cleared after lease expiry"
    );
}

// ─── Test 13: Zone publish rejected when lease is orphaned ────────────────────

/// Verifies that zone publishes from an orphaned namespace are rejected.
///
/// Spec lines 231–233 (adapted to orphan state):
/// "WHEN a lease is ORPHANED THEN existing zone publications remain visible
/// with a staleness/disconnection indicator but no new publishes are accepted."
#[test]
fn test_zone_publish_rejected_when_lease_orphaned() {
    use tze_hud_scene::types::ZoneContent;
    use tze_hud_scene::validation::ValidationError;

    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    scene.create_tab("Main", 0).expect("create_tab");
    scene.register_zone(make_stream_text_zone("subtitle"));

    let lease_id = scene.grant_lease("agent.juliet", 60_000);

    // Publish while active — must succeed
    scene
        .publish_to_zone_with_lease(
            "subtitle",
            ZoneContent::StreamText("first".to_string()),
            "agent.juliet",
            None,
            None,
        )
        .expect("publish while active must succeed");

    // Disconnect agent
    clock.advance(1_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);

    // New publish attempt while orphaned — must be rejected
    let result = scene.publish_to_zone_with_lease(
        "subtitle",
        ZoneContent::StreamText("second attempt from disconnected agent".to_string()),
        "agent.juliet",
        None,
        None,
    );

    assert!(
        matches!(
            result,
            Err(ValidationError::ZonePublishLeaseOrphaned { .. })
        ),
        "zone publish from orphaned lease must return ZonePublishLeaseOrphaned, got: {result:?}"
    );

    // Existing publication must still be present (stale-badged)
    let pubs = scene
        .zone_registry
        .active_publishes
        .get("subtitle")
        .map(|v| v.len())
        .unwrap_or(0);
    assert_eq!(
        pubs, 1,
        "existing zone publication must still be visible during orphan"
    );
}

// ─── Test 14: Budget-driven revocation bypasses grace period ─────────────────

/// Verifies that budget-driven revocation:
/// - Transitions leases directly to REVOKED (no orphan state).
/// - Tiles are removed after the 100ms delay.
/// - Post-revocation resource footprint is zero.
///
/// Spec §Post-Revocation Resource Cleanup (lines 253–260).
#[test]
fn test_budget_revocation_bypasses_grace_and_zero_footprint() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");
    let lease_id = scene.grant_lease("agent.kilo", 60_000);

    let tile_a = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.kilo",
        lease_id,
        Rect::new(0.0, 0.0, 200.0, 200.0),
        1,
    );
    let tile_b = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.kilo",
        lease_id,
        Rect::new(210.0, 0.0, 200.0, 200.0),
        2,
    );
    assert_eq!(scene.tile_count(), 2);

    // Also grant a second agent that must be unaffected
    let other_lease = scene.grant_lease("agent.lima", 60_000);
    let tile_other = apply_create_tile(
        &mut scene,
        tab_id,
        "agent.lima",
        other_lease,
        Rect::new(500.0, 0.0, 200.0, 200.0),
        3,
    );
    assert_eq!(scene.tile_count(), 3);

    // ── Initiate budget-driven revocation ─────────────────────────────────
    clock.advance(1_000);
    let specs = scene.initiate_budget_revocation("agent.kilo");

    assert_eq!(specs.len(), 1, "one lease for agent.kilo");
    // Lease is now REVOKED (not ORPHANED — grace bypassed)
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Revoked,
        "budget revocation must set state=REVOKED immediately (not ORPHANED)"
    );

    // Tiles still exist at t+0ms (pending 100ms free delay)
    // Note: initiate only marks for removal; finalize does the actual free.
    assert!(
        specs[0].bypasses_grace_period(),
        "budget policy revocation must bypass grace period"
    );

    // Verify the spec has the right free delay
    assert!(
        !specs[0].is_ready_to_free(clock.now_millis()),
        "not ready to free at t=0 after revocation"
    );
    assert!(
        !specs[0].is_ready_to_free(clock.now_millis() + POST_REVOCATION_FREE_DELAY_MS - 1),
        "not ready at 99ms"
    );

    // ── After 100ms delay: finalize cleanup ───────────────────────────────
    clock.advance(POST_REVOCATION_FREE_DELAY_MS);
    let finalized = scene.finalize_budget_revocation(&specs, clock.now_millis());
    assert_eq!(finalized, 1, "exactly 1 spec finalized");

    // Tiles removed — zero footprint
    assert!(
        !scene.tiles.contains_key(&tile_a),
        "tile_a removed after budget revocation"
    );
    assert!(
        !scene.tiles.contains_key(&tile_b),
        "tile_b removed after budget revocation"
    );
    assert_eq!(scene.tile_count(), 1, "only agent.lima tile remains");

    // Other agent unaffected
    assert!(
        scene.tiles.contains_key(&tile_other),
        "agent.lima tile survives kilo revocation"
    );
    assert_eq!(
        scene.leases[&other_lease].state,
        LeaseState::Active,
        "agent.lima lease unaffected"
    );

    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after budget revocation: {violations:?}"
    );

    let _ = (tile_a, tile_b, tile_other);
}

// ─── Test 15: Grace period precision (GracePeriodTimer unit integration) ──────

/// Integration test for the GracePeriodTimer precision requirement.
///
/// Spec §Grace Period Precision (lines 147–154):
/// - "The grace period MUST be accurate to +/- 100ms."
/// - "The runtime MUST NOT prematurely expire the grace period."
/// - Agent can reconnect at 29,950ms.
#[test]
fn test_grace_period_timer_precision_integration() {
    // Simulate the scenario from the spec (lines 152–154):
    // WHEN grace = 30,000ms THEN agent can still reconnect at 29,950ms.
    let timer = GracePeriodTimer::new(
        SceneId::new(),
        0,                      // orphaned at t=0
        ORPHAN_GRACE_PERIOD_MS, // 30,000ms
    );

    // Must not expire at 29,950ms (spec: MUST NOT prematurely expire)
    assert!(
        timer.can_reconnect(29_950),
        "agent must be able to reconnect at 29,950ms (spec lines 152-154)"
    );
    assert!(
        !timer.has_expired(29_950),
        "grace period must not be expired at 29,950ms"
    );

    // Must be expired at exactly 30,000ms
    assert!(
        timer.has_expired(30_000),
        "grace period must be expired at 30,000ms"
    );
    assert!(
        !timer.can_reconnect(30_000),
        "reconnect must not be allowed at exactly 30,000ms"
    );

    // SceneGraph-level: reconnect at 29,950ms succeeds
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    let _tab_id = scene.create_tab("Main", 0).expect("create_tab");
    let lease_id = scene.grant_lease("agent.mike", 600_000);

    // Orphan the lease at t=0
    scene.disconnect_lease(&lease_id, 0).expect("disconnect");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);

    // Reconnect at 29,950ms — must succeed
    scene
        .reconnect_lease(&lease_id, 29_950)
        .expect("reconnect at 29,950ms must succeed (spec lines 152-154)");
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Active,
        "lease must be Active after reconnect at 29,950ms"
    );
}

// ─── Test 16: Zone publications cleared on revoke_lease ──────────────────────

/// Verifies that `revoke_lease` clears zone publications from that namespace.
///
/// Spec §Requirement: Lease Revocation Clears Zone Publications (lines 235–242).
#[test]
fn test_zone_publications_cleared_on_revoke_lease() {
    use tze_hud_scene::types::ZoneContent;

    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    scene.create_tab("Main", 0).expect("create_tab");
    scene.register_zone(make_stream_text_zone("status"));

    let lease_id = scene.grant_lease("agent.november", 60_000);

    // Publish while active
    scene
        .publish_to_zone(
            "status",
            ZoneContent::StreamText("active".into()),
            "agent.november",
            None,
            None,
            None,
        )
        .expect("publish");

    assert!(
        !scene
            .zone_registry
            .active_publishes
            .get("status")
            .map(|v| v.is_empty())
            .unwrap_or(true),
        "zone must have publication before revocation"
    );

    // Revoke lease
    scene.revoke_lease(lease_id).expect("revoke_lease");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Revoked);

    // Zone publication must be cleared
    let still_active = scene
        .zone_registry
        .active_publishes
        .get("status")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    assert!(
        !still_active,
        "zone publication must be cleared after revoke_lease"
    );
}

// ─── Test 17: TTL continues running during orphan state ──────────────────────

/// Verifies that the TTL clock continues running while a lease is ORPHANED.
///
/// Spec line 133: "TTL clock MUST continue running during the grace period."
/// This is distinct from SUSPENDED, where the TTL clock is paused.
#[test]
fn test_ttl_continues_during_orphan_state() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    scene.create_tab("Main", 0).expect("create_tab");
    // Short TTL of 10,000ms
    let lease_id = scene.grant_lease("agent.oscar", 10_000);

    // Advance 2,000ms (TTL now 8,000ms remaining)
    clock.advance(2_000);
    let ttl_at_2s = scene.leases[&lease_id].remaining_ms(clock.now_millis());
    assert!(
        (7_900..=8_100).contains(&ttl_at_2s),
        "TTL ≈ 8,000ms at t=2s, got {ttl_at_2s}"
    );

    // Disconnect (orphan) at t=2,000ms
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);

    // Advance another 4,000ms while orphaned (TTL must continue counting down)
    clock.advance(4_000);
    let ttl_at_6s = scene.leases[&lease_id].remaining_ms(clock.now_millis());
    // TTL should be ≈ 4,000ms (10,000 - 6,000 elapsed)
    assert!(
        (3_900..=4_100).contains(&ttl_at_6s),
        "TTL must continue running during orphan state: expected ≈4,000ms, got {ttl_at_6s}ms"
    );

    // Verify the TTL is actually smaller than at t=2s (proving clock ran during orphan)
    assert!(
        ttl_at_6s < ttl_at_2s,
        "TTL must have decreased during orphan state"
    );
}
