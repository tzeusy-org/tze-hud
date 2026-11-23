//! Portal tile overlay and input behavior exercised through direct scene operations.

use super::*;

#[test]
fn checked_unread_count_updates_overlay_and_enforces_namespace() {
    // The native checked setter updates the compositor overlay and enforces
    // namespace and lease admission before changing state.
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
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
    assert_eq!(
        scene.tile_unread_count(tile_id),
        0,
        "no badge before any count is set"
    );

    // Set the badge count. A count CHANGE must bump `scene.version` so a
    // count-only update re-arms the idle present-gate and the badge repaints
    // (mirrors the lifecycle accent).
    let version_before = scene.version;
    assert!(
        scene
            .set_tile_unread_count_checked(tile_id, 5, "agent")
            .is_ok()
    );
    assert_eq!(
        scene.tile_unread_count(tile_id),
        5,
        "the direct write must land on the compositor-read overlay"
    );
    assert!(
        scene.version > version_before,
        "a count change must bump scene.version so a count-only update repaints"
    );

    // A redundant re-write of the SAME count must NOT bump the version — a
    // steady-state portal stays idle.
    let version_steady = scene.version;
    assert!(
        scene
            .set_tile_unread_count_checked(tile_id, 5, "agent")
            .is_ok()
    );
    assert_eq!(
        scene.version, version_steady,
        "re-writing the same count must not bump scene.version"
    );

    // Count 0 clears the badge.
    assert!(
        scene
            .set_tile_unread_count_checked(tile_id, 0, "agent")
            .is_ok()
    );
    assert_eq!(
        scene.tile_unread_count(tile_id),
        0,
        "count 0 clears the badge"
    );

    // Re-set, then prove a cross-namespace write is rejected and leaves the
    // count untouched.
    assert!(
        scene
            .set_tile_unread_count_checked(tile_id, 3, "agent")
            .is_ok()
    );
    let version_before_reject = scene.version;
    assert!(matches!(
        scene.set_tile_unread_count_checked(tile_id, 99, "intruder"),
        Err(ValidationError::NamespaceMismatch { .. })
    ));
    assert_eq!(
        scene.tile_unread_count(tile_id),
        3,
        "a rejected cross-namespace write must not change the count"
    );
    assert_eq!(scene.version, version_before_reject);
    scene.suspend_lease(&lease_id, scene.now_millis()).unwrap();
    let version_suspended = scene.version;
    assert!(matches!(
        scene.set_tile_unread_count_checked(tile_id, 99, "agent"),
        Err(ValidationError::InvalidField { field, .. }) if field == "lease_state"
    ));
    assert_eq!(scene.tile_unread_count(tile_id), 3);
    assert_eq!(scene.version, version_suspended);
    scene.revoke_lease(lease_id).unwrap();
    assert_eq!(scene.tile_unread_count(tile_id), 0);
}

#[test]
fn set_tile_lifecycle_accent_sets_clears_and_cleans_up() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
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

    // Set: the accent lands in overlay state, keyed by tile id, and bumps
    // scene.version so the #943 idle present-gate re-arms for an accent-only
    // transition.
    let accent = LifecycleAccent {
        color: Rgba::new(0.1, 0.2, 0.3, 1.0),
        width_px: 4.0,
    };

    let v_before_set = scene.version;
    assert!(
        scene
            .set_tile_lifecycle_accent_checked(tile_id, accent, "agent")
            .is_ok()
    );
    assert_eq!(scene.tile_lifecycle_accent(tile_id), Some(accent));
    assert!(
        scene.version > v_before_set,
        "setting a new accent must bump scene.version so the present-gate repaints"
    );

    // Redundant set (same accent) is a no-op for the present-gate: version
    // must NOT change, so a steady-state portal stays idle.
    let v_before_redundant = scene.version;

    assert!(
        scene
            .set_tile_lifecycle_accent_checked(tile_id, accent, "agent")
            .is_ok()
    );
    assert_eq!(
        scene.version, v_before_redundant,
        "re-publishing the same accent must not bump version (keeps idle idle)"
    );

    // A different accent replaces the previous value and invalidates the frame.
    let latest = LifecycleAccent {
        width_px: 6.0,
        ..accent
    };
    let v_before_change = scene.version;
    scene
        .set_tile_lifecycle_accent_checked(tile_id, latest, "agent")
        .unwrap();
    assert_eq!(scene.tile_lifecycle_accent(tile_id), Some(latest));
    assert!(scene.version > v_before_change);

    // Clear: None removes it and bumps version (the redaction-CLEAR path).
    let v_before_clear = scene.version;
    assert!(
        scene
            .clear_tile_lifecycle_accent_checked(tile_id, "agent")
            .is_ok()
    );
    assert_eq!(scene.tile_lifecycle_accent(tile_id), None);
    assert!(
        scene.version > v_before_clear,
        "clearing a present accent must bump scene.version (stale accent must repaint away)"
    );

    // Clearing again (nothing stored) must NOT bump version.
    let v_before_noop_clear = scene.version;

    assert!(
        scene
            .clear_tile_lifecycle_accent_checked(tile_id, "agent")
            .is_ok()
    );
    assert_eq!(
        scene.version, v_before_noop_clear,
        "clearing an absent accent must not bump version"
    );

    // Cleanup: deleting the tile drops any accent overlay state.
    assert!(
        scene
            .set_tile_lifecycle_accent_checked(tile_id, accent, "agent")
            .is_ok()
    );
    assert_eq!(scene.tile_lifecycle_accent(tile_id), Some(accent));
    scene.delete_tile(tile_id, "agent").unwrap();
    assert_eq!(
        scene.tile_lifecycle_accent(tile_id),
        None,
        "accent overlay state must be cleaned up on tile deletion"
    );
}

/// Build a leased scene with a portal tile at the display origin whose root is
/// a TextMarkdown "transcript" node — the shape an interaction-enabled portal
/// republishes each streaming render.
fn transcript_scene_with_tile(text: &str) -> (SceneGraph, SceneId) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
            lease_id,
            Rect::new(0.0, 0.0, 300.0, 200.0),
            1,
        )
        .unwrap();
    scene
        .set_tile_root_checked(tile_id, transcript_root(text), "agent")
        .unwrap();
    (scene, tile_id)
}

fn transcript_root(text: &str) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: text.to_string(),
            bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
            font_size_px: 14.0,
            font_family: FontFamily::default(),
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::default(),
            overflow: TextOverflow::default(),
            color_runs: Box::default(),
        }),
    }
}

fn composer_region() -> HitRegionNode {
    HitRegionNode {
        bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
        interaction_id: "portal-42-composer".to_string(),
        accepts_focus: true,
        accepts_pointer: true,
        accepts_composer_input: true,
        ..Default::default()
    }
}

/// Return the tile root's HitRegion child that accepts composer input, if any.
fn composer_child(scene: &SceneGraph, tile_id: SceneId) -> Option<(SceneId, HitRegionNode)> {
    let root_id = scene.tiles.get(&tile_id)?.root_node?;
    let root = scene.nodes.get(&root_id)?;
    root.children
        .iter()
        .find_map(|id| match &scene.nodes.get(id)?.data {
            NodeData::HitRegion(region) if region.accepts_composer_input => {
                Some((*id, region.clone()))
            }
            _ => None,
        })
}

/// Composer identity and local hit state survive streamed transcript replacements.
#[test]
fn composer_interaction_hit_region_survives_consecutive_transcript_republishes() {
    let (mut scene, tile_id) = transcript_scene_with_tile("render 1");

    // Enable interaction: the composer hit region attaches under the root.
    assert!(
        scene
            .set_tile_composer_interaction_checked(tile_id, composer_region(), "agent")
            .is_ok()
    );

    let (composer_id_1, hr1) =
        composer_child(&scene, tile_id).expect("composer hit region must attach on enable");
    assert!(
        hr1.accepts_composer_input,
        "attached composer must accept composer input"
    );
    assert!(
        hr1.accepts_pointer && hr1.accepts_focus,
        "attached composer must accept pointer (click-to-focus)"
    );
    assert_eq!(hr1.interaction_id, "portal-42-composer");
    // Click-to-focus resolves to the composer node via hit_test (the tile sits
    // at the display origin, so display coords == tile-local coords here).
    assert_eq!(
        scene.hit_test(150.0, 100.0),
        HitResult::NodeHit {
            tile_id,
            node_id: composer_id_1,
            interaction_id: "portal-42-composer".to_string(),
        },
        "a click inside the composer must resolve to the composer NodeHit"
    );

    // The pointer rests on the composer (compositor-managed local state).
    scene
        .hit_region_states
        .get_mut(&composer_id_1)
        .unwrap()
        .hovered = true;

    // Republish the transcript root twice (consecutive streaming renders). Each
    // republish replaces the whole node tree; the composer must be re-derived.
    for (i, text) in ["render 2", "render 3"].iter().enumerate() {
        scene
            .set_tile_root_tree_checked(tile_id, transcript_root(text), vec![], "agent")
            .unwrap();

        // The transcript content updated…
        let root_id = scene.tiles.get(&tile_id).unwrap().root_node.unwrap();
        match &scene.nodes.get(&root_id).unwrap().data {
            NodeData::TextMarkdown(tm) => assert_eq!(&tm.content, text),
            other => panic!("root must be TextMarkdown after republish, got {other:?}"),
        }

        // …and the composer hit region SURVIVED, still accepting composer input
        // and still click-to-focus.
        let (composer_id, hr) = composer_child(&scene, tile_id)
            .unwrap_or_else(|| panic!("composer hit region must survive republish {}", i + 2));
        assert_eq!(
            composer_id,
            composer_id_1,
            "the composer keeps its node id (focus and draft key) across republish {}",
            i + 2
        );
        assert!(
            scene.hit_region_states[&composer_id].hovered,
            "the composer keeps its hover state across republish {}",
            i + 2
        );
        assert!(
            hr.accepts_composer_input && hr.accepts_pointer && hr.accepts_focus,
            "composer must still accept composer input after republish {}",
            i + 2
        );
        assert_eq!(
            scene.hit_test(150.0, 100.0),
            HitResult::NodeHit {
                tile_id,
                node_id: composer_id,
                interaction_id: "portal-42-composer".to_string(),
            },
            "composer must remain click-to-focus after republish {}",
            i + 2
        );
    }

    // A changed spec (new bounds) updates the node in place, same id.
    let mut moved = composer_region();
    moved.bounds.y += 10.0;
    scene
        .set_tile_composer_interaction_checked(tile_id, moved.clone(), "agent")
        .unwrap();
    let (composer_id, hr) = composer_child(&scene, tile_id).expect("composer still attached");
    assert_eq!(
        composer_id, composer_id_1,
        "a spec change keeps the composer node id"
    );
    assert_eq!(
        hr.bounds, moved.bounds,
        "a spec change applies the new bounds"
    );

    // Disable interaction: the derived composer node is detached.
    assert!(
        scene
            .clear_tile_composer_interaction_checked(tile_id, "agent")
            .is_ok()
    );
    assert!(
        composer_child(&scene, tile_id).is_none(),
        "disabling interaction must detach the composer hit region"
    );
    assert!(
        !scene.hit_test(150.0, 100.0).is_node_hit(),
        "after disable a click must no longer resolve to a composer NodeHit"
    );
}

/// A checked composer write by an agent that does not own the
/// tile must be rejected (lease/namespace gate), leaving no composer node
/// attached — mirroring the accent's cross-namespace guard.
#[test]
fn set_tile_composer_interaction_rejects_cross_namespace() {
    let (mut scene, tile_id) = transcript_scene_with_tile("render 1");
    // A different namespace with its own lease must not attach a composer.
    let _intruder = scene.grant_lease("intruder", 60_000);

    let version_before = scene.version;
    let nodes_before = scene.nodes.len();
    let result =
        scene.set_tile_composer_interaction_checked(tile_id, composer_region(), "intruder");
    assert!(
        result.is_err(),
        "cross-namespace composer interaction must be rejected"
    );
    assert!(matches!(
        result,
        Err(ValidationError::NamespaceMismatch { .. })
    ));
    assert!(
        composer_child(&scene, tile_id).is_none(),
        "a rejected composer interaction must not attach a hit region"
    );
    assert_eq!(scene.version, version_before);
    assert_eq!(scene.nodes.len(), nodes_before);
    assert!(
        !scene
            .overlay
            .tile_composer_interactions
            .contains_key(&tile_id)
    );
}

/// A checked accent write by an agent that does not own the tile
/// must be rejected with `NamespaceMismatch` and must not mutate the accent
/// overlay state — the accent is a per-tile visual owned by the tile's
/// namespace, exactly like every other tile mutation.
#[test]
fn set_tile_lifecycle_accent_rejects_cross_namespace() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let owner_lease = scene.grant_lease("owner", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "owner",
            owner_lease,
            Rect::new(10.0, 10.0, 200.0, 150.0),
            1,
        )
        .unwrap();

    // Intruder (a different namespace) tries to set the accent on owner's tile.

    let version_before = scene.version;
    let nodes_before = scene.nodes.len();
    let result = scene.set_tile_lifecycle_accent_checked(
        tile_id,
        LifecycleAccent {
            color: Rgba::new(0.9, 0.1, 0.1, 1.0),
            width_px: 4.0,
        },
        "intruder",
    );
    assert!(
        result.is_err(),
        "cross-namespace accent write must be rejected"
    );
    assert!(matches!(
        result,
        Err(ValidationError::NamespaceMismatch { .. })
    ));
    assert_eq!(
        scene.tile_lifecycle_accent(tile_id),
        None,
        "rejected cross-namespace write must not mutate accent overlay state"
    );
    assert_eq!(scene.version, version_before);
    assert_eq!(scene.nodes.len(), nodes_before);
}

// ── Portal surface ────────────────────────────────────────────────

/// Set up a tab + lease + tile whose root is a known SolidColor node, and
/// return `(scene, tile_id, root_node_id)`. Mirrors the raw-tile pilot: the
/// tile's content node is what a portal part points at.
fn portal_scene_with_tile() -> (SceneGraph, SceneId, SceneId) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "agent",
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
    scene.set_tile_root_checked(tile_id, root, "agent").unwrap();
    (scene, tile_id, root_id)
}

/// The Phase-0 raw-tile assembly is expressible via the promoted surface:
/// `set_portal_surface` declaring all eight named parts lands in overlay
/// state, bumps `scene.version`, and reads back verbatim.
#[test]
fn set_portal_surface_expresses_all_eight_parts() {
    let (mut scene, tile_id, root_id) = portal_scene_with_tile();

    // All eight parts; the transcript part points at the tile's content node
    // (raw-tile expression), the rest carry geometry only.
    let parts: Vec<PortalPart> = PortalPartKind::ALL
        .iter()
        .enumerate()
        .map(|(i, &kind)| PortalPart {
            kind,
            bounds: Rect::new(0.0, i as f32 * 10.0, 400.0, 10.0),
            node: if kind == PortalPartKind::Transcript {
                Some(root_id)
            } else {
                None
            },
        })
        .collect();
    let surface = PortalSurface {
        identity: PortalIdentity {
            session_id: "sess-1".to_string(),
            display_name: "Claude".to_string(),
            peer_class: PortalPeerClass::ResidentLlm,
        },
        lifecycle: PortalLifecycleState::Active,
        display_state: PortalDisplayState::Expanded,
        parts,
    };

    let v_before = scene.version;

    assert!(
        scene
            .set_portal_surface(tile_id, surface.clone(), "agent")
            .is_ok(),
        "direct portal declaration must apply"
    );
    assert_eq!(scene.portal_surface(tile_id), Some(&surface));
    assert!(
        scene.version > v_before,
        "declaring a portal surface must bump scene.version"
    );

    // Redundant re-declare of the same surface is a version no-op (idle stays idle).
    let v_before_redundant = scene.version;

    assert!(scene.set_portal_surface(tile_id, surface, "agent").is_ok());
    assert_eq!(scene.version, v_before_redundant);

    // Cleanup on tile deletion.
    scene.delete_tile(tile_id, "agent").unwrap();
    assert_eq!(
        scene.portal_surface(tile_id),
        None,
        "portal surface overlay state must be cleaned up on tile deletion"
    );
}

/// `update_portal_surface_state` patches: a `None` field is left unchanged and
/// a redundant patch does not bump `scene.version`.
#[test]
fn update_portal_surface_state_patches_and_coalesces() {
    let (mut scene, tile_id, _root_id) = portal_scene_with_tile();

    assert!(
        scene
            .set_portal_surface(
                tile_id,
                PortalSurface {
                    lifecycle: PortalLifecycleState::Active,
                    display_state: PortalDisplayState::Expanded,
                    ..Default::default()
                },
                "agent"
            )
            .is_ok()
    );

    // Patch only display_state; lifecycle (None) is left unchanged.
    let v_before = scene.version;
    assert!(
        scene
            .update_portal_surface_state(
                tile_id,
                None,
                Some(PortalDisplayState::Collapsed),
                "agent"
            )
            .is_ok()
    );
    let s = scene.portal_surface(tile_id).unwrap();
    assert_eq!(
        s.lifecycle,
        PortalLifecycleState::Active,
        "lifecycle unchanged"
    );
    assert_eq!(
        s.display_state,
        PortalDisplayState::Collapsed,
        "display patched"
    );
    assert!(
        scene.version > v_before,
        "a real state change bumps version"
    );

    // Redundant patch (same values) is a version no-op.
    let v_before_redundant = scene.version;

    assert!(
        scene
            .update_portal_surface_state(
                tile_id,
                None,
                Some(PortalDisplayState::Collapsed),
                "agent"
            )
            .is_ok()
    );
    assert_eq!(scene.version, v_before_redundant);
    scene
        .update_portal_surface_state(tile_id, Some(PortalLifecycleState::Blocked), None, "agent")
        .unwrap();
    let latest = scene.portal_surface(tile_id).unwrap();
    assert_eq!(latest.lifecycle, PortalLifecycleState::Blocked);
    assert_eq!(latest.display_state, PortalDisplayState::Collapsed);
    let version_noop = scene.version;
    scene
        .update_portal_surface_state(tile_id, None, None, "agent")
        .unwrap();
    assert_eq!(scene.version, version_noop);
}

/// Patching a tile that has no declared portal surface is rejected with
/// `PortalSurfaceNotFound` (you must `set_portal_surface` first).
#[test]
fn update_portal_surface_state_requires_prior_surface() {
    let (mut scene, tile_id, _root_id) = portal_scene_with_tile();

    let version_before = scene.version;
    let nodes_before = scene.nodes.len();
    let result = scene.update_portal_surface_state(
        tile_id,
        Some(PortalLifecycleState::Blocked),
        None,
        "agent",
    );
    assert!(result.is_err());
    assert!(matches!(
        result,
        Err(ValidationError::PortalSurfaceNotFound { .. })
    ));
    assert_eq!(scene.version, version_before);
    assert_eq!(scene.nodes.len(), nodes_before);
    assert!(scene.portal_surface(tile_id).is_none());
}

/// A portal part referencing a node that is not in the host tile's tree is
/// rejected with `InvalidPortalSurface` (a surface cannot borrow foreign nodes).
#[test]
fn set_portal_surface_rejects_foreign_part_node() {
    let (mut scene, tile_id, _root_id) = portal_scene_with_tile();

    let version_before = scene.version;
    let nodes_before = scene.nodes.len();
    let result = scene.set_portal_surface(
        tile_id,
        PortalSurface {
            parts: vec![PortalPart {
                kind: PortalPartKind::Transcript,
                bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                node: Some(SceneId::new()), // not in the tile
            }],
            ..Default::default()
        },
        "agent",
    );
    assert!(result.is_err());
    assert!(matches!(
        result,
        Err(ValidationError::InvalidPortalSurface { .. })
    ));
    assert_eq!(scene.version, version_before);
    assert_eq!(scene.nodes.len(), nodes_before);
    assert!(scene.portal_surface(tile_id).is_none());
}

/// Cross-namespace `set_portal_surface` is rejected and mutates no overlay state.
#[test]
fn set_portal_surface_rejects_cross_namespace() {
    let (mut scene, tile_id, _root_id) = portal_scene_with_tile();

    let version_before = scene.version;
    let nodes_before = scene.nodes.len();
    let result = scene.set_portal_surface(tile_id, PortalSurface::default(), "intruder");
    assert!(result.is_err());
    assert!(matches!(
        result,
        Err(ValidationError::NamespaceMismatch { .. })
    ));
    assert_eq!(scene.version, version_before);
    assert_eq!(scene.nodes.len(), nodes_before);
    assert!(scene.portal_surface(tile_id).is_none());
}

/// review P2: the descriptor survives a content republish, but a
/// part node reference that pointed at the now-removed root must not dangle —
/// `SetTileRoot` prunes it back to `None` so consumers never resolve a stale
/// `SceneId`.
#[test]
fn portal_part_node_refs_pruned_on_root_republish() {
    let (mut scene, tile_id, root_id) = portal_scene_with_tile();

    // Declare a surface whose transcript part points at the current root node.
    assert!(
        scene
            .set_portal_surface(
                tile_id,
                PortalSurface {
                    parts: vec![PortalPart {
                        kind: PortalPartKind::Transcript,
                        bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                        node: Some(root_id),
                    }],
                    ..Default::default()
                },
                "agent"
            )
            .is_ok()
    );
    assert_eq!(
        scene.portal_surface(tile_id).unwrap().parts[0].node,
        Some(root_id),
        "part must reference the declared node before republish"
    );

    // Republish the tile root with a fresh node tree (the old root is removed).
    let new_root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.1, 0.1, 0.1, 1.0),
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            radius: None,
        }),
    };
    let new_root_id = new_root.id;
    scene
        .set_tile_root_tree_checked(tile_id, new_root, vec![], "agent")
        .unwrap();

    // The descriptor survives, but the dangling ref to the removed old root is
    // pruned to None (not left pointing at a node no longer in the graph, and
    // not silently repointed at the new root).
    let part = &scene.portal_surface(tile_id).unwrap().parts[0];
    assert_eq!(
        part.node, None,
        "stale part node ref must be pruned to None on root republish"
    );
    assert_ne!(
        part.node,
        Some(new_root_id),
        "prune must not silently adopt the new root as the part's node"
    );
}

/// review (Gemini): a portal part with negative width/height is
/// rejected as an invalid portal surface, mirroring the non-negative extent
/// expected of tile/node bounds.
#[test]
fn set_portal_surface_rejects_negative_part_bounds() {
    let (mut scene, tile_id, _root_id) = portal_scene_with_tile();

    let version_before = scene.version;
    let nodes_before = scene.nodes.len();
    let result = scene.set_portal_surface(
        tile_id,
        PortalSurface {
            parts: vec![PortalPart {
                kind: PortalPartKind::Frame,
                bounds: Rect::new(0.0, 0.0, -10.0, 20.0),
                node: None,
            }],
            ..Default::default()
        },
        "agent",
    );
    assert!(
        result.is_err(),
        "negative-extent part bounds must be rejected"
    );
    assert!(matches!(
        result,
        Err(ValidationError::InvalidPortalSurface { .. })
    ));
    assert_eq!(
        scene.portal_surface(tile_id),
        None,
        "rejected write must not store overlay state"
    );
    assert_eq!(scene.version, version_before);
    assert_eq!(scene.nodes.len(), nodes_before);
}
