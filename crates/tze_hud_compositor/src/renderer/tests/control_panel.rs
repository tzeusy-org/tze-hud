use super::*;
use crate::{
    ControlPanelModel, PanelBody, PanelChip, PanelControl, PanelFocus, PanelItem, PanelSection,
    PanelTone,
};

fn item(id: &str, label: &str, control: PanelControl) -> PanelItem {
    PanelItem {
        id: id.into(),
        label: label.into(),
        detail: Some("Applies now".into()),
        detail_tone: PanelTone::Normal,
        context: None,
        control,
        badge: None,
        shortcut: None,
        emphasis: PanelTone::Normal,
    }
}

fn panel() -> ControlPanelModel {
    ControlPanelModel {
        query: String::new(),
        placeholder: "Search every HUD setting".into(),
        notice: None,
        tiles: vec![item(
            "agents.pause",
            "Pause agents",
            PanelControl::Toggle(false),
        )],
        chips: vec![PanelChip {
            title: "Agents".into(),
            key: "1".into(),
        }],
        state_line: "Agents running".into(),
        body: PanelBody::Sections(vec![PanelSection {
            id: "agents".into(),
            title: "Agents".into(),
            key: Some("Alt+1".into()),
            items: vec![item(
                "agents.pause",
                "Pause agents",
                PanelControl::Toggle(false),
            )],
        }]),
        hints: vec!["↑↓ move".into()],
        hint_end: "Esc close".into(),
        focus: PanelFocus::Search,
        hover: None,
        scroll_px: 0.0,
        scale: 1.0,
    }
}

/// A full-screen opaque red tile, so any panel pixel shows as non-red.
fn red_scene(w: f32, h: f32) -> SceneGraph {
    let mut scene = SceneGraph::new(w, h);
    let tab = scene.create_tab("panel", 0).unwrap();
    let lease = scene.grant_lease("agent", 60_000);
    let tile = scene
        .create_tile(tab, "agent", lease, Rect::new(0.0, 0.0, w, h), 1)
        .unwrap();
    scene
        .set_tile_root(
            tile,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba::new(1.0, 0.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 0.0, w, h),
                    radius: None,
                }),
            },
        )
        .unwrap();
    scene
}

fn is_red(p: [u8; 4]) -> bool {
    p[0] > 200 && p[1] < 40 && p[2] < 40
}

/// The open panel draws over the frame (a scrim over the whole surface and
/// an opaque surface inside the inset), closing it restores the frame, and
/// the admin capture never contains it (CP-2).
#[tokio::test]
async fn panel_draws_over_the_frame_and_capture_leaves_it_out() {
    let (w, h) = (640_u32, 400_u32);
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(w, h).await);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    compositor.init_text_renderer(format);
    let mut scene = red_scene(w as f32, h as f32);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    // Settle the tile fade-in so the red fill is opaque.
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.portal_tile_anim_states.clear();

    let center = |px: &[u8]| crate::test_pixels::pixel_at(px, w, w / 2, h / 2);
    let corner = |px: &[u8]| crate::test_pixels::pixel_at(px, w, 4, 4);

    compositor.render_frame_headless(&mut scene, &surface);
    let before = surface.read_pixels(&compositor.device);
    assert!(is_red(center(&before)) && is_red(corner(&before)));

    assert!(compositor.set_control_panel(Some(panel())));
    assert!(
        !compositor.set_control_panel(Some(panel())),
        "same model: no repaint"
    );
    compositor.render_frame_headless(&mut scene, &surface);
    let open = surface.read_pixels(&compositor.device);
    assert!(
        !is_red(center(&open)),
        "panel surface covers the center: {:?}",
        center(&open)
    );
    let dim = corner(&open);
    assert!(
        !is_red(dim) && dim[0] > dim[1],
        "the scrim dims, not hides, the frame outside the panel: {dim:?}"
    );

    let build = compositor.build_windowed_frame(&mut scene, w, h);
    let target = crate::FrameTarget::primary(w, h);
    let shot = compositor
        .capture_windowed_frame(&build, &target, format)
        .expect("capture");
    let at = |x: u32, y: u32| {
        let i = ((y * shot.width + x) * 4) as usize;
        [
            shot.rgba[i],
            shot.rgba[i + 1],
            shot.rgba[i + 2],
            shot.rgba[i + 3],
        ]
    };
    assert!(
        is_red(at(w / 2, h / 2)) && is_red(at(4, 4)),
        "the admin capture leaves the panel and its scrim out"
    );

    assert!(compositor.set_control_panel(None));
    assert!(
        compositor
            .control_panel_geometry(w as f32, h as f32)
            .is_none()
    );
    compositor.render_frame_headless(&mut scene, &surface);
    let closed = surface.read_pixels(&compositor.device);
    assert_eq!(
        center(&closed),
        center(&before),
        "closing restores the frame"
    );
}

/// The panel's layout (and so the geometry the runtime hit-tests against) is
/// cached: an unchanged model and size reuse it, and only a model change or
/// a resize recomputes it (CP-11: idle costs nothing).
#[tokio::test]
async fn panel_layout_is_reused_until_the_model_or_size_changes() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(640, 400).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.set_control_panel(Some(panel()));
    let first = compositor.control_panel_geometry(640.0, 400.0).unwrap().0;
    assert_eq!(
        compositor.control_panel_geometry(640.0, 400.0).unwrap().0,
        first
    );

    let mut typed = panel();
    typed.query = "pa".into();
    compositor.set_control_panel(Some(typed));
    let second = compositor.control_panel_geometry(640.0, 400.0).unwrap().0;
    assert_ne!(second, first, "model change relays out");
    let (third, geometry) = compositor.control_panel_geometry(800.0, 400.0).unwrap();
    assert_ne!(third, second, "resize relays out");
    assert_eq!(
        geometry.panel.w,
        800.0 - 64.0,
        "the panel spans the new width"
    );
}

/// Owner hiding (CP-6): a hidden tile and a hidden zone draw nothing, showing
/// them again draws the current content, and while anything is hidden the
/// closed panel's notice pill sits at the top centre.
#[tokio::test]
async fn hidden_content_is_not_drawn_and_the_closed_notice_is() {
    use tze_hud_scene::graph::SurfaceClasses;

    let (w, h) = (640_u32, 400_u32);
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(w, h).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = red_scene(w as f32, h as f32);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "pip".to_owned(),
        description: "zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.25,
        },
        accepted_media_types: vec![ZoneMediaType::StaticImage],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Replace,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    let image = ResourceId::of(b"image");
    compositor.register_image_bytes(image, std::sync::Arc::from(vec![255u8; 16]), 2, 2);
    scene
        .publish_to_zone(
            "pip",
            ZoneContent::StaticImage(image),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.portal_tile_anim_states.clear();

    let zone_vertices = |compositor: &Compositor, scene: &SceneGraph| {
        let (mut vertices, mut textured) = (Vec::new(), Vec::new());
        compositor.render_zone_content(
            scene,
            &mut vertices,
            &mut textured,
            w as f32,
            h as f32,
            None,
        );
        vertices.len() + textured.len()
    };
    let center = |px: &[u8]| crate::test_pixels::pixel_at(px, w, w / 2, h / 2);
    let notice_at = |px: &[u8]| crate::test_pixels::pixel_at(px, w, w / 2, 34);
    assert!(zone_vertices(&compositor, &scene) > 0);

    scene.set_owner_hidden(SurfaceClasses::ALL);
    // A visibility change bumps the scene version; production primes at
    // commit each frame, so the test does too.
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    assert_eq!(
        zone_vertices(&compositor, &scene),
        0,
        "a hidden zone emits nothing"
    );
    compositor.render_frame_headless(&mut scene, &surface);
    let hidden = surface.read_pixels(&compositor.device);
    assert!(
        !is_red(center(&hidden)),
        "a hidden tile is not drawn: {:?}",
        center(&hidden)
    );
    let bare = notice_at(&hidden);
    assert!(
        compositor.image_bytes.contains_key(&image),
        "a hidden zone's image survives eviction, to draw when shown"
    );

    assert!(
        compositor.set_closed_notice(Some("Hidden: everything · open with Ctrl+Shift+F11".into()))
    );
    assert!(
        !compositor.set_closed_notice(Some("Hidden: everything · open with Ctrl+Shift+F11".into())),
        "same notice: no repaint"
    );
    compositor.render_frame_headless(&mut scene, &surface);
    let noticed = surface.read_pixels(&compositor.device);
    assert_ne!(notice_at(&noticed), bare, "the notice pill is drawn");
    assert_eq!(
        crate::test_pixels::pixel_at(&noticed, w, w / 2, h - 4),
        crate::test_pixels::pixel_at(&hidden, w, w / 2, h - 4),
        "the notice is a pill, not a full-screen layer"
    );

    // Showing again draws the content as it is now.
    scene.set_owner_hidden(SurfaceClasses::NONE);
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    assert!(compositor.set_closed_notice(None));
    compositor.render_frame_headless(&mut scene, &surface);
    compositor.portal_tile_anim_states.clear();
    compositor.render_frame_headless(&mut scene, &surface);
    let shown = surface.read_pixels(&compositor.device);
    assert!(is_red(center(&shown)), "{:?}", center(&shown));
    assert!(
        compositor.image_texture_cache.contains_key(&image),
        "uploaded again"
    );
    assert!(zone_vertices(&compositor, &scene) > 0);
}
