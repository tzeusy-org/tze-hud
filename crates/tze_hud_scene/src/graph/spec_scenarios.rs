use super::*;
use crate::clock::TestClock;
use crate::types::{
    Capability, FontFamily, HitRegionNode, Node, NodeData, Rect, Rgba, SceneId, SolidColorNode,
    TextAlign, TextMarkdownNode, TextOverflow,
};
use std::sync::Arc;

fn make_scene() -> SceneGraph {
    SceneGraph::new(1920.0, 1080.0)
}

fn make_scene_with_clock() -> (SceneGraph, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(1_000_000));
    let scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    (scene, clock)
}

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
}

// ─ Tile limit enforcement (spec line 54) ─────────────────────────────────
// WHEN an agent attempts CreateTile on a tab that already has 1024 tiles
// THEN the runtime MUST reject with BudgetExceeded

#[test]
fn tile_limit_1024_per_tab_enforced() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );

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
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
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
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
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

// ─ Tab mutation without capability (spec line 83) ─────────────────────────
// WHEN an agent without manage_tabs capability submits CreateTab
// THEN the runtime MUST reject with CapabilityMissing

#[test]
fn tab_create_without_manage_tabs_rejected() {
    let mut scene = make_scene();
    // Lease with no capabilities
    let lease_id = scene.grant_lease("agent", 300_000, vec![]);
    let err = scene
        .create_tab_with_lease("My Tab", 0, lease_id)
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::CapabilityMissing { ref capability } if capability.contains("ManageTabs")),
        "expected CapabilityMissing(ManageTabs), got {err:?}"
    );
}

// ─ Create and switch tab (spec line 71) ──────────────────────────────────
// WHEN an agent with manage_tabs submits CreateTab + SwitchActiveTab
// THEN the new tab MUST be created and become active

#[test]
fn create_and_switch_tab_with_capability() {
    let mut scene = make_scene();
    let lease_id = scene.grant_lease("agent", 300_000, vec![Capability::ManageTabs]);
    let tab_id = scene.create_tab_with_lease("New Tab", 0, lease_id).unwrap();
    scene
        .switch_active_tab_with_lease(tab_id, lease_id)
        .unwrap();
    assert_eq!(scene.active_tab, Some(tab_id));
}

// ─ Tab rename (spec line 75) ─────────────────────────────────────────────
// WHEN an agent submits RenameTab with a new name of 100 UTF-8 bytes
// THEN the tab name MUST be updated

#[test]
fn rename_tab_with_100_byte_name() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Original", 0).unwrap();
    let new_name = "a".repeat(100);
    scene.rename_tab(tab_id, &new_name).unwrap();
    assert_eq!(scene.tabs[&tab_id].name, new_name);
}

// ─ Create tile with valid lease (spec line 92) ────────────────────────────
// WHEN an agent with create_tiles + modify_own_tiles and valid lease submits CreateTile
// THEN the tile MUST be created with specified bounds, z_order, and opacity

#[test]
fn create_tile_checked_requires_capabilities() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();

    // No capabilities — should fail
    let lease_no_caps = scene.grant_lease("agent", 300_000, vec![]);
    let err = scene
        .create_tile_checked(
            tab_id,
            "agent",
            lease_no_caps,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::CapabilityMissing { .. }),
        "got {err:?}"
    );

    // Only create_tiles (not modify_own_tiles) — should still fail
    let lease_create_only = scene.grant_lease("agent", 300_000, vec![Capability::CreateTiles]);
    let err = scene
        .create_tile_checked(
            tab_id,
            "agent",
            lease_create_only,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap_err();
    assert!(
        matches!(err, ValidationError::CapabilityMissing { .. }),
        "got {err:?}"
    );

    // Full capabilities — should succeed
    let lease_full = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
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
    let lease_id = scene.grant_lease(
        "agent",
        100,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
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
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
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
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
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
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
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
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );

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
    let lease_unchecked = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
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
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );

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
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );

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

// ─ TextMarkdownNode content limit (spec line 130) ─────────────────────────
// WHEN TextMarkdownNode with content exceeding 65535 UTF-8 bytes
// THEN reject with InvalidFieldValue

#[test]
fn text_markdown_content_limit_enforced() {
    let oversized = "x".repeat(MAX_MARKDOWN_BYTES + 1);
    // Validate that the node construction itself is possible but the validation
    // catches it. We check via validate_node_data if it exists, or directly.
    // For now, test that creating such content is flagged at the graph level.
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: oversized.clone(),
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    // The validation function
    let err = validate_text_markdown_node_data(&node.data);
    assert!(err.is_some(), "oversized content should be flagged");
}

// ─ Cross-namespace tile access denied (spec line 37) ─────────────────────
// WHEN agent "weather-agent" attempts to mutate a tile owned by namespace "cal"
// THEN reject with CapabilityMissing or LeaseNotFound

#[test]
fn cross_namespace_tile_access_denied() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let cal_lease = scene.grant_lease(
        "cal",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
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

// ─ Tab CRUD full cycle ────────────────────────────────────────────────────

#[test]
fn tab_delete_removes_tiles_too() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();
    assert_eq!(scene.tile_count(), 1);

    scene.delete_tab(tab_id).unwrap();
    assert_eq!(scene.tabs.len(), 0, "tab should be removed");
    assert_eq!(scene.tile_count(), 0, "tiles should be removed with tab");
    assert_eq!(
        scene.active_tab, None,
        "active_tab should be None after deleting last tab"
    );
}

#[test]
fn tab_reorder_updates_display_order() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.reorder_tab(tab_id, 5).unwrap();
    assert_eq!(scene.tabs[&tab_id].display_order, 5);
}

#[test]
fn tab_reorder_conflict_rejected() {
    let mut scene = make_scene();
    let tab_a = scene.create_tab("A", 0).unwrap();
    let _tab_b = scene.create_tab("B", 1).unwrap();
    // Try to give tab_a the same order as tab_b
    let err = scene.reorder_tab(tab_a, 1).unwrap_err();
    assert!(
        matches!(err, ValidationError::DuplicateDisplayOrder { .. }),
        "got {err:?}"
    );
}

// ─ Opacity valid range ────────────────────────────────────────────────────

#[test]
fn tile_opacity_accepts_boundary_values() {
    let mut scene = make_scene();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease(
        "agent",
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
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

// ─ All test scenes pass Layer 0 invariants ───────────────────────────────

#[test]
fn all_test_scenes_pass_layer0_invariants() {
    use crate::test_scenes::{ClockMs, TestSceneRegistry, assert_layer0_invariants};

    let registry = TestSceneRegistry::new();
    let names = TestSceneRegistry::scene_names();

    for name in names {
        let (graph, _spec) = registry
            .build(name, ClockMs::FIXED)
            .unwrap_or_else(|| panic!("scene '{name}' failed to build"));
        let violations = assert_layer0_invariants(&graph);
        assert!(
            violations.is_empty(),
            "scene '{name}' has Layer 0 violations: {violations:?}"
        );
    }
}

// ─ V1 node types constructable without GPU ───────────────────────────────

#[test]
fn all_v1_node_types_constructable() {
    // SolidColorNode
    let _ = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.5, 0.5, 0.5, 1.0),
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            radius: None,
        }),
    };

    // TextMarkdownNode
    let _ = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "# Hello".to_string(),
            bounds: Rect::new(0.0, 0.0, 400.0, 200.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };

    // HitRegionNode
    let _ = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(10.0, 10.0, 100.0, 50.0),
            interaction_id: "btn-ok".to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            ..Default::default()
        }),
    };

    // StaticImageNode — constructable without GPU context
    // RS-4: uses resource_id + decoded_bytes, no raw blob data embedded.
    use crate::types::ImageFitMode;
    use crate::types::StaticImageNode;
    let _ = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id: ResourceId::of(b"4x4 test image"),
            width: 4,
            height: 4,
            decoded_bytes: 4u64 * 4 * 4, // 4×4 RGBA8
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
        }),
    };
}

// ─── Widget system unit tests ─────────────────────────────────────────────
//
// Acceptance criteria from hud-mim2.7:
// 1. WidgetParameterValue validation (f32 NaN/Inf rejection, type mismatch, enum constraint)
// 2. Widget registry (definition registration, instance creation, publish, occupancy)
// 3. Widget contention policies (LatestWins, Stack, MergeByKey, Replace)
//
// Source: widget-system/spec.md §Requirement: Widget Parameter Validation,
//         §Requirement: Widget Registry, §Requirement: Widget Contention.

// ── Helpers ───────────────────────────────────────────────────────────────

use crate::types::{
    ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetDefinition, WidgetInstance,
    WidgetParamConstraints, WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue,
    WidgetSvgLayer,
};

/// Build a minimal gauge WidgetDefinition for testing.
///
/// Parameters: level (f32, 0–1), label (string), severity (enum info/warning/error).
fn make_gauge_definition() -> WidgetDefinition {
    WidgetDefinition {
        id: "gauge".to_string(),
        name: "gauge".to_string(),
        description: "test gauge".to_string(),
        parameter_schema: vec![
            WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: Some(WidgetParamConstraints {
                    f32_min: Some(0.0),
                    f32_max: Some(1.0),
                    ..Default::default()
                }),
            },
            WidgetParameterDeclaration {
                name: "label".to_string(),
                param_type: WidgetParamType::String,
                default_value: WidgetParameterValue::String(String::new()),
                constraints: None,
            },
            WidgetParameterDeclaration {
                name: "severity".to_string(),
                param_type: WidgetParamType::Enum,
                default_value: WidgetParameterValue::Enum("info".to_string()),
                constraints: Some(WidgetParamConstraints {
                    enum_allowed_values: vec![
                        "info".to_string(),
                        "warning".to_string(),
                        "error".to_string(),
                    ],
                    ..Default::default()
                }),
            },
        ],
        layers: vec![WidgetSvgLayer {
            svg_file: "fill.svg".to_string(),
            bindings: vec![],
        }],
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.25,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::LatestWins,
        max_publishers: u32::MAX,
        ephemeral: false,
        hover_behavior: None,
    }
}

/// Register gauge definition + instance in a scene with one tab.
fn scene_with_gauge(contention: ContentionPolicy) -> (SceneGraph, SceneId /* tab_id */) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();

    let mut def = make_gauge_definition();
    def.default_contention_policy = contention;

    scene.widget_registry.register_definition(def);
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "gauge".to_string(),
        current_params: std::collections::HashMap::from([
            ("level".to_string(), WidgetParameterValue::F32(0.0)),
            (
                "label".to_string(),
                WidgetParameterValue::String(String::new()),
            ),
            (
                "severity".to_string(),
                WidgetParameterValue::Enum("info".to_string()),
            ),
        ]),
    });

    (scene, tab_id)
}

// ── WidgetParameterValue validation ───────────────────────────────────────

/// WHEN an f32 NaN value is submitted THEN publish_to_widget returns
/// WidgetParameterInvalidValue.
/// Source: widget-system/spec.md §Requirement: Widget Parameter Validation (F32 invariant).
#[test]
fn widget_publish_f32_nan_rejected() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "level".to_string(),
        WidgetParameterValue::F32(f32::NAN),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "NaN f32 should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

/// WHEN an f32 +Inf value is submitted THEN publish_to_widget returns
/// WidgetParameterInvalidValue.
#[test]
fn widget_publish_f32_pos_inf_rejected() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "level".to_string(),
        WidgetParameterValue::F32(f32::INFINITY),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "positive infinity f32 should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

/// WHEN an f32 -Inf value is submitted THEN publish_to_widget returns
/// WidgetParameterInvalidValue.
#[test]
fn widget_publish_f32_neg_inf_rejected() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "level".to_string(),
        WidgetParameterValue::F32(f32::NEG_INFINITY),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "negative infinity f32 should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

/// WHEN a string value is submitted for an f32 parameter THEN type mismatch error.
/// Source: widget-system/spec.md §Requirement: Widget Parameter Validation (type safety).
#[test]
fn widget_publish_f32_type_mismatch_rejected() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "level".to_string(),
        WidgetParameterValue::String("not a float".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterTypeMismatch { .. })
        ),
        "string for f32 param should produce WidgetParameterTypeMismatch, got: {result:?}"
    );
}

/// WHEN an enum value outside allowed_values is submitted THEN invalid value error.
/// Source: widget-system/spec.md §Requirement: Widget Parameter Validation (enum constraint).
#[test]
fn widget_publish_enum_out_of_allowed_values_rejected() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("critical".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "enum value outside allowed_values should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

/// WHEN an enum value within allowed_values is submitted THEN publish succeeds.
#[test]
fn widget_publish_enum_in_allowed_values_accepted() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("warning".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        result.is_ok(),
        "valid enum value should be accepted, got: {result:?}"
    );
}

/// WHEN an f32 value is within [min, max] THEN it is accepted unchanged.
#[test]
fn widget_publish_f32_in_range_accepted_unchanged() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params =
        std::collections::HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.75))]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(result.is_ok(), "in-range f32 should be accepted");
}

/// WHEN an f32 value exceeds max THEN it is clamped, not rejected.
/// Source: widget-system/spec.md — f32 out of range is clamped.
#[test]
fn widget_publish_f32_above_max_clamped() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    // level has max=1.0; submit 2.5 — should clamp to 1.0 without error
    let params =
        std::collections::HashMap::from([("level".to_string(), WidgetParameterValue::F32(2.5))]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(result.is_ok(), "out-of-range f32 should clamp, not reject");

    // The recorded publish should contain the clamped value.
    let pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(pubs.len(), 1);
    let recorded_level = pubs[0].params.get("level");
    assert!(
        matches!(recorded_level, Some(WidgetParameterValue::F32(v)) if (*v - 1.0).abs() < 1e-6),
        "clamped value should be 1.0, got: {recorded_level:?}"
    );
}

/// WHEN a parameter name is not in the widget schema THEN unknown-parameter error.
#[test]
fn widget_publish_unknown_parameter_rejected() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "bogus_param".to_string(),
        WidgetParameterValue::F32(0.5),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(result, Err(ValidationError::WidgetUnknownParameter { .. })),
        "unknown param name should produce WidgetUnknownParameter, got: {result:?}"
    );
}

/// WHEN a widget instance is not found THEN WidgetNotFound error.
#[test]
fn widget_publish_nonexistent_widget_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let params =
        std::collections::HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.5))]);
    let result = scene.publish_to_widget("no-such-widget", params, "agent", None, 0, None);
    assert!(
        matches!(result, Err(ValidationError::WidgetNotFound { .. })),
        "nonexistent widget should produce WidgetNotFound, got: {result:?}"
    );
}

// ── Widget registry unit tests ─────────────────────────────────────────────

/// WHEN a widget definition is registered THEN it can be retrieved by id.
/// Source: widget-system/spec.md §Requirement: Widget Registry.
#[test]
fn widget_registry_register_and_retrieve_definition() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let def = make_gauge_definition();
    scene.widget_registry.register_definition(def.clone());

    let retrieved = scene.widget_registry.get_definition("gauge");
    assert!(
        retrieved.is_some(),
        "registered definition should be retrievable"
    );
    assert_eq!(retrieved.unwrap().id, "gauge");
    assert_eq!(retrieved.unwrap().parameter_schema.len(), 3);
}

/// WHEN a widget instance is registered THEN it can be retrieved by instance_name.
#[test]
fn widget_registry_register_and_retrieve_instance() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();

    scene
        .widget_registry
        .register_definition(make_gauge_definition());
    let instance = WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "cpu-gauge".to_string(),
        current_params: Default::default(),
    };
    scene.widget_registry.register_instance(instance);

    let retrieved = scene.widget_registry.get_instance("cpu-gauge");
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().instance_name, "cpu-gauge");
    assert_eq!(retrieved.unwrap().widget_type_name, "gauge");
}

/// WHEN a definition is registered with the same id THEN it overwrites the old one.
#[test]
fn widget_registry_definition_overwrites_on_duplicate_id() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let mut def1 = make_gauge_definition();
    def1.description = "first".to_string();
    let mut def2 = make_gauge_definition();
    def2.description = "second".to_string();

    scene.widget_registry.register_definition(def1);
    scene.widget_registry.register_definition(def2);

    let retrieved = scene.widget_registry.get_definition("gauge").unwrap();
    assert_eq!(
        retrieved.description, "second",
        "second registration should win"
    );
}

#[test]
fn widget_registry_runtime_svg_handle_round_trip() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene
        .widget_registry
        .register_runtime_svg_handle("gauge", "fill.svg", "asset:runtime-handle");
    assert_eq!(
        scene
            .widget_registry
            .runtime_svg_handle("gauge", "fill.svg"),
        Some("asset:runtime-handle")
    );
}

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
    let lease_id = scene.grant_lease(
        "portal-agent",
        60_000,
        vec![
            crate::Capability::CreateTiles,
            crate::Capability::ModifyOwnTiles,
        ],
    );
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
    let lease_id = scene.grant_lease(
        "portal-agent",
        60_000,
        vec![
            crate::Capability::CreateTiles,
            crate::Capability::ModifyOwnTiles,
        ],
    );
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

#[test]
fn pending_widget_svg_queue_drains_in_fifo_order() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.enqueue_widget_svg_asset("gauge", "a.svg", vec![1, 2, 3]);
    scene.enqueue_widget_svg_asset("gauge", "b.svg", vec![4, 5]);

    let drained = scene.drain_pending_widget_svg_assets();
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].0, "gauge");
    assert_eq!(drained[0].1, "a.svg");
    assert_eq!(drained[0].2, vec![1, 2, 3]);
    assert_eq!(drained[1].1, "b.svg");
    assert!(scene.drain_pending_widget_svg_assets().is_empty());
}

/// WHEN querying occupancy with no active publications THEN effective_params
/// falls back to the definition's parameter defaults.
#[test]
fn widget_registry_occupancy_defaults_when_no_publications() {
    let (scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(occ.occupant_count, 0);
    assert_eq!(occ.active_publications.len(), 0);

    // Should fall back to definition defaults for all three declared parameters.
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.0).abs() < 1e-6),
        "default level should be 0.0, got: {level:?}"
    );
    let label = occ.effective_params.get("label");
    assert!(
        matches!(label, Some(WidgetParameterValue::String(s)) if s.is_empty()),
        "default label should be empty string, got: {label:?}"
    );
    let severity = occ.effective_params.get("severity");
    assert!(
        matches!(severity, Some(WidgetParameterValue::Enum(s)) if s == "info"),
        "default severity should be 'info', got: {severity:?}"
    );
}

/// WHEN querying occupancy for an unknown instance THEN None is returned.
#[test]
fn widget_registry_occupancy_unknown_instance_returns_none() {
    let (scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);
    let occ = scene.widget_registry.get_occupancy("no-such-gauge", tab_id);
    assert!(occ.is_none(), "unknown instance should return None");
}

// ── get_occupancy per-policy effective_params tests ───────────────────────

/// LatestWins: WHEN one publication is active THEN effective_params = that
/// publication's params merged over schema defaults.
///
/// Source: widget-system/spec.md §Requirement: Widget Contention.
#[test]
fn widget_occupancy_latest_wins_merges_over_defaults() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);

    // Publish only "level"; "label" and "severity" should fall back to defaults.
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.75),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(occ.occupant_count, 1);

    // Published param should reflect the publication value.
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.75).abs() < 1e-6),
        "LatestWins level should be 0.75, got: {level:?}"
    );

    // Unpublished params should retain schema defaults.
    let label = occ.effective_params.get("label");
    assert!(
        matches!(label, Some(WidgetParameterValue::String(s)) if s.is_empty()),
        "LatestWins: missing label should fall back to default empty string, got: {label:?}"
    );
    let severity = occ.effective_params.get("severity");
    assert!(
        matches!(severity, Some(WidgetParameterValue::Enum(s)) if s == "info"),
        "LatestWins: missing severity should fall back to default 'info', got: {severity:?}"
    );
}

/// LatestWins: WHEN two sequential publishes arrive THEN effective_params
/// reflects only the most recent one (merged over defaults).
#[test]
fn widget_occupancy_latest_wins_uses_most_recent() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.2),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.9),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(
        occ.occupant_count, 1,
        "LatestWins retains only 1 publication"
    );
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.9).abs() < 1e-6),
        "LatestWins: most recent level (0.9) should win, got: {level:?}"
    );
}

/// Stack: WHEN three publishes arrive THEN effective_params reflects the
/// top-of-stack (most recent) publication merged over defaults.
///
/// Source: widget-system/spec.md §Requirement: Widget Contention (Stack).
#[test]
fn widget_occupancy_stack_uses_top_of_stack() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 5 });

    for (i, level) in [0.1f32, 0.5f32, 0.8f32].iter().enumerate() {
        scene
            .publish_to_widget(
                "gauge",
                std::collections::HashMap::from([(
                    "level".to_string(),
                    WidgetParameterValue::F32(*level),
                )]),
                &format!("agent.{i}"),
                None,
                0,
                None,
            )
            .unwrap();
    }

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(
        occ.occupant_count, 3,
        "Stack should have 3 active publications"
    );

    // Top-of-stack = most recent = last pushed = 0.8.
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.8).abs() < 1e-6),
        "Stack: top-of-stack level should be 0.8, got: {level:?}"
    );

    // Unpublished params should fall back to schema defaults.
    let label = occ.effective_params.get("label");
    assert!(
        matches!(label, Some(WidgetParameterValue::String(s)) if s.is_empty()),
        "Stack: missing label should fall back to default empty string, got: {label:?}"
    );
}

/// Stack: WHEN stack exceeds max_depth THEN effective_params still reflects
/// the most recent (top-of-stack) publication.
#[test]
fn widget_occupancy_stack_top_after_depth_cap() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 3 });

    // Push 5 publications; oldest 2 will be evicted, leaving levels [0.2, 0.3, 0.4].
    for (i, level) in [0.0f32, 0.1f32, 0.2f32, 0.3f32, 0.4f32].iter().enumerate() {
        scene
            .publish_to_widget(
                "gauge",
                std::collections::HashMap::from([(
                    "level".to_string(),
                    WidgetParameterValue::F32(*level),
                )]),
                &format!("agent.{i}"),
                None,
                0,
                None,
            )
            .unwrap();
    }

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(
        occ.occupant_count, 3,
        "Stack(3) should cap at 3 publications"
    );

    // Top-of-stack is the most recent surviving publication (0.4).
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.4).abs() < 1e-6),
        "Stack: top-of-stack after depth cap should be 0.4, got: {level:?}"
    );
}

/// MergeByKey: WHEN two different-keyed publications are active THEN
/// effective_params merges both over defaults.
///
/// Source: widget-system/spec.md §Requirement: Widget Contention (MergeByKey).
#[test]
fn widget_occupancy_merge_by_key_merges_all_keys_over_defaults() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::MergeByKey { max_keys: 8 });

    // "cpu" key sets level=0.4; "mem" key sets level=0.6.
    // Since both touch the same param ("level"), the last-inserted key wins.
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.4),
            )]),
            "agent.a",
            Some("cpu".to_string()),
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([
                ("level".to_string(), WidgetParameterValue::F32(0.6)),
                (
                    "label".to_string(),
                    WidgetParameterValue::String("mem".to_string()),
                ),
            ]),
            "agent.b",
            Some("mem".to_string()),
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(
        occ.occupant_count, 2,
        "MergeByKey should have 2 active publications"
    );

    // "mem" was pushed after "cpu", so its level (0.6) wins for "level".
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.6).abs() < 1e-6),
        "MergeByKey: last-inserted key's level (0.6) should win, got: {level:?}"
    );

    // "label" was only set by "mem" — should appear in effective_params.
    let label = occ.effective_params.get("label");
    assert!(
        matches!(label, Some(WidgetParameterValue::String(s)) if s == "mem"),
        "MergeByKey: label from 'mem' key should be 'mem', got: {label:?}"
    );

    // "severity" was not set by either key — should fall back to schema default.
    let severity = occ.effective_params.get("severity");
    assert!(
        matches!(severity, Some(WidgetParameterValue::Enum(s)) if s == "info"),
        "MergeByKey: missing severity should fall back to default 'info', got: {severity:?}"
    );
}

/// MergeByKey: WHEN the same key is updated THEN effective_params reflects
/// the updated value.
#[test]
fn widget_occupancy_merge_by_key_updated_key_reflects_latest_value() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::MergeByKey { max_keys: 8 });

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.a",
            Some("cpu".to_string()),
            0,
            None,
        )
        .unwrap();
    // Same key — should replace the previous value in-place.
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.7),
            )]),
            "agent.a",
            Some("cpu".to_string()),
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(
        occ.occupant_count, 1,
        "Same-key update should not add a second record"
    );

    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.7).abs() < 1e-6),
        "MergeByKey: updated key level should be 0.7, got: {level:?}"
    );
}

/// Replace: WHEN a publication is active THEN effective_params = that
/// publication's params only (no defaults for missing keys).
///
/// Source: widget-system/spec.md §Requirement: Widget Contention (Replace).
#[test]
fn widget_occupancy_replace_no_default_fallback_for_missing_keys() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::Replace);

    // Publish only "level" — "label" and "severity" are omitted intentionally.
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.5),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(occ.occupant_count, 1);

    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.5).abs() < 1e-6),
        "Replace level should be 0.5, got: {level:?}"
    );

    // Replace must NOT include defaults for missing keys.
    assert!(
        !occ.effective_params.contains_key("label"),
        "Replace: absent keys must NOT be filled from defaults (label), got: {:?}",
        occ.effective_params.get("label")
    );
    assert!(
        !occ.effective_params.contains_key("severity"),
        "Replace: absent keys must NOT be filled from defaults (severity), got: {:?}",
        occ.effective_params.get("severity")
    );
}

/// Replace: WHEN two sequential publishes arrive THEN effective_params
/// reflects only the most recent one (no merge, no defaults).
#[test]
fn widget_occupancy_replace_uses_most_recent_params_only() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::Replace);

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.1),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "label".to_string(),
                WidgetParameterValue::String("replaced".to_string()),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(occ.occupant_count, 1, "Replace retains only 1 publication");

    // Second publish only set "label"; "level" must NOT appear (not in params,
    // and Replace does not fall back to defaults).
    assert!(
        !occ.effective_params.contains_key("level"),
        "Replace: prior 'level' must be gone after Replace by second publish, got: {:?}",
        occ.effective_params.get("level")
    );
    let label = occ.effective_params.get("label");
    assert!(
        matches!(label, Some(WidgetParameterValue::String(s)) if s == "replaced"),
        "Replace: label from second publish should be 'replaced', got: {label:?}"
    );
}

/// WHEN a publish is recorded THEN active_for_widget returns it.
#[test]
fn widget_registry_publish_recorded_in_active_for_widget() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params =
        std::collections::HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.8))]);
    scene
        .publish_to_widget("gauge", params, "agent.a", None, 0, None)
        .unwrap();

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(active.len(), 1);
    let level = active[0].params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.8).abs() < 1e-6),
        "recorded level should be 0.8, got: {level:?}"
    );
}

/// WHEN snapshot() is called THEN it includes all registered types and instances.
#[test]
fn widget_registry_snapshot_includes_all_types_and_instances() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);

    // Add a second instance
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "mem-gauge".to_string(),
        current_params: Default::default(),
    });

    let snapshot = scene.widget_registry.snapshot();
    assert_eq!(snapshot.widget_types.len(), 1, "one type registered");
    assert_eq!(snapshot.widget_instances.len(), 2, "two instances");
}

// ── Widget contention policy tests ─────────────────────────────────────────

/// LatestWins: WHEN two publishes arrive THEN only the latest is retained.
/// Source: widget-system/spec.md §Requirement: Widget Contention.
#[test]
fn widget_contention_latest_wins_replaces_previous() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.7),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(active.len(), 1, "LatestWins keeps only one publication");
    assert!(
        matches!(active[0].params.get("level"), Some(WidgetParameterValue::F32(v)) if (*v - 0.7).abs() < 1e-6),
        "latest publish (0.7) should win"
    );
}

/// Replace: identical to LatestWins in effect — only one record retained.
#[test]
fn widget_contention_replace_retains_only_latest() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Replace);

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.1),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.9),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(active.len(), 1, "Replace keeps only one publication");
    assert!(
        matches!(active[0].params.get("level"), Some(WidgetParameterValue::F32(v)) if (*v - 0.9).abs() < 1e-6),
    );
}

/// Stack: WHEN max_depth=3 and 4 publishes arrive THEN oldest is evicted.
/// Source: widget-system/spec.md §Requirement: Widget Contention (Stack depth cap).
#[test]
fn widget_contention_stack_evicts_oldest_at_max_depth() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 3 });

    for i in 0u32..4 {
        scene
            .publish_to_widget(
                "gauge",
                std::collections::HashMap::from([(
                    "level".to_string(),
                    WidgetParameterValue::F32(i as f32 * 0.25),
                )]),
                &format!("agent.{i}"),
                None,
                0,
                None,
            )
            .unwrap();
    }

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(active.len(), 3, "Stack(3) should keep at most 3 records");

    // The oldest (i=0, level=0.0) should have been evicted.
    let has_zero = active.iter().any(|r| {
        matches!(r.params.get("level"), Some(WidgetParameterValue::F32(v)) if (*v).abs() < 1e-6)
    });
    assert!(!has_zero, "oldest publish (level=0.0) should be evicted");

    // The correct items (i=1,2,3) should all be present.
    let levels: std::collections::BTreeSet<u32> = active
        .iter()
        .filter_map(|r| {
            if let Some(WidgetParameterValue::F32(v)) = r.params.get("level") {
                Some((v * 4.0).round() as u32)
            } else {
                None
            }
        })
        .collect();
    let expected_levels: std::collections::BTreeSet<u32> = [1, 2, 3].into();
    assert_eq!(
        levels, expected_levels,
        "Stack(3) should contain levels for i=1, 2, 3"
    );
}

/// Stack: WHEN max_depth=0 THEN every publish is immediately trimmed out,
/// leaving the stack empty.
///
/// Canonical semantics (matches zone publish_to_zone behavior): the push is
/// followed by a trim that drains all entries when max_depth == 0, so the
/// record is silently discarded.  The old widget implementation had a
/// diverged `if max > 0 &&` guard that made max_depth=0 unbounded instead —
/// that was a bug corrected by extracting apply_contention.
#[test]
fn widget_contention_stack_max_depth_zero_discards_all() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 0 });

    for i in 0u32..3 {
        scene
            .publish_to_widget(
                "gauge",
                std::collections::HashMap::from([(
                    "level".to_string(),
                    WidgetParameterValue::F32(i as f32 * 0.1),
                )]),
                &format!("agent.{i}"),
                None,
                0,
                None,
            )
            .unwrap();
    }

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        active.len(),
        0,
        "Stack(0) trims to 0: all publishes must be discarded (canonical semantics)"
    );
}

/// MergeByKey: WHEN same key is published twice THEN the record is replaced.
/// WHEN a different key is published THEN both records coexist.
/// Source: widget-system/spec.md §Requirement: Widget Contention (MergeByKey).
#[test]
fn widget_contention_merge_by_key_replaces_same_key() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::MergeByKey { max_keys: 8 });

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.4),
            )]),
            "agent.a",
            Some("cpu".to_string()),
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.6),
            )]),
            "agent.b",
            Some("mem".to_string()),
            0,
            None,
        )
        .unwrap();
    // Overwrite "cpu" key
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.2),
            )]),
            "agent.a",
            Some("cpu".to_string()),
            0,
            None,
        )
        .unwrap();

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(active.len(), 2, "MergeByKey should keep one record per key");

    let cpu_pub = active
        .iter()
        .find(|r| r.merge_key.as_deref() == Some("cpu"))
        .unwrap();
    assert!(
        matches!(cpu_pub.params.get("level"), Some(WidgetParameterValue::F32(v)) if (*v - 0.2).abs() < 1e-6),
        "cpu key should have updated to 0.2"
    );

    // The mem key must remain unaffected at its original value (0.6).
    let mem_pub = active
        .iter()
        .find(|r| r.merge_key.as_deref() == Some("mem"))
        .unwrap();
    assert!(
        matches!(mem_pub.params.get("level"), Some(WidgetParameterValue::F32(v)) if (*v - 0.6).abs() < 1e-6),
        "mem key should be unaffected and still be 0.6"
    );
}

// ── Widget publication TTL / expiry tests ─────────────────────────────────

/// Helper: scene with a gauge backed by a controllable TestClock.
fn scene_with_gauge_and_clock(contention: ContentionPolicy) -> (SceneGraph, SceneId, TestClock) {
    let clock = TestClock::new(1_000); // t=1 000 ms = 1 000 000 µs
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
    let tab_id = scene.create_tab("Main", 0).unwrap();

    let mut def = make_gauge_definition();
    def.default_contention_policy = contention;
    scene.widget_registry.register_definition(def);
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "gauge".to_string(),
        current_params: std::collections::HashMap::from([
            ("level".to_string(), WidgetParameterValue::F32(0.0)),
            (
                "label".to_string(),
                WidgetParameterValue::String(String::new()),
            ),
            (
                "severity".to_string(),
                WidgetParameterValue::Enum("info".to_string()),
            ),
        ]),
    });

    (scene, tab_id, clock)
}

/// WHEN drain_expired_widget_publications is called before any expiry time
/// has elapsed THEN no publications are removed.
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_publication_not_expired_before_deadline() {
    let (mut scene, _tab, _clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);

    // Publish with an expiry 10 s in the future (clock is at 1 000 ms = 1 000 000 µs).
    let expires_at = 1_000_000u64 + 10_000_000u64; // +10 s
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.5),
            )]),
            "agent.test",
            None,
            0,
            Some(expires_at),
        )
        .unwrap();

    // Drain without advancing the clock — publication must survive.
    let removed = scene.drain_expired_widget_publications();
    assert_eq!(removed, 0, "no publications should expire before deadline");
    assert_eq!(
        scene.widget_registry.active_for_widget("gauge").len(),
        1,
        "publication must still be present"
    );
}

/// WHEN drain_expired_widget_publications is called after the expiry time
/// has elapsed THEN the publication is removed.
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_publication_expires_after_deadline() {
    let (mut scene, _tab, clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);

    // Publish with a 1 s TTL (expires 1 s after t=1 000 ms).
    let expires_at = 1_000_000u64 + 1_000_000u64; // expires at t=2 000 ms
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.5),
            )]),
            "agent.test",
            None,
            0,
            Some(expires_at),
        )
        .unwrap();

    // Advance clock past the expiry point.
    clock.advance(1_001); // now at t=2 001 ms = 2 001 000 µs

    let removed = scene.drain_expired_widget_publications();
    assert_eq!(removed, 1, "one publication should have expired");
    assert_eq!(
        scene.widget_registry.active_for_widget("gauge").len(),
        0,
        "expired publication must be removed"
    );
}

/// WHEN drain_expired_widget_publications removes all publications from a
/// widget THEN the active_publishes entry is cleaned up (no empty Vec left).
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_empty_entry_cleaned_up_after_expiry() {
    let (mut scene, _tab, clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);

    let expires_at = 1_000_000u64 + 500_000u64; // +500 ms
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.75),
            )]),
            "agent.test",
            None,
            0,
            Some(expires_at),
        )
        .unwrap();

    clock.advance(600); // advance 600 ms past expiry
    scene.drain_expired_widget_publications();

    // The HashMap entry itself must be gone (no empty Vec).
    assert!(
        !scene.widget_registry.active_publishes.contains_key("gauge"),
        "empty widget publication entry must be removed after expiry"
    );
}

/// WHEN a publication with no expiry and one with an expiry coexist (Stack
/// policy) THEN only the expired publication is removed.
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_only_expired_publication_removed_when_mixed() {
    let (mut scene, _tab, clock) =
        scene_with_gauge_and_clock(ContentionPolicy::Stack { max_depth: 10 });

    let now_us = 1_000_000u64; // clock starts at t=1 000 ms
    let expires_soon = now_us + 500_000u64; // expires in 500 ms

    // Publish the soon-to-expire record first.
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.1),
            )]),
            "agent.short",
            None,
            0,
            Some(expires_soon),
        )
        .unwrap();

    // Publish a permanent record (no expiry).
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.9),
            )]),
            "agent.permanent",
            None,
            0,
            None,
        )
        .unwrap();

    assert_eq!(
        scene.widget_registry.active_for_widget("gauge").len(),
        2,
        "both publications should be present before expiry"
    );

    // Advance clock past the short expiry.
    clock.advance(600);

    let removed = scene.drain_expired_widget_publications();
    assert_eq!(removed, 1, "only the TTL publication should expire");

    let remaining = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(remaining.len(), 1, "one publication should remain");
    assert_eq!(
        remaining[0].publisher_namespace, "agent.permanent",
        "the permanent publication should survive"
    );
}

/// WHEN drain_expired_widget_publications removes a publication THEN the
/// scene version is incremented.
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_expiry_bumps_scene_version() {
    let (mut scene, _tab, clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);

    let expires_at = 1_000_000u64 + 200_000u64;
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.test",
            None,
            0,
            Some(expires_at),
        )
        .unwrap();

    let version_before = scene.version;
    clock.advance(300);
    scene.drain_expired_widget_publications();

    assert!(
        scene.version > version_before,
        "scene version must be incremented when a widget publication expires"
    );
}

/// WHEN drain_expired_widget_publications is called with no publications
/// THEN it returns 0 and does not panic.
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_drain_with_no_publications_is_noop() {
    let (mut scene, _tab, _clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);

    let removed = scene.drain_expired_widget_publications();
    assert_eq!(removed, 0, "draining an empty registry must return 0");
}

// ── clear_widget_for_publisher tests ──────────────────────────────────────

/// WHEN clear_widget_for_publisher is called with the publishing namespace
/// THEN that agent's publications are removed and the widget reverts to defaults.
#[test]
fn clear_widget_for_publisher_removes_own_publications() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);

    // Publish as "agent.a"
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.9),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    assert_eq!(scene.widget_registry.active_for_widget("gauge").len(), 1);

    // Clear as "agent.a" — should remove the publication
    scene
        .clear_widget_for_publisher("gauge", "agent.a")
        .unwrap();
    assert_eq!(
        scene.widget_registry.active_for_widget("gauge").len(),
        0,
        "agent.a's publication should be cleared"
    );
    match scene.widget_registry.instances["gauge"]
        .current_params
        .get("level")
    {
        Some(WidgetParameterValue::F32(v)) => {
            assert!(
                (*v - 0.0).abs() < f32::EPSILON,
                "level should reset to default after clear, got {v}"
            )
        }
        other => panic!("expected default F32 level after clear, got {other:?}"),
    }
}

/// WHEN the top stacked widget publication is cleared THEN current_params
/// refresh to the remaining publication instead of retaining stale pixels.
#[test]
fn clear_widget_for_publisher_refreshes_current_params_from_remaining_publish() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 4 });

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.7),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    scene
        .clear_widget_for_publisher("gauge", "agent.b")
        .unwrap();

    match scene.widget_registry.instances["gauge"]
        .current_params
        .get("level")
    {
        Some(WidgetParameterValue::F32(v)) => {
            assert!(
                (*v - 0.3).abs() < f32::EPSILON,
                "level should refresh to remaining publication, got {v}"
            )
        }
        other => panic!("expected remaining F32 level after clear, got {other:?}"),
    }
}

/// WHEN clear_widget_for_publisher is called with a different namespace
/// THEN only the matching publisher's records are removed.
#[test]
fn clear_widget_for_publisher_only_affects_own_publications() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 4 });

    // Publish as "agent.a" and "agent.b"
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.7),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();
    assert_eq!(scene.widget_registry.active_for_widget("gauge").len(), 2);

    // Clear as "agent.a" — only "agent.a"'s publication should be removed
    scene
        .clear_widget_for_publisher("gauge", "agent.a")
        .unwrap();
    let remaining = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        remaining.len(),
        1,
        "only agent.a's publication should be cleared"
    );
    assert_eq!(
        remaining[0].publisher_namespace, "agent.b",
        "agent.b's publication should remain"
    );
}

/// WHEN clear_widget_for_publisher is called for a namespace with no publications
/// THEN it succeeds as a no-op.
#[test]
fn clear_widget_for_publisher_noop_when_no_publications() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);

    // No publications yet — clear should succeed silently
    let result = scene.clear_widget_for_publisher("gauge", "agent.nobody");
    assert!(
        result.is_ok(),
        "should succeed even when no publications exist"
    );
    assert_eq!(scene.widget_registry.active_for_widget("gauge").len(), 0);
}

/// WHEN clear_widget_for_publisher is called with an unknown widget name
/// THEN it returns WidgetNotFound.
#[test]
fn clear_widget_for_publisher_widget_not_found() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);

    let result = scene.clear_widget_for_publisher("nonexistent", "agent.a");
    assert!(
        matches!(result, Err(ValidationError::WidgetNotFound { .. })),
        "unknown widget should produce WidgetNotFound, got: {result:?}"
    );
}

/// WHEN clear_widget_publications_for_namespace is called
/// THEN ALL widget publications for that namespace are removed across all widgets.
#[test]
fn clear_widget_publications_for_namespace_removes_all_for_namespace() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);

    // Register a second widget instance using the same definition
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "mem-gauge".to_string(),
        current_params: Default::default(),
    });

    // Publish as "agent.a" to both widgets
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.5),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "mem-gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.8),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();

    // Publish as "agent.b" to "gauge" only
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.9),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    // Clear ALL of "agent.a" publications
    scene.clear_widget_publications_for_namespace("agent.a");

    // "agent.a"'s publication on "gauge" is gone; "agent.b"'s remains
    let gauge_pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        gauge_pubs.len(),
        1,
        "only agent.b's gauge pub should remain"
    );
    assert_eq!(gauge_pubs[0].publisher_namespace, "agent.b");

    // "agent.a"'s publication on "mem-gauge" is gone
    let mem_pubs = scene.widget_registry.active_for_widget("mem-gauge");
    assert_eq!(
        mem_pubs.len(),
        0,
        "agent.a's mem-gauge pub should be cleared"
    );
    match scene.widget_registry.instances["mem-gauge"]
        .current_params
        .get("level")
    {
        Some(WidgetParameterValue::F32(v)) => {
            assert!(
                (*v - 0.0).abs() < f32::EPSILON,
                "mem-gauge should reset to default, got {v}"
            )
        }
        other => {
            panic!("expected default level for mem-gauge after namespace clear, got {other:?}")
        }
    }
}

/// WHEN ClearWidget is sent as a scene mutation batch
/// THEN it removes the agent's publications via the standard pipeline.
#[test]
fn clear_widget_via_mutation_batch() {
    use crate::mutation::{MutationBatch, SceneMutation};

    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 4 });

    // Publish as two agents
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.5),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();
    assert_eq!(scene.widget_registry.active_for_widget("gauge").len(), 2);

    // Send ClearWidget from "agent.a"
    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent.a".to_string(),
        mutations: vec![SceneMutation::ClearWidget {
            widget_name: "gauge".to_string(),
            instance_id: None,
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result = scene.apply_batch(&batch);
    assert!(result.applied, "ClearWidget batch should be accepted");

    // Only "agent.b"'s publication should remain
    let remaining = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        remaining.len(),
        1,
        "agent.a's publication should be cleared"
    );
    assert_eq!(remaining[0].publisher_namespace, "agent.b");
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
