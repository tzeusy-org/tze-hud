use super::*;
use std::time::{Duration, Instant};

#[tokio::test]
async fn portal_tile_fades_and_interrupted_restore_keeps_eased_opacity() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.token_map.extend([
        ("portal.transition.in_ms".into(), "200".into()),
        ("portal.transition.out_ms".into(), "150".into()),
    ]);
    let node = || solid_node(Rect::new(0.0, 0.0, 256.0, 256.0), Rgba::WHITE, None);
    let mut scene = scene_with_node(node());
    let tile = *scene.tiles.keys().next().unwrap();
    let start = Instant::now();
    compositor.update_portal_tile_animations_at(&scene, start);
    assert!(!compositor.portal_tile_anim_states.contains_key(&tile));
    assert_eq!(compositor.portal_tile_anim_opacity_at(tile, start), 1.0);

    scene
        .register_tile_scroll_config(tile, TileScrollConfig::vertical())
        .unwrap();
    compositor.update_portal_tile_animations_at(&scene, start);
    let fade = &compositor.portal_tile_anim_states[&tile];
    assert_eq!(
        (fade.from_opacity, fade.target_opacity, fade.duration_ms),
        (0.0, 1.0, 200)
    );
    assert_eq!(compositor.portal_tile_anim_opacity_at(tile, start), 0.0);

    let settled = start + Duration::from_millis(200);
    compositor.update_portal_tile_animations_at(&scene, settled);
    assert!(!compositor.portal_tile_anim_states.contains_key(&tile));
    assert_eq!(compositor.portal_tile_anim_opacity_at(tile, settled), 1.0);

    let portal = scene.tiles.get_mut(&tile).unwrap();
    let root = portal.root_node.take();
    assert!(root.is_some());
    compositor.update_portal_tile_animations_at(&scene, settled);
    let fade = &compositor.portal_tile_anim_states[&tile];
    assert_eq!(
        (fade.from_opacity, fade.target_opacity, fade.duration_ms),
        (1.0, 0.0, 150)
    );

    let interrupt = settled + Duration::from_micros(37_500);
    let eased = compositor.portal_tile_anim_opacity_at(tile, interrupt);
    assert!((eased - fade.current_opacity_at(interrupt)).abs() > 0.05);
    scene.tiles.get_mut(&tile).unwrap().root_node = root;
    compositor.update_portal_tile_animations_at(&scene, interrupt);
    let restore = &compositor.portal_tile_anim_states[&tile];
    assert_eq!(
        (
            restore.from_opacity,
            restore.target_opacity,
            restore.duration_ms
        ),
        (eased, 1.0, 200)
    );
    assert_eq!(
        compositor.portal_tile_anim_opacity_at(tile, interrupt),
        eased
    );
}
