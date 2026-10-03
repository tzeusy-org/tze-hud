//! Text stream portal governance and shell-isolation validation (hud-t98e.4).
//!
//! Focus: lease lifecycle, safe-mode/freeze behavior, and shell isolation
//! constraints for the phase-0 raw-tile portal pilot.
//!
//! The second half of this file (hud-8z3w3, RFC 0013 §7.2 promotion) mirrors
//! every governance contract above over the promoted first-class `PortalSurface`
//! attached to the same host tile — proving lease ownership, safe mode, freeze,
//! dismissal, and orphan handling hold UNCHANGED on the first-class surface,
//! and that it stays content-layer below chrome.

use std::sync::Arc;

use tze_hud_runtime::{
    ChromeState, EnqueueResult, FreezeQueue, MutationTrafficClass, QueuedMutation,
    classify_mutation_batch, collect_diagnostic,
};
use tze_hud_scene::{
    Clock, SceneGraph, SceneId, TestClock, ZONE_TILE_Z_MIN,
    lease::{LeaseState, ORPHAN_GRACE_PERIOD_MS},
    mutation::{MutationBatch, SceneMutation},
    types::{
        FontFamily, Node, NodeData, PortalDisplayState, PortalIdentity, PortalLifecycleState,
        PortalPart, PortalPartKind, PortalPeerClass, PortalSurface, Rect, Rgba, SolidColorNode,
        TextAlign, TextMarkdownNode, TextOverflow,
    },
};

const DISPLAY_W: f32 = 1920.0;
const DISPLAY_H: f32 = 1080.0;
const PORTAL_BOUNDS: Rect = Rect {
    x: 64.0,
    y: 132.0,
    width: 720.0,
    height: 320.0,
};

fn make_batch(namespace: &str, lease_id: SceneId, mutations: Vec<SceneMutation>) -> MutationBatch {
    MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: namespace.to_string(),
        mutations,
        timing_hints: None,
        lease_id: Some(lease_id),
    }
}

fn portal_root(transcript: &str) -> (Node, Vec<Node>) {
    let root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.09, 0.11, 0.14, 0.9),
            bounds: Rect::new(0.0, 0.0, PORTAL_BOUNDS.width, PORTAL_BOUNDS.height),
            radius: None,
        }),
    };
    let transcript_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: transcript.to_string(),
            bounds: Rect::new(
                16.0,
                44.0,
                PORTAL_BOUNDS.width - 32.0,
                PORTAL_BOUNDS.height - 84.0,
            ),
            font_size_px: 13.0,
            font_family: FontFamily::SystemMonospace,
            color: Rgba::new(0.92, 0.95, 1.0, 0.98),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    (root, vec![transcript_node])
}

fn root_batch_for_tile(tile_id: SceneId, root: Node, children: Vec<Node>) -> Vec<SceneMutation> {
    let root_id = root.id;
    let mut mutations = vec![SceneMutation::SetTileRoot {
        tile_id,
        node: root,
        descendants: vec![],
    }];
    mutations.extend(children.into_iter().map(|node| SceneMutation::AddNode {
        tile_id,
        parent_id: Some(root_id),
        node,
    }));
    mutations
}

fn create_portal_scene(ttl_ms: u64) -> (SceneGraph, TestClock, SceneId, SceneId, SceneId) {
    let clock = TestClock::new(1_000);
    let mut scene = SceneGraph::new_with_clock(DISPLAY_W, DISPLAY_H, Arc::new(clock.clone()));
    let namespace = "portal-gov";
    let tab_id = scene.create_tab("Main", 0).expect("tab");
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(namespace, ttl_ms);

    let create = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds: PORTAL_BOUNDS,
            z_order: 180,
        }],
    ));
    assert!(create.applied, "portal tile create must apply");
    let tile_id = create.created_ids[0];

    let (root, children) = portal_root("line 0");
    let set_root = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        root_batch_for_tile(tile_id, root, children),
    ));
    assert!(set_root.applied, "initial portal transcript must apply");

    (scene, clock, tab_id, lease_id, tile_id)
}

fn portal_text(scene: &SceneGraph, tile_id: SceneId) -> String {
    let tile = scene.tiles.get(&tile_id).expect("tile exists");
    let root_id = tile.root_node.expect("root exists");
    let root = scene.nodes.get(&root_id).expect("root node exists");
    for child in &root.children {
        let node = scene.nodes.get(child).expect("child exists");
        if let NodeData::TextMarkdown(t) = &node.data {
            return t.content.clone();
        }
    }
    panic!("portal root must contain transcript TextMarkdown node");
}

#[test]
fn lease_expiry_removes_portal_tile() {
    let (mut scene, clock, _tab_id, lease_id, tile_id) = create_portal_scene(20);
    clock.advance(25);

    let expiries = scene.expire_leases();
    assert!(
        expiries.iter().any(|e| e.lease_id == lease_id),
        "lease expiry list must include portal lease"
    );
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Expired,
        "lease must be terminal after ttl expiry"
    );
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "portal tile must be removed on lease expiry"
    );
}

#[test]
fn lease_revocation_removes_portal_tile() {
    let (mut scene, _clock, _tab_id, lease_id, tile_id) = create_portal_scene(120_000);
    scene.revoke_lease(lease_id).expect("revoke lease");

    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Revoked,
        "lease must transition to revoked"
    );
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "revoked portal lease must remove tile"
    );
}

#[test]
fn orphaned_portal_freezes_and_grace_expiry_removes_tile() {
    let (mut scene, clock, _tab_id, lease_id, tile_id) = create_portal_scene(120_000);
    let before = portal_text(&scene, tile_id);

    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect lease");
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Orphaned,
        "disconnect must transition lease to orphaned"
    );
    assert!(
        scene.tiles.contains_key(&tile_id),
        "orphaned portal stays visible through grace period"
    );

    let (root, children) = portal_root("line after disconnect");
    let rejected = scene.apply_batch(&make_batch(
        "portal-gov",
        lease_id,
        root_batch_for_tile(tile_id, root, children),
    ));
    assert!(
        !rejected.applied,
        "orphaned lease must reject further portal updates"
    );
    assert_eq!(
        portal_text(&scene, tile_id),
        before,
        "portal surface must freeze at last coherent transcript state"
    );

    clock.advance(ORPHAN_GRACE_PERIOD_MS + 1);
    let expiries = scene.expire_leases();
    assert!(
        expiries.iter().any(|e| e.lease_id == lease_id),
        "orphaned lease must expire after grace timeout"
    );
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Expired);
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "portal tile must be removed when orphan grace expires"
    );
}

#[test]
fn safe_mode_suspend_blocks_portal_updates_until_resume() {
    let (mut scene, clock, _tab_id, lease_id, tile_id) = create_portal_scene(120_000);
    let baseline = portal_text(&scene, tile_id);

    scene
        .suspend_lease(&lease_id, clock.now_millis())
        .expect("suspend lease");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Suspended);

    let (root, children) = portal_root("line while safe mode active");
    let rejected = scene.apply_batch(&make_batch(
        "portal-gov",
        lease_id,
        root_batch_for_tile(tile_id, root, children),
    ));
    assert!(
        !rejected.applied,
        "suspended lease must reject updates while safe mode is active"
    );
    assert_eq!(portal_text(&scene, tile_id), baseline);

    scene
        .resume_lease(&lease_id, clock.now_millis() + 1)
        .expect("resume lease");
    let (root, children) = portal_root("line after safe mode exit");
    let accepted = scene.apply_batch(&make_batch(
        "portal-gov",
        lease_id,
        root_batch_for_tile(tile_id, root, children),
    ));
    assert!(
        accepted.applied,
        "portal updates should resume after safe mode exit"
    );
    assert_eq!(portal_text(&scene, tile_id), "line after safe mode exit");
}

#[test]
fn freeze_path_uses_generic_backpressure_signal_not_portal_specific_signal() {
    let mut queue = FreezeQueue::new(1);
    let first = queue.enqueue(QueuedMutation {
        batch_id: b"tx-1".to_vec(),
        original_batch_id: b"tx-1".to_vec(),
        traffic_class: MutationTrafficClass::Transactional,
        coalesce_key: None,
        submitted_at_wall_us: 1,
        payload: b"tx-1".to_vec(),
    });
    assert!(matches!(
        first,
        EnqueueResult::Queued | EnqueueResult::QueuedWithPressure
    ));

    let second = queue.enqueue(QueuedMutation {
        batch_id: b"tx-2".to_vec(),
        original_batch_id: b"tx-2".to_vec(),
        traffic_class: MutationTrafficClass::Transactional,
        coalesce_key: None,
        submitted_at_wall_us: 2,
        payload: b"tx-2".to_vec(),
    });
    assert!(
        matches!(second, EnqueueResult::BackpressureRequired { .. }),
        "freeze overflow should produce generic backpressure for transactional batches"
    );

    let proto = include_str!("../../crates/tze_hud_protocol/proto/session.proto");
    assert!(
        !proto.contains("PORTAL_FREEZE"),
        "protocol must not expose a portal-specific freeze signal"
    );
}

#[test]
fn shell_status_snapshot_exposes_no_portal_identity_or_transcript() {
    let (scene, _clock, _tab_id, _lease_id, tile_id) = create_portal_scene(120_000);
    let transcript = portal_text(&scene, tile_id);
    let mut chrome = ChromeState::new();
    chrome.connected_agent_count = 1;
    chrome.add_tab(1, "portal://agent/session".to_string());
    chrome.add_tab(2, format!("preview:{transcript}"));
    assert!(
        chrome.tabs.iter().any(|tab| tab.name.contains("portal://")),
        "test setup must include portal identity in chrome input state"
    );
    assert!(
        chrome.tabs.iter().any(|tab| tab.name.contains(&transcript)),
        "test setup must include transcript-like content in chrome input state"
    );

    let snapshot = collect_diagnostic(&chrome, 123_456, scene.leases.len());
    assert_eq!(
        snapshot.tab_count, 2,
        "snapshot should still report tab count"
    );
    let text = snapshot.to_string();

    assert!(
        !text.contains("portal://"),
        "shell diagnostics must not expose portal identity"
    );
    assert!(
        !text.contains(&transcript),
        "shell diagnostics must not expose transcript-derived content"
    );
}

#[test]
fn shell_dismiss_override_removes_portal_tile() {
    let (mut scene, _clock, _tab_id, lease_id, tile_id) = create_portal_scene(120_000);
    // The hover close button's viewer-dismiss entry reclaims the tile's lease.
    let expiry = scene
        .viewer_dismiss_tile(tile_id)
        .expect("viewer dismiss reclaims the portal lease");
    assert_eq!(expiry.lease_id, lease_id);
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "shell override dismiss must remove portal tile"
    );
}

// ─── First-class PortalSurface governance parity (hud-8z3w3) ──────────────────
//
// The RFC 0013 §7.2 promotion replaces the ad-hoc raw-tile assembly with a
// single first-class `PortalSurface` overlay descriptor keyed by the host tile
// id. Promotion must not add or relax governance: every contract asserted above
// on the RAW-TILE path must hold UNCHANGED once a first-class surface is
// attached to the same host tile. The tests below MIRROR the raw-tile suite
// one-for-one over a tile that carries a declared `PortalSurface`, and add the
// content-layer proof.
//
// Governance for the surface lives on the host tile's lease exactly as the raw
// tiles do: the surface holds no capability, no lease, and no z-order of its
// own (`PortalSurface` has no z field — it inherits the host tile's layer).

/// Stable identity carried on the declared first-class surface. Both strings are
/// deliberately shaped like leakable secrets (a `portal://` session URI and a
/// human display name) so the shell-isolation test can prove neither escapes.
const FC_SESSION_ID: &str = "portal://agent/first-class-session";
const FC_DISPLAY_NAME: &str = "First-Class Portal Peer";

/// Resolve the transcript `TextMarkdown` node id inside the host tile so a
/// `PortalPart` can reference it (raw-tile expression: the promoted surface
/// points its Transcript part at the existing bounded-viewport content node).
fn transcript_node_id(scene: &SceneGraph, tile_id: SceneId) -> SceneId {
    let tile = scene.tiles.get(&tile_id).expect("tile exists");
    let root_id = tile.root_node.expect("root exists");
    let root = scene.nodes.get(&root_id).expect("root node exists");
    for child in &root.children {
        let node = scene.nodes.get(child).expect("child exists");
        if matches!(node.data, NodeData::TextMarkdown(_)) {
            return *child;
        }
    }
    panic!("portal root must contain transcript TextMarkdown node");
}

/// Build the first-class surface descriptor: identity + Active/Expanded state +
/// a Frame part (full-bounds geometry) and a Transcript part pointing at the
/// tile's existing content node.
fn first_class_surface(transcript_node: SceneId) -> PortalSurface {
    PortalSurface {
        identity: PortalIdentity {
            session_id: FC_SESSION_ID.to_string(),
            display_name: FC_DISPLAY_NAME.to_string(),
            peer_class: PortalPeerClass::ResidentLlm,
        },
        lifecycle: PortalLifecycleState::Active,
        display_state: PortalDisplayState::Expanded,
        parts: vec![
            PortalPart {
                kind: PortalPartKind::Frame,
                bounds: Rect::new(0.0, 0.0, PORTAL_BOUNDS.width, PORTAL_BOUNDS.height),
                node: None,
            },
            PortalPart {
                kind: PortalPartKind::Transcript,
                bounds: Rect::new(
                    16.0,
                    44.0,
                    PORTAL_BOUNDS.width - 32.0,
                    PORTAL_BOUNDS.height - 84.0,
                ),
                node: Some(transcript_node),
            },
        ],
    }
}

/// Same headless portal scene as `create_portal_scene`, but with a first-class
/// `PortalSurface` declared over the host tile via `SetPortalSurface`.
fn create_first_class_portal_scene(
    ttl_ms: u64,
) -> (SceneGraph, TestClock, SceneId, SceneId, SceneId) {
    let (mut scene, clock, tab_id, lease_id, tile_id) = create_portal_scene(ttl_ms);
    let transcript_node = transcript_node_id(&scene, tile_id);
    let set = scene.apply_batch(&make_batch(
        "portal-gov",
        lease_id,
        vec![SceneMutation::SetPortalSurface {
            tile_id,
            surface: first_class_surface(transcript_node),
        }],
    ));
    assert!(
        set.applied,
        "first-class portal surface declaration must apply"
    );
    assert!(
        scene.portal_surface(tile_id).is_some(),
        "surface descriptor must be attached to the host tile"
    );
    (scene, clock, tab_id, lease_id, tile_id)
}

#[test]
fn first_class_surface_pruned_on_lease_expiry() {
    let (mut scene, clock, _tab_id, lease_id, tile_id) = create_first_class_portal_scene(20);
    assert!(scene.portal_surface(tile_id).is_some());
    clock.advance(25);

    let expiries = scene.expire_leases();
    assert!(
        expiries.iter().any(|e| e.lease_id == lease_id),
        "lease expiry list must include portal lease"
    );
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Expired);
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "host tile must be removed on lease expiry"
    );
    assert!(
        scene.portal_surface(tile_id).is_none(),
        "first-class surface overlay must be pruned with its host tile on expiry"
    );
}

#[test]
fn first_class_surface_pruned_on_lease_revocation() {
    let (mut scene, _clock, _tab_id, lease_id, tile_id) = create_first_class_portal_scene(120_000);
    scene.revoke_lease(lease_id).expect("revoke lease");

    assert_eq!(scene.leases[&lease_id].state, LeaseState::Revoked);
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "revoked lease must remove the host tile"
    );
    assert!(
        scene.portal_surface(tile_id).is_none(),
        "first-class surface overlay must be pruned on revocation"
    );
}

#[test]
fn orphaned_first_class_surface_freezes_and_grace_expiry_prunes_surface() {
    let (mut scene, clock, _tab_id, lease_id, tile_id) = create_first_class_portal_scene(120_000);

    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect lease");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);
    assert!(
        scene.tiles.contains_key(&tile_id),
        "orphaned portal stays visible through the grace window"
    );
    // The surface descriptor is preserved across the grace window so a reconnect
    // can resume it (identity/lifecycle/geometry survive) — same as any tile.
    assert!(
        scene.portal_surface(tile_id).is_some(),
        "surface descriptor must survive the orphan grace window"
    );

    // An orphaned lease must reject further surface mutations of BOTH shapes
    // (structural declare + coalescible state patch), and freeze the descriptor
    // at its last coherent state.
    let patch_rejected = scene.apply_batch(&make_batch(
        "portal-gov",
        lease_id,
        vec![SceneMutation::UpdatePortalSurfaceState {
            tile_id,
            lifecycle: Some(PortalLifecycleState::Blocked),
            display_state: None,
        }],
    ));
    assert!(
        !patch_rejected.applied,
        "orphaned lease must reject UpdatePortalSurfaceState"
    );
    let transcript_node = transcript_node_id(&scene, tile_id);
    let declare_rejected = scene.apply_batch(&make_batch(
        "portal-gov",
        lease_id,
        vec![SceneMutation::SetPortalSurface {
            tile_id,
            surface: first_class_surface(transcript_node),
        }],
    ));
    assert!(
        !declare_rejected.applied,
        "orphaned lease must reject SetPortalSurface"
    );
    assert_eq!(
        scene
            .portal_surface(tile_id)
            .expect("surface present")
            .lifecycle,
        PortalLifecycleState::Active,
        "surface must freeze at its last coherent lifecycle while orphaned"
    );

    clock.advance(ORPHAN_GRACE_PERIOD_MS + 1);
    let expiries = scene.expire_leases();
    assert!(
        expiries.iter().any(|e| e.lease_id == lease_id),
        "orphaned lease must expire after grace timeout"
    );
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "host tile removed when orphan grace expires"
    );
    assert!(
        scene.portal_surface(tile_id).is_none(),
        "first-class surface overlay must be pruned via the same orphan path"
    );
}

#[test]
fn safe_mode_suspend_blocks_first_class_surface_mutations_until_resume() {
    let (mut scene, clock, _tab_id, lease_id, tile_id) = create_first_class_portal_scene(120_000);

    scene
        .suspend_lease(&lease_id, clock.now_millis())
        .expect("suspend lease");
    assert_eq!(scene.leases[&lease_id].state, LeaseState::Suspended);

    // Both surface mutation shapes are blocked while safe mode holds the lease.
    let patch_rejected = scene.apply_batch(&make_batch(
        "portal-gov",
        lease_id,
        vec![SceneMutation::UpdatePortalSurfaceState {
            tile_id,
            lifecycle: Some(PortalLifecycleState::Blocked),
            display_state: Some(PortalDisplayState::Collapsed),
        }],
    ));
    assert!(
        !patch_rejected.applied,
        "suspended lease must reject UpdatePortalSurfaceState"
    );
    let transcript_node = transcript_node_id(&scene, tile_id);
    let declare_rejected = scene.apply_batch(&make_batch(
        "portal-gov",
        lease_id,
        vec![SceneMutation::SetPortalSurface {
            tile_id,
            surface: first_class_surface(transcript_node),
        }],
    ));
    assert!(
        !declare_rejected.applied,
        "suspended lease must reject SetPortalSurface"
    );
    let frozen = scene.portal_surface(tile_id).expect("surface present");
    assert_eq!(frozen.lifecycle, PortalLifecycleState::Active);
    assert_eq!(frozen.display_state, PortalDisplayState::Expanded);

    // Exiting safe mode restores the surface mutation path.
    scene
        .resume_lease(&lease_id, clock.now_millis() + 1)
        .expect("resume lease");
    let accepted = scene.apply_batch(&make_batch(
        "portal-gov",
        lease_id,
        vec![SceneMutation::UpdatePortalSurfaceState {
            tile_id,
            lifecycle: Some(PortalLifecycleState::WaitingForInput),
            display_state: None,
        }],
    ));
    assert!(
        accepted.applied,
        "surface updates must resume after safe mode exit"
    );
    assert_eq!(
        scene
            .portal_surface(tile_id)
            .expect("surface present")
            .lifecycle,
        PortalLifecycleState::WaitingForInput,
        "resumed patch must land"
    );
}

#[test]
fn freeze_path_governs_first_class_surface_via_generic_queue() {
    // Derive the traffic class through the REAL classifier rather than hardcoding
    // it — a hardcoded `Transactional` label would let this test pass even if the
    // classifier routed `set_portal_surface` as an evictable state-stream (which
    // would silently drop a surface DECLARATION under freeze pressure). The
    // classifier is the contract boundary; assert it, then feed its verdict into
    // the queue.
    let declare_class = classify_mutation_batch(&["set_portal_surface"]);
    assert_eq!(
        declare_class,
        MutationTrafficClass::Transactional,
        "SetPortalSurface (structural declare) must classify Transactional so it \
         is never evicted under freeze pressure"
    );
    let patch_class = classify_mutation_batch(&["update_portal_surface_state"]);
    assert_eq!(
        patch_class,
        MutationTrafficClass::StateStream,
        "UpdatePortalSurfaceState (lifecycle/display patch) must classify \
         StateStream so it stays on the coalescible path"
    );

    // `SetPortalSurface` is Transactional; it must ride the same generic freeze
    // queue as any transactional batch and overflow into a generic backpressure
    // signal — not a portal-specific one.
    let mut queue = FreezeQueue::new(1);
    let first = queue.enqueue(QueuedMutation {
        batch_id: b"set-portal-surface-1".to_vec(),
        original_batch_id: b"set-portal-surface-1".to_vec(),
        traffic_class: declare_class,
        coalesce_key: None,
        submitted_at_wall_us: 1,
        payload: b"SetPortalSurface".to_vec(),
    });
    assert!(matches!(
        first,
        EnqueueResult::Queued | EnqueueResult::QueuedWithPressure
    ));
    let overflow = queue.enqueue(QueuedMutation {
        batch_id: b"set-portal-surface-2".to_vec(),
        original_batch_id: b"set-portal-surface-2".to_vec(),
        traffic_class: declare_class,
        coalesce_key: None,
        submitted_at_wall_us: 2,
        payload: b"SetPortalSurface".to_vec(),
    });
    assert!(
        matches!(overflow, EnqueueResult::BackpressureRequired { .. }),
        "surface declare overflow must produce generic backpressure"
    );

    // `UpdatePortalSurfaceState` is a coalescible StateStream patch; under freeze
    // it must coalesce latest-wins on the generic queue, exactly like the
    // lifecycle accent / unread-count paths — no portal-specific freeze channel.
    let mut coalescing = FreezeQueue::new(4);
    let key = "portal-surface-state:tile-fc".to_string();
    let queued = coalescing.enqueue(QueuedMutation {
        batch_id: b"update-state-1".to_vec(),
        original_batch_id: b"update-state-1".to_vec(),
        traffic_class: patch_class,
        coalesce_key: Some(key.clone()),
        submitted_at_wall_us: 3,
        payload: b"lifecycle=Blocked".to_vec(),
    });
    assert!(matches!(
        queued,
        EnqueueResult::Queued | EnqueueResult::QueuedWithPressure
    ));
    let coalesced = coalescing.enqueue(QueuedMutation {
        batch_id: b"update-state-2".to_vec(),
        original_batch_id: b"update-state-2".to_vec(),
        traffic_class: patch_class,
        coalesce_key: Some(key),
        submitted_at_wall_us: 4,
        payload: b"lifecycle=WaitingForInput".to_vec(),
    });
    assert!(
        matches!(coalesced, EnqueueResult::Coalesced),
        "surface state patches must coalesce latest-wins on the generic freeze queue"
    );

    // No portal- or surface-specific freeze signal was minted for the promotion.
    let proto = include_str!("../../crates/tze_hud_protocol/proto/session.proto");
    assert!(
        !proto.contains("PORTAL_FREEZE") && !proto.contains("SURFACE_FREEZE"),
        "protocol must not expose a portal/surface-specific freeze signal"
    );
}

#[test]
fn shell_snapshot_exposes_no_first_class_surface_identity_or_transcript() {
    let (scene, _clock, _tab_id, _lease_id, tile_id) = create_first_class_portal_scene(120_000);
    let transcript = portal_text(&scene, tile_id);
    // Sanity: the surface really does hold the leakable identity strings.
    let surface = scene.portal_surface(tile_id).expect("surface present");
    assert_eq!(surface.identity.session_id, FC_SESSION_ID);
    assert_eq!(surface.identity.display_name, FC_DISPLAY_NAME);

    let mut chrome = ChromeState::new();
    chrome.connected_agent_count = 1;
    chrome.add_tab(1, FC_SESSION_ID.to_string());
    chrome.add_tab(2, format!("preview:{transcript}"));

    let snapshot = collect_diagnostic(&chrome, 123_456, scene.leases.len());
    let text = snapshot.to_string();
    assert!(
        !text.contains("portal://") && !text.contains(FC_DISPLAY_NAME),
        "shell diagnostics must not expose first-class surface identity"
    );
    assert!(
        !text.contains(&transcript),
        "shell diagnostics must not expose transcript-derived content"
    );
}

#[test]
fn shell_dismiss_override_prunes_first_class_surface() {
    let (mut scene, _clock, _tab_id, lease_id, tile_id) = create_first_class_portal_scene(120_000);
    // Viewer dismiss reclaims the lease; the first-class surface overlay must be
    // pruned with the tile, leaving no dangling descriptor.
    let expiry = scene
        .viewer_dismiss_tile(tile_id)
        .expect("viewer dismiss reclaims the portal lease");
    assert_eq!(expiry.lease_id, lease_id);
    assert!(
        !scene.tiles.contains_key(&tile_id),
        "shell override dismiss must remove the host tile"
    );
    assert!(
        scene.portal_surface(tile_id).is_none(),
        "shell override dismiss must prune the first-class surface overlay"
    );
}

#[test]
fn first_class_surface_stays_content_layer_below_chrome() {
    let (scene, _clock, _tab_id, _lease_id, tile_id) = create_first_class_portal_scene(120_000);

    // The host tile stays strictly below the runtime-managed zone/chrome band.
    let tile = scene.tiles.get(&tile_id).expect("tile exists");
    assert!(
        tile.z_order < ZONE_TILE_Z_MIN,
        "surface host tile must stay below the reserved zone/chrome z band"
    );
    // The surface holds no z-order of its own: it is a descriptor over the tile,
    // inheriting the tile's layer. This is a structural invariant of the schema
    // (there is no z field on PortalSurface/PortalPart), proven here by the tile
    // continuing to own the only z-order after the surface is attached.
    assert_eq!(
        tile.z_order, 180,
        "attaching a surface must not change the host tile's declared z-order"
    );
}
