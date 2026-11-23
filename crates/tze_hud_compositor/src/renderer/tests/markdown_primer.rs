use super::markdown_cache::{make_text_compositor, markdown_node, prime_text};
use super::*;

const LINE: &str = "The quick brown fox jumps over the lazy dog.\n";
const TRANSCRIPT_SIZES: [usize; 2] = [12, 1440];

fn transcript(content: impl Into<String>, lines: usize, y: f32) -> Node {
    markdown_node(
        content,
        Rect::new(0.0, y, 240.0, lines as f32 * 20.0 + 200.0),
        14.0,
        FontFamily::SystemMonospace,
        TextOverflow::Clip,
    )
}

fn transcript_scene(root: Node) -> (SceneGraph, SceneId) {
    let mut scene = scene_with_node(root);
    let tile = *scene.tiles.keys().next().unwrap();
    scene
        .register_tile_scroll_config(tile, TileScrollConfig::vertical())
        .unwrap();
    (scene, tile)
}

fn append_transcript(scene: &mut SceneGraph, tile: SceneId, content: &str, lines: usize) {
    let appended = transcript(format!("{content}Appended tail line.\n"), lines, 0.0);
    scene.set_tile_root(tile, appended).unwrap();
}

fn assert_primer_settled(
    compositor: &mut Compositor,
    scene: &mut SceneGraph,
    surface: &HeadlessSurface,
    root: SceneId,
    attempts: usize,
) {
    // The real frame path consumes the worker snapshot. A first hit-only
    // frame must be followed by five more with the same miss count.
    for _ in 0..attempts {
        let misses = compositor.markdown_cache_miss_count();
        let settled = (0..6).all(|_| {
            compositor.render_frame_headless(scene, surface);
            compositor.markdown_cache_miss_count() == misses
        });
        if settled {
            let key = &compositor.node_key_cache[&root];
            assert!(compositor.markdown_cache().get_by_key(key).is_some());
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!(
        "Markdown primer never reached six hit-only frames after {attempts} attempts ({} total misses)",
        compositor.markdown_cache_miss_count()
    );
}

#[tokio::test]
async fn markdown_primer_landing_converges_on_render_path() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(256, 256).await);
    let lines = 1440;
    let root = transcript(LINE.repeat(lines), lines, 0.0);
    let root_id = root.id;
    let (mut scene, _) = transcript_scene(root);
    prime_text(&mut compositor, &scene);
    assert_primer_settled(&mut compositor, &mut scene, &surface, root_id, 200);
}

#[tokio::test]
async fn reused_compositor_across_scenes_lands_markdown_primer() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(256, 256).await);
    let mut previous_version = None;
    for lines in TRANSCRIPT_SIZES {
        let content = LINE.repeat(lines);
        let (mut scene, tile) = transcript_scene(transcript(&content, lines, 0.0));
        if let Some(version) = previous_version {
            // A fresh scene's lower version must not suppress its worker snapshot.
            assert!(scene.version < version);
        }
        prime_text(&mut compositor, &scene);
        append_transcript(&mut scene, tile, &content, lines);
        prime_text(&mut compositor, &scene);
        let root = scene.tiles[&tile].root_node.unwrap();
        assert_primer_settled(&mut compositor, &mut scene, &surface, root, 60);
        previous_version = Some(scene.version);
    }
}

#[tokio::test]
async fn scroll_reshape_reuse() {
    let (mut compositor, surface) = require_gpu!(make_text_compositor(256, 256).await);
    for lines in TRANSCRIPT_SIZES {
        let content = LINE.repeat(lines);
        let root = transcript(&content, lines, 0.0);
        let root_id = root.id;
        let (mut scene, tile) = transcript_scene(root);
        if lines == 12 {
            scene
                .add_node_to_tile(tile, Some(root_id), transcript(&content, lines, 16.0))
                .unwrap();
        }

        prime_text(&mut compositor, &scene);
        compositor.render_frame_headless(&mut scene, &surface);
        let cold = compositor
            .take_work_counts()
            .expect("first transcript frame");
        assert!(cold.layout > 0);
        assert_eq!((cold.raster, cold.upload), (0, 0));
        assert!(compositor.take_work_counts().is_none());

        let before_scroll = compositor.text_shape_call_count();
        for _ in 0..3 {
            compositor.render_frame_headless(&mut scene, &surface);
        }
        assert_eq!(compositor.text_shape_call_count(), before_scroll);
        for step in 1..=10 {
            let offset = step as f32 * 120.0;
            scene
                .set_tile_scroll_offset_local(tile, 0.0, offset)
                .unwrap();
            compositor.render_frame_headless(&mut scene, &surface);
            assert_eq!(
                compositor.text_shape_call_count(),
                before_scroll,
                "{lines} lines, {offset}px"
            );
        }

        append_transcript(&mut scene, tile, &content, lines);
        prime_text(&mut compositor, &scene);
        compositor.render_frame_headless(&mut scene, &surface);
        assert_eq!(compositor.text_shape_call_count(), before_scroll + 1);
    }
}
