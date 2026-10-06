use super::test_helpers::{make_scene, scene_with_gauge_and_clock};
use super::*;
use crate::clock::TestClock;
use crate::types::{
    ContentionPolicy, HitRegionNode, Node, NodeData, Rect, Rgba, SceneId, SolidColorNode,
    WidgetParameterValue,
};

/// `remove_tile_and_nodes` populates `recently_removed_tile_ids`; draining
/// that queue via `drain_removed_tile_ids` yields the removed tile ID.
///
/// This is the scene-layer half of the hud-4tuw5 contract.  The windowed
/// runtime drains this queue in `prune_portal_resize_states` to eagerly
/// remove the tile's entry from `portal_resize_states`.
#[test]
fn portal_resize_drain_queue_populated_by_remove_tile() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("portal-agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-agent",
            lease_id,
            crate::Rect::new(100.0, 100.0, 400.0, 300.0),
            1,
        )
        .unwrap();

    // Drain queue must be empty before any removal.
    assert!(
        scene.drain_removed_tile_ids().is_empty(),
        "drain queue must be empty before any tile removal"
    );

    // Remove the tile via the canonical path.
    scene.remove_tile_and_nodes(tile_id);

    // The tile must no longer be in the tiles map.
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "tile must be absent from scene after remove_tile_and_nodes"
    );

    // Drain the queue — must yield exactly the removed tile ID.
    let removed_ids = scene.drain_removed_tile_ids();
    assert_eq!(
        removed_ids,
        vec![tile_id],
        "drain queue must contain exactly the removed tile ID (hud-4tuw5)"
    );

    // Queue must be empty after drain (idempotent).
    assert!(
        scene.drain_removed_tile_ids().is_empty(),
        "drain queue must be empty after drain"
    );
}

/// Multiple successive tile removals each append to the drain queue;
/// a single `drain_removed_tile_ids` call returns all of them.
#[test]
fn portal_resize_drain_queue_accumulates_multiple_removals() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("portal-agent", 60_000);
    let tile_a = scene
        .create_tile(
            tab_id,
            "portal-agent",
            lease_id,
            crate::Rect::new(0.0, 0.0, 300.0, 200.0),
            1,
        )
        .unwrap();
    let tile_b = scene
        .create_tile(
            tab_id,
            "portal-agent",
            lease_id,
            crate::Rect::new(400.0, 0.0, 300.0, 200.0),
            2,
        )
        .unwrap();

    scene.remove_tile_and_nodes(tile_a);
    scene.remove_tile_and_nodes(tile_b);

    let removed_ids = scene.drain_removed_tile_ids();
    assert_eq!(
        removed_ids.len(),
        2,
        "both removed tile IDs must be in queue"
    );
    assert!(
        removed_ids.contains(&tile_a),
        "tile_a must be in the drain queue"
    );
    assert!(
        removed_ids.contains(&tile_b),
        "tile_b must be in the drain queue"
    );

    assert!(
        scene.drain_removed_tile_ids().is_empty(),
        "drain queue must be empty after drain"
    );
}

// ─── Cycle-guard tests ───────────────────────────────────────────────────
//
// These tests inject synthetic cycles directly into `scene.nodes` (bypassing
// the public API which would normally prevent cycles) to verify that each DFS
// traversal function terminates instead of recursing indefinitely.

/// Helper: build a SolidColor node with explicit id and children list.
fn solid_node(id: SceneId, children: Vec<SceneId>) -> Node {
    Node {
        layout: Default::default(),
        id,
        children,
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            radius: None,
        }),
    }
}

/// Helper: build a HitRegion node with explicit id and children list.
fn hit_node(id: SceneId, children: Vec<SceneId>) -> Node {
    Node {
        layout: Default::default(),
        id,
        children,
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            interaction_id: "cycle-test".to_string(),
            accepts_pointer: true,
            accepts_focus: false,
            ..Default::default()
        }),
    }
}

/// count_node_subtree: cycle A→B→A terminates and returns a finite count.
#[test]
fn count_node_subtree_cycle_terminates() {
    let mut scene = make_scene();
    let id_a = SceneId::new();
    let id_b = SceneId::new();
    // A points to B, B points back to A — a direct 2-node cycle.
    scene.nodes.insert(id_a, solid_node(id_a, vec![id_b]));
    scene.nodes.insert(id_b, solid_node(id_b, vec![id_a]));

    // Must not hang; result should be finite (2: A + B, cycle back to A is skipped).
    let count = scene.count_node_subtree(id_a);
    assert_eq!(count, 2, "cycle should be detected; each node counted once");
}

/// count_node_subtree: self-referencing node (A→A) terminates.
#[test]
fn count_node_subtree_self_loop_terminates() {
    let mut scene = make_scene();
    let id_a = SceneId::new();
    scene.nodes.insert(id_a, solid_node(id_a, vec![id_a]));

    let count = scene.count_node_subtree(id_a);
    assert_eq!(count, 1, "self-loop: node counted once, cycle skipped");
}

/// sum_texture_bytes: cycle terminates and returns zero (no StaticImage nodes).
#[test]
fn sum_texture_bytes_cycle_terminates() {
    let mut scene = make_scene();
    let id_a = SceneId::new();
    let id_b = SceneId::new();
    scene.nodes.insert(id_a, solid_node(id_a, vec![id_b]));
    scene.nodes.insert(id_b, solid_node(id_b, vec![id_a]));

    // Must not hang; no StaticImage nodes so result is 0.
    let bytes = scene.sum_texture_bytes(id_a);
    assert_eq!(
        bytes, 0,
        "cycle should terminate; no texture bytes in solid-color nodes"
    );
}

/// hit_test_node: cycle terminates; HitRegion nodes in a cycle are still tested.
#[test]
fn hit_test_node_cycle_terminates() {
    let mut scene = make_scene();
    let id_a = SceneId::new();
    let id_b = SceneId::new();
    // Both nodes are HitRegion with accepts_pointer=true; A→B→A forms a cycle.
    scene.nodes.insert(id_a, hit_node(id_a, vec![id_b]));
    scene.nodes.insert(id_b, hit_node(id_b, vec![id_a]));

    // Point (50,50) is inside both nodes' bounds (0,0,100,100). Must not hang.
    let hit = scene.hit_test_node(id_a, 50.0, 50.0);
    assert!(
        hit.is_some(),
        "a HitRegion node should be found before cycle is detected"
    );
}

/// hit_test_node: no hit when point is outside all node bounds.
#[test]
fn hit_test_node_cycle_no_hit_outside_bounds() {
    let mut scene = make_scene();
    let id_a = SceneId::new();
    let id_b = SceneId::new();
    scene.nodes.insert(id_a, hit_node(id_a, vec![id_b]));
    scene.nodes.insert(id_b, hit_node(id_b, vec![id_a]));

    // Point (200, 200) is outside bounds (0,0,100,100). Must not hang.
    let hit = scene.hit_test_node(id_a, 200.0, 200.0);
    assert!(
        hit.is_none(),
        "point outside all bounds should yield no hit"
    );
}

/// is_node_in_subtree: returns true for a direct child.
#[test]
fn is_node_in_subtree_direct_child() {
    let mut scene = make_scene();
    let id_a = SceneId::new();
    let id_b = SceneId::new();
    scene.nodes.insert(id_a, solid_node(id_a, vec![id_b]));
    scene.nodes.insert(id_b, solid_node(id_b, vec![]));

    assert!(scene.is_node_in_subtree(id_a, id_b));
    assert!(!scene.is_node_in_subtree(id_b, id_a));
}

/// is_node_in_subtree: returns true when target equals root.
#[test]
fn is_node_in_subtree_root_equals_target() {
    let mut scene = make_scene();
    let id_a = SceneId::new();
    scene.nodes.insert(id_a, solid_node(id_a, vec![]));

    assert!(scene.is_node_in_subtree(id_a, id_a));
}

/// is_node_in_subtree: cycle A→B→A terminates; B is reachable from A.
#[test]
fn is_node_in_subtree_cycle_terminates() {
    let mut scene = make_scene();
    let id_a = SceneId::new();
    let id_b = SceneId::new();
    scene.nodes.insert(id_a, solid_node(id_a, vec![id_b]));
    scene.nodes.insert(id_b, solid_node(id_b, vec![id_a]));

    // Must not hang; B is reachable from A.
    assert!(scene.is_node_in_subtree(id_a, id_b));
}

/// is_node_in_subtree: cycle terminates when target is not in the subgraph.
#[test]
fn is_node_in_subtree_cycle_unreachable_node() {
    let mut scene = make_scene();
    let id_a = SceneId::new();
    let id_b = SceneId::new();
    let id_c = SceneId::new(); // not inserted — unreachable
    scene.nodes.insert(id_a, solid_node(id_a, vec![id_b]));
    scene.nodes.insert(id_b, solid_node(id_b, vec![id_a]));

    // Must not hang; C is not reachable from A.
    assert!(!scene.is_node_in_subtree(id_a, id_c));
}

// ── Terminal leases clear only their own publications ─────────────────────

/// One namespace, two leases: `tile` (short TTL) and `mcp` (long TTL), each
/// with a zone and a widget publication.
fn same_namespace_two_lease_publications() -> (SceneGraph, TestClock, SceneId, SceneId) {
    use crate::types::ZoneContent;
    let (mut scene, _tab, clock) =
        scene_with_gauge_and_clock(ContentionPolicy::MergeByKey { max_keys: 4 });
    let mut zone = super::tests::make_subtitle_zone();
    zone.contention_policy = ContentionPolicy::MergeByKey { max_keys: 4 };
    scene.register_zone(zone);
    let tile_lease = scene.grant_lease("agent.x", 1_000);
    let mcp_lease = scene.grant_lease("agent.x", 600_000);
    for (lease, text) in [(tile_lease, "tile"), (mcp_lease, "mcp")] {
        scene
            .publish_to_zone_with_lease(
                "subtitle",
                ZoneContent::StreamText(text.to_string()),
                "agent.x",
                lease,
                Some(format!("key-{text}")),
                None,
            )
            .unwrap();
        scene
            .publish_to_widget_for_lease(
                "gauge",
                std::collections::HashMap::from([(
                    "level".to_string(),
                    WidgetParameterValue::F32(0.5),
                )]),
                "agent.x",
                Some(format!("key-{text}")),
                0,
                None,
                Some(lease),
            )
            .unwrap();
    }
    (scene, clock, tile_lease, mcp_lease)
}

fn assert_only_lease_publications_remain(scene: &SceneGraph, lease: SceneId) {
    let zone = &scene.zone_registry.active_publishes["subtitle"];
    assert_eq!(zone.len(), 1, "zone: only the surviving lease remains");
    assert_eq!(zone[0].lease_id, Some(lease));
    let widget = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(widget.len(), 1, "widget: only the surviving lease remains");
    assert_eq!(widget[0].lease_id, Some(lease));
}

/// A tile lease expiring must not wipe the same namespace's MCP publications.
#[test]
fn tile_lease_reap_keeps_same_namespace_mcp_publications() {
    let (mut scene, clock, tile_lease, mcp_lease) = same_namespace_two_lease_publications();
    clock.advance(1_001 + SceneGraph::DEFAULT_GRACE_PERIOD_MS);
    let expiries = scene.expire_leases();
    assert_eq!(expiries.len(), 1);
    assert_eq!(expiries[0].lease_id, tile_lease);
    assert_only_lease_publications_remain(&scene, mcp_lease);
}

#[test]
fn revoked_lease_clears_only_its_publications() {
    let (mut scene, _clock, tile_lease, mcp_lease) = same_namespace_two_lease_publications();
    scene.revoke_lease(tile_lease).unwrap();
    assert_only_lease_publications_remain(&scene, mcp_lease);
}
