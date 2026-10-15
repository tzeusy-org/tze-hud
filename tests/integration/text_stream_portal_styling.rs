//! Token-resolved portal rendering through the native scene boundary.
use tze_hud_config::{resolve_portal_tokens, tokens::DesignTokenMap};
use tze_hud_projection::{
    ComposerStatus, OutputKind, PortalHub, PortalKey, PortalStatus, PortalView,
    ProjectedPortalPresentation, ProjectionLifecycleState, Publish, TranscriptUnit,
    render::{DEFAULT_COMPACT_SIZE, DEFAULT_PORTAL_SIZE, PortalRender, Size, render},
    tokens::{PortalVisualTokens, portal_visual_tokens_from_part_tokens},
};
use tze_hud_scene::{NodeData, Rgba, TextMarkdownNode};

fn build_expanded_state() -> PortalView {
    let mut hub = PortalHub::default();
    let key = PortalKey::new("styling-agent", "test");
    hub.publish(
        &key,
        Publish {
            display_name: Some("Session".into()),
            text: Some("assistant output".into()),
            status: Some(PortalStatus::Active),
            ..Default::default()
        },
        21,
    )
    .unwrap();
    let portal = hub.get(&key).unwrap();
    PortalView {
        portal_id: key.id.clone(),
        presentation: ProjectedPortalPresentation::Expanded,
        connection_degraded: false,
        has_ever_connected: true,
        interaction_enabled: true,
        composer: ComposerStatus::Ready,
        display_name: Some(portal.display_name.clone()),
        lifecycle_state: Some(ProjectionLifecycleState::Active),
        status_text: Some("styling test active".into()),
        visible_transcript: portal
            .visible_transcript(hub.limits().visible_bytes)
            .map(|u| TranscriptUnit {
                sequence: u.seq,
                output_text: u.text.clone(),
                output_kind: OutputKind::Assistant,
                expects_reply: u.expects_reply,
                appended_at_wall_us: u.at_us,
            })
            .collect(),
        input_history: vec![],
        unread_output_count: Some(portal.unread),
        visible_unread_output_count: Some(portal.unread),
        pending_input_count: Some(0),
        latest_viewer_delivery_state: None,
        last_input_feedback: None,
    }
}
fn text_nodes(rendered: &PortalRender) -> Vec<&TextMarkdownNode> {
    std::iter::once(&rendered.root)
        .chain(&rendered.children)
        .filter_map(|n| match &n.data {
            NodeData::TextMarkdown(t) => Some(t),
            _ => None,
        })
        .collect()
}
fn transcript(rendered: &PortalRender) -> &TextMarkdownNode {
    text_nodes(rendered)
        .into_iter()
        .find(|t| t.content.contains("assistant output"))
        .unwrap()
}
fn tokens(overrides: &[(&str, &str)]) -> PortalVisualTokens {
    let map: DesignTokenMap = overrides
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    portal_visual_tokens_from_part_tokens(&resolve_portal_tokens(&map))
}

#[test]
fn portal_node_uses_injected_visual_tokens() {
    let tokens = tokens(&[
        ("portal.transcript.text_color", "#FF0000"),
        ("portal.transcript.background", "#0000FF"),
        ("portal.transcript.font_size", "23"),
        ("portal.collapsed_card.text_color", "#00FF00"),
        ("portal.collapsed_card.background", "#FF00FF"),
        ("portal.collapsed_card.font_size", "21"),
        ("portal.composer.text_color", "#00FFFF"),
        ("portal.composer.background", "#FFFF00"),
        ("portal.composer.font_size", "19"),
    ]);
    let mut view = build_expanded_state();
    let expanded = render(&view, &tokens, DEFAULT_PORTAL_SIZE, 100);
    let body = transcript(&expanded);
    assert_eq!(body.color, Rgba::new(1.0, 0.0, 0.0, 1.0));
    assert_eq!(body.font_size_px, 23.0);
    // Pane backgrounds are separate native nodes; text is transparent above them.
    assert_eq!(body.background, Some(Rgba::TRANSPARENT));
    let solids: Vec<_> = expanded
        .children
        .iter()
        .filter_map(|n| match &n.data {
            NodeData::SolidColor(s) => Some(s.color),
            _ => None,
        })
        .collect();
    assert!(solids.contains(&tokens.transcript_background));
    assert!(solids.contains(&tokens.composer_background));
    let input = text_nodes(&expanded)
        .into_iter()
        .find(|t| t.content.contains("composer: ready"))
        .unwrap();
    assert_eq!(input.color, tokens.composer_text_color);
    assert_eq!(input.font_size_px, 19.0);
    assert!(expanded.composer.as_ref().unwrap().accepts_pointer);
    view.presentation = ProjectedPortalPresentation::Collapsed;
    let compact = render(&view, &tokens, DEFAULT_COMPACT_SIZE, 100);
    let text = transcript(&compact);
    assert_eq!(text.color, Rgba::new(0.0, 1.0, 0.0, 1.0));
    assert_eq!(text.background, Some(tokens.collapsed_background));
    assert_eq!(text.font_size_px, 21.0);
}

#[test]
fn profile_swap_reskins_renderer_without_logic_change() {
    let view = build_expanded_state();
    let baseline = PortalVisualTokens::default();
    let changed = tokens(&[
        ("portal.transcript.text_color", "#123456"),
        ("portal.transcript.font_size", "28"),
        ("portal.composer.font_size", "25"),
    ]);
    let first = render(&view, &baseline, DEFAULT_PORTAL_SIZE, 100);
    let second = render(&view, &changed, DEFAULT_PORTAL_SIZE, 100);
    assert_eq!(transcript(&first).content, transcript(&second).content);
    assert_ne!(transcript(&first).color, transcript(&second).color);
    assert_eq!(transcript(&second).color, changed.transcript_text_color);
    assert_eq!(transcript(&second).font_size_px, 28.0);
    let back = render(&view, &baseline, DEFAULT_PORTAL_SIZE, 100);
    assert_eq!(transcript(&back), transcript(&first));
    let expected =
        portal_visual_tokens_from_part_tokens(&tze_hud_config::PortalPartTokens::default());
    assert_eq!(baseline, expected);
}

#[test]
fn per_turn_timestamps_render_stamp_and_token_sentinel() {
    let mut view = build_expanded_state();
    view.visible_transcript[0].appended_at_wall_us = 86_399_000_000;
    view.visible_transcript.push(TranscriptUnit {
        sequence: 2,
        output_text: "next day".into(),
        output_kind: OutputKind::Assistant,
        expects_reply: false,
        appended_at_wall_us: 86_400_000_000,
    });
    for (granularity, expected) in [
        ("per_turn", "23:59 · assistant output"),
        ("grouped", "23:59 · assistant output"),
        ("off", "assistant output"),
    ] {
        let token = tokens(&[
            ("portal.timestamp.granularity", granularity),
            ("portal.timestamp.color", "#FF00FF"),
        ]);
        let rendered = render(&view, &token, DEFAULT_PORTAL_SIZE, 90_000_000_000);
        let body = transcript(&rendered);
        assert!(body.content.contains(expected));
        if granularity == "off" {
            assert!(!body.content.contains("23:59"));
        } else {
            assert!(body.content.contains("00:00 · next day"));
            assert!(
                body.color_runs.iter().any(|r| r.start_byte == 0
                    && r.end_byte == 0
                    && r.color == token.timestamp_color)
            );
        }
    }
}

#[test]
fn resize_and_interaction_changes_keep_surface_and_nodes_in_sync() {
    let mut view = build_expanded_state();
    let tokens = PortalVisualTokens::default();
    for size in [
        DEFAULT_PORTAL_SIZE,
        Size {
            width: 180.0,
            height: 140.0,
        },
        Size {
            width: 900.0,
            height: 600.0,
        },
    ] {
        let rendered = render(&view, &tokens, size, 100);
        assert_eq!(rendered.root.children.len(), rendered.children.len());
        for (id, node) in rendered.root.children.iter().zip(&rendered.children) {
            assert_eq!(*id, node.id);
        }
        let surface_composer = rendered
            .surface
            .parts
            .iter()
            .find(|p| p.kind == tze_hud_scene::PortalPartKind::Composer)
            .unwrap();
        assert_eq!(surface_composer.bounds, rendered.composer.unwrap().bounds);
        let frame = rendered
            .surface
            .parts
            .iter()
            .find(|p| p.kind == tze_hud_scene::PortalPartKind::Frame)
            .unwrap();
        assert_eq!(
            (frame.bounds.width, frame.bounds.height),
            (size.width, size.height)
        );
    }
    view.interaction_enabled = false;
    let rendered = render(&view, &tokens, DEFAULT_PORTAL_SIZE, 100);
    assert!(rendered.composer.is_none());
    assert!(
        !rendered
            .surface
            .parts
            .iter()
            .any(|p| p.kind == tze_hud_scene::PortalPartKind::Composer)
    );
    assert!(
        text_nodes(&rendered)
            .iter()
            .any(|t| t.content.contains("composer: unavailable"))
    );
}
