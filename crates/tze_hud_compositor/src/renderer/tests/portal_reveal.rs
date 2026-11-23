use super::*;
use tze_hud_scene::types::{PortalPart, PortalPartKind, PortalSurface, TextColorRun};

fn markdown(content: &str, color_runs: Box<[TextColorRun]>, top: f32) -> NodeData {
    NodeData::TextMarkdown(TextMarkdownNode {
        font_family: FontFamily::SystemMonospace,
        color_runs,
        ..markdown_text(content, Rect::new(0.0, top, 256.0, 200.0), 14.0)
    })
}

fn container(bounds: Rect) -> Node {
    solid_node(bounds, Rgba::new(0.0, 0.0, 0.0, 1.0), None)
}

fn scrollable_portal(scene: &mut SceneGraph) -> (SceneId, SceneId) {
    let bounds = Rect::new(0.0, 0.0, 256.0, 256.0);
    let tile_id = scrollable_tile(scene, "portal", bounds, None);
    let root = container(bounds);
    let root_id = root.id;
    scene.set_tile_root(tile_id, root).unwrap();
    (tile_id, root_id)
}

fn add_markdown(
    scene: &mut SceneGraph,
    tile_id: SceneId,
    parent_id: SceneId,
    content: &str,
    top: f32,
) -> SceneId {
    let child = leaf(markdown(content, Box::default(), top));
    let id = child.id;
    scene
        .add_node_to_tile(tile_id, Some(parent_id), child)
        .unwrap();
    id
}

fn part(kind: PortalPartKind, bounds: Rect, node: Option<SceneId>) -> PortalPart {
    PortalPart { kind, bounds, node }
}

fn surface<const N: usize>(parts: [PortalPart; N]) -> PortalSurface {
    PortalSurface {
        parts: Vec::from(parts),
        ..Default::default()
    }
}

fn update_reveals(compositor: &mut Compositor, scene: &SceneGraph) {
    compositor.prime_markdown_cache(scene);
    compositor.update_portal_tile_reveals(scene);
}

#[tokio::test]
async fn portal_append_and_color_runs_do_not_fade_sibling_nodes() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let mut scene = SceneGraph::new(256.0, 256.0);
    let (tile, root) = scrollable_portal(&mut scene);
    let settled = add_markdown(&mut scene, tile, root, "ok", 0.0);
    let growing = add_markdown(&mut scene, tile, root, "o", 24.0);
    update_reveals(&mut compositor, &scene);
    assert_eq!(compositor.portal_tile_reveal_states.len(), 2);
    assert!(
        compositor
            .portal_tile_reveal_states
            .values()
            .all(|state| !state.is_revealing())
    );
    scene
        .update_node_content(tile, growing, markdown("ok", Box::default(), 24.0))
        .unwrap();
    update_reveals(&mut compositor, &scene);
    assert!(!compositor.portal_tile_reveal_states[&(tile, settled)].is_revealing());
    let state = &compositor.portal_tile_reveal_states[&(tile, growing)];
    assert!(state.is_revealing());
    assert_eq!(state.reveal_start, "o".len());

    // Both nodes now paint the same text. Only the later node's suffix fades.
    let mut items: Vec<_> = compositor
        .collect_text_items(&scene, 256.0, 256.0)
        .into_iter()
        .filter(|item| item.text.as_ref() == "ok")
        .collect();
    items.sort_by(|a, b| a.pixel_y.total_cmp(&b.pixel_y));
    assert_eq!(items.len(), 2);
    assert!(items[0].pixel_y < items[1].pixel_y);
    let alpha = |item: &crate::text::TextItem| {
        item.styled_runs
            .iter()
            .filter_map(|run| run.color.map(|color| color[3]))
            .min()
            .unwrap_or(255)
    };
    assert_eq!(alpha(&items[0]), 255);
    assert!(alpha(&items[1]) < 255);

    // The old single-node append path must still enter reveal.
    let mut scene = SceneGraph::new(256.0, 256.0);
    let (tile, root) = scrollable_portal(&mut scene);
    let draft = add_markdown(&mut scene, tile, root, "hello", 0.0);
    update_reveals(&mut compositor, &scene);
    assert!(!compositor.portal_tile_reveal_states[&(tile, draft)].is_revealing());
    scene
        .update_node_content(tile, draft, markdown("hello world", Box::default(), 0.0))
        .unwrap();
    update_reveals(&mut compositor, &scene);
    let appended = &compositor.portal_tile_reveal_states[&(tile, draft)];
    assert!(appended.is_revealing());
    assert_eq!(appended.reveal_start, "hello".len());

    // Removing the first eligible node must not restart the longer history.
    let mut scene = SceneGraph::new(256.0, 256.0);
    let (tile, root) = scrollable_portal(&mut scene);
    let draft = add_markdown(&mut scene, tile, root, "hi", 0.0);
    let longer = "one two three four five six seven";
    let history = add_markdown(&mut scene, tile, root, longer, 24.0);
    update_reveals(&mut compositor, &scene);
    let assert_history_settled = |c: &Compositor| {
        let state = &c.portal_tile_reveal_states[&(tile, history)];
        assert!(!state.is_revealing());
        assert_eq!(state.reveal_start, longer.len());
    };
    assert_history_settled(&compositor);
    assert!(!compositor.portal_tile_reveal_states[&(tile, draft)].is_revealing());
    let run = TextColorRun {
        start_byte: 0,
        end_byte: 1,
        color: Rgba::new(0.9, 0.1, 0.1, 1.0),
    };
    scene
        .update_node_content(tile, draft, markdown("hi", Box::from([run]), 0.0))
        .unwrap();
    update_reveals(&mut compositor, &scene);
    assert_history_settled(&compositor);
    assert!(
        !compositor
            .portal_tile_reveal_states
            .contains_key(&(tile, draft))
    );
}

#[test]
fn portal_part_index_classifies_parts_and_falls_back() {
    use PortalPartKind::{Composer, Divider, Frame, Transcript};
    let mut scene = SceneGraph::new(256.0, 256.0);
    let (tile, _) = scrollable_portal(&mut scene);
    let treats_as_transcript =
        |parts: Option<&[PortalPart]>, node: SceneId, ancestor: Option<&PortalPart>| {
            let effective = text::resolve_effective_part(parts, node, ancestor);
            text::node_gets_transcript_treatment(parts, effective)
        };

    assert!(text::portal_part_index(&scene, tile).is_none());
    assert!(treats_as_transcript(None, SceneId::new(), None));
    let divider = part(Divider, Rect::new(0.0, 148.0, 256.0, 2.0), None);
    scene.overlay.portal_surfaces.insert(
        tile,
        surface([
            part(Frame, Rect::new(0.0, 0.0, 256.0, 200.0), None),
            divider,
        ]),
    );
    assert!(
        text::portal_part_index(&scene, tile).is_none(),
        "geometry-only surface uses the legacy fallback"
    );
    let transcript_node = SceneId::new();
    let composer_node = SceneId::new();
    let descendant = SceneId::new();
    let transcript = part(
        Transcript,
        Rect::new(0.0, 0.0, 256.0, 150.0),
        Some(transcript_node),
    );
    scene.overlay.portal_surfaces.insert(
        tile,
        surface([
            transcript,
            part(
                Composer,
                Rect::new(0.0, 150.0, 256.0, 50.0),
                Some(composer_node),
            ),
            divider,
        ]),
    );
    let parts = text::portal_part_index(&scene, tile).unwrap();
    assert_eq!(parts.len(), 3, "geometry-only parts stay in the index");
    for (case, node, ancestor, expected) in [
        ("declared transcript", transcript_node, None, true),
        ("declared composer", composer_node, None, false),
        ("undeclared node", descendant, None, false),
        (
            "descendant of transcript",
            descendant,
            Some(&transcript),
            true,
        ),
        (
            "declared composer overrides inherited transcript",
            composer_node,
            Some(&transcript),
            false,
        ),
    ] {
        assert_eq!(
            treats_as_transcript(Some(parts), node, ancestor),
            expected,
            "{case}"
        );
    }
}

/// The production text collector clips each portal part to its band and
/// applies tail anchoring only to transcript text.
#[tokio::test]
async fn portal_surface_renders_parts_with_per_part_scope_and_clip() {
    use crate::overflow::TruncationViewport::{HeadAnchored, TailAnchored};

    let (mut compositor, _) = require_gpu!(make_text_compositor(256, 256).await);
    let mut scene = SceneGraph::new(256.0, 256.0);
    let (tile, root) = scrollable_portal(&mut scene);
    let transcript_band = Rect::new(0.0, 0.0, 256.0, 150.0);
    let composer_band = Rect::new(0.0, 150.0, 256.0, 50.0);
    let transcript_container = container(transcript_band);
    let container = transcript_container.id;
    scene
        .add_node_to_tile(tile, Some(root), transcript_container)
        .unwrap();
    let transcript = add_markdown(&mut scene, tile, container, &"Transcript\n".repeat(20), 0.0);
    let composer = add_markdown(&mut scene, tile, root, "Draft reply", composer_band.y);
    for id in [transcript, composer] {
        let NodeData::TextMarkdown(text) = &mut scene.nodes.get_mut(&id).unwrap().data else {
            unreachable!()
        };
        text.overflow = TextOverflow::Ellipsis;
    }
    scene.set_tile_follow_tail_at_tail(tile, true);
    for transcript_root in [transcript, container] {
        scene
            .set_portal_surface(
                tile,
                surface([
                    part(
                        PortalPartKind::Transcript,
                        transcript_band,
                        Some(transcript_root),
                    ),
                    part(PortalPartKind::Composer, composer_band, Some(composer)),
                ]),
                "portal",
            )
            .unwrap();
        compositor.prime_markdown_cache(&scene);
        let items = compositor.collect_text_items(&scene, 256.0, 256.0);
        let rendered = |start| {
            items
                .iter()
                .find(|item| item.text.starts_with(start))
                .unwrap()
        };
        let transcript = rendered("Transcript");
        assert_eq!(transcript.viewport, TailAnchored);
        assert!(
            (transcript.clip_pixel_y + transcript.clip_bounds_height - composer_band.y).abs() < 0.5
        );
        let composer = rendered("Draft reply");
        assert_eq!(composer.viewport, HeadAnchored);
        assert!(composer.clip_pixel_y >= composer_band.y - 0.5);
        assert!(
            composer.clip_pixel_y + composer.clip_bounds_height
                <= composer_band.y + composer_band.height + 0.5
        );
    }
}
