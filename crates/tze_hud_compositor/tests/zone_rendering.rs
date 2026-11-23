//! Pixel behavior of zone rendering, one table or test per mapping:
//! urgency/severity colour, policy and token overrides, backdrop, text,
//! stacking, layer order, rounded corners and image fit.
//!
//! Expected sRGB bytes are the zone colour blended over the compositor clear
//! colour (linear 0.05, 0.05, 0.1 = sRGB 63, 63, 89), calibrated on llvmpipe.
//! Run via `just test-gpu`, which pins llvmpipe.

use crate::common;

use std::sync::Arc;

use common::{Frame, Gpu, publish_notification, relative};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    ContentionPolicy, DisplayEdge, GeometryPolicy, ImageFitMode, LayerAttachment, Node, NodeData,
    Rect, RenderingPolicy, ResourceId, Rgba, SceneId, SolidColorNode, StaticImageNode, ZoneContent,
    ZoneDefinition, ZoneMediaType, ZoneRegistry,
};

const CLEAR: [u8; 4] = [63, 63, 89, 255];
const TOL: u8 = 12;

/// Lift the ambient opacity ceiling for exact fill colors after the default
/// cap check in `assert_solid_ambient_pixels`.
fn uncap_ambient(gpu: &mut Gpu) {
    gpu.compositor.set_token_map(
        [("opacity.ambient.max".to_owned(), "1".to_owned())]
            .into_iter()
            .collect(),
    );
}

/// A scene with the shipped default zones (subtitle, notification-area,
/// alert-banner, ambient-background,...).
fn default_scene(w: u32, h: u32) -> SceneGraph {
    let mut scene = SceneGraph::new(w as f32, h as f32);
    scene.zone_registry = ZoneRegistry::with_defaults();
    scene
}

fn register_zone(
    scene: &mut SceneGraph,
    name: &str,
    geometry_policy: GeometryPolicy,
    rendering_policy: RenderingPolicy,
    contention_policy: ContentionPolicy,
    layer_attachment: LayerAttachment,
) {
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: name.to_owned(),
        description: String::new(),
        geometry_policy,
        accepted_media_types: vec![
            ZoneMediaType::StreamText,
            ZoneMediaType::ShortTextWithIcon,
            ZoneMediaType::SolidColor,
        ],
        rendering_policy,
        contention_policy,
        max_publishers: 16,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment,
    });
}

fn publish(scene: &mut SceneGraph, zone: &str, content: ZoneContent) {
    scene
        .publish_to_zone(zone, content, "test-agent", None, None, None)
        .unwrap_or_else(|e| panic!("publish to {zone} failed: {e:?}"));
}

fn solid(r: f32, g: f32, b: f32) -> ZoneContent {
    ZoneContent::SolidColor(Rgba { r, g, b, a: 1.0 })
}

fn rgba_image(w: u32, h: u32, px: [u8; 4]) -> (ResourceId, Vec<u8>) {
    let bytes = px.repeat((w * h) as usize);
    (ResourceId::of(&bytes), bytes)
}

// ─── Notification area ───────────────────────────────────────────────────────

/// A notification-area zone with a backdrop (the default zone has none), 256x256:
/// zone x=192..253, slot 0 centre (222, 17).
fn notification_scene() -> SceneGraph {
    let mut scene = SceneGraph::new(256.0, 256.0);
    register_zone(
        &mut scene,
        "notification-area",
        relative(0.75, 0.0, 0.24, 0.30),
        RenderingPolicy {
            backdrop: Some(Rgba::new(0.05, 0.05, 0.05, 0.85)),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        ContentionPolicy::Stack { max_depth: 5 },
        LayerAttachment::Chrome,
    );
    scene
}

// Canonical card tones at opacity.notification (0.92) over clear:
// color.notification.container, caution.container, error.container.
const NOTIF_LOW: [u8; 4] = [37, 42, 55, 255];
const NOTIF_URGENT: [u8; 4] = [73, 58, 30, 255];
const NOTIF_CRITICAL: [u8; 4] = [90, 31, 34, 255];

/// Urgency 0-1 share the neutral card tone; 2 and 3 take the caution and
/// error containers.
#[tokio::test]
async fn notification_and_alert_pixels_follow_their_content_and_roles() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    assert_alert_pixels(&mut gpu);
    gpu.compositor.set_token_map(Default::default());
    let (icon_id, icon_bytes) = rgba_image(24, 24, [0, 200, 0, 255]);
    gpu.compositor
        .register_image_bytes(icon_id, Arc::from(icon_bytes.as_slice()), 24, 24);
    let unregistered = ResourceId::of(b"unregistered-icon");

    // Use the registered icon first: the frame evicts unused textures.
    for (urgency, icon, (x, y), expected, tolerance, what) in [
        (
            0,
            Some(icon_id),
            (224, 26),
            [0, 200, 0, 255],
            20,
            "registered icon",
        ),
        (2, None, (222, 17), NOTIF_URGENT, TOL, "caution"),
        (3, None, (222, 17), NOTIF_CRITICAL, TOL, "critical"),
        (1, None, (224, 26), NOTIF_LOW, TOL, "no icon"),
        (
            0,
            Some(unregistered),
            (224, 26),
            NOTIF_LOW,
            20,
            "unregistered icon",
        ),
    ] {
        let mut scene = notification_scene();
        let icon = icon.map_or_else(String::new, |id| id.to_hex());
        publish_notification(&mut scene, "notification-area", urgency, icon, "a");
        gpu.render(&mut scene)
            .expect(x, y, expected, tolerance, what);
    }

    gpu.compositor
        .init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = notification_scene();
    scene
        .zone_registry
        .zones
        .get_mut("notification-area")
        .unwrap()
        .geometry_policy = relative(0.1, 0.0, 0.8, 0.3);
    publish_notification(&mut scene, "notification-area", 1, String::new(), "a");
    assert!(
        gpu.render(&mut scene).has_bright_pixel(42..180, 16..36),
        "notification body draws glyphs"
    );
}

// ─── Alert banner ────────────────────────────────────────────────────────────

struct AlertThemePixels {
    name: &'static str,
    tones: [[u8; 4]; 3],
    centers_y: [u32; 3],
    inset: bool,
}

// Explicit shipped-palette expectations, composited over CLEAR by llvmpipe.
// Centres are inside each theme's card; Tonal Glass and Blueprint leave a
// 16 dp outer margin and gap, while Classic retains flush coloured strips.
const ALERT_THEMES: [AlertThemePixels; 3] = [
    AlertThemePixels {
        name: "tonal-glass",
        tones: [[37, 42, 55, 255], [37, 42, 55, 255], [90, 31, 34, 255]],
        centers_y: [40, 104, 168],
        inset: true,
    },
    AlertThemePixels {
        name: "blueprint",
        tones: [[48, 52, 61, 255], [48, 52, 61, 255], [140, 42, 47, 255]],
        centers_y: [38, 98, 158],
        inset: true,
    },
    AlertThemePixels {
        name: "classic",
        tones: [[73, 152, 244, 255], [244, 176, 26, 255], [244, 15, 26, 255]],
        centers_y: [16, 50, 84],
        inset: false,
    },
];

/// Use the real startup token/policy resolution, rather than the registry's
/// intentionally unresolved defaults.
fn alert_scene(gpu: &mut Gpu, theme: &str) -> SceneGraph {
    let tokens = tze_hud_config::themes::resolve_config_tokens(
        &[("theme".to_owned(), theme.to_owned())].into(),
    );
    let mut scene = default_scene(256, 256);
    let policy = &mut scene
        .zone_registry
        .zones
        .get_mut("alert-banner")
        .unwrap()
        .rendering_policy;
    tze_hud_config::policy_builder::apply_alert_banner_token_defaults(policy, &tokens);
    gpu.compositor.set_token_map(tokens);
    scene
}

// Severity controls the stack, with modern inset cards and Classic strips.
fn assert_alert_pixels(gpu: &mut Gpu) {
    for (theme, [low, critical]) in ALERT_THEMES.iter().zip([[1, 3], [0, 3], [1, 5]]) {
        let mut scene = alert_scene(gpu, theme.name);
        for (urgency, agent) in [(low, "info"), (2, "warning"), (critical, "critical")] {
            publish_notification(&mut scene, "alert-banner", urgency, String::new(), agent);
        }
        let frame = gpu.render(&mut scene);
        for (slot, tone) in [2, 1, 0].into_iter().enumerate() {
            let label = format!("{} slot {slot}", theme.name);
            frame.expect(128, theme.centers_y[slot], theme.tones[tone], 8, &label);
            frame.expect(
                4,
                theme.centers_y[slot],
                if theme.inset {
                    CLEAR
                } else {
                    theme.tones[tone]
                },
                8,
                &format!("{label}: outer margin"),
            );
        }
        if theme.inset {
            frame.expect(128, 8, CLEAR, 2, "top margin stays clear");
            frame.expect(128, 72, CLEAR, 2, "inter-card gap stays clear");
        }
    }
    // Stream text uses the zone backdrop instead of a notification role.
    let theme = &ALERT_THEMES[0];
    let mut scene = alert_scene(gpu, theme.name);
    let policy = &mut scene
        .zone_registry
        .zones
        .get_mut("alert-banner")
        .unwrap()
        .rendering_policy;
    policy.backdrop = Some(Rgba::new(0.1, 0.1, 0.16, 0.9));
    policy.backdrop_opacity = Some(0.9);
    publish(
        &mut scene,
        "alert-banner",
        ZoneContent::StreamText("alert".into()),
    );
    gpu.render(&mut scene).expect(
        128,
        theme.centers_y[0],
        [87, 87, 109, 255],
        8,
        "stream text keeps its policy backdrop",
    );
}

// ─── Subtitle ────────────────────────────────────────────────────────────────

/// Subtitle zone centre on a 256x256 surface (bottom-anchored, 48px margin).
const SUBTITLE_Y: u32 = 194;

fn subtitle_scene(size: u32, policy: RenderingPolicy) -> SceneGraph {
    let mut scene = SceneGraph::new(size as f32, size as f32);
    register_zone(
        &mut scene,
        "subtitle",
        GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 48.0,
        },
        policy,
        ContentionPolicy::LatestWins,
        LayerAttachment::Content,
    );
    publish(
        &mut scene,
        "subtitle",
        ZoneContent::StreamText("Hello world".into()),
    );
    scene
}

fn subtitle_policy(backdrop: Option<Rgba>) -> RenderingPolicy {
    RenderingPolicy {
        backdrop,
        backdrop_opacity: Some(0.6),
        text_color: Some(Rgba::WHITE),
        font_size_px: Some(28.0),
        ..Default::default()
    }
}

/// The backdrop follows the policy colour at its opacity (a token override changes
/// it), is absent when the policy has none, and sits at the bottom edge.
#[tokio::test]
async fn subtitle_backdrop_follows_policy() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    use tze_hud_scene::DegradationLevel::{Nominal, Simplified};

    // Independent samples of the whole-zone fill and the local subtitle pill.
    let red = Some(Rgba::new(1.0, 0.0, 0.0, 1.0));
    let cases = [
        (
            Nominal,
            None,
            true,
            Some(Rgba::BLACK),
            [39, 39, 56, 255],
            [24, 26, 35, 255],
        ),
        (
            Nominal,
            Some(6.0),
            true,
            red,
            [206, 39, 56, 255],
            [116, 26, 35, 255],
        ),
        (Nominal, None, true, None, CLEAR, [36, 37, 52, 255]),
        (
            Nominal,
            Some(6.0),
            false,
            red,
            [206, 39, 56, 255],
            [206, 39, 56, 255],
        ),
        (
            Simplified,
            Some(6.0),
            true,
            red,
            [255, 0, 0, 255],
            [15, 18, 22, 255],
        ),
        (Simplified, None, true, None, CLEAR, [15, 18, 22, 255]),
        (Simplified, None, false, None, CLEAR, CLEAR),
    ];
    for (level, radius, pill, backdrop, outside, inside) in cases {
        gpu.compositor
            .set_degradation_policy(tze_hud_compositor::CompositorDegradationPolicy {
                level,
                ..Default::default()
            });
        gpu.compositor
            .set_token_map(std::collections::HashMap::from([(
                "opacity.subtitle".into(),
                if pill { "1" } else { "0" }.into(),
            )]));
        let mut policy = subtitle_policy(backdrop);
        policy.backdrop_radius = radius;
        let mut scene = subtitle_scene(256, policy);
        let frame = gpu.render(&mut scene);
        let label = format!("level={level:?} radius={radius:?} pill={pill} fill={backdrop:?}");
        for (x, y, expected) in [
            (28, SUBTITLE_Y, outside),
            (128, SUBTITLE_Y, inside),
            (128, 170, CLEAR),
        ] {
            frame.expect(x, y, expected, 10, &label);
        }
    }
}

/// White policy text actually draws glyphs in the zone (512px so the zone is tall
/// enough for 28px glyphs; text needs the text renderer).
#[tokio::test]
async fn subtitle_text_is_drawn() {
    let Some(mut gpu) = Gpu::new(512, 512).await else {
        return;
    };
    gpu.compositor
        .init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = subtitle_scene(512, subtitle_policy(Some(Rgba::BLACK)));
    assert!(
        gpu.render(&mut scene).has_bright_pixel(51..461, 416..464),
        "white subtitle text must put bright pixels in the zone"
    );

    scene.clear_zone("subtitle").unwrap();
    assert!(
        !gpu.render(&mut scene).has_bright_pixel(51..461, 416..464),
        "no publication, no bright pixels"
    );
}

// ─── Ambient background ──────────────────────────────────────────────────────

const AMBIENT_BLUE: [u8; 4] = [89, 89, 148, 255];
const PLACEHOLDER: [u8; 4] = [149, 149, 149, 255];

const CORNERS: [(u32, u32); 4] = [(0, 0), (255, 0), (0, 255), (255, 255)];

#[tokio::test]
async fn ambient_background_composites_latest_solid_or_image() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    assert_solid_ambient_pixels(&mut gpu);
    assert_ambient_image_pixels(&mut gpu);
}

fn assert_solid_ambient_pixels(gpu: &mut Gpu) {
    let mut scene = default_scene(256, 256);
    gpu.render(&mut scene)
        .expect(128, 128, CLEAR, 8, "no publication");
    publish(&mut scene, "ambient-background", solid(0.1, 0.1, 0.3));
    let frame = gpu.render(&mut scene);
    for (x, y) in CORNERS.into_iter().chain([(128, 128)]) {
        frame.expect(x, y, AMBIENT_BLUE, 8, "solid fill");
    }
    publish(&mut scene, "ambient-background", solid(0.0, 0.0, 1.0));
    let capped = gpu.render(&mut scene).at(128, 128);
    assert!(
        capped[0] > 8 && capped[1] > 8 && capped[2] > 200,
        "opacity cap lets base through: {capped:?}"
    );

    uncap_ambient(gpu);
    for (r, g, b) in [(1.0, 0.0, 0.0), (0.0, 0.0, 1.0), (0.0, 1.0, 0.0)] {
        publish(&mut scene, "ambient-background", solid(r, g, b));
    }
    gpu.render(&mut scene)
        .expect(128, 128, [0, 255, 0, 255], 8, "latest solid wins");

    register_zone(
        &mut scene,
        "centre-content",
        relative(0.25, 0.25, 0.5, 0.5),
        RenderingPolicy::default(),
        ContentionPolicy::Replace,
        LayerAttachment::Content,
    );
    publish(&mut scene, "ambient-background", solid(0.0, 0.0, 0.5));
    publish(&mut scene, "centre-content", solid(1.0, 0.0, 0.0));
    let frame = gpu.render(&mut scene);
    for (x, y) in CORNERS {
        frame.expect(x, y, [0, 0, 188, 255], 8, "uncovered background");
    }
    frame.expect(128, 128, [255, 0, 0, 255], 8, "content over background");
}

fn assert_ambient_image_pixels(gpu: &mut Gpu) {
    let mut scene = default_scene(256, 256);
    let extent = CORNERS
        .into_iter()
        .chain([(64, 0), (128, 255), (0, 128), (255, 128), (128, 128)]);
    let red = [255, 0, 0, 255];
    let green = [0, 255, 0, 255];
    let blue = [0, 0, 255, 255];
    let yellow = [255, 255, 0, 255];

    let register_image = |gpu: &mut Gpu, w, h, bytes: &[u8]| {
        let id = ResourceId::of(bytes);
        gpu.compositor
            .register_image_bytes(id, Arc::from(bytes), w, h);
        id
    };
    let ambient = |scene: &mut SceneGraph, id| {
        publish(scene, "ambient-background", ZoneContent::StaticImage(id))
    };

    let opaque = blue.repeat(16 * 8);
    // A single non-square texture pins the full extent and UV orientation.
    let directional: Vec<u8> = (0..8)
        .flat_map(|y| {
            (0..16).flat_map(move |x| match (x < 8, y < 4) {
                (true, true) => red,
                (false, true) => green,
                (true, false) => blue,
                (false, false) => yellow,
            })
        })
        .collect();
    ambient(&mut scene, register_image(gpu, 16, 8, &directional));
    let frame = gpu.render(&mut scene);
    let retained = gpu.render(&mut scene);
    for (x, y) in CORNERS
        .into_iter()
        .chain([(32, 32), (224, 32), (32, 224), (224, 224)])
    {
        let expected = match (x < 128, y < 128) {
            (true, true) => red,
            (false, true) => green,
            (true, false) => blue,
            (false, false) => yellow,
        };
        frame.expect(x, y, expected, 20, "wide image quadrant and display edge");
        assert_eq!(
            retained.at(x, y),
            frame.at(x, y),
            "retained directional texture"
        );
    }

    let alpha: Vec<u8> = (0..8)
        .flat_map(|y| {
            (0..16).flat_map(move |x| {
                if (4..12).contains(&x) && (2..6).contains(&y) {
                    green
                } else {
                    [0; 4]
                }
            })
        })
        .collect();
    ambient(&mut scene, register_image(gpu, 16, 8, &alpha));
    let frame = gpu.render(&mut scene);
    for (x, y) in CORNERS {
        frame.expect(x, y, CLEAR, 20, "transparent source");
        assert_ne!(frame.at(x, y), PLACEHOLDER);
    }
    frame.expect(128, 128, green, 20, "opaque source centre");

    // Replacement leaves no stale texture or placeholder, including after
    // the compositor evicts the resource from an unreferenced frame.
    let missing = ResourceId::of(b"unregistered ambient image");
    for (registered, expected) in [
        (false, PLACEHOLDER),
        (true, blue),
        (false, PLACEHOLDER),
        (true, blue),
    ] {
        let id = if registered {
            register_image(gpu, 16, 8, &opaque)
        } else {
            missing
        };
        ambient(&mut scene, id);
        let frame = gpu.render(&mut scene);
        for (x, y) in extent.clone() {
            frame.expect(x, y, expected, 20, "ambient replacement");
        }
    }
}

// ─── Rounded corners and image tiles ─────────────────────────────────────────

const GREEN: [u8; 4] = [0, 188, 0, 255];

/// Zone at x=64..192, y=102..153 with a green backdrop (when `backdrop`).
fn rounded_scene(radius: Option<f32>, backdrop: bool, layer: LayerAttachment) -> SceneGraph {
    let mut scene = default_scene(256, 256);
    register_zone(
        &mut scene,
        "rounded",
        relative(0.25, 0.40, 0.50, 0.20),
        RenderingPolicy {
            backdrop: backdrop.then(|| Rgba::new(0.0, 0.5, 0.0, 1.0)),
            backdrop_radius: radius,
            ..Default::default()
        },
        ContentionPolicy::LatestWins,
        layer,
    );
    publish(
        &mut scene,
        "rounded",
        ZoneContent::StreamText("test".into()),
    );
    scene
}

/// backdrop_radius rounds the zone backdrop's corners (flat without it, on any
/// layer), and does nothing when the policy has no backdrop.
#[tokio::test]
async fn zones_and_tiles_round_their_backdrop_corners() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    use LayerAttachment::{Background, Content};
    // (radius, backdrop, layer, interior, near-corner)
    let cases = [
        ("radius 16", Some(16.0), true, Content, GREEN, CLEAR),
        (
            "radius 40 clamps to 25",
            Some(40.0),
            true,
            Content,
            GREEN,
            CLEAR,
        ),
        ("flat", None, true, Content, GREEN, GREEN),
        (
            "background layer",
            Some(40.0),
            true,
            Background,
            GREEN,
            CLEAR,
        ),
        (
            "radius without backdrop",
            Some(8.0),
            false,
            Content,
            CLEAR,
            CLEAR,
        ),
    ];
    for (what, radius, backdrop, layer, interior, corner) in cases {
        let frame = gpu.render(&mut rounded_scene(radius, backdrop, layer));
        frame.expect(128, 128, interior, TOL, &format!("{what}: interior"));
        frame.expect(66, 104, corner, TOL, &format!("{what}: corner"));
    }

    let tile = NodeData::SolidColor(SolidColorNode {
        color: Rgba::new(0.0, 0.5, 0.0, 1.0),
        bounds: Rect::new(0.0, 0.0, 128.0, 51.0),
        radius: Some(16.0),
    });
    let frame = gpu.render(&mut tile_scene(
        Rect::new(64.0, 102.0, 128.0, 51.0),
        tile,
        None,
    ));
    frame.expect(128, 128, GREEN, TOL, "tile interior");
    frame.expect(64, 102, CLEAR, TOL, "rounded tile corner");
}

fn tile_scene(bounds: Rect, content: NodeData, image: Option<ResourceId>) -> SceneGraph {
    let mut scene = SceneGraph::new(256.0, 256.0);
    if let Some(image) = image {
        scene.register_resource(image);
    }
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("test-agent", 60_000);
    let tile = scene
        .create_tile(tab, "test-agent", lease, bounds, 1)
        .unwrap();
    scene
        .set_tile_root(
            tile,
            Node {
                layout: Default::default(),
                id: SceneId::new(),
                children: vec![],
                data: content,
            },
        )
        .unwrap();
    scene
}

/// Each image fit mode places a solid-colour image in a 256x256 tile:
/// (mode, image size, colour, point, expected).
#[tokio::test]
async fn tile_static_image_fit_modes() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    use ImageFitMode::{Contain, Cover, Fill, ScaleDown};
    let yellow = [255, 255, 0, 255];
    let blue = [0, 0, 255, 255];
    let magenta = [255, 0, 255, 255];
    let green = [0, 255, 0, 255];
    let cases = [
        (Fill, (8, 8), green, &[(128, 128, green)][..]),
        (
            Contain,
            (16, 8),
            blue,
            &[(128, 128, blue), (128, 30, CLEAR)][..],
        ),
        (
            Cover,
            (8, 16),
            magenta,
            &[(128, 128, magenta), (2, 2, magenta)][..],
        ),
        (
            ScaleDown,
            (4, 4),
            yellow,
            &[(128, 128, yellow), (0, 0, CLEAR)][..],
        ),
    ];
    for (fit_mode, (w, h), px, points) in cases {
        let (id, bytes) = rgba_image(w, h, px);
        let bounds = Rect::new(0.0, 0.0, 256.0, 256.0);
        let image = NodeData::StaticImage(StaticImageNode {
            resource_id: id,
            width: w,
            height: h,
            decoded_bytes: u64::from(w) * u64::from(h) * 4,
            fit_mode,
            bounds,
        });
        let mut scene = tile_scene(bounds, image, Some(id));
        if fit_mode == Fill {
            let placeholder = gpu.render(&mut scene);
            placeholder.expect(2, 2, [196, 188, 179, 255], TOL, "placeholder frame");
            placeholder.expect(128, 128, [225, 218, 211, 255], TOL, "placeholder inset");
        }
        gpu.compositor
            .register_image_bytes(id, Arc::from(bytes.as_slice()), w, h);
        let frame: Frame = gpu.render(&mut scene);
        for &(x, y, expected) in points {
            frame.expect(x, y, expected, 40, &format!("{fit_mode:?}"));
        }
    }
}
