use super::*;

// ── Alert-banner heading typography tests [hud-w3o6.2] ───────────────────
//
// Acceptance criteria from spec §Alert-Banner Heading Typography:
//   1. font_size_px = 24px
//   2. font_weight = 700 (bold)
//   3. font_family = SystemSansSerif
//   4. text_color = #FFFFFF white
//   5. margin_horizontal inset applied

/// Alert-banner RenderingPolicy carries heading typography:
/// 24px font, weight 700, SystemSansSerif, white text, margin_horizontal=8.
///
/// Acceptance criterion 3.1–3.3: heading typography wired to alert-banner zone.
#[tokio::test]
async fn test_alert_banner_heading_typography_in_rendering_policy() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Register alert-banner with heading-typography RenderingPolicy (spec values).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "heading typography test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.06,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(24.0),
            font_family: Some(FontFamily::SystemSansSerif),
            font_weight: Some(700),
            text_color: Some(Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            }),
            backdrop: Some(Rgba::new(0.1, 0.1, 0.16, 0.9)),
            backdrop_opacity: Some(0.9),
            margin_horizontal: Some(8.0),
            margin_vertical: Some(0.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 16,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish a notification payload.
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Weather alert: severe storms".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // collect_text_items uses the RenderingPolicy fields for TextItem construction.
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        items.len(),
        1,
        "expected one TextItem for alert-banner notification"
    );

    let item = &items[0];

    // AC 3.1: font_size_px = 24.0
    assert_eq!(
        item.font_size_px, 24.0,
        "alert-banner text must be 24px per spec §Alert-Banner Heading Typography"
    );

    // AC 3.1: font_family = SystemSansSerif
    assert_eq!(
        item.font_family,
        FontFamily::SystemSansSerif,
        "alert-banner text must use system sans-serif family"
    );

    // AC 3.1: font_weight = 700
    assert_eq!(
        item.font_weight, 700,
        "alert-banner text must be weight 700 (bold)"
    );

    // AC 3.2: text_color = #FFFFFF (white)
    // White in linear sRGB: R=1.0 → 255u8, G=1.0 → 255u8, B=1.0 → 255u8.
    assert_eq!(item.color[0], 255, "text R should be 255 (white)");
    assert_eq!(item.color[1], 255, "text G should be 255 (white)");
    assert_eq!(item.color[2], 255, "text B should be 255 (white)");

    // AC 3.3: text is inset from x=0 by margin_horizontal=8
    // Zone geometry: zx = (sw - sw*1.0)/2 = 0.0, so pixel_x = 0 + 8 = 8.
    assert_eq!(
        item.pixel_x, 8.0,
        "text must be inset by margin_horizontal=8 from backdrop edge"
    );
}

/// Alert-banner zone has LayerAttachment::Chrome — renders above all agent content.
///
/// Acceptance criterion: chrome-layer z-order verified by checking ZoneDefinition.
#[test]
fn test_alert_banner_default_zone_has_chrome_layer_attachment() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("alert-banner")
        .expect("alert-banner must be in default zone registry");

    assert_eq!(
        zone.layer_attachment,
        LayerAttachment::Chrome,
        "alert-banner zone must be attached to chrome layer (above all agent content)"
    );
}

/// Alert-banner zone spans full display width (width_pct = 1.0).
///
/// Acceptance criterion: backdrop quad spans from x=0 to x=display_width.
#[test]
fn test_alert_banner_default_zone_is_full_width() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("alert-banner")
        .expect("alert-banner must be in default zone registry");

    match zone.geometry_policy {
        GeometryPolicy::EdgeAnchored { width_pct, .. } => {
            assert_eq!(
                width_pct, 1.0,
                "alert-banner must span full display width (width_pct=1.0)"
            );
        }
        _ => panic!("alert-banner must use EdgeAnchored geometry for full-width positioning"),
    }
}

/// Alert-banner zone resolve_zone_geometry gives backdrop width = display width.
///
/// At 1920×1080, the backdrop must span from x=0 to x=1920.
#[test]
fn test_alert_banner_backdrop_spans_full_display_width() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("alert-banner")
        .expect("alert-banner zone must exist");

    let (x, _y, w, _h) = Compositor::resolve_zone_geometry(&zone.geometry_policy, 1920.0, 1080.0);
    assert_eq!(x, 0.0, "alert-banner left edge must be at x=0");
    assert_eq!(
        w, 1920.0,
        "alert-banner width must equal display width (1920)"
    );
}

/// Alert-banner zone height accommodates 24px heading + vertical padding.
///
/// At 720p, height_pct=0.06 → 43.2px > 24px + 2×8px = 40px minimum.
#[test]
fn test_alert_banner_zone_height_accommodates_heading_typography() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("alert-banner")
        .expect("alert-banner zone must exist");

    // Check that resolved height at 720p is sufficient for 24px heading.
    // margin_vertical=0.0 (flush to edge), so minimum is font_size_px only.
    // height_pct=0.06 → 0.06×720=43.2px, well above the 24px minimum.
    let (_x, _y, _w, h) = Compositor::resolve_zone_geometry(&zone.geometry_policy, 1280.0, 720.0);
    let font_size_px = zone.rendering_policy.font_size_px.unwrap_or(24.0);
    let min_required = font_size_px; // margin_vertical=0.0; height must cover font at minimum
    assert!(
        h >= min_required,
        "alert-banner height {h}px must accommodate heading ({font_size_px}px)"
    );
}

/// When no alert-banner publications are active, render_zone_content emits zero
/// backdrop vertices — the zone occupies zero visible space.
///
/// Acceptance criterion §Alert-Banner Chrome-Layer Positioning:
///   "When no alerts are active, the alert-banner zone MUST occupy zero vertical space."
#[tokio::test]
async fn test_alert_banner_zero_height_when_inactive() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Register alert-banner zone with a visible backdrop so it would render if active.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "zero-height-when-inactive test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.06,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(24.0),
            backdrop: Some(Rgba::new(1.0, 0.0, 0.0, 1.0)), // bright red — visible if leaked
            backdrop_opacity: Some(0.9),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Replace,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // No publications — zone is inactive.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // Zero vertices emitted → zone occupies zero visible space.
    assert!(
        vertices.is_empty(),
        "no backdrop quad must be emitted for inactive alert-banner zone (zero visible space)"
    );

    // Also verify no TextItems produced.
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert!(
        items.is_empty(),
        "no text must be rendered for inactive alert-banner zone"
    );
}

/// Alert-banner RenderingPolicy in ZoneRegistry::with_defaults() carries
/// heading typography: 24px, weight 700, white text, margin_horizontal=8.
#[test]
fn test_alert_banner_default_zone_rendering_policy_has_heading_typography() {
    use tze_hud_scene::types::ZoneRegistry;

    let registry = ZoneRegistry::with_defaults();
    let zone = registry
        .get_by_name("alert-banner")
        .expect("alert-banner must be in default zone registry");

    let policy = &zone.rendering_policy;

    assert_eq!(
        policy.font_size_px,
        Some(24.0),
        "alert-banner default rendering policy must have font_size_px=24"
    );
    assert_eq!(
        policy.font_weight,
        Some(700),
        "alert-banner default rendering policy must have font_weight=700 (bold)"
    );
    assert_eq!(
        policy.font_family,
        Some(FontFamily::SystemSansSerif),
        "alert-banner default rendering policy must use SystemSansSerif"
    );
    // text_color must be white (R=1.0, G=1.0, B=1.0).
    let tc = policy
        .text_color
        .expect("alert-banner default rendering policy must have text_color set");
    assert!(
        (tc.r - 1.0).abs() < 0.01,
        "text_color R must be 1.0 (white), got {}",
        tc.r
    );
    assert!(
        (tc.g - 1.0).abs() < 0.01,
        "text_color G must be 1.0 (white), got {}",
        tc.g
    );
    assert!(
        (tc.b - 1.0).abs() < 0.01,
        "text_color B must be 1.0 (white), got {}",
        tc.b
    );
    assert_eq!(
        policy.margin_horizontal,
        Some(8.0),
        "alert-banner default rendering policy must have margin_horizontal=8"
    );
}

// ─── Alert-banner severity-stack tests ───────────────────────────────────

/// Helper: build a SceneGraph with an alert-banner Stack zone for severity tests.
///
/// Zone: full-width, EdgeAnchored top, height_pct=0.05 (36px at 720p).
/// font_size_px=16, default margin_v=8 → slot_h = 16 + 2×8 + 2 = 34px.
/// max_depth=8, max_publishers=16.
fn make_alert_banner_scene() -> SceneGraph {
    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert-banner severity-stack test zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.05,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            backdrop_opacity: Some(1.0),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 16,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
}

/// Helper: publish an alert banner notification.
fn publish_alert(scene: &mut SceneGraph, text: &str, urgency: u32, publisher: &str) {
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: text.to_owned(),
                icon: String::new(),
                urgency,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            publisher,
            None,
            None,
            None,
        )
        .expect("alert-banner publish must succeed");
}

/// Critical (urgency=3) banner must appear above warning (urgency=2).
///
/// With two banners, slot 0 (top) must be the critical one regardless of
/// publication order (warning published before critical).
#[tokio::test]
async fn test_alert_banner_critical_above_warning() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = make_alert_banner_scene();

    // Publish warning first, then critical.
    publish_alert(&mut scene, "Warning: disk space low", 2, "agent-a");
    publish_alert(&mut scene, "Critical: system failure", 3, "agent-b");

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 2, "two banners → two TextItems");

    // Slot 0 (pixel_y=0) must be the critical banner (urgency 3).
    // Slot 1 (pixel_y=slot_h) must be the warning banner (urgency 2).
    // The critical banner is at a lower pixel_y value (top of the zone).
    let y_first = items[0].pixel_y;
    let y_second = items[1].pixel_y;
    assert!(
        y_first < y_second,
        "slot 0 (critical) must be above slot 1 (warning): y0={y_first} y1={y_second}"
    );
    assert!(
        items[0].text.contains("Critical"),
        "slot 0 must be the critical banner; got: {}",
        items[0].text
    );
    assert!(
        items[1].text.contains("Warning"),
        "slot 1 must be the warning banner; got: {}",
        items[1].text
    );
}

/// Warning (urgency=2) banner must appear above info (urgency=0-1).
///
/// Info published before warning — severity sort must override arrival order.
#[tokio::test]
async fn test_alert_banner_warning_above_info() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = make_alert_banner_scene();

    // Publish info first, then warning.
    publish_alert(&mut scene, "Info: update available", 1, "agent-a");
    publish_alert(&mut scene, "Warning: memory pressure", 2, "agent-b");

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 2, "two banners → two TextItems");

    assert!(
        items[0].text.contains("Warning"),
        "slot 0 must be the warning banner; got: {}",
        items[0].text
    );
    assert!(
        items[1].text.contains("Info"),
        "slot 1 must be the info banner; got: {}",
        items[1].text
    );
    assert!(
        items[0].pixel_y < items[1].pixel_y,
        "warning slot must be above info slot"
    );
}

/// Three-level severity stack: critical → warning → info (top to bottom).
///
/// Published in reverse order (info, warning, critical) to confirm severity
/// sort overrides arrival order.
#[tokio::test]
async fn test_alert_banner_three_level_severity_stack() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = make_alert_banner_scene();

    // Publish info first, then warning, then critical.
    publish_alert(&mut scene, "Info: routine scan complete", 0, "agent-a");
    publish_alert(&mut scene, "Warning: high load", 2, "agent-b");
    publish_alert(&mut scene, "Critical: disk full", 3, "agent-c");

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 3, "three banners → three TextItems");

    // Verify order: critical (slot 0), warning (slot 1), info (slot 2).
    assert!(
        items[0].text.contains("Critical"),
        "slot 0 must be critical; got: {}",
        items[0].text
    );
    assert!(
        items[1].text.contains("Warning"),
        "slot 1 must be warning; got: {}",
        items[1].text
    );
    assert!(
        items[2].text.contains("Info"),
        "slot 2 must be info; got: {}",
        items[2].text
    );
    // Pixel positions must decrease (slot 0 < slot 1 < slot 2 in pixel_y).
    assert!(
        items[0].pixel_y < items[1].pixel_y,
        "critical above warning"
    );
    assert!(items[1].pixel_y < items[2].pixel_y, "warning above info");
}

/// Same-severity banners: the newer one must appear above the older one.
///
/// Two warnings published in order (A first, B second).  Slot 0 must be
/// the newer one ("Warning B").
///
/// The sort is deterministic even when timestamps are equal: a tertiary
/// `index descending` key in `sort_alert_banner_indices` ensures the later
/// insert (higher index) always wins on exact timestamp ties.
#[tokio::test]
async fn test_alert_banner_same_severity_recency_order() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = make_alert_banner_scene();

    // Publish two warnings in order.  Even if both arrive in the same µs,
    // the tertiary index key ensures B (higher index) sorts above A.
    publish_alert(&mut scene, "Warning A (older)", 2, "agent-a");
    publish_alert(&mut scene, "Warning B (newer)", 2, "agent-b");

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 2, "two warnings → two TextItems");

    // Newer publish must be slot 0 (top).
    assert!(
        items[0].text.contains("Warning B"),
        "slot 0 must be the newer warning (B); got: {}",
        items[0].text
    );
    assert!(
        items[1].text.contains("Warning A"),
        "slot 1 must be the older warning (A); got: {}",
        items[1].text
    );
}

/// Alert-banner zone height grows dynamically with active banner count.
///
/// Test helper zone: height_pct=0.05, so static zone height at 720p = 36px.
/// slot_h = font_size_px(16) + 2 × margin_v(8) + SLOT_BASELINE_GAP(2) = 34px.
///
/// - 0 banners → 0 vertices (zero height, nothing rendered).
/// - 1 banner  → 1 backdrop quad (6 vertices), slot at y=0..34px.
/// - 3 banners → 3 backdrop quads (18 vertices), 3rd slot at y=68..102px —
///   this exceeds the 36px static height, proving dynamic expansion.
#[tokio::test]
async fn test_alert_banner_zone_height_grows_with_active_count() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    // ── 0 banners: no vertices emitted ──────────────────────────────────
    {
        let scene = make_alert_banner_scene();
        let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
        compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
        assert!(
            vertices.is_empty(),
            "0 banners → 0 vertices (zero height); got {} vertices",
            vertices.len()
        );
    }

    // ── 1 banner: one backdrop quad (6 vertices) ────────────────────────
    {
        let mut scene = make_alert_banner_scene();
        publish_alert(&mut scene, "Single banner", 2, "agent-a");
        let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
        compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
        assert_eq!(
            vertices.len(),
            6,
            "1 banner → 1 backdrop quad (6 vertices); got {}",
            vertices.len()
        );
    }

    // ── 3 banners: three backdrop quads (18 vertices) ───────────────────
    //
    // The static zone height is 36px (height_pct=0.05 × 720p).  Each slot
    // is 34px.  Under fixed-height logic the 2nd slot (y=34) would be
    // clipped at 36px (~2px visible) and the 3rd (y=68) would be invisible.
    // Dynamic height = 3 × 34 = 102px allows all three to render fully.
    {
        let mut scene = make_alert_banner_scene();
        publish_alert(&mut scene, "Banner A", 1, "agent-a");
        publish_alert(&mut scene, "Banner B", 2, "agent-b");
        publish_alert(&mut scene, "Banner C", 3, "agent-c");
        let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
        compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
        assert_eq!(
            vertices.len(),
            18,
            "3 banners → 3 backdrop quads (18 vertices); got {} — \
                 dynamic height must expand beyond static zone height",
            vertices.len()
        );
        // Verify that all 3 quads are at distinct y positions (slot 0 ≠ slot 2).
        // Vertex layout from rect_vertices: vertex 0 is top-left [left, top] in NDC.
        // Slot 0 starts at pixel y=0 → NDC y_top=1.0.
        // Slot 2 starts at pixel y≈68px → NDC y_top≈0.811 (strictly less than 1.0).
        let slot0_ndc_y = vertices[0].position[1];
        let slot2_ndc_y = vertices[12].position[1];
        assert!(
            slot2_ndc_y < slot0_ndc_y,
            "3rd slot must be below 1st slot in NDC y; slot0={slot0_ndc_y}, slot2={slot2_ndc_y}"
        );
    }
}

/// render_zone_content for alert-banner uses severity-ordered backdropcolors.
///
/// With critical (urgency=3) and warning (urgency=2), the first backdrop quad
/// (slot 0, top) must be red (critical color), and the second must be amber
/// (warning color).
#[tokio::test]
async fn test_alert_banner_backdrop_colors_ordered_by_severity() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = make_alert_banner_scene();

    // Publish warning first, then critical (to confirm severity overrides arrival).
    publish_alert(&mut scene, "Warning", 2, "agent-a");
    publish_alert(&mut scene, "Critical", 3, "agent-b");

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // 2 backdrop quads → 12 vertices.
    assert_eq!(vertices.len(), 12, "2 banners → 12 vertices");

    // Slot 0 (vertices 0-5) must be critical red: R > 0.9, G < 0.1, B < 0.1.
    let slot0_color = vertices[0].color;
    assert!(
        slot0_color[0] > 0.9,
        "slot 0 backdrop R should be ~1.0 (critical red); got {}",
        slot0_color[0]
    );
    assert!(
        slot0_color[1] < 0.1,
        "slot 0 backdrop G should be ~0.0 (critical red); got {}",
        slot0_color[1]
    );

    // Slot 1 (vertices 6-11) must be warning amber: R > 0.9, G mid, B < 0.1.
    let slot1_color = vertices[6].color;
    assert!(
        slot1_color[0] > 0.9,
        "slot 1 backdrop R should be ~1.0 (warning amber); got {}",
        slot1_color[0]
    );
    assert!(
        slot1_color[2] < 0.1,
        "slot 1 backdrop B should be ~0.0 (warning amber); got {}",
        slot1_color[2]
    );
    // Amber has non-trivial G (0.5–0.9), while critical has G < 0.1.
    assert!(
        slot1_color[1] > 0.5,
        "slot 1 backdrop G should be mid (warning amber ≈ 0.72); got {}",
        slot1_color[1]
    );
}
