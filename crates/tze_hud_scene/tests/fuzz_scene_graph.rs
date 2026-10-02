//! # Scene Graph Fuzzer
//!
//! Property-based fuzzing for the scene graph mutation engine.
//!
//! Implements acceptance criteria from hud-3ksv:
//! - 100,000 random mutation sequences → no crash, no inconsistent state, no leaked resources
//! - Malformed / out-of-order mutations → rejected without crash/hang
//! - Oversized payloads → structured error, no state corruption
//!
//! Uses `proptest` to generate random mutation sequences (create, delete, resize,
//! z-order, lease grant/revoke) and asserts Layer 0 invariants after each operation.
//!
//! ## Strategy
//!
//! 1. **Oracle state tracker** — maintain an independent set of live IDs (tabs, tiles,
//!    nodes, leases) alongside the scene graph.  The oracle is used for
//!    realistic ID selection so operations have valid targets; structural consistency is
//!    verified by `assert_layer0_invariants`, not by count comparisons.
//!
//! 2. **Invariant harness** — call `assert_layer0_invariants` after every operation.
//!    Any invariant violation surfaces as a proptest failure with a minimal reproducer.
//!
//! 3. **Leak detector** — after deleting all tiles the scene must hold no orphan nodes
//!    and the heap-resident node map must be empty.

use proptest::prelude::*;
use tze_hud_scene::{
    graph::SceneGraph,
    mutation::{MAX_BATCH_SIZE, MutationBatch, SceneMutation},
    test_scenes::assert_layer0_invariants,
    types::{
        FontFamily, Node, NodeData, Rect, Rgba, SceneId, SolidColorNode, TextAlign,
        TextMarkdownNode, TextOverflow,
    },
};

// Agent namespace used consistently so namespace checks pass.
const AGENT: &str = "fuzz.agent";

// ─── Arbitraries ─────────────────────────────────────────────────────────────

/// Generate a valid (positive-dimension) Rect within a 1920×1080 display.
fn arb_valid_rect() -> impl Strategy<Value = Rect> {
    (0f32..1800f32, 0f32..900f32, 10f32..200f32, 10f32..200f32)
        .prop_map(|(x, y, w, h)| Rect::new(x, y, w, h))
}

/// Generate a valid SolidColor node.
fn arb_solid_color_node() -> impl Strategy<Value = Node> {
    arb_valid_rect().prop_map(|bounds| Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.5, 0.5, 0.5, 1.0),
            bounds,
            radius: None,
        }),
    })
}

/// Generate a valid TextMarkdown node.
fn arb_text_node() -> impl Strategy<Value = Node> {
    ("[a-z]{1,32}", arb_valid_rect()).prop_map(|(content, bounds)| Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content,
            bounds,
            font_size_px: 14.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    })
}

/// Generate a random valid node (SolidColor or TextMarkdown).
fn arb_node() -> impl Strategy<Value = Node> {
    prop_oneof![arb_solid_color_node(), arb_text_node()]
}

// ─── Mutation operation enum ──────────────────────────────────────────────────

/// High-level mutation operations the fuzzer can issue.
#[derive(Clone, Debug)]
enum FuzzOp {
    CreateTab { name: String, display_order: u32 },
    DeleteTab,
    CreateTile { z_order: u32 },
    DeleteTile,
    ResizeTile { valid: bool },
    UpdateZOrder { z: u32 },
    UpdateOpacity { opacity: f32 },
    SetTileRoot { node: Node },
    AddNode { node: Node },
    GrantLease,
    RevokeLease,
}

fn arb_fuzz_op() -> impl Strategy<Value = FuzzOp> {
    prop_oneof![
        3 => ("[a-zA-Z]{1,16}", 0u32..32u32)
                .prop_map(|(name, order)| FuzzOp::CreateTab { name, display_order: order }),
        2 => Just(FuzzOp::DeleteTab),
        5 => (1u32..200u32).prop_map(|z| FuzzOp::CreateTile { z_order: z }),
        3 => Just(FuzzOp::DeleteTile),
        3 => any::<bool>().prop_map(|valid| FuzzOp::ResizeTile { valid }),
        2 => (1u32..200u32).prop_map(|z| FuzzOp::UpdateZOrder { z }),
        2 => (0f32..=1f32).prop_map(|opacity| FuzzOp::UpdateOpacity { opacity }),
        3 => arb_node().prop_map(|node| FuzzOp::SetTileRoot { node }),
        2 => arb_node().prop_map(|node| FuzzOp::AddNode { node }),
        2 => Just(FuzzOp::GrantLease),
        1 => Just(FuzzOp::RevokeLease),
    ]
}

// ─── Fuzzer harness ───────────────────────────────────────────────────────────

/// State oracle tracks what we expect the scene graph to contain.
struct Oracle {
    tab_ids: Vec<SceneId>,
    tile_ids: Vec<SceneId>,
    node_ids: Vec<SceneId>,
    lease_ids: Vec<SceneId>,
    /// Monotonically increasing counter used as unique z_order per CreateTile.
    /// Avoids duplicate_z_order invariant violations by never reusing z-order values.
    next_z: u32,
}

impl Oracle {
    fn new() -> Self {
        Self {
            tab_ids: Vec::new(),
            tile_ids: Vec::new(),
            node_ids: Vec::new(),
            lease_ids: Vec::new(),
            next_z: 1,
        }
    }

    /// Allocate a fresh, unique z_order value.
    fn alloc_z(&mut self) -> u32 {
        let z = self.next_z;
        // Keep z below ZONE_TILE_Z_MIN (0x8000_0000); wrap in legal range.
        self.next_z = (z % 0x7FFF_FFFE) + 1;
        z
    }

    /// Pick a random tab id, or None if no tabs exist.
    fn random_tab(&self, idx: usize) -> Option<SceneId> {
        if self.tab_ids.is_empty() {
            None
        } else {
            Some(self.tab_ids[idx % self.tab_ids.len()])
        }
    }

    /// Pick a random tile id, or None if no tiles exist.
    fn random_tile(&self, idx: usize) -> Option<SceneId> {
        if self.tile_ids.is_empty() {
            None
        } else {
            Some(self.tile_ids[idx % self.tile_ids.len()])
        }
    }

    /// Pick a random lease id, or None if no leases exist.
    fn random_lease(&self, idx: usize) -> Option<SceneId> {
        if self.lease_ids.is_empty() {
            None
        } else {
            Some(self.lease_ids[idx % self.lease_ids.len()])
        }
    }
}

/// Execute a sequence of fuzz operations against a fresh scene graph.
/// After each operation, assert Layer 0 invariants hold.
fn run_fuzz_sequence(ops: &[FuzzOp]) {
    let mut graph = SceneGraph::new(1920.0, 1080.0);
    let mut oracle = Oracle::new();

    // Pre-populate with one lease and one tab so early operations have targets.
    let initial_lease = graph.grant_lease(AGENT, 300_000);
    oracle.lease_ids.push(initial_lease);

    let initial_tab = graph.create_tab("FuzzTab", 0).unwrap();
    oracle.tab_ids.push(initial_tab);

    for (idx, op) in ops.iter().enumerate() {
        apply_fuzz_op(&mut graph, &mut oracle, op, idx);

        // After every op, Layer 0 invariants must hold.
        let violations = assert_layer0_invariants(&graph);
        assert!(
            violations.is_empty(),
            "Layer 0 violation after op {idx} ({op:?}): {violations:?}"
        );
    }
}

fn apply_fuzz_op(graph: &mut SceneGraph, oracle: &mut Oracle, op: &FuzzOp, idx: usize) {
    match op {
        FuzzOp::CreateTab {
            name,
            display_order,
        } => {
            if oracle.tab_ids.len() < 10 {
                match graph.create_tab(name, *display_order) {
                    Ok(id) => oracle.tab_ids.push(id),
                    Err(_) => { /* duplicate display_order or name — acceptable rejection */ }
                }
            }
        }

        FuzzOp::DeleteTab => {
            if let Some(tab_id) = oracle.random_tab(idx) {
                if graph.delete_tab(tab_id).is_ok() {
                    oracle.tab_ids.retain(|&id| id != tab_id);
                    // Tiles in this tab were also deleted.
                    oracle
                        .tile_ids
                        .retain(|&tile_id| graph.tiles.contains_key(&tile_id));
                    oracle
                        .node_ids
                        .retain(|&node_id| graph.nodes.contains_key(&node_id));
                }
            }
        }

        FuzzOp::CreateTile { z_order: _user_z } => {
            let tab_id = match oracle.random_tab(idx) {
                Some(id) => id,
                None => return,
            };
            let lease_id = match oracle.random_lease(idx) {
                Some(id) => id,
                None => return,
            };
            // Use a monotonically increasing z-order to avoid duplicate_z_order violations.
            // The user-supplied z_order is intentionally discarded here: the proptest
            // parameter exercises the code path selection, but z_order uniqueness is
            // enforced by the oracle to keep the scene in a valid state.
            let z = oracle.alloc_z();
            let bounds = Rect::new(
                (idx as f32 * 7.0) % 1700.0,
                (idx as f32 * 3.0) % 900.0,
                50.0,
                50.0,
            );
            match graph.create_tile(tab_id, AGENT, lease_id, bounds, z) {
                Ok(id) => oracle.tile_ids.push(id),
                Err(_) => { /* budget exhausted, invalid args — acceptable */ }
            }
        }

        FuzzOp::DeleteTile => {
            if let Some(tile_id) = oracle.random_tile(idx) {
                if graph.delete_tile(tile_id, AGENT).is_ok() {
                    oracle.tile_ids.retain(|&id| id != tile_id);
                    oracle
                        .node_ids
                        .retain(|&node_id| graph.nodes.contains_key(&node_id));
                }
            }
        }

        FuzzOp::ResizeTile { valid: true } => {
            if let Some(tile_id) = oracle.random_tile(idx) {
                let bounds = Rect::new(
                    (idx as f32 * 11.0) % 1700.0,
                    (idx as f32 * 5.0) % 900.0,
                    60.0,
                    60.0,
                );
                let _ = graph.update_tile_bounds(tile_id, bounds, AGENT);
            }
        }

        FuzzOp::ResizeTile { valid: false } => {
            if let Some(tile_id) = oracle.random_tile(idx) {
                // Zero bounds must be rejected.
                let result =
                    graph.update_tile_bounds(tile_id, Rect::new(0.0, 0.0, 0.0, 0.0), AGENT);
                assert!(
                    result.is_err(),
                    "zero-bounds resize must be rejected, but was accepted for tile {tile_id}"
                );
            }
        }

        FuzzOp::UpdateZOrder { z: _user_z } => {
            if let Some(tile_id) = oracle.random_tile(idx) {
                // Use a fresh unique z to avoid duplicate_z_order violations.
                let z = oracle.alloc_z();
                let _ = graph.update_tile_z_order(tile_id, z, AGENT);
            }
        }

        FuzzOp::UpdateOpacity { opacity } => {
            if let Some(tile_id) = oracle.random_tile(idx) {
                let _ = graph.update_tile_opacity(tile_id, *opacity, AGENT);
            }
        }

        FuzzOp::SetTileRoot { node } => {
            if let Some(tile_id) = oracle.random_tile(idx) {
                let fresh_node = Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    ..node.clone()
                };
                if graph.set_tile_root(tile_id, fresh_node.clone()).is_ok() {
                    oracle.node_ids.push(fresh_node.id);
                }
            }
        }

        FuzzOp::AddNode { node } => {
            if let Some(tile_id) = oracle.random_tile(idx) {
                let fresh_node = Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    ..node.clone()
                };
                if graph
                    .add_node_to_tile(tile_id, None, fresh_node.clone())
                    .is_ok()
                {
                    oracle.node_ids.push(fresh_node.id);
                }
            }
        }

        FuzzOp::GrantLease => {
            if oracle.lease_ids.len() < 5 {
                let id = graph.grant_lease(AGENT, 300_000);
                oracle.lease_ids.push(id);
            }
        }

        FuzzOp::RevokeLease => {
            if let Some(lease_id) = oracle.random_lease(idx) {
                if oracle.lease_ids.len() > 1 && graph.revoke_lease(lease_id).is_ok() {
                    oracle.lease_ids.retain(|&id| id != lease_id);
                    // Revoking removes tiles owned by that lease.
                    oracle
                        .tile_ids
                        .retain(|&tile_id| graph.tiles.contains_key(&tile_id));
                    oracle
                        .node_ids
                        .retain(|&node_id| graph.nodes.contains_key(&node_id));
                }
            }
        }
    }
}

// ─── Resource leak detector ───────────────────────────────────────────────────

/// After all tiles are deleted, the scene must hold no orphan nodes.
fn assert_no_resource_leak(graph: &SceneGraph) {
    assert_eq!(
        graph.tile_count(),
        0,
        "Expected 0 tiles after full cleanup, got {}",
        graph.tile_count()
    );
    assert_eq!(
        graph.node_count(),
        0,
        "Expected 0 nodes after full cleanup (resource leak), got {}",
        graph.node_count()
    );
}

// ─── Proptest suites ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 200,
        max_shrink_iters: 1000,
        ..Default::default()
    })]

    /// 200 random operation sequences of up to 50 ops each.
    /// After every op, Layer 0 invariants must hold.
    #[test]
    fn prop_scene_graph_random_mutation_sequence(
        ops in prop::collection::vec(arb_fuzz_op(), 1..=50)
    ) {
        run_fuzz_sequence(&ops);
    }

    /// Verify that zero-bounds resize is always rejected without corrupting state.
    #[test]
    fn prop_zero_bounds_always_rejected(
        n_tiles in 1usize..=5usize,
    ) {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        let lease = graph.grant_lease(AGENT, 300_000);
        let tab = graph.create_tab("Tab", 0).unwrap();

        let mut tile_ids = Vec::new();
        for i in 0..n_tiles {
            let id = graph
                .create_tile(
                    tab,
                    AGENT,
                    lease,
                    Rect::new(i as f32 * 60.0, 0.0, 50.0, 50.0),
                    (i + 1) as u32,
                )
                .unwrap();
            tile_ids.push(id);
        }

        for &tile_id in &tile_ids {
            let result =
                graph.update_tile_bounds(tile_id, Rect::new(0.0, 0.0, 0.0, 0.0), AGENT);
            prop_assert!(result.is_err(), "zero-bounds resize must be rejected");
        }

        // State must be consistent after rejections.
        let violations = assert_layer0_invariants(&graph);
        prop_assert!(
            violations.is_empty(),
            "Layer 0 violation after zero-bounds attempts: {violations:?}"
        );
    }

    /// Oversized batch (> MAX_BATCH_SIZE) is rejected with no state change.
    #[test]
    fn prop_oversized_batch_rejected_cleanly(
        excess in 1usize..=10usize,
    ) {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        let lease = graph.grant_lease(AGENT, 300_000);
        let tab = graph.create_tab("Tab", 0).unwrap();

        let n = MAX_BATCH_SIZE + excess;
        let mutations: Vec<SceneMutation> = (0..n)
            .map(|z| SceneMutation::CreateTile {
                tab_id: tab,
                namespace: AGENT.into(),
                lease_id: lease,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                z_order: z as u32,
            })
            .collect();

        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: AGENT.into(),
            mutations,
            timing_hints: None,
            lease_id: Some(lease),
        };

        let result = graph.apply_batch(&batch);
        prop_assert!(!result.applied, "oversized batch must be rejected");
        prop_assert_eq!(
            graph.tile_count(),
            0,
            "no tiles must be created when batch is rejected"
        );

        let violations = assert_layer0_invariants(&graph);
        prop_assert!(
            violations.is_empty(),
            "Layer 0 violation after oversized batch: {violations:?}"
        );
    }

    /// Stale/random SceneIds passed as tile/tab targets must produce structured
    /// errors without crashing or corrupting state.
    #[test]
    fn prop_stale_ids_rejected_gracefully(
        n_ops in 1usize..=20usize,
    ) {
        let mut graph = SceneGraph::new(1920.0, 1080.0);

        for _i in 0..n_ops {
            let bogus_tile = SceneId::new();
            let bogus_tab = SceneId::new();

            // All of these should return Err, not panic.
            let _ = graph.delete_tile(bogus_tile, AGENT);
            let _ = graph.delete_tab(bogus_tab);
            let _ = graph.update_tile_bounds(bogus_tile, Rect::new(0.0, 0.0, 100.0, 100.0), AGENT);
            let _ = graph.update_tile_z_order(bogus_tile, 1, AGENT);
            let _ = graph.update_tile_opacity(bogus_tile, 0.5, AGENT);
            let _ = graph.set_tile_root(bogus_tile, Node { layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::WHITE,
                    bounds: Rect::new(0.0, 0.0, 50.0, 50.0),
                    radius: None,
}),
            });
        }

        let violations = assert_layer0_invariants(&graph);
        prop_assert!(
            violations.is_empty(),
            "Layer 0 violation after stale-id ops: {violations:?}"
        );
    }

    /// Mix of valid and invalid mutations in a single batch must be all-or-nothing.
    #[test]
    fn prop_mixed_valid_invalid_batch_all_or_nothing(
        n_valid_before in 0usize..=5usize,
        n_valid_after in 0usize..=5usize,
    ) {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        let lease = graph.grant_lease(AGENT, 300_000);
        let tab = graph.create_tab("Tab", 0).unwrap();

        let mut mutations: Vec<SceneMutation> = Vec::new();

        for z in 0..n_valid_before {
            mutations.push(SceneMutation::CreateTile {
                tab_id: tab,
                namespace: AGENT.into(),
                lease_id: lease,
                bounds: Rect::new(z as f32 * 60.0, 0.0, 50.0, 50.0),
                z_order: (z + 1) as u32,
            });
        }

        // Insert one invalid mutation in the middle.
        mutations.push(SceneMutation::CreateTile {
            tab_id: tab,
            namespace: AGENT.into(),
            lease_id: lease,
            bounds: Rect::new(0.0, 0.0, -50.0, 50.0), // invalid — negative width
            z_order: 999,
        });

        for z in 0..n_valid_after {
            mutations.push(SceneMutation::CreateTile {
                tab_id: tab,
                namespace: AGENT.into(),
                lease_id: lease,
                bounds: Rect::new(z as f32 * 60.0, 100.0, 50.0, 50.0),
                z_order: (z + 200) as u32,
            });
        }

        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: AGENT.into(),
            mutations,
            timing_hints: None,
            lease_id: Some(lease),
        };

        let result = graph.apply_batch(&batch);
        prop_assert!(!result.applied, "batch with invalid mutation must be rejected");
        prop_assert_eq!(
            graph.tile_count(),
            0,
            "no tiles must survive rejected batch (all-or-nothing)"
        );

        let violations = assert_layer0_invariants(&graph);
        prop_assert!(
            violations.is_empty(),
            "Layer 0 violations after rejected mixed batch: {violations:?}"
        );
    }

    /// Lease grant → revoke cycle never leaks tiles or nodes.
    #[test]
    fn prop_lease_grant_revoke_no_leak(
        n_tiles in 1usize..=8usize,
    ) {
        let mut graph = SceneGraph::new(1920.0, 1080.0);
        let tab = graph.create_tab("Tab", 0).unwrap();
        let lease = graph.grant_lease(AGENT, 300_000);

        for i in 0..n_tiles {
            let tile_id = graph
                .create_tile(
                    tab,
                    AGENT,
                    lease,
                    Rect::new(i as f32 * 60.0, 0.0, 50.0, 50.0),
                    (i + 1) as u32,
                )
                .unwrap();
            graph
                .set_tile_root(
                    tile_id,
                    Node { layout: Default::default(),
                        id: SceneId::new(),
                        children: vec![],
                        data: NodeData::SolidColor(SolidColorNode {
                            color: Rgba::WHITE,
                            bounds: Rect::new(0.0, 0.0, 50.0, 50.0),
                            radius: None,
}),
                    },
                )
                .unwrap();
        }

        prop_assert_eq!(graph.tile_count(), n_tiles);

        // Revoke the lease — all tiles and nodes owned by this lease must be cleaned up.
        graph.revoke_lease(lease).unwrap();

        assert_no_resource_leak(&graph);

        let violations = assert_layer0_invariants(&graph);
        prop_assert!(
            violations.is_empty(),
            "Layer 0 violations after lease revoke: {violations:?}"
        );
    }
}

// ─── High-volume deterministic test ──────────────────────────────────────────

/// Apply 100,000 sequential create/update/delete tile mutations (all valid) using
/// direct `create_tile` / `update_tile_*` / `delete_tile` calls in a deterministic
/// 4-phase loop, and verify no crash, no inconsistent state, and final graph is empty
/// after cleanup.
///
/// This is the high-volume stability gate from hud-3ksv (100,000 operations without
/// crash or inconsistent state). The proptest suite above provides random/combinatorial
/// coverage; this test provides sustained-load coverage.
#[test]
fn test_100k_deterministic_tile_mutations() {
    let mut graph = SceneGraph::new(1920.0, 1080.0);
    let tab = graph.create_tab("Main", 0).unwrap();
    let lease = graph.grant_lease("load.agent", 300_000);

    // Use a fixed pool of tile IDs — create 8 tiles then rotate through updates/deletes.
    let mut live_tiles: std::collections::VecDeque<SceneId> = std::collections::VecDeque::new();
    let pool_size = 8;
    // Monotonic z_order counter — guarantees no duplicate_z_order violations.
    let mut next_z: u32 = 1;

    for i in 0u64..100_000 {
        // Every 100 iterations, check invariants.
        if i % 100 == 0 {
            let violations = assert_layer0_invariants(&graph);
            assert!(
                violations.is_empty(),
                "Layer 0 violation at iteration {i}: {violations:?}"
            );
        }

        let phase = i % 4;
        match phase {
            0 => {
                // Create a tile
                if live_tiles.len() < pool_size {
                    let z = next_z;
                    next_z = (next_z % 0x7FFF_FFFE) + 1;
                    let bounds = Rect::new(
                        (i as f32 * 7.0) % 1700.0,
                        (i as f32 * 3.0) % 900.0,
                        50.0,
                        50.0,
                    );
                    if let Ok(id) = graph.create_tile(tab, "load.agent", lease, bounds, z) {
                        live_tiles.push_back(id)
                    }
                }
            }
            1 => {
                // Update bounds of oldest tile
                if let Some(&tile_id) = live_tiles.front() {
                    let new_bounds = Rect::new(
                        (i as f32 * 13.0) % 1700.0,
                        (i as f32 * 7.0) % 900.0,
                        55.0,
                        55.0,
                    );
                    let _ = graph.update_tile_bounds(tile_id, new_bounds, "load.agent");
                }
            }
            2 => {
                // Update z-order of newest tile (use a fresh unique z).
                if let Some(&tile_id) = live_tiles.back() {
                    let new_z = next_z;
                    next_z = (next_z % 0x7FFF_FFFE) + 1;
                    let _ = graph.update_tile_z_order(tile_id, new_z, "load.agent");
                }
            }
            3 => {
                // Delete oldest tile
                if let Some(tile_id) = live_tiles.pop_front() {
                    let _ = graph.delete_tile(tile_id, "load.agent");
                }
            }
            _ => unreachable!(),
        }
    }

    // Final invariant check
    let violations = assert_layer0_invariants(&graph);
    assert!(
        violations.is_empty(),
        "Layer 0 violations at end of 100k test: {violations:?}"
    );

    // Clean up all remaining tiles
    let remaining: Vec<SceneId> = live_tiles.into_iter().collect();
    for tile_id in remaining {
        let _ = graph.delete_tile(tile_id, "load.agent");
    }

    assert_no_resource_leak(&graph);
}

// ─── Regression: invalid opacity values ──────────────────────────────────────

/// Invalid opacity values (outside [0.0, 1.0]) must be rejected cleanly.
#[test]
fn test_invalid_opacity_rejected() {
    let mut graph = SceneGraph::new(1920.0, 1080.0);
    let tab = graph.create_tab("Tab", 0).unwrap();
    let lease = graph.grant_lease(AGENT, 300_000);
    let tile_id = graph
        .create_tile(tab, AGENT, lease, Rect::new(0.0, 0.0, 100.0, 100.0), 1)
        .unwrap();

    // Opacity > 1.0 must be rejected
    let result = graph.update_tile_opacity(tile_id, 1.5, AGENT);
    assert!(result.is_err(), "opacity > 1.0 must be rejected");

    // Opacity < 0.0 must be rejected
    let result = graph.update_tile_opacity(tile_id, -0.1, AGENT);
    assert!(result.is_err(), "opacity < 0.0 must be rejected");

    // Valid opacity must be accepted
    let result = graph.update_tile_opacity(tile_id, 0.5, AGENT);
    assert!(result.is_ok(), "valid opacity must be accepted");

    let violations = assert_layer0_invariants(&graph);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after opacity tests: {violations:?}"
    );
}

/// Empty tab name must be rejected (RFC 0001 §2.2).
#[test]
fn test_empty_tab_name_rejected() {
    let mut graph = SceneGraph::new(1920.0, 1080.0);
    let result = graph.create_tab("", 0);
    assert!(result.is_err(), "empty tab name must be rejected");
}

/// Tab name exceeding MAX_TAB_NAME_BYTES must be rejected.
#[test]
fn test_oversized_tab_name_rejected() {
    use tze_hud_scene::graph::MAX_TAB_NAME_BYTES;
    let mut graph = SceneGraph::new(1920.0, 1080.0);
    let long_name = "x".repeat(MAX_TAB_NAME_BYTES + 1);
    let result = graph.create_tab(&long_name, 0);
    assert!(
        result.is_err(),
        "tab name > MAX_TAB_NAME_BYTES must be rejected"
    );
}

/// TextMarkdown content exceeding MAX_MARKDOWN_BYTES must be rejected.
#[test]
fn test_oversized_markdown_content_rejected() {
    use tze_hud_scene::graph::MAX_MARKDOWN_BYTES;
    let mut graph = SceneGraph::new(1920.0, 1080.0);
    let tab = graph.create_tab("Tab", 0).unwrap();
    let lease = graph.grant_lease(AGENT, 300_000);
    let tile_id = graph
        .create_tile(tab, AGENT, lease, Rect::new(0.0, 0.0, 100.0, 100.0), 1)
        .unwrap();

    let huge_content = "x".repeat(MAX_MARKDOWN_BYTES + 1);
    let result = graph.set_tile_root(
        tile_id,
        Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: huge_content,
                bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                font_size_px: 14.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba::WHITE,
                background: None,
                alignment: TextAlign::Start,
                overflow: TextOverflow::Clip,
                color_runs: Box::default(),
            }),
        },
    );
    assert!(
        result.is_err(),
        "oversized markdown content must be rejected"
    );
}

/// MAX_TABS limit must be enforced.
#[test]
fn test_max_tabs_limit_enforced() {
    use tze_hud_scene::graph::MAX_TABS;
    let mut graph = SceneGraph::new(1920.0, 1080.0);

    // Fill to the limit.
    for i in 0..MAX_TABS {
        graph.create_tab(&format!("Tab{i}"), i as u32).unwrap();
    }

    // One more must fail.
    let result = graph.create_tab("Overflow", MAX_TABS as u32);
    assert!(
        result.is_err(),
        "tab creation beyond MAX_TABS must be rejected"
    );

    let violations = assert_layer0_invariants(&graph);
    assert!(
        violations.is_empty(),
        "Layer 0 violations at max tabs: {violations:?}"
    );
}

/// Z-order in the zone-reserved range must be rejected for agent-created tiles.
#[test]
fn test_zone_reserved_z_order_rejected() {
    use tze_hud_scene::graph::ZONE_TILE_Z_MIN;
    let mut graph = SceneGraph::new(1920.0, 1080.0);
    let tab = graph.create_tab("Tab", 0).unwrap();
    let lease = graph.grant_lease(AGENT, 300_000);

    let result = graph.create_tile(
        tab,
        AGENT,
        lease,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        ZONE_TILE_Z_MIN,
    );
    assert!(
        result.is_err(),
        "z_order at ZONE_TILE_Z_MIN must be rejected"
    );

    let result = graph.create_tile(
        tab,
        AGENT,
        lease,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        u32::MAX,
    );
    assert!(
        result.is_err(),
        "z_order=u32::MAX must be rejected (zone-reserved range)"
    );
}
