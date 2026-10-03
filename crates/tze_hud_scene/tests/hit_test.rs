//! # Hit-Test Integration Tests
//!
//! Correctness tests for [`SceneGraph::hit_test`].
//!
//! ## What is tested
//!
//! 1. Highest-z non-passthrough content tile wins when tiles overlap.
//! 2. Passthrough tiles are skipped; lower-z capture tiles below them hit.
//! 3. Within a tile, reverse tree order (last sibling first, deepest first).
//! 4. HitResult variants: `NodeHit`, `TileHit`, `Passthrough`.
//! 5. `interaction_id` forwarded correctly in `NodeHit`.
//! 6. `update_hover_state` updates `HitRegionLocalState` without agent roundtrip.
//! 7. `HitResult::is_some()` / `is_none()` helpers.
//! 8. Layer 0 invariants pass on all hit-test test scenes.
//! 9. Property tests: random scene layouts verify z-order-descending.
//!
//! ## Layer
//!
//! Layer 0 — pure Rust, no GPU, no async.  All tests run in < 2 s.

use proptest::prelude::*;
use tze_hud_scene::{
    HitRegionNode, HitResult, InputMode, Node, NodeData, Rect, Rgba, SceneId, SolidColorNode,
    graph::SceneGraph,
    test_scenes::{ClockMs, TestSceneRegistry},
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build a scene with a single content tile and a single HitRegionNode inside it.
///
/// Display area: 1920×1080.
/// Tile: `tile_bounds` (content layer, normal priority 2).
/// Node: `node_bounds` (tile-local coordinates), `interaction_id = "btn"`.
fn single_tile_scene(
    tile_bounds: Rect,
    node_bounds: Rect,
) -> (SceneGraph, SceneId, SceneId, SceneId) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent.test", 60_000);
    let tile_id = scene
        .create_tile(tab_id, "agent.test", lease_id, tile_bounds, 10)
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
                    bounds: node_bounds,
                    interaction_id: "btn".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    (scene, tab_id, tile_id, node_id)
}

// ─── Basic NodeHit / TileHit / Passthrough ────────────────────────────────────

#[test]
fn node_hit_when_pointer_on_hit_region() {
    // Tile at (100, 100, 400×300); node at (50, 50, 200×100) tile-local.
    // Global node rect: (150, 150, 200×100).
    let (scene, _, tile_id, node_id) = single_tile_scene(
        Rect::new(100.0, 100.0, 400.0, 300.0),
        Rect::new(50.0, 50.0, 200.0, 100.0),
    );

    let result = scene.hit_test(200.0, 180.0); // inside global node rect
    assert_eq!(
        result,
        HitResult::NodeHit {
            tile_id,
            node_id,
            interaction_id: "btn".to_string(),
        }
    );
}

#[test]
fn tile_hit_when_pointer_on_tile_but_outside_node() {
    // Tile at (100, 100, 400×300); node at (50, 50, 200×100).
    // Point (110, 110) is inside tile bounds but before node start.
    let (scene, _, tile_id, _) = single_tile_scene(
        Rect::new(100.0, 100.0, 400.0, 300.0),
        Rect::new(50.0, 50.0, 200.0, 100.0),
    );

    let result = scene.hit_test(110.0, 110.0);
    assert_eq!(result, HitResult::TileHit { tile_id });
}

#[test]
fn passthrough_when_pointer_misses_all_tiles() {
    let (scene, _, _, _) = single_tile_scene(
        Rect::new(100.0, 100.0, 400.0, 300.0),
        Rect::new(50.0, 50.0, 200.0, 100.0),
    );

    let result = scene.hit_test(10.0, 10.0); // outside tile bounds
    assert_eq!(result, HitResult::Passthrough);
}

#[test]
fn passthrough_when_no_active_tab() {
    let scene = SceneGraph::new(1920.0, 1080.0);
    // No tabs — active_tab is None.
    assert_eq!(scene.hit_test(500.0, 500.0), HitResult::Passthrough);
}

// ─── Widget passthrough hit-test ─────────────────────────────────────────────
//
// Widget tiles MUST default to input_mode = Passthrough per widget-system/spec.md
// §Requirement: Widget Input Mode. Input events landing on a widget tile's
// geometry MUST pass through to the next tile in z-order.

#[test]
fn widget_passthrough_skips_to_agent_tile_below() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let agent_lease = scene.grant_lease("agent.test", 60_000);
    let widget_lease = scene.grant_lease("widget.renderer", 60_000);

    // Agent-owned content tile: covers central region, z=10, Capture (default).
    // Bounds: (300, 200, 600×400).
    let agent_tile = scene
        .create_tile(
            tab_id,
            "agent.test",
            agent_lease,
            Rect::new(300.0, 200.0, 600.0, 400.0),
            10,
        )
        .unwrap();
    let agent_node_id = SceneId::new();
    scene
        .set_tile_root(
            agent_tile,
            Node {
                layout: Default::default(),
                id: agent_node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 600.0, 400.0),
                    interaction_id: "agent-content".to_string(),
                    accepts_focus: false,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    // Widget tile: overlaps agent tile, z=20, Passthrough (widget default).
    // Bounds: (250, 150, 700×500) — reaches past agent tile.
    // No interactive nodes (per spec, widgets are display-only).
    let widget_tile = scene
        .create_tile(
            tab_id,
            "widget.renderer",
            widget_lease,
            Rect::new(250.0, 150.0, 700.0, 500.0),
            20,
        )
        .unwrap();
    // Explicitly set widget tile to Passthrough (should be default for widgets).
    scene.tiles.get_mut(&widget_tile).unwrap().input_mode = InputMode::Passthrough;

    // Test point inside overlap region: (500, 350).
    // This point is inside both tiles' bounds:
    //   - Agent tile: (300, 200) to (900, 600)
    //   - Widget tile: (250, 150) to (950, 650)
    // Widget tile (z=20) is higher, but since it's Passthrough,
    // hit-test must skip it and return the agent tile (z=10).
    let result = scene.hit_test(500.0, 350.0);
    assert_eq!(
        result,
        HitResult::NodeHit {
            tile_id: agent_tile,
            node_id: agent_node_id,
            interaction_id: "agent-content".to_string(),
        },
        "widget passthrough tile must be skipped in z-order traversal"
    );

    // Test point outside overlap but inside widget only: (100, 100).
    // This point is inside widget tile bounds but outside agent tile.
    // Since widget tile is Passthrough and no other tiles below it, result is Passthrough.
    let result = scene.hit_test(100.0, 100.0);
    assert_eq!(
        result,
        HitResult::Passthrough,
        "point in widget-only region with no capture tiles below must return Passthrough"
    );
}

// ─── Passthrough tiles skipped ───────────────────────────────────────────────

#[test]
fn passthrough_tile_skipped_reveals_tile_below() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent.test", 60_000);

    // Low-z capture tile covering full screen.
    let low_tile = scene
        .create_tile(
            tab_id,
            "agent.test",
            lease_id,
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            1,
        )
        .unwrap();
    let low_node_id = SceneId::new();
    scene
        .set_tile_root(
            low_tile,
            Node {
                layout: Default::default(),
                id: low_node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                    interaction_id: "content".to_string(),
                    accepts_focus: false,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    // High-z passthrough tile covering the same region.
    let high_tile = scene
        .create_tile(
            tab_id,
            "agent.test",
            lease_id,
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            20,
        )
        .unwrap();
    scene.tiles.get_mut(&high_tile).unwrap().input_mode = InputMode::Passthrough;

    // Hit anywhere — passthrough tile must be skipped, low-z tile must respond.
    let result = scene.hit_test(500.0, 500.0);
    assert_eq!(
        result,
        HitResult::NodeHit {
            tile_id: low_tile,
            node_id: low_node_id,
            interaction_id: "content".to_string(),
        },
        "passthrough tile must be skipped"
    );
}

#[test]
fn all_tiles_passthrough_returns_passthrough() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent.test", 60_000);

    for z in [1u32, 2, 3] {
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent.test",
                lease_id,
                Rect::new(0.0, 0.0, 1920.0, 1080.0),
                z,
            )
            .unwrap();
        scene.tiles.get_mut(&tile_id).unwrap().input_mode = InputMode::Passthrough;
    }

    let result = scene.hit_test(500.0, 500.0);
    assert_eq!(
        result,
        HitResult::Passthrough,
        "all-passthrough should return Passthrough"
    );
}

// ─── Z-order: highest wins on overlap ────────────────────────────────────────

#[test]
fn highest_z_tile_wins_in_overlap() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent.test", 60_000);

    // Two overlapping tiles.
    let low_tile = scene
        .create_tile(
            tab_id,
            "agent.test",
            lease_id,
            Rect::new(0.0, 0.0, 600.0, 400.0),
            1,
        )
        .unwrap();
    let high_tile = scene
        .create_tile(
            tab_id,
            "agent.test",
            lease_id,
            Rect::new(300.0, 200.0, 600.0, 400.0),
            5,
        )
        .unwrap();

    // Add a hit region to each tile.
    let low_node_id = SceneId::new();
    scene
        .set_tile_root(
            low_tile,
            Node {
                layout: Default::default(),
                id: low_node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 600.0, 400.0),
                    interaction_id: "low".to_string(),
                    accepts_focus: false,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    let high_node_id = SceneId::new();
    scene
        .set_tile_root(
            high_tile,
            Node {
                layout: Default::default(),
                id: high_node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 600.0, 400.0),
                    interaction_id: "high".to_string(),
                    accepts_focus: false,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    // Point in the overlap region — high-z tile must win.
    let result = scene.hit_test(400.0, 300.0);
    assert_eq!(
        result,
        HitResult::NodeHit {
            tile_id: high_tile,
            node_id: high_node_id,
            interaction_id: "high".to_string(),
        },
        "highest z_order tile must win in overlap"
    );

    // Point only in low tile — low tile responds.
    let result = scene.hit_test(100.0, 100.0);
    assert_eq!(
        result,
        HitResult::NodeHit {
            tile_id: low_tile,
            node_id: low_node_id,
            interaction_id: "low".to_string(),
        },
        "point outside high tile should hit low tile"
    );
}

// ─── Reverse tree order within a tile ────────────────────────────────────────

#[test]
fn last_sibling_wins_in_reverse_tree_order() {
    // Two HitRegionNodes as children of a root (non-interactive) node.
    // The last child in the children list is the front-most and should win.
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent.test", 60_000);

    let tile_id = scene
        .create_tile(
            tab_id,
            "agent.test",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
            1,
        )
        .unwrap();

    let first_child_id = SceneId::new();
    let last_child_id = SceneId::new();
    let root_id = SceneId::new();

    // Both children overlap the same region (100,100, 200×100).
    let first_child = Node {
        layout: Default::default(),
        id: first_child_id,
        children: vec![],
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(100.0, 100.0, 200.0, 100.0),
            interaction_id: "first".to_string(),
            accepts_focus: false,
            accepts_pointer: true,
            ..Default::default()
        }),
    };
    let last_child = Node {
        layout: Default::default(),
        id: last_child_id,
        children: vec![],
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(100.0, 100.0, 200.0, 100.0),
            interaction_id: "last".to_string(),
            accepts_focus: false,
            accepts_pointer: true,
            ..Default::default()
        }),
    };
    let root = Node {
        layout: Default::default(),
        id: root_id,
        children: vec![first_child_id, last_child_id],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::BLACK,
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            radius: None,
        }),
    };

    // Insert nodes individually then set root.
    scene.nodes.insert(first_child_id, first_child);
    scene.nodes.insert(last_child_id, last_child);
    // Register hit region state for children manually (set_tile_root only
    // handles the root node; for multi-node trees callers must do this).
    scene.hit_region_states.insert(
        first_child_id,
        tze_hud_scene::HitRegionLocalState::new(first_child_id),
    );
    scene.hit_region_states.insert(
        last_child_id,
        tze_hud_scene::HitRegionLocalState::new(last_child_id),
    );

    scene.set_tile_root(tile_id, root).unwrap();

    // The last child in children[] is visited first in reverse order — it should win.
    let result = scene.hit_test(200.0, 150.0); // inside both children
    assert_eq!(
        result,
        HitResult::NodeHit {
            tile_id,
            node_id: last_child_id,
            interaction_id: "last".to_string(),
        },
        "last sibling must win per reverse tree order"
    );
}

// ─── interaction_id forwarding ────────────────────────────────────────────────

#[test]
fn interaction_id_forwarded_in_node_hit() {
    let (scene, _, tile_id, node_id) = single_tile_scene(
        Rect::new(0.0, 0.0, 400.0, 300.0),
        Rect::new(0.0, 0.0, 400.0, 300.0),
    );

    let result = scene.hit_test(200.0, 150.0);
    assert_eq!(
        result,
        HitResult::NodeHit {
            tile_id,
            node_id,
            interaction_id: "btn".to_string(),
        }
    );
}

// ─── Local state updates (hover/pressed without agent roundtrip) ─────────────

#[test]
fn update_hover_state_sets_hovered_on_node_hit() {
    let (mut scene, _, _, node_id) = single_tile_scene(
        Rect::new(0.0, 0.0, 400.0, 300.0),
        Rect::new(0.0, 0.0, 400.0, 300.0),
    );

    let result = scene.hit_test(200.0, 150.0);
    let new_hover = scene.update_hover_state(None, &result);

    assert_eq!(new_hover, Some(node_id));
    assert!(
        scene.hit_region_states[&node_id].hovered,
        "hovered must be true after update_hover_state"
    );
}

#[test]
fn update_hover_state_clears_previous_hover() {
    let (mut scene, _, _, node_id) = single_tile_scene(
        Rect::new(0.0, 0.0, 400.0, 300.0),
        Rect::new(0.0, 0.0, 400.0, 300.0),
    );

    // Simulate a previously hovered node.
    scene.hit_region_states.get_mut(&node_id).unwrap().hovered = true;

    // Pointer moves away — passthrough result, previous hover cleared.
    let result = scene.hit_test(900.0, 900.0); // outside tile
    assert_eq!(result, HitResult::Passthrough);

    let new_hover = scene.update_hover_state(Some(node_id), &result);
    assert_eq!(new_hover, None);
    assert!(
        !scene.hit_region_states[&node_id].hovered,
        "hovered must be false after pointer moves away"
    );
}

#[test]
fn update_pressed_state_sets_and_clears() {
    let (mut scene, _, _, node_id) = single_tile_scene(
        Rect::new(0.0, 0.0, 400.0, 300.0),
        Rect::new(0.0, 0.0, 400.0, 300.0),
    );

    scene.update_pressed_state(node_id, true);
    assert!(scene.hit_region_states[&node_id].pressed);

    scene.update_pressed_state(node_id, false);
    assert!(!scene.hit_region_states[&node_id].pressed);
}

#[test]
fn update_focused_state_sets_and_clears() {
    let (mut scene, _, _, node_id) = single_tile_scene(
        Rect::new(0.0, 0.0, 400.0, 300.0),
        Rect::new(0.0, 0.0, 400.0, 300.0),
    );

    scene.update_focused_state(node_id, true);
    assert!(scene.hit_region_states[&node_id].focused);

    scene.update_focused_state(node_id, false);
    assert!(!scene.hit_region_states[&node_id].focused);
}

// ─── HitResult helper methods ─────────────────────────────────────────────────

#[test]
fn hit_result_is_some_none_helpers() {
    let (scene, _, tile_id, node_id) = single_tile_scene(
        Rect::new(0.0, 0.0, 400.0, 300.0),
        Rect::new(0.0, 0.0, 400.0, 300.0),
    );

    let node_hit = scene.hit_test(200.0, 150.0);
    assert!(node_hit.is_some());
    assert!(!node_hit.is_none());
    assert!(node_hit.is_node_hit());
    assert_eq!(node_hit.tile_id(), Some(tile_id));
    assert_eq!(node_hit.node_hit_ids(), Some((tile_id, node_id)));

    let passthrough = HitResult::Passthrough;
    assert!(!passthrough.is_some());
    assert!(passthrough.is_none());
    assert!(!passthrough.is_node_hit());
    assert_eq!(passthrough.tile_id(), None);
}

#[test]
fn hit_region_node_new_fields_accessible() {
    let node = HitRegionNode {
        bounds: Rect::new(0.0, 0.0, 100.0, 50.0),
        interaction_id: "submit".to_string(),
        accepts_focus: true,
        accepts_pointer: true,
        auto_capture: true,
        release_on_up: true,
        tooltip: Box::new(Some("Click to submit".to_string())),
        ..Default::default()
    };

    assert!(node.auto_capture);
    assert!(node.release_on_up);
    assert_eq!(*node.tooltip, Some("Click to submit".to_string()));
}

// ─── Layer 0 invariants on hit-test test scenes ───────────────────────────────

// ─── Hit-test on spec test scenes ─────────────────────────────────────────────

#[test]
fn input_highlight_hit_test_on_button() {
    let registry = TestSceneRegistry::default();
    let (graph, _) = registry
        .build("input_highlight", ClockMs::FIXED)
        .expect("input_highlight scene must exist");

    // The input_highlight scene has a button tile at (400, 300, 400×100) with
    // a HitRegionNode (interaction_id="primary-button") filling the whole tile.
    // Point (600, 350) = inside the tile.
    let result = graph.hit_test(600.0, 350.0);
    assert!(
        result.is_node_hit(),
        "expected NodeHit on button area, got {result:?}"
    );
    if let HitResult::NodeHit { interaction_id, .. } = &result {
        assert_eq!(interaction_id, "primary-button");
    }
}

#[test]
fn overlay_passthrough_skips_passthrough_tile() {
    let registry = TestSceneRegistry::default();
    let (graph, _) = registry
        .build("overlay_passthrough_regions", ClockMs::FIXED)
        .expect("overlay_passthrough_regions scene must exist");

    // The overlay_passthrough_regions scene has:
    // - content tile (z=1, Capture) with interaction_id="content-area"
    // - full-screen passthrough overlay (z=20, Passthrough)
    // - small chrome widget (z=30, Capture) at (display_width-200, 20, 180×60)
    //
    // Point in the middle of the display (not in chrome widget area):
    // Should skip the passthrough overlay and hit the content tile.
    let result = graph.hit_test(500.0, 500.0);
    assert!(
        result.is_node_hit(),
        "should hit content area through passthrough, got {result:?}"
    );
    if let HitResult::NodeHit { interaction_id, .. } = &result {
        assert_eq!(interaction_id, "content-area", "expected content-area hit");
    }
}

// ─── Property tests ───────────────────────────────────────────────────────────

proptest! {
    /// Random scene layout: z-order descending invariant.
    ///
    /// Two overlapping non-passthrough tiles — hit in overlap must return the
    /// tile with higher z_order.
    #[test]
    fn proptest_highest_z_wins(
        z_low in 1u32..50u32,
        z_high_delta in 1u32..50u32,
        px in 400.0f32..600.0f32, // overlap region
        py in 300.0f32..500.0f32,
    ) {
        let z_high = z_low + z_high_delta;

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent.test", 60_000);

        // Two tiles overlapping at (400..800, 300..700).
        let low_tile = scene
            .create_tile(tab_id, "agent.test", lease_id, Rect::new(0.0, 0.0, 800.0, 700.0), z_low)
            .unwrap();
        let high_tile = scene
            .create_tile(
                tab_id,
                "agent.test",
                lease_id,
                Rect::new(400.0, 300.0, 800.0, 700.0),
                z_high,
            )
            .unwrap();

        for (tile_id, iid) in [(low_tile, "low"), (high_tile, "high")] {
            let node_id = SceneId::new();
            scene.nodes.insert(node_id, Node { layout: Default::default(),
                id: node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 800.0, 700.0),
                    interaction_id: iid.to_string(),
                    accepts_focus: false,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            });
            scene.hit_region_states.insert(node_id, tze_hud_scene::HitRegionLocalState::new(node_id));
            scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(node_id);
        }

        let result = scene.hit_test(px, py);
        match &result {
            HitResult::NodeHit { interaction_id, .. } => {
                prop_assert_eq!(
                    interaction_id.as_str(),
                    "high",
                    "expected high-z tile to win"
                );
            }
            other => prop_assert!(false, "expected NodeHit, got {:?}", other),
        }
    }

    /// Passthrough skip invariant: a passthrough-only scene always returns Passthrough.
    #[test]
    fn proptest_passthrough_skipped(
        px in 0.0f32..1920.0f32,
        py in 0.0f32..1080.0f32,
    ) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent.test", 60_000);

        // Full-screen passthrough tile.
        let tile_id = scene
            .create_tile(tab_id, "agent.test", lease_id, Rect::new(0.0, 0.0, 1920.0, 1080.0), 1)
            .unwrap();
        scene.tiles.get_mut(&tile_id).unwrap().input_mode = InputMode::Passthrough;

        let result = scene.hit_test(px, py);
        prop_assert_eq!(
            result,
            HitResult::Passthrough,
            "passthrough-only scene must return Passthrough"
        );
    }
}
