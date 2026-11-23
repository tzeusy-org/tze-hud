use super::*;
use tze_hud_scene::TextFit;
use tze_hud_scene::types::{
    FontFamily, NotificationAction, NotificationPayload, ZoneContent, ZoneInteractionKind,
    ZoneRegistry,
};

#[tokio::test]
async fn overflowing_notification_action_label_ellipsizes_without_moving_later_actions() {
    const SIZE: u32 = 640;
    const ZONE: &str = "notification-area";
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(SIZE, SIZE).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let size = SIZE as f32;
    let mut scene = SceneGraph::new(size, size);
    let mut zone = ZoneRegistry::with_defaults().zones.remove(ZONE).unwrap();
    zone.rendering_policy.font_family = Some(FontFamily::SystemMonospace);
    scene.register_zone(zone);

    let publish = |scene: &mut SceneGraph, first: &str| {
        let actions = [first, "Wait", "Skip", "Stop"]
            .into_iter()
            .map(|label| NotificationAction {
                label: label.into(),
                callback_id: Some(label.into()),
                ..Default::default()
            })
            .collect();
        scene
            .publish_to_zone(
                ZONE,
                ZoneContent::Notification(NotificationPayload {
                    text: "Deploy?".into(),
                    actions,
                    ..Default::default()
                }),
                "test",
                Some("deployment".into()),
                None,
                None,
            )
            .unwrap();
    };
    let action_rects = |scene: &SceneGraph| {
        let mut rects: Vec<_> = scene
            .overlay
            .zone_hit_regions
            .iter()
            .filter_map(|region| match region.kind {
                ZoneInteractionKind::Action { index } => Some((index, region.bounds)),
                _ => None,
            })
            .collect();
        rects.sort_by_key(|(index, _)| *index);
        rects
    };

    let long = "W".repeat(160);
    publish(&mut scene, &long);
    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let overflowed = surface.read_pixels(&compositor.device);
    let original_rects = action_rects(&scene);
    assert_eq!(original_rects.len(), 4);

    let item = compositor
        .collect_text_items(&scene, size, size)
        .into_iter()
        .find(|item| item.text.as_ref() == long)
        .expect("the frame collects the full label for shaping");
    let fonts = compositor
        .text_rasterizer
        .as_mut()
        .unwrap()
        .font_system_mut();
    // Derive a literal reference with a single-line width measurement. It
    // bypasses truncation when painted, so clipping off the marker fails.
    let explicit = (1..long.len())
        .rev()
        .find_map(|count| {
            let candidate = format!("{}…", &long[..count]);
            (crate::text::single_line_width(
                fonts,
                &candidate,
                item.font_size_px,
                item.font_family,
                item.font_weight,
            ) <= item.bounds_width)
                .then_some(candidate)
        })
        .expect("the label box fits a prefix and an ellipsis");

    let fit = super::super::text_fit::TextFitMeasurer::new();
    fit.set_tokens(compositor.token_map.clone());
    let ZoneContent::Notification(payload) = &scene.zone_registry.active_publishes[ZONE][0].content
    else {
        panic!("expected the original notification");
    };
    assert_eq!(
        fit.notification(&scene, ZONE, payload),
        [("actions", explicit.chars().count() - 1)],
        "admission counts the painted original characters"
    );

    publish(&mut scene, &explicit);
    compositor.prime_markdown_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let reference = surface.read_pixels(&compositor.device);
    assert_eq!(action_rects(&scene), original_rects);
    assert!(
        overflowed == reference,
        "the long label must paint its ellipsis"
    );
}
