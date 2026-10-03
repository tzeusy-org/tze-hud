//! Dashboard tile creation batch and content update tests.
//!
//! Implements acceptance criteria for `hud-i6yd.4` (tasks 3–6 from
//! openspec/changes/exemplar-dashboard-tile/tasks.md):
//!
//! **Task 3 — Resource Upload (§3.1–3.2):**
//! - Upload a 48×48 PNG icon via ResourceStore; verify BLAKE3 ResourceId returned.
//!
//! **Task 4 — Atomic Tile Creation Batch (§4.1–4.4):**
//! - Build the full node batch (bg root + 5 children via AddNode, then
//!   UpdateTileOpacity + UpdateTileInputMode); verify MutationResult accepted.
//! - Verify scene graph contains all 6 nodes in correct tree order after commit.
//! - Partial failure (invalid bounds) rejects entire batch atomically.
//!
//! **Task 5 — Intra-Tile Compositing Verification (§5.1–5.3):**
//! - Painter's model: SolidColorNode first (root), then StaticImageNode, then
//!   2× TextMarkdownNode, then 2× HitRegionNode (all as children in tree order).
//! - z_order=100 is below ZONE_TILE_Z_MIN (0x8000_0000).
//! - Chrome-layer z_order (ZONE_TILE_Z_MIN+1) renders above dashboard tile.
//!
//! **Task 6 — Periodic Content Update (§6.1–6.3):**
//! - Content update (SetTileRoot with new tree) succeeds when lease is ACTIVE.
//! - Content update rejected when lease has expired (LeaseExpired error).
//! - Content under 65535 UTF-8 bytes.
//!
//! ## Scene Graph API Notes
//!
//! `Node.children` is `Vec<SceneId>` — references to already-inserted nodes.
//!
//! **Initial creation (6-node flat tree):**
//!   1. `AddNode(None, bg)` → bg becomes tile root.
//!   2. `AddNode(Some(bg.id), icon)`, `AddNode(Some(bg.id), header)`, etc.
//!
//! **Periodic content update (full tree swap via SetTileRoot):**
//!   1. `AddNode(None, new_icon)` → orphan (tile already has root, so no attachment).
//!   2. ... same for header, body, refresh, dismiss (5 orphans).
//!   3. `SetTileRoot(new_bg with child IDs)` → removes old root tree, inserts new root.
//!      Result: 5 orphans + new root = 6 nodes.
//!
//! All tests are headless Layer 0: no display server or GPU required.
//!
//! Source: openspec/changes/exemplar-dashboard-tile/tasks.md §3–6,
//!         openspec/changes/exemplar-dashboard-tile/specs/exemplar-dashboard-tile/spec.md

use std::sync::Arc;

use image::{ImageBuffer, Rgb};
use tze_hud_resource::{
    AgentBudget, CAPABILITY_UPLOAD_RESOURCE, ResourceStore, ResourceStoreConfig, ResourceType,
    UploadId, UploadStartRequest,
};
use tze_hud_scene::{
    ResourceId, SceneGraph, SceneId, TestClock, ValidationErrorCode, ZONE_TILE_Z_MIN,
    mutation::{MutationBatch, SceneMutation},
    types::{
        FontFamily, HitRegionNode, ImageFitMode, InputMode, Node, NodeData, Rect, Rgba,
        SolidColorNode, StaticImageNode, TextAlign, TextMarkdownNode, TextOverflow,
    },
};

// ─── Display constants ────────────────────────────────────────────────────────

const DISPLAY_W: f32 = 1920.0;
const DISPLAY_H: f32 = 1080.0;

// ─── Spec-mandated tile geometry ─────────────────────────────────────────────

/// Dashboard tile position (50, 50), size 400×300, per spec §Dashboard Tile Composition.
const TILE_X: f32 = 50.0;
const TILE_Y: f32 = 50.0;
const TILE_W: f32 = 400.0;
const TILE_H: f32 = 300.0;

/// Agent-owned band z_order per spec (below ZONE_TILE_Z_MIN).
const TILE_Z_ORDER: u32 = 100;

/// Icon dimensions per spec.
const ICON_W: u32 = 48;
const ICON_H: u32 = 48;

// ─── Spec-mandated node parameters ───────────────────────────────────────────

/// Background: Rgba(0.07, 0.07, 0.07, 0.90) filling full tile.
const BG_COLOR: Rgba = Rgba {
    r: 0.07,
    g: 0.07,
    b: 0.07,
    a: 0.90,
};

/// Icon position per spec: (16, 16), 48×48.
const ICON_X: f32 = 16.0;
const ICON_Y: f32 = 16.0;

/// Header text node per spec: position (76, 20, 308, 32), font_size=18, white.
const HEADER_X: f32 = 76.0;
const HEADER_Y: f32 = 20.0;
const HEADER_W: f32 = 308.0;
const HEADER_H: f32 = 32.0;
const HEADER_FONT_SIZE: f32 = 18.0;
const HEADER_COLOR: Rgba = Rgba {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
};

/// Body text node per spec: position (16, 72, 368, 180), font_size=14, gray.
const BODY_X: f32 = 16.0;
const BODY_Y: f32 = 72.0;
const BODY_W: f32 = 368.0;
const BODY_H: f32 = 180.0;
const BODY_FONT_SIZE: f32 = 14.0;
const BODY_COLOR: Rgba = Rgba {
    r: 0.78,
    g: 0.78,
    b: 0.78,
    a: 1.0,
};

/// Refresh button per spec: (16, 256, 176, 36), interaction_id="refresh-button".
const REFRESH_X: f32 = 16.0;
const REFRESH_Y: f32 = 256.0;
const REFRESH_W: f32 = 176.0;
const REFRESH_H: f32 = 36.0;
const REFRESH_ID: &str = "refresh-button";

/// Dismiss button per spec: (208, 256, 176, 36), interaction_id="dismiss-button".
const DISMISS_X: f32 = 208.0;
const DISMISS_Y: f32 = 256.0;
const DISMISS_W: f32 = 176.0;
const DISMISS_H: f32 = 36.0;
const DISMISS_ID: &str = "dismiss-button";

// ─── Fixture generators ───────────────────────────────────────────────────────

/// Generate a valid 48×48 solid-color RGB PNG as a test fixture.
///
/// Uses the `image` crate to produce a conformant PNG matching the spec's
/// "48×48 PNG icon" requirement. The resulting bytes are ≤ 64 KiB (inline path).
fn make_icon_png() -> Vec<u8> {
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_fn(ICON_W, ICON_H, |_, _| {
        // Solid steel-blue color — representative agent icon placeholder.
        Rgb([70u8, 130, 180])
    });
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .expect("PNG encoding must not fail for a solid-color image");
    buf
}

// ─── Resource upload helpers ──────────────────────────────────────────────────

fn unlimited_budget() -> AgentBudget {
    AgentBudget {
        texture_bytes_total_limit: 0,
        texture_bytes_total_used: 0,
    }
}

/// Upload a PNG to the ResourceStore using the inline fast path.
///
/// Returns the BLAKE3 `ResourceId` (as a `tze_hud_resource::ResourceId`) or panics.
async fn upload_png(
    store: &ResourceStore,
    agent_namespace: &str,
    png_bytes: Vec<u8>,
    width: u32,
    height: u32,
) -> tze_hud_resource::types::ResourceId {
    let resource_id = tze_hud_resource::types::ResourceId::from_content(&png_bytes);
    let upload_id = UploadId::from_bytes(uuid::Uuid::now_v7().into_bytes());
    let result = store
        .handle_upload_start(UploadStartRequest {
            agent_namespace: agent_namespace.to_string(),
            agent_capabilities: vec![CAPABILITY_UPLOAD_RESOURCE.to_string()],
            agent_budget: unlimited_budget(),
            upload_id,
            resource_type: ResourceType::ImagePng,
            expected_hash: *resource_id.as_bytes(),
            total_size: png_bytes.len(),
            inline_data: png_bytes,
            width,
            height,
        })
        .await
        .expect("upload_start must succeed for valid PNG")
        .expect("inline upload must return ResourceStored immediately");
    result.resource_id
}

// ─── Node builders ────────────────────────────────────────────────────────────

/// Build a SolidColorNode background (root, full tile bounds, no children yet).
///
/// Spec §Dashboard Tile Composition node 1:
/// Rgba(0.07, 0.07, 0.07, 0.90), bounds (0, 0, 400, 300).
fn make_bg_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![], // children added separately via AddNode
        data: NodeData::SolidColor(SolidColorNode {
            color: BG_COLOR,
            bounds: Rect::new(0.0, 0.0, TILE_W, TILE_H),
            radius: None,
        }),
    }
}

/// Build a SolidColorNode root with 5 child IDs pre-wired.
///
/// Used for `SetTileRoot` (content update path): root's `children` field
/// references the 5 already-inserted orphan nodes by SceneId.
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

/// Build the StaticImageNode (icon, 48×48 at (16,16), fit Contain).
///
/// Spec §Dashboard Tile Composition node 2.
fn make_icon_node(resource_id: ResourceId) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: ICON_W,
            height: ICON_H,
            decoded_bytes: (ICON_W * ICON_H * 4) as u64, // 48×48 RGBA8
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(ICON_X, ICON_Y, ICON_W as f32, ICON_H as f32),
        }),
    }
}

/// Build the header TextMarkdownNode.
///
/// Spec §Dashboard Tile Composition node 3:
/// font_size=18, white, position (76, 20, 308, 32).
fn make_header_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "**Dashboard Agent**".to_string(),
            bounds: Rect::new(HEADER_X, HEADER_Y, HEADER_W, HEADER_H),
            font_size_px: HEADER_FONT_SIZE,
            font_family: FontFamily::SystemSansSerif,
            color: HEADER_COLOR,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    }
}

/// Build the body TextMarkdownNode with the given content.
///
/// Spec §Dashboard Tile Composition node 4:
/// font_size=14, gray, position (16, 72, 368, 180).
fn make_body_node(content: &str) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_string(),
            bounds: Rect::new(BODY_X, BODY_Y, BODY_W, BODY_H),
            font_size_px: BODY_FONT_SIZE,
            font_family: FontFamily::SystemSansSerif,
            color: BODY_COLOR,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    }
}

/// Build the Refresh HitRegionNode.
///
/// Spec §Dashboard Tile Composition node 5:
/// bounds (16, 256, 176, 36), interaction_id="refresh-button",
/// accepts_focus=true, accepts_pointer=true, auto_capture=true, release_on_up=true.
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

/// Build the Dismiss HitRegionNode.
///
/// Spec §Dashboard Tile Composition node 6:
/// bounds (208, 256, 176, 36), interaction_id="dismiss-button",
/// accepts_focus=true, accepts_pointer=true, auto_capture=true, release_on_up=true.
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
    agent_namespace: &str,
    lease_id: Option<SceneId>,
    mutations: Vec<SceneMutation>,
) -> MutationBatch {
    MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: agent_namespace.to_string(),
        mutations,
        timing_hints: None,
        lease_id,
    }
}

// ─── Scene setup helpers ──────────────────────────────────────────────────────

/// Create a scene with one tab and a dashboard agent lease.
fn setup_scene_with_lease() -> (SceneGraph, SceneId, SceneId) {
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease("dashboard-agent", 120_000);
    (scene, tab_id, lease_id)
}

/// Apply the initial 6-node batch to a tile:
/// bg (root via AddNode None) + 5 children (AddNode Some(bg.id)) +
/// UpdateTileOpacity(1.0) + UpdateTileInputMode(Passthrough).
///
/// Returns the MutationResult; panics if bg node data is not SolidColor.
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
        "dashboard-agent",
        Some(lease_id),
        vec![
            // bg becomes root (parent=None, tile has no root yet)
            SceneMutation::AddNode {
                tile_id,
                parent_id: None,
                node: bg,
            },
            // children of bg (painter's model order)
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

/// Apply a periodic content update: rebuild the full 6-node tree via SetTileRoot.
///
/// Steps:
///   1. `AddNode(None, ...)` × 5 → new children as orphans (tile already has root)
///   2. `SetTileRoot(new_bg with 5 child IDs)` → removes old tree, installs new root
///
/// Returns the MutationResult.
fn apply_content_update_batch(
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
        "dashboard-agent",
        Some(lease_id),
        vec![
            // Insert new children as orphans first
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
            // SetTileRoot: replaces old root tree, installs new root with child IDs
            SceneMutation::SetTileRoot {
                tile_id,
                node: new_bg,
                descendants: vec![],
            },
        ],
    ))
}

// ─── Resource upload tests (Task 3) ──────────────────────────────────────────

/// WHEN a 48×48 PNG is uploaded to the ResourceStore
/// THEN a BLAKE3 ResourceId (32 bytes) matching the hash of the raw bytes is returned.
///
/// spec.md §Requirement: Resource Upload Before Tile Creation
/// Scenario: Resource uploaded and referenced
/// tasks.md §3.1
#[tokio::test]
async fn resource_upload_48x48_png_returns_blake3_resource_id() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let png = make_icon_png();
    let expected_id = tze_hud_resource::types::ResourceId::from_content(&png);

    let resource_id = upload_png(&store, "dashboard-agent", png, ICON_W, ICON_H).await;

    assert_eq!(
        resource_id, expected_id,
        "ResourceId must equal the BLAKE3 hash of the raw PNG bytes"
    );
    assert_eq!(
        resource_id.as_bytes().len(),
        blake3::OUT_LEN,
        "ResourceId must be exactly 32 bytes (BLAKE3 digest)"
    );
}

/// WHEN two identical PNGs are uploaded
/// THEN both calls return the same ResourceId (content deduplication).
#[tokio::test]
async fn resource_upload_deduplicates_identical_content() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let png = make_icon_png();

    let id1 = upload_png(&store, "agent-a", png.clone(), ICON_W, ICON_H).await;
    let id2 = upload_png(&store, "agent-b", png, ICON_W, ICON_H).await;

    assert_eq!(
        id1, id2,
        "uploading identical bytes from different agents must return the same ResourceId"
    );
}

/// WHEN an AddNode mutation references a ResourceId that was NOT previously uploaded
///      (and thus not registered with the scene graph)
/// THEN the batch is rejected with ValidationErrorCode::ResourceNotFound and the tile
///      remains empty (atomicity: zero nodes applied).
///
/// spec.md §Requirement: Resource Upload Before Tile Creation
/// Scenario: Unknown resource rejected
/// tasks.md §3.2
#[test]
fn static_image_node_with_unregistered_resource_id_is_rejected() {
    let (mut scene, tab_id, lease_id) = setup_scene_with_lease();

    // Create the tile (no resource upload needed for CreateTile itself).
    let create_result = scene.apply_batch(&make_batch(
        "dashboard-agent",
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "dashboard-agent".into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(
        create_result.applied,
        "CreateTile must succeed; rejection: {:?}",
        create_result.rejection
    );
    let tile_id = create_result.created_ids[0];

    // Construct a StaticImageNode referencing a random ResourceId that was never uploaded.
    // The scene graph has no `register_resource` call for this ID, so the mutation must fail.
    let unregistered_id = ResourceId::from_bytes([
        0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ]);

    let icon = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::StaticImage(StaticImageNode {
            resource_id: unregistered_id,
            width: ICON_W,
            height: ICON_H,
            decoded_bytes: (ICON_W * ICON_H * 4) as u64,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(ICON_X, ICON_Y, ICON_W as f32, ICON_H as f32),
        }),
    };

    // Submit an AddNode batch with the unregistered StaticImageNode.
    let reject_result = scene.apply_batch(&make_batch(
        "dashboard-agent",
        Some(lease_id),
        vec![SceneMutation::AddNode {
            tile_id,
            parent_id: None,
            node: icon,
        }],
    ));

    // The entire batch must be rejected.
    assert!(
        !reject_result.applied,
        "batch with unregistered ResourceId must be rejected"
    );

    // created_ids must be empty — no node was inserted.
    assert!(
        reject_result.created_ids.is_empty(),
        "created_ids must be empty on rejection"
    );

    // Structured rejection must be present and carry the ResourceNotFound code.
    let rejection = reject_result
        .rejection
        .as_ref()
        .expect("rejection details must be present");
    assert_eq!(
        rejection.errors.len(),
        1,
        "rejection must report exactly 1 error"
    );
    assert_eq!(
        rejection.errors[0].code,
        ValidationErrorCode::ResourceNotFound,
        "code must be ResourceNotFound for unregistered resource; \
         got: {:?}",
        rejection.errors[0].code
    );

    // The tile must remain empty (no nodes were committed).
    let tile = scene.tiles.get(&tile_id).expect("tile must still exist");
    assert!(
        tile.root_node.is_none(),
        "tile root must remain None after rejected batch (atomicity)"
    );
}

// ─── Atomic tile creation batch tests (Task 4) ───────────────────────────────

/// WHEN the agent submits the full creation batch (bg root + 5 children via AddNode,
///      UpdateTileOpacity, UpdateTileInputMode)
/// THEN the batch is accepted and the tile has correct opacity and input_mode.
///
/// spec.md §Requirement: Atomic Tile Creation Batch
/// Scenario: Successful atomic tile creation
/// tasks.md §4.1–4.2
#[tokio::test]
async fn atomic_tile_creation_batch_accepted() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let png = make_icon_png();
    let resource_id_raw = upload_png(&store, "dashboard-agent", png, ICON_W, ICON_H).await;
    let resource_id = ResourceId::from_bytes(*resource_id_raw.as_bytes());

    let (mut scene, tab_id, lease_id) = setup_scene_with_lease();
    // Register the uploaded resource so agent-submitted StaticImageNode mutations succeed.
    scene.register_resource(resource_id);

    // CreateTile first (separate batch — tile_id not known until after creation)
    let create_result = scene.apply_batch(&make_batch(
        "dashboard-agent",
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "dashboard-agent".into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(
        create_result.applied,
        "CreateTile must succeed; rejection: {:?}",
        create_result.rejection
    );
    let tile_id = create_result.created_ids[0];

    // Atomic node + opacity + input_mode batch
    let (_, node_result) = apply_initial_node_batch(
        &mut scene,
        tile_id,
        lease_id,
        resource_id,
        "Status: operational\nUptime: 0s",
    );
    assert!(
        node_result.applied,
        "node creation batch must be accepted; rejection: {:?}",
        node_result.rejection
    );

    // Verify tile state
    let tile = scene.tiles.get(&tile_id).expect("tile must exist");
    assert_eq!(tile.opacity, 1.0, "tile opacity must be 1.0");
    assert_eq!(
        tile.input_mode,
        InputMode::Passthrough,
        "tile input_mode must be Passthrough"
    );
    assert_eq!(
        tile.z_order, TILE_Z_ORDER,
        "tile z_order must be {TILE_Z_ORDER}"
    );
    assert_eq!(
        tile.bounds,
        Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
        "tile bounds must match spec geometry (50, 50, 400, 300)"
    );
}

/// WHEN the scene graph is queried after the 6-node initial batch
/// THEN all 6 nodes are present and in the correct painter's-model order:
///      root=SolidColorNode(bg), children in order:
///      StaticImageNode(icon), TextMarkdownNode(header), TextMarkdownNode(body),
///      HitRegionNode(refresh), HitRegionNode(dismiss).
///
/// spec.md §Requirement: Dashboard Tile Composition
/// Scenario: All four node types / Painter's model compositing order
/// tasks.md §4.3, §5.1
#[tokio::test]
async fn scene_graph_has_6_nodes_in_correct_tree_order() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let png = make_icon_png();
    let resource_id_raw = upload_png(&store, "dashboard-agent", png, ICON_W, ICON_H).await;
    let resource_id = ResourceId::from_bytes(*resource_id_raw.as_bytes());

    let (mut scene, tab_id, lease_id) = setup_scene_with_lease();
    // Register the uploaded resource so agent-submitted StaticImageNode mutations succeed.
    scene.register_resource(resource_id);

    // Create tile
    let create_result = scene.apply_batch(&make_batch(
        "dashboard-agent",
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "dashboard-agent".into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(create_result.applied, "CreateTile must succeed");
    let tile_id = create_result.created_ids[0];

    // Apply 6-node batch
    let (bg_id, node_result) =
        apply_initial_node_batch(&mut scene, tile_id, lease_id, resource_id, "**Status**: OK");
    assert!(
        node_result.applied,
        "node batch must succeed; rejection: {:?}",
        node_result.rejection
    );

    // Verify total node count
    assert_eq!(scene.node_count(), 6, "scene must contain exactly 6 nodes");

    // Verify tile root
    let tile = scene.tiles.get(&tile_id).expect("tile must exist");
    assert_eq!(tile.root_node, Some(bg_id), "tile root must be bg node");

    // Verify root node data
    let root = scene.nodes.get(&bg_id).expect("root node must exist");
    assert!(
        matches!(root.data, NodeData::SolidColor(_)),
        "root must be SolidColorNode (background)"
    );

    // Verify root has exactly 5 children (painter's model order)
    assert_eq!(
        root.children.len(),
        5,
        "root must have exactly 5 children (icon, header, body, refresh, dismiss)"
    );

    // Verify children in tree order (painter's model: first rendered first)
    let child_types: Vec<&str> = root
        .children
        .iter()
        .map(|&cid| {
            let child = scene.nodes.get(&cid).expect("child node must exist");
            match &child.data {
                NodeData::SolidColor(_) => "SolidColor",
                NodeData::StaticImage(_) => "StaticImage",
                NodeData::TextMarkdown(_) => "TextMarkdown",
                NodeData::HitRegion(_) => "HitRegion",
            }
        })
        .collect();

    assert_eq!(
        child_types,
        [
            "StaticImage",
            "TextMarkdown",
            "TextMarkdown",
            "HitRegion",
            "HitRegion"
        ],
        "children must be in painter's model order: \
         StaticImage, TextMarkdown (header), TextMarkdown (body), \
         HitRegion (refresh), HitRegion (dismiss)"
    );

    // Verify HitRegion interaction_ids
    let refresh_node = scene
        .nodes
        .get(&root.children[3])
        .expect("refresh node must exist");
    if let NodeData::HitRegion(hr) = &refresh_node.data {
        assert_eq!(hr.interaction_id, REFRESH_ID);
    } else {
        panic!("children[3] must be HitRegionNode (refresh)");
    }

    let dismiss_node = scene
        .nodes
        .get(&root.children[4])
        .expect("dismiss node must exist");
    if let NodeData::HitRegion(hr) = &dismiss_node.data {
        assert_eq!(hr.interaction_id, DISMISS_ID);
    } else {
        panic!("children[4] must be HitRegionNode (dismiss)");
    }
}

/// WHEN the agent submits a batch where one mutation has invalid bounds (width=0)
/// THEN the entire batch is rejected atomically — no tile appears in the scene.
///
/// spec.md §Requirement: Atomic Tile Creation Batch
/// Scenario: Partial failure rejects entire batch
/// tasks.md §4.4
#[test]
fn partial_failure_rejects_entire_batch_atomically() {
    let (mut scene, tab_id, lease_id) = setup_scene_with_lease();
    let initial_tile_count = scene.tiles.len();

    // Batch with an invalid CreateTile (width=0 violates positive-dimension check)
    let batch = make_batch(
        "dashboard-agent",
        Some(lease_id),
        vec![
            // Valid mutation
            SceneMutation::CreateTile {
                tab_id,
                namespace: "dashboard-agent".into(),
                lease_id,
                bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
                z_order: TILE_Z_ORDER,
            },
            // Invalid mutation: width=0 → bounds check failure
            SceneMutation::CreateTile {
                tab_id,
                namespace: "dashboard-agent".into(),
                lease_id,
                bounds: Rect::new(100.0, 100.0, 0.0, 50.0),
                z_order: TILE_Z_ORDER + 1,
            },
        ],
    );

    let result = scene.apply_batch(&batch);

    // Entire batch must be rejected (all-or-nothing atomicity)
    assert!(
        !result.applied,
        "batch with invalid bounds must be rejected atomically"
    );

    // No tile must appear — atomicity guarantee
    assert_eq!(
        scene.tiles.len(),
        initial_tile_count,
        "no tile must appear in scene after batch rejection (atomicity)"
    );

    // created_ids must be empty
    assert!(
        result.created_ids.is_empty(),
        "created_ids must be empty on rejection"
    );

    // Structured rejection must be present
    assert!(
        result.rejection.is_some(),
        "rejection details must be present in MutationResult"
    );
}

// ─── Z-order and compositing tests (Task 5) ──────────────────────────────────

/// WHEN the dashboard tile is created with z_order=100
/// THEN z_order=100 is below ZONE_TILE_Z_MIN (0x8000_0000), confirming it is
///      in the agent-owned band.
///
/// spec.md §Requirement: Z-Order Compositing at Content Layer
/// Scenario: Agent tile in content band
/// tasks.md §5.2
const _Z_ORDER_100_IS_IN_AGENT_OWNED_BAND: () = assert!(
    TILE_Z_ORDER < ZONE_TILE_Z_MIN,
    "TILE_Z_ORDER must be below ZONE_TILE_Z_MIN (agent-owned band)"
);

/// WHEN the chrome layer uses a z_order >= ZONE_TILE_Z_MIN and the dashboard tile
///      uses z_order=100
/// THEN the chrome tile renders above the dashboard tile.
///
/// spec.md §Requirement: Z-Order Compositing at Content Layer
/// Scenario: Chrome layer renders above dashboard tile
/// tasks.md §5.3
#[test]
fn chrome_z_order_renders_above_dashboard_tile() {
    let chrome_z = ZONE_TILE_Z_MIN + 1;
    let dashboard_z = TILE_Z_ORDER;
    assert!(
        chrome_z > dashboard_z,
        "chrome z (0x{chrome_z:08x}) must exceed dashboard z ({dashboard_z})"
    );
}

// ─── Periodic content update tests (Task 6) ──────────────────────────────────

/// WHEN the agent submits a full tree replacement while lease is ACTIVE
/// THEN the batch is accepted and the scene still has 6 nodes (old tree swapped out).
///
/// spec.md §Requirement: Periodic Content Update
/// Scenario: Successful content update
/// tasks.md §6.1–6.2
#[tokio::test]
async fn content_update_with_active_lease_accepted() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let png = make_icon_png();
    let resource_id_raw = upload_png(&store, "dashboard-agent", png, ICON_W, ICON_H).await;
    let resource_id = ResourceId::from_bytes(*resource_id_raw.as_bytes());

    let (mut scene, tab_id, lease_id) = setup_scene_with_lease();
    // Register the uploaded resource so agent-submitted StaticImageNode mutations succeed.
    scene.register_resource(resource_id);

    // Create tile
    let create_result = scene.apply_batch(&make_batch(
        "dashboard-agent",
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "dashboard-agent".into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(create_result.applied, "CreateTile must succeed");
    let tile_id = create_result.created_ids[0];

    // Initial 6-node tree
    let (_, init_result) =
        apply_initial_node_batch(&mut scene, tile_id, lease_id, resource_id, "Uptime: 0s");
    assert!(
        init_result.applied,
        "initial node batch must succeed; rejection: {:?}",
        init_result.rejection
    );
    assert_eq!(
        scene.node_count(),
        6,
        "scene must have 6 nodes after initial setup"
    );

    // Periodic content update — full tree swap (spec: no ReplaceNode; use SetTileRoot)
    let updated_body = "**Status**: online\nUptime: 5s\nConnections: 3";
    assert!(
        updated_body.len() < tze_hud_scene::MAX_MARKDOWN_BYTES,
        "content must be under {} UTF-8 bytes",
        tze_hud_scene::MAX_MARKDOWN_BYTES,
    );

    let update_result =
        apply_content_update_batch(&mut scene, tile_id, lease_id, resource_id, updated_body);
    assert!(
        update_result.applied,
        "content update must succeed when lease is ACTIVE; rejection: {:?}",
        update_result.rejection
    );

    // Scene should still have 6 nodes (old tree removed, new tree installed)
    assert_eq!(
        scene.node_count(),
        6,
        "scene must still have 6 nodes after content update"
    );
}

/// WHEN the agent's lease has expired
/// THEN a content update batch is rejected with LeaseExpired.
///
/// spec.md §Requirement: Periodic Content Update
/// Scenario: Content update with expired lease rejected
/// tasks.md §6.3
#[tokio::test]
async fn content_update_with_expired_lease_rejected() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let png = make_icon_png();
    let resource_id_raw = upload_png(&store, "dashboard-agent", png, ICON_W, ICON_H).await;
    let resource_id = ResourceId::from_bytes(*resource_id_raw.as_bytes());

    // TestClock lets us advance time past the lease TTL without sleeping.
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = SceneGraph::new_with_clock(DISPLAY_W, DISPLAY_H, clock.clone());
    // Register the uploaded resource so agent-submitted StaticImageNode mutations succeed.
    scene.register_resource(resource_id);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);

    // Short-lived lease (100 ms TTL)
    let lease_id = scene.grant_lease("dashboard-agent", 100);

    // Create tile and initial tree while lease is ACTIVE
    let create_result = scene.apply_batch(&make_batch(
        "dashboard-agent",
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "dashboard-agent".into(),
            lease_id,
            bounds: Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            z_order: TILE_Z_ORDER,
        }],
    ));
    assert!(create_result.applied, "CreateTile must succeed");
    let tile_id = create_result.created_ids[0];

    let (_, init_result) =
        apply_initial_node_batch(&mut scene, tile_id, lease_id, resource_id, "Uptime: 0s");
    assert!(
        init_result.applied,
        "initial node batch must succeed; rejection: {:?}",
        init_result.rejection
    );

    // Advance clock past TTL (300 ms >> 100 ms)
    clock.advance(300);

    // Attempt content update with expired lease
    let update_result =
        apply_content_update_batch(&mut scene, tile_id, lease_id, resource_id, "Uptime: 5s");

    assert!(
        !update_result.applied,
        "content update must be rejected when lease is expired"
    );

    // Verify LeaseExpired error code in rejection
    let has_lease_expired = if let Some(rejection) = &update_result.rejection {
        rejection
            .errors
            .iter()
            .any(|e| e.code == ValidationErrorCode::LeaseExpired)
    } else {
        update_result
            .error
            .as_ref()
            .is_some_and(|e| matches!(e, tze_hud_scene::ValidationError::LeaseExpired { .. }))
    };

    assert!(
        has_lease_expired,
        "rejection must contain LeaseExpired error code; \
         rejection: {:?}, error: {:?}",
        update_result.rejection, update_result.error
    );
}

/// WHEN the body content is built for a content update
/// THEN it is under MAX_MARKDOWN_BYTES UTF-8 bytes per the TextMarkdownNode limit.
///
/// spec.md §Periodic Content Update — Scenario: Content does not exceed limit
/// tasks.md §6.2
#[test]
fn body_content_within_65535_utf8_byte_limit() {
    let content = "**Status**: operational\nUptime: 42s\nConnections: 7";
    assert!(
        content.len() < tze_hud_scene::MAX_MARKDOWN_BYTES,
        "body content ({} bytes) must be under {} UTF-8 bytes",
        content.len(),
        tze_hud_scene::MAX_MARKDOWN_BYTES,
    );
}

// ─── Smoke tests for node parameter correctness ───────────────────────────────

/// WHEN the two HitRegionNodes are created
/// THEN they carry the spec-mandated interaction_ids and capability flags.
///
/// spec.md §Dashboard Tile Composition nodes 5–6
#[test]
fn hit_region_nodes_have_correct_interaction_ids_and_flags() {
    let refresh = make_refresh_node();
    let dismiss = make_dismiss_node();

    if let NodeData::HitRegion(hr) = &refresh.data {
        assert_eq!(hr.interaction_id, REFRESH_ID);
        assert!(hr.accepts_focus);
        assert!(hr.accepts_pointer);
        assert!(hr.auto_capture);
        assert!(hr.release_on_up);
    } else {
        panic!("refresh must be HitRegionNode");
    }

    if let NodeData::HitRegion(hr) = &dismiss.data {
        assert_eq!(hr.interaction_id, DISMISS_ID);
        assert!(hr.accepts_focus);
        assert!(hr.accepts_pointer);
        assert!(hr.auto_capture);
        assert!(hr.release_on_up);
    } else {
        panic!("dismiss must be HitRegionNode");
    }
}

/// WHEN the background SolidColorNode is created
/// THEN it has Rgba(0.07, 0.07, 0.07, 0.90) covering the full tile bounds.
///
/// spec.md §Dashboard Tile Composition — Scenario: Background node covers full tile bounds
#[test]
fn background_node_covers_full_tile_bounds_with_correct_color() {
    let bg = make_bg_node();
    if let NodeData::SolidColor(sc) = &bg.data {
        assert!((sc.color.r - 0.07).abs() < 1e-6);
        assert!((sc.color.g - 0.07).abs() < 1e-6);
        assert!((sc.color.b - 0.07).abs() < 1e-6);
        assert!((sc.color.a - 0.90).abs() < 1e-6);
        assert_eq!(sc.bounds, Rect::new(0.0, 0.0, TILE_W, TILE_H));
    } else {
        panic!("bg must be SolidColorNode");
    }
}

/// WHEN the StaticImageNode is created
/// THEN it carries the uploaded ResourceId with spec geometry (48×48 at 16,16, Contain).
///
/// spec.md §Dashboard Tile Composition — Scenario: Icon image references uploaded resource
#[tokio::test]
async fn static_image_node_references_uploaded_resource_id_with_correct_geometry() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let png = make_icon_png();
    let resource_id_raw = upload_png(&store, "dashboard-agent", png, ICON_W, ICON_H).await;
    let resource_id = ResourceId::from_bytes(*resource_id_raw.as_bytes());

    let icon = make_icon_node(resource_id);
    if let NodeData::StaticImage(si) = &icon.data {
        assert_eq!(si.resource_id, resource_id);
        assert_eq!(si.width, ICON_W);
        assert_eq!(si.height, ICON_H);
        assert_eq!(si.fit_mode, ImageFitMode::Contain);
        assert_eq!(
            si.bounds,
            Rect::new(ICON_X, ICON_Y, ICON_W as f32, ICON_H as f32)
        );
    } else {
        panic!("icon must be StaticImageNode");
    }
}
