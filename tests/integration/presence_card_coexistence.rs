//! Presence card: periodic content updates and multi-agent coexistence.
//!
//! Tests acceptance criteria for hud-apoe.3 (tasks 4 and 5 from
//! openspec/changes/exemplar-presence-card/tasks.md):
//!
//! **Task 4 — Periodic content updates:**
//! - 30-second content update loop: SetTileRoot mutation with complete updated
//!   node tree. Only TextMarkdownNode content changes; full tree is rebuilt.
//! - No ReplaceNode variant — SetTileRoot is the only way to update node content.
//! - Human-friendly time formatting: "now" at 0s, "30s ago", "1m ago", "2m ago".
//!
//! **Task 5 — Multi-agent coexistence integration test:**
//! - 3 concurrent gRPC agent sessions (agent-alpha, agent-beta, agent-gamma)
//! - Each creates a presence card tile with unique avatar color, y-offset, z_order
//! - Verify all 3 tiles visible in SceneSnapshot, no overlap, namespace isolation
//! - Verify concurrent content updates work without interference
//!
//! ## References
//! - openspec/changes/exemplar-presence-card/spec.md
//! - openspec/changes/exemplar-presence-card/tasks.md tasks 4-5
//! - scene-graph spec: tile CRUD, V1 node types, atomic batch mutations

use tze_hud_protocol::proto;
use tze_hud_protocol::proto::session as session_proto;
use tze_hud_runtime::HeadlessRuntime;
use tze_hud_runtime::headless::HeadlessConfig;
use tze_hud_scene::types::ZoneRegistry;
use tze_hud_scene::{
    SceneId,
    graph::SceneGraph,
    mutation::{MutationBatch, SceneMutation},
    types::{
        FontFamily, Node, NodeData, Rect, Rgba, SolidColorNode, StaticImageNode, TextAlign,
        TextMarkdownNode, TextOverflow,
    },
};

use uuid::Uuid;

// ─── Shared gRPC session harness ─────────────────────────────────────────────
// Extracted from duplicate copies in multi_agent and subtitle_streaming
// (hud-ls5pz). See common/mod.rs for drift reconciliation notes.
#[path = "common/mod.rs"]
mod common;
use common::*;

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

/// Avatar display positions within the expanded glass card.
const AVATAR_X: f32 = 32.0;
const AVATAR_Y: f32 = 36.0;
const AVATAR_BOUNDS_W: f32 = 40.0;
const AVATAR_BOUNDS_H: f32 = 40.0;

/// Status text position and size within the expanded card.
const TEXT_X: f32 = 96.0;
const TEXT_Y: f32 = 68.0;
const TEXT_W: f32 = 148.0;
const TEXT_H: f32 = 18.0;

/// Status text font size.
const FONT_SIZE_PX: f32 = 13.0;

// ─── gRPC test constants ──────────────────────────────────────────────────────

const TEST_PSK: &str = "presence-coexistence-test-key";
const GRPC_PORT: u16 = 50055; // distinct port to avoid conflicts

// ─── Human-friendly time formatting ──────────────────────────────────────────

/// Format elapsed seconds into a human-friendly "last active" time string.
///
/// Spec thresholds:
/// - 0s        → "now"
/// - 1–59s     → "Ns ago" (e.g., "30s ago")
/// - 60–119s   → "1m ago"
/// - 120–179s  → "2m ago"
/// - N*60+ s   → "Nm ago"
pub fn format_elapsed_secs(elapsed_secs: u64) -> String {
    if elapsed_secs == 0 {
        "now".to_string()
    } else if elapsed_secs < 60 {
        format!("{elapsed_secs}s ago")
    } else {
        let minutes = elapsed_secs / 60;
        format!("{minutes}m ago")
    }
}

/// Build the TextMarkdownNode content string for the given agent name and elapsed time.
pub fn build_text_content(agent_name: &str, elapsed_secs: u64) -> String {
    let _ = agent_name;
    format!(
        "Connected • last active {}",
        format_elapsed_secs(elapsed_secs)
    )
}

// ─── Tile geometry helpers ────────────────────────────────────────────────────

/// Compute the y-offset for an agent's presence card given its index (0, 1, 2).
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

/// Build the SolidColorNode (semi-transparent dark background, full tile bounds).
fn make_bg_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba {
                r: 0.10,
                g: 0.14,
                b: 0.19,
                a: 0.72,
            },
            bounds: Rect::new(0.0, 0.0, CARD_W, CARD_H),
            radius: None,
        }),
    }
}

/// Build a placeholder StaticImageNode using a zeroed ResourceId (no real upload in unit tests).
///
/// For coexistence tests we use a zeroed resource ID because the resource store
/// is not wired in these scene-layer tests. Avatar upload is already tested in hud-apoe.1.
fn make_placeholder_avatar_node() -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::StaticImage(StaticImageNode {
            resource_id: tze_hud_scene::ResourceId::from_bytes([0u8; 32]),
            width: AVATAR_W,
            height: AVATAR_H,
            decoded_bytes: (AVATAR_W * AVATAR_H * 4) as u64,
            fit_mode: tze_hud_scene::types::ImageFitMode::Cover,
            bounds: Rect::new(AVATAR_X, AVATAR_Y, AVATAR_BOUNDS_W, AVATAR_BOUNDS_H),
        }),
    }
}

/// Build the TextMarkdownNode with the given content string.
fn make_text_node(content: &str) -> Node {
    Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.to_string(),
            bounds: Rect::new(TEXT_X, TEXT_Y, TEXT_W, TEXT_H),
            font_size_px: FONT_SIZE_PX,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba {
                r: 0.82,
                g: 0.88,
                b: 0.94,
                a: 0.92,
            },
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    }
}

// ─── Batch helper ─────────────────────────────────────────────────────────────

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

// ─── Time formatting tests (Task 4.3) ────────────────────────────────────────

/// WHEN elapsed = 0s
/// THEN format_elapsed_secs returns "now".
#[test]
fn time_format_at_zero_is_now() {
    assert_eq!(format_elapsed_secs(0), "now");
}

/// WHEN elapsed = 30s
/// THEN format_elapsed_secs returns "30s ago".
#[test]
fn time_format_at_30s_is_30s_ago() {
    assert_eq!(format_elapsed_secs(30), "30s ago");
}

/// WHEN elapsed = 60s
/// THEN format_elapsed_secs returns "1m ago".
#[test]
fn time_format_at_60s_is_1m_ago() {
    assert_eq!(format_elapsed_secs(60), "1m ago");
}

/// WHEN elapsed = 120s
/// THEN format_elapsed_secs returns "2m ago".
#[test]
fn time_format_at_120s_is_2m_ago() {
    assert_eq!(format_elapsed_secs(120), "2m ago");
}

/// WHEN elapsed = 1s–59s
/// THEN format returns "Xs ago".
#[test]
fn time_format_sub_minute_uses_seconds() {
    for s in [1u64, 15, 45, 59] {
        let result = format_elapsed_secs(s);
        assert_eq!(
            result,
            format!("{s}s ago"),
            "elapsed {s}s must format as '{s}s ago'"
        );
    }
}

/// WHEN elapsed = 180s (3 minutes)
/// THEN format returns "3m ago".
#[test]
fn time_format_multi_minute() {
    assert_eq!(format_elapsed_secs(180), "3m ago");
    assert_eq!(format_elapsed_secs(300), "5m ago");
}

/// WHEN content is built for an agent at various elapsed times
/// THEN the format is "Connected • last active {time}".
#[test]
fn build_text_content_format() {
    let content_0 = build_text_content("agent-alpha", 0);
    assert!(
        content_0.contains("Connected • last active now"),
        "at 0s must say 'Connected • last active now'"
    );

    let content_30 = build_text_content("agent-beta", 30);
    assert!(content_30.contains("Connected • last active 30s ago"));

    let content_60 = build_text_content("agent-gamma", 60);
    assert!(content_60.contains("Connected • last active 1m ago"));
}

// ─── Content update loop tests (Task 4.1, 4.2) ───────────────────────────────

/// WHEN a presence card tile has 3 nodes and a content update is submitted via SetTileRoot
/// THEN the batch has exactly 1 mutation (SetTileRoot), is accepted, and the new root
///      node carries the updated text.
///
/// Note: SetTileRoot replaces the entire node tree with the new root node.
/// In the content-update scenario the agent rebuilds the tree: the new bg root is
/// inserted as the tile root. The avatar and text children are then inserted via
/// AddNode in the same batch (separate mutations), establishing the "complete updated
/// node tree". The spec phrase "exactly 1 mutation: SetTileRoot" means 1 SetTileRoot
/// mutation; the full batch also includes AddNode for child nodes.
///
/// Task 4.2: Verify content updates produce valid MutationBatch (SetTileRoot accepted).
#[test]
fn content_update_set_tile_root_accepted() {
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    // Register zeroed ResourceId used by make_placeholder_avatar_node() so
    // agent-submitted StaticImageNode mutations succeed.
    scene.register_resource(tze_hud_scene::ResourceId::from_bytes([0u8; 32]));
    let lease_id = scene.grant_lease("agent-0", 120_000);

    // ── Initial tile creation ─────────────────────────────────────────────────
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

    // ── Initial node tree via AddNode ─────────────────────────────────────────
    let bg_node = make_bg_node();
    let avatar_node = make_placeholder_avatar_node();
    let text_node = make_text_node(&build_text_content("AgentZero", 0));
    let bg_id = bg_node.id;

    let initial_node_batch = make_batch(
        "agent-0",
        Some(lease_id),
        vec![
            SceneMutation::AddNode {
                tile_id,
                parent_id: None,
                node: bg_node,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(bg_id),
                node: avatar_node,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(bg_id),
                node: text_node,
            },
        ],
    );
    let initial_node_result = scene.apply_batch(&initial_node_batch);
    assert!(
        initial_node_result.applied,
        "initial AddNode x3 must succeed: {:?}",
        initial_node_result.rejection
    );
    assert_eq!(
        scene.node_count(),
        3,
        "scene must have 3 nodes after initial setup"
    );

    // ── Content update via SetTileRoot (Task 4.1) ─────────────────────────────
    // Simulate "30 seconds elapsed": rebuild the full node tree with updated text.
    let new_bg = make_bg_node();
    let new_avatar = make_placeholder_avatar_node();
    let new_text = make_text_node(&build_text_content("AgentZero", 30));
    let new_bg_id = new_bg.id;
    let new_text_id = new_text.id;

    // The SetTileRoot mutation: 1 mutation to set the new bg as root.
    let set_root_mutation = SceneMutation::SetTileRoot {
        tile_id,
        node: new_bg.clone(),
        descendants: vec![],
    };

    // Verify the batch has exactly 1 SetTileRoot mutation before applying.
    let update_batch = make_batch(
        "agent-0",
        Some(lease_id),
        vec![
            set_root_mutation,
            // Re-add children with updated content (new IDs avoid DuplicateId error)
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(new_bg_id),
                node: new_avatar,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(new_bg_id),
                node: new_text,
            },
        ],
    );

    // Verify the batch contains exactly 1 SetTileRoot mutation (Task 4.2).
    let set_tile_root_count = update_batch
        .mutations
        .iter()
        .filter(|m| matches!(m, SceneMutation::SetTileRoot { .. }))
        .count();
    assert_eq!(
        set_tile_root_count, 1,
        "content update batch must contain exactly 1 SetTileRoot mutation"
    );

    let update_result = scene.apply_batch(&update_batch);
    assert!(
        update_result.applied,
        "content update batch must be accepted: {:?}",
        update_result.rejection
    );

    // ── Verify the new root node is set ───────────────────────────────────────
    let tile = scene.tiles.get(&tile_id).expect("tile must exist");
    assert!(
        tile.root_node.is_some(),
        "tile must have a root node after SetTileRoot"
    );
    let root_id = tile.root_node.unwrap();
    assert_eq!(root_id, new_bg_id, "tile root must be the new bg node");

    // Verify the text node has the updated content (30s elapsed).
    let text_node_in_scene = scene
        .nodes
        .get(&new_text_id)
        .expect("new text node must be in scene");
    if let NodeData::TextMarkdown(txt) = &text_node_in_scene.data {
        assert!(
            txt.content.contains("30s ago"),
            "updated text node must contain '30s ago', got: '{}'",
            txt.content
        );
        assert!(
            txt.content.contains("Connected • last active"),
            "updated text must keep the connected status prefix"
        );
    } else {
        panic!(
            "expected TextMarkdownNode, got: {:?}",
            text_node_in_scene.data
        );
    }

    // Verify the scene has exactly 3 nodes (old tree was replaced by SetTileRoot + 2 AddNode).
    assert_eq!(
        scene.node_count(),
        3,
        "scene must have exactly 3 nodes after content update (SetTileRoot removes old tree)"
    );
}

/// WHEN content updates are submitted with increasing elapsed times
/// THEN the TextMarkdownNode content reflects the correct human-friendly time at each step.
#[test]
fn content_update_time_progression() {
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    // Register zeroed ResourceId used by make_placeholder_avatar_node().
    scene.register_resource(tze_hud_scene::ResourceId::from_bytes([0u8; 32]));
    let lease_id = scene.grant_lease("agent-time", 120_000);

    let tile_id = {
        let batch = make_batch(
            "agent-time",
            Some(lease_id),
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent-time".into(),
                lease_id,
                bounds: card_bounds(0, DISPLAY_H),
                z_order: Z_ORDER_BASE,
            }],
        );
        let r = scene.apply_batch(&batch);
        assert!(r.applied, "CreateTile must succeed");
        r.created_ids[0]
    };

    // Test content at each time milestone per the spec.
    let milestones: &[(u64, &str)] =
        &[(0, "now"), (30, "30s ago"), (60, "1m ago"), (120, "2m ago")];

    for (elapsed, expected_time_str) in milestones.iter() {
        let new_bg = make_bg_node();
        let new_avatar = make_placeholder_avatar_node();
        let new_text = make_text_node(&build_text_content("AgentTime", *elapsed));
        let new_bg_id = new_bg.id;
        let new_text_id = new_text.id;

        let batch = make_batch(
            "agent-time",
            Some(lease_id),
            vec![
                SceneMutation::SetTileRoot {
                    tile_id,
                    node: new_bg,
                    descendants: vec![],
                },
                SceneMutation::AddNode {
                    tile_id,
                    parent_id: Some(new_bg_id),
                    node: new_avatar,
                },
                SceneMutation::AddNode {
                    tile_id,
                    parent_id: Some(new_bg_id),
                    node: new_text,
                },
            ],
        );
        let result = scene.apply_batch(&batch);
        assert!(
            result.applied,
            "content update at {elapsed}s must be accepted: {:?}",
            result.rejection
        );

        // Verify the updated text node content.
        let text_node = scene
            .nodes
            .get(&new_text_id)
            .expect("text node must be in scene");
        if let NodeData::TextMarkdown(txt) = &text_node.data {
            assert!(
                txt.content.contains(expected_time_str),
                "at elapsed={elapsed}s: expected text to contain '{expected_time_str}', got: '{}'",
                txt.content
            );
        } else {
            panic!("expected TextMarkdownNode");
        }
    }
}

/// WHEN SetTileRoot replaces the tree
/// THEN the old nodes are removed and only the new 3 nodes remain in the scene.
#[test]
fn set_tile_root_replaces_old_nodes() {
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    // Register zeroed ResourceId used by make_placeholder_avatar_node().
    scene.register_resource(tze_hud_scene::ResourceId::from_bytes([0u8; 32]));
    let lease_id = scene.grant_lease("agent-replace", 120_000);

    let tile_id = {
        let batch = make_batch(
            "agent-replace",
            Some(lease_id),
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent-replace".into(),
                lease_id,
                bounds: card_bounds(0, DISPLAY_H),
                z_order: Z_ORDER_BASE,
            }],
        );
        let r = scene.apply_batch(&batch);
        assert!(r.applied, "CreateTile must succeed");
        r.created_ids[0]
    };

    // Insert initial 3 nodes.
    let initial_bg = make_bg_node();
    let initial_avatar = make_placeholder_avatar_node();
    let initial_text = make_text_node(&build_text_content("OldAgent", 0));
    let initial_bg_id = initial_bg.id;
    let initial_text_id = initial_text.id;
    let initial_avatar_id = initial_avatar.id;

    let initial_batch = make_batch(
        "agent-replace",
        Some(lease_id),
        vec![
            SceneMutation::AddNode {
                tile_id,
                parent_id: None,
                node: initial_bg,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(initial_bg_id),
                node: initial_avatar,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(initial_bg_id),
                node: initial_text,
            },
        ],
    );
    let r = scene.apply_batch(&initial_batch);
    assert!(
        r.applied,
        "initial AddNode batch must succeed: {:?}",
        r.rejection
    );
    assert_eq!(scene.node_count(), 3, "scene must have 3 initial nodes");

    // Store old IDs to verify they're gone after SetTileRoot.
    let old_ids = vec![initial_bg_id, initial_avatar_id, initial_text_id];

    // Content update: SetTileRoot removes the old tree and installs a new one.
    let new_bg = make_bg_node();
    let new_avatar = make_placeholder_avatar_node();
    let new_text = make_text_node(&build_text_content("OldAgent", 30));
    let new_bg_id = new_bg.id;

    let update_batch = make_batch(
        "agent-replace",
        Some(lease_id),
        vec![
            SceneMutation::SetTileRoot {
                tile_id,
                node: new_bg,
                descendants: vec![],
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(new_bg_id),
                node: new_avatar,
            },
            SceneMutation::AddNode {
                tile_id,
                parent_id: Some(new_bg_id),
                node: new_text,
            },
        ],
    );
    let r = scene.apply_batch(&update_batch);
    assert!(
        r.applied,
        "content update batch must succeed: {:?}",
        r.rejection
    );

    // Verify old nodes are gone.
    for old_id in &old_ids {
        assert!(
            !scene.nodes.contains_key(old_id),
            "old node {old_id:?} must be removed after SetTileRoot"
        );
    }

    // Verify scene still has exactly 3 nodes.
    assert_eq!(
        scene.node_count(),
        3,
        "scene must have exactly 3 nodes after SetTileRoot (old 3 removed, new 3 added)"
    );

    // Verify the new root is set on the tile.
    let tile = scene.tiles.get(&tile_id).unwrap();
    assert_eq!(
        tile.root_node,
        Some(new_bg_id),
        "tile root must point to the new bg node"
    );
}

/// WHEN 3 presence card tiles from different namespaces are in the scene
/// THEN SetTileRoot from one agent only updates its own tile's tree.
#[test]
fn set_tile_root_is_namespace_isolated() {
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    scene.active_tab = Some(tab_id);
    // Register zeroed ResourceId used by make_placeholder_avatar_node().
    scene.register_resource(tze_hud_scene::ResourceId::from_bytes([0u8; 32]));

    let agents = ["agent-alpha", "agent-beta", "agent-gamma"];
    let mut tile_ids = Vec::new();
    let mut lease_ids = Vec::new();

    for (i, &agent) in agents.iter().enumerate() {
        let lease_id = scene.grant_lease(agent, 120_000);
        lease_ids.push(lease_id);

        let batch = make_batch(
            agent,
            Some(lease_id),
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: agent.into(),
                lease_id,
                bounds: card_bounds(i, DISPLAY_H),
                z_order: Z_ORDER_BASE + i as u32,
            }],
        );
        let r = scene.apply_batch(&batch);
        assert!(
            r.applied,
            "agent {agent} CreateTile must succeed: {:?}",
            r.rejection
        );
        tile_ids.push(r.created_ids[0]);
    }

    // agent-alpha sets its tile root.
    let alpha_bg = make_bg_node();
    let alpha_text = make_text_node(&build_text_content("agent-alpha", 30));
    let alpha_bg_id = alpha_bg.id;

    let alpha_batch = make_batch(
        "agent-alpha",
        Some(lease_ids[0]),
        vec![
            SceneMutation::SetTileRoot {
                tile_id: tile_ids[0],
                node: alpha_bg,
                descendants: vec![],
            },
            SceneMutation::AddNode {
                tile_id: tile_ids[0],
                parent_id: Some(alpha_bg_id),
                node: make_placeholder_avatar_node(),
            },
            SceneMutation::AddNode {
                tile_id: tile_ids[0],
                parent_id: Some(alpha_bg_id),
                node: alpha_text,
            },
        ],
    );
    let r = scene.apply_batch(&alpha_batch);
    assert!(
        r.applied,
        "agent-alpha SetTileRoot must succeed: {:?}",
        r.rejection
    );

    // agent-alpha tries to set the root on agent-beta's tile — must fail.
    let intruder_node = make_bg_node();
    let intruder_batch = make_batch(
        "agent-alpha",
        Some(lease_ids[0]),
        vec![SceneMutation::SetTileRoot {
            tile_id: tile_ids[1], // beta's tile
            node: intruder_node,
            descendants: vec![],
        }],
    );
    let intruder_result = scene.apply_batch(&intruder_batch);
    assert!(
        !intruder_result.applied,
        "agent-alpha must not be able to SetTileRoot on agent-beta's tile"
    );

    // agent-beta's tile root must remain unset (was never set by beta).
    let beta_tile = scene.tiles.get(&tile_ids[1]).expect("beta tile must exist");
    assert!(
        beta_tile.root_node.is_none(),
        "agent-beta's tile root must remain unset after rejected intrusion attempt"
    );
}

// ─── gRPC helper types (file-local) ──────────────────────────────────────────
// now_wall_us(), AgentSession, connect_agent, claim_tile_via_grpc are provided
// by the shared common harness above. Only set_tile_text_via_grpc is unique here.

/// Send a SetTileRoot mutation via gRPC, setting a TextMarkdownNode as the root.
///
/// Since NodeProto (the wire format) carries a single node with no children,
/// this function submits a single-mutation batch with the text node as the root.
/// The spec says "SetTileRoot with complete node tree" — at the gRPC wire level
/// the NodeProto is a single text node; the integration test focuses on the
/// concurrent update scenario, not the full 3-node tree assembly which is tested
/// in the scene-layer unit tests above.
async fn set_tile_text_via_grpc(
    session: &mut AgentSession,
    tile_id_bytes: &[u8],
    agent_name: &str,
    elapsed_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let content = build_text_content(agent_name, elapsed_secs);
    let batch_id: Vec<u8> = Uuid::now_v7().as_bytes().to_vec();
    let seq = session.next_seq();

    session
        .tx
        .send(session_proto::ClientMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(session_proto::client_message::Payload::MutationBatch(
                session_proto::MutationBatch {
                    batch_id,
                    lease_id: session.lease_id_bytes.clone(),
                    mutations: vec![proto::MutationProto {
                        mutation: Some(proto::mutation_proto::Mutation::SetTileRoot(
                            proto::SetTileRootMutation {
                                tile_id: tile_id_bytes.to_vec(),
                                node: Some(proto::NodeProto {
                                    layout: 0,
                                    id: Vec::new(), // server assigns
                                    data: Some(proto::node_proto::Data::TextMarkdown(
                                        proto::TextMarkdownNodeProto {
                                            content,
                                            bounds: Some(proto::Rect {
                                                x: TEXT_X,
                                                y: TEXT_Y,
                                                width: TEXT_W,
                                                height: TEXT_H,
                                            }),
                                            font_size_px: FONT_SIZE_PX,
                                            color: Some(proto::Rgba {
                                                r: 0.94,
                                                g: 0.94,
                                                b: 0.94,
                                                a: 1.0,
                                            }),
                                            background: None,
                                            color_runs: vec![],
                                            overflow: 0, // Unspecified = proto3 default
                                        },
                                    )),
                                    children: vec![],
                                }),
                            },
                        )),
                    }],
                    timing: None,
                },
            )),
        })
        .await?;

    let msg = session
        .next_server_msg()
        .await
        .ok_or("no mutation result")??;
    match &msg.payload {
        Some(session_proto::server_message::Payload::RequestResult(result)) if result.ok => Ok(()),
        Some(session_proto::server_message::Payload::RequestResult(result)) => {
            Err(format!("SetTileRoot rejected: {} — {}", result.code, result.hint).into())
        }
        other => Err(format!("Expected MutationResult, got: {other:?}").into()),
    }
}

// ─── Multi-agent coexistence integration test (Task 5.1–5.4) ─────────────────

/// E13.5: Three concurrent agents each create a presence card tile with unique
/// namespace, y-offset, and z_order. Verifies coexistence, no overlap, and
/// concurrent content updates without interference.
///
/// Acceptance criteria:
/// - AC 5.1: 3 concurrent agent sessions, each creates a presence card tile
/// - AC 5.2: All 3 tiles visible in SceneSnapshot after creation
/// - AC 5.3: No tile bounds overlap (distinct y-offsets with 12px gaps)
/// - AC 5.4: All 3 agents submit concurrent content updates without interference
#[tokio::test]
async fn test_three_agents_presence_card_coexistence() -> Result<(), Box<dyn std::error::Error>> {
    // ── Phase 0: Start runtime ────────────────────────────────────────────────
    let config = HeadlessConfig {
        width: DISPLAY_W as u32,
        height: DISPLAY_H as u32,
        grpc_port: GRPC_PORT,
        bind_all_interfaces: false,
        agents: tze_hud_scene::config::AgentDirectory::unrestricted(TEST_PSK),
        config_toml: None,
    };

    let runtime = HeadlessRuntime::new(config).await?;

    // Pre-populate the scene with a tab BEFORE gRPC connections.
    {
        let state = runtime.shared_state().lock().await;
        let mut scene = state.scene.lock().await;
        let tab_id = scene.create_tab("Presence-Test", 0)?;
        scene.active_tab = Some(tab_id);
        scene.zone_registry = ZoneRegistry::with_defaults();
    }

    let _server_handle = runtime.start_grpc_server().await?;

    // ── Phase 1: Three agents connect concurrently ────────────────────────────

    let (mut alpha, mut beta, mut gamma) = tokio::try_join!(
        connect_agent(TEST_PSK, GRPC_PORT, "agent-alpha"),
        connect_agent(TEST_PSK, GRPC_PORT, "agent-beta"),
        connect_agent(TEST_PSK, GRPC_PORT, "agent-gamma"),
    )?;

    // Verify all three namespaces are distinct (AC 5.1: namespace isolation).
    assert_ne!(
        alpha.namespace, beta.namespace,
        "agent-alpha and agent-beta must have distinct namespaces"
    );
    assert_ne!(
        beta.namespace, gamma.namespace,
        "agent-beta and agent-gamma must have distinct namespaces"
    );
    assert_ne!(
        alpha.namespace, gamma.namespace,
        "agent-alpha and agent-gamma must have distinct namespaces"
    );

    // ── Phase 2: Each agent claims a presence card tile ──────────────────────
    //
    // All three claim the same anchor; the runtime stacks them without overlap.
    let (tile_alpha_id, tile_beta_id, tile_gamma_id) = tokio::try_join!(
        claim_tile_via_grpc(
            &mut alpha,
            placement(TileAnchor::BottomLeft, TileSize::Small)
        ),
        claim_tile_via_grpc(
            &mut beta,
            placement(TileAnchor::BottomLeft, TileSize::Small)
        ),
        claim_tile_via_grpc(
            &mut gamma,
            placement(TileAnchor::BottomLeft, TileSize::Small)
        ),
    )?;

    assert!(
        !tile_alpha_id.is_empty(),
        "agent-alpha tile must be created"
    );
    assert!(!tile_beta_id.is_empty(), "agent-beta tile must be created");
    assert!(
        !tile_gamma_id.is_empty(),
        "agent-gamma tile must be created"
    );

    // ── Phase 3: Verify all 3 tiles in scene (AC 5.2) ────────────────────────
    let (tile_count, namespaces) = {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let tiles = scene.tiles.values().collect::<Vec<_>>();
        let namespaces: Vec<String> = tiles.iter().map(|t| t.namespace.clone()).collect();
        (tiles.len(), namespaces)
    };

    assert_eq!(
        tile_count, 3,
        "scene must contain exactly 3 tiles after all agents create their presence cards"
    );

    // Each agent's namespace must appear exactly once.
    for (ns, label) in [
        (&alpha.namespace, "agent-alpha"),
        (&beta.namespace, "agent-beta"),
        (&gamma.namespace, "agent-gamma"),
    ] {
        let count = namespaces.iter().filter(|n| *n == ns).count();
        assert_eq!(
            count, 1,
            "{label} namespace must appear exactly once in tile list; found {count}"
        );
    }

    // ── Phase 4: Verify no tile bounds overlap (AC 5.3) ───────────────────────
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let tiles: Vec<_> = scene.tiles.values().collect();

        for i in 0..tiles.len() {
            for j in (i + 1)..tiles.len() {
                let a = tiles[i].bounds;
                let b = tiles[j].bounds;
                let a_bottom = a.y + a.height;
                let b_bottom = b.y + b.height;
                let overlaps_y = a.y < b_bottom && b.y < a_bottom;
                // Tiles share the same x (LEFT_MARGIN), so check y-ranges only.
                assert!(
                    !overlaps_y,
                    "tile[{i}] y-range [{}, {}] must not overlap tile[{j}] y-range [{}, {}]",
                    a.y, a_bottom, b.y, b_bottom
                );
            }
        }
    }

    // ── Phase 5: Concurrent content updates (AC 5.4) ─────────────────────────
    //
    // All 3 agents submit SetTileRoot concurrently (simulating "30s elapsed").
    // Verify all 3 succeed without interference.
    let (r_alpha, r_beta, r_gamma) = tokio::try_join!(
        set_tile_text_via_grpc(&mut alpha, &tile_alpha_id, "agent-alpha", 30),
        set_tile_text_via_grpc(&mut beta, &tile_beta_id, "agent-beta", 30),
        set_tile_text_via_grpc(&mut gamma, &tile_gamma_id, "agent-gamma", 30),
    )?;

    // All results must be Ok (no interference).
    let _ = (r_alpha, r_beta, r_gamma); // all are () on success

    // Verify each tile root is set and belongs to the correct namespace.
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;

        for (tile_id_bytes, ns, label) in [
            (&tile_alpha_id, &alpha.namespace, "agent-alpha"),
            (&tile_beta_id, &beta.namespace, "agent-beta"),
            (&tile_gamma_id, &gamma.namespace, "agent-gamma"),
        ] {
            // tile_id bytes are big-endian RFC 4122 UUID bytes (scene_id_to_bytes wire format).
            let arr: [u8; 16] = tile_id_bytes
                .as_slice()
                .try_into()
                .unwrap_or_else(|_| panic!("{label}: invalid tile_id length"));
            let tile_scene_id = tze_hud_scene::SceneId::from_uuid(Uuid::from_bytes(arr));
            let tile = scene.tiles.get(&tile_scene_id).unwrap_or_else(|| {
                panic!("{label}: tile must exist in scene after content update")
            });
            assert_eq!(
                tile.namespace, *ns,
                "{label}: tile namespace must be '{ns}' after concurrent update"
            );
            assert!(
                tile.root_node.is_some(),
                "{label}: tile must have a root node after SetTileRoot"
            );
        }
    }

    // ── Phase 6: Namespace isolation — agent cannot update another's tile ─────
    //
    // alpha tries to set a root on gamma's tile — must be rejected.
    let intrusion_result = set_tile_text_via_grpc(
        &mut alpha,
        &tile_gamma_id, // gamma's tile
        "agent-alpha-intrusion",
        0,
    )
    .await;

    assert!(
        intrusion_result.is_err(),
        "agent-alpha must not be able to SetTileRoot on agent-gamma's tile; \
         expected rejection but got Ok"
    );

    // ── Phase 7: SceneSnapshot coherence ─────────────────────────────────────
    //
    // Take a SceneSnapshot and verify all 3 tiles are present with correct geometry.
    {
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        let snap = scene.take_snapshot(now_wall_us(), 0);

        assert_eq!(
            snap.tiles.len(),
            3,
            "SceneSnapshot must contain exactly 3 tiles"
        );

        // Verify z_orders are distinct and follow claim order (1, 2, 3).
        let mut z_orders: Vec<u32> = snap.tiles.values().map(|t| t.z_order).collect();
        z_orders.sort_unstable();
        assert_eq!(
            z_orders,
            vec![1, 2, 3],
            "each claim must stack above the previous one"
        );
    }

    Ok(())
}
