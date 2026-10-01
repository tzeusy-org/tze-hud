//! Resident raw-tile text stream portal pilot surface tests (hud-t98e.2).
//!
//! Covers the phase-0 surface requirements:
//! - collapsed + expanded portal surfaces built only from v1 node types
//! - bounded transcript materialization for expanded viewport
//! - content-layer governance compatibility (privacy redaction + lease/orphan path)

use image::{ImageBuffer, Rgb};
use tze_hud_resource::{
    AgentBudget, CAPABILITY_UPLOAD_RESOURCE, ResourceStore, ResourceStoreConfig, ResourceType,
    UploadId, UploadStartRequest,
};
use tze_hud_scene::{
    Capability, DeliveryPolicy, MAX_MARKDOWN_BYTES, MessageClass, MonoUs, TimestampValidationInput,
    TimingError, TimingHints, WallUs, ZONE_TILE_Z_MIN,
    graph::{MAX_NODES_PER_TILE, SceneGraph},
    lease::LeaseState,
    mutation::{MutationBatch, SceneMutation},
    types::{
        FontFamily, HitRegionNode, ImageFitMode, InputMode, Node, NodeData, Rect, SceneId,
        SolidColorNode, TextAlign, TextMarkdownNode, TextOverflow, TileScrollConfig,
    },
};

const DISPLAY_W: f32 = 1920.0;
const DISPLAY_H: f32 = 1080.0;

const COLLAPSED_W: f32 = 420.0;
const COLLAPSED_H: f32 = 96.0;
const EXPANDED_W: f32 = 720.0;
const EXPANDED_H: f32 = 360.0;
const PORTAL_Z_ORDER: u32 = 160;

const ICON_W: u32 = 24;
const ICON_H: u32 = 24;

const INTERACTION_EXPAND: &str = "portal.expand";
const INTERACTION_COLLAPSE: &str = "portal.collapse";
const INTERACTION_REPLY: &str = "portal.reply";

#[derive(Clone, Debug)]
struct PortalSurfaceState {
    portal_id: String,
    session_title: String,
    history: Vec<String>,
    unread_count: usize,
    typing_active: bool,
    expanded: bool,
    viewport_start_line: usize,
    viewport_max_lines: usize,
}

impl PortalSurfaceState {
    fn clamp_markdown_to_budget(mut markdown: String) -> String {
        if markdown.len() <= MAX_MARKDOWN_BYTES {
            return markdown;
        }

        let mut clamp_at = MAX_MARKDOWN_BYTES;
        while clamp_at > 0 && !markdown.is_char_boundary(clamp_at) {
            clamp_at -= 1;
        }
        markdown.truncate(clamp_at);
        markdown
    }

    fn activity_text(&self) -> String {
        if self.typing_active {
            "typing...".to_string()
        } else if self.unread_count == 0 {
            "idle".to_string()
        } else {
            format!("{} unread", self.unread_count)
        }
    }

    fn bounded_transcript_markdown(&self) -> String {
        let end = (self.viewport_start_line + self.viewport_max_lines).min(self.history.len());
        let mut start = self.viewport_start_line.min(end);
        loop {
            let joined = self.history[start..end].join("\n");
            if joined.len() <= MAX_MARKDOWN_BYTES {
                return joined;
            }
            if start + 1 >= end {
                return Self::clamp_markdown_to_budget(joined);
            }
            start += 1;
        }
    }
}

fn make_batch(namespace: &str, lease_id: SceneId, mutations: Vec<SceneMutation>) -> MutationBatch {
    MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: namespace.to_string(),
        mutations,
        timing_hints: None,
        lease_id: Some(lease_id),
    }
}

async fn upload_png_icon(
    store: &ResourceStore,
    agent_namespace: &str,
) -> tze_hud_scene::ResourceId {
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_fn(ICON_W, ICON_H, |_, _| Rgb([32, 178, 170]));
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("portal icon fixture must encode as PNG");

    let hash = *blake3::hash(&png).as_bytes();
    let upload_id = UploadId::from_bytes(uuid::Uuid::now_v7().into_bytes());
    let stored = store
        .handle_upload_start(UploadStartRequest {
            agent_namespace: agent_namespace.to_string(),
            agent_capabilities: vec![CAPABILITY_UPLOAD_RESOURCE.to_string()],
            agent_budget: AgentBudget {
                texture_bytes_total_limit: 0,
                texture_bytes_total_used: 0,
            },
            upload_id,
            resource_type: ResourceType::ImagePng,
            expected_hash: hash,
            total_size: png.len(),
            inline_data: png,
            width: ICON_W,
            height: ICON_H,
        })
        .await
        .expect("portal icon upload must succeed")
        .expect("inline portal icon upload must complete immediately");
    tze_hud_scene::ResourceId::from_bytes(*stored.resource_id.as_bytes())
}

fn portal_bounds(expanded: bool) -> Rect {
    if expanded {
        Rect::new(48.0, 160.0, EXPANDED_W, EXPANDED_H)
    } else {
        Rect::new(48.0, 160.0, COLLAPSED_W, COLLAPSED_H)
    }
}

fn build_collapsed_nodes(
    state: &PortalSurfaceState,
    icon_id: tze_hud_scene::ResourceId,
) -> Vec<Node> {
    let root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: tze_hud_scene::Rgba::new(0.10, 0.12, 0.16, 0.88),
            bounds: Rect::new(0.0, 0.0, COLLAPSED_W, COLLAPSED_H),
            radius: None,
        }),
    };
    let title = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: format!("**{}**", state.session_title),
            bounds: Rect::new(44.0, 10.0, COLLAPSED_W - 120.0, 24.0),
            font_size_px: 15.0,
            font_family: FontFamily::SystemSansSerif,
            color: tze_hud_scene::Rgba::new(0.96, 0.98, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    let preview = state
        .history
        .last()
        .cloned()
        .unwrap_or_else(|| "<empty stream>".to_string());
    let preview = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: preview,
            bounds: Rect::new(44.0, 40.0, COLLAPSED_W - 120.0, 18.0),
            font_size_px: 12.0,
            font_family: FontFamily::SystemMonospace,
            color: tze_hud_scene::Rgba::new(0.82, 0.88, 0.94, 0.96),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    let activity = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: state.activity_text(),
            bounds: Rect::new(COLLAPSED_W - 130.0, 64.0, 80.0, 18.0),
            font_size_px: 11.0,
            font_family: FontFamily::SystemSansSerif,
            color: tze_hud_scene::Rgba::new(0.48, 0.95, 0.68, 0.96),
            background: None,
            alignment: TextAlign::End,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let icon = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::StaticImage(tze_hud_scene::StaticImageNode {
            resource_id: icon_id,
            width: ICON_W,
            height: ICON_H,
            decoded_bytes: (ICON_W as u64) * (ICON_H as u64) * 4,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(12.0, 10.0, 24.0, 24.0),
        }),
    };
    let expand_hit = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(COLLAPSED_W - 46.0, 10.0, 34.0, 24.0),
            interaction_id: INTERACTION_EXPAND.to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            ..Default::default()
        }),
    };

    vec![root, icon, title, preview, activity, expand_hit]
}

fn build_expanded_nodes(
    state: &PortalSurfaceState,
    icon_id: tze_hud_scene::ResourceId,
) -> Vec<Node> {
    let root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: tze_hud_scene::Rgba::new(0.08, 0.10, 0.13, 0.92),
            bounds: Rect::new(0.0, 0.0, EXPANDED_W, EXPANDED_H),
            radius: None,
        }),
    };
    let transcript_text = state.bounded_transcript_markdown();
    let transcript = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: transcript_text,
            bounds: Rect::new(12.0, 44.0, EXPANDED_W - 24.0, EXPANDED_H - 108.0),
            font_size_px: 13.0,
            font_family: FontFamily::SystemMonospace,
            color: tze_hud_scene::Rgba::new(0.90, 0.94, 1.0, 0.98),
            background: Some(tze_hud_scene::Rgba::new(0.03, 0.04, 0.06, 0.78)),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let title = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: format!("{} · {}", state.portal_id, state.activity_text()),
            bounds: Rect::new(44.0, 10.0, EXPANDED_W - 180.0, 24.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemSansSerif,
            color: tze_hud_scene::Rgba::new(0.96, 0.98, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };
    let icon = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::StaticImage(tze_hud_scene::StaticImageNode {
            resource_id: icon_id,
            width: ICON_W,
            height: ICON_H,
            decoded_bytes: (ICON_W as u64) * (ICON_H as u64) * 4,
            fit_mode: ImageFitMode::Contain,
            bounds: Rect::new(12.0, 10.0, 24.0, 24.0),
        }),
    };
    let reply_label = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "Reply".to_string(),
            bounds: Rect::new(EXPANDED_W - 140.0, EXPANDED_H - 38.0, 60.0, 20.0),
            font_size_px: 12.0,
            font_family: FontFamily::SystemSansSerif,
            color: tze_hud_scene::Rgba::new(0.72, 0.86, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Center,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let collapse_hit = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(EXPANDED_W - 46.0, 10.0, 34.0, 24.0),
            interaction_id: INTERACTION_COLLAPSE.to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            ..Default::default()
        }),
    };
    let reply_hit = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::HitRegion(HitRegionNode {
            bounds: Rect::new(EXPANDED_W - 150.0, EXPANDED_H - 42.0, 74.0, 28.0),
            interaction_id: INTERACTION_REPLY.to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            ..Default::default()
        }),
    };

    vec![
        root,
        icon,
        title,
        transcript,
        reply_label,
        collapse_hit,
        reply_hit,
    ]
}

fn root_batch_for_tile(tile_id: SceneId, root: Node, children: Vec<Node>) -> Vec<SceneMutation> {
    let root_id = root.id;
    let mut mutations = vec![SceneMutation::SetTileRoot {
        tile_id,
        node: root.clone(),
        descendants: vec![],
    }];
    mutations.extend(children.into_iter().map(|node| SceneMutation::AddNode {
        tile_id,
        parent_id: Some(root_id),
        node,
    }));
    mutations
}

fn materialized_text_nodes(scene: &SceneGraph, tile_id: SceneId) -> Vec<String> {
    let tile = scene.tiles.get(&tile_id).expect("tile must exist");
    let root = tile.root_node.expect("tile must have root node");
    let root_node = scene.nodes.get(&root).expect("root node must exist");
    let mut texts = Vec::new();
    for child in &root_node.children {
        let node = scene.nodes.get(child).expect("child node must exist");
        if let NodeData::TextMarkdown(t) = &node.data {
            texts.push(t.content.clone());
        }
    }
    texts
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PortalEventType {
    InputReceived,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PortalEvent {
    event_type: PortalEventType,
    input_text: String,
}

#[derive(Clone, Debug, Default)]
struct LocalReplyController {
    draft_input: String,
}

impl LocalReplyController {
    fn submit(&mut self) -> Option<PortalEvent> {
        let submitted = self.draft_input.trim().to_string();
        if submitted.is_empty() {
            return None;
        }
        // Local feedback contract: clear draft immediately on submit.
        self.draft_input.clear();
        Some(PortalEvent {
            event_type: PortalEventType::InputReceived,
            input_text: submitted,
        })
    }
}

#[test]
fn expand_and_collapse_toggle_transcript_visibility() {
    let icon_id = tze_hud_scene::ResourceId::from_bytes([0x11; 32]);
    let base = PortalSurfaceState {
        portal_id: "portal://interaction/expand".to_string(),
        session_title: "Interaction Surface".to_string(),
        history: vec![
            "line one".to_string(),
            "line two".to_string(),
            "line three".to_string(),
        ],
        unread_count: 3,
        typing_active: false,
        expanded: false,
        viewport_start_line: 0,
        viewport_max_lines: 16,
    };

    let expanded = PortalSurfaceState {
        expanded: true,
        ..base.clone()
    };
    let expanded_nodes = build_expanded_nodes(&expanded, icon_id);
    let expanded_texts: Vec<String> = expanded_nodes
        .iter()
        .filter_map(|node| {
            if let NodeData::TextMarkdown(t) = &node.data {
                Some(t.content.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(
        expanded_texts.iter().any(|t| t.contains("line one")),
        "expanded state must show transcript markdown"
    );
    assert!(
        expanded_texts.iter().any(|t| t == "Reply"),
        "expanded state must show reply affordance label"
    );

    let collapsed_nodes = build_collapsed_nodes(&base, icon_id);
    let collapsed_texts: Vec<String> = collapsed_nodes
        .iter()
        .filter_map(|node| {
            if let NodeData::TextMarkdown(t) = &node.data {
                Some(t.content.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(
        collapsed_texts.iter().any(|t| t.contains("line three")),
        "collapsed state keeps latest preview line"
    );
    assert!(
        !collapsed_texts.iter().any(|t| t.contains('\n')),
        "collapsed state must not show full transcript block"
    );
}

#[test]
fn reply_submit_clears_local_input_before_adapter_roundtrip() {
    let mut controller = LocalReplyController {
        draft_input: "test input".to_string(),
    };

    let event = controller
        .submit()
        .expect("submit should produce input-received event");
    assert!(
        controller.draft_input.is_empty(),
        "input field must clear immediately for local visual acknowledgement"
    );
    assert_eq!(event.event_type, PortalEventType::InputReceived);
    assert_eq!(event.input_text, "test input");
}

#[test]
fn user_scroll_offset_remains_authoritative_after_append_update() {
    let namespace = "portal-scroll-authority";
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("tab create");
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(
        namespace,
        120_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    let create = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds: portal_bounds(true),
            z_order: PORTAL_Z_ORDER,
        }],
    ));
    assert!(create.applied, "tile create must apply");
    let tile_id = create.created_ids[0];

    scene
        .register_tile_scroll_config(
            tile_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: Some(EXPANDED_W),
                content_height: Some(EXPANDED_H * 3.0),
            },
        )
        .expect("register scroll config");
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 100.0)
        .expect("set local scroll offset");

    let root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: tze_hud_scene::Rgba::new(0.08, 0.10, 0.13, 0.92),
            bounds: Rect::new(0.0, 0.0, EXPANDED_W, EXPANDED_H),
            radius: None,
        }),
    };
    let transcript = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "line 1\nline 2\nline 3".to_string(),
            bounds: Rect::new(12.0, 44.0, EXPANDED_W - 24.0, EXPANDED_H - 108.0),
            font_size_px: 13.0,
            font_family: FontFamily::SystemMonospace,
            color: tze_hud_scene::Rgba::new(0.90, 0.94, 1.0, 0.98),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let append = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        root_batch_for_tile(tile_id, root, vec![transcript]),
    ));
    assert!(append.applied, "append update should apply");

    let (sx, sy) = scene.tile_scroll_offset_local(tile_id);
    assert_eq!((sx, sy), (0.0, 100.0));
}

fn collapsed_activity_node(nodes: &[Node]) -> &TextMarkdownNode {
    nodes
        .iter()
        .find_map(|node| match &node.data {
            NodeData::TextMarkdown(text)
                if text.bounds == Rect::new(COLLAPSED_W - 130.0, 64.0, 80.0, 18.0) =>
            {
                Some(text)
            }
            _ => None,
        })
        .expect("collapsed portal nodes must include activity indicator text")
}

fn find_text_node_in_tile(scene: &SceneGraph, tile_id: SceneId, content: &str) -> SceneId {
    let tile = scene.tiles.get(&tile_id).expect("tile must exist");
    let root = tile.root_node.expect("tile must have root node");
    let root_node = scene.nodes.get(&root).expect("root node must exist");
    root_node
        .children
        .iter()
        .copied()
        .find(|child_id| {
            scene
                .nodes
                .get(child_id)
                .and_then(|node| match &node.data {
                    NodeData::TextMarkdown(text) => Some(text.content.as_str() == content),
                    _ => None,
                })
                .unwrap_or(false)
        })
        .expect("tile must contain requested text node")
}

fn text_markdown_from_node(scene: &SceneGraph, node_id: SceneId) -> TextMarkdownNode {
    let node = scene.nodes.get(&node_id).expect("node must exist");
    match &node.data {
        NodeData::TextMarkdown(text) => text.clone(),
        other => panic!("expected text markdown node, got: {other:?}"),
    }
}
#[tokio::test]
async fn collapsed_and_expanded_portal_surface_use_only_v1_node_types() {
    let namespace = "portal-agent";
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let icon_id = upload_png_icon(&store, namespace).await;

    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("must create tab");
    scene.active_tab = Some(tab_id);
    scene.register_resource(icon_id);
    let lease_id = scene.grant_lease(
        namespace,
        120_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );

    let create = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds: portal_bounds(false),
            z_order: PORTAL_Z_ORDER,
        }],
    ));
    assert!(create.applied, "collapsed portal tile must be creatable");
    let tile_id = create.created_ids[0];
    const _PORTAL_BELOW_ZONE_BAND: () = assert!(
        PORTAL_Z_ORDER < ZONE_TILE_Z_MIN,
        "portal pilot tile must stay below runtime-managed zone band"
    );
    assert_ne!(
        scene.leases[&lease_id].priority, 0,
        "portal pilot must remain content-layer, not chrome lease-priority 0"
    );

    let collapsed = PortalSurfaceState {
        portal_id: "portal://pilot/1".to_string(),
        session_title: "Resident Text Stream".to_string(),
        history: vec!["warmup output".to_string(), "portal ready".to_string()],
        unread_count: 1,
        typing_active: false,
        expanded: false,
        viewport_start_line: 0,
        viewport_max_lines: 12,
    };
    assert!(!collapsed.expanded, "collapsed state must be false");
    let mut collapsed_nodes = build_collapsed_nodes(&collapsed, icon_id);
    let collapsed_root = collapsed_nodes.remove(0);
    let collapsed_batch = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        root_batch_for_tile(tile_id, collapsed_root, collapsed_nodes),
    ));
    assert!(collapsed_batch.applied, "collapsed root batch must apply");

    let collapsed_kinds = materialized_text_nodes(&scene, tile_id);
    assert!(
        collapsed_kinds
            .iter()
            .any(|c| c.contains("Resident Text Stream")),
        "collapsed state must include portal identity text in content layer"
    );
    assert!(
        collapsed_kinds.iter().any(|c| c.contains("unread")),
        "collapsed state must include portal activity text in content layer"
    );

    let expanded = PortalSurfaceState {
        expanded: true,
        viewport_start_line: 0,
        viewport_max_lines: 20,
        ..collapsed
    };
    assert!(expanded.expanded, "expanded state must be true");
    let mut expanded_nodes = build_expanded_nodes(&expanded, icon_id);
    let expanded_root = expanded_nodes.remove(0);
    let expanded_batch = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![
            SceneMutation::UpdateTileBounds {
                tile_id,
                bounds: portal_bounds(true),
            },
            SceneMutation::UpdateTileInputMode {
                tile_id,
                input_mode: InputMode::Capture,
            },
        ]
        .into_iter()
        .chain(root_batch_for_tile(tile_id, expanded_root, expanded_nodes))
        .collect(),
    ));
    assert!(expanded_batch.applied, "expanded root batch must apply");

    let tile = scene.tiles.get(&tile_id).expect("tile must still exist");
    assert_eq!(tile.bounds, portal_bounds(true));
    assert_eq!(tile.input_mode, InputMode::Capture);

    let root = tile.root_node.expect("expanded tile must have root");
    let root_node = scene.nodes.get(&root).expect("expanded root must exist");
    for child in &root_node.children {
        let node = scene.nodes.get(child).expect("child must exist");
        match &node.data {
            NodeData::SolidColor(_)
            | NodeData::TextMarkdown(_)
            | NodeData::StaticImage(_)
            | NodeData::HitRegion(_) => {}
        }
    }
}

#[test]
fn expanded_transcript_materialization_is_bounded_to_viewport_and_budget() {
    let history: Vec<String> = (0..240)
        .map(|i| format!("[{i:03}] {}", "x".repeat(420)))
        .collect();
    let state = PortalSurfaceState {
        portal_id: "portal://pilot/2".to_string(),
        session_title: "Budget Window".to_string(),
        history,
        unread_count: 0,
        typing_active: false,
        expanded: true,
        viewport_start_line: 120,
        viewport_max_lines: 80,
    };
    let markdown = state.bounded_transcript_markdown();
    assert!(
        markdown.len() <= MAX_MARKDOWN_BYTES,
        "materialized transcript must stay within TextMarkdown node byte budget"
    );
    let line_count = markdown.lines().count();
    assert!(
        line_count <= state.viewport_max_lines,
        "materialized transcript must not exceed viewport line window"
    );

    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("must create tab");
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(
        "portal-agent",
        120_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    let create = scene.apply_batch(&make_batch(
        "portal-agent",
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: "portal-agent".to_string(),
            lease_id,
            bounds: portal_bounds(true),
            z_order: PORTAL_Z_ORDER,
        }],
    ));
    assert!(create.applied);
    let tile_id = create.created_ids[0];
    scene
        .register_tile_scroll_config(
            tile_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: Some(EXPANDED_W),
                content_height: Some(EXPANDED_H * 2.5),
            },
        )
        .expect("expanded portal tile must allow local-first scroll config");
    let (sx, sy) = scene.tile_scroll_offset_local(tile_id);
    assert_eq!((sx, sy), (0.0, 0.0));
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 48.0)
        .expect("local-first scroll offset must be writable");

    let root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(SolidColorNode {
            color: tze_hud_scene::Rgba::new(0.08, 0.10, 0.13, 0.92),
            bounds: Rect::new(0.0, 0.0, EXPANDED_W, EXPANDED_H),
            radius: None,
        }),
    };
    let transcript = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: markdown,
            bounds: Rect::new(12.0, 44.0, EXPANDED_W - 24.0, EXPANDED_H - 108.0),
            font_size_px: 13.0,
            font_family: FontFamily::SystemMonospace,
            color: tze_hud_scene::Rgba::new(0.90, 0.94, 1.0, 0.98),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let apply = scene.apply_batch(&make_batch(
        "portal-agent",
        lease_id,
        root_batch_for_tile(tile_id, root, vec![transcript]),
    ));
    assert!(
        apply.applied,
        "bounded expanded transcript batch must apply"
    );
    let usage = scene.lease_resource_usage(&lease_id);
    let tile_nodes = usage.nodes_per_tile.get(&tile_id).copied().unwrap_or(0);
    assert!(
        (tile_nodes as usize) <= MAX_NODES_PER_TILE,
        "expanded pilot node count must stay under per-tile node budget"
    );
}

#[tokio::test]
async fn portal_surface_state_remains_governed_by_orphan_rules() {
    let namespace = "portal-agent-governed";
    let store = ResourceStore::new(ResourceStoreConfig::default());
    let icon_id = upload_png_icon(&store, namespace).await;

    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("must create tab");
    scene.active_tab = Some(tab_id);
    scene.register_resource(icon_id);
    let lease_id = scene.grant_lease(
        namespace,
        120_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    let create = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds: portal_bounds(false),
            z_order: PORTAL_Z_ORDER,
        }],
    ));
    assert!(create.applied);
    let tile_id = create.created_ids[0];
    let collapsed = PortalSurfaceState {
        portal_id: "portal://gov/1".to_string(),
        session_title: "Governed".to_string(),
        history: vec!["sensitive response".to_string()],
        unread_count: 2,
        typing_active: false,
        expanded: false,
        viewport_start_line: 0,
        viewport_max_lines: 8,
    };
    let mut nodes = build_collapsed_nodes(&collapsed, icon_id);
    let root = nodes.remove(0);
    assert!(
        scene
            .apply_batch(&make_batch(
                namespace,
                lease_id,
                root_batch_for_tile(tile_id, root, nodes),
            ))
            .applied
    );

    scene
        .disconnect_lease(&lease_id, 10_000)
        .expect("disconnect should transition lease into orphan path");
    assert_eq!(
        scene.leases[&lease_id].state,
        LeaseState::Orphaned,
        "portal lease must enter normal orphan path on disconnect"
    );

    let rejected = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::UpdateTileBounds {
            tile_id,
            bounds: Rect::new(48.0, 220.0, COLLAPSED_W, COLLAPSED_H),
        }],
    ));
    assert!(
        !rejected.applied,
        "portal tile must not bypass existing lease/orphan governance after disconnect"
    );
}

#[test]
fn portal_activity_indicators_remain_ambient_under_backlog_and_typing() {
    let icon_id = tze_hud_scene::ResourceId::from_bytes([0xAB; 32]);
    let base = PortalSurfaceState {
        portal_id: "portal://ambient/1".to_string(),
        session_title: "Ambient".to_string(),
        history: vec!["stream active".to_string()],
        unread_count: 1,
        typing_active: false,
        expanded: false,
        viewport_start_line: 0,
        viewport_max_lines: 8,
    };

    let low_backlog_nodes = build_collapsed_nodes(&base, icon_id);
    let high_backlog_nodes = build_collapsed_nodes(
        &PortalSurfaceState {
            unread_count: 500,
            ..base.clone()
        },
        icon_id,
    );
    let typing_nodes = build_collapsed_nodes(
        &PortalSurfaceState {
            unread_count: 500,
            typing_active: true,
            ..base.clone()
        },
        icon_id,
    );

    let low_activity = collapsed_activity_node(&low_backlog_nodes);
    let high_activity = collapsed_activity_node(&high_backlog_nodes);
    let typing_activity = collapsed_activity_node(&typing_nodes);

    assert_eq!(
        low_activity.color, high_activity.color,
        "backlog growth must not auto-upgrade portal activity indicator styling"
    );
    assert_eq!(
        typing_activity.color, low_activity.color,
        "typing indicator must stay ambient and not switch to urgency-escalated styling"
    );
    assert_eq!(
        typing_activity.content, "typing...",
        "typing indicator should be transient status text, not notification-style urgency text"
    );
}

#[test]
fn portal_typing_indicator_updates_use_transient_in_place_path() {
    let namespace = "portal-agent-typing";
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("must create tab");
    scene.active_tab = Some(tab_id);
    let icon_id = tze_hud_scene::ResourceId::from_bytes([0xCD; 32]);
    scene.register_resource(icon_id);
    let lease_id = scene.grant_lease(
        namespace,
        120_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    let create = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds: portal_bounds(false),
            z_order: PORTAL_Z_ORDER,
        }],
    ));
    assert!(create.applied);
    let tile_id = create.created_ids[0];

    let state = PortalSurfaceState {
        portal_id: "portal://typing/1".to_string(),
        session_title: "Typing".to_string(),
        history: vec!["hello".to_string()],
        unread_count: 2,
        typing_active: true,
        expanded: false,
        viewport_start_line: 0,
        viewport_max_lines: 8,
    };
    let mut nodes = build_collapsed_nodes(&state, icon_id);
    let root = nodes.remove(0);
    let apply = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        root_batch_for_tile(tile_id, root, nodes),
    ));
    assert!(apply.applied);
    let activity_node_id = find_text_node_in_tile(&scene, tile_id, "typing...");
    let nodes_before = scene
        .lease_resource_usage(&lease_id)
        .nodes_per_tile
        .get(&tile_id)
        .copied()
        .unwrap_or(0);

    for content in ["typing...", "57 unread", "idle"] {
        let mut text = text_markdown_from_node(&scene, activity_node_id);
        text.content = content.to_string();
        let update = scene.apply_batch(&make_batch(
            namespace,
            lease_id,
            vec![SceneMutation::UpdateNodeContent {
                tile_id,
                node_id: activity_node_id,
                data: NodeData::TextMarkdown(text),
            }],
        ));
        assert!(
            update.applied,
            "activity update must succeed as transient in-place content refresh"
        );
        assert!(
            update.created_ids.is_empty(),
            "typing/activity refresh must not allocate new scene nodes"
        );
    }

    let nodes_after = scene
        .lease_resource_usage(&lease_id)
        .nodes_per_tile
        .get(&tile_id)
        .copied()
        .unwrap_or(0);
    assert_eq!(
        nodes_after, nodes_before,
        "typing/activity indicator updates should not take a transactional structural path"
    );
    let final_text = text_markdown_from_node(&scene, activity_node_id);
    assert_eq!(final_text.content, "idle");
    assert!(
        scene.zone_registry.active_publishes.is_empty(),
        "typing indicator path must remain raw-tile ambient state, not notification publishes"
    );
}

#[test]
fn portal_typing_indicator_timing_profile_is_ephemeral_realtime() {
    let mut typing_hints = TimingHints::new();
    typing_hints.message_class = MessageClass::EphemeralRealtime;
    typing_hints.delivery_policy = DeliveryPolicy::DropIfLate;

    let now = 2_000_000_000_u64;
    let ctx = TimestampValidationInput {
        session_open_wall_us: WallUs(now),
        now_wall_us: WallUs(now),
        max_future_schedule_us: 300_000_000,
        estimated_skew_us: 0,
    };
    assert!(
        tze_hud_scene::validate_timing_hints(&typing_hints, &ctx).is_ok(),
        "portal typing indicators should use valid ephemeral-realtime timing semantics"
    );

    let mut transactional = typing_hints.clone();
    transactional.message_class = MessageClass::Transactional;
    let err = tze_hud_scene::validate_timing_hints(&transactional, &ctx)
        .expect_err("drop-if-late must reject transactional message class");
    assert_eq!(err, TimingError::InvalidDeliveryPolicy);
}

// ─── Scroll-offset seam tests (hud-w5ih) ─────────────────────────────────────
//
// These tests cover the Transcript Interaction Contract and Bounded Transcript
// Viewport requirements from openspec/specs/text-stream-portals/spec.md without
// requiring live pointer events (hud-dih4 pointer capture not needed). They drive
// scroll state directly via InputProcessor::process_scroll_event and
// set_tile_scroll_offset_local, bypassing the OS input path.

/// Direct `process_scroll_event` call updates the portal tile scroll offset
/// local-first, without waiting for any adapter response.
///
/// AC: local-first scroll offset updates before adapter ack.
#[test]
fn portal_scroll_updates_local_first_via_input_processor() {
    use tze_hud_input::{InputProcessor, ScrollEvent};

    let namespace = "portal-scroll-seam";
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("tab create");
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(
        namespace,
        120_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    let create = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds: portal_bounds(true),
            z_order: PORTAL_Z_ORDER,
        }],
    ));
    assert!(create.applied);
    let tile_id = create.created_ids[0];
    let tile_bounds = portal_bounds(true);

    // Register a scroll config so the tile is scrollable.
    scene
        .register_tile_scroll_config(
            tile_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: Some(EXPANDED_W),
                content_height: Some(EXPANDED_H * 4.0),
            },
        )
        .expect("register scroll config");

    let mut input_processor = InputProcessor::new();

    // Fire a scroll event whose (x, y) hits inside the expanded portal tile.
    // Use the tile's center as the hit point so hit-test resolves to the tile.
    let hit_x = tile_bounds.x + tile_bounds.width / 2.0;
    let hit_y = tile_bounds.y + tile_bounds.height / 2.0;
    let delta_y = 60.0_f32;

    let changed_event = input_processor.process_scroll_event(
        &ScrollEvent {
            x: hit_x,
            y: hit_y,
            delta_x: 0.0,
            delta_y,
        },
        &mut scene,
    );

    // The scroll must register on the tile.
    assert!(
        changed_event.is_some(),
        "process_scroll_event must return Some for a scrollable portal tile hit"
    );
    let ev = changed_event.unwrap();
    assert_eq!(
        ev.tile_id, tile_id,
        "changed event must reference the portal tile"
    );
    assert!(
        (ev.offset_y - delta_y).abs() < f32::EPSILON,
        "offset_y must equal delta"
    );

    // The scene's local offset must be updated synchronously (local-first).
    let (sx, sy) = scene.tile_scroll_offset_local(tile_id);
    assert!(
        (sx).abs() < f32::EPSILON,
        "x offset must be zero (axis-locked)"
    );
    assert!(
        (sy - delta_y).abs() < f32::EPSILON,
        "scene scroll offset must equal delta after local-first update; got sy={sy}"
    );
}

/// Multiple scroll events for the same tile coalesce to the latest offset under
/// backpressure. The currently-visible scroll window is preserved (not clobbered
/// to zero).
///
/// AC: Coherent Transcript Coalescing requirement — coalescing under backpressure
/// preserves the currently-visible scroll window.
#[test]
fn portal_scroll_coalescing_preserves_visible_window() {
    // The coalescing pipeline uses `tze_hud_input::envelope::InputEnvelope`
    // (the full multi-variant pipeline type), distinct from the agent-facing
    // `tze_hud_input::events::InputEnvelope` which carries only pointer events.
    use tze_hud_input::FrameCoalescer;
    use tze_hud_input::envelope::{InputEnvelope as PipelineEnvelope, ScrollOffsetChangedData};

    let tile_id = SceneId::new();
    let mut coalescer = FrameCoalescer::default();

    // Simulate several scroll events arriving quickly for the same tile.
    // Only the LAST offset must survive coalescing.
    let offsets: &[f32] = &[20.0, 50.0, 90.0, 130.0, 170.0];
    for (i, &oy) in offsets.iter().enumerate() {
        coalescer.push(PipelineEnvelope::ScrollOffsetChanged(
            ScrollOffsetChangedData {
                tile_id,
                timestamp_mono_us: MonoUs(i as u64 * 1_000),
                offset_x: 0.0,
                offset_y: oy,
            },
        ));
    }

    let events = coalescer.into_events();
    let scroll_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PipelineEnvelope::ScrollOffsetChanged(d) if d.tile_id == tile_id))
        .collect();

    assert_eq!(
        scroll_events.len(),
        1,
        "FrameCoalescer must coalesce multiple scroll events into one"
    );
    if let PipelineEnvelope::ScrollOffsetChanged(d) = scroll_events[0] {
        assert!(
            (d.offset_y - 170.0).abs() < f32::EPSILON,
            "coalesced scroll must carry the latest (largest) offset; got {}",
            d.offset_y
        );
    }
}

/// Scroll offset is clamped to the retained transcript window; the viewer
/// cannot scroll past the content boundary registered in `TileScrollConfig`.
///
/// AC: Bounded Transcript Viewport — runtime limits scroll-offset range.
#[test]
fn portal_scroll_offset_clamped_to_content_boundary() {
    use tze_hud_input::{InputProcessor, ScrollEvent};

    let namespace = "portal-scroll-clamp";
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("tab create");
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(
        namespace,
        120_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    let create = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds: portal_bounds(true),
            z_order: PORTAL_Z_ORDER,
        }],
    ));
    assert!(create.applied);
    let tile_id = create.created_ids[0];
    let tile_bounds = portal_bounds(true);

    let content_height = 300.0_f32;
    scene
        .register_tile_scroll_config(
            tile_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: Some(EXPANDED_W),
                content_height: Some(content_height),
            },
        )
        .expect("register scroll config");

    let mut input_processor = InputProcessor::new();

    let hit_x = tile_bounds.x + tile_bounds.width / 2.0;
    let hit_y = tile_bounds.y + tile_bounds.height / 2.0;

    // Scroll far beyond the content boundary.
    let huge_delta = 9999.0_f32;
    let ev = input_processor.process_scroll_event(
        &ScrollEvent {
            x: hit_x,
            y: hit_y,
            delta_x: 0.0,
            delta_y: huge_delta,
        },
        &mut scene,
    );
    assert!(ev.is_some(), "scroll event must be accepted");
    let ev = ev.unwrap();
    assert!(
        ev.offset_y <= content_height,
        "scroll offset must be clamped to content_height={content_height}; got {}",
        ev.offset_y
    );

    let (_, sy) = scene.tile_scroll_offset_local(tile_id);
    assert!(
        sy <= content_height,
        "scene scroll_y must be clamped to {content_height}; got {sy}"
    );
}

/// Adapter append (content update) while user holds a non-zero scroll offset
/// preserves the user's scroll position (not reset to 0).
///
/// AC: Coherent Transcript Coalescing — adapter continues publishing at the
/// tail while viewer stays at their chosen offset.
#[test]
fn portal_adapter_append_preserves_user_scroll_position() {
    let namespace = "portal-append-preserves-scroll";
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("tab create");
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(
        namespace,
        120_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    let create = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds: portal_bounds(true),
            z_order: PORTAL_Z_ORDER,
        }],
    ));
    assert!(create.applied);
    let tile_id = create.created_ids[0];

    scene
        .register_tile_scroll_config(
            tile_id,
            TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: Some(EXPANDED_W),
                content_height: Some(EXPANDED_H * 5.0),
            },
        )
        .expect("register scroll config");

    // User scrolled to a mid-transcript position.
    let user_scroll_y = 200.0_f32;
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, user_scroll_y)
        .expect("set user scroll offset");

    // Simulate an adapter append: update the tile content (SetTileRoot).
    let root = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::SolidColor(tze_hud_scene::types::SolidColorNode {
            color: tze_hud_scene::Rgba::new(0.08, 0.10, 0.13, 0.92),
            bounds: Rect::new(0.0, 0.0, EXPANDED_W, EXPANDED_H),
            radius: None,
        }),
    };
    let transcript = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: Vec::new(),
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "new tail content appended by adapter".to_string(),
            bounds: Rect::new(12.0, 44.0, EXPANDED_W - 24.0, EXPANDED_H - 108.0),
            font_size_px: 13.0,
            font_family: FontFamily::SystemMonospace,
            color: tze_hud_scene::Rgba::new(0.90, 0.94, 1.0, 0.98),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };
    let append = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        root_batch_for_tile(tile_id, root, vec![transcript]),
    ));
    assert!(append.applied, "adapter append must apply");

    // Scroll offset must be unchanged — the MutationBatch path does NOT
    // reset local-first scroll state.
    let (sx, sy) = scene.tile_scroll_offset_local(tile_id);
    assert!(
        (sx).abs() < f32::EPSILON,
        "x offset must remain 0 after adapter append"
    );
    assert!(
        (sy - user_scroll_y).abs() < f32::EPSILON,
        "user scroll_y must be preserved after adapter append; expected {user_scroll_y} got {sy}"
    );
}

// ─── Click-to-focus + keyboard composer text mutation (hud-opkvq) ─────────────
//
// These tests exercise the full click-to-focus → keyboard dispatch → text
// mutation loop using only the input crate types (no windowed runtime required).
// They prove:
//   1. Clicking a composer HitRegionNode grants focus to that node.
//   2. After focus is granted, KeyboardProcessor produces KeyboardDispatch events
//      targeting the focused node.
//   3. The composer agent processes CharacterEvent payloads to mutate its draft
//      buffer (insert characters, handle Backspace).

/// A minimal in-process simulation of the composer portal agent's text buffer.
///
/// In production, this logic lives in the resident portal agent session. Here
/// we drive it directly to prove the keyboard event contract.
#[derive(Clone, Debug, Default)]
struct ComposerBuffer {
    draft: String,
}

impl ComposerBuffer {
    /// Apply a keyboard dispatch from the runtime to the draft buffer.
    ///
    /// Handles:
    /// - `Character` payload → append text to draft.
    /// - `KeyDown` with `key == "Backspace"` → remove last character.
    /// - `KeyDown` with `key == "Enter"` → submit (returns `Some(draft)` and clears).
    /// - All other events → no-op.
    ///
    /// Returns `Some(submitted)` on Enter, `None` otherwise.
    fn apply_dispatch(&mut self, dispatch: &tze_hud_input::KeyboardDispatch) -> Option<String> {
        use tze_hud_input::KeyboardDispatchKind;
        match &dispatch.kind {
            KeyboardDispatchKind::Character { character, .. } => {
                self.draft.push_str(character);
                None
            }
            KeyboardDispatchKind::KeyDown { key, .. } => {
                match key.as_str() {
                    "Backspace" => {
                        // Remove last Unicode scalar value (not last byte).
                        if let Some(pos) = self.draft.char_indices().next_back().map(|(i, _)| i) {
                            self.draft.truncate(pos);
                        }
                        None
                    }
                    "Enter" => {
                        let submitted = self.draft.trim().to_string();
                        self.draft.clear();
                        if submitted.is_empty() {
                            None
                        } else {
                            Some(submitted)
                        }
                    }
                    _ => None,
                }
            }
            KeyboardDispatchKind::KeyUp { .. } => None,
        }
    }
}

/// Clicking a composer HitRegionNode with `accepts_focus=true` grants focus
/// to that node, and subsequent KeyboardProcessor calls target it.
#[test]
fn click_to_focus_grants_focus_to_composer_hit_region() {
    use tze_hud_input::{FocusManager, FocusOwner, InputProcessor, PointerEvent, PointerEventKind};
    use tze_hud_scene::{Capability, HitRegionNode, InputMode, Node, NodeData, Rect, SceneGraph};

    let namespace = "composer-agent";
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("tab create");
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(
        namespace,
        120_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );

    // Create the expanded portal tile in Capture mode (required for keyboard focus).
    let create = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds: portal_bounds(true),
            z_order: PORTAL_Z_ORDER,
        }],
    ));
    assert!(create.applied);
    let tile_id = create.created_ids[0];

    // Explicitly set input_mode to Capture so the tile accepts focus.
    scene.tiles.get_mut(&tile_id).unwrap().input_mode = InputMode::Capture;

    // Add the composer HitRegionNode as the tile root.
    let composer_node_id = tze_hud_scene::SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: composer_node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, EXPANDED_H - 42.0, EXPANDED_W - 20.0, 28.0),
                    interaction_id: INTERACTION_REPLY.to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .expect("set tile root");

    let mut input_processor = InputProcessor::new();
    let mut focus_manager = FocusManager::new();
    focus_manager.add_tab(tab_id);

    // Before any click, focus must be None.
    assert_eq!(
        focus_manager.current_owner(tab_id),
        &FocusOwner::None,
        "no click yet — focus must be None"
    );

    // Simulate a pointer-down inside the composer hit region.
    // The composer node is at (48 + 0, 160 + EXPANDED_H - 42) in display space.
    let tile_origin_x = portal_bounds(true).x;
    let tile_origin_y = portal_bounds(true).y;
    let click_x = tile_origin_x + EXPANDED_W / 2.0;
    let click_y = tile_origin_y + EXPANDED_H - 28.0; // inside composer row

    let down = PointerEvent {
        x: click_x,
        y: click_y,
        kind: PointerEventKind::Down,
        device_id: 0,
        timestamp: None,
    };

    let (_result, focus_transition) =
        input_processor.process_with_focus(&down, &mut scene, &mut focus_manager, tab_id);

    // Focus must have been granted to the composer node.
    assert!(
        focus_transition.is_some(),
        "pointer-down on focusable hit region must produce a focus transition"
    );
    assert_eq!(
        focus_manager.current_owner(tab_id),
        &FocusOwner::Node {
            tile_id,
            node_id: composer_node_id,
        },
        "focus must be on the composer hit region node after click"
    );
}

/// After click-to-focus, `KeyboardProcessor` produces `KeyboardDispatch`
/// payloads targeting the focused node, and the `ComposerBuffer` can mutate
/// its draft text in response.
#[test]
fn keyboard_processor_delivers_to_focused_composer_and_buffer_mutates() {
    use tze_hud_input::{
        FocusManager, FocusOwner, InputProcessor, KeyboardModifiers, KeyboardProcessor,
        PointerEvent, PointerEventKind, RawCharacterEvent, RawKeyDownEvent,
    };
    use tze_hud_scene::{
        Capability, HitRegionNode, InputMode, MonoUs, Node, NodeData, Rect, SceneGraph,
    };

    let namespace = "composer-kb-agent";
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("tab create");
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(
        namespace,
        120_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    let create = scene.apply_batch(&make_batch(
        namespace,
        lease_id,
        vec![SceneMutation::CreateTile {
            tab_id,
            namespace: namespace.to_string(),
            lease_id,
            bounds: portal_bounds(true),
            z_order: PORTAL_Z_ORDER,
        }],
    ));
    assert!(create.applied);
    let tile_id = create.created_ids[0];
    scene.tiles.get_mut(&tile_id).unwrap().input_mode = InputMode::Capture;

    let composer_node_id = tze_hud_scene::SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: composer_node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, EXPANDED_H - 42.0, EXPANDED_W - 20.0, 28.0),
                    interaction_id: INTERACTION_REPLY.to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .expect("set tile root");

    // Click-to-focus: wire focus onto the composer node.
    let mut input_processor = InputProcessor::new();
    let mut focus_manager = FocusManager::new();
    focus_manager.add_tab(tab_id);

    let tile_x = portal_bounds(true).x;
    let tile_y = portal_bounds(true).y;
    let click_x = tile_x + EXPANDED_W / 2.0;
    let click_y = tile_y + EXPANDED_H - 28.0;

    let down = PointerEvent {
        x: click_x,
        y: click_y,
        kind: PointerEventKind::Down,
        device_id: 0,
        timestamp: None,
    };
    input_processor.process_with_focus(&down, &mut scene, &mut focus_manager, tab_id);

    assert_eq!(
        focus_manager.current_owner(tab_id),
        &FocusOwner::Node {
            tile_id,
            node_id: composer_node_id
        },
        "pre-condition: composer node must be focused"
    );

    // ── Keyboard drain ────────────────────────────────────────────────────
    let kb = KeyboardProcessor::new();
    let focus_owner = focus_manager.current_owner(tab_id).clone();
    let ns_fn = |_: tze_hud_scene::SceneId| -> Option<String> { Some(namespace.to_string()) };

    let mut buffer = ComposerBuffer::default();
    let ts = MonoUs(1_000);

    // Type "hello" via CharacterEvent payloads.
    for ch in ["h", "e", "l", "l", "o"] {
        let raw = RawCharacterEvent {
            character: ch.to_string(),
            timestamp_mono_us: ts,
        };
        if let Some(dispatch) = kb.process_character(&raw, &focus_owner, ns_fn) {
            assert_eq!(dispatch.tile_id, tile_id);
            assert_eq!(dispatch.node_id, Some(composer_node_id));
            buffer.apply_dispatch(&dispatch);
        }
    }
    assert_eq!(
        buffer.draft, "hello",
        "draft must accumulate typed characters"
    );

    // Backspace: erase last character.
    let backspace = RawKeyDownEvent {
        key_code: "Backspace".to_string(),
        key: "Backspace".to_string(),
        modifiers: KeyboardModifiers::NONE,
        repeat: false,
        timestamp_mono_us: ts,
    };
    if let Some(dispatch) = kb.process_key_down(&backspace, &focus_owner, ns_fn) {
        buffer.apply_dispatch(&dispatch);
    }
    assert_eq!(buffer.draft, "hell", "backspace must remove last character");

    // Type " world" (space then "world").
    for ch in [" ", "w", "o", "r", "l", "d"] {
        let raw = RawCharacterEvent {
            character: ch.to_string(),
            timestamp_mono_us: ts,
        };
        if let Some(dispatch) = kb.process_character(&raw, &focus_owner, ns_fn) {
            buffer.apply_dispatch(&dispatch);
        }
    }
    assert_eq!(buffer.draft, "hell world");

    // Enter: submit and clear.
    let enter = RawKeyDownEvent {
        key_code: "Enter".to_string(),
        key: "Enter".to_string(),
        modifiers: KeyboardModifiers::NONE,
        repeat: false,
        timestamp_mono_us: ts,
    };
    let submitted = if let Some(dispatch) = kb.process_key_down(&enter, &focus_owner, ns_fn) {
        buffer.apply_dispatch(&dispatch)
    } else {
        None
    };
    assert_eq!(
        submitted.as_deref(),
        Some("hell world"),
        "Enter must submit the accumulated draft"
    );
    assert!(
        buffer.draft.is_empty(),
        "draft must be cleared after Enter submit"
    );
}

/// `KeyboardProcessor` returns `None` when no node has focus (FocusOwner::None).
/// This ensures no spurious dispatches leak before the user clicks the composer.
#[test]
fn keyboard_processor_no_dispatch_without_focus() {
    use tze_hud_input::{FocusOwner, KeyboardModifiers, KeyboardProcessor, RawKeyDownEvent};
    use tze_hud_scene::MonoUs;

    let kb = KeyboardProcessor::new();
    let focus = FocusOwner::None;
    let ns_fn = |_: tze_hud_scene::SceneId| -> Option<String> { Some("agent".to_string()) };

    let raw = RawKeyDownEvent {
        key_code: "KeyA".to_string(),
        key: "a".to_string(),
        modifiers: KeyboardModifiers::NONE,
        repeat: false,
        timestamp_mono_us: MonoUs(0),
    };
    let dispatch = kb.process_key_down(&raw, &focus, ns_fn);
    assert!(
        dispatch.is_none(),
        "no focus → KeyboardProcessor must not produce a dispatch"
    );
}
