//! Full lifecycle integration tests for the exemplar dashboard tile.
//!
//! Implements acceptance criteria for `hud-i6yd.8` (tasks.md §12, spec.md
//! §Requirement: Full Lifecycle User-Test Scenario, §Requirement: Headless
//! Test Coverage).
//!
//! # Test inventory
//!
//! ## §12.1 / spec §Full Lifecycle User-Test Scenario
//!
//! - [`full_lifecycle_connect_lease_upload_create_update_refresh_dismiss`]
//!   End-to-end happy path: session connect → lease → resource upload →
//!   atomic tile creation → content update → Refresh click → Dismiss click →
//!   tile cleanly removed from scene graph.
//!
//! ## §12.2 / spec §Disconnect-during-lifecycle
//!
//! - [`disconnect_during_lifecycle_orphans_tile_with_badge`]
//!   Agent disconnects after tile creation (before Dismiss); tile enters ORPHANED
//!   state with `DisconnectionBadge` visual hint.
//!
//! - [`grace_period_expiry_removes_tile_after_disconnect`]
//!   After disconnect + 30 s grace period elapses, lease transitions to EXPIRED
//!   and tile is removed from scene graph.
//!
//! ## Namespace isolation
//!
//! - [`second_agent_cannot_mutate_dashboard_tile`]
//!   A second agent cannot mutate the first agent's dashboard tile (rejected
//!   with NamespaceMismatch).
//!
//! - [`dashboard_agent_cannot_mutate_foreign_namespace_tile`]
//!   The dashboard agent cannot mutate a tile owned by a different namespace
//!   (rejected with NamespaceMismatch).
//!
//! ## §12.3 / spec §Headless Test Coverage
//!
//! - [`headless_tile_creation_produces_6_nodes_in_correct_tree_order`]
//!   MutationBatch injection (no GPU) produces scene graph with 6 nodes in
//!   correct tree order.
//!
//! - [`headless_pointer_down_at_refresh_bounds_returns_node_hit`]
//!   Synthetic point query at Refresh button bounds returns `HitResult::NodeHit`
//!   with `interaction_id = "refresh-button"`.
//!
//! - [`headless_lease_expiry_advances_to_expired_and_removes_tile`]
//!   Simulated time advancement past lease TTL transitions state to EXPIRED
//!   and removes the tile without a display server or GPU.
//!
//! All tests are headless Layer 0: no display server or GPU required.
//!
//! Source:
//!   openspec/changes/exemplar-dashboard-tile/tasks.md §12
//!   openspec/changes/exemplar-dashboard-tile/specs/exemplar-dashboard-tile/spec.md
//!     §Requirement: Full Lifecycle User-Test Scenario (lines 262-279)
//!     §Requirement: Headless Test Coverage (lines 242-258)

use std::sync::Arc;

use image::{ImageBuffer, Rgb};
use tze_hud_resource::{
    AgentBudget, CAPABILITY_UPLOAD_RESOURCE, ResourceStore, ResourceStoreConfig, ResourceType,
    UploadId, UploadStartRequest,
};
use tze_hud_scene::{
    Clock, HitResult, LeaseState, ResourceId, SceneGraph, SceneId, TestClock,
    lease::{ORPHAN_GRACE_PERIOD_MS, TileVisualHint},
    mutation::{MutationBatch, SceneMutation},
    types::{
        FontFamily, HitRegionNode, ImageFitMode, InputMode, LeaseExpiry, Node, NodeData, Rect,
        Rgba, SolidColorNode, StaticImageNode, TextAlign, TextMarkdownNode, TextOverflow,
    },
};

// ─── Display constants ────────────────────────────────────────────────────────

const DISPLAY_W: f32 = 1920.0;
const DISPLAY_H: f32 = 1080.0;

// ─── Spec-mandated dashboard tile geometry ────────────────────────────────────

const TILE_X: f32 = 50.0;
const TILE_Y: f32 = 50.0;
const TILE_W: f32 = 400.0;
const TILE_H: f32 = 300.0;
const TILE_Z_ORDER: u32 = 100;

// ─── Icon dimensions (spec §Resource Upload Before Tile Creation) ─────────────

const ICON_W: u32 = 48;
const ICON_H: u32 = 48;

// ─── Spec-mandated node geometry ─────────────────────────────────────────────

const BG_COLOR: Rgba = Rgba {
    r: 0.07,
    g: 0.07,
    b: 0.07,
    a: 0.90,
};

const ICON_X: f32 = 16.0;
const ICON_Y: f32 = 16.0;

const HEADER_X: f32 = 76.0;
const HEADER_Y: f32 = 20.0;
const HEADER_W: f32 = 308.0;
const HEADER_H: f32 = 32.0;

const BODY_X: f32 = 16.0;
const BODY_Y: f32 = 72.0;
const BODY_W: f32 = 368.0;
const BODY_H: f32 = 180.0;

const REFRESH_X: f32 = 16.0;
const REFRESH_Y: f32 = 256.0;
const REFRESH_W: f32 = 176.0;
const REFRESH_H: f32 = 36.0;
const REFRESH_ID: &str = "refresh-button";

const DISMISS_X: f32 = 208.0;
const DISMISS_Y: f32 = 256.0;
const DISMISS_W: f32 = 176.0;
const DISMISS_H: f32 = 36.0;
const DISMISS_ID: &str = "dismiss-button";

const DASHBOARD_NS: &str = "dashboard-agent";

// ─── Test fixture: PNG generator ─────────────────────────────────────────────

/// Generate a valid 48×48 solid-color RGB PNG for use as the icon fixture.
fn make_icon_png() -> Vec<u8> {
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_fn(ICON_W, ICON_H, |_, _| Rgb([70u8, 130, 180]));
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .expect("PNG encoding must not fail for a solid-color image");
    buf
}

// ─── Resource upload helper ───────────────────────────────────────────────────

fn unlimited_budget() -> AgentBudget {
    AgentBudget {
        texture_bytes_total_limit: 0,
        texture_bytes_total_used: 0,
    }
}

/// Upload a PNG and return `(raw_hash_bytes, scene_ResourceId)`.
async fn upload_icon_png(store: &ResourceStore, namespace: &str) -> ([u8; 32], ResourceId) {
    let png = make_icon_png();
    let resource_id_raw = tze_hud_resource::types::ResourceId::from_content(&png);
    let upload_id = UploadId::from_bytes(uuid::Uuid::now_v7().into_bytes());
    let result = store
        .handle_upload_start(UploadStartRequest {
            agent_namespace: namespace.to_string(),
            agent_capabilities: vec![CAPABILITY_UPLOAD_RESOURCE.to_string()],
            agent_budget: unlimited_budget(),
            upload_id,
            resource_type: ResourceType::ImagePng,
            expected_hash: *resource_id_raw.as_bytes(),
            total_size: png.len(),
            inline_data: png,
            width: ICON_W,
            height: ICON_H,
        })
        .await
        .expect("upload_start must succeed")
        .expect("inline upload must return ResourceStored immediately");
    let resource_id = ResourceId::from_bytes(*result.resource_id.as_bytes());
    (*resource_id_raw.as_bytes(), resource_id)
}

// ─── Node builders ────────────────────────────────────────────────────────────

fn make_bg_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: BG_COLOR,
            bounds: Rect::new(0.0, 0.0, TILE_W, TILE_H),
            radius: None,
        }),
    }
}

fn make_bg_root_with_children(
    icon_id: SceneId,
    header_id: SceneId,
    body_id: SceneId,
    refresh_id: SceneId,
    dismiss_id: SceneId,
) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![icon_id, header_id, body_id, refresh_id, dismiss_id],
        data: NodeData::SolidColor(SolidColorNode {
            color: BG_COLOR,
            bounds: Rect::new(0.0, 0.0, TILE_W, TILE_H),
            radius: None,
        }),
    }
}

fn make_icon_node(resource_id: ResourceId) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: ICON_W,
            height: ICON_H,
            decoded_bytes: (ICON_W * ICON_H * 4) as u64,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(ICON_X, ICON_Y, ICON_W as f32, ICON_H as f32),
        }),
    }
}

fn make_header_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "**Dashboard Agent**".to_string(),
            bounds: Rect::new(HEADER_X, HEADER_Y, HEADER_W, HEADER_H),
            font_size_px: 18.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    }
}

fn make_body_node(content: &str) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_string(),
            bounds: Rect::new(BODY_X, BODY_Y, BODY_W, BODY_H),
            font_size_px: 14.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba {
                r: 0.78,
                g: 0.78,
                b: 0.78,
                a: 1.0,
            },
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    }
}

fn make_refresh_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(REFRESH_X, REFRESH_Y, REFRESH_W, REFRESH_H),
            interaction_id: REFRESH_ID.to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            auto_capture: true,
            release_on_up: true,
            tooltip: Box::new(Some("Refresh dashboard content".to_string())),
            ..Default::default()
        }),
    }
}

fn make_dismiss_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(DISMISS_X, DISMISS_Y, DISMISS_W, DISMISS_H),
            interaction_id: DISMISS_ID.to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            auto_capture: true,
            release_on_up: true,
            tooltip: Box::new(Some("Dismiss this tile".to_string())),
            ..Default::default()
        }),
    }
}

// ─── Batch helpers ────────────────────────────────────────────────────────────

fn make_batch(
    namespace: &str,
    lease_id: Option<SceneId>,
    mutations: Vec<SceneMutation>,
) -> MutationBatch {
    MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: namespace.to_string(),
        mutations,
        timing_hints: None,
        lease_id,
    }
}

// ─── Scene setup helpers ──────────────────────────────────────────────────────

/// Create a scene with a tab and a dashboard agent lease.
/// Returns `(scene, tab_id, lease_id)`.
fn setup_scene() -> (SceneGraph, SceneId, SceneId) {
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(DASHBOARD_NS, 120_000);
    (scene, tab_id, lease_id)
}

/// Create a scene backed by a shared `TestClock`.
/// Returns `(scene, clock, tab_id, lease_id)`.
fn setup_scene_with_clock() -> (SceneGraph, TestClock, SceneId, SceneId) {
    let clock = TestClock::new(1_000);
    let mut scene = SceneGraph::new_with_clock(DISPLAY_W, DISPLAY_H, Arc::new(clock.clone()));
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(DASHBOARD_NS, 60_000);
    (scene, clock, tab_id, lease_id)
}

/// Apply the atomic 6-node initial batch.
///
/// Returns `(bg_node_id, MutationResult)`.
fn apply_initial_node_batch(
    scene: &mut SceneGraph,
    tile_id: SceneId,
    lease_id: SceneId,
    resource_id: ResourceId,
    body_content: &str,
) -> (SceneId, tze_hud_scene::mutation::MutationResult) {
    let bg = make_bg_node();
    let icon = make_icon_node(resource_id);
    let header = make_header_node();
    let body = make_body_node(body_content);
    let refresh = make_refresh_node();
    let dismiss = make_dismiss_node();
    let bg_id = bg.id;

    let result = scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![
            SceneMutation::AddNode {
                tile_id,
                parent_id: None,
                node: bg,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(bg_id),
                node: icon,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(bg_id),
                node: header,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(bg_id),
                node: body,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(bg_id),
                node: refresh,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(bg_id),
                node: dismiss,
            },
            SceneMutation::UpdateTileOpacity {
                tile_id,
                opacity: 1.0,
            },
            SceneMutation::UpdateTileInputMode {
                tile_id,
                input_mode: InputMode::Passthrough,
            },
        ],
    ));
    (bg_id, result)
}

/// Apply a periodic content update via SetTileRoot (full 6-node tree swap).
fn apply_content_update(
    scene: &mut SceneGraph,
    tile_id: SceneId,
    lease_id: SceneId,
    resource_id: ResourceId,
    body_content: &str,
) -> tze_hud_scene::mutation::MutationResult {
    let new_icon = make_icon_node(resource_id);
    let new_header = make_header_node();
    let new_body = make_body_node(body_content);
    let new_refresh = make_refresh_node();
    let new_dismiss = make_dismiss_node();
    let new_bg = make_bg_root_with_children(
        new_icon.id,
        new_header.id,
        new_body.id,
        new_refresh.id,
        new_dismiss.id,
    );

    scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![
            SceneMutation::AddNode {
                tile_id,
                parent_id: None,
                node: new_icon,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: None,
                node: new_header,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: None,
                node: new_body,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: None,
                node: new_refresh,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: None,
                node: new_dismiss,
            },
            SceneMutation::SetTileRoot {
                tile_id,
                node: new_bg,
                descendants: vec![],
            },
        ],
    ))
}

// ═══════════════════════════════════════════════════════════════════════════════
// §12.1 — Full Lifecycle User-Test Scenario (spec lines 262-270)
// ═══════════════════════════════════════════════════════════════════════════════

/// Spec §Requirement: Full Lifecycle User-Test Scenario
/// Scenario: End-to-end lifecycle completes successfully
///
/// WHEN the full lifecycle is executed in order:
///   session connect → lease request → resource upload → atomic tile creation
///   → content update → Refresh click → Dismiss click
/// THEN each step shall succeed, the agent receives the expected events and
///   responses at each step, and the tile shall be cleanly removed on Dismiss.
///
/// The gRPC session handshake and LeaseResponse are exercised in detail by
/// the session-server tests in `tze_hud_protocol` and `tests/event_delivery.rs`. This test
/// proxies the session outcome (a granted lease) and focuses on the scene-layer
/// lifecycle that underpins those interactions.
///
/// tasks.md §12.1
#[tokio::test]
async fn full_lifecycle_connect_lease_upload_create_update_refresh_dismiss() {
    // ── Phase 1: Connect + Lease (session proxy) ──────────────────────────────
    let (mut scene, tab_id, lease_id) = setup_scene();

    let lease = scene
        .leases
        .get(&lease_id)
        .expect("lease must exist after grant");
    assert_eq!(
        lease.state,
        LeaseState::Active,
        "Phase 1: lease must be ACTIVE after grant"
    );

    // ── Phase 2: Resource Upload ──────────────────────────────────────────────
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let (raw_hash, resource_id) = upload_icon_png(&store, DASHBOARD_NS).await;

    // Verify BLAKE3 ResourceId is 32 bytes.
    assert_eq!(
        raw_hash.len(),
        blake3::OUT_LEN,
        "Phase 2: BLAKE3 hash must be 32 bytes"
    );

    // Register the uploaded resource so agent-submitted StaticImageNode mutations succeed.
    scene.register_resource(resource_id);

    // ── Phase 3: Atomic Tile Creation ─────────────────────────────────────────
    let create_result = scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: DASHBOARD_NS.into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(
        create_result.applied,
        "Phase 3a: CreateTile must be accepted; rejection: {:?}",
        create_result.rejection
    );
    assert_eq!(
        create_result.created_ids.len(),
        1,
        "Phase 3a: exactly 1 tile_id must be returned"
    );
    let tile_id = create_result.created_ids[0];

    let (bg_id, node_result) = apply_initial_node_batch(
        &mut scene,
        tile_id,
        lease_id,
        resource_id,
        "Status: OK\nUptime: 0s",
    );
    assert!(
        node_result.applied,
        "Phase 3b: 6-node batch must be accepted; rejection: {:?}",
        node_result.rejection
    );

    // Verify 6 nodes, tile root, opacity, input_mode.
    assert_eq!(
        scene.node_count(),
        6,
        "Phase 3: scene must have exactly 6 nodes"
    );
    {
        let tile = scene.tiles.get(&tile_id).expect("tile must exist");
        assert_eq!(
            tile.root_node,
            Some(bg_id),
            "Phase 3: tile root must be bg node"
        );
        assert_eq!(tile.opacity, 1.0, "Phase 3: tile opacity must be 1.0");
        assert_eq!(
            tile.input_mode,
            InputMode::Passthrough,
            "Phase 3: tile input_mode must be Passthrough"
        );
        assert_eq!(
            tile.z_order, TILE_Z_ORDER,
            "Phase 3: tile z_order must be {TILE_Z_ORDER}"
        );
    }

    // ── Phase 4: Periodic Content Update ─────────────────────────────────────
    let update_result = apply_content_update(
        &mut scene,
        tile_id,
        lease_id,
        resource_id,
        "Status: OK\nUptime: 5s",
    );
    assert!(
        update_result.applied,
        "Phase 4: content update (SetTileRoot) must be accepted; rejection: {:?}",
        update_result.rejection
    );
    // SetTileRoot replaces the old 6-node tree with a new 6-node tree.
    assert_eq!(
        scene.node_count(),
        6,
        "Phase 4: scene must still have exactly 6 nodes after content update"
    );

    // Verify new body text.
    {
        let updated_tile = scene
            .tiles
            .get(&tile_id)
            .expect("tile must still exist after update");
        let new_root_id = updated_tile
            .root_node
            .expect("tile must still have a root node");
        let new_root = scene
            .nodes
            .get(&new_root_id)
            .expect("new root node must exist");
        // children[2] = body TextMarkdownNode (painter's model: StaticImage, Header, Body, ...)
        assert!(
            new_root.children.len() > 2,
            "Phase 4: new root node must have at least 3 children; found {}",
            new_root.children.len()
        );
        let body_cid = new_root.children[2];
        let body = scene
            .nodes
            .get(&body_cid)
            .expect("Phase 4: body node must exist");
        let NodeData::TextMarkdown(ref tm) = body.data else {
            panic!("Phase 4: body node at children[2] must be a TextMarkdownNode");
        };
        assert!(
            tm.content.contains("5s"),
            "Phase 4: body TextMarkdownNode must contain updated uptime '5s'"
        );
    }

    // ── Phase 5: Refresh click → agent submits content update ─────────────────
    // In the live system the runtime dispatches ClickEvent with
    // interaction_id="refresh-button". Here we simulate the agent's response:
    // another SetTileRoot content update (event delivery tested in tze_hud_protocol/tests/event_delivery.rs).
    let refresh_result = apply_content_update(
        &mut scene,
        tile_id,
        lease_id,
        resource_id,
        "Status: OK\nUptime: 10s — refreshed",
    );
    assert!(
        refresh_result.applied,
        "Phase 5: Refresh-triggered content update must be accepted; rejection: {:?}",
        refresh_result.rejection
    );
    assert_eq!(
        scene.node_count(),
        6,
        "Phase 5: node count must remain 6 after Refresh callback update"
    );

    // ── Phase 6: Dismiss click → LeaseRelease → tile removed ─────────────────
    assert_eq!(
        scene.tile_count(),
        1,
        "Phase 6: one tile must exist before dismiss"
    );
    scene
        .revoke_lease(lease_id)
        .expect("Phase 6: LeaseRelease (revoke_lease) must succeed");

    assert_eq!(
        scene.tile_count(),
        0,
        "Phase 6: tile must be removed from scene after LeaseRelease"
    );
    assert_eq!(
        scene.node_count(),
        0,
        "Phase 6: all nodes must be removed after LeaseRelease"
    );

    let released_lease = scene
        .leases
        .get(&lease_id)
        .expect("lease record must still exist");
    assert!(
        released_lease.state.is_terminal(),
        "Phase 6: lease must be in a terminal state after release; got {:?}",
        released_lease.state
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// §12.2a — Disconnect during lifecycle: orphan path with badge
// ═══════════════════════════════════════════════════════════════════════════════

/// Spec §Requirement: Full Lifecycle User-Test Scenario
/// Scenario: Disconnect during lifecycle triggers orphan path
///
/// WHEN the agent's session disconnects unexpectedly after tile creation but
///   before Dismiss
/// THEN the tile SHALL enter orphan state with a disconnection badge.
///
/// tasks.md §12.2 (orphan path)
#[test]
fn disconnect_during_lifecycle_orphans_tile_with_badge() {
    let (mut scene, clock, tab_id, lease_id) = setup_scene_with_clock();

    // Create the dashboard tile and give it a minimal root node.
    let create_result = scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: DASHBOARD_NS.into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(create_result.applied, "CreateTile must be accepted");
    let tile_id = create_result.created_ids[0];

    // Minimal root (single SolidColor node — lifecycle focus, not node content).
    let root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: BG_COLOR,
            bounds: Rect::new(0.0, 0.0, TILE_W, TILE_H),
            radius: None,
        }),
    };
    let set_result = scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![SceneMutation::SetTileRoot {
            tile_id,
            node: root,
            descendants: vec![],
        }],
    ));
    assert!(set_result.applied, "SetTileRoot must be accepted");
    assert_eq!(scene.tile_count(), 1, "one tile exists before disconnect");

    // ── Simulate agent disconnect ─────────────────────────────────────────────
    let now_ms = clock.now_millis();
    scene
        .disconnect_lease(&lease_id, now_ms)
        .expect("disconnect_lease must succeed for an active lease");

    // Lease must be ORPHANED.
    let lease = scene.leases.get(&lease_id).expect("lease must exist");
    assert!(
        matches!(lease.state, LeaseState::Orphaned),
        "lease must be ORPHANED after agent disconnect; got {:?}",
        lease.state
    );

    // Tile must still exist (frozen state during grace period).
    assert_eq!(
        scene.tile_count(),
        1,
        "tile must still exist during grace period (frozen state)"
    );

    // Disconnection badge must be set on the tile.
    let tile = scene.tiles.get(&tile_id).expect("tile must still exist");
    assert_eq!(
        tile.visual_hint,
        TileVisualHint::DisconnectionBadge,
        "tile must show DisconnectionBadge visual hint after disconnect"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// §12.2b — Grace period expiry removes tile after disconnect
// ═══════════════════════════════════════════════════════════════════════════════

/// Spec §Requirement: Lease Orphan Handling on Disconnect
/// Scenario: Grace period expiry removes tile
///
/// WHEN the agent fails to reconnect within 30 seconds
/// THEN the lease SHALL transition to EXPIRED and the dashboard tile SHALL be
///   removed from the scene graph.
///
/// tasks.md §12.2 (grace period expiry path)
#[test]
fn grace_period_expiry_removes_tile_after_disconnect() {
    let (mut scene, clock, tab_id, lease_id) = setup_scene_with_clock();

    // Set up a tile.
    let create_result = scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: DASHBOARD_NS.into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(create_result.applied, "CreateTile must be accepted");
    let tile_id = create_result.created_ids[0];

    let root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: BG_COLOR,
            bounds: Rect::new(0.0, 0.0, TILE_W, TILE_H),
            radius: None,
        }),
    };
    scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![SceneMutation::SetTileRoot {
            tile_id,
            node: root,
            descendants: vec![],
        }],
    ));

    // Disconnect.
    scene
        .disconnect_lease(&lease_id, clock.now_millis())
        .expect("disconnect_lease must succeed");
    assert_eq!(
        scene.tile_count(),
        1,
        "tile must still exist at disconnect time"
    );

    // ── Advance clock past grace period ───────────────────────────────────────
    // ORPHAN_GRACE_PERIOD_MS = 30_000 ms; add 1 ms margin.
    clock.advance(ORPHAN_GRACE_PERIOD_MS + 1);

    let expiries: Vec<LeaseExpiry> = scene.expire_leases();

    assert_eq!(
        expiries.len(),
        1,
        "exactly one lease must expire after grace period"
    );
    assert_eq!(
        expiries[0].lease_id, lease_id,
        "the dashboard agent's lease must be the one that expired"
    );
    assert_eq!(
        expiries[0].terminal_state,
        LeaseState::Expired,
        "terminal_state must be EXPIRED after grace period"
    );
    assert!(
        expiries[0].removed_tiles.contains(&tile_id),
        "tile_id must be in removed_tiles"
    );

    assert_eq!(
        scene.tile_count(),
        0,
        "tile must be removed after grace period expiry"
    );
    assert_eq!(
        scene.node_count(),
        0,
        "all nodes must be removed after grace period expiry"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Namespace isolation — §11.1 / spec §Full Lifecycle: Namespace isolation during
// lifecycle (lines 276-278)
// ═══════════════════════════════════════════════════════════════════════════════

/// Spec §Requirement: Full Lifecycle User-Test Scenario
/// Scenario: Namespace isolation during lifecycle
///
/// WHEN the dashboard agent creates its tile
/// THEN no other agent session SHALL be able to mutate or delete the dashboard
///   tile, and attempts SHALL be rejected.
///
/// tasks.md §11.1, §12 namespace isolation clause
#[test]
fn second_agent_cannot_mutate_dashboard_tile() {
    let (mut scene, tab_id, lease_id) = setup_scene();

    // Dashboard agent creates a tile.
    let create_result = scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: DASHBOARD_NS.into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(
        create_result.applied,
        "CreateTile must succeed for dashboard agent"
    );
    let tile_id = create_result.created_ids[0];

    // Second agent acquires a separate lease.
    let intruder_ns = "intruder-agent";
    let intruder_lease = scene.grant_lease(intruder_ns, 120_000);

    // Intruder attempts to add a node to the dashboard agent's tile.
    let intrusion_result = scene.apply_batch(&make_batch(
        intruder_ns,
        Some(intruder_lease),
        vec![SceneMutation::AddNode {
            tile_id,
            parent_id: None,
            node: Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba {
                        r: 1.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    },
                    bounds: Rect::new(0.0, 0.0, TILE_W, TILE_H),
                    radius: None,
                }),
            },
        }],
    ));

    assert!(
        !intrusion_result.applied,
        "intruder AddNode must be rejected (namespace isolation)"
    );
    // Scene unchanged: still 0 nodes in the tile.
    assert_eq!(
        scene.node_count(),
        0,
        "node count must be unchanged after rejected intrusion"
    );
    assert_eq!(scene.tile_count(), 1, "dashboard tile must still exist");
}

/// Spec §Requirement: Namespace isolation
/// Scenario: Dashboard agent cannot mutate foreign tile
///
/// WHEN the dashboard agent attempts to mutate a tile owned by another namespace
/// THEN the mutation SHALL be rejected.
///
/// tasks.md §11.2
#[test]
fn dashboard_agent_cannot_mutate_foreign_namespace_tile() {
    let (mut scene, tab_id, lease_id) = setup_scene();

    // Another agent creates a tile.
    let other_ns = "other-agent";
    let other_lease = scene.grant_lease(other_ns, 120_000);
    let other_create = scene.apply_batch(&make_batch(
        other_ns,
        Some(other_lease),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: other_ns.into(),
            lease_id: other_lease,
            bounds: Rect::new(500.0, 50.0, 200.0, 100.0),
            z_order: 200,
        }],
    ));
    assert!(other_create.applied, "other agent CreateTile must succeed");
    let other_tile_id = other_create.created_ids[0];

    // Dashboard agent attempts to set the root of the other agent's tile.
    let cross_mutation = scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![SceneMutation::SetTileRoot {
            tile_id: other_tile_id,
            node: Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: BG_COLOR,
                    bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                    radius: None,
                }),
            },
            descendants: vec![],
        }],
    ));

    assert!(
        !cross_mutation.applied,
        "dashboard agent must not be able to mutate foreign namespace tile"
    );
    assert_eq!(
        scene.node_count(),
        0,
        "node count must be unchanged after rejected cross-namespace mutation"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// §12.3a — Headless tile creation: 6 nodes in correct tree order (no GPU)
// ═══════════════════════════════════════════════════════════════════════════════

/// Spec §Requirement: Headless Test Coverage
/// Scenario: Headless tile creation test
///
/// WHEN a Layer 0 test creates the dashboard tile via injected MutationBatch
/// THEN the scene graph SHALL contain the tile with all 6 nodes in the correct
///   tree order, verifiable without a GPU.
///
/// tasks.md §12.3
#[tokio::test]
async fn headless_tile_creation_produces_6_nodes_in_correct_tree_order() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let (_, resource_id) = upload_icon_png(&store, DASHBOARD_NS).await;

    let (mut scene, tab_id, lease_id) = setup_scene();
    // Register the uploaded resource so agent-submitted StaticImageNode mutations succeed.
    scene.register_resource(resource_id);

    // CreateTile.
    let create_result = scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: DASHBOARD_NS.into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(create_result.applied, "CreateTile must succeed");
    let tile_id = create_result.created_ids[0];

    // 6-node atomic batch.
    let (bg_id, node_result) =
        apply_initial_node_batch(&mut scene, tile_id, lease_id, resource_id, "**Status**: OK");
    assert!(
        node_result.applied,
        "6-node batch must succeed; rejection: {:?}",
        node_result.rejection
    );

    // ── Verify 6 nodes in correct tree order ──────────────────────────────────
    //
    // Expected painter's model order:
    //   root = SolidColorNode (bg)
    //   children[0] = StaticImageNode   (icon)
    //   children[1] = TextMarkdownNode  (header)
    //   children[2] = TextMarkdownNode  (body)
    //   children[3] = HitRegionNode     (refresh)
    //   children[4] = HitRegionNode     (dismiss)

    assert_eq!(scene.node_count(), 6, "scene must contain exactly 6 nodes");

    let tile = scene.tiles.get(&tile_id).expect("tile must exist");
    assert_eq!(tile.root_node, Some(bg_id), "tile root must be bg node");

    let root = scene.nodes.get(&bg_id).expect("root node must exist");
    assert!(
        matches!(root.data, NodeData::SolidColor(_)),
        "root must be SolidColorNode"
    );
    assert_eq!(root.children.len(), 5, "root must have exactly 5 children");

    let expected_types = [
        "StaticImage",
        "TextMarkdown",
        "TextMarkdown",
        "HitRegion",
        "HitRegion",
    ];
    for (i, &expected) in expected_types.iter().enumerate() {
        let child = scene
            .nodes
            .get(&root.children[i])
            .expect("child node must exist");
        let actual = match &child.data {
            NodeData::SolidColor(_) => "SolidColor",
            NodeData::StaticImage(_) => "StaticImage",
            NodeData::TextMarkdown(_) => "TextMarkdown",
            NodeData::HitRegion(_) => "HitRegion",
        };
        assert_eq!(
            actual, expected,
            "child[{i}] must be {expected}, got {actual}"
        );
    }

    // Verify HitRegionNode interaction_ids.
    let NodeData::HitRegion(ref hr) = scene.nodes[&root.children[3]].data else {
        panic!("child[3] must be a HitRegionNode");
    };
    assert_eq!(
        hr.interaction_id, REFRESH_ID,
        "child[3] must have interaction_id=refresh-button"
    );

    let NodeData::HitRegion(ref hr) = scene.nodes[&root.children[4]].data else {
        panic!("child[4] must be a HitRegionNode");
    };
    assert_eq!(
        hr.interaction_id, DISMISS_ID,
        "child[4] must have interaction_id=dismiss-button"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// §12.3b — Headless input test: pointer at Refresh bounds returns NodeHit
// ═══════════════════════════════════════════════════════════════════════════════

/// Spec §Requirement: Headless Test Coverage
/// Scenario: Headless input test
///
/// WHEN a Layer 0 test injects a synthetic PointerDownEvent at coordinates
///   within the "Refresh" HitRegionNode bounds
/// THEN the hit-test SHALL return a NodeHit for the Refresh node with
///   interaction_id = "refresh-button".
///
/// Uses `SceneGraph::hit_test` directly (pure Rust, no GPU, no display server).
///
/// tasks.md §12.3 (headless input test)
#[test]
fn headless_pointer_down_at_refresh_bounds_returns_node_hit() {
    let (mut scene, tab_id, lease_id) = setup_scene();

    // Create tile with a root bg node, plus Refresh and Dismiss HitRegionNodes.
    // No icon upload needed — the hit-test operates on node bounds only.
    let create_result = scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: DASHBOARD_NS.into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(create_result.applied, "CreateTile must succeed");
    let tile_id = create_result.created_ids[0];

    let bg = make_bg_node();
    let bg_id = bg.id;
    let refresh = make_refresh_node();
    let refresh_node_id = refresh.id;
    let dismiss = make_dismiss_node();

    scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![
            SceneMutation::AddNode {
                tile_id,
                parent_id: None,
                node: bg,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(bg_id),
                node: refresh,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(bg_id),
                node: dismiss,
            },
        ],
    ));

    // ── Inject a synthetic pointer hit at the centre of the Refresh button ────
    //
    // Display-space coordinates: tile_origin + node_local_offset + half_button_size
    let ptr_x = TILE_X + REFRESH_X + REFRESH_W / 2.0; // 50 + 16 + 88 = 154
    let ptr_y = TILE_Y + REFRESH_Y + REFRESH_H / 2.0; // 50 + 256 + 18 = 324

    let hit = scene.hit_test(ptr_x, ptr_y);

    match hit {
        HitResult::NodeHit {
            tile_id: hit_tile_id,
            node_id,
            interaction_id,
        } => {
            assert_eq!(
                hit_tile_id, tile_id,
                "hit tile_id must match the dashboard tile"
            );
            assert_eq!(
                node_id, refresh_node_id,
                "hit node_id must be the Refresh HitRegionNode"
            );
            assert_eq!(
                interaction_id, REFRESH_ID,
                "hit interaction_id must be 'refresh-button'"
            );
        }
        other => panic!("Expected NodeHit for Refresh button, got: {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// §12.3c — Headless lease expiry: time past TTL removes tile (no GPU)
// ═══════════════════════════════════════════════════════════════════════════════

/// Spec §Requirement: Headless Test Coverage
/// Scenario: Headless lease expiry test
///
/// WHEN a Layer 0 test simulates time advancement past the lease TTL without
///   renewal
/// THEN the lease SHALL transition to EXPIRED and the tile SHALL be removed
///   from the scene graph.
///
/// tasks.md §12.3 (headless lease expiry)
#[test]
fn headless_lease_expiry_advances_to_expired_and_removes_tile() {
    let clock = TestClock::new(1_000);
    let mut scene = SceneGraph::new_with_clock(DISPLAY_W, DISPLAY_H, Arc::new(clock.clone()));
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);

    // Grant a short-TTL lease (500 ms) so we can advance past it cheaply.
    const SHORT_TTL_MS: u64 = 500;
    let lease_id = scene.grant_lease(DASHBOARD_NS, SHORT_TTL_MS);

    // Create a tile under this lease.
    let create_result = scene.apply_batch(&make_batch(
        DASHBOARD_NS,
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: DASHBOARD_NS.into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(create_result.applied, "CreateTile must succeed");
    let tile_id = create_result.created_ids[0];

    // Before TTL elapses: no expiry.
    let before_expiries = scene.expire_leases();
    assert_eq!(before_expiries.len(), 0, "no expiries before TTL elapses");
    assert_eq!(scene.tile_count(), 1, "tile must exist before TTL");

    // ── Advance clock past TTL ────────────────────────────────────────────────
    clock.advance(SHORT_TTL_MS + 1);
    let expiries: Vec<LeaseExpiry> = scene.expire_leases();

    assert_eq!(expiries.len(), 1, "exactly one lease must expire after TTL");
    assert_eq!(
        expiries[0].lease_id, lease_id,
        "the expired lease must be the dashboard agent's lease"
    );
    assert_eq!(
        expiries[0].terminal_state,
        LeaseState::Expired,
        "terminal_state must be EXPIRED"
    );
    assert!(
        expiries[0].removed_tiles.contains(&tile_id),
        "tile_id must appear in removed_tiles"
    );

    assert_eq!(
        scene.tile_count(),
        0,
        "tile must be removed after TTL expiry"
    );
    assert_eq!(
        scene.node_count(),
        0,
        "all nodes must be removed after TTL expiry"
    );
}
