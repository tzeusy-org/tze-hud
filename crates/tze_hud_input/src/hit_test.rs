//! Hit-test pipeline: point queries against the scene graph.
//!
//! Implements spec §Requirement: Hit-Test Performance (lines 263-265):
//! - < 100µs p99 for 50 tiles
//! - Traversal order: chrome layer first, then content tiles by z-order
//!   descending, within each tile nodes in reverse tree order (last child first)
//! - First HitRegionNode whose bounds contain the point wins
//!
//! This module operates entirely on pure Rust data structures — no GPU, no
//! display server required (spec §Requirement: Headless Testability, line 422).

use tze_hud_scene::{InputMode, NodeData, SceneGraph, SceneId};

use crate::events::HitTestResult;

/// Perform a point query against the scene graph.
///
/// Returns the first hit in priority order:
/// 1. Chrome layer (tiles with `namespace` starting with "chrome.")
/// 2. Content tiles in descending z-order
/// 3. Within each tile, nodes in reverse tree order (deepest last child wins)
/// 4. First HitRegionNode whose tile-local bounds contain the point
///
/// If a tile is hit but no HitRegionNode within it matches, returns `TileHit`.
/// If no tile matches, returns `Passthrough`.
///
/// # Headless testability
///
/// This function takes an immutable reference to `SceneGraph` and operates
/// purely on Rust data structures. It requires no GPU context, display server,
/// or winit instance — all Layer 0 tests can inject synthetic events directly.
pub fn hit_test(scene: &SceneGraph, display_x: f32, display_y: f32) -> HitTestResult {
    // Collect tiles sorted for traversal:
    // chrome first (by z desc), then content (by z desc).
    let mut chrome_tiles: Vec<(u32, SceneId)> = Vec::new();
    let mut content_tiles: Vec<(u32, SceneId)> = Vec::new();

    for (&tile_id, tile) in &scene.tiles {
        if tile.bounds.contains_point(display_x, display_y) {
            if tile.namespace.starts_with("chrome.") {
                chrome_tiles.push((tile.z_order, tile_id));
            } else {
                content_tiles.push((tile.z_order, tile_id));
            }
        }
    }

    // Sort chrome and content tiles by z-order descending (highest z wins)
    chrome_tiles.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    content_tiles.sort_unstable_by(|a, b| b.0.cmp(&a.0));

    // Chrome tiles are checked before content tiles (spec line 264)
    let ordered: Vec<(u32, SceneId)> = chrome_tiles
        .iter()
        .chain(content_tiles.iter())
        .copied()
        .collect();

    for (_, tile_id) in &ordered {
        let tile = match scene.tiles.get(tile_id) {
            Some(t) => t,
            None => continue,
        };

        // Passthrough tiles pass events to whatever is below them.
        // For content tiles only — chrome tiles always capture.
        if !tile.namespace.starts_with("chrome.") && tile.input_mode == InputMode::Passthrough {
            continue;
        }

        // Convert to tile-local coordinates
        let local_x = display_x - tile.bounds.x;
        let local_y = display_y - tile.bounds.y;

        // Try to find a HitRegionNode within this tile.
        // Walk nodes in reverse tree order starting from the root.
        if let Some(root_id) = tile.root_node {
            if let Some(node_hit) =
                hit_test_node_reverse(scene, root_id, local_x, local_y, *tile_id)
            {
                return node_hit;
            }
        }

        // The tile itself was hit but no HitRegionNode matched.
        return HitTestResult::TileHit { tile_id: *tile_id };
    }

    HitTestResult::Passthrough
}

/// Recursively test a node and its children in reverse tree order.
///
/// "Reverse tree order" means last child is tested first (deepest in paint
/// order wins). This matches the spec's "reverse tree order (last child first)"
/// requirement (line 264).
///
/// Returns `Some(HitTestResult)` when a hit is found, `None` otherwise.
fn hit_test_node_reverse(
    scene: &SceneGraph,
    node_id: SceneId,
    local_x: f32,
    local_y: f32,
    tile_id: SceneId,
) -> Option<HitTestResult> {
    let node = scene.nodes.get(&node_id)?;

    // Test children in reverse order first (last child wins per spec)
    for &child_id in node.children.iter().rev() {
        if let Some(hit) = hit_test_node_reverse(scene, child_id, local_x, local_y, tile_id) {
            return Some(hit);
        }
    }

    // Then test this node itself
    if let NodeData::HitRegion(hr) = &node.data {
        if hr.accepts_pointer && hr.bounds.contains_point(local_x, local_y) {
            return Some(HitTestResult::NodeHit { tile_id, node_id });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::{
        HitRegionNode, InputMode, Node, NodeData, Rect, Rgba, SceneGraph, SceneId, SolidColorNode,
    };

    fn make_scene_with_hit_regions(n_tiles: usize) -> (SceneGraph, Vec<SceneId>, Vec<SceneId>) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);

        let cols = 5_usize;
        let tile_w = 200.0_f32;
        let tile_h = 100.0_f32;
        let mut tile_ids = Vec::new();
        let mut node_ids = Vec::new();

        for i in 0..n_tiles {
            let col = (i % cols) as f32;
            let row = (i / cols) as f32;
            let tile_id = scene
                .create_tile(
                    tab_id,
                    "agent",
                    lease_id,
                    Rect::new(col * tile_w, row * tile_h, tile_w - 2.0, tile_h - 2.0),
                    (i + 1) as u32,
                )
                .unwrap();
            let node_id = SceneId::new();
            scene
                .set_tile_root(
                    tile_id,
                    Node {
                        layout: Default::default(),
                        id: node_id,
                        children: vec![],
                        data: NodeData::HitRegion(HitRegionNode {
                            bounds: Rect::new(0.0, 0.0, tile_w - 2.0, tile_h - 2.0),
                            interaction_id: format!("node-{i}"),
                            accepts_focus: false,
                            accepts_pointer: true,
                            ..Default::default()
                        }),
                    },
                )
                .unwrap();
            tile_ids.push(tile_id);
            node_ids.push(node_id);
        }

        (scene, tile_ids, node_ids)
    }

    // ── Headless testability: Layer 0 tests ───────────────────────────────────

    #[test]
    fn hit_test_returns_correct_node_without_gpu() {
        // Spec §Requirement: Headless Testability, line 428:
        // Layer 0 test injects synthetic PointerDownEvent at (50,50) — hit-test
        // returns the correct result without GPU or display server.
        let (scene, tile_ids, node_ids) = make_scene_with_hit_regions(1);

        let result = hit_test(&scene, 50.0, 50.0);
        assert_eq!(
            result,
            HitTestResult::NodeHit {
                tile_id: tile_ids[0],
                node_id: node_ids[0]
            },
            "first tile (0,0) must be hit at (50,50)"
        );
    }

    #[test]
    fn hit_test_misses_outside_all_tiles() {
        let (scene, _, _) = make_scene_with_hit_regions(3);
        let result = hit_test(&scene, 1900.0, 900.0);
        assert_eq!(
            result,
            HitTestResult::Passthrough,
            "point outside all tiles must passthrough"
        );
    }

    #[test]
    fn hit_test_returns_tile_hit_when_no_node_matches() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 200.0, 100.0),
                1,
            )
            .unwrap();
        // Set a non-HitRegion root
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::WHITE,
                        bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                        radius: None,
                    }),
                },
            )
            .unwrap();

        let result = hit_test(&scene, 50.0, 50.0);
        assert_eq!(
            result,
            HitTestResult::TileHit { tile_id },
            "solid color tile returns TileHit"
        );
    }

    // ── Chrome-first hit ordering ─────────────────────────────────────────────

    #[test]
    fn chrome_wins_over_content_tile() {
        // Spec §Requirement: Hit-Test Performance, line 272-274:
        // Chrome layer always wins when overlapping content.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let content_lease = scene.grant_lease("agent", 60_000);
        let chrome_lease = scene.grant_lease("chrome.ui", 60_000);

        // Content tile at z=1
        let content_tile = scene
            .create_tile(
                tab_id,
                "agent",
                content_lease,
                Rect::new(0.0, 0.0, 200.0, 100.0),
                1,
            )
            .unwrap();
        let content_node_id = SceneId::new();
        scene
            .set_tile_root(
                content_tile,
                Node {
                    layout: Default::default(),
                    id: content_node_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                        interaction_id: "content-btn".to_string(),
                        accepts_pointer: true,
                        accepts_focus: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        // Chrome tile at z=2 (lower z but chrome prefix wins regardless)
        let chrome_tile = scene
            .create_tile(
                tab_id,
                "chrome.ui",
                chrome_lease,
                Rect::new(0.0, 0.0, 200.0, 100.0),
                2,
            )
            .unwrap();
        let chrome_node_id = SceneId::new();
        scene
            .set_tile_root(
                chrome_tile,
                Node {
                    layout: Default::default(),
                    id: chrome_node_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                        interaction_id: "chrome-btn".to_string(),
                        accepts_pointer: true,
                        accepts_focus: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        let result = hit_test(&scene, 50.0, 50.0);
        assert_eq!(
            result,
            HitTestResult::NodeHit {
                tile_id: chrome_tile,
                node_id: chrome_node_id
            },
            "chrome tile must win even when content tile has equal or higher z"
        );
    }

    // ── Passthrough tile skipping ─────────────────────────────────────────────

    #[test]
    fn passthrough_tile_does_not_block_content() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let content_lease = scene.grant_lease("agent", 60_000);
        let overlay_lease = scene.grant_lease("overlay", 60_000);

        // Content tile (z=1)
        let content_tile = scene
            .create_tile(
                tab_id,
                "agent",
                content_lease,
                Rect::new(0.0, 0.0, 200.0, 100.0),
                1,
            )
            .unwrap();
        let content_node_id = SceneId::new();
        scene
            .set_tile_root(
                content_tile,
                Node {
                    layout: Default::default(),
                    id: content_node_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                        interaction_id: "content-btn".to_string(),
                        accepts_pointer: true,
                        accepts_focus: false,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        // Passthrough overlay (z=10, higher z but passthrough)
        let overlay_tile = scene
            .create_tile(
                tab_id,
                "overlay",
                overlay_lease,
                Rect::new(0.0, 0.0, 200.0, 100.0),
                10,
            )
            .unwrap();
        // Set input_mode to Passthrough on the overlay tile
        scene.tiles.get_mut(&overlay_tile).unwrap().input_mode = InputMode::Passthrough;
        scene
            .set_tile_root(
                overlay_tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::SolidColor(SolidColorNode {
                        color: Rgba::TRANSPARENT,
                        bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                        radius: None,
                    }),
                },
            )
            .unwrap();

        let result = hit_test(&scene, 50.0, 50.0);
        assert_eq!(
            result,
            HitTestResult::NodeHit {
                tile_id: content_tile,
                node_id: content_node_id
            },
            "passthrough overlay must not block content tile"
        );
    }

    // ── Hit-test performance budget ───────────────────────────────────────────

    #[test]
    fn hit_test_50_tiles_under_100us_p99() {
        // Spec §Requirement: Hit-Test Performance, line 268-270:
        // Hit-test for 50 tiles must complete in < 100µs.
        use std::time::Instant;
        use tze_hud_scene::perf_budget::{budgets, test_budget};

        let (scene, _, _) = make_scene_with_hit_regions(50);
        let budget_us = test_budget(budgets::HIT_TEST_BUDGET_US);

        // Warm up
        for _ in 0..10 {
            let _ = hit_test(&scene, 50.0, 50.0);
        }

        // Sample 100 iterations and check p99
        let mut durations: Vec<u64> = (0..100)
            .map(|_| {
                let start = Instant::now();
                let _ = hit_test(&scene, 50.0, 50.0);
                start.elapsed().as_micros() as u64
            })
            .collect();
        durations.sort_unstable();
        let p99 = durations[98]; // 99th percentile of 100 samples

        assert!(
            p99 < budget_us,
            "hit-test p99 was {}µs, calibrated budget is {}µs (base: {}µs)",
            p99,
            budget_us,
            budgets::HIT_TEST_BUDGET_US,
        );
    }

    // ── Reverse tree order ────────────────────────────────────────────────────

    #[test]
    fn last_child_wins_over_first_child() {
        // Spec line 264: within a tile, nodes tested in reverse tree order
        // (last child first).
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 200.0, 200.0),
                1,
            )
            .unwrap();

        // Create parent node with two overlapping HitRegion children
        let child1_id = SceneId::new();
        let child2_id = SceneId::new();
        let parent_id = SceneId::new();

        // Insert child nodes directly
        scene.nodes.insert(
            child1_id,
            Node {
                layout: Default::default(),
                id: child1_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                    interaction_id: "first-child".to_string(),
                    accepts_pointer: true,
                    accepts_focus: false,
                    ..Default::default()
                }),
            },
        );
        scene.nodes.insert(
            child2_id,
            Node {
                layout: Default::default(),
                id: child2_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                    interaction_id: "last-child".to_string(),
                    accepts_pointer: true,
                    accepts_focus: false,
                    ..Default::default()
                }),
            },
        );

        // Parent has child1 first, child2 last
        scene.nodes.insert(
            parent_id,
            Node {
                layout: Default::default(),
                id: parent_id,
                children: vec![child1_id, child2_id],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::TRANSPARENT,
                    bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                    radius: None,
                }),
            },
        );
        scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(parent_id);
        // Register local state for the hit region children
        scene.hit_region_states.insert(
            child1_id,
            tze_hud_scene::HitRegionLocalState::new(child1_id),
        );
        scene.hit_region_states.insert(
            child2_id,
            tze_hud_scene::HitRegionLocalState::new(child2_id),
        );

        let result = hit_test(&scene, 50.0, 50.0);
        assert_eq!(
            result,
            HitTestResult::NodeHit {
                tile_id,
                node_id: child2_id
            },
            "last child must win when children overlap"
        );
    }
}
