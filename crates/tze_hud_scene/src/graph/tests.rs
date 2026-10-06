use super::*;
use crate::clock::TestClock;

/// Convenience: build a SceneGraph backed by a TestClock starting at t=1000ms.
fn scene_with_test_clock() -> (SceneGraph, TestClock) {
    let clock = TestClock::new(1_000);
    let scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
    (scene, clock)
}

#[test]
fn test_create_scene_with_tab_and_tiles() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);

    // Create a tab
    let tab_id = scene.create_tab("Main", 0).unwrap();
    assert_eq!(scene.active_tab, Some(tab_id));

    // Grant a lease
    let lease_id = scene.grant_lease("test-agent", 60_000);

    // Create two tiles
    let tile1_id = scene
        .create_tile(
            tab_id,
            "test-agent",
            lease_id,
            Rect::new(10.0, 10.0, 400.0, 300.0),
            1,
        )
        .unwrap();

    let tile2_id = scene
        .create_tile(
            tab_id,
            "test-agent",
            lease_id,
            Rect::new(420.0, 10.0, 400.0, 300.0),
            2,
        )
        .unwrap();

    assert_eq!(scene.tile_count(), 2);

    // Add nodes
    let text_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "Hello, tze_hud!".to_string(),
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            font_size_px: 24.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::WHITE,
            background: Some(Rgba::new(0.1, 0.1, 0.2, 1.0)),
            alignment: TextAlign::Center,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    scene.set_tile_root(tile1_id, text_node).unwrap();

    let hit_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(50.0, 50.0, 200.0, 100.0),
            interaction_id: "btn-click".to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            ..Default::default()
        }),
    };
    scene.set_tile_root(tile2_id, hit_node.clone()).unwrap();

    assert_eq!(scene.node_count(), 2);
    assert!(scene.hit_region_states.contains_key(&hit_node.id));
}

#[test]
fn test_hit_test() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);

    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(100.0, 100.0, 400.0, 300.0),
            1,
        )
        .unwrap();

    let hr_node_id = SceneId::new();
    let hit_node = Node {
        layout: Default::default(),
        id: hr_node_id,
        children: vec![],
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(50.0, 50.0, 200.0, 100.0),
            interaction_id: "btn".to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            ..Default::default()
        }),
    };
    scene.set_tile_root(tile_id, hit_node).unwrap();

    // Hit the hit region (tile at 100,100; region at 50,50 within tile = 150,150 global)
    let result = scene.hit_test(200.0, 180.0);
    assert_eq!(
        result,
        HitResult::NodeHit {
            tile_id,
            node_id: hr_node_id,
            interaction_id: "btn".to_string(),
        }
    );

    // Miss the hit region but hit the tile
    let result = scene.hit_test(110.0, 110.0);
    assert_eq!(result, HitResult::TileHit { tile_id });

    // Miss everything
    let result = scene.hit_test(10.0, 10.0);
    assert_eq!(result, HitResult::Passthrough);
}

#[test]
fn test_hit_test_applies_tile_scroll_offset() {
    let mut scene = SceneGraph::new(800.0, 600.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("scroll-agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "scroll-agent",
            lease_id,
            Rect::new(100.0, 100.0, 300.0, 200.0),
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
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(10.0, 60.0, 120.0, 40.0),
                    interaction_id: "scroll-hit".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 50.0)
        .unwrap();

    assert_eq!(
        scene.hit_test(120.0, 115.0),
        HitResult::NodeHit {
            tile_id,
            node_id,
            interaction_id: "scroll-hit".to_string(),
        }
    );
}

/// During an in-flight smoothed scroll, `hit_test` must map pointer coordinates
/// using the *displayed* (lagged) offset the renderer drew with — not the
/// authoritative scroll target (hud-3lynp). When no displayed offset is
/// published (headless / snap), hit-testing falls back to the authoritative
/// offset and behavior is unchanged.
#[test]
fn test_hit_test_uses_displayed_scroll_offset_during_animation() {
    let mut scene = SceneGraph::new(800.0, 600.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("scroll-agent", 60_000);
    // Tile anchored at the origin so screen coords == tile-local coords (before
    // the scroll offset is applied), keeping the arithmetic easy to follow.
    let tile_id = scene
        .create_tile(
            tab_id,
            "scroll-agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 200.0),
            1,
        )
        .unwrap();

    // Two stacked, non-overlapping content rows:
    //   row A occupies content y ∈ [0, 100)
    //   row B occupies content y ∈ [100, 200)
    let row_a = SceneId::new();
    let row_b = SceneId::new();
    let root = SceneId::new();
    scene.nodes.insert(
        row_a,
        Node {
            layout: Default::default(),
            id: row_a,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                interaction_id: "row-a".to_string(),
                accepts_focus: false,
                accepts_pointer: true,
                ..Default::default()
            }),
        },
    );
    scene.nodes.insert(
        row_b,
        Node {
            layout: Default::default(),
            id: row_b,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 100.0, 200.0, 100.0),
                interaction_id: "row-b".to_string(),
                accepts_focus: false,
                accepts_pointer: true,
                ..Default::default()
            }),
        },
    );
    scene.nodes.insert(
        root,
        Node {
            layout: Default::default(),
            id: root,
            children: vec![row_a, row_b],
            data: NodeData::SolidColor(SolidColorNode {
                color: Rgba::TRANSPARENT,
                bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                radius: None,
            }),
        },
    );
    scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(root);

    // Authoritative scroll target is fully scrolled to content y=100: with no
    // displayed override, screen y=50 maps to content y=150 → row B.
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 100.0)
        .unwrap();
    assert_eq!(
        scene.hit_test(50.0, 50.0),
        HitResult::NodeHit {
            tile_id,
            node_id: row_b,
            interaction_id: "row-b".to_string(),
        },
        "with no displayed offset published, hit_test uses the authoritative offset"
    );

    // Now simulate an in-flight smoothed scroll: the renderer is still drawing
    // at the lagged displayed offset y=0 (animation has not caught up to the
    // y=100 target). screen y=50 must map to content y=50 → row A, matching the
    // row the operator actually sees.
    scene.set_displayed_tile_scroll_offset(tile_id, 0.0, 0.0);
    assert_eq!(
        scene.hit_test(50.0, 50.0),
        HitResult::NodeHit {
            tile_id,
            node_id: row_a,
            interaction_id: "row-a".to_string(),
        },
        "during animation, hit_test must use the displayed (lagged) offset, not the authoritative one"
    );

    // Once the animation settles (or smoothing is disabled), clearing the
    // displayed offsets restores authoritative behavior — row B again.
    scene.clear_displayed_tile_scroll_offsets();
    assert_eq!(
        scene.hit_test(50.0, 50.0),
        HitResult::NodeHit {
            tile_id,
            node_id: row_b,
            interaction_id: "row-b".to_string(),
        },
        "clearing displayed offsets restores authoritative hit-testing"
    );
}

/// A geometry-only portal surface keeps its derived composer interaction region
/// tile-anchored. During a smoothed scroll, pointer mapping must use the same
/// fixed-chrome classification as rendering rather than applying the displayed
/// document scroll to the composer region.
#[test]
fn test_hit_test_keeps_geometry_only_portal_composer_fixed_after_resize_and_displayed_scroll() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("portal", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal",
            lease_id,
            Rect::new(100.0, 100.0, 500.0, 300.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();

    // Model a viewer-resized portal: the composer pane and its derived
    // interaction region use the resized surface-local geometry.
    scene
        .update_tile_bounds(tile_id, Rect::new(100.0, 100.0, 700.0, 440.0), "portal")
        .unwrap();
    let composer_bounds = Rect::new(18.0, 272.0, 664.0, 150.0);
    let document_bounds = Rect::new(30.0, 304.0, 640.0, 106.0);
    let root_id = SceneId::new();
    let document_id = SceneId::new();
    let composer_id = SceneId::new();
    scene
        .set_tile_root_tree(
            tile_id,
            Node {
                layout: Default::default(),
                id: root_id,
                children: vec![document_id, composer_id],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::TRANSPARENT,
                    bounds: Rect::new(0.0, 0.0, 700.0, 440.0),
                    radius: None,
                }),
            },
            vec![
                Node {
                    layout: Default::default(),
                    id: document_id,
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "draft document content".to_string(),
                        bounds: document_bounds,
                        font_size_px: 16.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
                Node {
                    layout: Default::default(),
                    id: composer_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: composer_bounds,
                        interaction_id: "portal-composer".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        accepts_composer_input: true,
                        ..Default::default()
                    }),
                },
            ],
        )
        .unwrap();
    scene
        .set_portal_surface(
            tile_id,
            PortalSurface {
                identity: PortalIdentity {
                    session_id: "sess-portal".to_string(),
                    display_name: "Claude".to_string(),
                    peer_class: PortalPeerClass::ResidentLlm,
                },
                lifecycle: PortalLifecycleState::Active,
                display_state: PortalDisplayState::Expanded,
                // No part has a backing node: this is the resident adapter's
                // geometry-only descriptor, whose pane chrome remains fixed.
                parts: vec![
                    PortalPart {
                        kind: PortalPartKind::Composer,
                        bounds: composer_bounds,
                        node: None,
                    },
                    PortalPart {
                        kind: PortalPartKind::Transcript,
                        bounds: Rect::new(18.0, 52.0, 664.0, 202.0),
                        node: None,
                    },
                ],
            },
            "portal",
        )
        .unwrap();

    // The renderer is still displaying an intermediate scroll position. The
    // fixed composer pane is visibly at y=272..422 tile-local, so a click near
    // its bottom must resolve to its derived HitRegion rather than being shifted
    // out of the pane by the displayed document scroll.
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 220.0)
        .unwrap();
    scene.set_displayed_tile_scroll_offset(tile_id, 0.0, 140.0);
    assert_eq!(
        scene.hit_test(150.0, 500.0),
        HitResult::NodeHit {
            tile_id,
            node_id: composer_id,
            interaction_id: "portal-composer".to_string(),
        },
        "the resized geometry-only composer region must remain at its rendered fixed position"
    );
}

#[test]
fn test_hit_test_zone_regions_without_active_tab() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    // Intentionally do not create/activate a tab

    // Add a zone hit region (as the compositor would do each frame)
    scene.overlay.zone_hit_regions.push(ZoneHitRegion {
        zone_name: "notifications".to_string(),
        published_at_wall_us: 123456,
        publisher_namespace: "test".to_string(),
        bounds: Rect::new(100.0, 100.0, 200.0, 150.0),
        kind: ZoneInteractionKind::Dismiss,
        interaction_id: "zone:notifications:dismiss:123456:test".to_string(),
        tab_order: 0,
    });

    // Hit the zone region even though active_tab is None
    let result = scene.hit_test(150.0, 125.0);
    match result {
        HitResult::ZoneInteraction {
            zone_name,
            published_at_wall_us,
            publisher_namespace,
            interaction_id,
            kind: ZoneInteractionKind::Dismiss,
        } => {
            assert_eq!(zone_name, "notifications");
            assert_eq!(published_at_wall_us, 123456);
            assert_eq!(publisher_namespace, "test");
            assert_eq!(interaction_id, "zone:notifications:dismiss:123456:test");
        }
        _ => panic!("Expected ZoneInteraction, got {result:?}"),
    }

    // Miss the zone region
    let result = scene.hit_test(50.0, 50.0);
    assert_eq!(result, HitResult::Passthrough);
}

#[test]
fn test_snapshot_roundtrip() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);
    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    let json = scene.snapshot_json().unwrap();
    let restored = SceneGraph::from_json(&json).unwrap();

    assert_eq!(scene.tile_count(), restored.tile_count());
    assert_eq!(scene.active_tab, restored.active_tab);
    assert_eq!(scene.version, restored.version);
}

#[test]
fn take_snapshot_includes_display_area() {
    let scene = SceneGraph::new(2560.0, 1440.0);

    let snapshot = scene.take_snapshot(1_000, 2_000);

    assert_eq!(snapshot.display_area, Rect::new(0.0, 0.0, 2560.0, 1440.0));
    assert!(snapshot.verify_checksum());
}

/// Build a scene with one tile owned by `namespace` whose root is a solid-color
/// node, returning `(scene, tile_id, root_id)`. Mirrors `portal_scene_with_tile`
/// in mutation.rs but drives the direct graph API.
fn portal_snapshot_scene(namespace: &str) -> (SceneGraph, SceneId, SceneId) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease(namespace, 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            namespace,
            lease_id,
            Rect::new(10.0, 10.0, 400.0, 300.0),
            1,
        )
        .unwrap();
    let root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.0, 0.0, 0.0, 1.0),
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            radius: None,
        }),
    };
    let root_id = root.id;
    scene.set_tile_root(tile_id, root).unwrap();
    (scene, tile_id, root_id)
}

fn portal_surface_pointing_at(node: SceneId) -> PortalSurface {
    PortalSurface {
        identity: PortalIdentity {
            session_id: "sess-1".to_string(),
            display_name: "Claude".to_string(),
            peer_class: PortalPeerClass::ResidentLlm,
        },
        lifecycle: PortalLifecycleState::Active,
        display_state: PortalDisplayState::Expanded,
        parts: vec![PortalPart {
            kind: PortalPartKind::Transcript,
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            node: Some(node),
        }],
    }
}

/// A reconnecting session must recover its declared portal surface from the
/// snapshot rather than re-declaring blindly (hud-ruynm reconnect parity).
#[test]
fn snapshot_carries_previously_declared_portal_surface() {
    let (mut scene, tile_id, root_id) = portal_snapshot_scene("agent");
    let surface = portal_surface_pointing_at(root_id);
    scene
        .set_portal_surface(tile_id, surface.clone(), "agent")
        .unwrap();

    let snapshot = scene.take_snapshot(1_000, 2_000);
    assert_eq!(
        snapshot.portal_surfaces.get(&tile_id),
        Some(&surface),
        "the declared portal surface must appear in the snapshot for reconnect parity"
    );
    assert!(snapshot.verify_checksum());
}

/// Snapshot round-trips a portal surface whose part-node ref was nulled by
/// `revalidate_portal_surface_part_nodes` after a transcript republish — the
/// snapshot serializes `node = null` faithfully and never fabricates a ref.
#[test]
fn snapshot_portal_surface_with_nulled_part_node_round_trips() {
    let (mut scene, tile_id, root_id) = portal_snapshot_scene("agent");
    scene
        .set_portal_surface(tile_id, portal_surface_pointing_at(root_id), "agent")
        .unwrap();

    // Republish the tile root: the old subtree (root_id) is removed, so the
    // Transcript part's node ref is revalidated back to None.
    let new_root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            radius: None,
        }),
    };
    scene.set_tile_root(tile_id, new_root).unwrap();

    let snapshot = scene.take_snapshot(1_000, 2_000);
    let part = &snapshot.portal_surfaces.get(&tile_id).unwrap().parts[0];
    assert_eq!(
        part.node, None,
        "a part node nulled by revalidation must serialize as None, not a stale/fabricated ref"
    );

    // JSON round-trip preserves the nulled ref and the checksum stays valid.
    let json = snapshot.to_json().unwrap();
    let restored = SceneGraphSnapshot::from_json(&json).unwrap();
    assert_eq!(restored.portal_surfaces, snapshot.portal_surfaces);
    assert_eq!(
        restored.portal_surfaces.get(&tile_id).unwrap().parts[0].node,
        None
    );
    assert!(restored.verify_checksum());
}

/// Portal surfaces are keyed by host tile id, so a session filtering the snapshot
/// to the tiles it owns keeps exactly its own surfaces and none from another
/// namespace (visibility parity with `tiles`).
#[test]
fn snapshot_portal_surfaces_are_tile_keyed_for_namespace_visibility() {
    // Namespace A: tile + surface.
    let (mut scene, tile_a, root_a) = portal_snapshot_scene("agent-a");
    scene
        .set_portal_surface(tile_a, portal_surface_pointing_at(root_a), "agent-a")
        .unwrap();

    // Namespace B: a second tile (on the same tab) + its own surface.
    let tab_id = scene.active_tab.unwrap();
    let lease_b = scene.grant_lease("agent-b", 60_000);
    let tile_b = scene
        .create_tile(
            tab_id,
            "agent-b",
            lease_b,
            Rect::new(500.0, 10.0, 400.0, 300.0),
            2,
        )
        .unwrap();
    let root_b = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.0, 0.0, 1.0, 1.0),
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            radius: None,
        }),
    };
    let root_b_id = root_b.id;
    scene.set_tile_root(tile_b, root_b).unwrap();
    scene
        .set_portal_surface(tile_b, portal_surface_pointing_at(root_b_id), "agent-b")
        .unwrap();

    let snapshot = scene.take_snapshot(1_000, 2_000);
    assert_eq!(snapshot.portal_surfaces.len(), 2);

    // A session scoped to namespace A keeps only the tiles (and thus surfaces)
    // it owns; namespace B's surface is invisible after filtering.
    let visible_to_a: std::collections::BTreeMap<_, _> = snapshot
        .portal_surfaces
        .iter()
        .filter(|(tile_id, _)| {
            snapshot
                .tiles
                .get(tile_id)
                .is_some_and(|t| t.namespace == "agent-a")
        })
        .collect();
    assert_eq!(visible_to_a.len(), 1);
    assert!(visible_to_a.contains_key(&tile_a));
    assert!(!visible_to_a.contains_key(&tile_b));
}

/// A snapshot with no portal surfaces must omit the `portal_surfaces` key from
/// its canonical JSON, so its checksum is byte-identical to a pre-field snapshot.
/// This preserves `verify_checksum()` for older surface-less snapshots produced
/// before this field existed (hud-ruynm backward-compat; guards the P2 raised on
/// PR #1098 where an unconditionally-serialized empty map broke old checksums).
#[test]
fn snapshot_without_portal_surfaces_omits_field_and_verifies_like_old_format() {
    let (scene, _tile_id, _root_id) = portal_snapshot_scene("agent");
    let snapshot = scene.take_snapshot(1_000, 2_000);
    assert!(snapshot.portal_surfaces.is_empty());

    let json = snapshot.to_json().unwrap();
    assert!(
        !json.contains("portal_surfaces"),
        "an empty portal_surfaces map must be omitted from canonical JSON so the \
         checksum matches a pre-field snapshot; got: {json}"
    );

    // Deserializing that surface-less JSON (as an older client/tool would) still
    // reproduces a valid checksum — no spurious `\"portal_surfaces\":{}` sneaks in.
    let restored = SceneGraphSnapshot::from_json(&json).unwrap();
    assert!(restored.portal_surfaces.is_empty());
    assert!(restored.verify_checksum());
}

#[test]
fn test_lease_expiry() {
    let (mut scene, clock) = scene_with_test_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();

    // Grant a lease with a 500 ms TTL.
    // Clock is at t=1000; lease expires at t=1500.
    let lease_id = scene.grant_lease("test", 500);
    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    assert_eq!(scene.tile_count(), 1);
    assert_eq!(
        scene.next_lease_deadline_ms(SceneGraph::DEFAULT_MAX_SUSPENSION_MS),
        Some(1_500),
        "the idle scheduler must expose the lease TTL boundary"
    );

    // Before expiry: clock still at t=1000, lease lives.
    let expired = scene.expire_leases();
    assert_eq!(expired.len(), 0);
    assert_eq!(scene.tile_count(), 1);

    // Advance past the TTL.
    clock.advance(501);
    let expired = scene.expire_leases();
    assert_eq!(expired.len(), 1);
    assert_eq!(scene.tile_count(), 0);
}

#[test]
fn test_tab_created_at_uses_clock() {
    let (mut scene, clock) = scene_with_test_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    assert_eq!(scene.tabs[&tab_id].created_at_ms, 1_000);

    // Advancing the clock does NOT retroactively change existing timestamps.
    clock.advance(100);
    assert_eq!(scene.tabs[&tab_id].created_at_ms, 1_000);
}

#[test]
fn test_renew_lease_uses_clock() {
    let (mut scene, clock) = scene_with_test_clock();
    // Clock at t=1000.
    let lease_id = scene.grant_lease("test", 5_000);
    assert_eq!(scene.leases[&lease_id].granted_at_ms, 1_000);

    // Advance clock then renew.
    clock.advance(2_000);
    scene.renew_lease(lease_id, 10_000).unwrap();
    assert_eq!(scene.leases[&lease_id].granted_at_ms, 3_000);
    assert_eq!(scene.leases[&lease_id].ttl_ms, 10_000);
}

#[test]
fn test_lease_revocation_cleans_tiles() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);

    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();
    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(200.0, 0.0, 100.0, 100.0),
            2,
        )
        .unwrap();

    assert_eq!(scene.tile_count(), 2);
    scene.revoke_lease(lease_id).unwrap();
    assert_eq!(scene.tile_count(), 0);
    // Revoked leases remain in the map with terminal state
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Revoked);
}

#[test]
fn test_visible_tiles_sorted_by_z_order() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);

    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            5,
        )
        .unwrap();
    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();
    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            3,
        )
        .unwrap();

    let visible = scene.visible_tiles();
    assert_eq!(visible.len(), 3);
    assert_eq!(visible[0].z_order, 1);
    assert_eq!(visible[1].z_order, 3);
    assert_eq!(visible[2].z_order, 5);
}

// ─── Zone tests ───────────────────────────────────────────────────────

pub(super) fn make_subtitle_zone() -> ZoneDefinition {
    ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_string(),
        description: "Subtitle overlay".to_string(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 48.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 2,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    }
}

fn make_notification_zone() -> ZoneDefinition {
    ZoneDefinition {
        id: SceneId::new(),
        name: "notifications".to_string(),
        description: "Notification stack".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 3 },
        max_publishers: 4,
        auto_clear_ms: Some(5_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    }
}

fn make_status_bar_zone() -> ZoneDefinition {
    ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_string(),
        description: "Status bar".to_string(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.04,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 8 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    }
}

fn dummy_token() -> ZonePublishToken {
    ZonePublishToken {
        token: vec![0xDE, 0xAD, 0xBE, 0xEF],
    }
}

#[test]
fn test_zone_register_unregister() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let zone = make_subtitle_zone();

    scene.register_zone(zone.clone());
    assert!(scene.zone_registry.get_by_name("subtitle").is_some());

    let removed = scene.unregister_zone("subtitle");
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().name, "subtitle");
    assert!(scene.zone_registry.get_by_name("subtitle").is_none());
}

#[test]
fn test_zone_query_by_name() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone());
    scene.register_zone(make_notification_zone());

    let zone = scene.zone_registry.get_by_name("subtitle").unwrap();
    assert_eq!(zone.name, "subtitle");
    assert!(scene.zone_registry.get_by_name("nonexistent").is_none());
}

#[test]
fn test_zone_query_by_media_type() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone());
    scene.register_zone(make_notification_zone());

    let stream_text_zones = scene
        .zone_registry
        .zones_accepting(ZoneMediaType::StreamText);
    assert_eq!(stream_text_zones.len(), 1);
    assert_eq!(stream_text_zones[0].name, "subtitle");

    let notif_zones = scene
        .zone_registry
        .zones_accepting(ZoneMediaType::ShortTextWithIcon);
    assert_eq!(notif_zones.len(), 1);
    assert_eq!(notif_zones[0].name, "notifications");
}

#[test]
fn test_zone_publish_media_type_mismatch() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone()); // accepts StreamText only

    let result = scene.publish_to_zone(
        "subtitle",
        ZoneContent::Notification(NotificationPayload {
            text: "Hello".to_string(),
            icon: "".to_string(),
            urgency: 1,
            ttl_ms: None,
            title: String::new(),
            actions: Vec::new(),
        }),
        "agent",
        None,
        None,
        None,
    );
    assert!(matches!(
        result,
        Err(ValidationError::ZoneMediaTypeMismatch { .. })
    ));
}

#[test]
fn test_contention_stack() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_notification_zone()); // Stack { max_depth: 3 }

    let notification = |text: &str| {
        ZoneContent::Notification(NotificationPayload {
            text: text.to_string(),
            icon: "".to_string(),
            urgency: 1,
            ttl_ms: None,
            title: String::new(),
            actions: Vec::new(),
        })
    };

    scene
        .publish_to_zone(
            "notifications",
            notification("msg1"),
            "a1",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notifications",
            notification("msg2"),
            "a2",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notifications",
            notification("msg3"),
            "a3",
            None,
            None,
            None,
        )
        .unwrap();

    let publishes = scene.zone_registry.active_for_zone("notifications");
    assert_eq!(publishes.len(), 3);

    // 4th publish should trim the oldest
    scene
        .publish_to_zone(
            "notifications",
            notification("msg4"),
            "a4",
            None,
            None,
            None,
        )
        .unwrap();
    let publishes = scene.zone_registry.active_for_zone("notifications");
    assert_eq!(publishes.len(), 3);
    // Oldest (msg1) should be gone, newest (msg4) at end
    if let ZoneContent::Notification(n) = &publishes[0].content {
        assert_eq!(n.text, "msg2");
    } else {
        panic!("expected Notification");
    }
    if let ZoneContent::Notification(n) = &publishes[2].content {
        assert_eq!(n.text, "msg4");
    } else {
        panic!("expected Notification");
    }
}

// ─── Alert-Banner Auto-Dismiss Tests ────────────────────────────────

/// Helper: build a zone definition that accepts ShortTextWithIcon
/// (Notification content) with Stack contention policy.
fn make_alert_banner_zone() -> ZoneDefinition {
    ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_string(),
        description: "Alert banner zone".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 0.1,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    }
}

fn publish_notification(scene: &mut SceneGraph, urgency: u32, expires_at: Option<u64>) {
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: format!("urgency-{urgency}"),
                icon: "".to_string(),
                urgency,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test-agent",
            None,
            expires_at,
            None,
        )
        .unwrap();
}

/// urgency 0 (low) → expires_at = now + 8 s
#[test]
fn test_notification_auto_dismiss_urgency_info_low() {
    let (mut scene, clock) = scene_with_test_clock();
    scene.register_zone(make_alert_banner_zone());

    publish_notification(&mut scene, 0, None);

    let record = &scene.zone_registry.active_for_zone("alert-banner")[0];
    let expected = clock.now_us() + SceneGraph::NOTIFICATION_TTL_INFO_US;
    assert_eq!(
        record.expires_at_wall_us,
        Some(expected),
        "urgency 0 (low) should auto-dismiss after 8 s"
    );
}

/// urgency 1 (normal) → expires_at = now + 8 s
#[test]
fn test_notification_auto_dismiss_urgency_info_normal() {
    let (mut scene, clock) = scene_with_test_clock();
    scene.register_zone(make_alert_banner_zone());

    publish_notification(&mut scene, 1, None);

    let record = &scene.zone_registry.active_for_zone("alert-banner")[0];
    let expected = clock.now_us() + SceneGraph::NOTIFICATION_TTL_INFO_US;
    assert_eq!(
        record.expires_at_wall_us,
        Some(expected),
        "urgency 1 (normal) should auto-dismiss after 8 s"
    );
}

/// urgency 2 (urgent) → expires_at = now + 15 s
#[test]
fn test_notification_auto_dismiss_urgency_warning() {
    let (mut scene, clock) = scene_with_test_clock();
    scene.register_zone(make_alert_banner_zone());

    publish_notification(&mut scene, 2, None);

    let record = &scene.zone_registry.active_for_zone("alert-banner")[0];
    let expected = clock.now_us() + SceneGraph::NOTIFICATION_TTL_WARNING_US;
    assert_eq!(
        record.expires_at_wall_us,
        Some(expected),
        "urgency 2 (urgent) should auto-dismiss after 15 s"
    );
}

/// urgency 3 (critical) → expires_at = now + 30 s
#[test]
fn test_notification_auto_dismiss_urgency_critical() {
    let (mut scene, clock) = scene_with_test_clock();
    scene.register_zone(make_alert_banner_zone());

    publish_notification(&mut scene, 3, None);

    let record = &scene.zone_registry.active_for_zone("alert-banner")[0];
    let expected = clock.now_us() + SceneGraph::NOTIFICATION_TTL_CRITICAL_US;
    assert_eq!(
        record.expires_at_wall_us,
        Some(expected),
        "urgency 3 (critical) should auto-dismiss after 30 s"
    );
}

/// Publisher-supplied expires_at takes precedence over the urgency default.
#[test]
fn test_notification_auto_dismiss_publisher_override() {
    let (mut scene, clock) = scene_with_test_clock();
    scene.register_zone(make_alert_banner_zone());

    // Use a custom expiry that differs from both the default and the clock.
    let publisher_expires_at = clock.now_us() + 60_000_000u64; // 60 s
    publish_notification(&mut scene, 1, Some(publisher_expires_at));

    let record = &scene.zone_registry.active_for_zone("alert-banner")[0];
    assert_eq!(
        record.expires_at_wall_us,
        Some(publisher_expires_at),
        "publisher-supplied expires_at must take precedence over urgency default"
    );
    assert_eq!(
        scene.next_publication_expiry_wall_us(),
        Some(publisher_expires_at),
        "the idle scheduler must expose the same publication boundary"
    );
}

/// Non-Notification content (StreamText) must NOT have expires_at auto-set.
#[test]
fn test_non_notification_content_no_auto_dismiss() {
    let (mut scene, _clock) = scene_with_test_clock();
    scene.register_zone(make_subtitle_zone()); // subtitle zone accepts StreamText

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("hello".to_string()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    let record = &scene.zone_registry.active_for_zone("subtitle")[0];
    assert_eq!(
        record.expires_at_wall_us, None,
        "non-Notification content must not have auto-dismiss expires_at"
    );
}

/// End-to-end: advance clock past expiry and verify drain removes the publication.
#[test]
fn test_notification_auto_dismiss_drain_removes_after_expiry() {
    let (mut scene, clock) = scene_with_test_clock();
    scene.register_zone(make_alert_banner_zone());

    // Publish a low-urgency notification (auto-dismiss after 8 s).
    publish_notification(&mut scene, 0, None);
    assert_eq!(
        scene.zone_registry.active_for_zone("alert-banner").len(),
        1,
        "notification must be present before expiry"
    );

    // Advance clock to just before the TTL boundary — must still be visible.
    clock.advance(SceneGraph::NOTIFICATION_TTL_INFO_US / 1_000 - 1); // advance in ms
    let drained = scene.drain_expired_zone_publications();
    assert_eq!(drained, 0, "must not expire before TTL elapses");
    assert_eq!(scene.zone_registry.active_for_zone("alert-banner").len(), 1,);

    // Advance past the TTL boundary — must be removed.
    clock.advance(2); // total elapsed > 8 s
    let drained = scene.drain_expired_zone_publications();
    assert_eq!(drained, 1, "expired notification must be drained");
    assert_eq!(
        scene.zone_registry.active_for_zone("alert-banner").len(),
        0,
        "zone must be empty after auto-dismiss drain"
    );
}

#[test]
fn test_contention_merge_by_key() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_status_bar_zone()); // MergeByKey { max_keys: 8 }

    let kv = |k: &str, v: &str| {
        let mut entries = std::collections::HashMap::new();
        entries.insert(k.to_string(), v.to_string());
        ZoneContent::StatusBar(StatusBarPayload { entries })
    };

    // Publish with different keys
    scene
        .publish_to_zone(
            "status-bar",
            kv("clock", "12:00"),
            "a1",
            Some("clock".to_string()),
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "status-bar",
            kv("battery", "80%"),
            "a2",
            Some("battery".to_string()),
            None,
            None,
        )
        .unwrap();

    let publishes = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(publishes.len(), 2);

    // Update existing key "clock"
    scene
        .publish_to_zone(
            "status-bar",
            kv("clock", "12:01"),
            "a1",
            Some("clock".to_string()),
            None,
            None,
        )
        .unwrap();
    let publishes = scene.zone_registry.active_for_zone("status-bar");
    assert_eq!(publishes.len(), 2); // Still 2 (clock replaced, battery retained)
    let clock = publishes
        .iter()
        .find(|r| r.merge_key.as_deref() == Some("clock"))
        .unwrap();
    if let ZoneContent::StatusBar(sb) = &clock.content {
        assert_eq!(sb.entries["clock"], "12:01");
    } else {
        panic!("expected StatusBar");
    }
}

#[test]
fn test_contention_replace() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let zone = ZoneDefinition {
        id: SceneId::new(),
        name: "pip".to_string(),
        description: "Picture in picture".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.80,
            y_pct: 0.80,
            width_pct: 0.18,
            height_pct: 0.18,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Replace,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    };
    scene.register_zone(zone);

    scene
        .publish_to_zone(
            "pip",
            ZoneContent::SolidColor(Rgba::WHITE),
            "a1",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "pip",
            ZoneContent::SolidColor(Rgba::BLACK),
            "a2",
            None,
            None,
            None,
        )
        .unwrap();

    let publishes = scene.zone_registry.active_for_zone("pip");
    assert_eq!(publishes.len(), 1);
    assert_eq!(publishes[0].publisher_namespace, "a2");
}

// ─── Contention policy: apply_contention extraction tests ────────────────
// These tests were added alongside the extraction of apply_contention (the
// shared helper used by all three zone/widget publish entry points).  They
// specifically cover the behaviors that were either untested or diverged in
// the pre-extraction widget copy.
//
// Issue: hud-r5q6p
//   - max_publishers rejection was untested on zones, absent on widgets.
//   - max_depth == 0 was treated as "unbounded" on widgets but "reject all"
//     (trim-to-zero) on zones — the zone behavior is canonical.
//   - All three entry points (publish_to_zone, publish_to_zone_with_breakpoints,
//     publish_to_widget) now share the single apply_contention function.

/// Zone Stack: WHEN a publisher exceeds max_publishers THEN ZoneMaxPublishersReached.
///
/// max_publishers is per-namespace: each agent gets its own per-namespace
/// slot count.  This test uses a zone with max_publishers=1 and two publishes
/// from the same namespace to trigger the limit.
#[test]
fn test_contention_zone_max_publishers_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "single-pub".to_string(),
        description: "Stack zone with max_publishers=1".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 10 },
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    let notification = |text: &str| {
        ZoneContent::Notification(NotificationPayload {
            text: text.to_string(),
            icon: String::new(),
            urgency: 1,
            ttl_ms: None,
            title: String::new(),
            actions: Vec::new(),
        })
    };

    // First publish from "agent.a" succeeds.
    scene
        .publish_to_zone(
            "single-pub",
            notification("first"),
            "agent.a",
            None,
            None,
            None,
        )
        .expect("first publish from agent.a should succeed");

    // Second publish from the same namespace must be rejected.
    let err = scene
        .publish_to_zone(
            "single-pub",
            notification("second"),
            "agent.a",
            None,
            None,
            None,
        )
        .expect_err("second publish from same namespace must be rejected");

    assert!(
        matches!(
            err,
            ValidationError::ZoneMaxPublishersReached { max: 1, .. }
        ),
        "expected ZoneMaxPublishersReached(max=1), got: {err:?}"
    );

    // A different namespace is unaffected — it has its own slot count.
    scene
        .publish_to_zone(
            "single-pub",
            notification("from-b"),
            "agent.b",
            None,
            None,
            None,
        )
        .expect("publish from a different namespace must succeed");
}

/// Zone Stack: WHEN max_depth == 0 THEN every publish is trimmed to zero
/// (canonical behavior — mirrors widget path after apply_contention fix).
#[test]
fn test_contention_zone_stack_max_depth_zero_discards_all() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "depth-zero".to_string(),
        description: "Stack zone with max_depth=0".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 0 },
        max_publishers: 100,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    let notification = |text: &str| {
        ZoneContent::Notification(NotificationPayload {
            text: text.to_string(),
            icon: String::new(),
            urgency: 1,
            ttl_ms: None,
            title: String::new(),
            actions: Vec::new(),
        })
    };

    for i in 0..3 {
        scene
            .publish_to_zone(
                "depth-zero",
                notification(&format!("msg{i}")),
                &format!("agent.{i}"),
                None,
                None,
                None,
            )
            .unwrap();
    }

    let active = scene.zone_registry.active_for_zone("depth-zero");
    assert_eq!(
        active.len(),
        0,
        "Stack(max_depth=0) must trim to 0 — all publishes discarded"
    );
}

/// Widget Stack: WHEN a publisher exceeds max_publishers THEN WidgetMaxPublishersReached.
#[test]
fn test_contention_widget_max_publishers_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();

    scene.widget_registry.register_definition(WidgetDefinition {
        id: "counter".to_string(),
        name: "counter".to_string(),
        description: "test counter widget".to_string(),
        parameter_schema: vec![WidgetParameterDeclaration {
            name: "value".to_string(),
            param_type: WidgetParamType::F32,
            default_value: WidgetParameterValue::F32(0.0),
            constraints: None,
        }],
        layers: vec![],
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.1,
            height_pct: 0.1,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::Stack { max_depth: 10 },
        max_publishers: 1,
        ephemeral: false,
        hover_behavior: None,
    });
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "counter".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "counter".to_string(),
        current_params: std::collections::HashMap::from([(
            "value".to_string(),
            WidgetParameterValue::F32(0.0),
        )]),
    });

    let params =
        || std::collections::HashMap::from([("value".to_string(), WidgetParameterValue::F32(0.5))]);

    // First publish from "agent.a" succeeds.
    scene
        .publish_to_widget("counter", params(), "agent.a", None, 0, None)
        .expect("first publish from agent.a should succeed");

    // Second publish from the same namespace must be rejected.
    let err = scene
        .publish_to_widget("counter", params(), "agent.a", None, 0, None)
        .expect_err("second publish from same namespace must be rejected");

    assert!(
        matches!(
            err,
            ValidationError::WidgetMaxPublishersReached { max: 1, .. }
        ),
        "expected WidgetMaxPublishersReached(max=1), got: {err:?}"
    );

    // A different namespace is unaffected.
    scene
        .publish_to_widget("counter", params(), "agent.b", None, 0, None)
        .expect("publish from a different namespace must succeed");
}

/// Cross-entry-point: publish_to_zone and publish_to_zone_with_breakpoints
/// must produce identical record counts and apply identical contention logic.
///
/// This test verifies that both paths share the same apply_contention function.
#[test]
fn test_contention_zone_vs_breakpoints_entry_point_consistency() {
    // Zone via publish_to_zone.
    let mut scene_a = SceneGraph::new(1920.0, 1080.0);
    scene_a.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "sub".to_string(),
        description: String::new(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 0.1,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 2 },
        max_publishers: 2,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Zone via publish_to_zone_with_breakpoints.
    let mut scene_b = SceneGraph::new(1920.0, 1080.0);
    scene_b.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "sub".to_string(),
        description: String::new(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 0.1,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 2 },
        max_publishers: 2,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    for (ns, text) in [
        ("agent.a", "hello"),
        ("agent.b", "world"),
        ("agent.a", "overflow"),
    ] {
        let _ = scene_a.publish_to_zone(
            "sub",
            ZoneContent::StreamText(text.to_string()),
            ns,
            None,
            None,
            None,
        );
        let _ = scene_b.publish_to_zone_with_breakpoints(
            "sub",
            ZoneContent::StreamText(text.to_string()),
            ns,
            None,
            None,
            None,
            Vec::new(),
        );
    }

    let count_a = scene_a.zone_registry.active_for_zone("sub").len();
    let count_b = scene_b.zone_registry.active_for_zone("sub").len();
    assert_eq!(
        count_a, count_b,
        "publish_to_zone and publish_to_zone_with_breakpoints must produce identical record counts; got {count_a} vs {count_b}"
    );

    // Both should be 2: agent.a's first publish is at the limit for that namespace
    // (max_publishers=2 across all namespaces but only 1 per ns is counted before
    // the limit kicks in at max_publishers-per-namespace=2).
    // Actually max_publishers is per-namespace: agent.a published "hello" and
    // tried "overflow" as 2nd — 2nd is allowed since max_publishers=2.
    // agent.b published "world" as 1st = allowed.
    // Total stack is trimmed to max_depth=2 from back.
    assert_eq!(
        count_a, 2,
        "Stack(max_depth=2, max_publishers=2) should hold exactly 2 records after 3 publishes"
    );
}

#[test]
fn test_clear_zone() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone());

    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("hello".to_string()),
            "a1",
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 1);

    scene.clear_zone("subtitle").unwrap();
    assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 0);
}

#[test]
fn test_clear_zone_not_found() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let result = scene.clear_zone("nonexistent");
    assert!(matches!(result, Err(ValidationError::ZoneNotFound { .. })));
}

#[test]
fn test_zone_registry_snapshot() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone());
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("hi".to_string()),
            "a1",
            None,
            None,
            None,
        )
        .unwrap();

    let snap = scene.zone_registry.snapshot();
    assert_eq!(snap.zones.len(), 1);
    assert_eq!(snap.active_publishes.len(), 1);
    assert_eq!(snap.active_publishes[0].zone_name, "subtitle");
}

#[test]
fn test_zone_publish_via_mutation_batch() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone());

    use crate::mutation::{MutationBatch, SceneMutation};

    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent".to_string(),
        mutations: vec![SceneMutation::PublishToZone {
            zone_name: "subtitle".to_string(),
            content: ZoneContent::StreamText("batch publish".to_string()),
            publish_token: dummy_token(),
            merge_key: None,
            expires_at_wall_us: None,
            content_classification: None,
            breakpoints: Vec::new(),
            held: false,
        }],
        timing_hints: None,
        lease_id: None,
    };

    let result = scene.apply_batch(&batch);
    assert!(result.applied, "batch should be applied");
    let publishes = scene.zone_registry.active_for_zone("subtitle");
    assert_eq!(publishes.len(), 1);
    assert_eq!(
        publishes[0].content,
        ZoneContent::StreamText("batch publish".to_string())
    );
}

/// A notification applied through a batch with no explicit expiry takes the
/// urgency default (8 s for urgency 1); `held` leaves it until cleared.
#[test]
fn test_batch_notification_expiry_follows_held_flag() {
    use crate::mutation::{MutationBatch, SceneMutation};
    for (held, expected_ttl) in [
        (false, Some(SceneGraph::NOTIFICATION_TTL_INFO_US)),
        (true, None),
    ] {
        let (mut scene, clock) = scene_with_test_clock();
        scene.register_zone(make_alert_banner_zone());
        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: "agent".to_string(),
            mutations: vec![SceneMutation::PublishToZone {
                zone_name: "alert-banner".to_string(),
                content: ZoneContent::Notification(NotificationPayload {
                    text: "n".to_string(),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                publish_token: dummy_token(),
                merge_key: None,
                expires_at_wall_us: None,
                content_classification: None,
                breakpoints: Vec::new(),
                held,
            }],
            timing_hints: None,
            lease_id: None,
        };
        assert!(scene.apply_batch(&batch).applied);
        let record = &scene.zone_registry.active_for_zone("alert-banner")[0];
        assert_eq!(
            record.expires_at_wall_us,
            expected_ttl.map(|t| clock.now_us() + t),
            "held={held}"
        );
    }
}

/// A batch `ClearZone` (the replace pattern's first half) clears active
/// publications only; the agent's later scheduled publish still presents.
#[test]
fn batch_clear_zone_does_not_cancel_later_pending_publish() {
    use crate::mutation::{MutationBatch, SceneMutation};
    let (mut scene, clock) = scene_with_test_clock();
    scene.register_zone(make_subtitle_zone());
    let batch = |mutation| MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent".to_string(),
        mutations: vec![mutation],
        timing_hints: None,
        lease_id: None,
    };
    let now = scene.now_wall_us();
    scene.schedule_batch(
        now + 1_000,
        batch(SceneMutation::ClearZone {
            zone_name: "subtitle".to_string(),
            publish_token: dummy_token(),
        }),
    );
    scene.schedule_batch(
        now + 5_000_000,
        batch(SceneMutation::PublishToZone {
            zone_name: "subtitle".to_string(),
            content: ZoneContent::StreamText("B".to_string()),
            publish_token: dummy_token(),
            merge_key: None,
            expires_at_wall_us: None,
            content_classification: None,
            breakpoints: Vec::new(),
            held: true,
        }),
    );
    clock.advance(1);
    assert_eq!(scene.apply_due_batches().len(), 1);
    assert_eq!(scene.scheduled_batches.len(), 1, "B stays scheduled");
}

#[test]
fn test_clear_zone_via_mutation_batch() {
    // Per spec: ClearZone clears publications by THIS agent (batch.agent_namespace).
    // Publish as "agent", then clear as "agent" — should clear.
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.register_zone(make_subtitle_zone());
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("hello".to_string()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 1);

    use crate::mutation::{MutationBatch, SceneMutation};

    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent".to_string(),
        mutations: vec![SceneMutation::ClearZone {
            zone_name: "subtitle".to_string(),
            publish_token: dummy_token(),
        }],
        timing_hints: None,
        lease_id: None,
    };

    let result = scene.apply_batch(&batch);
    assert!(result.applied);
    // "agent" published, "agent" cleared — should be 0
    assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 0);
}

#[test]
fn test_clear_zone_per_publisher_only_affects_own_publishes() {
    // Publish as two agents; ClearZone from agent "a1" should only remove "a1"'s publish.
    // subtitle zone has max_publishers=2 for this test; use a zone that supports 2 publishers.
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    // Use a Stack zone so both publishes can coexist
    let stack_zone = ZoneDefinition {
        id: SceneId::new(),
        name: "shared".to_string(),
        description: "Stack zone for publisher isolation test".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 0.1,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 4 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    };
    scene.register_zone(stack_zone);

    scene
        .publish_to_zone(
            "shared",
            ZoneContent::StreamText("from a1".to_string()),
            "a1",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "shared",
            ZoneContent::StreamText("from a2".to_string()),
            "a2",
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(scene.zone_registry.active_for_zone("shared").len(), 2);

    // Clear only "a1"'s publication
    scene.clear_zone_for_publisher("shared", "a1").unwrap();
    let pubs = scene.zone_registry.active_for_zone("shared");
    assert_eq!(pubs.len(), 1);
    assert_eq!(pubs[0].publisher_namespace, "a2");
}

// ─── StaticImageNode tests ────────────────────────────────────────────

/// Build a test `ResourceId` and decoded size for a w×h RGBA8 image.
///
/// Per RS-4 ephemerality contract, `StaticImageNode` carries only the
/// content-addressed `ResourceId` and the decoded byte count for budget
/// accounting — no raw pixel data is embedded in the scene graph.
fn make_test_image_resource(w: u32, h: u32) -> (ResourceId, u64) {
    // Compute a deterministic ResourceId from the dimensions (as a stand-in
    // for "the BLAKE3 hash of the actual pixel bytes").  In production this
    // would be the ResourceId returned by the resource store after upload.
    let fake_bytes: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 0, 0, 255]).collect();
    let resource_id = ResourceId::of(&fake_bytes);
    let decoded_bytes = u64::from(w * h * 4);
    (resource_id, decoded_bytes)
}

#[test]
fn test_static_image_node_creation() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
            1,
        )
        .unwrap();

    let (resource_id, decoded_bytes) = make_test_image_resource(64, 48);
    scene.register_resource(resource_id);
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 64,
            height: 48,
            decoded_bytes,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
        }),
    };

    scene.set_tile_root(tile_id, node.clone()).unwrap();
    assert_eq!(scene.node_count(), 1);

    let stored = scene.nodes.get(&node.id).unwrap();
    if let NodeData::StaticImage(si) = &stored.data {
        assert_eq!(si.resource_id, resource_id);
        assert_eq!(si.width, 64);
        assert_eq!(si.height, 48);
        assert_eq!(si.decoded_bytes, 64u64 * 48 * 4);
        assert_eq!(si.fit_mode, ImageFitMode::Contain);
    } else {
        panic!("expected StaticImage node data");
    }
}

#[test]
fn test_static_image_node_all_fit_modes() {
    // Verify all ImageFitMode variants are constructable and round-trip through JSON.
    let (resource_id, decoded_bytes) = make_test_image_resource(4, 4);
    for fit_mode in [
        ImageFitMode::Contain,
        ImageFitMode::Cover,
        ImageFitMode::Fill,
        ImageFitMode::ScaleDown,
    ] {
        let node_data = NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 4,
            height: 4,
            decoded_bytes,
            fit_mode,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
        });
        let json = serde_json::to_string(&node_data).unwrap();
        // Acceptance (RS-4): snapshot must NOT contain raw blob data.
        assert!(
            !json.contains("image_data"),
            "snapshot JSON must not contain image_data blob"
        );
        let restored: NodeData = serde_json::from_str(&json).unwrap();
        if let NodeData::StaticImage(si) = restored {
            assert_eq!(si.fit_mode, fit_mode);
            assert_eq!(si.resource_id, resource_id);
        } else {
            panic!("wrong variant after JSON roundtrip");
        }
    }
}

#[test]
fn test_static_image_node_snapshot_roundtrip() {
    let mut scene = SceneGraph::new(1280.0, 720.0);
    let tab_id = scene.create_tab("Tab", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(10.0, 10.0, 200.0, 150.0),
            1,
        )
        .unwrap();

    let (resource_id, decoded_bytes) = make_test_image_resource(16, 16);
    scene.register_resource(resource_id);
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 16,
            height: 16,
            decoded_bytes,
            fit_mode: ImageFitMode::Cover,
            bounds: Rect::new(0.0, 0.0, 200.0, 150.0),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();

    let json = scene.snapshot_json().unwrap();

    // Acceptance (RS-4): scene snapshot includes ResourceId references but NOT blob data.
    // The JSON must not contain raw pixel data.
    assert!(
        !json.contains("image_data"),
        "snapshot JSON must not embed raw image blob data (RS-4 ephemerality contract)"
    );

    let restored = SceneGraph::from_json(&json).unwrap();

    assert_eq!(scene.node_count(), restored.node_count());
    // Verify the node data survived the roundtrip.
    for n in restored.nodes.values() {
        if let NodeData::StaticImage(si) = &n.data {
            assert_eq!(
                si.resource_id, resource_id,
                "resource_id must survive snapshot roundtrip"
            );
            assert_eq!(si.fit_mode, ImageFitMode::Cover);
            assert_eq!(si.width, 16);
            assert_eq!(si.height, 16);
            assert_eq!(si.decoded_bytes, decoded_bytes);
        }
    }
}

#[test]
fn test_static_image_node_replace_with_set_tile_root() {
    // Verify that replacing a StaticImageNode via set_tile_root removes the old node.
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

    let (resource_id, decoded_bytes) = make_test_image_resource(8, 8);
    scene.register_resource(resource_id);
    let node1 = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 8,
            height: 8,
            decoded_bytes,
            fit_mode: ImageFitMode::Fill,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
        }),
    };
    let node1_id = node1.id;
    scene.set_tile_root(tile_id, node1).unwrap();
    assert_eq!(scene.node_count(), 1);
    assert!(scene.nodes.contains_key(&node1_id));

    // Replace with a SolidColor node.
    let node2 = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            radius: None,
        }),
    };
    scene.set_tile_root(tile_id, node2).unwrap();
    // Old image node should be gone.
    assert!(!scene.nodes.contains_key(&node1_id));
    assert_eq!(scene.node_count(), 1);
}

// ─── UpdateNodeContent + StaticImage decoded_bytes tests ────────────

/// Helper: build a scene with a lease, a tile, and a StaticImage root node.
/// Returns (scene, lease_id, tile_id, node_id, original_decoded_bytes).
fn scene_with_static_image_node(w: u32, h: u32) -> (SceneGraph, SceneId, SceneId, SceneId, u64) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
            1,
        )
        .unwrap();
    let (resource_id, decoded_bytes) = make_test_image_resource(w, h);
    // Register the resource so that subsequent checked mutations (which
    // enforce resource-upload-before-use) can reference it.
    scene.register_resource(resource_id);
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: w,
            height: h,
            decoded_bytes,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
        }),
    };
    let node_id = node.id;
    scene.set_tile_root(tile_id, node).unwrap();
    (scene, lease_id, tile_id, node_id, decoded_bytes)
}

#[test]
fn test_update_static_image_same_resource_preserves_decoded_bytes() {
    // WHEN UpdateNodeContent is applied with the same resource_id and decoded_bytes=0
    // (as proto ingest always produces), the stored decoded_bytes must be preserved.
    let (mut scene, lease_id, tile_id, node_id, original_decoded_bytes) =
        scene_with_static_image_node(64, 48);
    assert_eq!(original_decoded_bytes, 64 * 48 * 4);

    let (resource_id, _) = make_test_image_resource(64, 48);

    // Simulate proto-ingest: decoded_bytes is zeroed out.
    let result = scene.update_node_content_checked(
        tile_id,
        node_id,
        NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 64,
            height: 48,
            decoded_bytes: 0,              // proto ingest always zeros this
            fit_mode: ImageFitMode::Cover, // changed fit mode
            bounds: Rect::new(10.0, 10.0, 380.0, 280.0),
        }),
        "agent",
    );
    assert!(result.is_ok(), "update should succeed: {result:?}");

    // decoded_bytes must be restored from the stored node — not zero.
    let stored = &scene.nodes[&node_id];
    match &stored.data {
        NodeData::StaticImage(si) => {
            assert_eq!(
                si.decoded_bytes, original_decoded_bytes,
                "decoded_bytes must be preserved when resource_id is unchanged"
            );
            // Other fields must reflect the update.
            assert_eq!(si.fit_mode, ImageFitMode::Cover);
        }
        _ => panic!("expected StaticImage node"),
    }

    // Texture budget accounting must also reflect the correct bytes.
    let usage = scene.lease_resource_usage(&lease_id);
    assert_eq!(
        usage.texture_bytes, original_decoded_bytes,
        "lease texture_bytes must still account for the full image size"
    );
}

#[test]
fn test_update_static_image_new_resource_uses_caller_decoded_bytes() {
    // WHEN UpdateNodeContent replaces a StaticImage with a different resource_id
    // AND the caller supplies non-zero decoded_bytes (as the session server should),
    // the new decoded_bytes must be used — not the old value.
    let (mut scene, lease_id, tile_id, node_id, original_decoded_bytes) =
        scene_with_static_image_node(64, 48);

    let (new_resource_id, new_decoded_bytes) = make_test_image_resource(128, 96);
    assert_ne!(
        new_resource_id,
        make_test_image_resource(64, 48).0,
        "resources must differ for this test to be meaningful"
    );
    // Register the new resource before the checked update (mirrors real-world
    // flow where the session server uploads the resource before referencing it).
    scene.register_resource(new_resource_id);

    let result = scene.update_node_content_checked(
        tile_id,
        node_id,
        NodeData::StaticImage(StaticImageNode {
            resource_id: new_resource_id,
            width: 128,
            height: 96,
            decoded_bytes: new_decoded_bytes, // caller explicitly provides the new size
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
        }),
        "agent",
    );
    assert!(result.is_ok(), "update should succeed: {result:?}");

    let stored = &scene.nodes[&node_id];
    match &stored.data {
        NodeData::StaticImage(si) => {
            assert_eq!(si.resource_id, new_resource_id);
            assert_eq!(
                si.decoded_bytes, new_decoded_bytes,
                "decoded_bytes must reflect the new resource size"
            );
            assert_ne!(
                si.decoded_bytes, original_decoded_bytes,
                "old decoded_bytes must not be carried forward to a new resource"
            );
        }
        _ => panic!("expected StaticImage node"),
    }

    let usage = scene.lease_resource_usage(&lease_id);
    assert_eq!(
        usage.texture_bytes, new_decoded_bytes,
        "lease texture_bytes must account for the new image size"
    );
}

#[test]
fn test_update_static_image_decoded_bytes_zero_after_resource_change_is_zero() {
    // WHEN UpdateNodeContent replaces a StaticImage with a different resource_id
    // AND decoded_bytes is 0 (caller bug / missing resource-store lookup),
    // the graph stores 0 (does NOT inherit the old resource's bytes).
    // This is the correct conservative behaviour: it's better to under-report
    // (visible as a budget accounting gap) than to silently charge the wrong amount.
    let (mut scene, _lease_id, tile_id, node_id, _) = scene_with_static_image_node(64, 48);

    let (new_resource_id, _) = make_test_image_resource(128, 96);
    // Register the new resource — even though decoded_bytes is 0 (simulating a
    // caller bug), the resource itself must be registered for the checked path to
    // accept the update.
    scene.register_resource(new_resource_id);

    let result = scene.update_node_content_checked(
        tile_id,
        node_id,
        NodeData::StaticImage(StaticImageNode {
            resource_id: new_resource_id,
            width: 128,
            height: 96,
            decoded_bytes: 0, // caller failed to populate
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
        }),
        "agent",
    );
    assert!(result.is_ok(), "update should succeed");

    let stored = &scene.nodes[&node_id];
    match &stored.data {
        NodeData::StaticImage(si) => {
            assert_eq!(
                si.decoded_bytes, 0,
                "with a changed resource_id and decoded_bytes=0, graph must store 0"
            );
        }
        _ => panic!("expected StaticImage node"),
    }
}

// ─── Resource ref-count tracking tests (hud-uar4) ────────────────────
//
// Spec: resource-store/spec.md §Requirement: Resource Freed On Last Tile Removal
// When the last tile referencing a resource is removed (via lease expiry,
// explicit DeleteTile, or SetTileRoot replacement), the resource MUST be freed
// from the registry.  If another tile still references the same resource the
// registry entry MUST be preserved.

/// Single tile with a StaticImage resource: removing the tile frees the resource.
#[test]
fn resource_freed_when_only_referencing_tile_is_removed() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
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

    let (resource_id, decoded_bytes) = make_test_image_resource(32, 32);
    scene.register_resource(resource_id);
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::StaticImage(StaticImageNode {
                    resource_id,
                    width: 32,
                    height: 32,
                    decoded_bytes,
                    fit_mode: ImageFitMode::Contain,
                    bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                }),
            },
        )
        .unwrap();

    // Resource must be registered and ref count = 1.
    assert!(
        scene.is_resource_registered(&resource_id),
        "resource must be registered after tile is set"
    );
    assert_eq!(
        scene.resource_ref_count(&resource_id),
        Some(1),
        "ref count must be 1 while one tile references it"
    );

    // Remove the tile (explicit delete).
    scene.delete_tile(tile_id, "agent").unwrap();

    // Resource must be freed.
    assert!(
        !scene.is_resource_registered(&resource_id),
        "resource must be freed when the last referencing tile is removed"
    );
    assert_eq!(
        scene.resource_ref_count(&resource_id),
        None,
        "resource_ref_count must return None after resource is freed"
    );
}

/// Two tiles share the same resource: removing one preserves it; removing both frees it.
#[test]
fn resource_kept_alive_while_second_tile_references_it_then_freed() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 300_000);

    let tile_a = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 200.0),
            1,
        )
        .unwrap();
    let tile_b = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(200.0, 0.0, 200.0, 200.0),
            2,
        )
        .unwrap();

    let (resource_id, decoded_bytes) = make_test_image_resource(16, 16);
    scene.register_resource(resource_id);

    let make_image_node = || Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: 16,
            height: 16,
            decoded_bytes,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
        }),
    };

    scene.set_tile_root(tile_a, make_image_node()).unwrap();
    scene.set_tile_root(tile_b, make_image_node()).unwrap();

    assert_eq!(
        scene.resource_ref_count(&resource_id),
        Some(2),
        "ref count must be 2 when two tiles reference the same resource"
    );

    // Remove first tile — resource must still be alive.
    scene.delete_tile(tile_a, "agent").unwrap();
    assert!(
        scene.is_resource_registered(&resource_id),
        "resource must still be registered while tile_b references it"
    );
    assert_eq!(
        scene.resource_ref_count(&resource_id),
        Some(1),
        "ref count must drop to 1 after first tile is removed"
    );

    // Remove second tile — resource must be freed.
    scene.delete_tile(tile_b, "agent").unwrap();
    assert!(
        !scene.is_resource_registered(&resource_id),
        "resource must be freed after both tiles are removed"
    );
    assert_eq!(
        scene.resource_ref_count(&resource_id),
        None,
        "resource_ref_count must return None after last tile removed"
    );
}

/// Lease expiry path: tiles removed by `expire_leases` also decrement resource refs.
#[test]
fn resource_freed_on_lease_expiry() {
    use crate::clock::TestClock;
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());

    let tab_id = scene.create_tab("Main", 0).unwrap();
    // Grant a short lease (100 ms TTL).
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

    let (resource_id, decoded_bytes) = make_test_image_resource(8, 8);
    scene.register_resource(resource_id);
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::StaticImage(StaticImageNode {
                    resource_id,
                    width: 8,
                    height: 8,
                    decoded_bytes,
                    fit_mode: ImageFitMode::Contain,
                    bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                }),
            },
        )
        .unwrap();

    assert_eq!(scene.resource_ref_count(&resource_id), Some(1));

    // Advance past TTL and trigger lease expiry sweep.
    clock.advance(200);
    let expiries = scene.expire_leases();
    assert_eq!(expiries.len(), 1, "one lease should have expired");
    assert_eq!(expiries[0].removed_tiles.len(), 1, "one tile removed");

    assert!(
        !scene.is_resource_registered(&resource_id),
        "resource must be freed when the lease expires and removes its tile"
    );
}

/// hud-i429x: `expire_lease` reaps ONLY the named lease when its grace has
/// elapsed, leaving a sibling lease and its tiles untouched — the scoped
/// counterpart to the whole-scene `expire_leases` sweep. It returns `None` for a
/// lease that is not yet due, and bumps the scene version on reap.
#[test]
fn expire_lease_scoped_reaps_only_the_named_grace_expired_lease() {
    use crate::clock::TestClock;
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    let tab_id = scene.create_tab("Main", 0).unwrap();

    // Two long-TTL leases, each with a tile, so neither expires on TTL.
    let lease_a = scene.grant_lease("agent-a", 86_400_000);
    let tile_a = scene
        .create_tile(
            tab_id,
            "agent-a",
            lease_a,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();
    let lease_b = scene.grant_lease("agent-b", 86_400_000);
    let tile_b = scene
        .create_tile(
            tab_id,
            "agent-b",
            lease_b,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();
    assert_eq!(scene.tile_count(), 2);

    // Orphan only lease A; advance past its grace.
    scene
        .disconnect_lease(&lease_a, clock.now_millis())
        .unwrap();
    clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1);

    // Lease B is not orphaned and not TTL-due → scoped expiry is a no-op.
    assert!(
        scene.expire_lease(&lease_b).is_none(),
        "expire_lease must not reap a lease that is not yet due"
    );
    assert_eq!(scene.tile_count(), 2, "no-op expiry removes nothing");

    // Lease A is orphaned + grace-expired → scoped expiry reaps exactly it.
    let version_before = scene.version;
    let expiry = scene
        .expire_lease(&lease_a)
        .expect("grace-expired lease A is reaped");
    assert_eq!(expiry.lease_id, lease_a);
    assert!(
        expiry.removed_tiles.contains(&tile_a),
        "A's tile is removed"
    );
    assert!(
        scene.version > version_before,
        "reaping bumps the scene version"
    );
    assert_eq!(scene.tile_count(), 1, "only A's surface is gone");
    assert!(
        scene.tiles.contains_key(&tile_b),
        "sibling lease B's tile survives"
    );
    assert!(
        scene.lease_is_active(&lease_b),
        "sibling lease B stays active"
    );
}

/// SetTileRoot replacement: old resource loses a ref, new resource gains one.
#[test]
fn resource_refs_updated_on_set_tile_root_replacement() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
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

    let (res_a, bytes_a) = make_test_image_resource(4, 4);
    let (res_b, bytes_b) = make_test_image_resource(8, 8);
    scene.register_resource(res_a);
    scene.register_resource(res_b);

    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::StaticImage(StaticImageNode {
                    resource_id: res_a,
                    width: 4,
                    height: 4,
                    decoded_bytes: bytes_a,
                    fit_mode: ImageFitMode::Contain,
                    bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                }),
            },
        )
        .unwrap();
    assert_eq!(scene.resource_ref_count(&res_a), Some(1));
    assert_eq!(
        scene.resource_ref_count(&res_b),
        Some(0),
        "res_b registered but not yet referenced by any node"
    );

    // Replace tile root with a node referencing res_b.
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::StaticImage(StaticImageNode {
                    resource_id: res_b,
                    width: 8,
                    height: 8,
                    decoded_bytes: bytes_b,
                    fit_mode: ImageFitMode::Contain,
                    bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                }),
            },
        )
        .unwrap();

    // res_a must have been freed (ref count 0 → removed).
    assert!(
        !scene.is_resource_registered(&res_a),
        "res_a must be freed after its node is replaced"
    );
    // res_b must now have ref count 1.
    assert_eq!(
        scene.resource_ref_count(&res_b),
        Some(1),
        "res_b must have ref count 1 after becoming the tile root"
    );
}

/// UpdateNodeContent with a different resource_id: ref counts are updated correctly.
#[test]
fn resource_refs_updated_on_update_node_content_resource_swap() {
    let (mut scene, _lease_id, tile_id, node_id, _) = scene_with_static_image_node(32, 32);
    let (old_resource_id, _) = make_test_image_resource(32, 32);

    // old resource should have ref count 1 from the initial set_tile_root.
    assert_eq!(scene.resource_ref_count(&old_resource_id), Some(1));

    let (new_resource_id, new_decoded_bytes) = make_test_image_resource(64, 64);
    scene.register_resource(new_resource_id);

    scene
        .update_node_content_checked(
            tile_id,
            node_id,
            NodeData::StaticImage(StaticImageNode {
                resource_id: new_resource_id,
                width: 64,
                height: 64,
                decoded_bytes: new_decoded_bytes,
                fit_mode: ImageFitMode::Contain,
                bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            }),
            "agent",
        )
        .unwrap();

    // Old resource must be freed.
    assert!(
        !scene.is_resource_registered(&old_resource_id),
        "old resource must be freed after UpdateNodeContent swaps it out"
    );
    // New resource must have ref count 1.
    assert_eq!(
        scene.resource_ref_count(&new_resource_id),
        Some(1),
        "new resource must have ref count 1 after node is updated"
    );
}

#[test]
fn aggregate_budget_delta_tracks_sequential_updates_within_one_batch() {
    let (mut scene, lease_id, tile_id, node_id, old_bytes) = scene_with_static_image_node(32, 32);
    let first_bytes = 64_u64 * 64 * 4;
    let final_bytes = 128_u64 * 128 * 4;
    let image = |width, decoded_bytes| {
        NodeData::StaticImage(StaticImageNode {
            resource_id: make_test_image_resource(width, width).0,
            width,
            height: width,
            decoded_bytes,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
        })
    };
    let batch = crate::mutation::MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent".to_string(),
        mutations: vec![
            crate::mutation::SceneMutation::UpdateNodeContent {
                tile_id,
                node_id,
                data: image(64, first_bytes),
            },
            crate::mutation::SceneMutation::UpdateNodeContent {
                tile_id,
                node_id,
                data: image(128, final_bytes),
            },
        ],
        timing_hints: None,
        lease_id: Some(lease_id),
    };

    let delta = scene.mutation_budget_delta(&lease_id, &batch);
    assert_eq!(
        delta.delta_texture_bytes,
        i64::try_from(final_bytes - old_bytes).unwrap(),
        "aggregate admission must charge the final batch state, not compare every update to the pre-batch state"
    );
    scene
        .leases
        .get_mut(&lease_id)
        .expect("lease exists")
        .resource_budget
        .max_texture_bytes = final_bytes;
    assert!(
        scene.check_budget(&lease_id, &batch).is_ok(),
        "per-session admission must evaluate the same final virtual batch state"
    );
}

/// UpdateNodeContent with the SAME resource_id must not change the ref count.
#[test]
fn resource_refs_unchanged_on_update_node_content_same_resource() {
    let (mut scene, _lease_id, tile_id, node_id, decoded_bytes) =
        scene_with_static_image_node(32, 32);
    let (resource_id, _) = make_test_image_resource(32, 32);

    assert_eq!(scene.resource_ref_count(&resource_id), Some(1));

    // Update node content with the same resource_id (only fit_mode changes).
    scene
        .update_node_content_checked(
            tile_id,
            node_id,
            NodeData::StaticImage(StaticImageNode {
                resource_id, // same
                width: 32,
                height: 32,
                decoded_bytes,                 // same
                fit_mode: ImageFitMode::Cover, // changed
                bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            }),
            "agent",
        )
        .unwrap();

    // Ref count must be unchanged.
    assert_eq!(
        scene.resource_ref_count(&resource_id),
        Some(1),
        "ref count must remain 1 when UpdateNodeContent uses the same resource_id"
    );
}

// ─── Lease State Machine Tests (RFC 0008) ───────────────────────────

#[test]
fn test_lease_state_defaults_to_active() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let lease_id = scene.grant_lease("test", 60_000);
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
    assert!(scene.leases[&lease_id].is_active());
    assert!(scene.leases[&lease_id].is_mutations_allowed());
}

#[test]
fn test_lease_suspend_from_active() {
    let (mut scene, clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 60_000);
    clock.advance(10_000); // 10s elapsed
    scene.suspend_lease(&lease_id, clock.now_millis()).unwrap();

    let lease = &scene.leases[&lease_id];
    assert_eq!(lease.state, LeaseState::Suspended);
    assert!(!lease.is_mutations_allowed());
    assert!(lease.suspended_at_ms.is_some());
    assert!(lease.ttl_remaining_at_suspend_ms.is_some());
    // 60_000 - 10_000 = 50_000 remaining at suspend
    assert_eq!(lease.ttl_remaining_at_suspend_ms, Some(50_000));
}

#[test]
fn test_lease_suspend_invalid_from_non_active() {
    let (mut scene, _clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 60_000);

    // Suspend once (valid)
    scene.suspend_lease(&lease_id, 1000).unwrap();

    // Suspend again from Suspended state (invalid)
    let err = scene.suspend_lease(&lease_id, 2000).unwrap_err();
    assert!(matches!(
        err,
        LeaseError::InvalidTransition {
            from: LeaseState::Suspended,
            to: LeaseState::Suspended,
        }
    ));
}

#[test]
fn test_lease_resume_from_suspended() {
    let (mut scene, clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 60_000);

    clock.advance(10_000);
    scene.suspend_lease(&lease_id, clock.now_millis()).unwrap();

    clock.advance(5_000); // 5s in suspended state
    scene.resume_lease(&lease_id, clock.now_millis()).unwrap();

    let lease = &scene.leases[&lease_id];
    assert_eq!(lease.state, LeaseState::Active);
    assert!(lease.is_mutations_allowed());
    assert!(lease.suspended_at_ms.is_none());
    assert!(lease.ttl_remaining_at_suspend_ms.is_none());
    // After resume: TTL should reflect the remaining time from suspension
    // remaining was 50_000 at suspend; now granted_at_ms is set to resume time
    // so remaining_ms(now) should be ~50_000
    assert_eq!(lease.remaining_ms(clock.now_millis()), 50_000);
}

#[test]
fn test_lease_resume_invalid_from_active() {
    let (mut scene, _clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 60_000);

    let err = scene.resume_lease(&lease_id, 1000).unwrap_err();
    assert!(matches!(
        err,
        LeaseError::InvalidTransition {
            from: LeaseState::Active,
            to: LeaseState::Active,
        }
    ));
}

#[test]
fn test_lease_disconnect_from_active() {
    let (mut scene, clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 60_000);

    clock.advance(5_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();

    let lease = &scene.leases[&lease_id];
    assert_eq!(lease.state, LeaseState::Orphaned);
    assert!(!lease.is_mutations_allowed());
    assert_eq!(lease.disconnected_at_ms, Some(6_000)); // 1000 start + 5000
}

#[test]
fn test_lease_disconnect_invalid_from_suspended() {
    let (mut scene, _clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 60_000);
    scene.suspend_lease(&lease_id, 1000).unwrap();

    let err = scene.disconnect_lease(&lease_id, 2000).unwrap_err();
    assert!(matches!(
        err,
        LeaseError::InvalidTransition {
            from: LeaseState::Suspended,
            to: LeaseState::Orphaned,
        }
    ));
}

#[test]
fn test_lease_reconnect_within_grace() {
    let (mut scene, clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 60_000);

    clock.advance(5_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();

    // Reconnect within the 30s grace period
    clock.advance(10_000);
    scene
        .reconnect_lease(&lease_id, clock.now_millis())
        .unwrap();

    let lease = &scene.leases[&lease_id];
    assert_eq!(lease.state, LeaseState::Active);
    assert!(lease.is_mutations_allowed());
    assert!(lease.disconnected_at_ms.is_none());
}

#[test]
fn test_lease_reconnect_after_grace_fails() {
    let (mut scene, clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 120_000);

    clock.advance(5_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();

    // Advance past the 30s grace period
    clock.advance(31_000);
    let err = scene
        .reconnect_lease(&lease_id, clock.now_millis())
        .unwrap_err();
    assert!(matches!(err, LeaseError::InvalidTransition { .. }));
}

#[test]
fn test_lease_revoke_from_any_non_terminal() {
    let (mut scene, _clock) = scene_with_test_clock();

    // Revoke from Active
    let l1 = scene.grant_lease("t1", 60_000);
    scene.leases.get_mut(&l1).unwrap().revoke().unwrap();
    assert_eq!(scene.leases[&l1].state, LeaseState::Revoked);

    // Revoke from Suspended
    let l2 = scene.grant_lease("t2", 60_000);
    scene.leases.get_mut(&l2).unwrap().suspend(1000).unwrap();
    scene.leases.get_mut(&l2).unwrap().revoke().unwrap();
    assert_eq!(scene.leases[&l2].state, LeaseState::Revoked);

    // Revoke from Orphaned
    let l3 = scene.grant_lease("t3", 60_000);
    scene.leases.get_mut(&l3).unwrap().disconnect(1000).unwrap();
    scene.leases.get_mut(&l3).unwrap().revoke().unwrap();
    assert_eq!(scene.leases[&l3].state, LeaseState::Revoked);
}

#[test]
fn test_lease_revoke_from_terminal_fails() {
    let (mut scene, _clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 60_000);
    scene.leases.get_mut(&lease_id).unwrap().revoke().unwrap();

    // Already revoked — should fail
    let err = scene
        .leases
        .get_mut(&lease_id)
        .unwrap()
        .revoke()
        .unwrap_err();
    assert!(matches!(
        err,
        LeaseError::InvalidTransition {
            from: LeaseState::Revoked,
            to: LeaseState::Revoked,
        }
    ));
}

#[test]
fn test_lease_is_expired_not_when_suspended() {
    let (mut scene, clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 1_000);

    // Suspend at t=500ms (halfway)
    clock.advance(500);
    scene.suspend_lease(&lease_id, clock.now_millis()).unwrap();

    // Advance well past TTL
    clock.advance(10_000);
    assert!(!scene.leases[&lease_id].is_expired(clock.now_millis()));
}

// ─── Budget Enforcement Tests ───────────────────────────────────────

#[test]
fn test_budget_tile_count_within_limit() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);

    // Default budget: max_tiles = 8. Create 1 tile — should be fine.
    let batch = crate::mutation::MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "test".to_string(),
        mutations: vec![crate::mutation::SceneMutation::CreateTile {
            tab_id,
            namespace: "test".to_string(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            z_order: 1,
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result = scene.apply_batch(&batch);
    assert!(result.applied);
    assert!(!result.budget_warning);
}

#[test]
fn test_budget_tile_count_exceeds_limit() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);

    // Set budget to max 2 tiles
    scene
        .leases
        .get_mut(&lease_id)
        .unwrap()
        .resource_budget
        .max_tiles = 2;

    // Create 2 tiles (OK)
    for i in 0..2 {
        let batch = crate::mutation::MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: "test".to_string(),
            mutations: vec![crate::mutation::SceneMutation::CreateTile {
                tab_id,
                namespace: "test".to_string(),
                lease_id,
                bounds: Rect::new(i as f32 * 120.0, 0.0, 100.0, 100.0),
                z_order: i + 1,
            }],
            timing_hints: None,
            lease_id: None,
        };
        let result = scene.apply_batch(&batch);
        assert!(result.applied);
    }

    // Create a 3rd tile — should be rejected
    let batch = crate::mutation::MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "test".to_string(),
        mutations: vec![crate::mutation::SceneMutation::CreateTile {
            tab_id,
            namespace: "test".to_string(),
            lease_id,
            bounds: Rect::new(240.0, 0.0, 100.0, 100.0),
            z_order: 3,
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result = scene.apply_batch(&batch);
    assert!(!result.applied);
    assert!(result.error.is_some());
    assert_eq!(scene.tile_count(), 2);
}

#[test]
fn test_budget_soft_limit_warning() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);

    // Set budget to max 5 tiles; soft limit at 80% = 4 tiles
    scene
        .leases
        .get_mut(&lease_id)
        .unwrap()
        .resource_budget
        .max_tiles = 5;

    // Create 4 tiles (should trigger soft limit warning on the 4th)
    for i in 0..4 {
        let batch = crate::mutation::MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: "test".to_string(),
            mutations: vec![crate::mutation::SceneMutation::CreateTile {
                tab_id,
                namespace: "test".to_string(),
                lease_id,
                bounds: Rect::new(i as f32 * 120.0, 0.0, 100.0, 100.0),
                z_order: i + 1,
            }],
            timing_hints: None,
            lease_id: None,
        };
        scene.apply_batch(&batch);
    }

    assert!(scene.is_lease_budget_warning(&lease_id));

    // 5th tile should succeed (within hard limit) but with budget_warning
    let batch = crate::mutation::MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "test".to_string(),
        mutations: vec![crate::mutation::SceneMutation::CreateTile {
            tab_id,
            namespace: "test".to_string(),
            lease_id,
            bounds: Rect::new(480.0, 0.0, 100.0, 100.0),
            z_order: 5,
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result = scene.apply_batch(&batch);
    assert!(result.applied);
    assert!(result.budget_warning);
}

// ─── Suspension Tests ───────────────────────────────────────────────

#[test]
fn test_suspend_blocks_mutations() {
    let (mut scene, clock) = scene_with_test_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);

    // Suspend the lease
    clock.advance(1_000);
    scene.suspend_lease(&lease_id, clock.now_millis()).unwrap();

    // Try to create a tile — should fail
    let batch = crate::mutation::MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "test".to_string(),
        mutations: vec![crate::mutation::SceneMutation::CreateTile {
            tab_id,
            namespace: "test".to_string(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            z_order: 1,
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result = scene.apply_batch(&batch);
    assert!(!result.applied);
    assert_eq!(scene.tile_count(), 0);
}

#[test]
fn test_resume_allows_mutations_again() {
    let (mut scene, clock) = scene_with_test_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);

    // Suspend then resume
    clock.advance(1_000);
    scene.suspend_lease(&lease_id, clock.now_millis()).unwrap();
    clock.advance(2_000);
    scene.resume_lease(&lease_id, clock.now_millis()).unwrap();

    // Create a tile — should succeed
    let batch = crate::mutation::MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "test".to_string(),
        mutations: vec![crate::mutation::SceneMutation::CreateTile {
            tab_id,
            namespace: "test".to_string(),
            lease_id,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            z_order: 1,
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result = scene.apply_batch(&batch);
    assert!(result.applied);
    assert_eq!(scene.tile_count(), 1);
}

#[test]
fn test_ttl_paused_during_suspension() {
    let (mut scene, clock) = scene_with_test_clock();
    // Grant a 10-second lease
    let lease_id = scene.grant_lease("test", 10_000);

    // At t=5s, suspend
    clock.advance(5_000);
    scene.suspend_lease(&lease_id, clock.now_millis()).unwrap();
    let remaining_at_suspend = scene.leases[&lease_id].ttl_remaining_at_suspend_ms;
    assert_eq!(remaining_at_suspend, Some(5_000));

    // Advance 20 seconds while suspended
    clock.advance(20_000);
    // Should NOT be expired (TTL paused)
    assert!(!scene.leases[&lease_id].is_expired(clock.now_millis()));
    // Remaining should still be 5_000
    assert_eq!(
        scene.leases[&lease_id].remaining_ms(clock.now_millis()),
        5_000
    );

    // Resume
    scene.resume_lease(&lease_id, clock.now_millis()).unwrap();
    // Now remaining should be 5_000 from the resume point
    assert_eq!(
        scene.leases[&lease_id].remaining_ms(clock.now_millis()),
        5_000
    );

    // Advance 4 seconds — not yet expired
    clock.advance(4_000);
    assert!(!scene.leases[&lease_id].is_expired(clock.now_millis()));
    assert_eq!(
        scene.leases[&lease_id].remaining_ms(clock.now_millis()),
        1_000
    );

    // Advance 2 more seconds — now expired
    clock.advance(2_000);
    assert!(scene.leases[&lease_id].is_expired(clock.now_millis()));
}

// ─── Grace Period Tests ─────────────────────────────────────────────

#[test]
fn test_grace_period_disconnect_and_reconnect() {
    let (mut scene, clock) = scene_with_test_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 120_000);
    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    // Disconnect
    clock.advance(5_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();
    assert_eq!(scene.tile_count(), 1); // Tiles preserved

    // Reconnect within grace (30s)
    clock.advance(15_000);
    scene
        .reconnect_lease(&lease_id, clock.now_millis())
        .unwrap();
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
    assert_eq!(scene.tile_count(), 1); // Tiles still there
}

#[test]
fn test_grace_period_expiry_cleans_up() {
    let (mut scene, clock) = scene_with_test_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 120_000);
    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    // Disconnect
    clock.advance(5_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();

    // Grace period expires (30s)
    clock.advance(31_000);
    let expiries = scene.expire_leases();
    assert_eq!(expiries.len(), 1);
    assert_eq!(expiries[0].terminal_state, LeaseState::Expired);
    assert_eq!(scene.tile_count(), 0); // Tiles cleaned up
}

/// hud-0q1dh: a DEGRADED (disconnected) portal surface is removed when its
/// lease's grace period expires, through the EXISTING orphan lifecycle — there
/// is no second timer and no leak.
///
/// The portal-disconnect/stale work (PR #878) renders a degraded treatment over
/// a live tile (dimmed surface, disconnection badge) but adds NO bespoke removal
/// path. This test pins the invariant that grace-bounded removal of a degraded
/// surface is the SAME orphan path that removes any other tile: `disconnect_lease`
/// dims the tile (degraded) and orphans the lease, then `expire_leases` — once
/// `check_grace_expired` — removes the still-degraded tile via the lease orphan
/// path, surfaces it in `LeaseExpiry::removed_tiles`, and enqueues it on the
/// `recently_removed_tile_ids` drain so out-of-graph per-tile state (driver drive
/// entry, resize state) is pruned with no dangling reference.
#[test]
fn degraded_portal_surface_removed_on_lease_grace_expiry_via_orphan_path() {
    let (mut scene, clock) = scene_with_test_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("portal-driver", 120_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-driver",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    // The portal stream/session drops: the HUD-side lease orphans and the surface
    // enters the DEGRADED treatment (dimmed + disconnection badge). The tile is
    // intentionally PRESERVED across the grace window so a reconnect can resume it.
    clock.advance(5_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Orphaned,
        "disconnect must orphan the lease (no second timer — grace lives on the lease)"
    );
    assert_eq!(
        scene.tiles[&tile_id].visual_hint,
        crate::lease::TileVisualHint::DisconnectionBadge,
        "the surviving surface must be in the degraded (disconnection-badge) state"
    );
    assert_eq!(
        scene.tile_count(),
        1,
        "degraded surface is preserved during grace"
    );

    // Within grace, expiry must NOT touch the degraded tile (no premature removal).
    clock.advance(29_000); // 34s since grant, 29s since disconnect (< 30s grace)
    let premature = scene.expire_leases();
    assert!(
        premature.is_empty(),
        "no lease may expire before its grace period elapses"
    );
    assert_eq!(
        scene.tile_count(),
        1,
        "degraded surface still present mid-grace"
    );
    assert_eq!(
        scene.tiles[&tile_id].visual_hint,
        crate::lease::TileVisualHint::DisconnectionBadge,
        "surface stays degraded until grace expiry or reconnect"
    );

    // Grace elapses: the SAME orphan path that removes any tile removes the
    // still-degraded one. No degraded-specific timer or pin keeps it alive.
    clock.advance(2_000); // now 31s since disconnect — past the 30s grace
    let expiries = scene.expire_leases();
    assert_eq!(
        expiries.len(),
        1,
        "grace-expired orphaned lease must be reaped"
    );
    assert_eq!(expiries[0].lease_id, lease_id);
    assert_eq!(expiries[0].terminal_state, LeaseState::Expired);
    assert!(
        expiries[0].removed_tiles.contains(&tile_id),
        "the degraded surface must be reported in LeaseExpiry::removed_tiles"
    );

    // No leak: the tile is gone from the graph...
    assert_eq!(
        scene.tile_count(),
        0,
        "degraded surface removed on grace expiry"
    );
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "no dangling tile entry after orphan-path removal"
    );
    // ...and its id is queued on the removed-tile drain so out-of-graph per-tile
    // state (driver drive entries, portal resize states) is pruned — the same
    // hook the windowed loop uses for any tile removal, not a degraded special case.
    let drained = scene.drain_removed_tile_ids();
    assert!(
        drained.contains(&tile_id),
        "removed degraded tile must be enqueued for out-of-graph state pruning"
    );
}

#[test]
fn test_grace_period_check() {
    let (mut scene, clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 120_000);

    clock.advance(5_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();

    // Not expired yet
    clock.advance(29_000);
    assert!(!scene.leases[&lease_id].check_grace_expired(clock.now_millis()));

    // Expired
    clock.advance(2_000);
    assert!(scene.leases[&lease_id].check_grace_expired(clock.now_millis()));
}

// ─── Safe Mode Tests ────────────────────────────────────────────────

#[test]
fn test_suspend_all_leases() {
    let (mut scene, clock) = scene_with_test_clock();
    let l1 = scene.grant_lease("agent1", 60_000);
    let l2 = scene.grant_lease("agent2", 60_000);
    let l3 = scene.grant_lease("agent3", 60_000);

    clock.advance(5_000);
    scene.suspend_all_leases(clock.now_millis());

    assert_eq!(scene.leases[&l1].state, LeaseState::Suspended);
    assert_eq!(scene.leases[&l2].state, LeaseState::Suspended);
    assert_eq!(scene.leases[&l3].state, LeaseState::Suspended);
}

#[test]
fn test_resume_all_leases() {
    let (mut scene, clock) = scene_with_test_clock();
    let l1 = scene.grant_lease("agent1", 60_000);
    let l2 = scene.grant_lease("agent2", 60_000);

    clock.advance(5_000);
    scene.suspend_all_leases(clock.now_millis());

    clock.advance(2_000);
    scene.resume_all_leases(clock.now_millis());

    assert_eq!(scene.leases[&l1].state, LeaseState::Active);
    assert_eq!(scene.leases[&l2].state, LeaseState::Active);
}

#[test]
fn test_suspend_all_skips_non_active() {
    let (mut scene, clock) = scene_with_test_clock();
    let l1 = scene.grant_lease("agent1", 60_000);
    let l2 = scene.grant_lease("agent2", 60_000);

    // Disconnect l2 first
    clock.advance(1_000);
    scene.disconnect_lease(&l2, clock.now_millis()).unwrap();

    // Suspend all — only l1 should be suspended
    clock.advance(1_000);
    scene.suspend_all_leases(clock.now_millis());

    assert_eq!(scene.leases[&l1].state, LeaseState::Suspended);
    assert_eq!(scene.leases[&l2].state, LeaseState::Orphaned); // Unchanged (not suspended)
}

#[test]
fn test_resume_all_only_resumes_suspended() {
    let (mut scene, clock) = scene_with_test_clock();
    let l1 = scene.grant_lease("agent1", 60_000);
    let l2 = scene.grant_lease("agent2", 60_000);

    // Disconnect l2
    clock.advance(1_000);
    scene.disconnect_lease(&l2, clock.now_millis()).unwrap();

    // Suspend only l1
    clock.advance(1_000);
    scene.suspend_lease(&l1, clock.now_millis()).unwrap();

    // Resume all — only l1 should be resumed
    clock.advance(1_000);
    scene.resume_all_leases(clock.now_millis());

    assert_eq!(scene.leases[&l1].state, LeaseState::Active);
    assert_eq!(scene.leases[&l2].state, LeaseState::Orphaned); // Unchanged (not suspended)
}

#[test]
fn test_suspension_timeout_revokes() {
    let (mut scene, clock) = scene_with_test_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 600_000);
    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    clock.advance(1_000);
    scene.suspend_lease(&lease_id, clock.now_millis()).unwrap();

    // Use a short max_suspend for testing
    let max_suspend = 5_000;
    clock.advance(6_000);

    let expiries = scene.expire_leases_with_max_suspend(max_suspend);
    assert_eq!(expiries.len(), 1);
    assert_eq!(expiries[0].terminal_state, LeaseState::Revoked);
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Revoked);
    assert_eq!(scene.tile_count(), 0);
}

// Spec §Requirement: Priority Assignment (lease-governance/spec.md lines 49-60)
// Spec §Requirement: Priority Sort Semantics (lease-governance/spec.md lines 62-69)

// ─── Resource Usage Tests ───────────────────────────────────────────

#[test]
fn test_lease_resource_usage() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);

    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();
    scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(200.0, 0.0, 100.0, 100.0),
            2,
        )
        .unwrap();

    let usage = scene.lease_resource_usage(&lease_id);
    assert_eq!(usage.tiles, 2);
}

#[test]
fn test_renew_lease_fails_when_not_active() {
    let (mut scene, clock) = scene_with_test_clock();
    let lease_id = scene.grant_lease("test", 60_000);

    // Suspend lease
    clock.advance(1_000);
    scene.suspend_lease(&lease_id, clock.now_millis()).unwrap();

    // Renew should fail (lease not active)
    let err = scene.renew_lease(lease_id, 120_000);
    assert!(err.is_err());
}

/// hud-pk9pz: `lease_is_active` is the liveness predicate that distinguishes a
/// usable lease from a merely-resident terminal one. It returns `false` for an
/// unknown lease, and — critically — `false` for an `Expired` lease that
/// grace-period reaping (`expire_leases`) left resident in the map. This is
/// what lets the portal driver start a fresh portal on a post-grace re-attach
/// instead of reusing a dead lease.
#[test]
fn lease_is_active_false_for_unknown_and_grace_expired_lease() {
    let (mut scene, clock) = scene_with_test_clock();

    // Unknown lease: not active.
    assert!(!scene.lease_is_active(&SceneId::new()));

    // Active lease: active.
    let lease_id = scene.grant_lease("agent", 120_000);
    assert!(scene.lease_is_active(&lease_id));

    // Orphan it and let the grace period elapse, then reap.
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();
    assert!(
        !scene.lease_is_active(&lease_id),
        "an orphaned (degraded) lease is not active"
    );
    clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1_000);
    let expiries = scene.expire_leases();
    assert_eq!(expiries.len(), 1);

    // The lease is Expired but STILL RESIDENT in the map, while
    // lease_is_active correctly reports false.
    assert!(
        scene.leases.contains_key(&lease_id),
        "expire_leases leaves the terminal lease resident"
    );
    assert!(
        !scene.lease_is_active(&lease_id),
        "a grace-expired lease must NOT be reported active — this is the load-bearing distinction"
    );
}

#[test]
fn test_lease_expiry_returns_lease_expiry_struct() {
    let (mut scene, clock) = scene_with_test_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("test", 500);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1,
        )
        .unwrap();

    clock.advance(501);
    let expiries = scene.expire_leases();
    assert_eq!(expiries.len(), 1);
    assert_eq!(expiries[0].lease_id, lease_id);
    assert_eq!(expiries[0].terminal_state, LeaseState::Expired);
    assert!(expiries[0].removed_tiles.contains(&tile_id));
}

/// hud-lyqun viewer geometry authority: once a tile is viewer-geometry-locked
/// (the viewer moved/resized the whole portal), an adapter's `update_tile_bounds`
/// republish MUST NOT reposition it — the call succeeds but the bounds are held.
/// Unlocking restores adapter control. This is the guard that keeps a portal
/// group coherent against a stale client-side layout republish.
#[test]
fn locked_tile_ignores_adapter_update_tile_bounds() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(10.0, 10.0, 100.0, 80.0),
            1,
        )
        .unwrap();

    // Before locking, the adapter can move the tile normally.
    scene
        .update_tile_bounds(tile_id, Rect::new(50.0, 60.0, 120.0, 90.0), "agent")
        .expect("unlocked adapter bounds update must apply");
    assert_eq!(
        scene.tiles[&tile_id].bounds,
        Rect::new(50.0, 60.0, 120.0, 90.0),
        "unlocked tile must accept adapter bounds"
    );

    // The viewer takes geometry authority (as a whole-portal move/resize would).
    scene.lock_viewer_geometry(tile_id);
    assert!(scene.is_viewer_geometry_locked(tile_id));
    let held = scene.tiles[&tile_id].bounds;
    let version_before = scene.version;

    // The adapter republishes its stale client-side layout — this must be a
    // no-op on geometry (Ok, but bounds unchanged and version not bumped).
    scene
        .update_tile_bounds(tile_id, Rect::new(300.0, 400.0, 200.0, 150.0), "agent")
        .expect("locked adapter bounds update must succeed as a no-op");
    assert_eq!(
        scene.tiles[&tile_id].bounds, held,
        "a viewer-geometry-locked tile must ignore adapter bounds republish"
    );
    assert_eq!(
        scene.version, version_before,
        "a suppressed adapter bounds republish must not bump the scene version"
    );

    // Unlocking restores adapter control.
    scene.unlock_viewer_geometry(tile_id);
    assert!(!scene.is_viewer_geometry_locked(tile_id));
    scene
        .update_tile_bounds(tile_id, Rect::new(11.0, 12.0, 130.0, 95.0), "agent")
        .expect("unlocked adapter bounds update must apply again");
    assert_eq!(
        scene.tiles[&tile_id].bounds,
        Rect::new(11.0, 12.0, 130.0, 95.0),
        "after unlock the adapter regains bounds control"
    );
}

/// Removing a tile must clear its viewer-geometry lock so a recycled SceneId
/// never inherits a stale authority flag.
#[test]
fn removing_tile_clears_viewer_geometry_lock() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(10.0, 10.0, 100.0, 80.0),
            1,
        )
        .unwrap();
    scene.lock_viewer_geometry(tile_id);
    assert!(scene.is_viewer_geometry_locked(tile_id));

    scene.remove_tile_and_nodes(tile_id);
    assert!(
        !scene.is_viewer_geometry_locked(tile_id),
        "removing a tile must drop its viewer-geometry lock"
    );
}

// ── hud-643dv: portal header-band drag handle + precedence ───────────────────

/// Build a minimal portal: a large frame tile (the anchor) carrying a minimize
/// HitRegion node in its header, plus a scrollable pane sharing the lease so the
/// lease group qualifies as a text-stream portal. Returns
/// `(scene, frame_id, pane_id, minimize_node_id)`.
fn portal_scene_with_minimize() -> (SceneGraph, SceneId, SceneId, SceneId) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("portal", 60_000);
    // Frame = the large anchor at (100,100) 600x400.
    let frame_id = scene
        .create_tile(
            tab_id,
            "portal",
            lease_id,
            Rect::new(100.0, 100.0, 600.0, 400.0),
            1,
        )
        .unwrap();
    // Minimize control in the header (tile-local x 0..44, y 0..52).
    let minimize_id = SceneId::new();
    let minimize = Node {
        layout: Default::default(),
        id: minimize_id,
        children: vec![],
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(0.0, 0.0, 44.0, 52.0),
            interaction_id: "portal-minimize".to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            ..Default::default()
        }),
    };
    scene.set_tile_root(frame_id, minimize).unwrap();
    // A scrollable pane member inside the frame → makes this a portal group.
    let pane_id = scene
        .create_tile(
            tab_id,
            "portal",
            lease_id,
            Rect::new(110.0, 160.0, 200.0, 320.0),
            3,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(pane_id, TileScrollConfig::vertical())
        .unwrap();
    (scene, frame_id, pane_id, minimize_id)
}

fn push_drag_handle(scene: &mut SceneGraph, element_id: SceneId, bounds: Rect, is_band: bool) {
    let interaction_id = "drag-handle:test".to_string();
    scene
        .overlay
        .drag_handle_hit_regions
        .push(DragHandleHitRegion {
            element_id,
            element_kind: DragHandleElementKind::Tile,
            bounds,
            interaction_id: interaction_id.clone(),
            hit_region: HitRegionNode {
                bounds,
                interaction_id,
                accepts_pointer: true,
                ..Default::default()
            },
            tab_order: 0,
            is_header_band: is_band,
        });
}

#[test]
fn header_band_drags_empty_header_but_yields_to_minimize_control() {
    let (mut scene, frame_id, _pane, minimize_id) = portal_scene_with_minimize();
    // Full-width header band over the frame's top strip (display space).
    push_drag_handle(
        &mut scene,
        frame_id,
        Rect::new(100.0, 100.0, 600.0, 52.0),
        true,
    );

    // A point on the band but NOT over the minimize control (global x=400 is
    // past the minimize rect which ends at global x=144) → the band drags.
    match scene.hit_test(400.0, 120.0) {
        HitResult::ZoneInteraction {
            kind: ZoneInteractionKind::DragHandle { element_id, .. },
            ..
        } => assert_eq!(element_id, frame_id, "empty header must drag the frame"),
        other => panic!("empty header band must hit the drag handle, got {other:?}"),
    }

    // A point over the minimize control (global (120,120) = tile-local (20,20),
    // inside the 44x52 minimize rect) → the CONTROL wins, not the band.
    assert_eq!(
        scene.hit_test(120.0, 120.0),
        HitResult::NodeHit {
            tile_id: frame_id,
            node_id: minimize_id,
            interaction_id: "portal-minimize".to_string(),
        },
        "an interactive control on the band must win over the drag band (Windows-titlebar)"
    );
}

#[test]
fn header_band_survives_full_tile_click_to_focus_region() {
    // Regression (hud-643dv): the #981/#987 projection portal publishes its
    // composer hit-region spanning the WHOLE tile (x:0,y:0,w,h, accepts_pointer)
    // for click-anywhere-to-focus. On a single-tile projection portal that region
    // overlaps the header band at EVERY point. A blanket "yield to any pointer
    // node" rule would make the band yield everywhere → drag dead on exactly the
    // surface live sessions use. The band must only yield to controls that FIT
    // INSIDE it (titlebar buttons), never a full-tile client-area region.
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("proj", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "proj",
            lease_id,
            Rect::new(0.0, 0.0, 300.0, 200.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    // Full-tile click-to-focus composer region (the #981 projection shape).
    let composer_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: composer_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
                    interaction_id: "proj-composer".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    // Header band over the top strip of the single tile.
    push_drag_handle(&mut scene, tile_id, Rect::new(0.0, 0.0, 300.0, 52.0), true);

    // A pointer-down inside the header band (also inside the full-tile composer)
    // must STILL resolve to the band → drag works on projection portals.
    match scene.hit_test(150.0, 20.0) {
        HitResult::ZoneInteraction {
            kind: ZoneInteractionKind::DragHandle { element_id, .. },
            ..
        } => assert_eq!(element_id, tile_id),
        other => {
            panic!("header band must drag over a full-tile click-to-focus region, got {other:?}")
        }
    }

    // Below the band (client area) the composer region still wins — the band
    // never reaches into the body.
    assert_eq!(
        scene.hit_test(150.0, 120.0),
        HitResult::NodeHit {
            tile_id,
            node_id: composer_id,
            interaction_id: "proj-composer".to_string(),
        },
        "outside the band the full-tile composer region still handles the click"
    );
}

#[test]
fn legacy_grip_keeps_chrome_priority_over_nodes() {
    // Regression guard: the header-band yield rule must NOT change legacy grip
    // precedence. A non-band grip overlapping a control still wins (grips never
    // overlap controls in practice, so their original chrome-priority stands).
    let (mut scene, frame_id, _pane, _minimize) = portal_scene_with_minimize();
    // Grip overlapping the minimize control (is_band=false).
    push_drag_handle(
        &mut scene,
        frame_id,
        Rect::new(100.0, 100.0, 44.0, 52.0),
        false,
    );

    match scene.hit_test(120.0, 120.0) {
        HitResult::ZoneInteraction {
            kind: ZoneInteractionKind::DragHandle { element_id, .. },
            ..
        } => assert_eq!(element_id, frame_id),
        other => panic!("legacy grip must keep chrome priority over nodes, got {other:?}"),
    }
}

#[test]
fn portal_header_band_anchors_targets_frame_only() {
    let (scene, frame_id, pane_id, _minimize) = portal_scene_with_minimize();
    let anchors = scene.portal_header_band_anchors(52.0);
    assert_eq!(anchors.len(), 1, "one portal → one header band");
    let (anchor, rect) = anchors[0];
    assert_eq!(
        anchor, frame_id,
        "the band belongs to the frame/anchor, not a pane"
    );
    assert_ne!(anchor, pane_id);
    // Band = top strip of the frame.
    assert_eq!(rect, Rect::new(100.0, 100.0, 600.0, 52.0));
}

#[test]
fn portal_header_band_anchors_includes_single_scrollable_tile() {
    // A degenerate one-member scrollable lease still gets the band (owner: same
    // as it gets whole-portal resize today).
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("solo", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "solo",
            lease_id,
            Rect::new(0.0, 0.0, 300.0, 200.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    let anchors = scene.portal_header_band_anchors(52.0);
    assert_eq!(anchors, vec![(tile_id, Rect::new(0.0, 0.0, 300.0, 52.0))]);
}

#[test]
fn portal_header_band_anchors_excludes_non_scrollable_tiles() {
    // A plain (non-scrollable) tile is not a portal and gets no band — it keeps
    // the legacy grip.
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("plain", 60_000);
    scene
        .create_tile(
            tab_id,
            "plain",
            lease_id,
            Rect::new(0.0, 0.0, 300.0, 200.0),
            1,
        )
        .unwrap();
    assert!(
        scene.portal_header_band_anchors(52.0).is_empty(),
        "non-scrollable tiles must not get a header band"
    );
}

#[test]
fn portal_anchor_tile_picks_largest_area_member() {
    let (scene, frame_id, pane_id, _minimize) = portal_scene_with_minimize();
    // From any member, the anchor resolves to the largest-area tile (the frame).
    assert_eq!(scene.portal_anchor_tile(pane_id), Some(frame_id));
    assert_eq!(scene.portal_anchor_tile(frame_id), Some(frame_id));
}

// ── hud-m4xay F5: drag band keys off the first-class surface Header part ──────

/// Build a single-tile portal that has DECLARED a first-class portal surface with
/// a `Header` part (empty backing node) + a `Transcript` part. Returns
/// (scene, tile_id). Header local bounds are (0,0,width,header_h).
fn portal_scene_with_declared_surface(header_h: f32) -> (SceneGraph, SceneId) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("portal", 60_000);
    let tile = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(100.0, 100.0, 600.0, 400.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(tile, TileScrollConfig::vertical())
        .unwrap();
    let surface = PortalSurface {
        identity: PortalIdentity {
            session_id: "sess-portal".to_string(),
            display_name: "Claude".to_string(),
            peer_class: PortalPeerClass::ResidentLlm,
        },
        lifecycle: PortalLifecycleState::Active,
        display_state: PortalDisplayState::Expanded,
        // Parts carry surface-local bounds and EMPTY backing nodes — exactly what
        // the resident adapter declares. The band must still key off the Header
        // part's bounds without any materialized node.
        parts: vec![
            PortalPart {
                kind: PortalPartKind::Frame,
                bounds: Rect::new(0.0, 0.0, 600.0, 400.0),
                node: None,
            },
            PortalPart {
                kind: PortalPartKind::Header,
                bounds: Rect::new(0.0, 0.0, 600.0, header_h),
                node: None,
            },
            PortalPart {
                kind: PortalPartKind::Transcript,
                bounds: Rect::new(0.0, header_h, 600.0, 400.0 - header_h),
                node: None,
            },
        ],
    };
    scene.set_portal_surface(tile, surface, "portal").unwrap();
    (scene, tile)
}

/// WHEN a portal declares a surface with a `Header` part THEN the drag band keys
/// off that part's bounds — NOT the top `band_h` strip heuristic.
#[test]
fn portal_header_band_anchors_keys_off_declared_header_part() {
    // Header part height (40) deliberately differs from the band_h argument (52)
    // so the assertion proves the band comes from the surface part, not the strip.
    let (scene, tile) = portal_scene_with_declared_surface(40.0);
    let anchors = scene.portal_header_band_anchors(52.0);
    assert_eq!(anchors.len(), 1, "one portal → one header band");
    assert_eq!(
        anchors[0],
        (tile, Rect::new(100.0, 100.0, 600.0, 40.0)),
        "band must equal the declared Header part bounds (absolute), not the 52px strip"
    );
}

/// A `Header` part with a negative local origin (accepted by `validate_structure`,
/// which only requires non-negative extents) is clamped to the tile origin — the
/// band never starts above/left of the frame (Codex review, hud-m4xay).
#[test]
fn portal_header_band_clamps_negative_part_origin_to_tile() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("portal", 60_000);
    let tile = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(100.0, 100.0, 600.0, 400.0),
            1,
        )
        .unwrap();
    // Header with a negative local origin and an extent that overhangs the frame.
    let surface = PortalSurface {
        identity: PortalIdentity {
            session_id: "sess-portal".to_string(),
            display_name: "Claude".to_string(),
            peer_class: PortalPeerClass::ResidentLlm,
        },
        lifecycle: PortalLifecycleState::Active,
        display_state: PortalDisplayState::Expanded,
        parts: vec![PortalPart {
            kind: PortalPartKind::Header,
            bounds: Rect::new(-20.0, -10.0, 600.0, 50.0),
            node: None,
        }],
    };
    scene.set_portal_surface(tile, surface, "portal").unwrap();
    let anchors = scene.portal_header_band_anchors(52.0);
    assert_eq!(anchors.len(), 1);
    let (_, rect) = anchors[0];
    // Origin clamped to the tile (100,100); the band never precedes the frame.
    assert!(
        rect.x >= 100.0 && rect.y >= 100.0,
        "band must not start before the tile: {rect:?}"
    );
    // And it stays within the tile bounds on the far edges.
    assert!(
        rect.x + rect.width <= 700.0 + f32::EPSILON,
        "band right edge within tile: {rect:?}"
    );
    assert!(
        rect.y + rect.height <= 500.0 + f32::EPSILON,
        "band bottom edge within tile: {rect:?}"
    );
}

/// A declared surface with NO `Header` part falls back to the raw-tile heuristic
/// (top `band_h` strip), so surface declaration never regresses the escape hatch.
#[test]
fn portal_header_band_anchors_falls_back_without_header_part() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("portal", 60_000);
    let tile = scene
        .create_tile(tab, "portal", lease, Rect::new(0.0, 0.0, 300.0, 200.0), 1)
        .unwrap();
    scene
        .register_tile_scroll_config(tile, TileScrollConfig::vertical())
        .unwrap();
    // Surface with only a Transcript part (no Header).
    let surface = PortalSurface {
        identity: PortalIdentity {
            session_id: "sess-portal".to_string(),
            display_name: "Claude".to_string(),
            peer_class: PortalPeerClass::ResidentLlm,
        },
        lifecycle: PortalLifecycleState::Active,
        display_state: PortalDisplayState::Expanded,
        parts: vec![PortalPart {
            kind: PortalPartKind::Transcript,
            bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
            node: None,
        }],
    };
    scene.set_portal_surface(tile, surface, "portal").unwrap();
    // No Header part → legacy top-strip heuristic (band_h = 52).
    assert_eq!(
        scene.portal_header_band_anchors(52.0),
        vec![(tile, Rect::new(0.0, 0.0, 300.0, 52.0))]
    );
}

// ── hud-ovjxu.1: viewer-local resize font-scale multiplier ───────────────────

// ── hud-cpjqe: live top-band drag flakiness diagnosis ────────────────────────

/// Build the EXACT live exemplar portal tile structure (PORTAL_W=860, H=680):
/// a capture-backstop and a frame with IDENTICAL bounds sharing the lease, plus
/// two scrollable panes, a far-corner drag shield, and a minimized-icon tile —
/// backstop/shield/minimized-icon in PASSTHROUGH, frame/panes in CAPTURE. The
/// frame carries the header nodes (minimize 44x52 accepts_pointer; the header
/// drag marker is inert). Returns (scene, frame, backstop, input, output).
fn live_exemplar_portal_scene() -> (SceneGraph, SceneId, SceneId, SceneId, SceneId) {
    const PX: f32 = 100.0;
    const PY: f32 = 100.0;
    const PORTAL_W: f32 = 860.0;
    const PORTAL_H: f32 = 680.0;
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("portal", 60_000);
    // Backstop FIRST (z lowest), same bounds as the frame.
    let backstop = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(PX, PY, PORTAL_W, PORTAL_H),
            0,
        )
        .unwrap();
    scene
        .update_tile_input_mode(backstop, InputMode::Passthrough, "portal")
        .unwrap();
    // Frame — identical bounds, CAPTURE. Carries the header nodes.
    let frame = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(PX, PY, PORTAL_W, PORTAL_H),
            1,
        )
        .unwrap();
    let minimize_id = SceneId::new();
    scene
        .set_tile_root(
            frame,
            Node {
                layout: Default::default(),
                id: minimize_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 44.0, 52.0),
                    interaction_id: "portal-minimize".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    // Panes (scrollable, CAPTURE), below the 52px header.
    let input = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(PX, PY + 53.0, 420.0, 590.0),
            3,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(input, TileScrollConfig::vertical())
        .unwrap();
    let output = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(PX + 440.0, PY + 53.0, 420.0, 590.0),
            4,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(output, TileScrollConfig::vertical())
        .unwrap();
    // Far-corner drag shield (passthrough, outside the frame).
    let shield = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(1919.0, 1079.0, 1.0, 1.0),
            20,
        )
        .unwrap();
    scene
        .update_tile_input_mode(shield, InputMode::Passthrough, "portal")
        .unwrap();
    (scene, frame, backstop, input, output)
}

/// Replicate the compositor's `collect_drag_handle_entries` + populate step
/// (band for the portal anchor, legacy 24x8 grip for every other visible tile)
/// so `hit_test` can be swept without a GPU.
fn populate_live_drag_handles(scene: &mut SceneGraph) {
    let band_rects: std::collections::HashMap<SceneId, Rect> =
        scene.portal_header_band_anchors(52.0).into_iter().collect();
    let tiles: Vec<(SceneId, Rect)> = scene
        .visible_tiles()
        .iter()
        .map(|t| (t.id, t.bounds))
        .collect();
    scene.overlay.drag_handle_hit_regions.clear();
    for (tab_order, (id, bounds)) in (0u32..).zip(tiles) {
        let (rect, is_band) = if let Some(band) = band_rects.get(&id) {
            (*band, true)
        } else {
            // drag_handle_bounds: 24x8 centered grip straddling the top edge.
            let w = 24.0_f32;
            let h = 8.0_f32;
            let x = (bounds.x + (bounds.width - w) * 0.5).clamp(0.0, 1920.0 - w);
            let y = (bounds.y - h * 0.5).clamp(0.0, 1080.0 - h);
            (Rect::new(x, y, w, h), false)
        };
        scene
            .overlay
            .drag_handle_hit_regions
            .push(DragHandleHitRegion {
                element_id: id,
                element_kind: DragHandleElementKind::Tile,
                bounds: rect,
                interaction_id: format!("drag-handle:{id:?}"),
                hit_region: HitRegionNode {
                    bounds: rect,
                    interaction_id: format!("drag-handle:{id:?}"),
                    accepts_pointer: true,
                    ..Default::default()
                },
                tab_order,
                is_header_band: is_band,
            });
    }
}

#[test]
fn header_band_anchor_is_deterministically_the_visible_frame_not_backstop() {
    // hud-cpjqe root cause: the capture-backstop and the frame have IDENTICAL
    // bounds, so the largest-area anchor pick tie-breaks on random UUIDs — the
    // header band lands on a non-deterministic tile each session ("fails half the
    // time"). The band MUST deterministically anchor on the visible interactive
    // frame, never the invisible passthrough backstop.
    let (scene, frame, backstop, _in, _out) = live_exemplar_portal_scene();
    let anchors = scene.portal_header_band_anchors(52.0);
    let anchor_ids: Vec<SceneId> = anchors.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        anchor_ids,
        vec![frame],
        "the header band must anchor on the visible frame, not the passthrough backstop"
    );
    assert!(
        !anchor_ids.contains(&backstop),
        "a passthrough capture-backstop must never own the drag band"
    );
}

#[test]
fn header_band_drag_engages_across_full_empty_header_width() {
    // hud-cpjqe: sweep hit_test across the full band width at three header y
    // values against the real live tile structure. Every empty-header point (past
    // the minimize control) MUST resolve to a whole-portal drag; the minimize
    // control MUST still win in its own rect. A DragHandle whose element resolves
    // into the portal group is a successful drag-engage.
    let (mut scene, frame, backstop, _in, _out) = live_exemplar_portal_scene();
    populate_live_drag_handles(&mut scene);

    // Any of the portal's structural tiles resolving to the same group = a good
    // drag (translate_portal_group_on_drag re-resolves the group from the anchor).
    let group_ids = [frame, backstop];
    let px = 100.0_f32;
    let py = 100.0_f32;
    let mut misses: Vec<(f32, f32, String)> = Vec::new();
    for &ly in &[10.0_f32, 26.0, 40.0] {
        let mut x = 2.0_f32;
        while x < 860.0 {
            let gx = px + x;
            let gy = py + ly;
            let hit = scene.hit_test(gx, gy);
            let ok = match &hit {
                HitResult::ZoneInteraction {
                    kind: ZoneInteractionKind::DragHandle { element_id, .. },
                    ..
                } => {
                    group_ids.contains(element_id)
                        || scene.portal_anchor_tile(*element_id) == Some(frame)
                }
                HitResult::NodeHit { interaction_id, .. } => {
                    // Only acceptable non-drag hit is the minimize control (x<44).
                    interaction_id == "portal-minimize" && x < 44.0
                }
                _ => false,
            };
            if !ok {
                misses.push((x, ly, format!("{hit:?}")));
            }
            x += 8.0;
        }
    }
    assert!(
        misses.is_empty(),
        "hud-cpjqe: {} header points did not engage a whole-portal drag \
         (dead spots => flaky drag). First few: {:?}",
        misses.len(),
        &misses[..misses.len().min(6)]
    );
}

#[test]
fn tile_font_scale_defaults_to_one_and_round_trips() {
    let mut scene = SceneGraph::new(800.0, 600.0);
    let id = SceneId::new();
    // Absent entry → default 1.0 (no scaling).
    assert_eq!(scene.tile_font_scale(id), 1.0);

    scene.set_tile_font_scale(id, 1.4);
    assert!((scene.tile_font_scale(id) - 1.4).abs() < 1e-6);

    // Setting back to exactly 1.0 drops the entry (map only holds zoomed tiles).
    scene.set_tile_font_scale(id, 1.0);
    assert_eq!(scene.tile_font_scale(id), 1.0);
    assert!(!scene.overlay.tile_font_scale.contains_key(&id));

    // Non-finite / non-positive factors are ignored (defensive).
    scene.set_tile_font_scale(id, 0.75);
    scene.set_tile_font_scale(id, f32::NAN);
    scene.set_tile_font_scale(id, -2.0);
    scene.set_tile_font_scale(id, 0.0);
    assert!((scene.tile_font_scale(id) - 0.75).abs() < 1e-6);

    scene.clear_tile_font_scale(id);
    assert_eq!(scene.tile_font_scale(id), 1.0);
}

#[test]
fn tile_font_scale_cleared_on_tile_removal() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 100.0, 80.0),
            1,
        )
        .unwrap();
    scene.set_tile_font_scale(tile_id, 1.5);
    assert!((scene.tile_font_scale(tile_id) - 1.5).abs() < 1e-6);

    scene.remove_tile_and_nodes(tile_id);
    assert_eq!(
        scene.tile_font_scale(tile_id),
        1.0,
        "removing a tile must drop its viewer font scale"
    );
}

// ─── Lifecycle-accent lease gate (hud-a745w) ─────────────────────────────────
//
// The in-process portal render-batch path (`apply_portal_render_batch_to_scene`)
// bypasses the `apply_batch` Stage-1 lease check, so it must reach the accent
// overlay through the *checked* variants. These prove the checked variants gate
// the mutation on lease state exactly like the sibling `set_tile_root_checked`
// content paint: a suspended/orphaned lease is rejected (and `scene.version` is
// NOT bumped, so the #943 present-gate does not re-arm), while an Active lease —
// including the degraded-grace state after a within-grace reconnect — still
// applies.

/// Build a scene with an active `ModifyOwnTiles` lease and one tile owned by it.
fn scene_with_accented_tile() -> (SceneGraph, TestClock, SceneId, SceneId) {
    let (mut scene, clock) = scene_with_test_clock();
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 200.0, 120.0),
            1,
        )
        .unwrap();
    (scene, clock, lease_id, tile_id)
}

fn sample_accent() -> LifecycleAccent {
    LifecycleAccent {
        color: Rgba::new(0.2, 0.6, 0.9, 1.0),
        width_px: 4.0,
    }
}

#[test]
fn test_lifecycle_accent_checked_applies_under_active_lease() {
    let (mut scene, _clock, _lease_id, tile_id) = scene_with_accented_tile();
    let version_before = scene.version;

    scene
        .set_tile_lifecycle_accent_checked(tile_id, sample_accent(), "agent")
        .expect("active lease + ModifyOwnTiles must apply the accent");

    assert_eq!(scene.tile_lifecycle_accent(tile_id), Some(sample_accent()));
    assert_eq!(
        scene.version,
        version_before + 1,
        "an applied accent must bump scene.version to re-arm the present-gate"
    );
}

#[test]
fn test_lifecycle_accent_checked_rejected_under_suspended_lease() {
    let (mut scene, clock, lease_id, tile_id) = scene_with_accented_tile();

    // Enter safe mode: suspend the lease.
    scene.suspend_lease(&lease_id, clock.now_millis()).unwrap();
    assert!(!scene.leases[&lease_id].is_mutations_allowed());

    let version_before = scene.version;
    let err = scene
        .set_tile_lifecycle_accent_checked(tile_id, sample_accent(), "agent")
        .expect_err("a suspended lease must reject the accent mutation");
    assert!(
        matches!(err, ValidationError::InvalidField { ref field, .. } if field == "lease_state"),
        "expected a lease_state rejection, got {err:?}"
    );

    assert_eq!(
        scene.tile_lifecycle_accent(tile_id),
        None,
        "the accent must not be stored under a suspended lease"
    );
    assert_eq!(
        scene.version, version_before,
        "a rejected accent must NOT bump scene.version (no present-gate re-arm \
         escaping lease suspension)"
    );
}

#[test]
fn test_lifecycle_accent_clear_checked_rejected_under_suspended_lease() {
    let (mut scene, clock, lease_id, tile_id) = scene_with_accented_tile();

    // Pre-seed an accent while Active, then suspend.
    scene
        .set_tile_lifecycle_accent_checked(tile_id, sample_accent(), "agent")
        .unwrap();
    scene.suspend_lease(&lease_id, clock.now_millis()).unwrap();

    let version_before = scene.version;
    let err = scene
        .clear_tile_lifecycle_accent_checked(tile_id, "agent")
        .expect_err("a suspended lease must reject clearing the accent");
    assert!(
        matches!(err, ValidationError::InvalidField { ref field, .. } if field == "lease_state"),
        "expected a lease_state rejection, got {err:?}"
    );
    assert_eq!(
        scene.tile_lifecycle_accent(tile_id),
        Some(sample_accent()),
        "the accent must survive a rejected clear under a suspended lease"
    );
    assert_eq!(
        scene.version, version_before,
        "a rejected clear must NOT bump scene.version"
    );
}

#[test]
fn test_lifecycle_accent_checked_rejected_while_orphaned_then_applies_after_grace_reconnect() {
    let (mut scene, clock, lease_id, tile_id) = scene_with_accented_tile();

    // Ungraceful drop: the driver lease is orphaned while grace runs.
    clock.advance(1_000);
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .unwrap();
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);

    // While orphaned (no resume yet), the accent — like the sibling content paint
    // — must be rejected: an orphaned lease is not a mutation-permitting state.
    let version_before = scene.version;
    let err = scene
        .set_tile_lifecycle_accent_checked(tile_id, sample_accent(), "agent")
        .expect_err("an orphaned lease must reject the accent mutation");
    assert!(
        matches!(err, ValidationError::InvalidField { ref field, .. } if field == "lease_state"),
        "expected a lease_state rejection, got {err:?}"
    );
    assert_eq!(scene.tile_lifecycle_accent(tile_id), None);
    assert_eq!(scene.version, version_before);

    // Owner returns within grace: the lease-grace path reconnects the lease to
    // Active (mirrors `reconnect_lease_grace_on_resume`) BEFORE the degraded
    // repaint renders. The accent must now apply — the gate rejects only genuinely
    // inactive leases, not the reconnected degraded-grace state.
    clock.advance(5_000);
    scene
        .reconnect_lease(&lease_id, clock.now_millis())
        .unwrap();
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);

    // Capture the version after the lease transitions (which themselves bump it)
    // so we isolate the accent mutation's own +1.
    let version_before_apply = scene.version;
    scene
        .set_tile_lifecycle_accent_checked(tile_id, sample_accent(), "agent")
        .expect("a reconnected (degraded-grace) Active lease must apply the accent");
    assert_eq!(scene.tile_lifecycle_accent(tile_id), Some(sample_accent()));
    assert_eq!(
        scene.version,
        version_before_apply + 1,
        "the degraded-grace repaint accent must apply after reconnect"
    );
}

// ── Inline subtree materialization: set_tile_root_tree (hud-ga4md) ───────────

#[cfg(test)]
mod inline_subtree_materialization {
    use super::*;

    /// (scene, tile_id) with a lease carrying CreateTiles + ModifyOwnTiles.
    fn scene_with_tile() -> (SceneGraph, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        (scene, tile_id)
    }

    fn text_node(id: SceneId, children: Vec<SceneId>, content: &str) -> Node {
        Node {
            layout: Default::default(),
            id,
            children,
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: content.to_string(),
                bounds: Rect::new(0.0, 0.0, 100.0, 20.0),
                font_size_px: 16.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba::WHITE,
                background: None,
                alignment: TextAlign::Start,
                overflow: TextOverflow::Ellipsis,
                color_runs: Box::default(),
            }),
        }
    }

    fn hit_node(id: SceneId) -> Node {
        Node {
            layout: Default::default(),
            id,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 0.0, 50.0, 20.0),
                interaction_id: "child-hit".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn materializes_full_subtree_atomically() {
        let (mut scene, tile_id) = scene_with_tile();
        let root_id = SceneId::new();
        let child_a = SceneId::new();
        let child_b = SceneId::new();
        let grandchild = SceneId::new();

        // root → [a → [grandchild(hit)], b]
        let root = text_node(root_id, vec![child_a, child_b], "root");
        let descendants = vec![
            text_node(child_a, vec![grandchild], "a"),
            hit_node(grandchild),
            text_node(child_b, vec![], "b"),
        ];

        scene
            .set_tile_root_tree(tile_id, root, descendants)
            .expect("subtree materializes");

        // Every node is in the flat map and reachable.
        assert_eq!(scene.node_count(), 4, "root + 3 descendants all inserted");
        for id in [root_id, child_a, child_b, grandchild] {
            assert!(scene.nodes.contains_key(&id), "{id:?} materialized");
        }
        // Tile root points at the subtree root.
        assert_eq!(scene.tiles.get(&tile_id).unwrap().root_node, Some(root_id));
        // Parent→child links preserved.
        assert_eq!(scene.nodes[&root_id].children, vec![child_a, child_b]);
        assert_eq!(scene.nodes[&child_a].children, vec![grandchild]);
        // Descendant hit-region got its local input state (registered per-node).
        assert!(
            scene.hit_region_states.contains_key(&grandchild),
            "descendant HitRegion must get local state, not just the root"
        );
    }

    #[test]
    fn empty_descendants_equals_flat_set_tile_root() {
        let (mut scene, tile_id) = scene_with_tile();
        let root_id = SceneId::new();
        scene
            .set_tile_root_tree(tile_id, text_node(root_id, vec![], "flat"), vec![])
            .expect("flat root");
        assert_eq!(scene.node_count(), 1);
        assert_eq!(scene.tiles.get(&tile_id).unwrap().root_node, Some(root_id));
    }

    #[test]
    fn republish_replaces_whole_subtree() {
        let (mut scene, tile_id) = scene_with_tile();
        let r1 = SceneId::new();
        let c1 = SceneId::new();
        scene
            .set_tile_root_tree(
                tile_id,
                text_node(r1, vec![c1], "root1"),
                vec![text_node(c1, vec![], "c1")],
            )
            .unwrap();
        assert_eq!(scene.node_count(), 2);

        // Republish with a fresh subtree — the old one is fully removed.
        let r2 = SceneId::new();
        let c2 = SceneId::new();
        let c3 = SceneId::new();
        scene
            .set_tile_root_tree(
                tile_id,
                text_node(r2, vec![c2, c3], "root2"),
                vec![text_node(c2, vec![], "c2"), text_node(c3, vec![], "c3")],
            )
            .unwrap();
        assert_eq!(
            scene.node_count(),
            3,
            "old subtree removed, new one present"
        );
        for old in [r1, c1] {
            assert!(!scene.nodes.contains_key(&old), "{old:?} should be gone");
        }
        for new in [r2, c2, c3] {
            assert!(scene.nodes.contains_key(&new));
        }
    }

    #[test]
    fn oversized_subtree_rejected_atomically() {
        let (mut scene, tile_id) = scene_with_tile();
        let root_id = SceneId::new();
        // Root + (MAX_NODES_PER_TILE) descendants = MAX+1 > limit.
        let child_ids: Vec<SceneId> = (0..crate::graph::MAX_NODES_PER_TILE)
            .map(|_| SceneId::new())
            .collect();
        let root = text_node(root_id, child_ids.clone(), "root");
        let descendants: Vec<Node> = child_ids
            .iter()
            .map(|id| text_node(*id, vec![], "c"))
            .collect();

        let err = scene
            .set_tile_root_tree(tile_id, root, descendants)
            .expect_err("must exceed per-tile node limit");
        assert!(matches!(err, ValidationError::NodeCountExceeded { .. }));
        // Atomic: nothing from the rejected subtree was inserted.
        assert_eq!(scene.node_count(), 0);
        assert!(!scene.nodes.contains_key(&root_id));
        assert_eq!(scene.tiles.get(&tile_id).unwrap().root_node, None);
    }

    #[test]
    fn duplicate_descendant_id_rejected_atomically() {
        let (mut scene, tile_id) = scene_with_tile();
        let root_id = SceneId::new();
        let dup = SceneId::new();
        // Two descendants share `dup` → DuplicateId, whole subtree rejected.
        let root = text_node(root_id, vec![dup], "root");
        let descendants = vec![text_node(dup, vec![], "x"), text_node(dup, vec![], "y")];
        let err = scene
            .set_tile_root_tree(tile_id, root, descendants)
            .expect_err("duplicate descendant id must be rejected");
        assert!(matches!(err, ValidationError::DuplicateId { .. }));
        assert_eq!(scene.node_count(), 0, "no partial materialization");
    }
}

/// Viewer dismiss reclaims the tile's whole lease (human override) from every
/// live state (Active, Orphaned, Suspended), reports the transition once, takes
/// every tile and zone publication the lease owns, and leaves other agents alone.
#[test]
fn viewer_dismiss_tile_revokes_lease_in_any_live_state() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.zone_registry = ZoneRegistry::with_defaults();
    let tab = scene.create_tab("Main", 0).unwrap();
    let tile_for = |scene: &mut SceneGraph, ns: &str, lease| {
        scene
            .create_tile(tab, ns, lease, Rect::new(0.0, 0.0, 100.0, 100.0), 1)
            .unwrap()
    };

    let bystander_lease = scene.grant_lease("other", 60_000);
    let bystander_tile = tile_for(&mut scene, "other", bystander_lease);
    scene
        .publish_to_zone_for_lease(
            "pip",
            ZoneContent::SolidColor(Rgba::WHITE),
            "other",
            None,
            None,
            None,
            Vec::new(),
            Some(bystander_lease),
        )
        .unwrap();

    for previous in [
        LeaseState::Active,
        LeaseState::Orphaned,
        LeaseState::Suspended,
    ] {
        let lease = scene.grant_lease("agent", 60_000);
        let (tile, second) = (
            tile_for(&mut scene, "agent", lease),
            tile_for(&mut scene, "agent", lease),
        );
        scene
            .publish_to_zone_for_lease(
                "subtitle",
                ZoneContent::StreamText("agent".into()),
                "agent",
                None,
                None,
                None,
                Vec::new(),
                Some(lease),
            )
            .unwrap();
        match previous {
            LeaseState::Orphaned => scene.disconnect_lease(&lease, 1_000).unwrap(),
            LeaseState::Suspended => scene.suspend_lease(&lease, 1_000).unwrap(),
            _ => {}
        }

        let expiry = scene.viewer_dismiss_tile(tile).expect("lease reclaimed");
        assert_eq!(expiry.lease_id, lease);
        assert_eq!(expiry.previous_state, previous);
        assert_eq!(expiry.terminal_state, LeaseState::Revoked);
        let mut removed = expiry.removed_tiles.clone();
        removed.sort();
        let mut both = vec![tile, second];
        both.sort();
        assert_eq!(removed, both, "dismiss reclaims the lease's other tile too");
        assert!(!scene.tiles.contains_key(&tile) && !scene.tiles.contains_key(&second));
        assert!(
            scene
                .zone_registry
                .active_for_zone("subtitle")
                .iter()
                .all(|r| r.publisher_namespace != "agent"),
            "the lease's zone publication is reclaimed"
        );
        assert!(
            scene.viewer_dismiss_tile(tile).is_none(),
            "a second dismiss finds nothing to reclaim"
        );
        assert!(scene.tiles.contains_key(&bystander_tile));
        assert!(
            scene
                .zone_registry
                .active_for_zone("pip")
                .iter()
                .any(|r| r.publisher_namespace == "other"),
            "another agent's publication survives"
        );
    }
}

/// Hard cap: the runtime-wide cap rejects the 65th live lease even from one
/// session; releasing one frees a slot.
#[test]
fn test_lease_hard_cap_rejects_65th_live_lease() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let session = SceneId::new();
    let leases: Vec<SceneId> = (0..SceneGraph::MAX_RUNTIME_LEASES)
        .map(|_| scene.grant_lease_for_session("agent", session, 60_000))
        .collect();

    let err = scene
        .try_grant_lease_for_session("agent", session, 60_000)
        .expect_err("65th live lease must be rejected");
    assert!(
        matches!(
            err,
            LeaseError::CapsExceeded(CapsError::MaxRuntimeLeasesExceeded { .. })
        ),
        "{err:?}"
    );

    scene.revoke_lease(leases[0]).expect("revoke");
    scene
        .try_grant_lease_for_session("agent", session, 60_000)
        .expect("a terminal lease frees its slot");
}
