use super::test_helpers::{make_scene, make_scene_with_clock};
use super::*;
use crate::types::{Node, NodeData, Rect, Rgba, SceneId, SolidColorNode};

// ─ Tab limit enforcement (spec line 50) ──────────────────────────────────
// WHEN an agent attempts CreateTab and 256 tabs already exist
// THEN the runtime MUST reject with BudgetExceeded

#[test]
fn tab_limit_256_enforced() {
    let mut scene = make_scene();
    for i in 0..MAX_TABS {
        scene
            .create_tab(&format!("Tab {i}"), i as u32)
            .expect("should create tab");
    }
    assert_eq!(scene.tabs.len(), MAX_TABS);
    let err = scene.create_tab("Overflow", MAX_TABS as u32).unwrap_err();
    assert!(
        matches!(err, ValidationError::BudgetExceeded { .. }),
        "expected BudgetExceeded, got {err:?}"
    );

    let violations = crate::test_scenes::assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations at max tabs: {violations:?}"
    );
}

// ─ Tile limit enforcement (spec line 54) ─────────────────────────────────
// WHEN an agent attempts CreateTile on a tab that already has 1024 tiles
// THEN the runtime MUST reject with BudgetExceeded

#[test]
fn tile_limit_1024_per_tab_enforced() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 300_000);

    // The test scene is 1920×1080; tiles are 1px×1px at unique positions.
    // Use a grid: 32 cols × 32 rows = 1024. We'll use tiny tiles in bounds.
    // Actually: MAX_TILES_PER_TAB = 1024.
    for i in 0..(MAX_TILES_PER_TAB) {
        let x = (i % 40) as f32 * 48.0;
        let y = (i / 40) as f32 * 42.0;
        if x + 40.0 <= 1920.0 && y + 40.0 <= 1080.0 {
            scene
                .create_tile(
                    tab_id,
                    "agent",
                    lease_id,
                    Rect::new(x, y, 40.0, 40.0),
                    i as u32,
                )
                .expect("should create tile within limit");
        } else {
            // Re-use same position for tiles that would go out of bounds (unchecked path ignores bounds)
            scene
                .create_tile(
                    tab_id,
                    "agent",
                    lease_id,
                    Rect::new(0.0, 0.0, 1.0, 1.0),
                    i as u32,
                )
                .expect("should create tile within limit");
        }
    }
    assert_eq!(
        scene.tiles.values().filter(|t| t.tab_id == tab_id).count(),
        MAX_TILES_PER_TAB
    );

    let err = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 1.0, 1.0),
            MAX_TILES_PER_TAB as u32,
        )
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::BudgetExceeded { .. }),
        "expected BudgetExceeded, got {err:?}"
    );
}

// ─ Node limit enforcement (spec line 58) ─────────────────────────────────
// WHEN an agent attempts InsertNode on a tile with 64 nodes
// THEN the runtime MUST reject with NodeCountExceeded

#[test]
fn node_limit_64_per_tile_enforced() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 300_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 400.0),
            1,
        )
        .unwrap();

    // Add root node first, then chain children off the root.
    let root_id = SceneId::new();
    let root_node = Node {
        layout: Default::default(),
        id: root_id,
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            bounds: Rect::new(0.0, 0.0, 400.0, 400.0),
            radius: None,
        }),
    };
    scene
        .add_node_to_tile(tile_id, None, root_node)
        .expect("root should be added");

    // Add MAX_NODES_PER_TILE - 1 children off the root (total will be MAX_NODES_PER_TILE)
    for i in 1..MAX_NODES_PER_TILE {
        let child = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::SolidColor(SolidColorNode {
                color: Rgba::new(0.1 * (i % 10) as f32, 0.0, 0.0, 1.0),
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                radius: None,
            }),
        };
        scene
            .add_node_to_tile(tile_id, Some(root_id), child)
            .unwrap_or_else(|e| panic!("should add child {i} ok: {e:?}"));
    }

    // Verify we have exactly MAX_NODES_PER_TILE nodes in the tile
    let count = scene.count_node_subtree(root_id);
    assert_eq!(
        count as usize, MAX_NODES_PER_TILE,
        "should have exactly {MAX_NODES_PER_TILE} nodes"
    );

    // One more should be rejected
    let overflow_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::BLACK,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            radius: None,
        }),
    };
    let err = scene
        .add_node_to_tile(tile_id, Some(root_id), overflow_node)
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::NodeCountExceeded { .. }),
        "expected NodeCountExceeded, got {err:?}"
    );
}

// ─ Duplicate NodeId rejection (spec line 62) ─────────────────────────────
// WHEN an agent attempts to add a node with a NodeId that already exists in the scene
// THEN the runtime MUST reject with DuplicateId

#[test]
fn duplicate_node_id_rejected() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 300_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 200.0),
            1,
        )
        .unwrap();

    let node_id = SceneId::new();
    let node = Node {
        layout: Default::default(),
        id: node_id,
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            radius: None,
        }),
    };
    // First insertion succeeds
    scene
        .add_node_to_tile(tile_id, None, node.clone())
        .expect("first insert should succeed");

    // Second insertion with the same node ID should fail
    let tile_id2 = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(200.0, 0.0, 200.0, 200.0),
            2,
        )
        .unwrap();
    let err = scene.add_node_to_tile(tile_id2, None, node).unwrap_err();
    assert!(
        matches!(err, ValidationError::DuplicateId { id } if id == node_id),
        "expected DuplicateId, got {err:?}"
    );
}

// ─ Tab name too long (spec line 79) ──────────────────────────────────────
// WHEN an agent submits CreateTab with a name exceeding 128 UTF-8 bytes
// THEN the runtime MUST reject with InvalidFieldValue

#[test]
fn tab_name_too_long_rejected() {
    let mut scene = make_scene();
    let long_name = "a".repeat(MAX_TAB_NAME_BYTES + 1);
    let err = scene.create_tab(&long_name, 0).unwrap_err();
    assert!(
        matches!(err, ValidationError::InvalidField { ref field, .. } if field == "name"),
        "expected InvalidField for name, got {err:?}"
    );
}

// ─ Create and switch tab (spec line 71) ──────────────────────────────────
// WHEN an agent with manage_tabs submits CreateTab + SwitchActiveTab
// THEN the new tab MUST be created and become active

#[test]
fn create_and_switch_tab_with_capability() {
    let mut scene = make_scene();
    let lease_id = scene.grant_lease("agent", 300_000);
    let tab_id = scene.create_tab_with_lease("New Tab", 0, lease_id).unwrap();
    scene
        .switch_active_tab_with_lease(tab_id, lease_id)
        .unwrap();
    assert_eq!(scene.active_tab, Some(tab_id));
}

// ─ Create tile with valid lease (spec line 92) ────────────────────────────
// WHEN an agent with a valid lease submits CreateTile
// THEN the tile MUST be created with specified bounds, z_order, and opacity

#[test]
fn create_tile_checked_with_active_lease() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();

    let lease_full = scene.grant_lease("agent", 300_000);
    let tile_id = scene
        .create_tile_checked(
            tab_id,
            "agent",
            lease_full,
            Rect::new(0.0, 0.0, 200.0, 200.0),
            5,
        )
        .unwrap();
    assert_eq!(scene.tiles[&tile_id].z_order, 5);
    assert!((scene.tiles[&tile_id].opacity - 1.0).abs() < f32::EPSILON);
}

// ─ Tile mutation with expired lease (spec line 96) ───────────────────────
// WHEN an agent submits UpdateTileBounds but the tile's lease has expired
// THEN the runtime MUST reject with LeaseExpired

#[test]
fn tile_mutation_with_expired_lease_rejected() {
    let (mut scene, clock) = make_scene_with_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 100);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 200.0),
            1,
        )
        .unwrap();

    // Advance clock past TTL
    clock.advance(200);

    let err = scene
        .update_tile_bounds(tile_id, Rect::new(10.0, 10.0, 100.0, 100.0), "agent")
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::LeaseExpired { .. }),
        "expected LeaseExpired, got {err:?}"
    );
}

// ─ Delete tile (spec line 100) ─────────────────────────────────────────────
// WHEN an agent submits DeleteTile for a tile it owns with a valid lease
// THEN the tile and all its nodes MUST be removed

#[test]
fn delete_tile_removes_tile_and_nodes() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 300_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 200.0),
            1,
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
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::WHITE,
                    bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                    radius: None,
                }),
            },
        )
        .unwrap();
    assert!(scene.nodes.contains_key(&node_id));

    scene.delete_tile(tile_id, "agent").unwrap();
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "tile should be removed"
    );
    assert!(
        !scene.nodes.contains_key(&node_id),
        "nodes should be removed with tile"
    );
}

// ─ Ambient unread count per tile (hud-g1ena.3) ───────────────────────────

#[test]
fn tile_unread_count_set_get_and_prune() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 300_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 200.0),
            1,
        )
        .unwrap();

    // Defaults to 0 (no entry) — no unread badge.
    assert_eq!(scene.tile_unread_count(tile_id), 0);

    scene.set_tile_unread_count(tile_id, 3);
    assert_eq!(scene.tile_unread_count(tile_id), 3);

    // Latest-wins; 0 clears the badge locally (viewer returned to the tail).
    scene.set_tile_unread_count(tile_id, 0);
    assert_eq!(scene.tile_unread_count(tile_id), 0);

    // Set on a nonexistent tile is a no-op.
    scene.set_tile_unread_count(SceneId::new(), 9);

    // Pruned on tile deletion.
    scene.set_tile_unread_count(tile_id, 5);
    scene.delete_tile(tile_id, "agent").unwrap();
    assert_eq!(
        scene.tile_unread_count(tile_id),
        0,
        "unread count must be pruned when the tile is deleted"
    );
    assert!(!scene.overlay.tile_unread_counts.contains_key(&tile_id));
}

// ─ Opacity out of range (spec line 109) ──────────────────────────────────
// WHEN an agent submits UpdateTileOpacity with opacity = 1.5
// THEN the runtime MUST reject with InvalidFieldValue

#[test]
fn opacity_out_of_range_rejected() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 300_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 200.0),
            1,
        )
        .unwrap();

    let err = scene
        .update_tile_opacity(tile_id, 1.5, "agent")
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::InvalidField { ref field, .. } if field == "opacity"),
        "expected InvalidField(opacity), got {err:?}"
    );

    let err2 = scene
        .update_tile_opacity(tile_id, -0.1, "agent")
        .unwrap_err();
    assert!(
        matches!(err2, ValidationError::InvalidField { .. }),
        "got {err2:?}"
    );

    let result = scene.update_tile_opacity(tile_id, 0.5, "agent");
    assert!(result.is_ok(), "valid opacity must be accepted");

    let violations = crate::test_scenes::assert_layer0_invariants(&scene);
    assert!(
        violations.is_empty(),
        "Layer 0 violations after opacity tests: {violations:?}"
    );
}

// ─ Zero-size bounds (spec line 113) ──────────────────────────────────────
// WHEN an agent submits CreateTile with width = 0.0
// THEN the runtime MUST reject with BoundsOutOfRange

#[test]
fn zero_size_bounds_rejected() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();

    // create_tile_checked requires CreateTiles + ModifyOwnTiles; use correct capabilities
    // so the bounds check is reached (not capability check).
    let lease_id = scene.grant_lease("agent", 300_000);

    let err = scene
        .create_tile_checked(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 0.0, 100.0), // width = 0.0
            1,
        )
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::BoundsOutOfRange { .. }),
        "expected BoundsOutOfRange, got {err:?}"
    );

    // Use the basic create_tile (no capability check) to also confirm bounds are rejected
    let lease_unchecked = scene.grant_lease("agent", 300_000);
    let err2 = scene
        .create_tile(
            tab_id,
            "agent",
            lease_unchecked,
            Rect::new(0.0, 0.0, 0.0, 100.0),
            1,
        )
        .unwrap_err();
    assert!(
        matches!(err2, ValidationError::BoundsOutOfRange { .. }),
        "expected BoundsOutOfRange, got {err2:?}"
    );
}

// ─ Bounds outside tab area (spec line 117) ───────────────────────────────
// WHEN UpdateTileBounds with x + width exceeding tab display width
// THEN reject with BoundsOutOfRange

#[test]
fn bounds_outside_display_rejected() {
    let mut scene = make_scene(); // 1920×1080
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 300_000);

    let err = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(1800.0, 0.0, 200.0, 100.0),
            1,
        ) // x + w = 2000 > 1920
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::BoundsOutOfRange { .. }),
        "expected BoundsOutOfRange, got {err:?}"
    );
}

// ─ Z-order in reserved zone band (spec line 121) ─────────────────────────
// WHEN CreateTile with z_order = ZONE_TILE_Z_MIN
// THEN reject with InvalidFieldValue

#[test]
fn z_order_reserved_zone_band_rejected() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 300_000);

    let err = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            ZONE_TILE_Z_MIN,
        )
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::InvalidField { ref field, .. } if field == "z_order"),
        "expected InvalidField(z_order), got {err:?}"
    );

    // Also reject z_order above the threshold
    let err2 = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            ZONE_TILE_Z_MIN + 1,
        )
        .unwrap_err();
    assert!(
        matches!(err2, ValidationError::InvalidField { .. }),
        "got {err2:?}"
    );

    let err3 = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            u32::MAX,
        )
        .unwrap_err();
    assert!(
        matches!(err3, ValidationError::InvalidField { ref field, .. } if field == "z_order"),
        "expected InvalidField(z_order) for u32::MAX, got {err3:?}"
    );

    // z_order just below threshold is fine
    scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            ZONE_TILE_Z_MIN - 1,
        )
        .expect("z_order just below ZONE_TILE_Z_MIN must succeed");
}

// ─ Cross-namespace tile access denied (spec line 37) ─────────────────────
// WHEN agent "weather-agent" attempts to mutate a tile owned by namespace "cal"
// THEN reject with NamespaceMismatch

#[test]
fn cross_namespace_tile_access_denied() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let cal_lease = scene.grant_lease("cal", 300_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "cal",
            cal_lease,
            Rect::new(0.0, 0.0, 200.0, 200.0),
            1,
        )
        .unwrap();

    // weather-agent tries to update bounds of cal's tile
    let err = scene
        .update_tile_bounds(tile_id, Rect::new(10.0, 10.0, 100.0, 100.0), "wtr")
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::NamespaceMismatch { .. }),
        "expected NamespaceMismatch, got {err:?}"
    );
}

// ─ Struct size budgets (spec line 307, 311) ───────────────────────────────
// Tile < 200 bytes, Node < 160 bytes
//
// hud-yfj8u: the Node budget was raised 150 → 160 when the additive
// `layout: NodeLayout` field (vertical-flow layout mode) grew `Node` from 144 to
// 152 bytes — one enum byte plus 8-aligned padding, as `Node` sat exactly on an
// 8-byte boundary. 64 nodes/tile ≈ 9.9 KB structural overhead (still ~the RFC
// 0001 §8/§10 ~9.8 KB target). See §Struct Overhead Budgets in the scene-graph
// spec (amended in the portal-vertical-flow-layout change).

#[test]
fn tile_struct_size_under_200_bytes() {
    use std::mem::size_of;
    let tile_size = size_of::<Tile>();
    assert!(
        tile_size < 200,
        "Tile struct is {tile_size} bytes, must be < 200 bytes per RFC 0001 §8"
    );
}

#[test]
fn node_struct_size_under_160_bytes() {
    use std::mem::size_of;
    let node_size = size_of::<Node>();
    assert!(
        node_size < 160,
        "Node struct is {node_size} bytes, must be < 160 bytes per RFC 0001 §8 \
         (raised 150 → 160 for the additive NodeLayout field, hud-yfj8u)"
    );
}

// ─ Opacity valid range ────────────────────────────────────────────────────

#[test]
fn tile_opacity_accepts_boundary_values() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 300_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    scene.update_tile_opacity(tile_id, 0.0, "agent").unwrap();
    assert!((scene.tiles[&tile_id].opacity - 0.0).abs() < f32::EPSILON);

    scene.update_tile_opacity(tile_id, 1.0, "agent").unwrap();
    assert!((scene.tiles[&tile_id].opacity - 1.0).abs() < f32::EPSILON);

    scene.update_tile_opacity(tile_id, 0.5, "agent").unwrap();
    assert!((scene.tiles[&tile_id].opacity - 0.5).abs() < f32::EPSILON);
}
