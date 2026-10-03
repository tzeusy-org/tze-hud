use super::*;

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
            // Non-default typography so the text path must honour the policy.
            font_size_px: Some(24.0),
            font_weight: Some(700),
            margin_horizontal: Some(8.0),
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

/// Banners stack by severity, critical on top, whatever the arrival order; equal
/// severities stack newest first. Each row lists (text, urgency) in publish order
/// and the expected top-to-bottom order.
#[tokio::test]
async fn test_alert_banner_stacks_by_severity_then_recency() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    type Case = (&'static [(&'static str, u32)], &'static [&'static str]);
    let cases: [Case; 3] = [
        (&[("Warning", 2), ("Critical", 3)], &["Critical", "Warning"]),
        (
            &[("Info", 0), ("Warning", 2), ("Critical", 3)],
            &["Critical", "Warning", "Info"],
        ),
        (
            &[("Warning A", 2), ("Warning B", 2)],
            &["Warning B", "Warning A"],
        ),
    ];
    for (published, expected) in cases {
        let mut scene = make_alert_banner_scene();
        for (i, &(text, urgency)) in published.iter().enumerate() {
            publish_alert(&mut scene, text, urgency, &format!("agent-{i}"));
        }
        let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
        let order: Vec<&str> = items.iter().map(|item| &*item.text).collect();
        assert_eq!(order, expected, "published {published:?}");
        assert!(
            items.windows(2).all(|w| w[0].pixel_y < w[1].pixel_y),
            "slots must descend the screen in order"
        );
        // The zone's heading typography reaches every TextItem: size, weight and
        // the horizontal inset from the full-width zone's left edge (x = 0).
        for item in &items {
            assert_eq!(item.font_size_px, 24.0, "policy font size");
            assert_eq!(item.font_weight, 700, "policy font weight");
            assert_eq!(item.pixel_x, 8.0, "margin_horizontal inset");
        }
    }
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
