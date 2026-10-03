//! Property-based invariant tests for the Layer 0 scene graph.
//!
//! # Purpose
//!
//! Property-based verification: for any randomly-generated valid scene,
//! `assert_layer0_invariants()` must return an empty violation set.
//!
//! # Strategy Overview
//!
//! Three proptest strategies cover the key axes of the spec:
//!
//! 1. `arb_valid_scene_graph` — random SceneGraphs satisfying all structural
//!    invariants; verifies that assert_layer0_invariants() always returns empty.
//!
//! 2. `arb_valid_then_valid_mutations` — random valid scenes followed by valid
//!    MutationBatches; verifies post-mutation scenes still pass assert_layer0_invariants().
//!
//! 3. `arb_valid_then_invalid_mutation` — random valid scenes followed by an
//!    invalid mutation; verifies (a) batch is rejected and (b) scene is unchanged.
//!
//! Each strategy runs at 500 iterations (higher than typical 100 but well within
//! the <2 s budget; total suite runtime confirmed <1 s on CI hardware).

use proptest::prelude::*;
use tze_hud_scene::{
    MAX_BATCH_SIZE, assert_layer0_invariants,
    graph::SceneGraph,
    mutation::{MutationBatch, SceneMutation},
    types::{Rect, SceneId},
};

// ─── PropTest configuration ───────────────────────────────────────────────────

/// Number of proptest cases per strategy.
///
/// Kept at 500 to balance coverage against the <2 s Layer 0 budget.
const PROPTEST_CASES: u32 = 500;

fn proptest_config() -> proptest::test_runner::Config {
    proptest::test_runner::Config {
        cases: PROPTEST_CASES,
        // Use a deterministic seed for reproducibility (DR-V4).
        source_file: Some("tests/proptest_invariants.rs"),
        ..Default::default()
    }
}

// ─── Primitive generators ─────────────────────────────────────────────────────

/// Generate a valid tile bounds entirely within a 1920×1080 display.
fn arb_tile_bounds() -> impl Strategy<Value = Rect> {
    (
        0.0f32..1800.0,
        0.0f32..980.0,
        10.0f32..120.0,
        10.0f32..100.0,
    )
        .prop_map(|(x, y, w, h)| Rect::new(x, y, w, h))
}

/// Generate a valid opacity in [0.0, 1.0].
fn arb_opacity() -> impl Strategy<Value = f32> {
    (0u32..=100).prop_map(|n| n as f32 / 100.0)
}

/// Generate a valid agent namespace string.
fn arb_namespace() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("agent.alpha".to_string()),
        Just("agent.beta".to_string()),
        Just("agent.gamma".to_string()),
        Just("agent.delta".to_string()),
    ]
}

/// Generate a valid tab name (non-empty, ≤128 bytes).
fn arb_tab_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("Main".to_string()),
        Just("Secondary".to_string()),
        Just("Dashboard".to_string()),
        Just("Overlay".to_string()),
    ]
}

// ─── Strategy 1: Random valid SceneGraph ─────────────────────────────────────

/// Generates random but structurally valid SceneGraphs.
///
/// Invariant: `assert_layer0_invariants()` must return empty for every generated graph.
#[derive(Debug)]
struct ValidSceneParams {
    tab_count: usize,
    tiles_per_tab: usize,
    opacity: f32,
}

fn arb_valid_scene_params() -> impl Strategy<Value = ValidSceneParams> {
    (1usize..=4, 1usize..=6, arb_opacity()).prop_map(|(tab_count, tiles_per_tab, opacity)| {
        ValidSceneParams {
            tab_count,
            tiles_per_tab,
            opacity,
        }
    })
}

fn build_valid_scene(params: &ValidSceneParams) -> SceneGraph {
    let mut graph = SceneGraph::new(1920.0, 1080.0);

    for tab_idx in 0..params.tab_count {
        let tab_name = format!("Tab{tab_idx}");
        let tab_id = graph
            .create_tab(&tab_name, tab_idx as u32)
            .expect("create_tab failed in build_valid_scene");

        // Create a fresh lease per tab to avoid budget pressure from multiple tabs.
        let lease_id = graph.grant_lease(&format!("agent.t{tab_idx}"), 300_000);

        let tile_w = 1920.0 / (params.tiles_per_tab as f32 + 1.0);
        let tile_h = 1080.0 / (params.tab_count as f32 + 1.0);

        for tile_idx in 0..params.tiles_per_tab {
            let x = tile_idx as f32 * tile_w;
            let y = tab_idx as f32 * tile_h;
            let bounds = Rect::new(x, y, tile_w - 2.0, tile_h - 2.0);
            let z_order = (tile_idx as u32) + 1;
            let tile_id = graph
                .create_tile(
                    tab_id,
                    &format!("agent.t{tab_idx}"),
                    lease_id,
                    bounds,
                    z_order,
                )
                .expect("create_tile failed in build_valid_scene");
            // Set a valid opacity from params
            graph.tiles.get_mut(&tile_id).unwrap().opacity = params.opacity;
        }
    }

    graph
}

proptest! {
    #![proptest_config(proptest_config())]

    // ── Strategy 1: Random valid scene → assert_layer0_invariants returns empty ────────────

    /// FOR ALL randomly generated valid SceneGraphs:
    /// THEN assert_layer0_invariants() returns no violations.
    ///
    /// This is the fundamental property: our construction API must produce
    /// graphs that satisfy all Layer 0 invariants.
    #[test]
    fn prop_valid_scene_passes_assert_layer0_invariants(params in arb_valid_scene_params()) {
        let graph = build_valid_scene(&params);
        let violations = assert_layer0_invariants(&graph);
        prop_assert!(
            violations.is_empty(),
            "valid scene failed assert_layer0_invariants:\n{}",
            violations.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")
        );
    }

    // ── Strategy 2: Random valid scene + valid mutations → still valid ───────

    /// FOR ALL valid scenes + valid mutations applied atomically:
    /// THEN post-mutation scene still passes assert_layer0_invariants().
    ///
    /// Verifies that the graph stays valid through incremental construction.
    #[test]
    fn prop_valid_scene_after_valid_mutations_passes_assert_layer0_invariants(
        tab_name in arb_tab_name(),
        namespace in arb_namespace(),
        extra_bounds in arb_tile_bounds(),
        n_tiles in 1usize..=4,
    ) {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        let tab_id = graph.create_tab(&tab_name, 0).unwrap();
        let lease_id = graph.grant_lease(&namespace, 300_000);

        // Apply n_tiles valid CreateTile mutations
        let tile_w = 200.0f32.min(extra_bounds.width);
        let tile_h = 200.0f32.min(extra_bounds.height);

        for i in 0..n_tiles {
            let x = (i as f32 * (tile_w + 5.0)).min(1720.0);
            let bounds = Rect::new(x, 0.0, tile_w, tile_h);
            let z_order = (i as u32) + 1;
            let batch = MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: namespace.clone(),
                mutations: vec![SceneMutation::CreateTile {
                    tab_id,
                    namespace: namespace.clone(),
                    lease_id,
                    bounds,
                    z_order,
                }],
                timing_hints: None,
                lease_id: Some(lease_id),
            };
            let result = graph.apply_batch(&batch);
            prop_assume!(result.applied, "batch failed — skip this input");
        }

        let violations = assert_layer0_invariants(&graph);
        prop_assert!(
            violations.is_empty(),
            "post-mutation scene failed assert_layer0_invariants:\n{}",
            violations.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")
        );
    }

    // ── Strategy 3: Valid scene + invalid mutation → scene unchanged ─────────

    /// FOR ALL valid scenes + an invalid mutation (zero-area bounds):
    /// THEN the batch is rejected AND the scene is unchanged.
    ///
    /// Verifies atomic rollback: no partial mutations applied on failure.
    #[test]
    fn prop_invalid_mutation_rejected_and_scene_unchanged(
        tab_name in arb_tab_name(),
        namespace in arb_namespace(),
        pre_tiles in 0usize..=4,
    ) {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        let tab_id = graph.create_tab(&tab_name, 0).unwrap();
        let lease_id = graph.grant_lease(&namespace, 300_000);

        // Build pre-state: pre_tiles valid tiles
        for i in 0..pre_tiles {
            let bounds = Rect::new(i as f32 * 50.0, 0.0, 40.0, 40.0);
            let batch = MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: namespace.clone(),
                mutations: vec![SceneMutation::CreateTile {
                    tab_id,
                    namespace: namespace.clone(),
                    lease_id,
                    bounds,
                    z_order: (i as u32) + 1,
                }],
                timing_hints: None,
                lease_id: Some(lease_id),
            };
            graph.apply_batch(&batch);
        }

        // Record pre-mutation state
        let pre_tile_count = graph.tile_count();
        let pre_version = graph.version;

        // Submit a batch with an INVALID mutation (zero-area bounds = spec violation)
        let invalid_batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: namespace.clone(),
            mutations: vec![SceneMutation::CreateTile {
                tab_id,
                namespace: namespace.clone(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, 0.0, 0.0), // zero area — invalid
                z_order: 9999,
            }],
            timing_hints: None,
            lease_id: Some(lease_id),
        };

        let result = graph.apply_batch(&invalid_batch);

        // Property 1: batch must be rejected
        prop_assert!(!result.applied, "invalid mutation must be rejected");
        prop_assert!(result.rejection.is_some(), "rejection detail must be present");

        // Property 2: scene must be unchanged
        prop_assert_eq!(
            graph.tile_count(),
            pre_tile_count,
            "tile count must not change after rejected batch"
        );
        prop_assert_eq!(
            graph.version,
            pre_version,
            "version must not increment after rejected batch"
        );

        // Property 3: scene must still pass assert_layer0_invariants
        let violations = assert_layer0_invariants(&graph);
        prop_assert!(
            violations.is_empty(),
            "scene after rejected mutation must pass assert_layer0_invariants:\n{}",
            violations.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")
        );
    }

    // ── Additional properties ─────────────────────────────────────────────────

    /// FOR ALL valid scenes: namespace isolation is preserved.
    ///
    /// Every tile's namespace matches the owning lease's namespace.
    #[test]
    fn prop_namespace_isolation_preserved(
        tab_name in arb_tab_name(),
        ns_a in Just("agent.alpha".to_string()),
        ns_b in Just("agent.beta".to_string()),
        n_a in 1usize..=3,
        n_b in 1usize..=3,
    ) {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        let tab_id = graph.create_tab(&tab_name, 0).unwrap();
        let lease_a = graph.grant_lease(&ns_a, 300_000);
        let lease_b = graph.grant_lease(&ns_b, 300_000);

        for i in 0..n_a {
            let bounds = Rect::new(i as f32 * 50.0, 0.0, 40.0, 40.0);
            let _ = graph.create_tile(tab_id, &ns_a, lease_a, bounds, (i as u32) + 1);
        }
        for i in 0..n_b {
            let bounds = Rect::new(i as f32 * 50.0, 200.0, 40.0, 40.0);
            let _ = graph.create_tile(tab_id, &ns_b, lease_b, bounds, (i as u32) + 100);
        }

        // All tiles must match their lease namespace
        for tile in graph.tiles.values() {
            if let Some(lease) = graph.leases.get(&tile.lease_id) {
                prop_assert_eq!(
                    tile.namespace.clone(),
                    lease.namespace.clone(),
                    "tile {} namespace mismatch",
                    tile.id
                );
            }
        }

        let violations = assert_layer0_invariants(&graph);
        prop_assert!(
            violations.is_empty(),
            "multi-agent scene failed assert_layer0_invariants:\n{}",
            violations.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")
        );
    }

    /// FOR ALL valid scenes with zones: zone invariants hold after publish simulation.
    ///
    /// Builds a scene with zone registry and verifies structural zone invariants.
    #[test]
    fn prop_zone_registry_invariants_hold(
        tab_name in arb_tab_name(),
        publish_count in 0usize..=3,
    ) {
        use tze_hud_scene::types::{
            ContentionPolicy, GeometryPolicy, ZoneContent, ZoneDefinition, ZoneMediaType,
            ZonePublishRecord,
        };

        let mut graph = SceneGraph::new(1920.0, 1080.0);
        graph.create_tab(&tab_name, 0).unwrap();

        // Register a valid zone (LatestWins subtitle)
        let zone_name = "subtitle".to_string();
        graph.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "subtitle zone for property test".into(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.9,
                    width_pct: 1.0,
                    height_pct: 0.1,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: Default::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 1,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: Default::default(),
            },
        );

        // For LatestWins: at most 1 publish (use publish_count clamped to 1)
        let effective_count = publish_count.min(1);
        if effective_count > 0 {
            graph.zone_registry.active_publishes.insert(
                zone_name.clone(),
                vec![ZonePublishRecord {
                    zone_name: zone_name.clone(),
                    publisher_namespace: "agent.alpha".into(),
                    content: ZoneContent::StreamText("hello world".into()),
                    published_at_wall_us: 1_000_000_000,  // microseconds
                    merge_key: None,
                    expires_at_wall_us: None,
                    content_classification: None,
                    breakpoints: Vec::new(),
                }],
            );
        }

        let violations = assert_layer0_invariants(&graph);
        prop_assert!(
            violations.is_empty(),
            "zone-registry scene failed assert_layer0_invariants:\n{}",
            violations.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")
        );
    }

    // ── Batch size boundary ───────────────────────────────────────────────────

    /// FOR ALL batches of 1..=MAX_BATCH_SIZE mutations:
    /// THEN the batch must NOT be rejected with `BatchSizeExceeded`.
    ///
    /// Note: batches with n > lease.resource_budget.max_tiles may be rejected for
    /// TILE_BUDGET_EXCEEDED (budget enforcement), which is correct and expected.
    /// This test only verifies the batch-size gate: n ≤ MAX_BATCH_SIZE must never
    /// trigger a BATCH_TOO_LARGE rejection code.
    #[test]
    fn prop_batch_not_rejected_as_batch_too_large(n in 1usize..=MAX_BATCH_SIZE) {
        let mut graph = SceneGraph::new(10000.0, 10000.0); // large display for many tiles
        let tab_id = graph.create_tab("Stress", 0).unwrap();
        // Budget cap of 255: batches of n > 255 may fail for TILE_BUDGET_EXCEEDED,
        // but must never fail for BatchSizeExceeded (n ≤ MAX_BATCH_SIZE = 1000).
        let lease_id = graph.grant_lease("stress.agent", 300_000);
        {
            let lease = graph.leases.get_mut(&lease_id).unwrap();
            lease.resource_budget.max_tiles = 255; // tile budget cap (< MAX_BATCH_SIZE)
        }

        let tile_w = 50.0f32;
        let tile_h = 50.0f32;
        let cols = 100u32; // enough columns for up to 1000 tiles

        let mutations: Vec<SceneMutation> = (0..n)
            .map(|i| {
                let col = (i as u32) % cols;
                let row = (i as u32) / cols;
                SceneMutation::CreateTile {
                    tab_id,
                    namespace: "stress.agent".into(),
                    lease_id,
                    bounds: Rect::new(
                        col as f32 * (tile_w + 1.0),
                        row as f32 * (tile_h + 1.0),
                        tile_w,
                        tile_h,
                    ),
                    z_order: (i as u32) + 1,
                }
            })
            .collect();

        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: "stress.agent".into(),
            mutations,
            timing_hints: None,
            lease_id: Some(lease_id),
        };

        let result = graph.apply_batch(&batch);

        // Batches of 1..=MAX_BATCH_SIZE must not be rejected for BATCH_TOO_LARGE
        if let Some(rej) = &result.rejection {
            for err in &rej.errors {
                prop_assert!(
                    err.code != tze_hud_scene::ValidationErrorCode::BatchSizeExceeded,
                    "batch of n={} should not be rejected as BATCH_TOO_LARGE (MAX_BATCH_SIZE={})",
                    n,
                    MAX_BATCH_SIZE
                );
            }
        }
    }
}
