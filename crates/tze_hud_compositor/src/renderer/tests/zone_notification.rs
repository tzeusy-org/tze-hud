use super::*;

// ── RenderingPolicy-driven zone rendering tests [hud-sc0a.8] ─────────────

/// Subtitle with outline: when outline_color + outline_width are set in
/// RenderingPolicy, collect_text_items produces a TextItem with non-None
/// outline fields.
#[tokio::test]
async fn test_zone_subtitle_with_outline_text_item() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "subtitle zone with outline".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(22.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            outline_color: Some(Rgba::BLACK),
            outline_width: Some(2.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Test outline text".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 1, "expected one TextItem");
    let item = &items[0];
    assert!(
        item.outline_color.is_some(),
        "outline_color should be set from RenderingPolicy"
    );
    assert!(
        item.outline_width.is_some(),
        "outline_width should be set from RenderingPolicy"
    );
    assert_eq!(
        item.outline_width.unwrap(),
        2.0,
        "outline_width should match policy"
    );
    // Text color should be white (from text_color).
    assert_eq!(
        item.color[0], 255,
        "text fill color R should be white (255)"
    );
}

/// Subtitle without outline: when outline_width is None, outline fields
/// on the TextItem should be None.
#[tokio::test]
async fn test_zone_subtitle_without_outline_text_item() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "subtitle zone without outline".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(22.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.7)),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            outline_color: None,
            outline_width: None,
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("No outline subtitle".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 1, "expected one TextItem");
    let item = &items[0];
    assert!(
        item.outline_color.is_none(),
        "outline_color should be None when policy has no outline"
    );
    assert!(
        item.outline_width.is_none(),
        "outline_width should be None when policy has no outline"
    );
}

/// Render-branch regression guard (hud-9v3t6): a portal carrying a *zero-length
/// lifecycle sentinel* color run must still take the cached/styled markdown
/// render path, NOT the lossy `from_text_markdown_node` / `strip_markdown_v1`
/// path.
///
/// Background: `lifecycle_marker_color_runs` (resident_grpc.rs) emits a
/// zero-length `TextColorRun` ([start..start], no pixel coverage) on every
/// permitted viewer of a normal active/attached portal.  The render branch used
/// to gate on `color_runs.is_empty()`, so this sentinel flipped *every* normal
/// portal onto the lossy/uncached path (losing markdown styling AND the
/// commit-time markdown cache) while painting no accent pixels.  The fix gates
/// on `markdown_node_has_pixel_runs` instead.
///
/// This asserts at the COMPOSITOR RENDER-BRANCH level (the existing tests only
/// covered node construction, which is why the regression slipped through):
/// `collect_text_items` must produce a `TextItem` with populated `styled_runs`
/// (the cached path's signature; the lossy node path always leaves
/// `styled_runs` empty).
#[tokio::test]
async fn test_lifecycle_sentinel_keeps_cached_markdown_render_path() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Markdown content that yields styled spans (heading + bold) — the cached
    // path emits StyledRunItems for these; the lossy strip path emits none.
    let content = "# Portal\n**attached** and ready".to_owned();

    let make_node = |runs: Box<[tze_hud_scene::types::TextColorRun]>| Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: content.clone(),
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: runs,
        }),
    };

    // Zero-length lifecycle sentinel: start_byte == end_byte (no pixel coverage),
    // exactly as lifecycle_marker_color_runs emits for a normal active portal.
    let sentinel = tze_hud_scene::types::TextColorRun {
        start_byte: 0,
        end_byte: 0,
        color: Rgba::new(0.2, 0.8, 0.4, 1.0),
    };
    let scene = scene_with_node(make_node(Box::from([sentinel])));
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);

    let items = compositor.collect_text_items(&scene, 256.0, 256.0);
    assert_eq!(
        items.len(),
        1,
        "expected exactly one TextItem for the portal node"
    );
    let item = &items[0];
    assert!(
        !item.styled_runs.is_empty(),
        "portal with a zero-length lifecycle sentinel must take the cached/styled \
         markdown path (styled_runs populated), not the lossy strip path"
    );
    assert!(
        item.color_runs.is_empty(),
        "the zero-length sentinel carries no pixel coverage and must be dropped \
         (no ColorRunItems) on the cached path"
    );

    // Control: a genuine *pixel-bearing* color run (start < end) must still force
    // the legacy from_text_markdown_node path (styled_runs empty), proving the
    // fix narrowed the gate to pixel runs without disabling the legacy path.
    let pixel_run = tze_hud_scene::types::TextColorRun {
        start_byte: 0,
        end_byte: 4,
        color: Rgba::new(0.9, 0.1, 0.1, 1.0),
    };
    let scene_pixel = scene_with_node(make_node(Box::from([pixel_run])));
    compositor.prime_markdown_cache(&scene_pixel);
    compositor.prime_truncation_cache(&scene_pixel);
    let pixel_items = compositor.collect_text_items(&scene_pixel, 256.0, 256.0);
    assert_eq!(
        pixel_items.len(),
        1,
        "expected one TextItem for the pixel-run node"
    );
    assert!(
        pixel_items[0].styled_runs.is_empty(),
        "a pixel-bearing color run must still take the legacy raw-content path \
         (styled_runs empty), preserving its raw byte offsets"
    );
    assert!(
        !pixel_items[0].color_runs.is_empty(),
        "the pixel-bearing color run must be preserved as a ColorRunItem"
    );
}

/// Notification action buttons are drawn at exactly the bounds the pointer
/// hit regions register, in token-styled fill, with their labels. Two
/// notifications (the older one titled, so slot heights differ) exercise the
/// shared slot layout. Draw-command level (no pixel readback).
#[tokio::test]
async fn notification_actions_layout_matches_hit_regions() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor
        .token_map
        .insert("notification.action.background".into(), "#336699".into());

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.create_tab("Main", 0).unwrap();
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.6,
            y_pct: 0.0,
            width_pct: 0.35,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(16.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    let action = |label: &str| tze_hud_scene::types::NotificationAction {
        label: label.to_owned(),
        callback_id: label.to_lowercase(),
    };
    for (ns, title, labels) in [
        ("a", "Deploy", vec!["Ship", "Hold"]),
        ("b", "", vec!["Open"]),
    ] {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: "body".to_owned(),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: title.to_owned(),
                    actions: labels.into_iter().map(action).collect(),
                }),
                ns,
                None,
                None,
                None,
            )
            .unwrap();
    }

    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    let fill = compositor.gpu_color(
        crate::renderer::token_colors::resolve_notification_action_tokens(&compositor.token_map)
            .background,
    );
    let mut checked = 0;
    for region in &scene.overlay.zone_hit_regions {
        let tze_hud_scene::types::ZoneInteractionKind::Action { callback_id } = &region.kind else {
            continue;
        };
        let b = region.bounds;
        let quad = crate::pipeline::rect_vertices(b.x, b.y, b.width, b.height, 1280.0, 720.0, fill);
        assert!(
            vertices.windows(6).any(|w| w
                .iter()
                .zip(&quad)
                .all(|(a, b)| a.position == b.position && a.color == b.color)),
            "no token-filled quad drawn at hit region of {callback_id}: {b:?}"
        );
        let label = callback_id.to_uppercase()[..1].to_string() + &callback_id[1..];
        let item = items
            .iter()
            .find(|i| &*i.text == label.as_str())
            .unwrap_or_else(|| panic!("no label text item for {callback_id}"));
        assert_eq!((item.pixel_x, item.bounds_width), (b.x, b.width));
        assert!(
            item.pixel_y >= b.y && item.pixel_y + item.font_size_px <= b.y + b.height,
            "label for {callback_id} must sit inside its button"
        );
        checked += 1;
    }
    assert_eq!(checked, 3, "all three actions must have hit regions");
}

/// Notification with opaque backdrop: backdrop_opacity=0.9 overrides
/// the backdrop color's alpha.  The backdrop quad should be rendered with
/// effective alpha = 0.9.
#[tokio::test]
async fn test_notification_with_opaque_backdrop() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.08,
            width_pct: 0.70,
            margin_px: 12.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(18.0),
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
            backdrop_opacity: Some(0.9),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        auto_clear_ms: Some(5_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Notification with opaque backdrop".to_owned(),
                icon: String::new(),
                urgency: 1,
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

    // render_zone_content should produce backdrop rect vertices.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    // We check that vertices were emitted (backdrop rendered).
    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for notification with opaque backdrop"
    );

    // Also verify the text items use the policy text_color.
    // collect_text_items emits 2 items for a notification slot when the text
    // rasterizer is active: the notification body text + the dismiss "X" button.
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        items.len(),
        2,
        "expected two TextItems: notification body + dismiss button"
    );
    // The first item is the notification body text. White text → R channel near 255.
    assert!(
        items[0].color[0] > 200,
        "text color R should be near-white from policy.text_color"
    );
}

/// Alert-banner severity mapping: urgency 2 (warning) should map to
/// color.severity.warning (amber/yellow), NOT the policy backdrop.
/// We verify by inspecting the vertices emitted by render_zone_content.
#[tokio::test]
async fn test_alert_banner_urgency2_maps_to_severity_warning() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert banner zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.07,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(20.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)), // dark default
            backdrop_opacity: Some(1.0),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish urgency=2 (warning).
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Warning: disk space low".to_owned(),
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

    // Collect vertices from render_zone_content.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // The backdrop should be severity warning color (~amber: R=1.0, G~0.72, B=0.0).
    // rect_vertices emits 6 vertices; each has color at the end.
    // We check that the R component is high (>0.9) and G is mid (~0.5-0.8) and B is low.
    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for alert-banner urgency=2"
    );

    // Verify urgency_to_severity_color directly (no token map → fallback constants).
    let no_tokens = HashMap::new();
    let warning_color = urgency_to_severity_color(2, &no_tokens);
    assert!(
        warning_color.r > 0.9,
        "warning severity R should be ~1.0 (amber)"
    );
    assert!(
        warning_color.g > 0.5,
        "warning severity G should be >0.5 (amber)"
    );
    assert!(
        warning_color.b < 0.1,
        "warning severity B should be ~0.0 (amber)"
    );
}

/// Alert-banner urgency=3 maps to critical (red).
#[tokio::test]
async fn test_alert_banner_urgency3_maps_to_severity_critical() {
    let no_tokens = HashMap::new();
    let critical = urgency_to_severity_color(3, &no_tokens);
    assert!(critical.r > 0.9, "critical R should be ~1.0");
    assert!(critical.g < 0.1, "critical G should be ~0.0");
    assert!(critical.b < 0.1, "critical B should be ~0.0");
}

/// Alert-banner urgency=0 and 1 both map to info (blue).
#[tokio::test]
async fn test_alert_banner_urgency_low_maps_to_info() {
    let no_tokens = HashMap::new();
    let info0 = urgency_to_severity_color(0, &no_tokens);
    let info1 = urgency_to_severity_color(1, &no_tokens);
    // Info color is blue-ish (#4A9EFF).
    assert!(info0.b > 0.9, "info urgency=0 should be blue");
    assert!(info1.b > 0.9, "info urgency=1 should be blue");
    // Both should be the same color.
    assert_eq!(info0.r, info1.r);
    assert_eq!(info0.b, info1.b);
}

/// notification-area does NOT use urgency-to-severity mapping (color.severity.*).
/// It uses color.notification.urgency.* tokens instead.
/// Even with urgency=3, it must NOT produce severity critical (red #FF0000).
#[tokio::test]
async fn test_notification_area_does_not_use_severity_tokens() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area - uses notification urgency tokens, not severity"
            .to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.05, 0.05, 0.05, 0.85)),
            backdrop_opacity: Some(0.85),
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
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "System alert".to_owned(),
                icon: String::new(),
                urgency: 3, // Critical — must use color.notification.urgency.critical, NOT color.severity.critical
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

    // notification-area must NOT be treated as alert-banner.
    assert!(
        !is_alert_banner_zone("notification-area"),
        "notification-area must not be treated as alert-banner"
    );

    // Render and check: the backdrop must NOT be severity critical (pure red R~1.0, G~0.0, B~0.0).
    // It should be notification urgency critical fallback: #450612.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for notification-area urgency=3"
    );

    // Check first vertex: R should NOT be ~1.0 (that would be severity critical).
    let first = &vertices[0];
    assert!(
        first.color[0] < 0.7,
        "notification-area urgency=3 R must NOT be severity critical (~1.0), got {}",
        first.color[0]
    );
}

// ── Notification urgency token tests ─────────────────────────────────────

/// urgency_to_notification_color: low (0) maps to #000000 fallback.
#[test]
fn test_notification_urgency_low_fallback() {
    let no_tokens = HashMap::new();
    let color = urgency_to_notification_color(0, &no_tokens);
    assert!(
        color.r < 0.001,
        "urgency low R should be ~0.0, got {}",
        color.r
    );
    assert!(
        color.g < 0.001,
        "urgency low G should be ~0.0, got {}",
        color.g
    );
    assert!(
        color.b < 0.001,
        "urgency low B should be ~0.0, got {}",
        color.b
    );
}

/// urgency_to_notification_color: normal (1) maps to #0C1426 fallback.
#[test]
fn test_notification_urgency_normal_fallback() {
    let no_tokens = HashMap::new();
    let color = urgency_to_notification_color(1, &no_tokens);
    assert!(
        (color.r - 0.0037).abs() < 0.002,
        "urgency normal R should be ~0.0037, got {}",
        color.r
    );
    assert!(
        (color.g - 0.0070).abs() < 0.002,
        "urgency normal G should be ~0.0070, got {}",
        color.g
    );
    assert!(
        (color.b - 0.0194).abs() < 0.003,
        "urgency normal B should be ~0.0194, got {}",
        color.b
    );
    assert!(
        color.b > color.r + 0.01,
        "urgency normal B should be > R (blue tint)"
    );
}

/// urgency_to_notification_color: urgent (2) maps to #2A1E08 fallback.
#[test]
fn test_notification_urgency_urgent_fallback() {
    let no_tokens = HashMap::new();
    let color = urgency_to_notification_color(2, &no_tokens);
    assert!(
        (color.r - 0.0232).abs() < 0.003,
        "urgency urgent R should be ~0.0232, got {}",
        color.r
    );
    assert!(
        (color.g - 0.0130).abs() < 0.003,
        "urgency urgent G should be ~0.0130, got {}",
        color.g
    );
    assert!(
        (color.b - 0.0024).abs() < 0.002,
        "urgency urgent B should be ~0.0024, got {}",
        color.b
    );
    assert!(
        color.r > color.g && color.g > color.b,
        "urgency urgent should retain an amber-black R > G > B ordering"
    );
}

/// urgency_to_notification_color: critical (3) maps to #450612 fallback.
#[test]
fn test_notification_urgency_critical_fallback() {
    let no_tokens = HashMap::new();
    let color = urgency_to_notification_color(3, &no_tokens);
    assert!(
        (color.r - 0.0595).abs() < 0.004,
        "urgency critical R should be ~0.0595, got {}",
        color.r
    );
    assert!(
        (color.g - 0.0018).abs() < 0.002,
        "urgency critical G should be ~0.0018, got {}",
        color.g
    );
    assert!(
        (color.b - 0.0060).abs() < 0.002,
        "urgency critical B should be ~0.0060, got {}",
        color.b
    );
    assert!(
        color.r > color.b && color.b > color.g,
        "urgency critical should retain a red-black R > B > G ordering"
    );
}

/// urgency_to_notification_color: urgency > 3 is clamped to critical (3).
#[test]
fn test_notification_urgency_clamped_above_3() {
    let no_tokens = HashMap::new();
    let critical3 = urgency_to_notification_color(3, &no_tokens);
    let clamped4 = urgency_to_notification_color(4, &no_tokens);
    let clamped100 = urgency_to_notification_color(100, &no_tokens);
    assert_eq!(
        critical3.r, clamped4.r,
        "urgency=4 should clamp to urgency=3"
    );
    assert_eq!(
        critical3.g, clamped4.g,
        "urgency=4 should clamp to urgency=3"
    );
    assert_eq!(
        critical3.b, clamped4.b,
        "urgency=4 should clamp to urgency=3"
    );
    assert_eq!(
        critical3.r, clamped100.r,
        "urgency=100 should clamp to urgency=3"
    );
}

/// Profile token override: color.notification.urgency.low overrides fallback.
#[test]
fn test_notification_urgency_low_token_override() {
    let mut token_map = HashMap::new();
    // Override low with pure cyan (#00FFFF) — clearly distinct from default.
    token_map.insert(
        "color.notification.urgency.low".to_string(),
        "#00FFFF".to_string(),
    );
    let color = urgency_to_notification_color(0, &token_map);
    assert!(
        color.r < 0.1,
        "custom low token R should be ~0.0 (cyan), got {}",
        color.r
    );
    assert!(
        color.g > 0.9,
        "custom low token G should be ~1.0 (cyan), got {}",
        color.g
    );
    assert!(
        color.b > 0.9,
        "custom low token B should be ~1.0 (cyan), got {}",
        color.b
    );
}

/// Profile token override: color.notification.urgency.critical overrides fallback.
#[test]
fn test_notification_urgency_critical_token_override() {
    let mut token_map = HashMap::new();
    // Override critical with the exemplar's dark red-black token.
    token_map.insert(
        "color.notification.urgency.critical".to_string(),
        "#450612".to_string(),
    );
    let color = urgency_to_notification_color(3, &token_map);
    assert!(
        (color.r - 0.0595).abs() < 0.004,
        "custom critical token R should decode from sRGB hex to ~0.0595 linear, got {}",
        color.r
    );
    assert!(
        (color.g - 0.0018).abs() < 0.002,
        "custom critical token G should decode from sRGB hex to ~0.0018 linear, got {}",
        color.g
    );
    assert!(
        (color.b - 0.0060).abs() < 0.002,
        "custom critical token B should decode from sRGB hex to ~0.0060 linear, got {}",
        color.b
    );
    assert!(
        color.r > color.b && color.b > color.g,
        "custom critical token should retain a red-black R > B > G ordering"
    );
}

/// notification-area urgency-tinted backdrop: renders backdrop at 0.8 opacity.
///
/// Per spec: non-alert-banner Notification content must use
/// color.notification.urgency.* tokens with fixed 0.8 opacity.
/// The policy.backdrop_opacity must NOT override this.
#[tokio::test]
async fn test_notification_area_backdrop_uses_0_8_opacity() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area urgency opacity test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 0.5)),
            // backdrop_opacity = 0.5 must NOT override the 0.8 fixed opacity
            backdrop_opacity: Some(0.5),
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "test".to_owned(),
                icon: String::new(),
                urgency: 1, // normal
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

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    assert!(!vertices.is_empty(), "expected vertices");
    // The first quad's alpha (index 3 of color) should be 0.8.
    let first_alpha = vertices[0].color[3];
    assert!(
        (first_alpha - 0.8).abs() < 0.01,
        "notification-area backdrop alpha must be 0.8, got {first_alpha}"
    );
}

/// notification-area border rendering: 1px 4-quad border is emitted after the
/// urgency-tinted backdrop quad.
///
/// For a Stack zone with one Notification publish, render_zone_content should emit:
///   - 6 vertices for the backdrop quad
///   - up to 24 vertices (4 × 6) for the border quads
#[tokio::test]
async fn test_notification_area_emits_border_quads() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area border test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.05, 0.05, 0.05, 1.0)),
            font_size_px: Some(18.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "border test".to_owned(),
                icon: String::new(),
                urgency: 0,
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

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // One backdrop (6 vertices) + up to 4 border quads (6 each) = 6 + 24 = 30 max.
    // Minimum: 6 (backdrop) + 6 (at least top edge border) = 12.
    assert!(
        vertices.len() >= 12,
        "expected at least 12 vertices (backdrop + border), got {}",
        vertices.len()
    );
    // Total should be 6 * N for some N ≥ 2 (backdrop + at least one border quad).
    assert_eq!(
        vertices.len() % 6,
        0,
        "vertex count must be a multiple of 6 (each quad = 6 vertices), got {}",
        vertices.len()
    );
}

/// alert-banner does NOT emit border quads — border rendering is only for
/// non-alert-banner notification zones.
#[tokio::test]
async fn test_alert_banner_does_not_emit_border_quads() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert banner — no border".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.07,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(20.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            backdrop_opacity: Some(1.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "no border here".to_owned(),
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

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // alert-banner: only 6 vertices (one backdrop quad, no border).
    assert_eq!(
        vertices.len(),
        6,
        "alert-banner must emit exactly 6 vertices (backdrop only, no border), got {}",
        vertices.len()
    );
}

/// Border color uses color.border.default token when present.
#[tokio::test]
async fn test_notification_area_border_uses_border_default_token() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    // Install a custom border token: pure cyan (#00FFFF).
    let mut token_map = HashMap::new();
    token_map.insert("color.border.default".to_string(), "#00FFFF".to_string());
    compositor.set_token_map(token_map);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area border token test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.05, 0.05, 0.05, 1.0)),
            font_size_px: Some(18.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "cyan border".to_owned(),
                icon: String::new(),
                urgency: 0,
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

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // vertices[0..6] = backdrop quad (urgency low color)
    // vertices[6..] = border quads (should be cyan: R≈0, G≈1, B≈1)
    assert!(
        vertices.len() > 6,
        "expected border quads after backdrop, only got {}",
        vertices.len()
    );
    // Check border quad color (vertex index 6 is the first border vertex).
    let border_v = &vertices[6];
    assert!(
        border_v.color[0] < 0.1,
        "border R should be ~0.0 (cyan token), got {}",
        border_v.color[0]
    );
    assert!(
        border_v.color[1] > 0.9,
        "border G should be ~1.0 (cyan token), got {}",
        border_v.color[1]
    );
    assert!(
        border_v.color[2] > 0.9,
        "border B should be ~1.0 (cyan token), got {}",
        border_v.color[2]
    );
}

// ── Token-resolved severity color tests ───────────────────────────────────

/// Custom `color.severity.warning` token overrides the hardcoded SEVERITY_WARNING
/// constant for urgency=2.
#[test]
fn test_custom_severity_warning_token_overrides_constant() {
    let mut token_map = HashMap::new();
    // Custom warning: bright green (#00FF00) — clearly distinct from amber.
    token_map.insert("color.severity.warning".to_string(), "#00FF00".to_string());
    let color = urgency_to_severity_color(2, &token_map);
    assert!(
        color.g > 0.9,
        "custom warning token G should be ~1.0 (green), got {}",
        color.g
    );
    assert!(
        color.r < 0.1,
        "custom warning token R should be ~0.0 (green), got {}",
        color.r
    );
    assert!(
        color.b < 0.1,
        "custom warning token B should be ~0.0 (green), got {}",
        color.b
    );
}

/// Custom `color.severity.critical` token overrides the hardcoded SEVERITY_CRITICAL.
#[test]
fn test_custom_severity_critical_token_overrides_constant() {
    let mut token_map = HashMap::new();
    // Custom critical: bright magenta (#FF00FF).
    token_map.insert("color.severity.critical".to_string(), "#FF00FF".to_string());
    let color = urgency_to_severity_color(3, &token_map);
    assert!(
        color.r > 0.9,
        "custom critical R should be ~1.0 (magenta), got {}",
        color.r
    );
    assert!(
        color.b > 0.9,
        "custom critical B should be ~1.0 (magenta), got {}",
        color.b
    );
    assert!(
        color.g < 0.1,
        "custom critical G should be ~0.0 (magenta), got {}",
        color.g
    );
}

/// Custom `color.severity.info` token overrides the hardcoded SEVERITY_INFO.
#[test]
fn test_custom_severity_info_token_overrides_constant() {
    let mut token_map = HashMap::new();
    // Custom info: pure red (#FF0000) — clearly distinct from default blue.
    token_map.insert("color.severity.info".to_string(), "#FF0000".to_string());
    let color0 = urgency_to_severity_color(0, &token_map);
    let color1 = urgency_to_severity_color(1, &token_map);
    for (urgency, color) in [(0, color0), (1, color1)] {
        assert!(
            color.r > 0.9,
            "custom info urgency={urgency} R should be ~1.0 (red), got {}",
            color.r
        );
        assert!(
            color.g < 0.1,
            "custom info urgency={urgency} G should be ~0.0 (red), got {}",
            color.g
        );
        assert!(
            color.b < 0.1,
            "custom info urgency={urgency} B should be ~0.0 (red), got {}",
            color.b
        );
    }
}

/// Invalid/absent token values fall back to hardcoded constants.
#[test]
fn test_invalid_severity_token_value_falls_back_to_constant() {
    let mut token_map = HashMap::new();
    // Not a valid hex color — should be ignored.
    token_map.insert(
        "color.severity.warning".to_string(),
        "not-a-color".to_string(),
    );
    let color = urgency_to_severity_color(2, &token_map);
    // Falls back to SEVERITY_WARNING (#FFB800): R~1.0, G~0.72, B~0.0.
    assert!(
        color.r > 0.9,
        "fallback warning R should be ~1.0, got {}",
        color.r
    );
    assert!(
        color.g > 0.5,
        "fallback warning G should be >0.5, got {}",
        color.g
    );
    assert!(
        color.b < 0.1,
        "fallback warning B should be ~0.0, got {}",
        color.b
    );
}

/// Custom severity tokens in [design_tokens] affect alert-banner backdrop colors.
///
/// This is the end-to-end integration test: `set_token_map` populates the
/// compositor, and `render_zone_content` uses the token-resolved color for the
/// alert-banner backdrop.
#[tokio::test]
async fn test_custom_severity_tokens_affect_alert_banner_backdrop() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    // Install a custom token map: override warning with pure green (#00FF00).
    let mut token_map = HashMap::new();
    token_map.insert("color.severity.warning".to_string(), "#00FF00".to_string());
    compositor.set_token_map(token_map);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "alert-banner".to_owned(),
        description: "alert banner zone".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Top,
            height_pct: 0.07,
            width_pct: 1.0,
            margin_px: 0.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            font_size_px: Some(20.0),
            backdrop: Some(Rgba::new(0.08, 0.08, 0.08, 1.0)),
            backdrop_opacity: Some(1.0),
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish urgency=2 (warning) — should use custom green token.
    scene
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                text: "Custom token warning".to_owned(),
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

    // Collect vertices from render_zone_content.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // rect_vertices emits 6 vertices per quad; each vertex has a `color: [f32; 4]` field.
    // The backdrop should be green (R~0.0, G~1.0, B~0.0), not amber.
    assert!(
        !vertices.is_empty(),
        "expected backdrop vertices for alert-banner"
    );

    // Check first vertex color. RectVertex layout: [position: [f32; 2], color: [f32; 4]].
    let first = &vertices[0];
    assert!(
        first.color[1] > 0.9,
        "alert-banner backdrop G should be ~1.0 (custom green token), got {}",
        first.color[1]
    );
    assert!(
        first.color[0] < 0.1,
        "alert-banner backdrop R should be ~0.0 (custom green token), got {}",
        first.color[0]
    );
    assert!(
        first.color[2] < 0.1,
        "alert-banner backdrop B should be ~0.0 (custom green token), got {}",
        first.color[2]
    );
}

/// ZoneAnimationState fade-in reaches 1.0 after duration elapses.
#[test]
fn test_zone_animation_state_fade_in_completes() {
    // Use 0ms duration for instant completion.
    let state = ZoneAnimationState::fade_in(0);
    // Opacity at duration=0 should immediately be target (1.0).
    let opacity = state.current_opacity();
    assert_eq!(opacity, 1.0, "fade-in with 0ms should be 1.0 immediately");
    assert!(
        state.is_complete(),
        "0ms fade-in should be complete immediately"
    );
}

/// ZoneAnimationState fade-out starts at 1.0 and reaches 0.0 after duration.
#[test]
fn test_zone_animation_state_fade_out_completes() {
    let state = ZoneAnimationState::fade_out(0);
    let opacity = state.current_opacity();
    assert_eq!(opacity, 0.0, "fade-out with 0ms should be 0.0 immediately");
    assert!(
        state.is_complete(),
        "0ms fade-out should be complete immediately"
    );
}

/// ZoneAnimationState with non-zero duration: opacity is interpolated.
#[test]
fn test_zone_animation_state_interpolates() {
    // 10_000ms duration — very long, so elapsed << duration.
    let state = ZoneAnimationState::fade_in(10_000);
    // Very shortly after creation, opacity should be close to 0.
    let opacity = state.current_opacity();
    assert!(
        (0.0..=0.1).contains(&opacity),
        "fade-in opacity shortly after start should be near 0, got {opacity}"
    );
    assert!(
        !state.is_complete(),
        "10s fade-in should not be complete immediately"
    );
}

/// backdrop_opacity overrides the backdrop color's alpha channel.
/// When backdrop_opacity=0.6 and backdrop.a=1.0, effective alpha=0.6.
#[tokio::test]
async fn test_backdrop_opacity_overrides_color_alpha() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // backdrop color has alpha=1.0 but backdrop_opacity=0.6 should override it.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "test backdrop opacity override".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)), // alpha=1.0
            backdrop_opacity: Some(0.6),                   // override to 0.6
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("opacity test".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // The backdrop rendered should use alpha=0.6 (backdrop_opacity), not 1.0.
    // We verify this by checking the vertex colors produced — the alpha channel
    // of the first rect vertex should reflect 0.6.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert!(!vertices.is_empty(), "expected backdrop vertices");
    // The RectVertex has color field [f32; 4]; alpha should be ~0.6.
    let alpha = vertices[0].color[3];
    assert!(
        (alpha - 0.6).abs() < 0.01,
        "backdrop alpha should be ~0.6 (backdrop_opacity override), got {alpha}"
    );
}

/// backdrop=None: no backdrop quad rendered even when backdrop_opacity is set.
#[tokio::test]
async fn test_no_backdrop_when_backdrop_is_none() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "no-backdrop test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            backdrop: None,
            backdrop_opacity: Some(0.9), // ignored because backdrop is None
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("no backdrop".to_owned()),
            "test",
            None,
            None,
            None,
        )
        .unwrap();

    // With backdrop=None, no rect vertices should be emitted.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert!(
        vertices.is_empty(),
        "no backdrop quad should be rendered when policy.backdrop is None"
    );
}

/// backdrop=None with Notification content: no backdrop or border quads rendered.
///
/// Even though Notification content in a non-alert-banner zone overrides the
/// backdrop color with urgency-tinted tokens, the override must respect the
/// policy.backdrop contract: when backdrop is None, nothing is emitted.
#[tokio::test]
async fn test_notification_no_backdrop_when_backdrop_is_none() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "notification area with backdrop=None".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.02,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: None,
            backdrop_opacity: Some(0.9),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "no backdrop".to_owned(),
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

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert!(
        vertices.is_empty(),
        "no backdrop or border quads should be rendered when policy.backdrop is None, got {} vertices",
        vertices.len()
    );
}

/// text.rs: TextItem::from_zone_policy respects all RenderingPolicy fields.
#[test]
fn test_from_zone_policy_reads_all_policy_fields() {
    use crate::text::TextItem;

    let policy = RenderingPolicy {
        font_size_px: Some(28.0),
        text_color: Some(Rgba::new(1.0, 0.5, 0.0, 1.0)), // orange
        font_family: Some(FontFamily::SystemMonospace),
        text_align: Some(TextAlign::Center),
        outline_color: Some(Rgba::BLACK),
        outline_width: Some(1.5),
        margin_horizontal: Some(12.0),
        margin_vertical: Some(6.0),
        ..Default::default()
    };

    let item = TextItem::from_zone_policy("test", 0.0, 0.0, 400.0, 100.0, &policy, 1.0);
    assert_eq!(item.font_size_px, 28.0);
    assert_eq!(item.font_family, FontFamily::SystemMonospace);
    assert_eq!(item.alignment, TextAlign::Center);
    assert!(item.outline_color.is_some(), "outline_color should be set");
    assert_eq!(item.outline_width.unwrap(), 1.5);
    // Margins: x+12, y+6
    assert_eq!(item.pixel_x, 12.0);
    assert_eq!(item.pixel_y, 6.0);
}
