//! Presence card tile builder — avatar upload, tile creation, node tree construction.
//!
//! Tests acceptance criteria for hud-apoe.1 (tasks 1 and 2 from
//! openspec/changes/exemplar-presence-card/tasks.md):
//!
//! **Task 1 — Avatar resource upload:**
//! - Create 3 placeholder 32x32 PNG avatars as test fixtures (blue, green, orange)
//! - Upload via ResourceStore, verify BLAKE3 ResourceId is returned
//! - Verify duplicate upload returns same ResourceId (content deduplication)
//!
//! **Task 2 — Presence card tile creation:**
//! - Tile builder: CreateTile with 320x112 bounds, computed y-offset per agent index,
//!   z_order (100+index). Separate UpdateTileOpacity (1.0) and UpdateTileInputMode
//!   (Capture) mutations in the same batch.
//! - Node tree builder: 13-node flat glass stack — background slab, sheen,
//!   accent rail, avatar plate, avatar, eyebrow, name, status line, chip bg,
//!   chip text, dismiss bg, dismiss label, and dismiss hit region
//! - Batch submission: CreateTile accepted + node batch accepted + opacity + input mode
//! - Verify tile visible in SceneSnapshot at correct geometry with the full glass layout
//!
//! ## References
//! - openspec/changes/exemplar-presence-card/spec.md
//! - openspec/changes/exemplar-presence-card/tasks.md tasks 1-2
//! - scene-graph spec: tile CRUD, V1 node types, atomic batch mutations

use image::{ImageBuffer, Rgb};
use tze_hud_resource::{
    AgentBudget, CAPABILITY_UPLOAD_RESOURCE, ResourceStore, ResourceStoreConfig, ResourceType,
    UploadId, UploadStartRequest,
};
use tze_hud_scene::{
    SceneId, ZONE_TILE_Z_MIN,
    graph::SceneGraph,
    mutation::{MutationBatch, SceneMutation},
    types::{
        FontFamily, HitRegionNode, ImageFitMode, InputMode, Node, NodeData, Rect, Rgba,
        SolidColorNode, StaticImageNode, TextAlign, TextMarkdownNode, TextOverflow,
    },
};

// ─── Display constants ────────────────────────────────────────────────────────

const DISPLAY_W: f32 = 1920.0;
const DISPLAY_H: f32 = 1080.0;

/// Tile dimensions: 320x112 logical pixels per spec.
const CARD_W: f32 = 320.0;
const CARD_H: f32 = 112.0;

/// Bottom margin from display edge (24px per spec).
const BOTTOM_MARGIN: f32 = 24.0;

/// Left margin from display edge (24px per spec).
const LEFT_MARGIN: f32 = 24.0;

/// Vertical gap between cards (12px per spec).
const CARD_GAP: f32 = 12.0;

/// Z-order base for presence cards (100 per spec, + agent index).
const Z_ORDER_BASE: u32 = 100;

/// Avatar image dimensions (32x32 per spec).
const AVATAR_W: u32 = 32;
const AVATAR_H: u32 = 32;

const BG_RGBA: Rgba = Rgba {
    r: 0.10,
    g: 0.14,
    b: 0.19,
    a: 0.72,
};
const SHEEN_RGBA: Rgba = Rgba {
    r: 0.92,
    g: 0.96,
    b: 1.0,
    a: 0.16,
};
const EYEBROW_RGBA: Rgba = Rgba {
    r: 0.72,
    g: 0.80,
    b: 0.90,
    a: 0.82,
};
const NAME_RGBA: Rgba = Rgba {
    r: 0.97,
    g: 0.99,
    b: 1.0,
    a: 1.0,
};
const STATUS_RGBA: Rgba = Rgba {
    r: 0.82,
    g: 0.88,
    b: 0.94,
    a: 0.92,
};
const CHIP_BG_RGBA: Rgba = Rgba {
    r: 0.86,
    g: 0.92,
    b: 1.0,
    a: 0.12,
};
const CHIP_TEXT_RGBA: Rgba = Rgba {
    r: 0.96,
    g: 0.98,
    b: 1.0,
    a: 0.96,
};

const SHEEN_H: f32 = 2.0;
const ACCENT_X: f32 = 0.0;
const ACCENT_Y: f32 = 18.0;
const ACCENT_W: f32 = 4.0;
const ACCENT_H: f32 = 76.0;
const AVATAR_PLATE_X: f32 = 24.0;
const AVATAR_PLATE_Y: f32 = 28.0;
const AVATAR_PLATE_W: f32 = 56.0;
const AVATAR_PLATE_H: f32 = 56.0;
const AVATAR_X: f32 = 34.0;
const AVATAR_Y: f32 = 38.0;
const AVATAR_BOUNDS_W: f32 = 36.0;
const AVATAR_BOUNDS_H: f32 = 36.0;
const EYEBROW_X: f32 = 96.0;
const EYEBROW_Y: f32 = 18.0;
const EYEBROW_W: f32 = 152.0;
const EYEBROW_H: f32 = 12.0;
const EYEBROW_FONT_SIZE_PX: f32 = 11.0;
const NAME_X: f32 = 96.0;
const NAME_Y: f32 = 34.0;
const NAME_W: f32 = 152.0;
const NAME_H: f32 = 26.0;
const NAME_FONT_SIZE_PX: f32 = 20.0;
const STATUS_X: f32 = 96.0;
const STATUS_Y: f32 = 68.0;
const STATUS_W: f32 = 148.0;
const STATUS_H: f32 = 18.0;
const STATUS_FONT_SIZE_PX: f32 = 13.0;
const CHIP_BG_X: f32 = 224.0;
const CHIP_BG_Y: f32 = 20.0;
const CHIP_BG_W: f32 = 44.0;
const CHIP_BG_H: f32 = 22.0;
const CHIP_TEXT_X: f32 = CHIP_BG_X;
const CHIP_TEXT_Y: f32 = 21.0;
const CHIP_TEXT_W: f32 = CHIP_BG_W;
const CHIP_TEXT_H: f32 = CHIP_BG_H;
const CHIP_FONT_SIZE_PX: f32 = 10.0;
const DISMISS_BG_X: f32 = 280.0;
const DISMISS_BG_Y: f32 = 18.0;
const DISMISS_BG_W: f32 = 24.0;
const DISMISS_BG_H: f32 = 24.0;
const DISMISS_FONT_SIZE_PX: f32 = 12.0;
const DISMISS_INTERACTION_ID: &str = "dismiss-card";

// ─── Agent avatar colors (per spec) ──────────────────────────────────────────

/// Agent 0: solid blue (RGB 66, 133, 244)
const BLUE: [u8; 3] = [66, 133, 244];

/// Agent 1: solid green (RGB 52, 168, 83)
const GREEN: [u8; 3] = [52, 168, 83];

/// Agent 2: solid orange (RGB 251, 188, 4)
const ORANGE: [u8; 3] = [251, 188, 4];

// ─── PNG fixture generator ────────────────────────────────────────────────────

/// Generate a valid 32x32 solid-color RGB PNG for the given color bytes.
///
/// Uses the `image` crate to produce a conformant PNG that the resource store
/// can decode and validate. The resulting bytes are a canonical representation
/// of a solid-color 32x32 avatar placeholder per the exemplar spec.
fn make_avatar_png(rgb: [u8; 3]) -> Vec<u8> {
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_fn(AVATAR_W, AVATAR_H, |_, _| Rgb([rgb[0], rgb[1], rgb[2]]));
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .expect("PNG encoding must not fail for a solid-color image");
    buf
}

// ─── Resource upload helpers ──────────────────────────────────────────────────

/// Build an unlimited `AgentBudget` (no texture byte cap).
///
/// Used in tests so that no budget errors obscure the test logic.
fn unlimited_budget() -> AgentBudget {
    AgentBudget {
        texture_bytes_total_limit: 0,
        texture_bytes_total_used: 0,
    }
}

/// Upload a PNG byte slice to the given ResourceStore.
///
/// Returns the `ResourceId` (BLAKE3 content hash) on success, or panics.
/// Uses the inline fast path since 32x32 PNG files are well under 64 KiB.
async fn upload_avatar_png(
    store: &ResourceStore,
    agent_namespace: &str,
    png_bytes: Vec<u8>,
) -> tze_hud_resource::types::ResourceId {
    let hash = *blake3::hash(&png_bytes).as_bytes();
    let upload_id = UploadId::from_bytes(uuid::Uuid::now_v7().into_bytes());
    let result = store
        .handle_upload_start(UploadStartRequest {
            agent_namespace: agent_namespace.to_string(),
            agent_capabilities: vec![CAPABILITY_UPLOAD_RESOURCE.to_string()],
            agent_budget: unlimited_budget(),
            upload_id,
            resource_type: ResourceType::ImagePng,
            expected_hash: hash,
            total_size: png_bytes.len(),
            inline_data: png_bytes,
            width: AVATAR_W,
            height: AVATAR_H,
        })
        .await
        .expect("upload_start must succeed")
        .expect("inline upload must return ResourceStored immediately");
    result.resource_id
}

// ─── Tile geometry helpers ────────────────────────────────────────────────────

/// Compute the y-offset for an agent's presence card given its index (0, 1, 2).
///
/// Stacking formula per spec (bottom-left corner, 12px gaps, 24px bottom margin):
/// - agent 0: y = tab_height - CARD_H - BOTTOM_MARGIN = tab_height - 136
/// - agent 1: y = tab_height - 2*CARD_H - CARD_GAP - BOTTOM_MARGIN = tab_height - 260
/// - agent 2: y = tab_height - 3*CARD_H - 2*CARD_GAP - BOTTOM_MARGIN = tab_height - 384
fn card_y_offset(agent_index: usize, tab_height: f32) -> f32 {
    tab_height
        - CARD_H * (agent_index as f32 + 1.0)
        - CARD_GAP * (agent_index as f32)
        - BOTTOM_MARGIN
}

/// Build the Rect for a presence card tile given agent index.
fn card_bounds(agent_index: usize, tab_height: f32) -> Rect {
    Rect::new(
        LEFT_MARGIN,
        card_y_offset(agent_index, tab_height),
        CARD_W,
        CARD_H,
    )
}

// ─── Node builders ────────────────────────────────────────────────────────────

fn rgba_from_rgb(rgb: [u8; 3], alpha: f32) -> Rgba {
    Rgba {
        r: rgb[0] as f32 / 255.0,
        g: rgb[1] as f32 / 255.0,
        b: rgb[2] as f32 / 255.0,
        a: alpha,
    }
}

fn format_last_active(elapsed_seconds: u64) -> String {
    if elapsed_seconds == 0 {
        "now".to_string()
    } else if elapsed_seconds < 60 {
        format!("{elapsed_seconds}s ago")
    } else {
        format!("{}m ago", elapsed_seconds / 60)
    }
}

fn build_status_content(elapsed_seconds: u64) -> String {
    format!(
        "Connected • last active {}",
        format_last_active(elapsed_seconds)
    )
}

fn build_chip_content(elapsed_seconds: u64) -> String {
    match format_last_active(elapsed_seconds).as_str() {
        "now" => "NOW".to_string(),
        label if label.ends_with("s ago") => format!("{}S", &label[..label.len() - 5]),
        label if label.ends_with("m ago") => format!("{}M", &label[..label.len() - 5]),
        label => label.to_uppercase(),
    }
}

/// Build the SolidColorNode (semi-transparent dark background, full tile bounds).
fn make_bg_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: BG_RGBA,
            bounds: Rect::new(0.0, 0.0, CARD_W, CARD_H),
            radius: None,
        }),
    }
}

fn make_sheen_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: SHEEN_RGBA,
            bounds: Rect::new(0.0, 0.0, CARD_W, SHEEN_H),
            radius: None,
        }),
    }
}

fn make_accent_node(rgb: [u8; 3]) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: rgba_from_rgb(rgb, 0.78),
            bounds: Rect::new(ACCENT_X, ACCENT_Y, ACCENT_W, ACCENT_H),
            radius: None,
        }),
    }
}

fn make_avatar_plate_node(rgb: [u8; 3]) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: rgba_from_rgb(rgb, 0.22),
            bounds: Rect::new(
                AVATAR_PLATE_X,
                AVATAR_PLATE_Y,
                AVATAR_PLATE_W,
                AVATAR_PLATE_H,
            ),
            radius: None,
        }),
    }
}

/// Build the StaticImageNode avatar, scaled within the tinted plate.
fn make_avatar_node(resource_id: tze_hud_scene::ResourceId) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::StaticImage(StaticImageNode {
            resource_id,
            width: AVATAR_W,
            height: AVATAR_H,
            decoded_bytes: (AVATAR_W * AVATAR_H * 4) as u64, // 32x32 RGBA8
            fit_mode: ImageFitMode::Cover,
            bounds: Rect::new(AVATAR_X, AVATAR_Y, AVATAR_BOUNDS_W, AVATAR_BOUNDS_H),
        }),
    }
}

fn make_eyebrow_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "RESIDENT AGENT".to_string(),
            bounds: Rect::new(EYEBROW_X, EYEBROW_Y, EYEBROW_W, EYEBROW_H),
            font_size_px: EYEBROW_FONT_SIZE_PX,
            font_family: FontFamily::SystemSansSerif,
            color: EYEBROW_RGBA,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    }
}

fn make_name_node(agent_name: &str) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: format!("**{agent_name}**"),
            bounds: Rect::new(NAME_X, NAME_Y, NAME_W, NAME_H),
            font_size_px: NAME_FONT_SIZE_PX,
            font_family: FontFamily::SystemSansSerif,
            color: NAME_RGBA,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    }
}

fn make_status_node(elapsed_seconds: u64) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: build_status_content(elapsed_seconds),
            bounds: Rect::new(STATUS_X, STATUS_Y, STATUS_W, STATUS_H),
            font_size_px: STATUS_FONT_SIZE_PX,
            font_family: FontFamily::SystemSansSerif,
            color: STATUS_RGBA,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    }
}

fn make_chip_bg_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: CHIP_BG_RGBA,
            bounds: Rect::new(CHIP_BG_X, CHIP_BG_Y, CHIP_BG_W, CHIP_BG_H),
            radius: None,
        }),
    }
}

fn make_chip_text_node(elapsed_seconds: u64) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: build_chip_content(elapsed_seconds),
            bounds: Rect::new(CHIP_TEXT_X, CHIP_TEXT_Y, CHIP_TEXT_W, CHIP_TEXT_H),
            font_size_px: CHIP_FONT_SIZE_PX,
            font_family: FontFamily::SystemSansSerif,
            color: CHIP_TEXT_RGBA,
            background: None,
            alignment: TextAlign::Center,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    }
}

fn make_dismiss_bg_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.94, 0.97, 1.0, 0.14),
            bounds: Rect::new(DISMISS_BG_X, DISMISS_BG_Y, DISMISS_BG_W, DISMISS_BG_H),
            radius: None,
        }),
    }
}

fn make_dismiss_text_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "X".to_string(),
            bounds: Rect::new(DISMISS_BG_X, DISMISS_BG_Y, DISMISS_BG_W, DISMISS_BG_H),
            font_size_px: DISMISS_FONT_SIZE_PX,
            font_family: FontFamily::SystemSansSerif,
            color: CHIP_TEXT_RGBA,
            background: None,
            alignment: TextAlign::Center,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    }
}

fn make_dismiss_hit_region_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(DISMISS_BG_X, DISMISS_BG_Y, DISMISS_BG_W, DISMISS_BG_H),
            interaction_id: DISMISS_INTERACTION_ID.to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            ..Default::default()
        }),
    }
}

fn make_presence_card_children(
    resource_id: tze_hud_scene::ResourceId,
    agent_name: &str,
    accent_rgb: [u8; 3],
    elapsed_seconds: u64,
) -> Vec<Node> {
    vec![
        make_sheen_node(),
        make_accent_node(accent_rgb),
        make_avatar_plate_node(accent_rgb),
        make_avatar_node(resource_id),
        make_eyebrow_node(),
        make_name_node(agent_name),
        make_status_node(elapsed_seconds),
        make_chip_bg_node(),
        make_chip_text_node(elapsed_seconds),
        make_dismiss_bg_node(),
        make_dismiss_text_node(),
        make_dismiss_hit_region_node(),
    ]
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

// ─── Tests ────────────────────────────────────────────────────────────────────

// ── 1. Avatar resource upload (Task 1.1, 1.2) ─────────────────────────────────

/// WHEN 3 solid-color 32x32 PNG avatars are generated
/// THEN each must be a valid PNG parseable by the resource store (non-empty bytes).
#[test]
fn avatar_fixtures_are_non_empty() {
    let blue_png = make_avatar_png(BLUE);
    let green_png = make_avatar_png(GREEN);
    let orange_png = make_avatar_png(ORANGE);

    assert!(!blue_png.is_empty(), "blue PNG must not be empty");
    assert!(!green_png.is_empty(), "green PNG must not be empty");
    assert!(!orange_png.is_empty(), "orange PNG must not be empty");

    // Distinct colors must produce distinct bytes (no accidental collision)
    assert_ne!(blue_png, green_png, "blue and green avatars must differ");
    assert_ne!(
        green_png, orange_png,
        "green and orange avatars must differ"
    );
    assert_ne!(blue_png, orange_png, "blue and orange avatars must differ");
}

/// WHEN a 32x32 PNG is uploaded via ResourceStore
/// THEN it returns a ResourceId (BLAKE3 hash) and upload is accepted.
#[tokio::test]
async fn avatar_upload_returns_resource_id() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let png = make_avatar_png(BLUE);
    let expected_hash = *blake3::hash(&png).as_bytes();

    let resource_id = upload_avatar_png(&store, "agent-test", png).await;

    assert_eq!(
        resource_id.as_bytes(),
        &expected_hash,
        "ResourceId must equal BLAKE3 hash of the raw PNG bytes"
    );
}

/// WHEN the same PNG is uploaded twice
/// THEN both calls return the same ResourceId (content deduplication).
#[tokio::test]
async fn avatar_upload_deduplicates() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let png = make_avatar_png(GREEN);

    let id1 = upload_avatar_png(&store, "agent-alpha", png.clone()).await;
    let id2 = upload_avatar_png(&store, "agent-beta", png.clone()).await;

    assert_eq!(
        id1, id2,
        "uploading identical content from different agents must return the same ResourceId"
    );
}

/// WHEN three distinct avatar PNGs are uploaded
/// THEN each returns a distinct ResourceId (no spurious dedup across different colors).
#[tokio::test]
async fn three_avatar_uploads_distinct_resource_ids() {
    let store = ResourceStore::new(ResourceStoreConfig::default());

    let id0 = upload_avatar_png(&store, "agent-0", make_avatar_png(BLUE)).await;
    let id1 = upload_avatar_png(&store, "agent-1", make_avatar_png(GREEN)).await;
    let id2 = upload_avatar_png(&store, "agent-2", make_avatar_png(ORANGE)).await;

    assert_ne!(
        id0, id1,
        "blue and green avatars must have different ResourceIds"
    );
    assert_ne!(
        id1, id2,
        "green and orange avatars must have different ResourceIds"
    );
    assert_ne!(
        id0, id2,
        "blue and orange avatars must have different ResourceIds"
    );
}

// ── 2. Tile geometry (Task 2.1) ───────────────────────────────────────────────

/// WHEN computing y-offsets for agents 0, 1, 2
/// THEN each should match the glass-card formula from spec (tab_height - 136, -260, -384).
#[test]
fn presence_card_y_offsets_match_spec() {
    let h = DISPLAY_H;

    assert_eq!(
        card_y_offset(0, h),
        h - 136.0,
        "agent 0 y = tab_height - 136"
    );
    assert_eq!(
        card_y_offset(1, h),
        h - 260.0,
        "agent 1 y = tab_height - 260"
    );
    assert_eq!(
        card_y_offset(2, h),
        h - 384.0,
        "agent 2 y = tab_height - 384"
    );
}

/// WHEN presence card bounds are computed for all 3 agents
/// THEN tiles do not overlap (y-ranges are disjoint, same x).
#[test]
fn presence_card_bounds_no_overlap() {
    let h = DISPLAY_H;
    let bounds: Vec<Rect> = (0..3).map(|i| card_bounds(i, h)).collect();

    for i in 0..3 {
        for j in (i + 1)..3 {
            let a = bounds[i];
            let b = bounds[j];
            // Same x, so check only y-ranges: [y, y+CARD_H) must not intersect
            let a_bottom = a.y + a.height;
            let b_bottom = b.y + b.height;
            let overlaps = a.y < b_bottom && b.y < a_bottom;
            assert!(
                !overlaps,
                "agent {i} card [{}, {}] overlaps agent {j} card [{}, {}]",
                a.y, a_bottom, b.y, b_bottom
            );
        }
    }
}

/// WHEN z_orders are computed for 3 agents
/// THEN they are sequential (100, 101, 102) and all below ZONE_TILE_Z_MIN.
#[test]
fn presence_card_z_orders_sequential_and_below_zone_min() {
    for i in 0..3u32 {
        let z = Z_ORDER_BASE + i;
        assert_eq!(z, 100 + i, "z_order for agent {i} must be {}", 100 + i);
        assert!(
            z < ZONE_TILE_Z_MIN,
            "z_order {z} must be below ZONE_TILE_Z_MIN ({ZONE_TILE_Z_MIN})"
        );
    }
}

// ── 3. Tile creation batch (Task 2.1, 2.3) ───────────────────────────────────

/// WHEN an agent submits CreateTile with correct geometry + UpdateTileOpacity +
///      UpdateTileInputMode in one batch
/// THEN the batch is accepted, tile appears in scene with correct fields.
#[test]
fn create_tile_batch_accepted_with_opacity_and_input_mode() {
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease("agent-0", 120_000);

    let agent_index = 0usize;
    let bounds = card_bounds(agent_index, DISPLAY_H);
    let z_order = Z_ORDER_BASE + agent_index as u32;

    // CreateTile batch
    let create_batch = make_batch(
        "agent-0",
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent-0".into(),
            lease_id,
            bounds,
            z_order,
        }],
    );
    let create_result = scene.apply_batch(&create_batch);
    assert!(
        create_result.applied,
        "CreateTile batch must be accepted; rejection: {:?}",
        create_result.rejection
    );
    assert_eq!(
        create_result.created_ids.len(),
        1,
        "CreateTile must produce exactly one created_id"
    );
    let tile_id = create_result.created_ids[0];

    // UpdateTileOpacity + UpdateTileInputMode batch (post-creation)
    let update_batch = make_batch(
        "agent-0",
        Some(lease_id),
        vec![
            SceneMutation::UpdateTileOpacity {
                tile_id,
                opacity: 1.0,
            },
            SceneMutation::UpdateTileInputMode {
                tile_id,
                input_mode: InputMode::Capture,
            },
        ],
    );
    let update_result = scene.apply_batch(&update_batch);
    assert!(
        update_result.applied,
        "UpdateTileOpacity + UpdateTileInputMode batch must be accepted; rejection: {:?}",
        update_result.rejection
    );

    // Verify tile state
    let tile = scene
        .tiles
        .get(&tile_id)
        .expect("tile must exist in scene after creation");
    assert_eq!(tile.opacity, 1.0, "tile opacity must be 1.0");
    assert_eq!(
        tile.input_mode,
        InputMode::Capture,
        "tile input_mode must be Capture"
    );
    assert_eq!(
        tile.bounds, bounds,
        "tile bounds must match specified geometry"
    );
    assert_eq!(tile.z_order, z_order, "tile z_order must be {z_order}");
    assert_eq!(tile.namespace, "agent-0", "tile namespace must match agent");
}

// ── 4. Node tree builder (Task 2.2) ──────────────────────────────────────────

/// WHEN the glass presence card tree is added via AddNode mutations
/// THEN the tile has a SolidColorNode root plus 12 child nodes implementing
///      sheen, accent, avatar plate, avatar, typography stack, and time chip.
#[tokio::test]
async fn node_tree_builder_glass_card_nodes() {
    // Set up resource store and upload avatar
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let resource_id_raw = upload_avatar_png(&store, "agent-0", make_avatar_png(BLUE)).await;
    // Convert from tze_hud_resource::ResourceId to tze_hud_scene::ResourceId
    let resource_id = tze_hud_scene::ResourceId::from_bytes(*resource_id_raw.as_bytes());

    // Set up scene
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    // Register the uploaded resource so agent-submitted StaticImageNode mutations succeed.
    scene.register_resource(resource_id);
    let lease_id = scene.grant_lease("agent-0", 120_000);

    // Create tile
    let create_batch = make_batch(
        "agent-0",
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent-0".into(),
            lease_id,
            bounds: card_bounds(0, DISPLAY_H),
            z_order: Z_ORDER_BASE,
        }],
    );
    let create_result = scene.apply_batch(&create_batch);
    assert!(create_result.applied, "CreateTile must succeed");
    let tile_id = create_result.created_ids[0];

    // Build nodes
    let bg_node = make_bg_node();
    let bg_id = bg_node.id;
    let child_nodes = make_presence_card_children(resource_id, "AgentAlpha", BLUE, 0);

    let mut mutations = vec![SceneMutation::AddNode {
        tile_id,
        parent_id: None,
        node: bg_node,
    }];
    mutations.extend(child_nodes.into_iter().map(|node| SceneMutation::AddNode {
        tile_id,
        parent_id: Some(bg_id),
        node,
    }));
    let node_batch = make_batch("agent-0", Some(lease_id), mutations);
    let node_result = scene.apply_batch(&node_batch);
    assert!(
        node_result.applied,
        "AddNode x13 batch must be accepted; rejection: {:?}",
        node_result.rejection
    );
    assert_eq!(
        node_result.created_ids.len(),
        13,
        "13 AddNode mutations must produce 13 created_ids"
    );

    // Verify tile root
    let tile = scene.tiles.get(&tile_id).expect("tile must exist");
    assert!(
        tile.root_node.is_some(),
        "tile must have a root node after AddNode"
    );
    let root_id = tile.root_node.unwrap();

    // Verify root is SolidColorNode
    let root_node = scene
        .nodes
        .get(&root_id)
        .expect("root node must be in graph");
    assert!(
        matches!(root_node.data, NodeData::SolidColor(_)),
        "root node must be SolidColorNode, got {:?}",
        std::mem::discriminant(&root_node.data)
    );

    // Verify root has 12 children (glass layers + avatar + typography + controls)
    assert_eq!(
        root_node.children.len(),
        12,
        "root SolidColorNode must have exactly 12 children for the interactive glass-card layout"
    );

    // Verify key child ordering in the painter's model.
    let child_ids = root_node.children.clone();
    let child0 = scene
        .nodes
        .get(&child_ids[0])
        .expect("first child must exist");
    let child3 = scene
        .nodes
        .get(&child_ids[3])
        .expect("fourth child must exist");
    let child5 = scene
        .nodes
        .get(&child_ids[5])
        .expect("sixth child must exist");
    let child8 = scene
        .nodes
        .get(&child_ids[8])
        .expect("ninth child must exist");
    let child11 = scene
        .nodes
        .get(&child_ids[11])
        .expect("twelfth child must exist");
    assert!(
        matches!(child0.data, NodeData::SolidColor(_)),
        "first child must be the glass sheen SolidColorNode"
    );
    assert!(
        matches!(child3.data, NodeData::StaticImage(_)),
        "fourth child must be StaticImageNode"
    );
    assert!(
        matches!(child5.data, NodeData::TextMarkdown(_)),
        "sixth child must be a TextMarkdownNode"
    );
    assert!(
        matches!(child8.data, NodeData::TextMarkdown(_)),
        "ninth child must be the time-chip TextMarkdownNode"
    );
    assert!(
        matches!(child11.data, NodeData::HitRegion(_)),
        "twelfth child must be the dismiss HitRegionNode"
    );

    // Verify SolidColorNode properties (background)
    if let NodeData::SolidColor(bg) = &root_node.data {
        assert_eq!(
            bg.bounds,
            Rect::new(0.0, 0.0, CARD_W, CARD_H),
            "bg bounds must cover full tile"
        );
        assert!(
            (bg.color.r - BG_RGBA.r).abs() < 1e-5,
            "bg color.r must match glass spec, got {}",
            bg.color.r
        );
        assert!(
            (bg.color.a - BG_RGBA.a).abs() < 1e-5,
            "bg color.a must match glass spec, got {}",
            bg.color.a
        );
    }

    // Verify StaticImageNode properties
    if let NodeData::StaticImage(img) = &child3.data {
        assert_eq!(
            img.resource_id, resource_id,
            "avatar must reference uploaded ResourceId"
        );
        assert_eq!(img.width, AVATAR_W, "avatar width must be 32");
        assert_eq!(img.height, AVATAR_H, "avatar height must be 32");
        assert_eq!(
            img.fit_mode,
            ImageFitMode::Cover,
            "avatar fit mode must be Cover"
        );
        assert_eq!(
            img.bounds,
            Rect::new(AVATAR_X, AVATAR_Y, AVATAR_BOUNDS_W, AVATAR_BOUNDS_H),
            "avatar bounds must match the expanded glass layout"
        );
    }

    // Verify the bold-name TextMarkdownNode properties.
    if let NodeData::TextMarkdown(txt) = &child5.data {
        assert!(
            txt.content.contains("**AgentAlpha**"),
            "glass-card layout must include the bold agent name"
        );
        assert_eq!(
            txt.font_size_px, NAME_FONT_SIZE_PX,
            "name font size must match spec"
        );
        assert!(
            (txt.color.r - NAME_RGBA.r).abs() < 1e-5,
            "glass-card name tint must match spec"
        );
        assert_eq!(
            txt.overflow,
            TextOverflow::Ellipsis,
            "overflow must be Ellipsis"
        );
    }

    if let NodeData::HitRegion(hr) = &child11.data {
        assert_eq!(hr.interaction_id, DISMISS_INTERACTION_ID);
        assert!(
            hr.accepts_pointer,
            "dismiss affordance must accept pointer input"
        );
    }

    // Verify total node count
    assert_eq!(
        scene.node_count(),
        13,
        "scene must have exactly 13 nodes total for the interactive glass-card layout"
    );
}

// ── 5. Full batch submission visible in SceneSnapshot (Task 2.3) ──────────────

/// WHEN a full presence card batch is submitted (CreateTile + AddNode x13 +
///      UpdateTileOpacity + UpdateTileInputMode)
/// THEN the tile is visible in SceneSnapshot with correct geometry, opacity,
///      input mode, and the full glass-card node stack.
#[tokio::test]
async fn full_presence_card_batch_visible_in_snapshot() {
    // Set up resource store and upload avatar
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let resource_id_raw = upload_avatar_png(&store, "agent-0", make_avatar_png(BLUE)).await;
    let resource_id = tze_hud_scene::ResourceId::from_bytes(*resource_id_raw.as_bytes());

    // Set up scene
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    // Register the uploaded resource so agent-submitted StaticImageNode mutations succeed.
    scene.register_resource(resource_id);
    let lease_id = scene.grant_lease("presence-agent", 120_000);

    let agent_index = 0usize;
    let expected_bounds = card_bounds(agent_index, DISPLAY_H);
    let expected_z = Z_ORDER_BASE + agent_index as u32;

    // ── Batch 1: CreateTile ───────────────────────────────────────────────────
    let b1 = make_batch(
        "presence-agent",
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "presence-agent".into(),
            lease_id,
            bounds: expected_bounds,
            z_order: expected_z,
        }],
    );
    let r1 = scene.apply_batch(&b1);
    assert!(r1.applied, "CreateTile batch must be accepted");
    let tile_id = r1.created_ids[0];

    // ── Batch 2: AddNode x13 + UpdateTileOpacity + UpdateTileInputMode ────────
    let bg_node = make_bg_node();
    let bg_id = bg_node.id;
    let child_nodes = make_presence_card_children(resource_id, "PresenceAgent", BLUE, 0);

    let mut mutations = vec![SceneMutation::AddNode {
        tile_id,
        parent_id: None,
        node: bg_node,
    }];
    mutations.extend(child_nodes.into_iter().map(|node| SceneMutation::AddNode {
        tile_id,
        parent_id: Some(bg_id),
        node,
    }));
    mutations.push(SceneMutation::UpdateTileOpacity {
        tile_id,
        opacity: 1.0,
    });
    mutations.push(SceneMutation::UpdateTileInputMode {
        tile_id,
        input_mode: InputMode::Capture,
    });
    let b2 = make_batch("presence-agent", Some(lease_id), mutations);
    let r2 = scene.apply_batch(&b2);
    assert!(
        r2.applied,
        "AddNode x13 + UpdateTileOpacity + UpdateTileInputMode batch must be accepted; rejection: {:?}",
        r2.rejection
    );

    // ── SceneSnapshot verification ────────────────────────────────────────────
    let snap = scene.take_snapshot(0, 0);

    // Tile must be present in snapshot
    let snap_tile = snap
        .tiles
        .values()
        .find(|t| t.id == tile_id)
        .expect("tile must appear in SceneSnapshot after creation and node tree assembly");

    assert_eq!(
        snap_tile.bounds, expected_bounds,
        "snapshot tile bounds must match spec geometry"
    );
    assert_eq!(
        snap_tile.z_order, expected_z,
        "snapshot tile z_order must be {expected_z}"
    );
    assert_eq!(snap_tile.opacity, 1.0, "snapshot tile opacity must be 1.0");
    assert_eq!(
        snap_tile.input_mode,
        InputMode::Capture,
        "snapshot tile input_mode must be Capture"
    );
    assert_eq!(
        snap_tile.namespace, "presence-agent",
        "snapshot tile namespace must match agent"
    );

    // Root node must be set
    assert!(
        snap_tile.root_node.is_some(),
        "snapshot tile must have root_node set"
    );

    // Full glass-card node stack must be in snapshot (1 bg + 12 children,
    // matching node_tree_builder_glass_card_nodes).
    assert_eq!(
        snap.nodes.len(),
        13,
        "SceneSnapshot must contain exactly 13 nodes (bg + layered glass children)"
    );

    // Verify node types in snapshot
    let has_bg = snap
        .nodes
        .values()
        .any(|n| matches!(n.data, NodeData::SolidColor(_)));
    let has_avatar = snap
        .nodes
        .values()
        .any(|n| matches!(n.data, NodeData::StaticImage(_)));
    let has_text = snap
        .nodes
        .values()
        .any(|n| matches!(n.data, NodeData::TextMarkdown(_)));

    assert!(
        has_bg,
        "SceneSnapshot must contain SolidColorNode (background)"
    );
    assert!(
        has_avatar,
        "SceneSnapshot must contain StaticImageNode (avatar)"
    );
    assert!(
        has_text,
        "SceneSnapshot must contain TextMarkdownNode (identity text)"
    );
}

// ── 6. Three agents: non-overlapping presence cards (Tasks 2.1, 5.x) ─────────

/// WHEN 3 agents each create a presence card tile with unique y-offsets and z_orders
/// THEN all 3 tiles are present in the scene with distinct, non-overlapping bounds.
#[test]
fn three_agents_non_overlapping_presence_cards() {
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);

    let agents = ["agent-alpha", "agent-beta", "agent-gamma"];
    let mut tile_ids = Vec::new();

    for (i, &agent) in agents.iter().enumerate() {
        let lease_id = scene.grant_lease(agent, 120_000);
        let bounds = card_bounds(i, DISPLAY_H);
        let z_order = Z_ORDER_BASE + i as u32;
        let batch = make_batch(
            agent,
            Some(lease_id),
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: agent.into(),
                lease_id,
                bounds,
                z_order,
            }],
        );
        let result = scene.apply_batch(&batch);
        assert!(
            result.applied,
            "agent {agent} CreateTile must be accepted; rejection: {:?}",
            result.rejection
        );
        tile_ids.push(result.created_ids[0]);
    }

    assert_eq!(scene.tile_count(), 3, "scene must have exactly 3 tiles");

    // Verify all 3 tiles present with distinct bounds
    let bounds: Vec<Rect> = tile_ids
        .iter()
        .map(|id| scene.tiles.get(id).expect("tile must exist").bounds)
        .collect();

    // Check non-overlap
    for i in 0..3 {
        for j in (i + 1)..3 {
            let a = bounds[i];
            let b = bounds[j];
            let a_bottom = a.y + a.height;
            let b_bottom = b.y + b.height;
            let overlaps = a.y < b_bottom && b.y < a_bottom;
            assert!(
                !overlaps,
                "tile {i} [{}, {}] must not overlap tile {j} [{}, {}]",
                a.y, a_bottom, b.y, b_bottom
            );
        }
    }

    // Verify each agent owns only its own tile (namespace isolation)
    for (i, &agent) in agents.iter().enumerate() {
        let tile = scene.tiles.get(&tile_ids[i]).unwrap();
        assert_eq!(tile.namespace, agent, "tile {i} must be owned by {agent}");
    }
}

// ── 7. SceneSnapshot round-trip (snapshot accuracy) ──────────────────────────

/// WHEN a presence card tile with the full glass-card node stack is snapshotted twice
/// THEN both snapshots have identical tile geometry, node types, and checksums.
#[tokio::test]
async fn snapshot_is_deterministic_after_presence_card_assembly() {
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let resource_id_raw = upload_avatar_png(&store, "agent-0", make_avatar_png(ORANGE)).await;
    let resource_id = tze_hud_scene::ResourceId::from_bytes(*resource_id_raw.as_bytes());

    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    // Register the uploaded resource so agent-submitted StaticImageNode mutations succeed.
    scene.register_resource(resource_id);
    let lease_id = scene.grant_lease("agent-det", 120_000);

    // Create tile
    let b1 = make_batch(
        "agent-det",
        Some(lease_id),
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "agent-det".into(),
            lease_id,
            bounds: card_bounds(0, DISPLAY_H),
            z_order: Z_ORDER_BASE,
        }],
    );
    let r1 = scene.apply_batch(&b1);
    assert!(r1.applied, "CreateTile must succeed");
    let tile_id = r1.created_ids[0];

    // Add nodes
    let bg_node = make_bg_node();
    let bg_id = bg_node.id;
    let child_nodes = make_presence_card_children(resource_id, "DetAgent", ORANGE, 0);

    let mut mutations = vec![SceneMutation::AddNode {
        tile_id,
        parent_id: None,
        node: bg_node,
    }];
    mutations.extend(child_nodes.into_iter().map(|node| SceneMutation::AddNode {
        tile_id,
        parent_id: Some(bg_id),
        node,
    }));
    let b2 = make_batch("agent-det", Some(lease_id), mutations);
    assert!(scene.apply_batch(&b2).applied, "AddNode batch must succeed");

    // Take two snapshots at the same logical time — must be identical
    let snap1 = scene.take_snapshot(1_000_000, 1_000);
    let snap2 = scene.take_snapshot(1_000_000, 1_000);

    assert_eq!(
        snap1.checksum, snap2.checksum,
        "snapshot checksum must be deterministic"
    );
    assert_eq!(
        snap1.nodes.len(),
        snap2.nodes.len(),
        "both snapshots must report same node count"
    );
    assert_eq!(
        snap1.tiles.len(),
        snap2.tiles.len(),
        "both snapshots must report same tile count"
    );
}
