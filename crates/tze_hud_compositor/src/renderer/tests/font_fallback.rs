use super::*;
use crate::fonts::{SystemFallback, test_standins};
use glyphon::{Attrs, Buffer, Family, Metrics, Shaping};
use std::sync::mpsc;
use std::time::Duration;
use tze_hud_scene::render_wake::RenderWakeNotifier;

/// R13.1 S3/S4 through the render path: fallback loads off the frame thread.
/// Installing faces re-shapes the scene once without a republish, and
/// rebuilding the renderer keeps those faces.
#[tokio::test]
async fn fallback_faces_land_and_text_relays_out_once() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (source, log) = test_standins::standin_source(&test_standins::all_standins());
    *compositor
        .text_rasterizer
        .as_mut()
        .unwrap()
        .system_fallback_mut() = SystemFallback::new(source);
    let (wake, wakes) = mpsc::channel();
    compositor.set_font_fallback_wake(RenderWakeNotifier::new(move || {
        let _ = wake.send(());
    }));

    let text_scene = |text: &str| {
        scene_with_node(Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: Vec::new(),
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: text.to_owned(),
                bounds: Rect::new(0.0, 0.0, 240.0, 60.0),
                font_size_px: 16.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba::WHITE,
                background: None,
                alignment: TextAlign::Start,
                overflow: TextOverflow::Clip,
                color_runs: Box::default(),
            }),
        })
    };
    let prime = |compositor: &mut Compositor, scene: &SceneGraph| {
        compositor.prime_markdown_cache(scene);
        compositor.prime_truncation_cache(scene);
    };
    let render = |compositor: &mut Compositor, scene: &mut SceneGraph| {
        let before = compositor.text_shape_call_count();
        let _ = compositor.render_frame_headless(scene, &surface);
        compositor.text_shape_call_count() - before
    };
    let faces = |compositor: &Compositor| {
        compositor
            .text_rasterizer
            .as_ref()
            .unwrap()
            .font_face_count()
    };

    let mut latin = text_scene("Plain Latin text");
    prime(&mut compositor, &latin);
    render(&mut compositor, &mut latin);
    assert!(
        log.lock().unwrap().is_empty(),
        "covered text needs no fallback"
    );
    assert!(!compositor.poll_font_fallback());

    let text = "✅ 完成 테스트";
    let mut scene = text_scene(text);
    prime(&mut compositor, &scene);
    assert_eq!(render(&mut compositor, &mut scene), 1, "first shape");
    let faces_before = faces(&compositor);
    wakes
        .recv_timeout(Duration::from_secs(10))
        .expect("loaded fallback faces must wake the compositor");
    {
        let calls = log.lock().unwrap();
        assert!(!calls.is_empty(), "the missing glyphs must request faces");
        assert!(
            calls
                .iter()
                .all(|(_, thread)| thread.as_deref() == Some("tze-font-fallback")),
            "fallback providers run off the frame thread"
        );
    }

    let version = scene.version;
    assert!(compositor.poll_font_fallback(), "faces installed");
    let installed_faces = faces(&compositor);
    assert!(installed_faces > faces_before);
    prime(&mut compositor, &scene);
    assert_eq!(render(&mut compositor, &mut scene), 1, "one re-layout");
    for _ in 0..3 {
        assert_eq!(render(&mut compositor, &mut scene), 0, "shape reused");
    }
    assert_eq!(scene.version, version, "no republish");
    assert!(!compositor.poll_font_fallback(), "nothing more to install");

    let fs = compositor
        .text_rasterizer
        .as_mut()
        .unwrap()
        .font_system_mut();
    let mut shaped = Buffer::new(fs, Metrics::new(16.0, 22.0));
    shaped.set_text(
        fs,
        text,
        Attrs::new().family(Family::SansSerif),
        Shaping::Advanced,
    );
    shaped.shape_until_scroll(fs, false);
    let mut glyphs = shaped.layout_runs().flat_map(|run| run.glyphs).peekable();
    assert!(glyphs.peek().is_some(), "the compositor must shape glyphs");
    assert!(glyphs.all(|glyph| glyph.glyph_id != 0), "no missing glyphs");

    let loads = log.lock().unwrap().len();
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    assert_eq!(faces(&compositor), installed_faces);
    assert_eq!(log.lock().unwrap().len(), loads);
}
