use super::*;
use crate::renderer::status_bar::StatusBarCard;

// ── Status-bar card: one hugged rect shared by every draw path ─────────────

const SW: f32 = 1920.0;
const SH: f32 = 1080.0;
const FONT_PX: f32 = 12.0;
const PAD_PX: f32 = 9.0;

fn status_bar_zone(policy: RenderingPolicy) -> ZoneDefinition {
    ZoneDefinition {
        id: SceneId::new(),
        name: "status-bar".to_owned(),
        description: "status-bar card test".to_owned(),
        // The built-in bottom-right region: 1574.4,799.2 307.2x259.2 at 1080p.
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.83,
            y_pct: 0.74,
            width_pct: 0.16,
            height_pct: 0.24,
        },
        accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
        rendering_policy: policy,
        contention_policy: ContentionPolicy::MergeByKey { max_keys: 32 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    }
}

fn card_policy() -> RenderingPolicy {
    RenderingPolicy {
        backdrop: Some(Rgba::new(0.1, 0.12, 0.15, 1.0)),
        backdrop_opacity: Some(0.92),
        backdrop_radius: Some(0.0),
        text_color: Some(Rgba::WHITE),
        font_size_px: Some(FONT_PX),
        margin_horizontal: Some(PAD_PX),
        margin_vertical: Some(PAD_PX),
        ..Default::default()
    }
}

fn scene_with_entries(policy: RenderingPolicy, entries: &[(&str, &str)]) -> SceneGraph {
    let mut scene = SceneGraph::new(SW, SH);
    scene.register_zone(status_bar_zone(policy));
    if !entries.is_empty() {
        publish_entries(&mut scene, "agent", entries.iter().copied());
    }
    scene
}

fn publish_entries<K: Into<String>, V: Into<String>>(
    scene: &mut SceneGraph,
    agent: &str,
    entries: impl IntoIterator<Item = (K, V)>,
) {
    let entries = entries
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload { entries }),
            agent,
            Some(agent.into()),
            None,
            None,
        )
        .unwrap();
}

fn zone_rect() -> Rect {
    Rect::new(0.83 * SW, 0.74 * SH, 0.16 * SW, 0.24 * SH)
}

fn records(scene: &SceneGraph) -> &[ZonePublishRecord] {
    scene
        .zone_registry
        .active_publishes
        .get("status-bar")
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn card_for(compositor: &Compositor, scene: &SceneGraph) -> Option<StatusBarCard> {
    let zone = &scene.zone_registry.zones["status-bar"];
    let (x, y, w, h) =
        compositor.zone_geometry("status-bar", &scene.effective_zone_geometry(zone), SW, SH);
    compositor.status_bar_card(
        "status-bar",
        records(scene),
        zone.contention_policy,
        &zone.rendering_policy,
        Rect::new(x, y, w, h),
    )
}

fn prime_status_text(compositor: &mut Compositor, scene: &SceneGraph) {
    compositor.prime_markdown_cache(scene);
    compositor.prime_truncation_cache(scene);
}

/// Entry count changes width, while the pill keeps one row and its screen insets.
#[tokio::test]
async fn status_bar_card_hugs_rows_and_is_bottom_anchored() {
    let (mut compositor, _surface) =
        require_gpu!(make_compositor_and_surface(SW as u32, SH as u32).await);
    let zone = zone_rect();
    let defaults = tze_hud_config::tokens::resolve_tokens(&HashMap::new(), &HashMap::new());
    let entries = [("k0", "v0"), ("k1", "v1"), ("k2", "v2"), ("k3", "v3")];
    let mut previous_width = 0.0;
    for n in 1..=entries.len() {
        let card = card_for(
            &compositor,
            &scene_with_entries(card_policy(), &entries[..n]),
        )
        .unwrap();
        assert_eq!(card.entries.len(), n);
        assert_eq!(card.rect.height, 36.0);
        assert!((card.rect.y + card.rect.height - (zone.y + zone.height - 16.0)).abs() < 0.01);
        assert!((card.rect.x + card.rect.width - (zone.x + zone.width - 16.0)).abs() < 0.01);
        assert!(card.rect.width > previous_width, "pill grows horizontally");
        previous_width = card.rect.width;
    }
    for factor in [1.0, 1.5, 1.875] {
        let mut tokens = defaults.clone();
        tze_hud_config::tokens::set_px_scale(&mut tokens, factor);
        tokens.insert("space.status.key_value".into(), "10".into());
        let policy = tze_hud_config::policy_builder::build_effective_policy(
            "status-bar",
            &RenderingPolicy::default(),
            &tokens,
        );
        compositor.set_token_map(tokens);
        let card = card_for(
            &compositor,
            &scene_with_entries(policy, &[("scale", "150%")]),
        )
        .unwrap();
        let factor = factor as f32;
        assert!((card.rect.height - 36.0 * factor).abs() < 0.01);
        assert!((zone.x + zone.width - card.rect.x - card.rect.width - 16.0 * factor).abs() < 0.01);
        let items = card.text_items(1.0);
        assert_eq!(items.len(), 2);
        assert!((items[0].font_size_px - 12.0 * factor).abs() < 0.01);
        assert!(
            (items[1].pixel_x - items[0].pixel_x - items[0].bounds_width - 10.0 * factor).abs()
                < 0.01
        );
    }

    compositor.set_token_map(defaults);
    let unordered = scene_with_entries(
        card_policy(),
        &[("wifi", "ok"), ("agent", "Claude"), ("cpu", "4%")],
    );
    let card = card_for(&compositor, &unordered).unwrap();
    assert!(
        card.entries
            .iter()
            .map(|entry| entry.key.as_str())
            .eq(["agent", "cpu", "wifi"])
    );

    let policy = RenderingPolicy {
        key_icon_map: HashMap::from([("build".into(), "build.svg".into())]),
        ..card_policy()
    };
    let mut scene = scene_with_entries(policy, &[("build", "green")]);
    publish_entries(&mut scene, "second", [("agent", "Claude reviewing")]);
    let card = card_for(&compositor, &scene).unwrap();
    let items = card.text_items(1.0);
    assert!(items.iter().map(|item| item.text.as_ref()).eq([
        "agent",
        "Claude reviewing",
        "build",
        "green",
    ]));
    for (entry, pair) in card.entries.iter().zip(items.chunks_exact(2)) {
        let [key, value] = pair else { unreachable!() };
        let icon = (entry.key == "build").then_some("build.svg");
        assert_eq!(entry.icon.as_deref(), icon);
        let inset = if icon.is_some() { 20.0 } else { 0.0 };
        assert!((key.pixel_x - card.rect.x - entry.x - inset).abs() < 0.01);
        assert!((value.pixel_x - key.pixel_x - key.bounds_width - 6.0).abs() < 0.01);
        assert_eq!(value.color[..3], [255, 255, 255]);
        assert_ne!(key.color[..3], value.color[..3]);
    }
    for item in &items {
        assert_eq!(item.overflow, TextOverflow::Ellipsis);
        assert_eq!(item.bounds_height, 16.0);
        assert_eq!(item.pixel_y, card.rect.y + 10.0);
        assert!(item.pixel_x >= card.rect.x + PAD_PX);
        assert!(item.pixel_x + item.bounds_width <= card.rect.x + card.rect.width - PAD_PX + 0.01);
    }
    let mut scene = scene_with_entries(card_policy(), &[]);
    publish_entries(
        &mut scene,
        "agent",
        (0..40).rev().map(|i| (format!("key{i:02}"), "v")),
    );
    let card = card_for(&compositor, &scene).unwrap();
    assert!(
        (1..40).contains(&card.entries.len()),
        "hide suffix that cannot fit"
    );
    assert!(
        card.entries
            .iter()
            .enumerate()
            .all(|(i, entry)| entry.key == format!("key{i:02}"))
    );
    assert!(card.rect.width <= zone.width - 32.0);
    assert_eq!(card.rect.height, 36.0);
    assert!(
        card.entries
            .windows(2)
            .all(|pair| pair[0].x + pair[0].width < pair[1].x)
    );
}

/// The flat backdrop, the SDF card (with its border) and the drag handle's
/// element bounds are all exactly the hugged card rect.
#[tokio::test]
async fn status_bar_draw_paths_share_the_card_rect() {
    let (mut compositor, surface) =
        require_gpu!(make_compositor_and_surface(SW as u32, SH as u32).await);
    let entries = [("agent", "Claude"), ("build", "green")];

    let rounded_policy = RenderingPolicy {
        backdrop_radius: Some(8.0),
        ..card_policy()
    };
    let mut empty = scene_with_entries(rounded_policy.clone(), &[]);
    empty
        .zone_registry
        .zones
        .get_mut("status-bar")
        .unwrap()
        .contention_policy = ContentionPolicy::LatestWins;
    publish_entries(&mut empty, "agent", std::iter::empty::<(&str, &str)>());
    assert!(card_for(&compositor, &empty).is_none());
    let flat = scene_with_entries(card_policy(), &entries);
    let card = card_for(&compositor, &flat).unwrap().rect;
    let mut scene = scene_with_entries(rounded_policy, &entries);
    for (scene, expected_vertices, radius) in [(&empty, 0, 0.0), (&flat, 12, 0.0), (&scene, 6, 8.0)]
    {
        let mut vertices = Vec::new();
        compositor.render_zone_content(scene, &mut vertices, &mut Vec::new(), SW, SH, None);
        assert_eq!(vertices.len(), expected_vertices);
        let sdf = compositor.collect_all_rounded_rect_cmds(scene, SW, SH);
        let handles = compositor.collect_drag_handle_entries(scene, SW, SH);
        if expected_vertices == 0 {
            assert!(sdf.chrome.is_empty() && handles.is_empty());
            assert!(compositor.collect_text_items(scene, SW, SH).is_empty());
            continue;
        }
        let [cmd] = sdf.chrome.as_slice() else {
            panic!("expected one card border")
        };
        assert_eq!(Rect::new(cmd.x, cmd.y, cmd.width, cmd.height), card);
        assert!(cmd.border.is_some());
        assert_eq!(cmd.radius, radius);
        assert_eq!(handles[0].element_bounds, card);
        if radius == 0.0 {
            let (left, top, right, bottom) = vertices[..6].iter().fold(
                (
                    f32::INFINITY,
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                    f32::NEG_INFINITY,
                ),
                |(left, top, right, bottom), vertex| {
                    let x = (vertex.position[0] + 1.0) * SW * 0.5;
                    let y = (1.0 - vertex.position[1]) * SH * 0.5;
                    (left.min(x), top.min(y), right.max(x), bottom.max(y))
                },
            );
            let expected = [card.x, card.y, card.x + card.width, card.y + card.height];
            for (actual, expected) in [left, top, right, bottom].into_iter().zip(expected) {
                assert!(
                    (actual - expected).abs() < 0.5,
                    "flat pill edge {actual} vs {expected}"
                );
            }
        }
    }
    let handles = compositor.collect_drag_handle_entries(&scene, SW, SH);
    compositor.populate_drag_handle_hit_regions_from(&mut scene, handles);
    assert_eq!(
        scene.overlay.drag_handle_hit_regions[0].element_bounds,
        Some(card)
    );

    // Runtime drag may translate the full-display maximum region left/up.
    // Later growth must move the final pill inward, without reducing capacity
    // to the intersection of that region and the display. Shrinking restores
    // the owner's short-pill position. All draw/hit paths use the same clamp.
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    for (display, dx, dy, region_width) in [
        (Rect::new(0.0, 0.0, SW, SH), -1000.0, -80.0, 1.0),
        (Rect::new(0.0, 0.0, SW, SH), 16.0, 16.0, 1.0),
        (
            Rect::new(-1280.0, -200.0, 1280.0, 720.0),
            -500.0,
            -80.0,
            1.0,
        ),
        (Rect::new(-1280.0, -200.0, 300.0, 32.0), -200.0, -20.0, 2.0),
    ] {
        let secondary = display.x < 0.0;
        let layout = crate::display::DisplayLayout::new(
            if secondary {
                vec![crate::display::DisplayRect {
                    name: "DISPLAY2".into(),
                    rect: display,
                    primary: false,
                }]
            } else {
                Vec::new()
            },
            &HashMap::from([("status-bar".into(), "DISPLAY2".into())]),
        );
        compositor.set_display_layout(layout);
        let mut scene = scene_with_entries(RenderingPolicy::default(), &[]);
        scene
            .zone_registry
            .zones
            .get_mut("status-bar")
            .unwrap()
            .geometry_policy = GeometryPolicy::Relative {
            x_pct: dx / display.width,
            y_pct: dy / display.height,
            width_pct: region_width,
            height_pct: 1.0,
        };
        let [short, long, restored] = [1, 20, 1].map(|count| {
            if count == 1 {
                publish_entries(&mut scene, "agent", [("build", "ok")]);
            } else {
                publish_entries(
                    &mut scene,
                    "agent",
                    (0..count)
                        .map(|i| (format!("session{i:02}"), "working on the current package")),
                );
            }
            compositor.resize_reprime_last_at = None;
            prime_status_text(&mut compositor, &scene);
            compositor.render_frame_headless(&mut scene, &surface);
            let rect = card_for(&compositor, &scene).unwrap().rect;
            let bounds = Rect::new(
                display.x,
                display.y,
                display.width + 0.01,
                display.height + 0.01,
            );
            assert!(rect.is_within(&bounds), "{rect:?} outside {display:?}");
            let sdf = compositor.collect_all_rounded_rect_cmds(&scene, SW, SH);
            let cmd = &sdf.chrome[0];
            assert_eq!(Rect::new(cmd.x, cmd.y, cmd.width, cmd.height), rect);
            for item in compositor.collect_text_items(&scene, SW, SH) {
                assert!(item.pixel_x >= rect.x && item.pixel_y >= rect.y);
                assert!(item.pixel_x + item.bounds_width <= rect.x + rect.width + 0.01);
                assert!(item.clip_pixel_y + item.clip_bounds_height <= rect.y + rect.height + 0.01);
            }
            let handles = compositor.collect_drag_handle_entries(&scene, SW, SH);
            assert_eq!(
                handles.first().map(|h| h.element_bounds),
                (!secondary).then_some(rect)
            );
            let shapes = compositor.status_bar_layouts.borrow().shapes;
            compositor.prime_truncation_cache(&scene);
            assert_eq!(
                compositor.status_bar_layouts.borrow().shapes,
                shapes,
                "unchanged display and placement reuse actual-font measurement on {display:?}"
            );
            rect
        });
        assert_eq!(short, restored, "shrinking restores the owner's placement");
        assert!(long.width > short.width);
        if region_width == 1.0 {
            assert!((long.width - (display.width - 32.0)).abs() < 0.01);
        }
    }
}

/// A value too long for the card renders cut with an ellipsis on its one
/// line, not wrapped onto a second line.
#[tokio::test]
async fn status_bar_long_value_is_ellipsized_not_wrapped() {
    let (mut compositor, surface) =
        require_gpu!(make_compositor_and_surface(SW as u32, SH as u32).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut tokens = compositor.token_map.clone();
    tokens.insert("color.surface.container.low".into(), "#336699".into());
    tokens.insert("opacity.surface".into(), "1.0".into());
    compositor.set_token_map(tokens.clone());
    let mut policy = RenderingPolicy {
        backdrop: None,
        backdrop_opacity: None,
        ..card_policy()
    };
    tze_hud_config::policy_builder::apply_token_defaults_for_zone(
        "status-bar",
        &mut policy,
        &tokens,
    );
    let mut scene = scene_with_entries(
        policy,
        &[(
            "agent",
            "Claude reviewing the status bar redesign pull request right now",
        )],
    );
    let card = card_for(&compositor, &scene).unwrap().rect;
    prime_status_text(&mut compositor, &scene);
    let item = compositor.collect_text_items(&scene, SW, SH).remove(1);
    let key = crate::text::effective_truncation_key(&item).expect("ellipsis item");
    let shown = compositor
        .text_rasterizer
        .as_ref()
        .unwrap()
        .truncation_cache
        .get_by_key(&key)
        .expect("primed truncation")
        .text
        .clone();
    assert!(
        shown.starts_with("Claude") && shown.ends_with(crate::overflow::ELLIPSIS),
        "long value must be cut with an ellipsis; shown {shown:?}"
    );
    let shapes = compositor.status_bar_layouts.borrow().shapes;
    compositor.prime_truncation_cache(&scene);
    assert_eq!(
        compositor.status_bar_layouts.borrow().shapes,
        shapes,
        "unchanged publication does not shape again"
    );
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    // Below the glyphs and inside the hairline, the authored sRGB hex survives policy resolution.
    let (x, y) = ((card.x + 3.0) as u32, (card.y + card.height - 3.0) as u32);
    crate::test_pixels::assert_pixel_color(
        &pixels,
        SW as u32,
        x,
        y,
        [0x33, 0x66, 0x99, 255],
        2,
        "token backdrop",
    )
    .unwrap();
    // Bright glyph pixels only on the single row: none below its line. Start
    // below the drag handle, which straddles the card's top edge.
    let right = (card.x + card.width) as u32;
    let bottom = (card.y + card.height) as u32;
    let bright = |x, y| {
        crate::test_pixels::pixel_at(&pixels, SW as u32, x, y)[..3]
            .iter()
            .all(|&value| value > 200)
    };
    let bright_rows: Vec<u32> = ((card.y + PAD_PX) as u32..bottom)
        .filter(|&y| (card.x as u32..right).any(|x| bright(x, y)))
        .collect();
    assert!(!bright_rows.is_empty(), "the row renders visible text");
    let line_h = 16.0;
    let span = (bright_rows.last().unwrap() - bright_rows.first().unwrap()) as f32;
    assert!(
        span < line_h,
        "text spans {span}px vertically: wrapped onto a second line"
    );
    // The ellipsis keeps the text off the card's right inset.
    let inset = (card.x + card.width - PAD_PX + 1.0) as u32;
    let right_ink = (card.y as u32..bottom).any(|y| (inset..right).any(|x| bright(x, y)));
    assert!(!right_ink, "no glyphs inside the right padding");
    let before_font_change = compositor.status_bar_layouts.borrow().shapes;
    let mut tokens = compositor.token_map.clone();
    tokens.insert("font.sans".into(), "IBM Plex Mono".into());
    compositor.set_token_map(tokens);
    compositor.prime_truncation_cache(&scene);
    assert!(
        compositor.status_bar_layouts.borrow().shapes > before_font_change,
        "changing real font roles invalidates measured status widths"
    );
    compositor.set_max_truncation_input_bytes(64);
    publish_entries(&mut scene, "agent", [("agent", "é\n🚀".repeat(4096))]);
    compositor.prime_truncation_cache(&scene);
    let bounded = compositor.collect_text_items(&scene, SW, SH);
    assert!(
        bounded
            .iter()
            .all(|item| item.text.len() <= 64 + crate::overflow::ELLIPSIS.len())
    );
    assert!(bounded[1].text.ends_with(crate::overflow::ELLIPSIS));
    assert!(
        !bounded[1].text.contains('\n'),
        "status values stay on one line"
    );
}
