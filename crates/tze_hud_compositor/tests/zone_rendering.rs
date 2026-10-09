//! Pixel behavior of zone rendering, one table or test per mapping:
//! urgency/severity colour, policy and token overrides, backdrop, text,
//! stacking, layer order, rounded corners and image fit.
//!
//! Expected sRGB bytes are the zone colour blended over the compositor clear
//! colour (linear 0.05, 0.05, 0.1 = sRGB 63, 63, 89), calibrated on llvmpipe.
//! Run via `just test-gpu`, which pins llvmpipe.

mod common;

use std::sync::Arc;

use common::{Frame, Gpu, publish_notification};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    ContentionPolicy, DisplayEdge, GeometryPolicy, ImageFitMode, LayerAttachment, Node, NodeData,
    Rect, RenderingPolicy, ResourceId, Rgba, SceneId, SolidColorNode, StaticImageNode, ZoneContent,
    ZoneDefinition, ZoneMediaType, ZoneRegistry,
};

const CLEAR: [u8; 4] = [63, 63, 89, 255];
const TOL: u8 = 12;

/// A scene with the shipped default zones (subtitle, notification-area,
/// alert-banner, ambient-background, ...).
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
        GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
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

const NOTIF_LOW: [u8; 4] = [25, 25, 39, 255];
const NOTIF_NORMAL: [u8; 4] = [30, 33, 53, 255];
const NOTIF_URGENT: [u8; 4] = [47, 39, 41, 255];
const NOTIF_CRITICAL: [u8; 4] = [68, 28, 44, 255];

/// Each urgency level maps to its own backdrop colour (0.8 alpha over clear).
#[tokio::test]
async fn notification_urgency_maps_to_backdrop_color() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    for (urgency, expected) in [
        (0, NOTIF_LOW),
        (1, NOTIF_NORMAL),
        (2, NOTIF_URGENT),
        (3, NOTIF_CRITICAL),
    ] {
        let mut scene = notification_scene();
        publish_notification(&mut scene, "notification-area", urgency, String::new(), "a");
        gpu.render(&mut scene)
            .expect(222, 17, expected, TOL, &format!("urgency {urgency}"));
    }
}

/// The icon renders as a texture when its bytes are registered; with no icon, or
/// an icon whose bytes were never registered, the slot shows only the backdrop.
#[tokio::test]
async fn notification_icon_renders_only_when_bytes_registered() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    let (icon_id, icon_bytes) = rgba_image(24, 24, [0, 200, 0, 255]);
    gpu.compositor
        .register_image_bytes(icon_id, Arc::from(icon_bytes.as_slice()), 24, 24);
    let unregistered = ResourceId::of(b"unregistered-icon");

    // Icon centre: zone x 192 + inset 9 + half the 24px icon.
    for (icon, expected, what) in [
        (icon_id.to_hex(), [0, 200, 0, 255], "registered icon"),
        (String::new(), NOTIF_LOW, "no icon"),
        (unregistered.to_hex(), NOTIF_LOW, "unregistered icon"),
    ] {
        let mut scene = notification_scene();
        publish_notification(&mut scene, "notification-area", 0, icon, "a");
        gpu.render(&mut scene).expect(213, 17, expected, 20, what);
    }
}

// ─── Alert banner ────────────────────────────────────────────────────────────

const ALERT_INFO: [u8; 4] = [78, 160, 245, 255];
const ALERT_WARNING: [u8; 4] = [244, 211, 26, 255];
const ALERT_CRITICAL: [u8; 4] = [244, 15, 26, 255];
/// The zone's own backdrop, shown for content that carries no urgency.
const ALERT_DEFAULT: [u8; 4] = [87, 87, 109, 255];

// Slot centres on a 256x256 surface (slot_h = 37.6).
const SLOT_Y: [u32; 3] = [18, 56, 94];

/// Urgency 0-1 map to the info colour, 2 to warning, 3 to critical; non-notification
/// content keeps the zone's default backdrop instead of a severity colour.
#[tokio::test]
async fn alert_banner_urgency_maps_to_severity_color() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    let cases = [
        (0, ALERT_INFO),
        (1, ALERT_INFO),
        (2, ALERT_WARNING),
        (3, ALERT_CRITICAL),
    ];
    for (urgency, expected) in cases {
        let mut scene = default_scene(256, 256);
        publish_notification(&mut scene, "alert-banner", urgency, String::new(), "a");
        gpu.render(&mut scene)
            .expect(128, SLOT_Y[0], expected, 8, &format!("urgency {urgency}"));
    }

    let mut scene = default_scene(256, 256);
    publish(
        &mut scene,
        "alert-banner",
        ZoneContent::StreamText("alert".into()),
    );
    gpu.render(&mut scene).expect(
        128,
        SLOT_Y[0],
        ALERT_DEFAULT,
        20,
        "stream text default backdrop",
    );
}

/// Banners stack by severity (critical on top) regardless of arrival order.
#[tokio::test]
async fn alert_banner_stacks_by_severity() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    let mut scene = default_scene(256, 256);
    for (urgency, agent) in [(1, "info"), (2, "warning"), (3, "critical")] {
        publish_notification(&mut scene, "alert-banner", urgency, String::new(), agent);
    }
    let frame = gpu.render(&mut scene);
    for (slot, expected) in [ALERT_CRITICAL, ALERT_WARNING, ALERT_INFO]
        .into_iter()
        .enumerate()
    {
        frame.expect(128, SLOT_Y[slot], expected, 8, &format!("slot {slot}"));
    }
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
    let cases = [
        ("black token default", Some(Rgba::BLACK), [39, 39, 56, 255]),
        (
            "red token override",
            Some(Rgba::new(1.0, 0.0, 0.0, 1.0)),
            [206, 39, 56, 255],
        ),
        ("no backdrop", None, CLEAR),
    ];
    for (what, backdrop, expected) in cases {
        let mut scene = subtitle_scene(256, subtitle_policy(backdrop));
        let frame = gpu.render(&mut scene);
        frame.expect(128, SUBTITLE_Y, expected, 10, what);
        // Anchored to the bottom: the row above the zone is untouched.
        frame.expect(128, 170, CLEAR, 10, &format!("{what}: above zone"));
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

    let mut empty = subtitle_scene(512, subtitle_policy(Some(Rgba::BLACK)));
    empty.zone_registry.active_publishes.clear();
    assert!(
        !gpu.render(&mut empty).has_bright_pixel(51..461, 416..464),
        "no publication, no bright pixels"
    );
}

// ─── Ambient background ──────────────────────────────────────────────────────

const AMBIENT_BLUE: [u8; 4] = [89, 89, 148, 255];
const PLACEHOLDER: [u8; 4] = [149, 149, 149, 255];

/// A solid colour fills the display, and nothing published leaves the clear colour.
#[tokio::test]
async fn ambient_background_fills_display_or_stays_clear() {
    let Some(mut gpu) = Gpu::new(64, 64).await else {
        return;
    };
    let mut scene = default_scene(64, 64);
    publish(&mut scene, "ambient-background", solid(0.1, 0.1, 0.3));
    let frame = gpu.render(&mut scene);
    for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63), (32, 32)] {
        frame.expect(x, y, AMBIENT_BLUE, 8, "solid fill");
    }

    let frame = gpu.render(&mut default_scene(64, 64));
    frame.expect(32, 32, CLEAR, 8, "no publication");
}

/// Latest wins: only the last publication is drawn, however many came before it.
#[tokio::test]
async fn ambient_background_shows_only_latest_publication() {
    let Some(mut gpu) = Gpu::new(64, 64).await else {
        return;
    };
    type Colors = &'static [(f32, f32, f32)];
    let cases: [(&str, Colors, [u8; 4]); 2] = [
        (
            "red then blue",
            &[(1.0, 0.0, 0.0), (0.0, 0.0, 1.0)],
            [0, 0, 255, 255],
        ),
        (
            "ten colours",
            &[
                (1.0, 0.0, 0.0),
                (0.0, 0.0, 1.0),
                (1.0, 1.0, 0.0),
                (1.0, 0.0, 1.0),
                (0.0, 1.0, 1.0),
                (0.5, 0.5, 0.5),
                (1.0, 0.5, 0.0),
                (0.5, 0.0, 0.5),
                (0.0, 0.5, 0.0),
                (0.0, 1.0, 0.0),
            ],
            [0, 255, 0, 255],
        ),
    ];
    for (what, colors, expected) in cases {
        let mut scene = default_scene(64, 64);
        for &(r, g, b) in colors {
            publish(&mut scene, "ambient-background", solid(r, g, b));
        }
        gpu.render(&mut scene).expect(32, 32, expected, 8, what);
    }
}

/// Layer order: a content-layer zone occludes the background; the background
/// still shows outside it.
#[tokio::test]
async fn background_layer_renders_below_content_zone() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    let mut scene = default_scene(256, 256);
    register_zone(
        &mut scene,
        "centre-content",
        GeometryPolicy::Relative {
            x_pct: 0.25,
            y_pct: 0.25,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        RenderingPolicy::default(),
        ContentionPolicy::Replace,
        LayerAttachment::Content,
    );
    publish(&mut scene, "ambient-background", solid(0.0, 0.0, 0.5));
    publish(&mut scene, "centre-content", solid(1.0, 0.0, 0.0));
    let frame = gpu.render(&mut scene);
    for (x, y) in [(0, 0), (255, 0), (0, 255), (255, 255)] {
        frame.expect(x, y, [0, 0, 188, 255], 8, "background corner");
    }
    frame.expect(128, 128, [255, 0, 0, 255], 8, "content over background");
}

/// A static image draws its texture (any aspect ratio) once bytes are registered,
/// and the warm-grey placeholder until then.
#[tokio::test]
async fn ambient_static_image_draws_texture_or_placeholder() {
    let require_gpu = std::env::var("TZE_HUD_REQUIRE_GPU").is_ok_and(|v| v.trim() == "1");
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        assert!(
            !require_gpu,
            "required ambient-image GPU proof cannot skip or lack an adapter"
        );
        return;
    };
    // Textures are evicted once unreferenced, so register right before each frame.
    // Retain the old centre check; the controls below test full-zone coverage
    // rather than assuming a tile-style non-square fitting policy.
    let cases = [
        (
            "square",
            Some((8, 8, [255, 0, 0, 255])),
            [255, 0, 0, 255],
            true,
        ),
        (
            "non-square",
            Some((16, 8, [0, 0, 255, 255])),
            [0, 0, 255, 255],
            false,
        ),
        ("unregistered", None, PLACEHOLDER, true),
    ];
    for (what, image, expected, full_display) in cases {
        let id = match image {
            Some((w, h, px)) => {
                let (id, bytes) = rgba_image(w, h, px);
                println!(
                    "ambient image input {what}: {w}x{h}, RGBA bytes={}, resource={}",
                    bytes.len(),
                    id.to_hex()
                );
                gpu.compositor
                    .register_image_bytes(id, Arc::from(bytes.as_slice()), w, h);
                id
            }
            None => ResourceId::of(b"never registered"),
        };
        let mut scene = default_scene(256, 256);
        publish(
            &mut scene,
            "ambient-background",
            ZoneContent::StaticImage(id),
        );
        let frame = gpu.render(&mut scene);
        let points: &[(u32, u32)] = if full_display {
            &[(0, 0), (255, 255), (128, 128)]
        } else {
            &[(128, 128)]
        };
        for &(x, y) in points {
            frame.expect(x, y, expected, 20, what);
        }
        if what == "non-square" {
            let definition = scene
                .zone_registry
                .get_by_name("ambient-background")
                .unwrap();
            println!(
                "default ambient geometry={:?}, policy={:?}, contention={:?}, layer={:?}",
                definition.geometry_policy,
                definition.rendering_policy,
                definition.contention_policy,
                definition.layer_attachment
            );
            // Source suggests full-zone/full-UV coverage; these literal pixels
            // decide whether the original opaque 16x8 symptom is reproducible.
            let repeated = gpu.render(&mut scene);
            for (x, y) in [
                (0, 0),
                (255, 0),
                (0, 255),
                (255, 255),
                (128, 0),
                (128, 255),
                (0, 128),
                (255, 128),
                (128, 128),
            ] {
                println!(
                    "opaque16x8 ({x},{y}): actual={:?}, expected={expected:?}, repeated={:?}",
                    frame.at(x, y),
                    repeated.at(x, y)
                );
                frame.expect(x, y, expected, 20, "opaque 16x8 full-zone sample");
                assert_eq!(frame.at(x, y), repeated.at(x, y), "referenced repeat");
            }
        }
    }

    // Directional source quadrants pin UV orientation and scaling without a
    // private draw-command oracle or sampling a filtered colour boundary.
    let mut directional: Vec<u8> = Vec::with_capacity(16 * 8 * 4);
    for y in 0..8 {
        for x in 0..16 {
            directional.extend_from_slice(&match (x < 8, y < 4) {
                (true, true) => [255, 0, 0, 255],
                (false, true) => [0, 255, 0, 255],
                (true, false) => [0, 0, 255, 255],
                (false, false) => [255, 255, 0, 255],
            });
        }
    }
    let directional_id = ResourceId::of(&directional);
    println!(
        "directional16x8 RGBA bytes={}, resource={}",
        directional.len(),
        directional_id.to_hex()
    );
    gpu.compositor
        .register_image_bytes(directional_id, Arc::from(directional.as_slice()), 16, 8);
    let mut scene = default_scene(256, 256);
    publish(
        &mut scene,
        "ambient-background",
        ZoneContent::StaticImage(directional_id),
    );
    let directional_frame = gpu.render(&mut scene);
    let directional_repeat = gpu.render(&mut scene);
    for (x, y, expected) in [
        (32, 32, [255, 0, 0, 255]),
        (224, 32, [0, 255, 0, 255]),
        (32, 224, [0, 0, 255, 255]),
        (224, 224, [255, 255, 0, 255]),
    ] {
        println!(
            "directional16x8 ({x},{y}): actual={:?}, expected={expected:?}",
            directional_frame.at(x, y)
        );
        directional_frame.expect(x, y, expected, 20, "directional quadrant");
        assert_eq!(
            directional_frame.at(x, y),
            directional_repeat.at(x, y),
            "directional referenced repeat"
        );
    }

    // Zero-alpha source corners show the compositor clear colour, not a missing
    // resource placeholder. The opaque source centre remains independently green.
    let mut alpha: Vec<u8> = Vec::with_capacity(16 * 8 * 4);
    for y in 0..8 {
        for x in 0..16 {
            alpha.extend_from_slice(&if (4..12).contains(&x) && (2..6).contains(&y) {
                [0, 255, 0, 255]
            } else {
                [0, 0, 0, 0]
            });
        }
    }
    let alpha_id = ResourceId::of(&alpha);
    println!(
        "alpha16x8 RGBA bytes={}, resource={}",
        alpha.len(),
        alpha_id.to_hex()
    );
    gpu.compositor
        .register_image_bytes(alpha_id, Arc::from(alpha.as_slice()), 16, 8);
    publish(
        &mut scene,
        "ambient-background",
        ZoneContent::StaticImage(alpha_id),
    );
    let alpha_frame = gpu.render(&mut scene);
    for (x, y) in [(0, 0), (255, 0), (0, 255), (255, 255)] {
        println!(
            "alpha16x8 ({x},{y}): actual={:?}, clear={CLEAR:?}, placeholder={PLACEHOLDER:?}",
            alpha_frame.at(x, y)
        );
        alpha_frame.expect(x, y, CLEAR, 20, "transparent source corner");
        assert_ne!(alpha_frame.at(x, y), PLACEHOLDER);
    }
    alpha_frame.expect(
        128,
        128,
        [0, 255, 0, 255],
        20,
        "opaque alpha-control centre",
    );

    // Default Replace transitions keep resource identity: missing -> registered
    // -> missing -> registered cannot retain a previous texture or placeholder.
    let missing = ResourceId::of(b"replace-never-registered");
    let (blue_id, blue) = rgba_image(16, 8, [0, 0, 255, 255]);
    for (what, registered, expected) in [
        ("replace missing", false, PLACEHOLDER),
        ("replace registered", true, [0, 0, 255, 255]),
        ("replace missing again", false, PLACEHOLDER),
        ("replace registered again", true, [0, 0, 255, 255]),
    ] {
        let id = if registered {
            // The previous missing frame legitimately evicts unreferenced bytes.
            gpu.compositor
                .register_image_bytes(blue_id, Arc::from(blue.as_slice()), 16, 8);
            blue_id
        } else {
            missing
        };
        publish(
            &mut scene,
            "ambient-background",
            ZoneContent::StaticImage(id),
        );
        let frame = gpu.render(&mut scene);
        for (x, y) in [
            (0, 0),
            (255, 0),
            (0, 255),
            (255, 255),
            (128, 0),
            (128, 255),
            (0, 128),
            (255, 128),
            (128, 128),
        ] {
            println!(
                "{what} ({x},{y}): actual={:?}, expected={expected:?}",
                frame.at(x, y)
            );
            frame.expect(x, y, expected, 20, what);
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
        GeometryPolicy::Relative {
            x_pct: 0.25,
            y_pct: 0.40,
            width_pct: 0.50,
            height_pct: 0.20,
        },
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
async fn zone_backdrop_radius_rounds_corners() {
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
}

/// A tile's solid-colour root with a radius rounds its corners the same way.
#[tokio::test]
async fn tile_solid_color_radius_rounds_corners() {
    let Some(mut gpu) = Gpu::new(256, 256).await else {
        return;
    };
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("test-agent", 300_000);
    let tile = scene
        .create_tile(
            tab,
            "test-agent",
            lease,
            Rect::new(64.0, 102.0, 128.0, 51.0),
            10,
        )
        .unwrap();
    let root = |radius| Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::new(0.0, 0.5, 0.0, 1.0),
            bounds: Rect::new(0.0, 0.0, 128.0, 51.0),
            radius,
        }),
    };
    scene.set_tile_root(tile, root(Some(16.0))).unwrap();
    let frame = gpu.render(&mut scene);
    frame.expect(128, 128, GREEN, TOL, "tile interior");
    frame.expect(64, 102, CLEAR, TOL, "rounded tile corner");
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
    // Tile background (dark) is anything with a low blue/red channel; compare to CLEAR.
    let cases = [
        ("fill", Fill, (8, 8), green, (128, 128), green),
        ("contain centre", Contain, (16, 8), blue, (128, 128), blue),
        (
            "contain letterbox",
            Contain,
            (16, 8),
            blue,
            (128, 30),
            CLEAR,
        ),
        ("cover centre", Cover, (8, 16), magenta, (128, 128), magenta),
        ("cover corner", Cover, (8, 16), magenta, (2, 2), magenta),
        (
            "scale-down native",
            ScaleDown,
            (4, 4),
            yellow,
            (128, 128),
            yellow,
        ),
        (
            "scale-down surround",
            ScaleDown,
            (4, 4),
            yellow,
            (0, 0),
            CLEAR,
        ),
    ];
    for (what, fit_mode, (w, h), px, (x, y), expected) in cases {
        let (id, bytes) = rgba_image(w, h, px);
        gpu.compositor
            .register_image_bytes(id, Arc::from(bytes.as_slice()), w, h);
        let mut scene = SceneGraph::new(256.0, 256.0);
        scene.register_resource(id);
        let tab = scene.create_tab("test", 0).unwrap();
        let lease = scene.grant_lease("test-agent", 60_000);
        let tile = scene
            .create_tile(
                tab,
                "test-agent",
                lease,
                Rect::new(0.0, 0.0, 256.0, 256.0),
                1,
            )
            .unwrap();
        scene
            .set_tile_root(
                tile,
                Node {
                    layout: Default::default(),
                    id: SceneId::new(),
                    children: vec![],
                    data: NodeData::StaticImage(StaticImageNode {
                        resource_id: id,
                        width: w,
                        height: h,
                        decoded_bytes: u64::from(w) * u64::from(h) * 4,
                        fit_mode,
                        bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
                    }),
                },
            )
            .unwrap();
        let frame: Frame = gpu.render(&mut scene);
        frame.expect(x, y, expected, 40, what);
    }
}
