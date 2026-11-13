use super::*;

/// hud-ljhku.13 R13.1 S3/S4 through the real render path: text whose glyphs
/// no loaded face covers draws as tofu and starts one background load (a
/// render frame never loads a face itself); once
/// `Compositor::poll_font_fallback` installs the faces, the text re-shapes
/// exactly once with real glyphs, with no republish (scene version
/// unchanged), and later frames reuse that shape.
#[tokio::test]
async fn fallback_faces_land_and_text_relays_out_once() {
    use tze_hud_scene::types::{
        FontFamily, NodeData, Rect, TextAlign, TextMarkdownNode, TextOverflow,
    };

    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let (source, log) =
        crate::fonts::test_standins::standin_source(&crate::fonts::test_standins::all_standins());
    let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&wakes);
    compositor.set_font_fallback_wake(tze_hud_scene::render_wake::RenderWakeNotifier::new(
        move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        },
    ));
    {
        let rasterizer = compositor.text_rasterizer.as_mut().unwrap();
        let mut fallback = crate::fonts::SystemFallback::new(source);
        fallback.set_wake(compositor.font_fallback_wake.clone());
        *rasterizer.system_fallback_mut() = fallback;
    }

    let node = |text: &str| Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: text.to_owned(),
            bounds: Rect::new(0.0, 0.0, 240.0, 60.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: tze_hud_scene::types::Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };

    // S3: Latin text never asks for a fallback face.
    let mut latin = scene_with_node(node("Plain Latin text"));
    compositor.prime_markdown_cache(&latin);
    compositor.prime_truncation_cache(&latin);
    let _ = compositor.render_frame_headless(&mut latin, &surface);
    assert!(
        log.lock().unwrap().is_empty(),
        "no fallback face for covered text"
    );
    assert!(!compositor.poll_font_fallback());

    let mut scene = scene_with_node(node("\u{2705} \u{5b8c}\u{6210} \u{d14c}\u{c2a4}\u{d2b8}"));
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    let shapes = compositor.text_shape_call_count();
    let _ = compositor.render_frame_headless(&mut scene, &surface);
    assert_eq!(
        compositor.text_shape_call_count() - shapes,
        1,
        "first shape"
    );
    let faces_before = compositor
        .text_rasterizer
        .as_mut()
        .unwrap()
        .font_face_count();

    // The load runs off the frame thread; wait for it, then install.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while wakes.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "load never landed");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .all(|(_, thread)| thread.as_deref() == Some("tze-font-fallback"))
    );
    let version = scene.version;
    assert!(compositor.poll_font_fallback(), "faces installed");
    assert!(
        compositor
            .text_rasterizer
            .as_mut()
            .unwrap()
            .font_face_count()
            > faces_before
    );
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    // S4: exactly one re-layout, no republish.
    let shapes = compositor.text_shape_call_count();
    let _ = compositor.render_frame_headless(&mut scene, &surface);
    assert_eq!(
        compositor.text_shape_call_count() - shapes,
        1,
        "one re-layout"
    );
    assert_eq!(scene.version, version, "no republish");
    let shapes = compositor.text_shape_call_count();
    for _ in 0..3 {
        let _ = compositor.render_frame_headless(&mut scene, &surface);
    }
    assert_eq!(compositor.text_shape_call_count(), shapes, "shape reused");
    assert!(!compositor.poll_font_fallback(), "nothing more to install");

    // The re-shaped text has no .notdef glyph.
    let rasterizer = compositor.text_rasterizer.as_mut().unwrap();
    let mut buf = glyphon::Buffer::new(
        rasterizer.font_system_mut(),
        glyphon::Metrics::new(16.0, 22.0),
    );
    let fs = rasterizer.font_system_mut();
    buf.set_text(
        fs,
        "\u{2705} \u{5b8c}\u{6210} \u{d14c}\u{c2a4}\u{d2b8}",
        glyphon::Attrs::new().family(glyphon::Family::SansSerif),
        glyphon::Shaping::Advanced,
    );
    buf.shape_until_scroll(fs, false);
    assert!(
        buf.layout_runs()
            .flat_map(|r| r.glyphs.iter())
            .all(|g| g.glyph_id != 0)
    );

    // A rebuilt text renderer (theme change) keeps the faces without loading
    // any family again.
    let asked = log.lock().unwrap().len();
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    assert!(
        compositor
            .text_rasterizer
            .as_mut()
            .unwrap()
            .font_face_count()
            > faces_before
    );
    assert_eq!(log.lock().unwrap().len(), asked);
}
