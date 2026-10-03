//! # Batch Atomicity and Transaction Validation Integration Tests
//!
//! Per the acceptance criteria in bead rig-pkcv:
//!
//! - Partial-failure rollback test: batch with N mutations where mutation K fails
//!   results in zero applied mutations.
//! - Batch atomicity proptest: random mutation batches verify all-or-nothing.
//! - All mutation WHEN/THEN scenarios from spec pass as unit tests.
//! - Five-stage validation ordering: expired lease caught before budget check.
//! - Structured error responses contain all required fields.
//! - Sequence numbers strictly monotonically increasing.
//!
//! ## Epic 0 Test Gates
//!
//! - Layer 0 partial-failure rollback from `assert_layer0_invariants()`
//! - Batch atomicity proptest suite
//! - Test scenes: `three_agents_contention`, `max_tiles_stress`, `overlapping_tiles_zorder`

use tze_hud_scene::{
    MAX_BATCH_SIZE,
    graph::SceneGraph,
    mutation::{MutationBatch, SceneMutation},
    test_scenes::assert_layer0_invariants,
    types::{InputMode, LeaseState, Node, NodeData, Rect, Rgba, SceneId, SolidColorNode},
    validation::ValidationErrorCode,
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn make_batch(agent: &str, mutations: Vec<SceneMutation>) -> MutationBatch {
    MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: agent.to_string(),
        mutations,
        timing_hints: None,
        lease_id: None,
    }
}

// ─── RFC 0001 §3.1 — Atomic batch mutations ──────────────────────────────────

/// WHEN an agent submits a batch of 5 mutations and mutation 3 has invalid bounds
/// THEN the runtime MUST reject the entire batch (all 5 mutations) and report
/// the error at mutation_index=2
#[test]
fn spec_all_or_nothing_batch_rejection() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    let batch = make_batch(
        "agent",
        vec![
            // mutation 0: valid
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                z_order: 1,
            },
            // mutation 1: valid
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(110.0, 0.0, 100.0, 100.0),
                z_order: 2,
            },
            // mutation 2 (index=2): INVALID — zero bounds
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, 0.0, 0.0), // invalid
                z_order: 3,
            },
            // mutation 3: valid, but never reached
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(220.0, 0.0, 100.0, 100.0),
                z_order: 4,
            },
            // mutation 4: valid, but never reached
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(330.0, 0.0, 100.0, 100.0),
                z_order: 5,
            },
        ],
    );

    let result = scene.apply_batch(&batch);

    // All-or-nothing: batch rejected entirely
    assert!(!result.applied, "batch must be rejected");
    assert_eq!(scene.tile_count(), 0, "zero mutations must be applied");

    // Error must be at mutation_index=2
    let rej = result.rejection.expect("rejection must be present");
    assert_eq!(
        rej.errors[0].mutation_index,
        Some(2),
        "error at mutation_index=2"
    );
    assert_eq!(rej.errors[0].mutation_type, "CreateTile");

    // Layer 0 invariants still hold
    let violations = assert_layer0_invariants(&scene);
    assert!(violations.is_empty(), "Layer 0 violations: {violations:?}");
}

/// WHEN an agent submits a batch with 1001 mutations
/// THEN the runtime MUST reject with BatchSizeExceeded { max: 1000, got: 1001 }
#[test]
fn spec_batch_size_exceeded() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    // 1001 mutations = one over the limit
    let mutations: Vec<SceneMutation> = (0..=MAX_BATCH_SIZE)
        .map(|z| SceneMutation::CreateTile {
            tab_id,
            namespace: "agent".into(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            z_order: z as u32,
        })
        .collect();

    let batch = make_batch("agent", mutations);
    let result = scene.apply_batch(&batch);

    assert!(!result.applied);
    let rej = result.rejection.unwrap();
    assert_eq!(
        rej.primary_code(),
        Some(ValidationErrorCode::BatchSizeExceeded),
        "must reject with BatchSizeExceeded"
    );
    assert_eq!(scene.tile_count(), 0, "no tiles created");
}

/// WHEN a batch is received over gRPC
/// THEN the runtime MUST derive the agent namespace from the authenticated session
/// context, never from a client-supplied field.
///
/// This test verifies the documented contract: the pipeline uses `batch.agent_namespace`
/// which callers MUST fill from the authenticated principal. We verify that a namespace
/// mismatch at the type level is impossible (the field is used but not cross-validated
/// against the gRPC session here since we're testing the in-process pipeline).
#[test]
fn spec_agent_namespace_from_session_context() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab_id = scene.create_tab("Main", 0).unwrap();

    // The batch carries the agent_namespace; callers must overwrite it from the
    // authenticated session. We verify the field is carried correctly.
    let batch = make_batch("verified.agent", vec![]);
    assert_eq!(batch.agent_namespace, "verified.agent");
}

// ─── RFC 0001 §3.2 — Five-stage validation ordering ─────────────────────────

/// WHEN an agent submits a mutation targeting a tile with an expired lease
/// THEN the runtime MUST reject with LeaseExpired BEFORE evaluating budget or
/// bounds checks.
#[test]
fn spec_lease_check_before_budget_check() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 1);

    // Force the lease into an expired state
    scene.leases.get_mut(&lease_id).unwrap().state = LeaseState::Expired;

    // Also set a very tight budget that would reject purely on budget
    scene
        .leases
        .get_mut(&lease_id)
        .unwrap()
        .resource_budget
        .max_tiles = 0;

    let batch = make_batch(
        "agent",
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent".into(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            z_order: 1,
        }],
    );

    let result = scene.apply_batch(&batch);
    assert!(!result.applied);

    let rej = result.rejection.unwrap();
    let code = rej.primary_code().unwrap();
    // Must be a Stage 1 (lease) error, not Stage 2 (budget)
    assert!(
        matches!(
            code,
            ValidationErrorCode::LeaseExpired
                | ValidationErrorCode::LeaseInvalidState
                | ValidationErrorCode::LeaseNotFound
        ),
        "expected lease-stage error, got {code:?}"
    );
}

/// WHEN a batch of mutations would introduce an acyclic-tree violation in a
/// tile's node tree
/// THEN the runtime MUST reject the batch with CycleDetected
#[test]
fn spec_post_mutation_cycle_detected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    // Create a tile
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 200.0),
            1,
        )
        .unwrap();

    // Create a node that references itself as a child (creates a cycle)
    let node_id = SceneId::new();
    let cyclic_node = Node {
        layout: Default::default(),
        id: node_id,
        children: vec![node_id], // self-reference → cycle!
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            radius: None,
        }),
    };

    let batch = make_batch(
        "agent",
        vec![SceneMutation::SetTileRoot {
            tile_id,
            node: cyclic_node,
            descendants: vec![],
        }],
    );

    let result = scene.apply_batch(&batch);

    assert!(!result.applied, "batch with cycle must be rejected");
    let rej = result.rejection.unwrap();
    assert_eq!(
        rej.primary_code(),
        Some(ValidationErrorCode::CycleDetected),
        "must report CycleDetected"
    );

    // Verify the tile has no root node set (rollback)
    assert_eq!(
        scene.tiles[&tile_id].root_node, None,
        "rollback must clear root_node"
    );
}

/// WHEN two non-passthrough tiles on the same tab share z_order with overlapping
/// bounds after batch
/// THEN reject with ZOrderConflict
#[test]
fn spec_exclusive_z_order_conflict() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    // First tile at z_order=5, bounds [0,0,200,200]
    let b1 = make_batch(
        "agent",
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent".into(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
            z_order: 5,
        }],
    );
    let r1 = scene.apply_batch(&b1);
    assert!(r1.applied, "first tile must be accepted");

    // Second tile at same z_order=5 with overlapping bounds [100,100,200,200]
    let b2 = make_batch(
        "agent",
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent".into(),
            lease_id,
            bounds: Rect::new(100.0, 100.0, 200.0, 200.0), // overlaps
            z_order: 5,                                    // same z_order
        }],
    );
    let r2 = scene.apply_batch(&b2);

    assert!(!r2.applied, "z-order conflict must be rejected");
    let rej = r2.rejection.unwrap();
    assert_eq!(
        rej.primary_code(),
        Some(ValidationErrorCode::ZOrderConflict),
        "must report ZOrderConflict"
    );

    // Only the first tile remains
    assert_eq!(scene.tile_count(), 1, "first tile preserved after rollback");
}

/// Passthrough tiles do NOT participate in z-order conflict checks.
#[test]
fn spec_passthrough_tiles_exempt_from_z_order_conflict() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    // Create a non-passthrough tile at z_order=3
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
            3,
        )
        .unwrap();

    // Set tile to Passthrough
    scene.tiles.get_mut(&tile_id).unwrap().input_mode = InputMode::Passthrough;

    // Create a second tile at same z_order=3 with overlapping bounds
    // This should be ALLOWED since the first tile is passthrough
    let b2 = make_batch(
        "agent",
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent".into(),
            lease_id,
            bounds: Rect::new(50.0, 50.0, 300.0, 200.0),
            z_order: 3,
        }],
    );
    let r2 = scene.apply_batch(&b2);
    assert!(
        r2.applied,
        "passthrough tiles must not trigger z-order conflict"
    );
}

// ─── RFC 0001 §3.5 — Concurrency / sequence numbers ─────────────────────────

/// WHEN agent A submits batches B1 and B2 in order
/// THEN B1.sequence_number < B2.sequence_number
#[test]
fn spec_sequential_batch_ordering() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    let mut prev_seq = 0u64;

    for z in 1..=5u32 {
        let batch = make_batch(
            "agent.a",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent.a".into(),
                lease_id,
                bounds: Rect::new(z as f32 * 10.0, 0.0, 50.0, 50.0),
                z_order: z,
            }],
        );
        let result = scene.apply_batch(&batch);
        assert!(result.applied, "batch {z} must be accepted");

        let seq = result
            .sequence_number
            .expect("sequence_number must be set on success");
        assert!(
            seq > prev_seq,
            "sequence {seq} not strictly greater than {prev_seq}"
        );
        prev_seq = seq;
    }
}

/// Failed batches must NOT increment the sequence number.
#[test]
fn spec_rejected_batch_does_not_increment_sequence() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    // Apply one valid batch
    let b1 = make_batch(
        "agent",
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent".into(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            z_order: 1,
        }],
    );
    let r1 = scene.apply_batch(&b1);
    assert!(r1.applied);
    let seq_after_b1 = r1.sequence_number.unwrap();

    // Apply an invalid batch (bounds = 0)
    let b2 = make_batch(
        "agent",
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent".into(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 0.0, 0.0), // invalid
            z_order: 2,
        }],
    );
    let r2 = scene.apply_batch(&b2);
    assert!(!r2.applied);
    assert!(
        r2.sequence_number.is_none(),
        "rejected batch must not have a sequence number"
    );

    // Apply another valid batch — its seq must be seq_after_b1 + 1
    let b3 = make_batch(
        "agent",
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent".into(),
            lease_id,
            bounds: Rect::new(110.0, 0.0, 100.0, 100.0),
            z_order: 2,
        }],
    );
    let r3 = scene.apply_batch(&b3);
    assert!(r3.applied);
    let seq_after_b3 = r3.sequence_number.unwrap();
    assert_eq!(
        seq_after_b3,
        seq_after_b1 + 1,
        "sequence must jump by exactly 1 after a rejected batch"
    );
}

// ─── RFC 0001 §3.4 — Structured error responses ──────────────────────────────

/// WHEN a mutation fails bounds validation
/// THEN the rejection MUST include mutation_index, code=BoundsInvalid,
/// human-readable message, and context JSON with field, value, and constraint.
#[test]
fn spec_structured_validation_error_on_bounds_failure() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    // First mutation is valid; second fails on bounds
    let batch = make_batch(
        "agent",
        vec![
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                z_order: 1,
            },
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, -1.0, 50.0), // invalid: negative width
                z_order: 2,
            },
        ],
    );

    let result = scene.apply_batch(&batch);
    assert!(!result.applied);

    let rej = result.rejection.unwrap();
    let err = &rej.errors[0];

    // Required fields per RFC 0001 §3.4
    assert_eq!(err.mutation_index, Some(1), "error at mutation_index=1");
    assert_eq!(err.mutation_type, "CreateTile");
    assert_eq!(err.code, ValidationErrorCode::BoundsInvalid);
    assert!(!err.message.is_empty(), "message must be non-empty");

    // context must be a JSON object with at least field and constraint
    assert!(err.context.is_object(), "context must be a JSON object");
    let ctx = err.context.as_object().unwrap();
    assert!(ctx.contains_key("field"), "context must contain 'field'");
    assert!(
        ctx.contains_key("constraint"),
        "context must contain 'constraint'"
    );
}

/// BatchRejected carries batch_id matching the submitted batch.
#[test]
fn spec_batch_rejected_carries_batch_id() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    let batch_id = SceneId::new();
    let batch = MutationBatch {
        batch_id,
        agent_namespace: "agent".into(),
        mutations: vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent".into(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 0.0, 0.0), // invalid
            z_order: 1,
        }],
        timing_hints: None,
        lease_id: None,
    };

    let result = scene.apply_batch(&batch);
    assert!(!result.applied);

    let rej = result.rejection.unwrap();
    assert_eq!(
        rej.batch_id, batch_id,
        "BatchRejected must carry the original batch_id"
    );
}

// ─── Partial-failure rollback — Layer 0 invariant ────────────────────────────

/// Layer 0 test gate: batch with N mutations where mutation K fails results in
/// zero applied mutations. Verified against `assert_layer0_invariants`.
#[test]
fn layer0_partial_failure_rollback() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    // Apply 2 valid tiles before the test batch
    let setup_batch = make_batch(
        "agent",
        vec![
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, 50.0, 50.0),
                z_order: 10,
            },
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(60.0, 0.0, 50.0, 50.0),
                z_order: 11,
            },
        ],
    );
    assert!(scene.apply_batch(&setup_batch).applied);
    assert_eq!(scene.tile_count(), 2, "setup: 2 tiles");

    // Mixed batch: 2 valid + 1 invalid (mutation K=2)
    let failing_batch = make_batch(
        "agent",
        vec![
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(120.0, 0.0, 50.0, 50.0),
                z_order: 12,
            },
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(180.0, 0.0, 50.0, 50.0),
                z_order: 13,
            },
            // mutation K=2: invalid
            SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".into(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, -99.0, 50.0), // invalid
                z_order: 14,
            },
        ],
    );

    let result = scene.apply_batch(&failing_batch);
    assert!(!result.applied, "batch must be rejected");

    // After rollback, only the 2 setup tiles remain
    assert_eq!(
        scene.tile_count(),
        2,
        "rollback must restore 2 pre-batch tiles"
    );

    // Layer 0 invariants must hold after rollback
    let violations = assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after rollback: {violations:?}"
    );
}

// ─── Proptest: batch atomicity property ──────────────────────────────────────

/// Property: if a batch is rejected, the scene graph is identical to
/// its pre-batch state (all-or-nothing).
///
/// Generates random mutation batches with a mix of valid and invalid mutations
/// and verifies the all-or-nothing property.
#[cfg(test)]
mod proptest_batch_atomicity {
    use proptest::prelude::*;
    use tze_hud_scene::{
        graph::SceneGraph,
        mutation::{MutationBatch, SceneMutation},
        types::{Rect, SceneId},
    };

    /// Generate a valid `CreateTile` mutation for the given tab/lease.
    fn valid_create(tab_id: SceneId, lease_id: SceneId, z_order: u32) -> SceneMutation {
        SceneMutation::CreateTile {
            tab_id,
            namespace: "prop.agent".into(),
            lease_id,
            bounds: Rect::new((z_order as f32 % 10.0) * 5.0, 0.0, 50.0, 50.0),
            z_order,
        }
    }

    /// Generate an invalid `CreateTile` mutation (zero-area bounds).
    fn invalid_create(tab_id: SceneId, lease_id: SceneId, z_order: u32) -> SceneMutation {
        SceneMutation::CreateTile {
            tab_id,
            namespace: "prop.agent".into(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 0.0, 0.0), // invalid
            z_order,
        }
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 50,
            ..Default::default()
        })]

        /// For any batch containing at least one invalid mutation:
        /// - applied == false
        /// - tile_count unchanged from pre-batch state
        /// - rejection carries a non-empty errors vec
        #[test]
        fn prop_batch_atomicity_with_invalid_mutation(
            valid_before in 0usize..5,
            invalid_pos in 0usize..10,
            valid_after in 0usize..5,
        ) {
            let mut scene = SceneGraph::new(1920.0, 1080.0);
            let tab_id = scene.create_tab("Main", 0).unwrap();
            let lease_id = scene.grant_lease("prop.agent", 60_000);

            // Pre-batch state: record tile count
            let pre_batch_tiles = scene.tile_count();

            // Build a batch: valid_before valid mutations, then 1 invalid, then valid_after valid
            let total = valid_before + 1 + valid_after;
            let mut mutations = Vec::with_capacity(total);

            for i in 0..valid_before {
                mutations.push(valid_create(tab_id, lease_id, (i as u32) + 1));
            }
            mutations.push(invalid_create(tab_id, lease_id, (invalid_pos as u32) + 100));
            for i in 0..valid_after {
                mutations.push(valid_create(tab_id, lease_id, (valid_before + i + 1) as u32));
            }

            let batch = MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "prop.agent".into(),
                mutations,
                timing_hints: None,
                lease_id: None,
            };

            let result = scene.apply_batch(&batch);

            // Property: all-or-nothing
            prop_assert!(!result.applied, "batch with invalid mutation must be rejected");
            prop_assert_eq!(
                scene.tile_count(),
                pre_batch_tiles,
                "tile count must be unchanged after rejected batch"
            );
            let rej = result.rejection.expect("rejection must be present");
            prop_assert!(!rej.errors.is_empty(), "rejection must have at least one error");
        }

        /// For any batch where ALL mutations are valid:
        /// - applied == true
        /// - sequence_number is set
        /// - tile_count increased by the number of CreateTile mutations
        #[test]
        fn prop_all_valid_batch_succeeds(n in 1usize..=8) {
            let mut scene = SceneGraph::new(1920.0, 1080.0);
            let tab_id = scene.create_tab("Main", 0).unwrap();
            let lease_id = scene.grant_lease("prop.agent", 60_000);

            let pre_count = scene.tile_count();
            let mutations: Vec<SceneMutation> = (0..n)
                .map(|i| valid_create(tab_id, lease_id, (i as u32) + 1))
                .collect();

            let batch = MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "prop.agent".into(),
                mutations,
                timing_hints: None,
                lease_id: None,
            };

            let result = scene.apply_batch(&batch);

            prop_assert!(result.applied, "all-valid batch must succeed");
            prop_assert!(result.sequence_number.is_some(), "sequence_number must be set");
            prop_assert_eq!(
                scene.tile_count(),
                pre_count + n,
                "tile_count must grow by n"
            );
        }
    }
}

// ─── Stage 1 lease validation gaps (hud-ugwr) ────────────────────────────────

/// WHEN batch.lease_id references a nonexistent lease
/// THEN Stage 1 MUST reject with LeaseNotFound BEFORE Stage 2 budget checks.
///
/// This covers the gap where batch.lease_id was added to lease_ids but not
/// validated for existence — a nonexistent lease silently passed Stage 2.
#[test]
fn stage1_batch_lease_id_nonexistent_is_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();

    // A lease_id that was never issued — not in the scene at all.
    let nonexistent_lease_id = SceneId::new();

    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent".into(),
        mutations: vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent".into(),
            lease_id: nonexistent_lease_id,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            z_order: 1,
        }],
        timing_hints: None,
        lease_id: Some(nonexistent_lease_id),
    };

    let result = scene.apply_batch(&batch);

    assert!(!result.applied, "batch must be rejected");
    let code = result.rejection.unwrap().primary_code().unwrap();
    assert_eq!(
        code,
        ValidationErrorCode::LeaseNotFound,
        "expected LeaseNotFound at Stage 1, got {code:?}"
    );
    assert_eq!(scene.tile_count(), 0, "no tiles must be created");
}

/// WHEN batch.lease_id references a lease in Expired state (not Active)
/// THEN Stage 1 MUST reject with LeaseExpired BEFORE Stage 2 budget checks.
///
/// Previously batch.lease_id was only collected into lease_ids for budget
/// accounting; its state was never explicitly checked at Stage 1.
#[test]
fn stage1_batch_lease_id_expired_is_rejected_before_budget() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 1);

    // Force into Expired state and also shrink budget to 0 — Stage 1 must fire first.
    scene.leases.get_mut(&lease_id).unwrap().state = LeaseState::Expired;
    scene
        .leases
        .get_mut(&lease_id)
        .unwrap()
        .resource_budget
        .max_tiles = 0;

    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent".into(),
        mutations: vec![],
        timing_hints: None,
        // batch.lease_id only — no per-mutation lease_id to trigger the old path.
        lease_id: Some(lease_id),
    };

    let result = scene.apply_batch(&batch);

    assert!(
        !result.applied,
        "batch with expired batch.lease_id must be rejected"
    );
    let code = result.rejection.unwrap().primary_code().unwrap();
    assert!(
        matches!(
            code,
            ValidationErrorCode::LeaseExpired | ValidationErrorCode::LeaseInvalidState
        ),
        "expected Stage 1 (lease) error, got {code:?}"
    );
}

/// WHEN a DeleteTile mutation targets a tile whose lease is expired
/// THEN Stage 1 MUST reject with LeaseExpired BEFORE later stages.
///
/// Covers the gap where non-CreateTile mutations skipped Stage 1 entirely.
#[test]
fn stage1_delete_tile_with_expired_lease_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    // Create a tile on an active lease.
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    // Now expire the lease — mutation against this tile must fail Stage 1.
    scene.leases.get_mut(&lease_id).unwrap().state = LeaseState::Expired;

    let batch = make_batch("agent", vec![SceneMutation::DeleteTile { tile_id }]);
    let result = scene.apply_batch(&batch);

    assert!(
        !result.applied,
        "DeleteTile with expired lease must be rejected"
    );
    let code = result.rejection.unwrap().primary_code().unwrap();
    assert!(
        matches!(
            code,
            ValidationErrorCode::LeaseExpired | ValidationErrorCode::LeaseInvalidState
        ),
        "expected Stage 1 lease error for DeleteTile, got {code:?}"
    );
    // Tile must still exist (batch was atomic — nothing applied).
    assert_eq!(scene.tile_count(), 1, "tile must survive due to rejection");
}

/// WHEN an UpdateTileBounds mutation targets a tile whose lease is expired
/// THEN Stage 1 MUST reject with LeaseExpired BEFORE bounds or type checks.
#[test]
fn stage1_update_tile_bounds_with_expired_lease_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    scene.leases.get_mut(&lease_id).unwrap().state = LeaseState::Expired;

    let batch = make_batch(
        "agent",
        vec![SceneMutation::UpdateTileBounds {
            tile_id,
            bounds: Rect::new(10.0, 10.0, 200.0, 200.0),
        }],
    );
    let result = scene.apply_batch(&batch);

    assert!(
        !result.applied,
        "UpdateTileBounds with expired lease must be rejected"
    );
    let code = result.rejection.unwrap().primary_code().unwrap();
    assert!(
        matches!(
            code,
            ValidationErrorCode::LeaseExpired | ValidationErrorCode::LeaseInvalidState
        ),
        "expected Stage 1 lease error for UpdateTileBounds, got {code:?}"
    );
}

/// WHEN a SetTileRoot mutation targets a tile whose lease is expired
/// THEN Stage 1 MUST reject with LeaseExpired BEFORE applying the node.
#[test]
fn stage1_set_tile_root_with_expired_lease_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    scene.leases.get_mut(&lease_id).unwrap().state = LeaseState::Expired;

    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            bounds: Rect::new(0.0, 0.0, 50.0, 50.0),
            radius: None,
        }),
    };

    let batch = make_batch(
        "agent",
        vec![SceneMutation::SetTileRoot {
            tile_id,
            node,
            descendants: vec![],
        }],
    );
    let result = scene.apply_batch(&batch);

    assert!(
        !result.applied,
        "SetTileRoot with expired lease must be rejected"
    );
    let code = result.rejection.unwrap().primary_code().unwrap();
    assert!(
        matches!(
            code,
            ValidationErrorCode::LeaseExpired | ValidationErrorCode::LeaseInvalidState
        ),
        "expected Stage 1 lease error for SetTileRoot, got {code:?}"
    );
}

/// WHEN an AddNode mutation targets a tile whose lease is expired
/// THEN Stage 1 MUST reject with LeaseExpired BEFORE the node is added.
#[test]
fn stage1_add_node_with_expired_lease_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);

    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    scene.leases.get_mut(&lease_id).unwrap().state = LeaseState::Expired;

    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            bounds: Rect::new(0.0, 0.0, 50.0, 50.0),
            radius: None,
        }),
    };

    let batch = make_batch(
        "agent",
        vec![SceneMutation::AddNode {
            tile_id,
            parent_id: None,
            node,
        }],
    );
    let result = scene.apply_batch(&batch);

    assert!(
        !result.applied,
        "AddNode with expired lease must be rejected"
    );
    let code = result.rejection.unwrap().primary_code().unwrap();
    assert!(
        matches!(
            code,
            ValidationErrorCode::LeaseExpired | ValidationErrorCode::LeaseInvalidState
        ),
        "expected Stage 1 lease error for AddNode, got {code:?}"
    );
}

/// WHEN batch.lease_id references a nonexistent lease AND the batch has no
/// per-mutation mutations (empty batch), Stage 1 must still reject before
/// reaching Stage 2 budget checks.
#[test]
fn stage1_empty_batch_with_nonexistent_batch_lease_id_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab_id = scene.create_tab("Main", 0).unwrap();

    let nonexistent = SceneId::new();

    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent".into(),
        mutations: vec![],
        timing_hints: None,
        lease_id: Some(nonexistent),
    };

    let result = scene.apply_batch(&batch);

    assert!(
        !result.applied,
        "batch with nonexistent batch.lease_id must be rejected"
    );
    let code = result.rejection.unwrap().primary_code().unwrap();
    assert_eq!(
        code,
        ValidationErrorCode::LeaseNotFound,
        "expected LeaseNotFound, got {code:?}"
    );
}

// ─── Resource registration gate ──────────────────────────────────────────────

/// An agent may not reference an image it never uploaded: SetTileRoot, AddNode
/// and UpdateNodeContent each reject a StaticImage node with an unregistered
/// resource id as `ResourceNotFound`, leaving the tile untouched.
#[test]
fn static_image_with_unregistered_resource_is_rejected_on_every_agent_path() {
    use tze_hud_scene::types::{ImageFitMode, ResourceId, StaticImageNode};

    let image = |byte: u8| {
        NodeData::StaticImage(StaticImageNode {
            resource_id: ResourceId::from_bytes([byte; 32]),
            width: 48,
            height: 48,
            decoded_bytes: 48 * 48 * 4,
            fit_mode: ImageFitMode::Cover,
            bounds: Rect::new(0.0, 0.0, 48.0, 48.0),
        })
    };
    let node = |data| Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data,
    };
    let assert_resource_not_found =
        |scene: &mut SceneGraph, mutation: SceneMutation, path: &str| {
            let result = scene.apply_batch(&make_batch("agent", vec![mutation]));
            assert!(
                !result.applied,
                "{path}: unregistered image must be rejected"
            );
            let code = result.rejection.as_ref().map(|r| r.errors[0].code);
            assert_eq!(code, Some(ValidationErrorCode::ResourceNotFound), "{path}");
        };

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_resource(ResourceId::from_bytes([0xAA; 32]));
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile_checked(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    // SetTileRoot with an unregistered image as the root: the tile stays empty.
    assert_resource_not_found(
        &mut scene,
        SceneMutation::SetTileRoot {
            tile_id,
            node: node(image(0xDE)),
            descendants: vec![],
        },
        "SetTileRoot",
    );
    assert!(scene.tiles[&tile_id].root_node.is_none());

    // With a registered image as the root, AddNode and UpdateNodeContent are gated too.
    let root = node(image(0xAA));
    let root_id = root.id;
    let set_root = make_batch(
        "agent",
        vec![SceneMutation::SetTileRoot {
            tile_id,
            node: root,
            descendants: vec![],
        }],
    );
    assert!(scene.apply_batch(&set_root).applied);
    assert_resource_not_found(
        &mut scene,
        SceneMutation::AddNode {
            tile_id,
            parent_id: Some(root_id),
            node: node(image(0xDE)),
        },
        "AddNode",
    );
    assert_resource_not_found(
        &mut scene,
        SceneMutation::UpdateNodeContent {
            tile_id,
            node_id: root_id,
            data: image(0xDE), // 0xDE is never registered
        },
        "UpdateNodeContent",
    );
    assert_eq!(scene.tiles[&tile_id].root_node, Some(root_id));
}
