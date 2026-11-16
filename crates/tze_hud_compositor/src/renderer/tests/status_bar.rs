use super::*;

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
        let entries = entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        scene
            .publish_to_zone(
                "status-bar",
                ZoneContent::StatusBar(StatusBarPayload { entries }),
                "agent",
                None,
                None,
                None,
            )
            .unwrap();
    }
    scene
}

fn zone_rect() -> Rect {
    Rect::new(0.83 * SW, 0.74 * SH, 0.16 * SW, 0.24 * SH)
}

fn card_for(
    compositor: &Compositor,
    scene: &SceneGraph,
) -> Option<crate::renderer::status_bar::StatusBarCard> {
    let zone = &scene.zone_registry.zones["status-bar"];
    let publishes = scene
        .zone_registry
        .active_publishes
        .get("status-bar")
        .map(Vec::as_slice)
        .unwrap_or_default();
    let (x, y, w, h) =
        compositor.zone_geometry("status-bar", &scene.effective_zone_geometry(zone), SW, SH);
    compositor.status_bar_card(
        "status-bar",
        publishes,
        zone.contention_policy,
        &zone.rendering_policy,
        Rect::new(x, y, w, h),
    )
}

/// Entry count changes width, never height; the pill keeps its bottom/right inset.
#[tokio::test]
async fn status_bar_card_hugs_rows_and_is_bottom_anchored() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1920, 1080).await);
    let zone = zone_rect();
    let mut previous_width = 0.0;
    for n in 1..=4 {
        let entries: Vec<(String, String)> =
            (0..n).map(|i| (format!("k{i}"), format!("v{i}"))).collect();
        let borrowed: Vec<(&str, &str)> = entries
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let card =
            card_for(&compositor, &scene_with_entries(card_policy(), &borrowed)).expect("card");
        assert_eq!(card.rect.height, 36.0, "entry count never adds rows");
        assert!((card.rect.y + card.rect.height - (zone.y + zone.height - 16.0)).abs() < 0.01);
        assert!((card.rect.x + card.rect.width - (zone.x + zone.width - 16.0)).abs() < 0.01);
        assert!(
            card.rect.width > previous_width,
            "content grows the pill horizontally"
        );
        previous_width = card.rect.width;
        assert_eq!(card.entries.len(), n);
    }
    for factor in [1.0, 1.5, 1.875] {
        let mut tokens = tze_hud_config::tokens::resolve_tokens(&HashMap::new(), &HashMap::new());
        tze_hud_config::tokens::set_px_scale(&mut tokens, factor);
        let factor = factor as f32;
        tokens.insert("space.status.key_value".into(), "10".into());
        let policy = tze_hud_config::policy_builder::build_effective_policy(
            "status-bar",
            &RenderingPolicy::default(),
            &tokens,
        );
        compositor.set_token_map(tokens);
        let scene = scene_with_entries(policy, &[("scale", "150%")]);
        let card = card_for(&compositor, &scene).unwrap();
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
}

/// Entries keep key order and the horizontal pill never exceeds its maximum region.
#[tokio::test]
async fn status_bar_card_sorts_keys_and_caps_rows_to_the_region() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1920, 1080).await);
    let card = card_for(
        &compositor,
        &scene_with_entries(
            card_policy(),
            &[("wifi", "ok"), ("agent", "Claude"), ("cpu", "4%")],
        ),
    )
    .unwrap();
    let keys: Vec<&str> = card
        .entries
        .iter()
        .map(|entry| entry.key.as_str())
        .collect();
    assert_eq!(keys, ["agent", "cpu", "wifi"]);

    let entries: Vec<(String, String)> = (0..40)
        .map(|i| (format!("key{i:02}"), "v".to_owned()))
        .collect();
    let borrowed: Vec<(&str, &str)> = entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let card = card_for(&compositor, &scene_with_entries(card_policy(), &borrowed)).unwrap();
    assert!(
        card.entries.len() < borrowed.len(),
        "hide sorted suffix that cannot fit"
    );
    assert!(!card.entries.is_empty());
    assert!(card.rect.width <= zone_rect().width - 32.0);
    assert_eq!(card.rect.height, 36.0);
    assert!(
        card.entries
            .windows(2)
            .all(|pair| pair[0].x + pair[0].width < pair[1].x)
    );
}

/// No entries: no card, and no backdrop quad, SDF command, text or drag
/// handle anywhere for the zone.
#[tokio::test]
async fn status_bar_without_entries_draws_nothing() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1920, 1080).await);
    let mut policy = card_policy();
    policy.backdrop_radius = Some(8.0);
    let mut scene = scene_with_entries(policy, &[]);
    scene
        .zone_registry
        .zones
        .get_mut("status-bar")
        .unwrap()
        .contention_policy = ContentionPolicy::LatestWins;
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload {
                entries: std::collections::HashMap::new(),
            }),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    assert!(card_for(&compositor, &scene).is_none());

    let mut vertices = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), SW, SH, None);
    assert!(
        vertices.is_empty(),
        "no backdrop quad for an empty status bar"
    );
    let sdf = compositor.collect_all_rounded_rect_cmds(&scene, SW, SH);
    assert!(sdf.chrome.is_empty(), "no SDF card for an empty status bar");
    assert!(compositor.collect_text_items(&scene, SW, SH).is_empty());
    assert!(
        compositor
            .collect_drag_handle_entries(&scene, SW, SH)
            .is_empty(),
        "no empty status handle"
    );
}

/// The flat backdrop, the SDF card (with its border) and the drag handle's
/// element bounds are all exactly the hugged card rect.
#[tokio::test]
async fn status_bar_draw_paths_share_the_card_rect() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1920, 1080).await);
    let entries = [("agent", "Claude"), ("build", "green")];

    // Flat path (no radius): one quad covering the card.
    let scene = scene_with_entries(card_policy(), &entries);
    let card = card_for(&compositor, &scene).unwrap().rect;
    let mut vertices = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), SW, SH, None);
    assert_eq!(vertices.len(), 12, "backdrop plus one divider quad");
    let to_px = |v: &crate::pipeline::RectVertex| {
        (
            (v.position[0] + 1.0) * 0.5 * SW,
            (1.0 - v.position[1]) * 0.5 * SH,
        )
    };
    let (min_x, min_y, max_x, max_y) = vertices[..6].iter().map(to_px).fold(
        (f32::MAX, f32::MAX, f32::MIN, f32::MIN),
        |(a, b, c, d), (x, y)| (a.min(x), b.min(y), c.max(x), d.max(y)),
    );
    for (got, want) in [
        (min_x, card.x),
        (min_y, card.y),
        (max_x, card.x + card.width),
        (max_y, card.y + card.height),
    ] {
        assert!(
            (got - want).abs() < 0.5,
            "quad edge {got} != card edge {want}"
        );
    }
    // The flat path still draws the card border through the SDF pass.
    let sdf = compositor.collect_all_rounded_rect_cmds(&scene, SW, SH);
    assert_eq!(sdf.chrome.len(), 1);
    let border = &sdf.chrome[0];
    assert!(border.border.is_some(), "the card carries a border");
    assert_eq!(
        (border.x, border.y, border.width, border.height),
        (card.x, card.y, card.width, card.height)
    );

    // Rounded path: the SDF card is the hugged rect, bordered.
    let mut rounded = card_policy();
    rounded.backdrop_radius = Some(8.0);
    let mut scene = scene_with_entries(rounded, &entries);
    let sdf = compositor.collect_all_rounded_rect_cmds(&scene, SW, SH);
    assert_eq!(sdf.chrome.len(), 1);
    let cmd = &sdf.chrome[0];
    assert_eq!(
        (cmd.x, cmd.y, cmd.width, cmd.height),
        (card.x, card.y, card.width, card.height)
    );
    assert!(cmd.border.is_some() && cmd.radius > 0.0);
    let mut vertices = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), SW, SH, None);
    assert_eq!(
        vertices.len(),
        6,
        "rounded card leaves only its divider quad"
    );

    // Drag handle: on the card, not the taller region.
    let handles = compositor.collect_drag_handle_entries(&scene, SW, SH);
    let handle = handles
        .iter()
        .find(|h| h.element_kind == DragHandleElementKind::Zone)
        .expect("status-bar handle");
    assert_eq!(handle.element_bounds, card);
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
        let mut short_rect = None;
        for count in [1, 20, 1] {
            scene
                .publish_to_zone(
                    "status-bar",
                    ZoneContent::StatusBar(StatusBarPayload {
                        entries: if count == 1 {
                            HashMap::from([("build".into(), "ok".into())])
                        } else {
                            (0..count)
                                .map(|i| {
                                    (
                                        format!("session{i:02}"),
                                        "working on the current package".into(),
                                    )
                                })
                                .collect()
                        },
                    }),
                    "agent",
                    None,
                    None,
                    None,
                )
                .unwrap();
            // A settled publication commit, outside the resize cadence.
            compositor.resize_reprime_last_at = None;
            compositor.prime_markdown_cache(&scene);
            compositor.prime_truncation_cache(&scene);
            compositor.render_frame_headless(&mut scene, &surface);
            let card = card_for(&compositor, &scene).unwrap();
            let rect = card.rect;
            assert!(
                rect.x >= display.x && rect.y >= display.y,
                "leading edges {rect:?} / {display:?}"
            );
            assert!(rect.x + rect.width <= display.x + display.width + 0.01);
            assert!(rect.y + rect.height <= display.y + display.height + 0.01);
            if count == 1 {
                if let Some(previous) = short_rect {
                    assert_eq!(
                        rect, previous,
                        "shrink restores short-content owner placement"
                    );
                } else {
                    short_rect = Some(rect);
                }
            } else {
                assert!(rect.width > short_rect.unwrap().width);
                if region_width == 1.0 {
                    assert!(
                        (rect.width - (display.width - 32.0)).abs() < 0.01,
                        "full layout capacity survives drag"
                    );
                }
            }
            let sdf = compositor.collect_all_rounded_rect_cmds(&scene, SW, SH);
            let cmd = &sdf.chrome[0];
            assert_eq!(
                (cmd.x, cmd.y, cmd.width, cmd.height),
                (rect.x, rect.y, rect.width, rect.height)
            );
            for item in compositor.collect_text_items(&scene, SW, SH) {
                assert!(item.pixel_x >= rect.x && item.pixel_y >= rect.y);
                assert!(item.pixel_x + item.bounds_width <= rect.x + rect.width + 0.01);
                assert!(item.clip_pixel_y + item.clip_bounds_height <= rect.y + rect.height + 0.01);
            }
            let handles = compositor.collect_drag_handle_entries(&scene, SW, SH);
            if secondary {
                assert!(
                    handles.is_empty(),
                    "secondary-display zones are not draggable"
                );
            } else {
                assert_eq!(handles[0].element_bounds, rect);
            }
            let shapes = compositor.status_bar_layouts.borrow().shapes;
            compositor.prime_truncation_cache(&scene);
            assert_eq!(
                compositor.status_bar_layouts.borrow().shapes,
                shapes,
                "unchanged display/layout reuses actual-font measurement"
            );
        }
    }
}

/// Separate key/value boxes share one baseline, token gap and pill bounds.
#[tokio::test]
async fn status_bar_rows_are_single_line_with_dimmed_keys() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1920, 1080).await);
    let scene = scene_with_entries(
        card_policy(),
        &[("agent", "Claude reviewing"), ("build", "green")],
    );
    let card = card_for(&compositor, &scene).unwrap();
    let items = compositor.collect_text_items(&scene, SW, SH);
    assert_eq!(
        items.len(),
        4,
        "separate key and value boxes preserve the token gap"
    );
    assert_eq!(&*items[0].text, "agent");
    assert_eq!(&*items[1].text, "Claude reviewing");
    assert_eq!(&*items[2].text, "build");
    assert_eq!(&*items[3].text, "green");
    for item in &items {
        assert_eq!(item.overflow, TextOverflow::Ellipsis);
        assert_eq!(item.bounds_height, 16.0);
        assert_eq!(item.pixel_y, card.rect.y + 10.0);
        assert!(item.pixel_x >= card.rect.x + PAD_PX);
        assert!(item.pixel_x + item.bounds_width <= card.rect.x + card.rect.width - PAD_PX + 0.01);
    }
    assert_eq!(items[1].color[..3], [255, 255, 255]);
    assert_ne!(
        items[0].color[..3],
        items[1].color[..3],
        "keys are dimmer than values"
    );
    for pair in items.chunks_exact(2) {
        assert!((pair[1].pixel_x - pair[0].pixel_x - pair[0].bounds_width - 6.0).abs() < 0.01);
    }
}

/// A value too long for the card renders cut with an ellipsis on its one
/// line, not wrapped onto a second line.
#[tokio::test]
async fn status_bar_long_value_is_ellipsized_not_wrapped() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1920, 1080).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = scene_with_entries(
        card_policy(),
        &[(
            "agent",
            "Claude reviewing the status bar redesign pull request right now",
        )],
    );
    let card = card_for(&compositor, &scene).unwrap().rect;
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
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
    // Bright glyph pixels only on the single row: none below its line. Start
    // below the drag handle, which straddles the card's top edge.
    let bright_rows: Vec<u32> = ((card.y + PAD_PX) as u32..(card.y + card.height) as u32)
        .filter(|&y| {
            (card.x as u32..(card.x + card.width) as u32).any(|x| {
                let o = ((y * 1920 + x) * 4) as usize;
                pixels[o] > 200 && pixels[o + 1] > 200 && pixels[o + 2] > 200
            })
        })
        .collect();
    assert!(!bright_rows.is_empty(), "the row renders visible text");
    let line_h = 16.0;
    let span = (bright_rows.last().unwrap() - bright_rows.first().unwrap()) as f32;
    assert!(
        span < line_h,
        "text spans {span}px vertically: wrapped onto a second line"
    );
    // The ellipsis keeps the text off the card's right inset.
    let right_ink = (card.y as u32..(card.y + card.height) as u32).any(|y| {
        ((card.x + card.width - PAD_PX + 1.0) as u32..(card.x + card.width) as u32).any(|x| {
            let o = ((y * 1920 + x) * 4) as usize;
            pixels[o] > 200 && pixels[o + 1] > 200 && pixels[o + 2] > 200
        })
    });
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
    scene
        .publish_to_zone(
            "status-bar",
            ZoneContent::StatusBar(StatusBarPayload {
                entries: HashMap::from([("agent".into(), "é\n🚀".repeat(4096))]),
            }),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
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

/// a RenderingPolicy colour taken from a design token draws as the
/// hex the token declares, the same as the compositor's own token fallback.
#[tokio::test]
async fn status_bar_token_policy_backdrop_draws_its_declared_hex() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1920, 1080).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut tokens = compositor.token_map.clone();
    tokens.insert("color.surface.container.low".into(), "#336699".into());
    tokens.insert("opacity.surface".into(), "1.0".into());
    compositor.set_token_map(tokens.clone());
    let mut policy = RenderingPolicy::default();
    tze_hud_config::policy_builder::apply_token_defaults_for_zone(
        "status-bar",
        &mut policy,
        &tokens,
    );
    policy.backdrop_radius = Some(0.0);
    let mut scene = scene_with_entries(policy, &[("k", "v")]);
    let card = card_for(&compositor, &scene).unwrap().rect;
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);
    let pixels = surface.read_pixels(&compositor.device);
    // Bottom-left inside the card: below the row's glyphs, inside the hairline.
    let (x, y) = ((card.x + 3.0) as u32, (card.y + card.height - 3.0) as u32);
    let o = ((y * 1920 + x) * 4) as usize;
    let got = [pixels[o], pixels[o + 1], pixels[o + 2]];
    for (channel, (g, want)) in got.iter().zip([0x33u8, 0x66, 0x99]).enumerate() {
        assert!(
            g.abs_diff(want) <= 2,
            "channel {channel}: drew {got:02x?}, token declares #336699"
        );
    }
}
