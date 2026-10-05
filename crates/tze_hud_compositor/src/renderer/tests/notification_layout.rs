use super::*;

// ── Notification text rendering [hud-j5g5.3] ─────────────────────────────
//
// Spec §Notification Text Rendering:
//   - typography.body.size (16px default) font size
//   - color.text.primary text color
//   - left-aligned, 9px inset (8px padding + 1px border)
//   - clips at content area boundary (no wrapping in v1)

/// Notification text uses typography.body.size (default 16px) when token absent.
///
/// AC: notification text must use font_size_px resolved from typography.body.size.
#[tokio::test]
async fn test_notification_text_uses_body_typography_token_default() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "text rendering test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            // No font_size_px set — must fall through to typography.body.size token.
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Doorbell rang".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    // No token map set → typography.body.size absent → default 16px.
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 1, "must produce one TextItem for notification");
    assert_eq!(
        items[0].font_size_px, 16.0,
        "notification text must use typography.body.size default (16px)"
    );
}

/// `typography.body.size` sets the notification font size (with or without a `px`
/// suffix); absent, it is 16px.
#[test]
fn test_notification_body_font_size_token() {
    for (token, expected) in [(Some("20px"), 20.0), (Some("18"), 18.0), (None, 16.0)] {
        let token_map: HashMap<String, String> = token
            .map(|v| ("typography.body.size".to_string(), v.to_string()))
            .into_iter()
            .collect();
        assert_eq!(
            Compositor::resolve_body_font_size(&token_map),
            expected,
            "typography.body.size={token:?}"
        );
    }
}

/// `color.text.primary` sets the notification text colour; absent, it is near-white.
#[test]
fn test_notification_text_primary_color_token() {
    let red = HashMap::from([("color.text.primary".to_string(), "#FF0000".to_string())]);
    let color = Compositor::resolve_text_primary_color(&red);
    assert_eq!(&color[..3], &[255, 0, 0], "#FF0000 token");

    let color = Compositor::resolve_text_primary_color(&HashMap::new());
    assert_eq!(&color[..3], &[255, 255, 255], "absent token is near-white");
    assert!(
        color[3] >= 200,
        "fallback alpha must be near-opaque, got {}",
        color[3]
    );
}

/// Stack notifications render a dedicated dismiss label and reserve width for it.
#[tokio::test]
async fn test_notification_stack_adds_dismiss_label_and_reserves_text_width() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "dismiss label test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Dismissible notification".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 2, "body text + dismiss label expected");

    let body_item = items
        .iter()
        .find(|item| &*item.text == "Dismissible notification")
        .expect("body text item must exist");
    let dismiss_item = items
        .iter()
        .find(|item| &*item.text == "X")
        .expect("dismiss text item must exist");

    assert_eq!(body_item.pixel_x, 9.0, "body text keeps left inset");
    assert_eq!(
        body_item.bounds_width, 274.0,
        "body width must reserve dismiss control space"
    );
    assert_eq!(
        dismiss_item.alignment,
        TextAlign::Center,
        "dismiss label must be centered in its button bounds"
    );
    assert!(
        dismiss_item.pixel_x >= 300.0,
        "dismiss label must sit near the right edge, got {}",
        dismiss_item.pixel_x
    );
}

// ── Dismiss button typography token tests [hud-y08tp] ────────────────────
//
// Acceptance criteria:
//   1. No-token path: dismiss button uses NOTIFICATION_DISMISS_FONT_SIZE_PX
//      (12.0 px) and NOTIFICATION_DISMISS_FONT_WEIGHT (700) as defaults.
//   2. Token path: typography.notification.dismiss.font_size_px and
//      typography.notification.dismiss.font_weight override the defaults.

/// Dismiss button uses default font_size_px (12.0) and font_weight (700)
/// when dismiss typography tokens are absent.
///
/// AC 1: no-token path preserves visual defaults.
#[tokio::test]
async fn test_dismiss_button_uses_default_font_size_and_weight() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "dismiss font default test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Default dismiss test".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let dismiss_item = items
        .iter()
        .find(|item| &*item.text == "X")
        .expect("dismiss text item must exist");

    assert_eq!(
        dismiss_item.font_size_px, NOTIFICATION_DISMISS_FONT_SIZE_PX,
        "dismiss button font_size_px must be the default (12.0) when token absent"
    );
    assert_eq!(
        dismiss_item.font_weight, NOTIFICATION_DISMISS_FONT_WEIGHT,
        "dismiss button font_weight must be the default (700) when token absent"
    );
}

/// Dismiss button font_size_px and font_weight read from design tokens when
/// `typography.notification.dismiss.font_size_px` and
/// `typography.notification.dismiss.font_weight` are present.
///
/// AC 2: token-override path correctly propagates to the rendered TextItem.
#[tokio::test]
async fn test_dismiss_button_respects_typography_tokens() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    // Inject dismiss typography tokens.
    let mut token_map = HashMap::new();
    token_map.insert(
        "typography.notification.dismiss.font_size_px".to_string(),
        "16px".to_string(),
    );
    token_map.insert(
        "typography.notification.dismiss.font_weight".to_string(),
        "400".to_string(),
    );
    compositor.set_token_map(token_map);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "dismiss font token test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Token override dismiss test".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-b",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let dismiss_item = items
        .iter()
        .find(|item| &*item.text == "X")
        .expect("dismiss text item must exist");

    assert_eq!(
        dismiss_item.font_size_px, 16.0,
        "dismiss button font_size_px must be 16.0 from token override"
    );
    assert_eq!(
        dismiss_item.font_weight, 400,
        "dismiss button font_weight must be 400 from token override"
    );
}

/// Notification text is inset by 9px (8px padding + 1px border) from backdrop edges.
///
/// AC: text content area starts at (x + 9, y + 9).
#[tokio::test]
async fn test_notification_text_inset_from_backdrop_edges() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone at x=0, y=0 (x_pct=0, y_pct=0) with 100% width and 50% height.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "inset test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,  // zx = 0
            height_pct: 0.5, // zy = 0
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Test notification".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items.len(), 1, "must produce one TextItem");

    let item = &items[0];
    // Zone starts at x=0, y=0. Text must be inset by 9px (1px border + 8px padding).
    assert_eq!(
        item.pixel_x, 9.0,
        "text pixel_x must be 9.0 (1px border + 8px padding inset); got {}",
        item.pixel_x
    );
    assert_eq!(
        item.pixel_y, 9.0,
        "text pixel_y must be 9.0 (1px border + 8px padding inset); got {}",
        item.pixel_y
    );
    // Text is left-aligned.
    assert_eq!(
        item.alignment,
        TextAlign::Start,
        "notification text must be left-aligned (TextAlign::Start)"
    );
    // Overflow is Clip.
    assert_eq!(
        item.overflow,
        TextOverflow::Clip,
        "notification text must clip at content area (no wrapping)"
    );
}

// ── Two-line notification rendering [hud-ltgk.3] ──────────────────────────
//
// Spec §Two-line notification layout:
//   - Empty title → single-line backward-compatible path (1 TextItem)
//   - Non-empty title → two-line path (2 TextItems: title bold, body regular)
//   - Title font_weight = NOTIFICATION_TITLE_WEIGHT (700)
//   - Body font_size = font_size_px * NOTIFICATION_BODY_SCALE (0.85×)
//   - Body font_weight = 400 (regular)
//   - Body positioned below title line + inter-line gap

/// Two-line notification: empty `title` produces exactly 1 TextItem (backward compat).
///
/// AC: `collect_text_items` with `NotificationPayload { title: "" }` MUST produce
/// the same output as before this feature was added.
#[tokio::test]
async fn test_two_line_notification_empty_title_produces_one_text_item() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "two-line backward-compat test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Body only notification".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(), // empty: must use single-line path
                actions: vec![],
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    assert_eq!(
        items.len(),
        1,
        "single-line notification (empty title) must produce exactly 1 TextItem"
    );
    assert_eq!(
        &*items[0].text, "Body only notification",
        "single-line TextItem must contain body text"
    );
    assert_eq!(
        items[0].font_weight, 400,
        "single-line notification must use regular weight (400)"
    );
}

/// Two-line notification: non-empty `title` produces 2 TextItems with correct properties.
///
/// AC:
///   1. `collect_text_items` produces exactly 2 TextItems for the slot.
///   2. First item (lower pixel_y): title text, weight=700, font_size_px=16.
///   3. Second item (higher pixel_y): body text, weight=400, font_size=16*0.85=13.6.
///   4. Body item pixel_y > title item pixel_y.
#[tokio::test]
async fn test_two_line_notification_title_produces_two_text_items() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone at x=0 (x_pct=0) so items are easy to find (pixel_x near 0).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "two-line title test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                title: "System Alert".to_owned(),
                text: "Disk space low on /dev/sda1".to_owned(),
                icon: String::new(),
                urgency: 2,
                ttl_ms: None,
                actions: vec![],
            }),
            "test-agent",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    assert_eq!(
        items.len(),
        2,
        "two-line notification (non-empty title) must produce exactly 2 TextItems, got {} items: {:?}",
        items.len(),
        items.iter().map(|i| &i.text).collect::<Vec<_>>()
    );

    // Sort by pixel_y to get title (top) and body (bottom).
    let mut sorted = items.clone();
    sorted.sort_by(|a, b| a.pixel_y.partial_cmp(&b.pixel_y).unwrap());
    let title_item = &sorted[0];
    let body_item = &sorted[1];

    // Title item checks.
    assert_eq!(
        &*title_item.text, "System Alert",
        "first item must be the title text"
    );
    assert_eq!(
        title_item.font_weight, NOTIFICATION_TITLE_WEIGHT,
        "title must use bold weight ({}), got {}",
        NOTIFICATION_TITLE_WEIGHT, title_item.font_weight
    );
    assert!(
        (title_item.font_size_px - 16.0).abs() < 0.01,
        "title must use policy font_size_px (16.0), got {}",
        title_item.font_size_px
    );

    // Body item checks.
    assert_eq!(
        &*body_item.text, "Disk space low on /dev/sda1",
        "second item must be the body text"
    );
    assert_eq!(
        body_item.font_weight, 400,
        "body must use regular weight (400), got {}",
        body_item.font_weight
    );
    let expected_body_size = 16.0 * NOTIFICATION_BODY_SCALE;
    assert!(
        (body_item.font_size_px - expected_body_size).abs() < 0.1,
        "body font size must be 0.85× title size ({expected_body_size:.2}), got {}",
        body_item.font_size_px
    );

    // Body must be below title.
    assert!(
        body_item.pixel_y > title_item.pixel_y,
        "body item must be below title: title_y={}, body_y={}",
        title_item.pixel_y,
        body_item.pixel_y
    );
}

/// Two-line slot height: `notification_slot_height` returns a larger height for
/// two-line notifications than for single-line notifications.
///
/// AC: two_line_slot_h > single_line_slot_h for the same RenderingPolicy.
#[test]
fn test_notification_slot_height_two_line_exceeds_single_line() {
    let policy = RenderingPolicy {
        font_size_px: Some(16.0),
        ..Default::default()
    };

    let single_line = NotificationPayload {
        text: "body".to_owned(),
        title: String::new(),
        ..Default::default()
    };

    let two_line = NotificationPayload {
        title: "Title".to_owned(),
        text: "body".to_owned(),
        ..Default::default()
    };

    let h_single = Compositor::notification_slot_height(
        &single_line,
        &policy,
        NOTIFICATION_BODY_SCALE,
        NOTIFICATION_INTER_LINE_GAP,
    );
    let h_two_line = Compositor::notification_slot_height(
        &two_line,
        &policy,
        NOTIFICATION_BODY_SCALE,
        NOTIFICATION_INTER_LINE_GAP,
    );

    assert!(
        h_two_line > h_single,
        "two-line slot height ({h_two_line:.2}) must exceed single-line ({h_single:.2})"
    );

    // Single-line == stack_slot_height
    let h_stack = Compositor::stack_slot_height(&policy);
    assert!(
        (h_single - h_stack).abs() < 0.01,
        "single-line notification_slot_height ({h_single:.2}) must equal stack_slot_height ({h_stack:.2})"
    );
}

/// Two-line notification stacking: two two-line notifications stack correctly,
/// with the second slot starting after the first two-line slot height.
#[tokio::test]
async fn test_two_line_notifications_stack_correctly() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "two-line stacking test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 0.9,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish two two-line notifications.
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                title: "First Alert".to_owned(),
                text: "First body text".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                actions: vec![],
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                title: "Second Alert".to_owned(),
                text: "Second body text".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                actions: vec![],
            }),
            "agent-b",
            None,
            None,
            None,
        )
        .unwrap();

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    // Two two-line notifications = 4 TextItems.
    assert_eq!(
        items.len(),
        4,
        "two two-line notifications must produce 4 TextItems (2 per notification), got {} items",
        items.len()
    );

    // All 4 items must have distinct pixel_y values (no overlap).
    let mut ys: Vec<f32> = items.iter().map(|i| i.pixel_y).collect();
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ys.dedup_by(|a, b| (*a - *b).abs() < 0.01);
    assert_eq!(
        ys.len(),
        4,
        "all 4 TextItems must have distinct pixel_y values, got: {ys:?}"
    );
}

/// Rounded notification backdrops must use per-notification two-line slot heights.
///
/// Regression guard: when `backdrop_radius` is enabled, backdrop geometry comes from
/// `collect_all_rounded_rect_cmds()` (not the flat-rect path). For two-line notifications,
/// the rounded backdrop height must match `notification_slot_height`, otherwise body text
/// can extend outside the card.
#[tokio::test]
async fn test_rounded_notification_backdrop_uses_two_line_slot_height() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    let policy = RenderingPolicy {
        backdrop: Some(Rgba::new(0.2, 0.2, 0.2, 0.9)),
        backdrop_radius: Some(12.0),
        font_size_px: Some(16.0),
        ..Default::default()
    };
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "rounded notification slot height test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: policy.clone(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    let payload = NotificationPayload {
        title: "Critical Alert".to_owned(),
        text: "Please check corner radius and typography.".to_owned(),
        ..Default::default()
    };
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(payload.clone()),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    let rr = compositor.collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0);
    assert_eq!(
        rr.chrome.len(),
        2,
        "single rounded notification: one card cmd, then its dismiss outline"
    );

    let expected_h = Compositor::notification_slot_height(
        &payload,
        &policy,
        NOTIFICATION_BODY_SCALE,
        NOTIFICATION_INTER_LINE_GAP,
    );
    let actual_h = rr.chrome[0].height;
    assert!(
        (actual_h - expected_h).abs() < 0.1,
        "rounded notification backdrop height must match two-line slot height: expected {expected_h:.2}, got {actual_h:.2}"
    );
}

// ── TTL auto-dismiss [hud-j5g5.3] ────────────────────────────────────────
//
// Spec §Notification TTL Auto-Dismiss with Fade-Out:
//   - Default TTL: 8000ms (zone auto_clear_ms)
//   - Per-publish ttl_ms overrides zone default
//   - 150ms linear fade-out from 1.0 to 0.0
//   - Opacity ~0.5 at 75ms midpoint
//   - Removal from active_publishes on fade completion
//   - Independent simultaneous fades for multiple notifications

/// PublicationAnimationState: before TTL expires, opacity is 1.0.
#[test]
fn test_pub_anim_state_before_ttl_expiry_opacity_is_1() {
    // TTL = 10_000ms (far future), fade not yet started.
    let state = PublicationAnimationState::new(Some(10_000), None);
    assert_eq!(
        state.current_opacity(),
        1.0,
        "opacity must be 1.0 before TTL expires"
    );
    assert!(
        !state.is_fade_complete(),
        "fade must not be complete before TTL expires"
    );
}

/// PublicationAnimationState: custom TTL=3000ms starts fade at 3000ms.
///
/// AC: notification published with ttl_ms=3000 begins fade-out at 3000ms.
#[test]
fn test_pub_anim_state_custom_ttl_3000ms_triggers_fade() {
    let mut state = PublicationAnimationState::new(Some(3_000), None);

    // Simulate 3001ms elapsed by setting first_seen to the past.
    state.first_seen = std::time::Instant::now() - std::time::Duration::from_millis(3_001);

    state.tick();

    assert!(
        state.fade_start.is_some(),
        "fade must start after TTL (3000ms) has elapsed"
    );
}

/// PublicationAnimationState: at 75ms into the 150ms fade, opacity ≈ 0.5.
///
/// AC: opacity interpolates linearly; at midpoint it must be approximately 0.5.
#[test]
fn test_pub_anim_state_opacity_at_75ms_midpoint_is_half() {
    let mut state = PublicationAnimationState::new(Some(0), None); // TTL=0 → instant expire

    // TTL already expired: set first_seen far in the past.
    state.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    state.tick(); // starts fade

    // Now simulate 75ms into the fade.
    state.fade_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(75));

    let opacity = state.current_opacity();
    assert!(
        (opacity - 0.5).abs() < 0.1,
        "at 75ms midpoint, opacity must be ≈ 0.5, got {opacity}"
    );
}

/// PublicationAnimationState: after 150ms, is_fade_complete returns true.
///
/// AC: publication must be removed from active_publishes when fade completes.
#[test]
fn test_pub_anim_state_is_complete_after_150ms() {
    let mut state = PublicationAnimationState::new(Some(0), None);

    // TTL already expired.
    state.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    state.tick(); // starts fade

    // Simulate 150ms+ elapsed since fade started.
    state.fade_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(151));

    assert!(
        state.is_fade_complete(),
        "is_fade_complete must return true after 150ms fade duration"
    );
    assert_eq!(
        state.current_opacity(),
        0.0,
        "opacity must be 0.0 after fade completes"
    );
}

/// prune_faded_publications removes a publication whose fade is complete.
///
/// AC: publication removed from active_publishes when fade-out completes;
///     remaining notifications reflow (slot positions recalculated).
#[tokio::test]
async fn test_prune_faded_publications_removes_completed_fades() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "prune test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish two notifications.
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "First".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Second".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-b",
            None,
            None,
            None,
        )
        .unwrap();

    // Manually seed pub_animation_states with a completed-fade for "agent-a".
    let publishes = scene
        .zone_registry
        .active_publishes
        .get("notification-area")
        .unwrap();
    let (a_wall_us, a_ns) = {
        let r = &publishes[0]; // agent-a is first (oldest)
        (r.published_at_wall_us, r.publisher_namespace.clone())
    };

    let mut completed_state = PublicationAnimationState::new(Some(0), None);
    completed_state.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    completed_state.tick(); // starts fade
    // Set fade_start 151ms in the past → fade complete.
    completed_state.fade_start =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(151));

    compositor
        .pub_animation_states
        .entry("notification-area".to_string())
        .or_default()
        .insert((a_wall_us, a_ns), completed_state);

    // Before prune: 2 publications.
    assert_eq!(
        scene
            .zone_registry
            .active_publishes
            .get("notification-area")
            .map(|v| v.len()),
        Some(2),
        "before prune: 2 publications expected"
    );

    // Prune: removes agent-a (completed fade).
    compositor.prune_faded_publications(&mut scene);

    // After prune: only 1 publication remains (agent-b).
    let remaining = scene
        .zone_registry
        .active_publishes
        .get("notification-area")
        .map(|v| v.len());
    assert_eq!(
        remaining,
        Some(1),
        "after prune: 1 publication must remain (agent-b)"
    );
    // Verify the remaining publication is agent-b.
    let remaining_pub = &scene.zone_registry.active_publishes["notification-area"][0];
    assert_eq!(
        remaining_pub.publisher_namespace, "agent-b",
        "remaining publication must be from agent-b"
    );
}

/// Two notifications with TTLs expiring simultaneously fade independently.
///
/// AC: each has its own PublicationAnimationState; neither affects the other.
#[test]
fn test_simultaneous_independent_fades() {
    // Create two independent publication animation states.
    let mut state_a = PublicationAnimationState::new(Some(0), None);
    let mut state_b = PublicationAnimationState::new(Some(0), None);

    // Both TTLs expired.
    state_a.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    state_b.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);

    state_a.tick();
    state_b.tick();

    // Simulate: state_a is 75ms into fade, state_b is 120ms into fade.
    state_a.fade_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(75));
    state_b.fade_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(120));

    let opacity_a = state_a.current_opacity();
    let opacity_b = state_b.current_opacity();

    // state_a at ~75ms → opacity ≈ 0.5.
    assert!(
        (opacity_a - 0.5).abs() < 0.15,
        "state_a at 75ms must have opacity ≈ 0.5, got {opacity_a}"
    );
    // state_b at ~120ms → opacity ≈ 0.2.
    assert!(
        opacity_b < 0.35,
        "state_b at 120ms must have opacity < 0.35, got {opacity_b}"
    );
    // They are independent — neither affects the other.
    assert!(
        opacity_a > opacity_b,
        "state_a (75ms) must be more opaque than state_b (120ms)"
    );
    assert!(
        !state_a.is_fade_complete(),
        "state_a (75ms into 150ms fade) must not be complete"
    );
    assert!(
        !state_b.is_fade_complete(),
        "state_b (120ms into 150ms fade) must not be complete"
    );
}

/// Stack reflow: after a publication is pruned, the remaining slot positions
/// are recalculated correctly in collect_text_items.
///
/// AC: remaining notifications reflow to fill vacated slot instantly.
#[tokio::test]
async fn test_stack_reflow_after_publication_pruned() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    // Zone at x=0, y=0 with font_size 16px default → slot_h = line_height(22.4) + 2*8 + 4 = 42.4px.
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "reflow test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 1.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy {
            backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    // Publish three notifications from three agents.
    for (agent, text) in [
        ("agent-a", "Alpha"),
        ("agent-b", "Beta"),
        ("agent-c", "Gamma"),
    ] {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: text.to_owned(),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    title: String::new(),
                    actions: Vec::new(),
                }),
                agent,
                None,
                None,
                None,
            )
            .unwrap();
    }

    // With 3 publications, newest (Gamma) at slot 0, oldest (Alpha) at slot 2.
    let items_before = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(items_before.len(), 3, "must have 3 TextItems before prune");

    // Manually mark the oldest (agent-a / Alpha) as fade-complete.
    let publishes = scene
        .zone_registry
        .active_publishes
        .get("notification-area")
        .unwrap();
    let (a_wall_us, a_ns) = {
        let r = &publishes[0]; // agent-a is oldest (index 0)
        (r.published_at_wall_us, r.publisher_namespace.clone())
    };

    let mut completed_state = PublicationAnimationState::new(Some(0), None);
    completed_state.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    completed_state.tick();
    completed_state.fade_start =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(151));

    compositor
        .pub_animation_states
        .entry("notification-area".to_string())
        .or_default()
        .insert((a_wall_us, a_ns), completed_state);

    // Prune: removes agent-a.
    compositor.prune_faded_publications(&mut scene);

    // After prune: 2 publications remain (agent-b, agent-c).
    let remaining = scene
        .zone_registry
        .active_publishes
        .get("notification-area")
        .map(|v| v.len());
    assert_eq!(remaining, Some(2), "2 publications must remain after prune");

    // collect_text_items should now produce 2 TextItems correctly reflowed.
    let items_after = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        items_after.len(),
        2,
        "must have 2 TextItems after prune (reflow)"
    );

    // Newest (Gamma = agent-c) is at slot 0 (top, pixel_y = 9.0).
    // Oldest remaining (Beta = agent-b) is at slot 1.
    // slot_h = line_height(16*1.4) + 2*margin_v(8) + SLOT_BASELINE_GAP(4) = 42.4px.
    // Slot 1 starts at y=42.4, text at y=42.4+9=51.4.
    let gamma_item = items_after.iter().find(|i| &*i.text == "Gamma");
    let beta_item = items_after.iter().find(|i| &*i.text == "Beta");

    assert!(gamma_item.is_some(), "Gamma must be in remaining TextItems");
    assert!(beta_item.is_some(), "Beta must be in remaining TextItems");

    // Gamma is newest → slot 0 → pixel_y = 0 + 9 = 9.
    assert_eq!(
        gamma_item.unwrap().pixel_y,
        9.0,
        "Gamma (newest) must be at slot 0, pixel_y=9.0"
    );
    // Beta is oldest remaining → slot 1 → pixel_y = 42.4 + 9 = 51.4.
    assert_eq!(
        beta_item.unwrap().pixel_y,
        51.4,
        "Beta (oldest remaining) must be at slot 1, pixel_y=51.4"
    );
}

/// update_publication_animations creates fresh state for new publications.
#[test]
fn test_update_publication_animations_seeds_fresh_state() {
    // We test this without GPU by constructing the compositor state manually.
    // Use a SceneGraph with a Stack zone and one publication.
    use std::sync::Arc;
    use tze_hud_scene::clock::TestClock;

    let clock = Arc::new(TestClock::new(1_000)); // start at t=1000ms
    let mut scene = SceneGraph::new_with_clock(1280.0, 720.0, clock.clone());

    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "animation seed test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Hello".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: Some(3_000),
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    // Build a minimal compositor state just for the animation map test.
    // We can't construct a full headless Compositor without GPU, so we test
    // the helper methods directly.
    let publishes = scene
        .zone_registry
        .active_publishes
        .get("notification-area")
        .unwrap();
    let record = &publishes[0];

    // Urgency-derived expires_at_wall_us (urgency=1: now + 8 s) sets the delay:
    // 8_000 - NOTIFICATION_FADE_OUT_MS(150) = 7_850. The per-notification
    // ttl_ms=3_000 is superseded.
    let ttl = Compositor::publication_fade_delay_ms(record, record.published_at_wall_us);
    assert_eq!(
        ttl,
        Some(7_850),
        "fade delay must use urgency-derived expires_at_wall_us (8_000ms - 150ms fade = 7_850ms)"
    );

    // No expiry: held, not the zone auto_clear_ms.
    let record_no_ttl = ZonePublishRecord {
        lease_id: None,
        zone_name: "notification-area".to_string(),
        publisher_namespace: "agent-b".to_string(),
        content: ZoneContent::Notification(NotificationPayload {
            text: "No TTL".to_owned(),
            icon: String::new(),
            urgency: 0,
            ttl_ms: None,
            title: String::new(),
            actions: Vec::new(),
        }),
        published_at_wall_us: 2_000_000,
        merge_key: None,
        expires_at_wall_us: None,
        content_classification: None,
        breakpoints: Vec::new(),
    };
    let ttl_fallback = Compositor::publication_fade_delay_ms(&record_no_ttl, 2_000_000);
    assert_eq!(
        ttl_fallback, None,
        "a notification with no expiry is held, not faded at the zone auto_clear_ms"
    );
}
