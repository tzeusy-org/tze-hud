use super::*;
use tze_hud_scene::types::{NotificationPayload, Rect, ZoneContent, ZoneRegistry};

#[tokio::test]
async fn keyed_notification_progress_draws_in_place_without_restart() {
    const SIZE: u32 = 640;
    const ZONE: &str = "notification-area";
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(SIZE, SIZE).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.set_token_map(HashMap::from([
        ("color.primary".into(), "#00FF00".into()),
        ("color.outline.variant".into(), "#0000FF".into()),
    ]));
    let mut scene = SceneGraph::new(SIZE as f32, SIZE as f32);
    let mut zone = ZoneRegistry::with_defaults().zones.remove(ZONE).unwrap();
    zone.rendering_policy.transition_in_ms = Some(100);
    scene.register_zone(zone);
    let publish = |scene: &mut SceneGraph, progress| {
        scene
            .publish_to_zone(
                ZONE,
                ZoneContent::Notification(NotificationPayload {
                    text: "Deploying".into(),
                    progress,
                    // Urgency must not change the progress palette.
                    urgency: 3,
                    ..Default::default()
                }),
                "test",
                Some("deployment".into()),
                None,
                None,
            )
            .unwrap();
    };
    let draw = |compositor: &mut Compositor, scene: &mut SceneGraph| {
        compositor.prime_markdown_cache(scene);
        compositor.render_frame_headless(scene, &surface);
        (
            scene.overlay.notification_geometry[0].bounds,
            surface.read_pixels(&compositor.device),
        )
    };
    let start = std::time::Instant::now() - std::time::Duration::from_secs(1);
    publish(&mut scene, None);
    compositor.update_zone_animations_at(&scene, start);
    assert_eq!(
        compositor.zone_animation_states[ZONE].transition_start,
        start
    );
    compositor.update_zone_animations_at(&scene, std::time::Instant::now());
    let (without, absent) = draw(&mut compositor, &mut scene);

    publish(&mut scene, Some(0.4));
    compositor.update_zone_animations_at(&scene, std::time::Instant::now());
    assert!(!compositor.zone_animation_states.contains_key(ZONE));
    let (initial, forty) = draw(&mut compositor, &mut scene);
    let zone = &scene.zone_registry.zones[ZONE];
    let tokens = tze_hud_config::tokens::for_zone(&compositor.token_map, ZONE);
    let style = NotificationStyle::for_zone(ZONE, &tokens, &zone.rendering_policy);
    let ZoneContent::Notification(payload) = &scene.zone_registry.active_publishes[ZONE][0].content
    else {
        panic!("expected a notification");
    };
    let [(track, _), _] = style.progress_bar(initial, payload, None).unwrap();
    assert_eq!((initial.x, initial.y), (without.x, without.y));
    assert!((initial.height - without.height - style.progress_reserve(payload)).abs() < 0.01);

    publish(&mut scene, Some(0.7));
    compositor.update_zone_animations_at(&scene, std::time::Instant::now());
    assert!(!compositor.zone_animation_states.contains_key(ZONE));
    let (updated, seventy) = draw(&mut compositor, &mut scene);
    assert_eq!(updated, initial);

    let sample = |pixels: &[u8], track: Rect, fraction: f32| {
        let x = (track.x + track.width * fraction) as usize;
        let y = (track.y + track.height * 0.5) as usize;
        let offset = (y * SIZE as usize + x) * 4;
        <[u8; 3]>::try_from(&pixels[offset..offset + 3]).unwrap()
    };
    let fill = [0, 255, 0];
    let track_color = [0, 0, 255];
    assert_ne!(sample(&absent, track, 0.2), fill);
    assert_ne!(sample(&absent, track, 0.8), track_color);
    for (pixels, fraction) in [(&forty, 0.4), (&seventy, 0.7)] {
        assert_eq!(sample(pixels, track, fraction - 0.03), fill);
        assert_eq!(sample(pixels, track, fraction + 0.03), track_color);
    }
}
