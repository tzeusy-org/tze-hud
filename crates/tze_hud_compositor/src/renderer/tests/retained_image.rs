use super::*;
use crate::test_pixels::{assert_pixel_color, pixel_at};

#[tokio::test]
async fn test_static_image_node_composited_with_other_nodes() {
    let (width, height) = (512, 256);
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(width, height).await);
    let mut scene = SceneGraph::new(width as f32, height as f32);
    let tab = scene.create_tab("test", 0).unwrap();
    let lease = scene.grant_lease("agent", 60_000);

    // Register the scene resource without uploading bytes to the compositor.
    let resource_id = ResourceId::of(b"missing 8x8 retained image");
    scene.register_resource(resource_id);
    let bounds = Rect::new(0.0, 0.0, 256.0, 256.0);
    let red = solid_node(bounds, Rgba::new(1.0, 0.0, 0.0, 1.0), None);
    let image = leaf(NodeData::StaticImage(StaticImageNode {
        resource_id,
        width: 8,
        height: 8,
        decoded_bytes: 8 * 8 * 4,
        fit_mode: ImageFitMode::Cover,
        bounds,
    }));

    for (x, z, node) in [(0.0, 1, red), (256.0, 2, image)] {
        let tile = scene
            .create_tile(tab, "agent", lease, Rect::new(x, 0.0, 256.0, 256.0), z)
            .unwrap();
        scene.set_tile_root(tile, node).unwrap();
    }

    prime_text(&mut compositor, &scene);
    let pixels = render_pixels(&mut compositor, &mut scene, &surface);
    assert_eq!(pixels.len(), (width * height * 4) as usize);
    assert_eq!(pixel_at(&pixels, width, 128, 128), [255, 0, 0, 255]);

    // Use the same sRGB swatches and tolerance as tile_static_image_fit_modes.
    for (x, rgba, label) in [
        (258, [196, 188, 179, 255], "placeholder frame"),
        (384, [225, 218, 211, 255], "placeholder inset"),
    ] {
        assert_pixel_color(&pixels, width, x, 128, rgba, 12, label).unwrap();
    }
}
