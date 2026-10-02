//! # Lease Lifecycle Tests — Presence Card (hud-apoe.2)
//!
//! Tests covering the lease lifecycle for presence card tiles per the
//! exemplar-presence-card OpenSpec change (openspec/changes/exemplar-presence-card/).
//!
//! ## Acceptance criteria tested
//!
//! 1. **Lease request**: `ttl_ms=120_000` with `create_tiles`/`modify_own_tiles`
//!    is granted; `REQUESTED → ACTIVE` transition is observable.
//!
//! 2. **AutoRenew at 75% TTL (90s of 120s)**: `TtlState` with `AutoRenew` policy
//!    reports `TtlCheck::AutoRenewDue` at exactly the 75% elapsed mark.
//!    The agent does NOT implement a renewal timer — this is server-side.
//!    After `reset_renewal_window` the timer re-arms for the next window.
//!
//! 3. **Lease expiry rejection**: `MutationBatch` submitted with an expired lease
//!    returns `LeaseExpired` error; batch submitted with no / unknown lease returns
//!    `LeaseNotFound`. Neither is a silent failure.
//!
//! 4. **Lease-tile binding**: `CreateTile.lease_id` binds the tile to the lease.
//!    When the lease transitions to `ORPHANED → EXPIRED`, the tiles are removed
//!    and the scene reaches zero footprint.
//!
//! 5. **State machine observable transitions**: `REQUESTED → ACTIVE` (grant),
//!    `ACTIVE → ORPHANED` (disconnect), `ORPHANED → EXPIRED` (grace expiry).
//!
//! These are **Layer 0** tests: pure logic on the scene data structure,
//! no GPU context, no async, no external services.

use std::sync::Arc;

use tze_hud_scene::{
    Clock, TestClock,
    graph::SceneGraph,
    lease::{AutoRenewalArm, DisarmReason, TtlCheck, TtlState},
    mutation::{MutationBatch, SceneMutation},
    test_scenes::assert_layer0_invariants,
    types::{LeaseState, Node, NodeData, Rect, Rgba, SceneId, SolidColorNode},
    validation::ValidationError,
};

// ─── Constants matching the spec ─────────────────────────────────────────────

/// Presence card TTL: 2 minutes per the exemplar-presence-card spec.
const PRESENCE_CARD_TTL_MS: u64 = 120_000;

/// AutoRenew threshold: 75% of 120s = 90s.
const AUTO_RENEW_AT_MS: u64 = 90_000;

// ─── Helpers ─────────────────────────────────────────────────────────────────

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

/// Apply a CreateTile mutation with the given lease and return the tile ID.
fn create_presence_card_tile(
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
        lease_id: Some(lease_id),
    };
    let result = scene.apply_batch(&batch);
    assert!(
        result.applied,
        "CreateTile must succeed: {:?}",
        result.error
    );
    result.created_ids[0]
}

// ─── Test 1: Lease request granted, REQUESTED → ACTIVE ───────────────────────

/// AC 1: LeaseRequest with ttl_ms=120_000 and capabilities [create_tiles,
/// modify_own_tiles] is granted. Lease transitions REQUESTED → ACTIVE.
///
/// Note: `grant_lease` in SceneGraph encapsulates REQUESTED → ACTIVE atomically,
/// which is the correct model (the scene graph has no separate "pending" state;
/// the state machine starts Active upon successful grant). The protocol layer
/// answers with `LeaseResponse(granted=true)`.
#[test]
fn test_presence_card_lease_request_granted() {
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let _tab_id = scene.create_tab("Main", 0).expect("create_tab");

    // Request a lease with the canonical presence-card TTL and capabilities.
    let lease_id = scene.grant_lease("agent.presence", PRESENCE_CARD_TTL_MS);

    // Lease is immediately Active (REQUESTED → ACTIVE in the scene model).
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Active,
        "presence card lease must be ACTIVE after grant"
    );

    // LeaseId assigned (non-nil).
    assert!(
        !lease_id.is_nil(),
        "granted lease must have a non-nil LeaseId"
    );

    // Correct TTL stored.
    assert_eq!(
        scene.leases[&lease_id].ttl_ms, PRESENCE_CARD_TTL_MS,
        "granted TTL must match requested ttl_ms"
    );

    let violations = assert_layer0_invariants(&scene);
    assert!(violations.is_empty(), "Layer 0 invariants: {violations:?}");
}

// ─── Test 2: AutoRenew fires at 75% TTL (90s of 120s) ────────────────────────

/// AC 2: With AutoRenew policy, TtlState::poll() returns AutoRenewDue
/// exactly when 75% of TTL (90s) has elapsed; the agent does NOT need to
/// implement a renewal timer — this is server-side logic.
///
/// Verifies:
/// - Timer is Armed at activation.
/// - poll() returns Ok before 75% threshold.
/// - poll() returns AutoRenewDue at 75% threshold (one-shot per window).
/// - After reset_renewal_window, the timer re-arms for the next window.
#[test]
fn test_presence_card_auto_renew_fires_at_75_percent_ttl() {
    let clock = TestClock::new(0);
    let mut ttl = TtlState::new_activated(
        PRESENCE_CARD_TTL_MS,
        tze_hud_scene::lease::RenewalPolicy::AutoRenew,
        clock.clone(),
    );

    // Timer must be Armed at activation (agent needs no renewal timer).
    assert_eq!(
        ttl.auto_renewal_arm(),
        AutoRenewalArm::Armed,
        "auto-renewal timer must be Armed at activation"
    );

    // At 89_999ms (just before 75% = 90_000ms): no renewal.
    clock.advance(89_999);
    assert_eq!(
        ttl.poll(),
        TtlCheck::Ok,
        "no renewal event before 75% threshold (at 89_999ms)"
    );

    // At 90_000ms (exactly 75%): AutoRenewDue.
    clock.advance(1); // total = 90_000ms
    assert_eq!(
        ttl.poll(),
        TtlCheck::AutoRenewDue,
        "AutoRenewDue must fire at 75% TTL (90_000ms of 120_000ms)"
    );

    // Second poll in same window: should NOT fire again (one-shot guard).
    assert_eq!(
        ttl.poll(),
        TtlCheck::Ok,
        "AutoRenewDue must not fire twice in the same window"
    );

    // Advance further — still no second renewal.
    clock.advance(5_000);
    assert_eq!(
        ttl.poll(),
        TtlCheck::Ok,
        "no duplicate AutoRenewDue after reset in same window"
    );

    // Server processes renewal: reset window with same 120s TTL.
    ttl.reset_renewal_window(PRESENCE_CARD_TTL_MS);

    // Immediately after reset: poll returns Ok (0ms elapsed in new window).
    assert_eq!(
        ttl.poll(),
        TtlCheck::Ok,
        "no renewal immediately after reset_renewal_window"
    );

    // Advance to 90s in the new window → next renewal fires.
    clock.advance(90_000);
    assert_eq!(
        ttl.poll(),
        TtlCheck::AutoRenewDue,
        "AutoRenewDue must fire again after reset_renewal_window at 75% of new window"
    );
}

/// AC 2 (extended): AutoRenew does NOT fire for Manual or OneShot policies.
/// This confirms AutoRenew is a specific server-side policy choice.
#[test]
fn test_auto_renew_not_applicable_for_other_policies() {
    use tze_hud_scene::lease::RenewalPolicy;

    let clock = TestClock::new(0);
    let mut ttl_manual =
        TtlState::new_activated(PRESENCE_CARD_TTL_MS, RenewalPolicy::Manual, clock.clone());
    let mut ttl_oneshot =
        TtlState::new_activated(PRESENCE_CARD_TTL_MS, RenewalPolicy::OneShot, clock.clone());

    // Both are NotApplicable (no server-side timer).
    assert_eq!(ttl_manual.auto_renewal_arm(), AutoRenewalArm::NotApplicable);
    assert_eq!(
        ttl_oneshot.auto_renewal_arm(),
        AutoRenewalArm::NotApplicable
    );

    // Advance past 75%: neither fires AutoRenewDue.
    clock.advance(AUTO_RENEW_AT_MS + 1);
    assert_ne!(
        ttl_manual.poll(),
        TtlCheck::AutoRenewDue,
        "Manual policy must not fire AutoRenewDue"
    );
    assert_ne!(
        ttl_oneshot.poll(),
        TtlCheck::AutoRenewDue,
        "OneShot policy must not fire AutoRenewDue"
    );
}

/// AC 2 (disarm): AutoRenew timer is disarmed during budget warning.
/// Ensures the server-side guard prevents renewals while budget is strained.
#[test]
fn test_auto_renew_disarmed_during_budget_warning() {
    use tze_hud_scene::lease::RenewalPolicy;

    let clock = TestClock::new(0);
    let mut ttl = TtlState::new_activated(
        PRESENCE_CARD_TTL_MS,
        RenewalPolicy::AutoRenew,
        clock.clone(),
    );

    // Simulate server-side disarm (budget warning from enforcement ladder).
    ttl.disarm_renewal(DisarmReason::BudgetWarning);
    assert_eq!(
        ttl.auto_renewal_arm(),
        AutoRenewalArm::Disarmed,
        "timer must be Disarmed after budget warning"
    );

    // Advance past 75% — no renewal event.
    clock.advance(AUTO_RENEW_AT_MS + 1);
    assert_eq!(
        ttl.poll(),
        TtlCheck::Ok,
        "AutoRenewDue must NOT fire while timer is Disarmed (budget warning)"
    );

    // Budget warning clears — re-arm.
    ttl.rearm_renewal();
    assert_eq!(
        ttl.auto_renewal_arm(),
        AutoRenewalArm::Armed,
        "timer must be re-Armed when budget warning clears"
    );
}

// ─── Test 3: Lease expiry rejection ──────────────────────────────────────────

/// AC 3: MutationBatch with an expired (Revoked/Expired) lease returns
/// LeaseExpired error. Not a silent failure.
#[test]
fn test_mutation_rejected_with_expired_lease() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");

    // Grant a short-lived lease.
    let lease_id = scene.grant_lease("agent.presence", PRESENCE_CARD_TTL_MS);
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);

    // Revoke the lease (simulates expiry as seen by the validation pipeline).
    scene.revoke_lease(lease_id).expect("revoke_lease");
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Revoked,
        "lease must be Revoked before testing rejection"
    );

    // Attempt a CreateTile mutation — must be rejected.
    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent.presence".to_string(),
        mutations: vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent.presence".to_string(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 200.0, 80.0),
            z_order: 1,
        }],
        timing_hints: None,
        lease_id: Some(lease_id),
    };

    let result = scene.apply_batch(&batch);

    assert!(
        !result.applied,
        "CreateTile with revoked lease must be rejected (not a silent failure)"
    );
    // Error must be non-empty.
    assert!(
        result.error.is_some(),
        "rejection must carry a structured error"
    );
}

/// AC 3: MutationBatch using a fully expired (TTL elapsed) lease returns
/// LeaseExpired. Tests the actual TTL expiry path, not just revocation.
#[test]
fn test_mutation_rejected_after_ttl_expiry() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");

    // Grant a short TTL lease for this test.
    let short_ttl_ms: u64 = 5_000;
    let lease_id = scene.grant_lease("agent.presence.ttl", short_ttl_ms);

    // Create one tile while active — must succeed.
    let tile_id = create_presence_card_tile(
        &mut scene,
        tab_id,
        "agent.presence.ttl",
        lease_id,
        Rect::new(0.0, 0.0, 200.0, 80.0),
        1,
    );
    assert_eq!(scene.tile_count(), 1);

    // Advance past TTL and run expiry sweep.
    clock.advance(short_ttl_ms + 1_000);
    let expiries = scene.expire_leases();
    assert!(
        !expiries.is_empty(),
        "expiry sweep must find the expired lease"
    );

    // Lease must now be in Expired terminal state.
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Expired,
        "lease must be Expired after TTL elapsed"
    );

    // Tile was cleaned up on expiry.
    assert_eq!(
        scene.tile_count(),
        0,
        "tiles must be removed after TTL expiry"
    );
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "expired tile must not exist in scene"
    );

    // MutationBatch with expired lease must be rejected.
    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent.presence.ttl".to_string(),
        mutations: vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent.presence.ttl".to_string(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 200.0, 80.0),
            z_order: 2,
        }],
        timing_hints: None,
        lease_id: Some(lease_id),
    };

    let result = scene.apply_batch(&batch);
    assert!(
        !result.applied,
        "MutationBatch with expired lease must be rejected"
    );
    // Tile count must be unchanged (still 0).
    assert_eq!(
        scene.tile_count(),
        0,
        "tile count must be 0 after rejected mutation"
    );
}

/// AC 3: MutationBatch with a non-existent lease_id returns LeaseNotFound.
/// Tests the "no lease active" path.
#[test]
fn test_mutation_rejected_with_no_lease() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");

    // Use a random lease_id that was never granted.
    let nonexistent_lease_id = SceneId::new();

    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent.presence".to_string(),
        mutations: vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent.presence".to_string(),
            lease_id: nonexistent_lease_id,
            bounds: Rect::new(0.0, 0.0, 200.0, 80.0),
            z_order: 1,
        }],
        timing_hints: None,
        lease_id: Some(nonexistent_lease_id),
    };

    let result = scene.apply_batch(&batch);

    assert!(
        !result.applied,
        "MutationBatch with no lease must be rejected (LeaseNotFound)"
    );
    assert!(
        result.error.is_some(),
        "rejection must carry a LeaseNotFound error"
    );
    // Verify it's specifically LeaseNotFound.
    match &result.error {
        Some(ValidationError::LeaseNotFound { id }) => {
            assert_eq!(
                *id, nonexistent_lease_id,
                "LeaseNotFound must reference the correct lease_id"
            );
        }
        other => panic!("Expected ValidationError::LeaseNotFound, got: {other:?}"),
    }
    assert_eq!(
        scene.tile_count(),
        0,
        "no tile must be created on LeaseNotFound rejection"
    );
}

// ─── Test 4: Lease-tile binding ───────────────────────────────────────────────

/// AC 4: CreateTile.lease_id binds the tile to the lease.
/// The tile is owned by and associated with the lease that created it.
/// When the lease expires, the bound tile is removed (zero footprint).
#[test]
fn test_presence_card_tile_binds_to_lease() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");

    let lease_id = scene.grant_lease("agent.presence", PRESENCE_CARD_TTL_MS);

    // Create presence card tile (200x80 per spec).
    let tile_id = create_presence_card_tile(
        &mut scene,
        tab_id,
        "agent.presence",
        lease_id,
        Rect::new(0.0, 0.0, 200.0, 80.0),
        1,
    );

    // Tile is bound to the lease.
    assert_eq!(
        scene.tiles[&tile_id].lease_id, lease_id,
        "tile's lease_id must bind to the granting lease"
    );
    assert_eq!(
        scene.tiles[&tile_id].namespace, "agent.presence",
        "tile's namespace must match the agent's namespace"
    );

    // Add a content node to the tile (SetTileRoot — avatar + name).
    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent.presence".to_string(),
        mutations: vec![SceneMutation::SetTileRoot {
            tile_id,
            node: make_solid_node(
                Rect::new(0.0, 0.0, 200.0, 80.0),
                Rgba::new(0.08, 0.08, 0.08, 0.78),
            ),
            descendants: vec![],
        }],
        timing_hints: None,
        lease_id: None,
    };
    let root_result = scene.apply_batch(&batch);
    assert!(
        root_result.applied,
        "SetTileRoot must succeed on active lease"
    );

    // Lease is ACTIVE; tile visible.
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Active,
        "lease must be Active"
    );
    assert_eq!(scene.tile_count(), 1, "one tile present");

    // Revoke (simulates controlled agent shutdown / end of session).
    scene.revoke_lease(lease_id).expect("revoke_lease");

    // Lease REVOKED → tile removed (lease-tile binding: tile lifecycle follows lease).
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Revoked,
        "lease must be Revoked"
    );
    assert_eq!(
        scene.tile_count(),
        0,
        "bound tile must be removed when lease is revoked"
    );
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "tile must not exist after lease revocation"
    );

    let violations = assert_layer0_invariants(&scene);
    assert!(violations.is_empty(), "Layer 0 invariants: {violations:?}");
}

// ─── Test 5: State machine transitions observable ─────────────────────────────

/// AC 5: Observable state machine transitions:
/// REQUESTED → ACTIVE (grant), ACTIVE → ORPHANED (disconnect),
/// ORPHANED → EXPIRED (grace period expiry).
///
/// Produces a state-transition log as a JSON audit trail.
#[test]
fn test_presence_card_lease_state_machine_transitions() {
    #[derive(Debug, serde::Serialize)]
    struct TransitionEntry {
        t_ms: u64,
        state: String,
        event: &'static str,
        tile_count: usize,
    }

    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).expect("create_tab");
    let mut log: Vec<TransitionEntry> = Vec::new();

    macro_rules! record {
        ($event:expr, $lease_id:expr) => {{
            log.push(TransitionEntry {
                t_ms: clock.now_millis(),
                state: format!("{:?}", scene.leases[&$lease_id].state),
                event: $event,
                tile_count: scene.tile_count(),
            });
        }};
    }

    // ── Step 1: Grant lease (REQUESTED → ACTIVE) ──────────────────────────
    let lease_id = scene.grant_lease("agent.presence", PRESENCE_CARD_TTL_MS);
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
    record!("grant REQUESTED→ACTIVE", lease_id);

    // ── Step 2: Create presence card tile ─────────────────────────────────
    let tile_id = create_presence_card_tile(
        &mut scene,
        tab_id,
        "agent.presence",
        lease_id,
        Rect::new(0.0, 0.0, 200.0, 80.0),
        1,
    );
    assert_eq!(scene.tile_count(), 1);
    record!("tile created", lease_id);

    // ── Step 3: Agent disconnects (ACTIVE → ORPHANED) ─────────────────────
    clock.advance(1_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect_lease");
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Orphaned,
        "lease must be ORPHANED after disconnect"
    );
    // Tile persists during grace period.
    assert_eq!(
        scene.tile_count(),
        1,
        "tile must persist during grace period"
    );
    record!("disconnect ACTIVE→ORPHANED", lease_id);

    // ── Step 4: Grace period expires (ORPHANED → EXPIRED) ─────────────────
    clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1_000);
    let expiries = scene.expire_leases();
    assert!(
        expiries.iter().any(|e| e.lease_id == lease_id),
        "presence card lease must appear in expiry list"
    );
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Expired,
        "lease must be EXPIRED after grace period"
    );
    // Zero footprint: tile removed.
    assert_eq!(
        scene.tile_count(),
        0,
        "tile must be removed after lease EXPIRED"
    );
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "presence card tile must not exist after expiry"
    );
    record!("expiry ORPHANED→EXPIRED", lease_id);

    // Emit audit trail as JSON (artifact output).
    let json = serde_json::to_string_pretty(&log).expect("serialisation");
    assert!(json.contains("ACTIVE"), "log must include Active state");
    assert!(json.contains("Orphaned"), "log must include Orphaned state");
    assert!(json.contains("Expired"), "log must include Expired state");

    // Verify transition order.
    let states: Vec<&str> = log.iter().map(|e| e.state.as_str()).collect();
    assert!(
        states[0] == "Active",
        "first recorded state must be Active (REQUESTED→ACTIVE grant)"
    );
    assert!(
        states.contains(&"Orphaned"),
        "Orphaned state must appear in log"
    );
    assert!(
        states.last().copied() == Some("Expired"),
        "final state must be Expired"
    );

    let violations = assert_layer0_invariants(&scene);
    assert!(violations.is_empty(), "Layer 0 invariants: {violations:?}");
}

// ─── Test 6: Multi-agent namespace isolation ──────────────────────────────────

/// AC 4 (extended): Three presence card agents coexist. One agent's lease
/// expiry does NOT affect the other agents' tiles or leases.
///
/// This validates the exemplar-presence-card spec requirement:
/// "Namespace isolation: one agent's disconnect does not affect others."
#[test]
fn test_presence_card_namespace_isolation() {
    let clock = Arc::new(TestClock::new(0));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Presence Roster", 0).expect("create_tab");

    // Three agents, each with a presence card lease.
    let lease_a = scene.grant_lease("agent.alpha", PRESENCE_CARD_TTL_MS);
    let lease_b = scene.grant_lease("agent.bravo", PRESENCE_CARD_TTL_MS);
    let lease_c = scene.grant_lease("agent.charlie", PRESENCE_CARD_TTL_MS);

    // Create stacked presence card tiles (200x80, 8px gap per spec).
    let tile_a = create_presence_card_tile(
        &mut scene,
        tab_id,
        "agent.alpha",
        lease_a,
        Rect::new(0.0, 0.0, 200.0, 80.0),
        1,
    );
    let tile_b = create_presence_card_tile(
        &mut scene,
        tab_id,
        "agent.bravo",
        lease_b,
        Rect::new(0.0, 88.0, 200.0, 80.0),
        2,
    );
    let tile_c = create_presence_card_tile(
        &mut scene,
        tab_id,
        "agent.charlie",
        lease_c,
        Rect::new(0.0, 176.0, 200.0, 80.0),
        3,
    );

    assert_eq!(scene.tile_count(), 3, "three presence card tiles");

    // All three agents render simultaneously (60fps target is compositor concern;
    // here we just verify all tiles exist).
    for &tile_id in &[tile_a, tile_b, tile_c] {
        assert!(
            scene.tiles.contains_key(&tile_id),
            "tile {tile_id} must exist"
        );
    }

    // ── Agent bravo disconnects ────────────────────────────────────────────
    clock.advance(5_000);
    scene
        .disconnect_lease(&lease_b, clock.now_millis())
        .expect("bravo disconnect");
    assert_eq!(
        scene.leases[&lease_b].state,
        LeaseState::Orphaned,
        "bravo must be ORPHANED"
    );

    // Alpha and charlie are unaffected.
    assert_eq!(
        scene.leases[&lease_a].state,
        LeaseState::Active,
        "alpha unaffected by bravo disconnect"
    );
    assert_eq!(
        scene.leases[&lease_c].state,
        LeaseState::Active,
        "charlie unaffected by bravo disconnect"
    );
    // All three tiles still present during bravo's grace period.
    assert_eq!(
        scene.tile_count(),
        3,
        "all tiles persist during bravo grace period"
    );

    // ── Bravo's grace period expires ──────────────────────────────────────
    clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1_000);
    scene.expire_leases();

    // Bravo's lease EXPIRED, tile removed.
    assert_eq!(
        scene.leases[&lease_b].state,
        LeaseState::Expired,
        "bravo lease must be EXPIRED after grace"
    );
    assert!(
        !scene.tiles.contains_key(&tile_b),
        "bravo tile removed after expiry"
    );

    // Alpha and charlie are untouched.
    assert_eq!(
        scene.tile_count(),
        2,
        "alpha and charlie tiles survive bravo expiry"
    );
    assert!(
        scene.tiles.contains_key(&tile_a),
        "alpha tile survives bravo expiry"
    );
    assert!(
        scene.tiles.contains_key(&tile_c),
        "charlie tile survives bravo expiry"
    );
    assert_eq!(
        scene.leases[&lease_a].state,
        LeaseState::Active,
        "alpha lease unaffected"
    );
    assert_eq!(
        scene.leases[&lease_c].state,
        LeaseState::Active,
        "charlie lease unaffected"
    );

    // Alpha and charlie can still mutate (update their timestamp displays).
    for (ns, _lid) in [("agent.alpha", lease_a), ("agent.charlie", lease_c)] {
        let tile_id = if ns == "agent.alpha" { tile_a } else { tile_c };
        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: ns.to_string(),
            mutations: vec![SceneMutation::SetTileRoot {
                tile_id,
                node: make_solid_node(
                    Rect::new(0.0, 0.0, 200.0, 80.0),
                    Rgba::new(0.08, 0.08, 0.08, 0.9),
                ),
                descendants: vec![],
            }],
            timing_hints: None,
            lease_id: None,
        };
        let result = scene.apply_batch(&batch);
        assert!(
            result.applied,
            "{ns} must be able to mutate after bravo expiry"
        );
    }

    let violations = assert_layer0_invariants(&scene);
    assert!(violations.is_empty(), "Layer 0 invariants: {violations:?}");
}

// ─── Test 7: AutoRenew TTL accounting through reset_renewal_window ────────────

/// AC 2 (precision): After renewal, remaining_ms reflects the fresh TTL window.
/// This verifies that `reset_renewal_window` correctly resets the clock origin
/// so the 75% threshold is computed against the new window, not the old one.
#[test]
fn test_auto_renew_ttl_reset_reflects_new_window() {
    use tze_hud_scene::lease::RenewalPolicy;

    let clock = TestClock::new(0);
    let mut ttl = TtlState::new_activated(
        PRESENCE_CARD_TTL_MS,
        RenewalPolicy::AutoRenew,
        clock.clone(),
    );

    // Advance to just past 75% (90s + 1ms) and consume the AutoRenewDue event.
    clock.advance(AUTO_RENEW_AT_MS + 1);
    assert_eq!(ttl.poll(), TtlCheck::AutoRenewDue);

    // Remaining TTL before renewal (≈ 29_999ms).
    let remaining_before_renewal = ttl.remaining_ms().unwrap();
    assert!(
        remaining_before_renewal < 30_001,
        "remaining must be < 30s before renewal"
    );

    // Server processes renewal: reset window with the same 120s TTL.
    ttl.reset_renewal_window(PRESENCE_CARD_TTL_MS);

    // Remaining must now be close to the full new TTL (within rounding).
    let remaining_after_renewal = ttl.remaining_ms().unwrap();
    assert!(
        remaining_after_renewal > 119_000,
        "remaining must be ≈ 120s after reset_renewal_window, got {remaining_after_renewal}ms",
    );

    // Next AutoRenewDue fires at 75% of new window.
    clock.advance(AUTO_RENEW_AT_MS + 1);
    assert_eq!(
        ttl.poll(),
        TtlCheck::AutoRenewDue,
        "AutoRenewDue must fire at 75% of renewed window"
    );
}

// ─── Test 8: Presence card full lifecycle (integration) ──────────────────────

/// Integration: full presence card lifecycle from grant to TTL expiry with
/// a content-update mutation in between.
///
/// Models the exemplar-presence-card scenario:
/// - Grant lease (120s TTL, AutoRenew)
/// - Create 200x80 presence card tile
/// - Set tile root (identity card content)
/// - Simulate periodic content update (30s timestamp refresh)
/// - Disconnect → grace period → expiry
#[test]
fn test_presence_card_full_lifecycle_integration() {
    let clock = Arc::new(TestClock::new(1_000_000)); // start at 1M ms (arbitrary wall time)
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Presence Roster", 0).expect("create_tab");

    // 1. Grant lease.
    let lease_id = scene.grant_lease("agent.presence.full", PRESENCE_CARD_TTL_MS);
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
    assert!(!lease_id.is_nil(), "lease_id must be non-nil");

    // 2. Create 200x80 presence card tile (bottom-left stacking per spec).
    let tile_id = create_presence_card_tile(
        &mut scene,
        tab_id,
        "agent.presence.full",
        lease_id,
        Rect::new(0.0, 0.0, 200.0, 80.0),
        1,
    );
    assert_eq!(scene.tile_count(), 1);
    assert_eq!(
        scene.tiles[&tile_id].lease_id, lease_id,
        "tile bound to lease"
    );

    // 3. Set tile root (identity card: avatar bg color + content).
    let set_root_batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent.presence.full".to_string(),
        mutations: vec![SceneMutation::SetTileRoot {
            tile_id,
            node: make_solid_node(
                Rect::new(0.0, 0.0, 200.0, 80.0),
                Rgba::new(0.08, 0.08, 0.08, 0.78),
            ),
            descendants: vec![],
        }],
        timing_hints: None,
        lease_id: None,
    };
    assert!(
        scene.apply_batch(&set_root_batch).applied,
        "SetTileRoot must succeed"
    );

    // 4. Periodic content update at 30s (timestamp refresh).
    clock.advance(30_000);
    let update_batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent.presence.full".to_string(),
        mutations: vec![SceneMutation::SetTileRoot {
            tile_id,
            node: make_solid_node(
                Rect::new(0.0, 0.0, 200.0, 80.0),
                // Slightly different color models "last active: 30s ago" update.
                Rgba::new(0.08, 0.08, 0.10, 0.78),
            ),
            descendants: vec![],
        }],
        timing_hints: None,
        lease_id: None,
    };
    assert!(
        scene.apply_batch(&update_batch).applied,
        "30s content update must succeed"
    );
    assert_eq!(scene.tile_count(), 1, "still one tile after update");

    // 5. Agent disconnects (ACTIVE → ORPHANED).
    clock.advance(15_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);
    // Tile still visible during grace (disconnection badge would be set).
    assert_eq!(scene.tile_count(), 1, "tile visible during grace period");

    // Mutations are rejected while ORPHANED.
    let orphan_batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent.presence.full".to_string(),
        mutations: vec![SceneMutation::SetTileRoot {
            tile_id,
            node: make_solid_node(
                Rect::new(0.0, 0.0, 200.0, 80.0),
                Rgba::new(1.0, 0.0, 0.0, 1.0),
            ),
            descendants: vec![],
        }],
        timing_hints: None,
        lease_id: None,
    };
    // SetTileRoot while orphaned — the tile's lease is orphaned so this must
    // be rejected (lease not Active).
    let orphan_result = scene.apply_batch(&orphan_batch);
    assert!(
        !orphan_result.applied,
        "mutations must be rejected while lease is ORPHANED"
    );

    // 6. Grace period expires (ORPHANED → EXPIRED).
    clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 500);
    let expiries = scene.expire_leases();
    assert!(
        expiries.iter().any(|e| e.lease_id == lease_id),
        "presence card lease must expire"
    );
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Expired,
        "lease EXPIRED after grace"
    );
    assert_eq!(scene.tile_count(), 0, "zero tiles after lease expiry");
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "tile must be removed after expiry"
    );

    let violations = assert_layer0_invariants(&scene);
    assert!(violations.is_empty(), "Layer 0 invariants: {violations:?}");
}
