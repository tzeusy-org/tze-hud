use super::*;

// ── Status-bar card: one hugged rect shared by every draw path ─────────────

const SW: f32 = 1920.0;
const SH: f32 = 1080.0;
const FONT_PX: f32 = 16.0;
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

fn card_for(scene: &SceneGraph) -> Option<super::zone_render::StatusBarCard> {
    let zone = &scene.zone_registry.zones["status-bar"];
    let publishes = scene
        .zone_registry
        .active_publishes
        .get("status-bar")
        .map(Vec::as_slice)
        .unwrap_or_default();
    Compositor::status_bar_card(
        publishes,
        zone.contention_policy,
        &zone.rendering_policy,
        1.0,
        zone_rect(),
    )
}

/// The card is rows × line height + padding tall, sits on the region's bottom
/// edge, and grows upward one line per entry.
#[test]
fn status_bar_card_hugs_rows_and_is_bottom_anchored() {
    let line_h = FONT_PX * crate::text::LINE_HEIGHT_MULTIPLIER;
    let zone = zone_rect();
    let mut previous_top = f32::MAX;
    for n in 1..=4 {
        let entries: Vec<(String, String)> = (0..n)
            .map(|i| (format!("k{i}"), format!("v{i}")))
            .collect();
        let borrowed: Vec<(&str, &str)> = entries
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let card = card_for(&scene_with_entries(card_policy(), &borrowed)).expect("card");
        let expected_h = n as f32 * line_h + 2.0 * PAD_PX;
        assert!(
            (card.rect.height - expected_h).abs() < 0.01,
            "{n} rows: height {} != {expected_h}",
            card.rect.height
        );
        assert!(
            (card.rect.y + card.rect.height - (zone.y + zone.height)).abs() < 0.01,
            "{n} rows: card must end on the region's bottom edge"
        );
        assert_eq!((card.rect.x, card.rect.width), (zone.x, zone.width));
        assert!(card.rect.y < previous_top, "more rows grow the card upward");
        previous_top = card.rect.y;
        assert_eq!(card.rows.len(), n);
    }
}

/// Rows are in key order (publish order is not carried) and capped to what
/// fits the region; the card never outgrows it.
#[test]
fn status_bar_card_sorts_keys_and_caps_rows_to_the_region() {
    let card = card_for(&scene_with_entries(
        card_policy(),
        &[("wifi", "ok"), ("agent", "Claude"), ("cpu", "4%")],
    ))
    .unwrap();
    let keys: Vec<&str> = card.rows.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["agent", "cpu", "wifi"]);

    let entries: Vec<(String, String)> = (0..40)
        .map(|i| (format!("key{i:02}"), "v".to_owned()))
        .collect();
    let borrowed: Vec<(&str, &str)> = entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let card = card_for(&scene_with_entries(card_policy(), &borrowed)).unwrap();
    let line_h = FONT_PX * crate::text::LINE_HEIGHT_MULTIPLIER;
    let fit = ((zone_rect().height - 2.0 * PAD_PX) / line_h).floor() as usize;
    assert_eq!(card.rows.len(), fit);
    assert!(card.rect.height <= zone_rect().height);
    assert!(card.rect.y >= zone_rect().y);
}

/// No entries: no card, and no backdrop quad, SDF command, text or drag
/// handle anywhere for the zone.
#[tokio::test]
async fn status_bar_without_entries_draws_nothing() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1920, 1080).await);
    let mut policy = card_policy();
    policy.backdrop_radius = Some(8.0);
    let mut scene = scene_with_entries(policy, &[]);
    scene.zone_registry.zones.get_mut("status-bar").unwrap().contention_policy =
        ContentionPolicy::LatestWins;
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
    assert!(card_for(&scene).is_none());

    let mut vertices = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), SW, SH, None);
    assert!(vertices.is_empty(), "no backdrop quad for an empty status bar");
    let sdf = compositor.collect_all_rounded_rect_cmds(&scene, SW, SH);
    assert!(sdf.chrome.is_empty(), "no SDF card for an empty status bar");
    assert!(compositor.collect_text_items(&scene, SW, SH).is_empty());
}

/// The flat backdrop, the SDF card (with its border) and the drag handle's
/// element bounds are all exactly the hugged card rect.
#[tokio::test]
async fn status_bar_draw_paths_share_the_card_rect() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1920, 1080).await);
    let entries = [("agent", "Claude"), ("build", "green")];

    // Flat path (no radius): one quad covering the card.
    let scene = scene_with_entries(card_policy(), &entries);
    let card = card_for(&scene).unwrap().rect;
    let mut vertices = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), SW, SH, None);
    assert_eq!(vertices.len(), 6, "one backdrop quad");
    let to_px = |v: &crate::pipeline::RectVertex| {
        (
            (v.position[0] + 1.0) * 0.5 * SW,
            (1.0 - v.position[1]) * 0.5 * SH,
        )
    };
    let (min_x, min_y, max_x, max_y) = vertices.iter().map(to_px).fold(
        (f32::MAX, f32::MAX, f32::MIN, f32::MIN),
        |(a, b, c, d), (x, y)| (a.min(x), b.min(y), c.max(x), d.max(y)),
    );
    for (got, want) in [
        (min_x, card.x),
        (min_y, card.y),
        (max_x, card.x + card.width),
        (max_y, card.y + card.height),
    ] {
        assert!((got - want).abs() < 0.5, "quad edge {got} != card edge {want}");
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
    let scene = scene_with_entries(rounded, &entries);
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
    assert!(vertices.is_empty(), "rounded cards skip the flat quad");

    // Drag handle: on the card, not the taller region.
    let handles = compositor.collect_drag_handle_entries(&scene, SW, SH);
    let handle = handles
        .iter()
        .find(|h| h.element_kind == DragHandleElementKind::Zone)
        .expect("status-bar handle");
    assert_eq!(handle.element_bounds, card);
}

/// One single-line, ellipsized TextItem per row, each inside its row of the
/// card; the key (with its colon) is dimmed, the value keeps the text color.
#[tokio::test]
async fn status_bar_rows_are_single_line_with_dimmed_keys() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1920, 1080).await);
    let scene = scene_with_entries(
        card_policy(),
        &[
            ("agent", "Claude · reviewing a very long pull request title"),
            ("build", "green"),
        ],
    );
    let card = card_for(&scene).unwrap();
    let items = compositor.collect_text_items(&scene, SW, SH);
    assert_eq!(items.len(), 2, "one TextItem per entry");
    assert_eq!(
        &*items[0].text,
        "agent: Claude · reviewing a very long pull request title"
    );
    assert_eq!(&*items[1].text, "build: green");
    let secondary = crate::text::rgba_to_srgb_u8(super::token_colors::TEXT_SECONDARY_FALLBACK);
    for (i, item) in items.iter().enumerate() {
        assert_eq!(item.overflow, TextOverflow::Ellipsis, "values never wrap");
        assert!(
            (item.bounds_height - card.line_h).abs() < 0.01,
            "exactly one line tall"
        );
        assert!(item.pixel_y >= card.row_y(i) && item.pixel_y < card.row_y(i + 1));
        assert!(item.pixel_x >= card.rect.x + PAD_PX);
        assert!(item.pixel_x + item.bounds_width <= card.rect.x + card.rect.width - PAD_PX + 0.01);
        assert_eq!(item.color[..3], [255, 255, 255], "values in text_color");
        let key_len = item.text.find(' ').unwrap();
        assert_eq!(item.color_runs.len(), 1);
        let run = &item.color_runs[0];
        assert_eq!((run.start_byte, run.end_byte), (0, key_len));
        assert_eq!(run.color[..3], secondary[..3], "keys dimmed");
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
    let card = card_for(&scene).unwrap().rect;
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    let item = compositor.collect_text_items(&scene, SW, SH).remove(0);
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
        shown.starts_with("agent: Claude") && shown.ends_with(crate::overflow::ELLIPSIS),
        "long value must be cut with an ellipsis; shown {shown:?}"
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
    let line_h = FONT_PX * crate::text::LINE_HEIGHT_MULTIPLIER;
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
}
