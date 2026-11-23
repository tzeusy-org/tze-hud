use super::*;
use tze_hud_scene::graph::overlay::{NotificationRevealField, NotificationRevealRequest};
use tze_hud_scene::types::{
    GeometryPolicy, NotificationAction, NotificationPayload, ZoneContent, ZoneRegistry,
};

#[tokio::test]
async fn notification_reveal_flips_within_the_display_and_keeps_the_button_clear() {
    const SIZE: u32 = 640;
    const ZONE: &str = "notification-area";
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(SIZE, SIZE).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let label = format!("{} last visible words", "a long choice label ".repeat(70));
    for near_bottom in [false, true] {
        let mut scene = SceneGraph::new(SIZE as f32, SIZE as f32);
        let mut zone = ZoneRegistry::with_defaults().zones.remove(ZONE).unwrap();
        zone.geometry_policy = GeometryPolicy::Relative {
            x_pct: 0.65,
            y_pct: if near_bottom { 0.80 } else { 0.03 },
            width_pct: 0.35,
            height_pct: 0.19,
        };
        scene.register_zone(zone);
        scene
            .publish_to_zone(
                ZONE,
                ZoneContent::Notification(NotificationPayload {
                    text: "Choose one".into(),
                    actions: vec![NotificationAction {
                        label: label.clone(),
                        callback_id: Some("choice".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                "example",
                None,
                None,
                None,
            )
            .unwrap();
        compositor.prime_markdown_cache(&scene);
        compositor.render_frame_headless(&mut scene, &surface);
        let [source] = scene.overlay.notification_reveal_regions.as_slice() else {
            panic!("only the cut label can reveal");
        };
        assert_eq!(source.target.field, NotificationRevealField::Action(0));
        let card = scene.overlay.notification_geometry[0].clone();
        let target = source.target;
        scene.overlay.notification_reveal = Some(NotificationRevealRequest {
            target,
            scroll: 0.0,
        });
        compositor.render_frame_headless(&mut scene, &surface);
        let panel = scene
            .overlay
            .notification_reveal_panel
            .expect("the label reveals");
        let initial = surface.read_pixels(&compositor.device);
        let bounds = panel.bounds;
        assert!(bounds.x >= 0.0 && bounds.y >= 0.0);
        assert!(bounds.x + bounds.width <= SIZE as f32 && bounds.y + bounds.height <= SIZE as f32);
        assert!(!bounds.intersects(&card.actions[0]));
        assert_eq!(bounds.y < card.bounds.y, near_bottom);
        assert_eq!(scene.overlay.notification_geometry[0].actions, card.actions);
        assert!(panel.max_scroll > 0.0);

        scene.overlay.notification_reveal.as_mut().unwrap().scroll = panel.max_scroll;
        compositor.render_frame_headless(&mut scene, &surface);
        let scrolled = surface.read_pixels(&compositor.device);
        assert!(
            initial != scrolled,
            "the viewport draws a different part of the complete label"
        );
        let sources = compositor.collect_encode_sources(&scene, SIZE, SIZE, true);
        assert!(
            sources
                .affordance_items
                .iter()
                .any(|item| item.text.as_ref() == label)
        );
    }
}
