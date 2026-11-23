use super::*;

// ── Notification text rendering ─────────────────────────────
//
// Notification Text Rendering:
//   - typography.body.size (16px default) font size
//   - color.text.primary text color
//   - left-aligned, 9px inset (8px padding + 1px border)
//   - bounded title/body bands visibly ellipsize incomplete text

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
        .find(|item| &*item.text == crate::renderer::notification_style::DISMISS_GLYPH)
        .expect("dismiss text item must exist");

    // Canonical space.notification padding; the × is a type.notification.title line-height square
    // plus a space.s gap: 320 - 16 - 16 - 24 - 8.
    assert_eq!(body_item.pixel_x, 16.0, "body text keeps left inset");
    assert_eq!(
        body_item.bounds_width, 256.0,
        "body width must reserve dismiss control space"
    );
    assert_eq!(
        dismiss_item.alignment,
        TextAlign::Center,
        "dismiss label must be centered in its button bounds"
    );
    assert!(
        dismiss_item.pixel_x >= 280.0,
        "dismiss label must sit near the right edge, got {}",
        dismiss_item.pixel_x
    );
    // Admin diagnostics retain the compositor's exact last-frame phase, not
    // publication age, chosen input state, or the time of the status request.
    let frame = std::time::Instant::now();
    compositor.zone_animation_states.insert(
        "notification-area".into(),
        ZoneAnimationState::fade_in_at(100, frame),
    );
    compositor.last_animation_frame_at = Some(frame);
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    assert_eq!(scene.overlay.notification_geometry[0].state, "entering");
    let drawn_bounds = scene.overlay.notification_geometry[0].bounds;
    compositor.last_animation_frame_at = Some(frame + std::time::Duration::from_millis(100));
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    assert_eq!(scene.overlay.notification_geometry[0].state, "shown");
    let record = &scene.zone_registry.active_for_zone("notification-area")[0];
    let id = record.publication_id;
    scene
        .overlay
        .notification_choices
        .insert(id, "chosen".into());
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    assert_eq!(
        scene.overlay.notification_geometry[0].state, "shown",
        "choice alone does not invent an exit"
    );
    let mut exit = PublicationAnimationState::new(None, None, 150, super::easing::Easing::Linear);
    exit.fade_start = Some(frame);
    compositor
        .pub_animation_states
        .entry("notification-area".into())
        .or_default()
        .insert(id, exit);
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    assert_eq!(scene.overlay.notification_geometry[0].state, "exiting");
    assert_eq!(scene.overlay.notification_geometry[0].bounds, drawn_bounds);
    assert!(
        scene.overlay.zone_hit_regions.is_empty(),
        "chosen exit remains observable without input"
    );
    compositor.degradation_policy.level = DegradationLevel::Simplified;
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    assert_eq!(
        scene.overlay.notification_geometry[0].state, "shown",
        "disabled fades report the actual fully shown frame"
    );
}

// ── Dismiss button typography token tests [, ] ─────
//
// The × glyph fills the `type.notification.title` line box at its weight: canonical
// defaults without tokens, theme/owner token values otherwise.

/// A body-only card uses the canonical `type.notification.body` size; the dismiss ×
/// reads `type.notification.title.line_height` / `.weight` (defaults and overrides).
#[tokio::test]
async fn test_dismiss_button_respects_typography_tokens() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut text_renderer_initialized = false;

    for (case, text, agent, x_pct, tokens, expected_dismiss) in [
        ("body-default", "Doorbell rang", "agent-a", 0.75, None, None),
        (
            "dismiss-default",
            "Default dismiss test",
            "agent-a",
            0.0,
            None,
            // The × fills the canonical type.notification.title line box (24 dp, 600).
            Some((24.0_f32, 600_u16)),
        ),
        (
            "dismiss-token-override",
            "Token override dismiss test",
            "agent-b",
            0.0,
            Some(("28", "700")),
            Some((28.0, 700)),
        ),
    ] {
        if expected_dismiss.is_some() && !text_renderer_initialized {
            compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
            text_renderer_initialized = true;
        }

        let mut token_map = HashMap::new();
        if let Some((line_height, weight)) = tokens {
            token_map.insert(
                "type.notification.title.line_height".to_string(),
                line_height.to_string(),
            );
            token_map.insert(
                "type.notification.title.weight".to_string(),
                weight.to_string(),
            );
        }
        compositor.set_token_map(token_map);

        let mut scene = SceneGraph::new(1280.0, 720.0);
        scene.register_zone(ZoneDefinition {
            id: SceneId::new(),
            name: "notification-area".to_owned(),
            description: case.to_owned(),
            geometry_policy: GeometryPolicy::Relative {
                x_pct,
                y_pct: 0.0,
                width_pct: 0.25,
                height_pct: 0.5,
            },
            accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
            rendering_policy: RenderingPolicy {
                backdrop: Some(Rgba::new(0.1, 0.1, 0.1, 0.9)),
                // No explicit font size: the body role's canonical 14 dp.
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

        let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
        let body_item = items
            .iter()
            .find(|item| &*item.text == text)
            .unwrap_or_else(|| panic!("{case}: notification body text item must exist"));
        assert_eq!(
            body_item.font_size_px, 14.0,
            "{case}: a body-only card uses the canonical type.notification.body size"
        );

        if let Some((size, weight)) = expected_dismiss {
            let dismiss_item = items
                .iter()
                .find(|item| &*item.text == crate::renderer::notification_style::DISMISS_GLYPH)
                .unwrap_or_else(|| panic!("{case}: dismiss text item must exist"));
            assert_eq!(
                dismiss_item.font_size_px, size,
                "{case}: dismiss font_size_px"
            );
            assert_eq!(
                dismiss_item.font_weight, weight,
                "{case}: dismiss font_weight"
            );
        } else {
            assert_eq!(
                items.len(),
                1,
                "{case}: must produce one notification TextItem"
            );
        }
    }
}

/// Notification text is inset by the card padding (`space.l`) from the
/// backdrop edges, in its role's token line height.
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
    // Zone starts at x=0, y=0. Text sits the card padding (canonical
    // space.l, 16 dp) in from the backdrop edges, in the body role's line box.
    let style = crate::renderer::notification_style::NotificationStyle::resolve(
        &HashMap::new(),
        &scene.zone_registry.zones["notification-area"].rendering_policy,
    );
    assert_eq!(style.padding_h, 16.0);
    assert_eq!(
        item.pixel_x, style.padding_h,
        "text pixel_x is the card padding"
    );
    assert_eq!(
        item.pixel_y, style.padding_v,
        "text pixel_y is the card padding"
    );
    assert_eq!(item.bounds_height, style.body.line_height);
    assert!(
        (item.line_height_multiplier * item.font_size_px - style.body.line_height).abs() < 1e-4,
        "the line box is type.notification.body.line_height, not a fixed multiplier"
    );
    // Text is left-aligned.
    assert_eq!(
        item.alignment,
        TextAlign::Start,
        "notification text must be left-aligned (TextAlign::Start)"
    );
    // Overflow is visible while the original content bounds stay unchanged.
    assert_eq!(
        item.overflow,
        TextOverflow::Ellipsis,
        "notification text must visibly mark overflow inside its bounded line"
    );
}

// ── Two-line notification rendering ──────────────────────────
//
// Two-line notification layout:
//   - Empty title → single-line backward-compatible path (1 TextItem)
//   - Non-empty title → two-line path (2 TextItems: title bold, body regular)
//   - Card title: type.notification.title (size scaled by an explicit policy size,
//     weight 600); body: type.notification.body (14/16 of the title, weight 400)
//   - Body positioned directly below the title's line box

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
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

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
    let style = crate::renderer::notification_style::NotificationStyle::resolve(
        &HashMap::new(),
        &scene.zone_registry.zones["notification-area"].rendering_policy,
    );
    assert_eq!(
        title_item.font_weight, style.title.weight,
        "title must use the type.notification.title weight ({}), got {}",
        style.title.weight, title_item.font_weight
    );
    assert_eq!(
        title_item.font_weight, 600,
        "canonical type.notification.title.weight"
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
    let expected_body_size = style.body.size;
    assert!(
        (body_item.font_size_px - 14.0).abs() < 0.1,
        "body font size must be type.notification.body ({expected_body_size:.2}), got {}",
        body_item.font_size_px
    );
    assert_eq!(
        body_item.pixel_y,
        title_item.pixel_y + style.title.line_height,
        "the body line box follows the title's"
    );

    // Body must be below title.
    assert!(
        body_item.pixel_y > title_item.pixel_y,
        "body item must be below title: title_y={}, body_y={}",
        title_item.pixel_y,
        body_item.pixel_y
    );

    // The owner's admitted instruction must be visibly incomplete in the
    // bounded body line, rather than silently losing "title changes.".
    let owner_body =
        "Held-input check: press and hold Hold here; keep holding until the title changes.";
    let template = scene.zone_registry.zones["notification-area"].clone();
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    // hud-ljhku.5: admission-time fit must agree with what this draws.
    let fit = std::sync::Arc::new(super::super::TextFitMeasurer::new());
    compositor.attach_text_fit(std::sync::Arc::clone(&fit));
    for scale in [1.0, 1.5] {
        compositor.set_token_map(HashMap::from([
            ("font.sans".into(), "IBM Plex Sans".into()),
            ("font.mono".into(), "IBM Plex Mono".into()),
            (
                tze_hud_config::tokens::PX_SCALE_KEY.into(),
                scale.to_string(),
            ),
        ]));
        for width in [320.0, 640.0] {
            for count in [2, 0, 4] {
                for (title, body, overflows) in [
                    (
                        "Codex · Target · Before".to_owned(),
                        owner_body.to_owned(),
                        true,
                    ),
                    (String::new(), "Ready".to_owned(), false),
                    (
                        "確認してください — résumé — 完了まで待機してください".repeat(4),
                        "待機 café 👩‍💻 — ".repeat(16),
                        true,
                    ),
                ] {
                    let truncated = overflows && (body != owner_body || width == 320.0);
                    let mut scene = SceneGraph::new(1280.0, 720.0);
                    let mut zone = template.clone();
                    zone.geometry_policy = GeometryPolicy::Relative {
                        x_pct: 0.0,
                        y_pct: 0.0,
                        width_pct: width * scale / 1280.0,
                        height_pct: 1.0,
                    };
                    zone.rendering_policy.font_size_px = Some(16.0 * scale);
                    scene.register_zone(zone);
                    let labels = ["Hold here", "Leave alone", "Skip", "Open"];
                    for payload in [
                        NotificationPayload {
                            title: "Next".into(),
                            text: "Following card".into(),
                            ..Default::default()
                        },
                        NotificationPayload {
                            title: title.clone(),
                            text: body.clone(),
                            actions: labels[..count]
                                .iter()
                                .map(|label| NotificationAction {
                                    label: (*label).into(),
                                    callback_id: Some((*label).into()),
                                    url: None,
                                    role: None,
                                })
                                .collect(),
                            ..Default::default()
                        },
                    ] {
                        scene
                            .publish_to_zone(
                                "notification-area",
                                ZoneContent::Notification(payload),
                                "test-agent",
                                None,
                                None,
                                None,
                            )
                            .unwrap();
                    }
                    // Production commits prime truncation before the draw pass
                    // resolves notification geometry with the live font system.
                    compositor.prime_truncation_cache(&scene);
                    let commit_shapes = compositor.notification_layouts.borrow().shapes;
                    compositor.prime_notification_layouts(&scene, 1280.0, 720.0);
                    assert_eq!(
                        compositor.notification_layouts.borrow().shapes,
                        commit_shapes,
                        "commit prime must resolve live-font geometry before drawing"
                    );
                    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
                    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
                    let body_item = items
                        .iter()
                        .find(|i| i.text.as_ref() == body.as_str())
                        .unwrap();
                    let raw_width = compositor
                        .text_rasterizer
                        .as_mut()
                        .unwrap()
                        .measure_single_line(
                            &body_item.text,
                            body_item.font_size_px,
                            body_item.font_family,
                            body_item.font_weight,
                        );
                    assert_eq!(raw_width > body_item.bounds_width, truncated);
                    eprintln!(
                        "notification overflow: scale={scale} actions={count} raw_width={raw_width} bounds_width={} bounds_height={}",
                        body_item.bounds_width, body_item.bounds_height
                    );
                    let key = crate::text::effective_truncation_key(body_item)
                        .expect("notification body must participate in visible ellipsis");
                    let rasterizer = compositor.text_rasterizer.as_mut().unwrap();
                    let result = rasterizer
                        .truncation_cache
                        .get_by_key(&key)
                        .expect("actual frame geometry must be primed before drawing")
                        .clone();
                    assert_eq!(result.was_truncated, truncated, "{scale}/{count}/{body}");
                    let ZoneContent::Notification(published) = &scene
                        .zone_registry
                        .active_for_zone("notification-area")
                        .last()
                        .unwrap()
                        .content
                    else {
                        unreachable!("notification fixture")
                    };
                    let clipped = tze_hud_scene::TextFit::notification(
                        &*fit,
                        &scene,
                        "notification-area",
                        published,
                    );
                    let reported =
                        |field: &str| clipped.iter().find(|(f, _)| *f == field).map(|(_, n)| *n);
                    // The reported count is what this draws.
                    assert_eq!(
                        reported("body"),
                        truncated.then(|| result.drawn_chars(&body)),
                        "fit agrees with drawing: {scale}/{width}/{count}/{clipped:?}"
                    );
                    assert_eq!(
                        reported("title").is_some(),
                        title.starts_with('確'),
                        "fit agrees with drawing: {scale}/{width}/{count}/{clipped:?}"
                    );
                    if truncated {
                        assert!(result.text.ends_with(crate::overflow::ELLIPSIS));
                        assert!(!result.text.is_empty());
                    } else {
                        assert_eq!(result.text, body);
                    }
                    assert!(
                        rasterizer.measure_single_line(
                            &result.text,
                            body_item.font_size_px,
                            body_item.font_family,
                            body_item.font_weight
                        ) <= body_item.bounds_width + 0.01
                    );
                    if !title.is_empty() {
                        let item = items
                            .iter()
                            .find(|i| i.text.as_ref() == title.as_str())
                            .unwrap();
                        let key = crate::text::effective_truncation_key(item).unwrap();
                        let result = rasterizer
                            .truncation_cache
                            .get_by_key(&key)
                            .unwrap()
                            .clone();
                        assert_eq!(result.was_truncated, title.starts_with('確'));
                        assert_eq!(
                            reported("title"),
                            result.was_truncated.then(|| result.drawn_chars(&title))
                        );
                        assert!(
                            rasterizer.measure_single_line(
                                &result.text,
                                item.font_size_px,
                                item.font_family,
                                item.font_weight
                            ) <= item.bounds_width + 0.01
                        );
                        assert!(item.pixel_y + item.bounds_height <= body_item.pixel_y);
                    }
                    let records = scene.zone_registry.active_for_zone("notification-area");
                    let policy = &scene.zone_registry.zones["notification-area"].rendering_policy;
                    let layout = compositor.zone_slot_layout(
                        &scene,
                        "notification-area",
                        records,
                        policy,
                        720.0,
                        width * scale,
                    );
                    assert!(layout.slot_offsets[1] >= layout.slot_heights[0]);
                    let following = items.iter().find(|i| &*i.text == "Following card").unwrap();
                    assert!(body_item.pixel_y + body_item.bounds_height <= following.pixel_y);
                    let actions: Vec<_> = scene
                        .overlay
                        .zone_hit_regions
                        .iter()
                        .filter(|r| {
                            r.publication_id == records[1].publication_id
                                && matches!(r.kind, ZoneInteractionKind::Action { .. })
                        })
                        .collect();
                    assert_eq!(actions.len(), count);
                    for (action, label) in actions.iter().zip(labels) {
                        let item = items.iter().find(|i| &*i.text == label).unwrap();
                        assert!(body_item.pixel_y + body_item.bounds_height <= action.bounds.y);
                        assert!(
                            action.bounds.x >= 0.0
                                && action.bounds.x + action.bounds.width <= width * scale
                        );
                        assert!(action.bounds.y + action.bounds.height <= layout.slot_heights[0]);
                        assert!(item.pixel_x >= action.bounds.x && item.pixel_y >= action.bounds.y);
                        assert!(
                            item.pixel_x + item.clip_bounds_width
                                <= action.bounds.x + action.bounds.width
                        );
                        assert!(
                            item.pixel_y + item.clip_bounds_height
                                <= action.bounds.y + action.bounds.height
                        );
                    }
                    let shapes = compositor.notification_layouts.borrow().shapes;
                    let rasterizer = compositor.text_rasterizer.as_mut().unwrap();
                    rasterizer.update_viewport(&compositor.queue, 1280, 720);
                    rasterizer
                        .prepare_text_items(&compositor.device, &compositor.queue, &items)
                        .unwrap();
                    let shaped = rasterizer.shape_call_count;
                    let cached = rasterizer.truncation_cache.len();
                    rasterizer
                        .prepare_text_items(&compositor.device, &compositor.queue, &items)
                        .unwrap();
                    assert_eq!(
                        rasterizer.shape_call_count, shaped,
                        "unchanged labels reuse shaped buffers"
                    );
                    compositor.prime_truncation_cache(&scene);
                    let _ = compositor.collect_text_items(&scene, 1280.0, 720.0);
                    assert_eq!(
                        compositor
                            .text_rasterizer
                            .as_ref()
                            .unwrap()
                            .truncation_cache
                            .len(),
                        cached
                    );
                    assert_eq!(compositor.notification_layouts.borrow().shapes, shapes);
                }
            }
        }
    }
}

/// Two-line height: a card's or banner's text block is taller with a title
/// than with a lone line; a lone line takes the body role on a card and the
/// title role on an alert banner.
#[test]
fn test_notification_text_block_two_line_exceeds_single_line() {
    let policy = RenderingPolicy::default();
    let tokens = HashMap::new();
    for zone in ["notification-area", "alert-banner"] {
        let style = NotificationStyle::for_zone(zone, &tokens, &policy);
        let (single, two) = (
            style.text_block_height(false),
            style.text_block_height(true),
        );
        assert!(two > single, "{zone}: two-line {two} must exceed {single}");
        assert_eq!(
            single,
            2.0 * style.padding_v + style.lone.line_height,
            "{zone}"
        );
    }
    let card = NotificationStyle::for_zone("notification-area", &tokens, &policy);
    let banner = NotificationStyle::for_zone("alert-banner", &tokens, &policy);
    assert_eq!(card.lone, card.body);
    assert_eq!(banner.lone, banner.title);
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
    let before = compositor.zone_slot_layout(
        &scene,
        "notification-area",
        scene.zone_registry.active_for_zone("notification-area"),
        &scene.zone_registry.zones["notification-area"].rendering_policy,
        648.0,
        640.0,
    );
    if let ZoneContent::Notification(payload) = &mut scene
        .zone_registry
        .active_publishes
        .get_mut("notification-area")
        .unwrap()[1]
        .content
    {
        payload.actions = [
            "Proceed only after checking the entire deployment and its rollback plan",
            "Hold",
            "Skip",
            "Open",
        ]
        .into_iter()
        .map(|label| NotificationAction {
            label: label.into(),
            callback_id: Some(label.into()),
            url: None,
            role: None,
        })
        .collect();
    }
    let after = compositor.zone_slot_layout(
        &scene,
        "notification-area",
        scene.zone_registry.active_for_zone("notification-area"),
        &scene.zone_registry.zones["notification-area"].rendering_policy,
        648.0,
        640.0,
    );
    assert!(after.slot_heights[0] > before.slot_heights[0]);
    assert!(after.slot_offsets[1] > before.slot_offsets[1]);
    assert_eq!(
        after.slot_heights[1], before.slot_heights[1],
        "following text height does not oscillate"
    );
}

/// Rounded notification backdrops must use per-notification two-line slot heights.
///
/// Regression guard: when `backdrop_radius` is enabled, backdrop geometry comes from
/// `collect_all_rounded_rect_cmds()` (not the flat-rect path). For two-line notifications,
/// the rounded backdrop height must match the card style's text block, otherwise body
/// text can extend outside the card.
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
        1,
        "single rounded notification: one card cmd (an idle × draws nothing)"
    );

    // padding + title line + body line + padding, all token-derived.
    let expected_h =
        crate::renderer::notification_style::NotificationStyle::resolve(&HashMap::new(), &policy)
            .text_block_height(true);
    assert_eq!(expected_h, 76.0, "16 + 24 + 20 + 16 (canonical)");
    let actual_h = rr.chrome[0].height;
    assert!(
        (actual_h - expected_h).abs() < 0.1,
        "rounded notification backdrop height must match two-line slot height: expected {expected_h:.2}, got {actual_h:.2}"
    );
}

// ── TTL auto-dismiss ────────────────────────────────────────
//
// Notification TTL Auto-Dismiss with Fade-Out:
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
    let state =
        PublicationAnimationState::new(Some(10_000), None, 150, super::easing::Easing::Linear);
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
    let now = std::time::Instant::now();
    let mut state = PublicationAnimationState::new_at(
        Some(3_000),
        None,
        150,
        super::easing::Easing::Linear,
        now,
    );
    state.tick_at(now + std::time::Duration::from_millis(2_999));
    assert_eq!(state.fade_start, None, "not before the fade deadline");
    assert_eq!(
        state.fade_start_deadline(),
        Some(now + std::time::Duration::from_millis(3_000))
    );
    let fade_at = now + std::time::Duration::from_millis(3_000);
    state.tick_at(fade_at);

    assert!(
        state.fade_start.is_some(),
        "fade must start after TTL (3000ms) has elapsed"
    );
    assert_eq!(state.fade_start, Some(fade_at));
    state.tick_at(fade_at + std::time::Duration::from_millis(1));
    assert_eq!(state.fade_start, Some(fade_at), "ticking is idempotent");
    assert_eq!(state.fade_start_deadline(), None);
}

/// PublicationAnimationState: at 75ms into the 150ms fade, opacity ≈ 0.5.
///
/// AC: opacity interpolates linearly; at midpoint it must be approximately 0.5.
#[test]
fn test_pub_anim_state_opacity_at_75ms_midpoint_is_half() {
    let now = std::time::Instant::now();
    let mut state = PublicationAnimationState::new_at(
        Some(0), // zero fade delay, unlike a held scene record's None
        None,
        150,
        super::easing::Easing::Linear,
        now,
    );
    state.tick_at(now);
    assert_eq!(state.current_opacity_at(now), 1.0);
    let opacity = state.current_opacity_at(now + std::time::Duration::from_millis(75));
    assert!(
        (opacity - 0.5).abs() < 0.1,
        "at 75ms midpoint, opacity must be ≈ 0.5, got {opacity}"
    );
    assert!(!state.is_fade_complete_at(now + std::time::Duration::from_millis(149)));
    let end = now + std::time::Duration::from_millis(150);
    assert!(state.is_fade_complete_at(end));
    assert_eq!(state.current_opacity_at(end), 0.0);
    assert_eq!(
        state.current_opacity_at(now - std::time::Duration::from_millis(1)),
        1.0,
        "an earlier explicit sample saturates at the start"
    );
}

/// PublicationAnimationState: after 150ms, is_fade_complete returns true.
///
/// AC: publication must be removed from active_publishes when fade completes.
#[test]
fn test_pub_anim_state_is_complete_after_150ms() {
    let mut state =
        PublicationAnimationState::new(Some(0), None, 150, super::easing::Easing::Linear);

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
    let a_id = {
        let r = &publishes[0]; // agent-a is first (oldest)
        r.publication_id
    };

    let mut completed_state =
        PublicationAnimationState::new(Some(0), None, 150, super::easing::Easing::Linear);
    completed_state.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    completed_state.tick(); // starts fade
    // Set fade_start 151ms in the past → fade complete.
    completed_state.fade_start =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(151));

    compositor
        .pub_animation_states
        .entry("notification-area".to_string())
        .or_default()
        .insert(a_id, completed_state);

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
    let mut state_a =
        PublicationAnimationState::new(Some(0), None, 150, super::easing::Easing::Linear);
    let mut state_b =
        PublicationAnimationState::new(Some(0), None, 150, super::easing::Easing::Linear);

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

    let mut scene = SceneGraph::new_with_clock(
        1280.0,
        720.0,
        std::sync::Arc::new(tze_hud_scene::TestClock::new(1_000)),
    );
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
    let a_id = {
        let r = &publishes[0]; // agent-a is oldest (index 0)
        r.publication_id
    };

    let mut completed_state =
        PublicationAnimationState::new(Some(0), None, 150, super::easing::Easing::Linear);
    completed_state.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(1);
    completed_state.tick();
    completed_state.fade_start =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(151));

    compositor
        .pub_animation_states
        .entry("notification-area".to_string())
        .or_default()
        .insert(a_id, completed_state);

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

    // Newest (Gamma = agent-c) is at slot 0, oldest remaining (Beta) at
    // slot 1. Body-only cards (canonical tokens): slot_h = space.l + body
    // line (20) + space.l = 52; slot 1 starts after the space.s (8) stack gap.
    let gamma_item = items_after.iter().find(|i| &*i.text == "Gamma");
    let beta_item = items_after.iter().find(|i| &*i.text == "Beta");

    assert!(gamma_item.is_some(), "Gamma must be in remaining TextItems");
    assert!(beta_item.is_some(), "Beta must be in remaining TextItems");

    assert_eq!(
        gamma_item.unwrap().pixel_y,
        16.0,
        "Gamma (newest) must be at slot 0, pixel_y=16"
    );
    // Beta → slot 1 → pixel_y = 52 + 8 + 16 = 76.
    assert_eq!(
        beta_item.unwrap().pixel_y,
        76.0,
        "Beta (oldest remaining) must be at slot 1, pixel_y=76"
    );
    let mut payload = NotificationPayload {
        text: "Choose next".into(),
        ..Default::default()
    };
    payload.actions = [
        "Wait until all dependent service checks have finished successfully",
        "Retry",
        "Open",
        "Skip",
    ]
    .into_iter()
    .map(|label| NotificationAction {
        label: label.into(),
        callback_id: Some(label.into()),
        url: None,
        role: None,
    })
    .collect();
    scene
        .zone_registry
        .zones
        .get_mut("notification-area")
        .unwrap()
        .geometry_policy = GeometryPolicy::Relative {
        x_pct: 0.0,
        y_pct: 0.0,
        width_pct: 0.25,
        height_pct: 1.0,
    };
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(payload),
            "choice-agent",
            None,
            None,
            None,
        )
        .unwrap();
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let before = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let gamma_before = before
        .iter()
        .find(|item| &*item.text == "Gamma")
        .unwrap()
        .pixel_y;
    let newest = scene
        .zone_registry
        .active_for_zone("notification-area")
        .last()
        .unwrap()
        .clone();
    let policy = &scene.zone_registry.zones["notification-area"].rendering_policy;
    assert!(
        compositor
            .notification_action_layout("notification-area", &newest, policy, 320.0)
            .stacked
    );
    scene.dismiss_notification("notification-area", newest.publication_id, "choice-agent");
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let after = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert!(
        gamma_before
            > after
                .iter()
                .find(|item| &*item.text == "Gamma")
                .unwrap()
                .pixel_y
    );

    // Equal-time cards from one publisher retain separate shaped descriptors
    // and fade state. Different labels would thrash a timestamp-keyed cache.
    scene.clear_zone("notification-area").unwrap();
    for long in [false, true] {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: if long { "Long" } else { "Short" }.into(),
                    actions: ["Ship", "Hold", "Skip", "Open"]
                        .into_iter()
                        .map(|label| NotificationAction {
                            label: if long {
                                label.repeat(100)
                            } else {
                                label.into()
                            },
                            callback_id: Some(label.into()),
                            url: None,
                            role: None,
                        })
                        .collect(),
                    ..Default::default()
                }),
                "choice-agent",
                None,
                None,
                None,
            )
            .unwrap();
    }
    let records = scene
        .zone_registry
        .active_for_zone("notification-area")
        .to_vec();
    assert_eq!(
        records[0].published_at_wall_us,
        records[1].published_at_wall_us
    );
    let policy = &scene.zone_registry.zones["notification-area"].rendering_policy;
    // The mock's 368 dp card fits four short actions in one row.
    assert!(
        !compositor
            .notification_action_layout("notification-area", &records[0], policy, 368.0)
            .stacked
    );
    assert!(
        compositor
            .notification_action_layout("notification-area", &records[1], policy, 368.0)
            .stacked
    );
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let _ = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let shapes = compositor.notification_layouts.borrow().shapes;
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let _ = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(
        compositor.notification_layouts.borrow().shapes,
        shapes,
        "unchanged same-time cards reuse both layouts"
    );
    let action = scene
        .overlay
        .zone_hit_regions
        .iter()
        .find(|r| {
            r.publication_id == records[1].publication_id
                && matches!(r.kind, ZoneInteractionKind::Action { .. })
        })
        .unwrap()
        .clone();
    assert!(scene.activate_notification(&action, 240).is_some());
    compositor.update_publication_animations(&scene);
    let states = compositor
        .pub_animation_states
        .get_mut("notification-area")
        .unwrap();
    assert_eq!(states.len(), 2, "same tick still means independent fades");
    assert_ne!(
        states[&records[0].publication_id].source_expiry_us,
        states[&records[1].publication_id].source_expiry_us
    );
    states
        .get_mut(&records[1].publication_id)
        .unwrap()
        .fade_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(241));
    compositor.prune_faded_publications(&mut scene);
    assert_eq!(
        scene.zone_registry.active_for_zone("notification-area")[0].publication_id,
        records[0].publication_id
    );
    assert_eq!(
        scene
            .zone_registry
            .active_for_zone("notification-area")
            .len(),
        1
    );
    assert!(scene.overlay.notification_choices.is_empty());
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    assert!(
        scene
            .overlay
            .zone_hit_regions
            .iter()
            .all(|r| r.publication_id == records[0].publication_id)
    );
    let _ = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert_eq!(compositor.notification_layouts.borrow().shapes, shapes);

    // Actual zone enter state survives in-place keyed replacement; fade state
    // belongs to the new exact publication and its fresh absolute expiry.
    let mut zone = scene.zone_registry.zones["notification-area"].clone();
    zone.id = SceneId::new();
    zone.name = "keyed-animation".into();
    zone.rendering_policy.transition_in_ms = Some(200);
    scene.register_zone(zone);
    let content = ZoneContent::Notification(NotificationPayload {
        text: "Before".into(),
        ..Default::default()
    });
    let now_us = scene.now_wall_us();
    scene
        .publish_to_zone(
            "keyed-animation",
            content.clone(),
            "a",
            Some("slot".into()),
            Some(now_us + 2_000_000),
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "keyed-animation",
            content,
            "b",
            Some("slot".into()),
            Some(now_us + 8_000_000),
            None,
        )
        .unwrap();
    let before = scene
        .zone_registry
        .active_for_zone("keyed-animation")
        .to_vec();
    let start = std::time::Instant::now();
    compositor.update_zone_animations_at(&scene, start);
    compositor.update_publication_animations_at(&scene, start);
    let enter = compositor.zone_animation_states["keyed-animation"].transition_start;
    scene
        .publish_to_zone(
            "keyed-animation",
            ZoneContent::Notification(NotificationPayload {
                title: "After".into(),
                text: "Changed body with a new layout".into(),
                ..Default::default()
            }),
            "a",
            Some("slot".into()),
            Some(now_us + 5_000_000),
            None,
        )
        .unwrap();
    let later = start + std::time::Duration::from_millis(80);
    compositor.update_zone_animations_at(&scene, later);
    compositor.update_publication_animations_at(&scene, later);
    assert_eq!(
        compositor.zone_animation_states["keyed-animation"].transition_start,
        enter
    );
    let records = scene.zone_registry.active_for_zone("keyed-animation");
    assert_eq!(records[1], before[1]);
    assert_ne!(records[0].publication_id, before[0].publication_id);
    let fades = &compositor.pub_animation_states["keyed-animation"];
    assert!(!fades.contains_key(&before[0].publication_id));
    assert_eq!(
        fades[&records[0].publication_id].source_expiry_us,
        Some(now_us + 5_000_000)
    );
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert!(items.iter().any(|i| i.text.contains("Changed body")));
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
    let ttl = Compositor::publication_fade_delay_ms(record, record.published_at_wall_us, 150);
    assert_eq!(
        ttl,
        Some(7_850),
        "fade delay must use urgency-derived expires_at_wall_us (8_000ms - 150ms fade = 7_850ms)"
    );

    // No expiry: held, not the zone auto_clear_ms.
    let record_no_ttl = ZonePublishRecord {
        publication_id: SceneId::new(),
        lease_id: None,
        publication_origin: None,
        caption: None,
        window_token: None,
        display: None,
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
    let ttl_fallback = Compositor::publication_fade_delay_ms(&record_no_ttl, 2_000_000, 150);
    assert_eq!(
        ttl_fallback, None,
        "a notification with no expiry is held, not faded at the zone auto_clear_ms"
    );
    let profile = super::easing::ZoneMotion::from_token_map(&HashMap::from([
        ("motion.exit.ms".into(), "240".into()),
        ("motion.exit.easing".into(), "accelerate".into()),
    ]));
    let mut state = PublicationAnimationState::new(
        Compositor::publication_fade_delay_ms(record, record.published_at_wall_us, profile.exit_ms),
        record.expires_at_wall_us,
        profile.exit_ms,
        profile.exit,
    );
    assert_eq!(state.ttl_ms, Some(7_760));
    assert_eq!(state.fade_duration_ms, 240);
    assert_eq!(state.easing, super::easing::Easing::EaseInQuad);
    state.retarget(None, None);
    assert_eq!(state.ttl_ms, None);
    assert_eq!(
        state.fade_duration_ms, 240,
        "hold preserves the captured profile"
    );
    assert_eq!(state.current_opacity(), 1.0);
}

/// A session's card draws its label as one secondary line above the title;
/// the title, body and dismiss move down by that line and the slot grows by
/// it. An unlabeled publisher's card is unchanged.
#[tokio::test]
async fn test_session_caption_takes_one_secondary_line_above_the_title() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "caption test".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.5,
            height_pct: 0.9,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    let card = |title: &str| {
        ZoneContent::Notification(NotificationPayload {
            title: title.to_owned(),
            text: "Body".to_owned(),
            icon: String::new(),
            urgency: 1,
            ttl_ms: None,
            actions: vec![],
        })
    };
    let session = tze_hud_scene::scope::scoped_namespace("claude", "s1");
    scene.set_session_label(&session, Some("tze-hud".to_owned()));
    scene
        .publish_to_zone(
            "notification-area",
            card("Labeled"),
            &session,
            None,
            None,
            None,
        )
        .unwrap();
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let at = |text: &str| items.iter().find(|i| &*i.text == text).unwrap().clone();
    let (caption, title, body) = (at("tze-hud"), at("Labeled"), at("Body"));
    let style = crate::renderer::notification_style::NotificationStyle::resolve(
        &HashMap::new(),
        &RenderingPolicy::default(),
    );
    assert_eq!(caption.font_weight, style.body.weight);
    assert_eq!(
        caption.color, body.color,
        "the caption takes the secondary role"
    );
    assert_eq!(caption.pixel_x, title.pixel_x, "one text column");
    assert!(
        (title.pixel_y - caption.pixel_y - style.body.line_height).abs() < 0.5,
        "the title sits one body line below the caption"
    );

    // Unlabeled: the same card has no caption and starts a line higher.
    let mut plain = SceneGraph::new(1280.0, 720.0);
    plain.register_zone(scene.zone_registry.zones["notification-area"].clone());
    plain
        .publish_to_zone(
            "notification-area",
            card("Labeled"),
            "claude",
            None,
            None,
            None,
        )
        .unwrap();
    let plain_items = compositor.collect_text_items(&plain, 1280.0, 720.0);
    assert_eq!(plain_items.len(), 2, "no caption without a label");
    let plain_title = plain_items.iter().find(|i| &*i.text == "Labeled").unwrap();
    assert!((title.pixel_y - plain_title.pixel_y - style.body.line_height).abs() < 0.5);

    // The label is fixed at publish: a later relabel leaves this card's caption.
    scene.set_session_label(&session, Some("renamed".to_owned()));
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert!(items.iter().any(|i| &*i.text == "tze-hud"));
}

#[tokio::test]
async fn go_to_uses_existing_caption_geometry_and_draws_local_cues() {
    use tze_hud_scene::graph::overlay::{GoToCue, NotificationGoTo};
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.set_token_map(HashMap::from([("motion.enter.ms".into(), "0".into())]));
    let mut scene =
        SceneGraph::new_with_clock(1280.0, 720.0, Arc::new(tze_hud_scene::TestClock::new(1000)));
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".into(),
        description: "session caption control".into(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.1,
            y_pct: 0.1,
            width_pct: 0.45,
            height_pct: 0.7,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 2 },
        max_publishers: 2,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    let session = tze_hud_scene::scope::scoped_namespace("claude", "s1");
    scene.set_scoped_session_label(
        &session,
        Some(tze_hud_scene::scope::SessionLabel {
            caption: "test folder".into(),
            window_token: Some("test folder".into()),
        }),
    );
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                title: "Question".into(),
                text: "Body".into(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                actions: vec![],
            }),
            &session,
            None,
            None,
            None,
        )
        .unwrap();

    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let baseline = scene.overlay.notification_geometry[0].clone();
    assert!(baseline.has_window_caption);
    assert!(baseline.goto.is_none());
    compositor.render_frame_headless(&mut scene, &surface);
    let plain_pixels = surface.read_pixels(&compositor.device);

    scene.overlay.notification_go_to.insert(
        baseline.publication_id,
        NotificationGoTo {
            available: true,
            ..Default::default()
        },
    );
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let card = &scene.overlay.notification_geometry[0];
    assert_eq!(card.bounds, baseline.bounds);
    assert_eq!(card.close, baseline.close);
    assert_eq!(card.actions, baseline.actions);
    let bounds = card.goto.unwrap();
    let control = scene
        .overlay
        .zone_hit_regions
        .iter()
        .find(|hit| hit.kind == tze_hud_scene::ZoneInteractionKind::GoTo)
        .unwrap()
        .clone();
    assert_eq!(control.bounds, bounds);
    let pixels_in = |pixels: &[u8]| {
        let left = bounds.x.ceil() as usize;
        let right = (bounds.x + bounds.width).floor() as usize;
        let top = bounds.y.ceil() as usize;
        let bottom = (bounds.y + bounds.height).floor() as usize;
        (top..bottom)
            .flat_map(|y| pixels[(y * 1280 + left) * 4..(y * 1280 + right) * 4].to_vec())
            .collect::<Vec<_>>()
    };
    let mut previous_pixels = pixels_in(&plain_pixels);
    for (cue, label) in [
        (None, "Go to"),
        (Some(GoToCue::NotFound), "Window not found"),
        (Some(GoToCue::Ambiguous(2)), "2 windows match"),
        (Some(GoToCue::Blocked), "Windows blocked the switch"),
    ] {
        scene
            .overlay
            .notification_go_to
            .get_mut(&baseline.publication_id)
            .unwrap()
            .cue = cue;
        let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
        let goto = items.iter().find(|item| &*item.text == label).unwrap();
        let caption = items
            .iter()
            .find(|item| &*item.text == "test folder")
            .unwrap();
        assert_eq!(goto.pixel_y, caption.pixel_y);
        assert_eq!(goto.color, caption.color);
        assert_eq!(goto.bounds_width, bounds.width);
        compositor.prime_truncation_cache(&scene);
        compositor.render_frame_headless(&mut scene, &surface);
        let pixels = pixels_in(&surface.read_pixels(&compositor.device));
        assert_ne!(
            pixels, previous_pixels,
            "{label} reaches the caption-row pixels"
        );
        previous_pixels = pixels;
    }
    scene.set_chrome_hovered(&control.interaction_id, true);
    compositor.render_frame_headless(&mut scene, &surface);
    assert_ne!(
        pixels_in(&surface.read_pixels(&compositor.device)),
        previous_pixels,
        "the control draws token-resolved hover fill"
    );
}

/// A caption longer than the column ellipsizes on its one line; the title,
/// × and action row move down by exactly that line at unchanged widths. A
/// labeled session's alert banner draws no caption.
#[tokio::test]
async fn test_long_session_caption_ellipsizes_and_moves_the_card_by_one_line() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let label = "W".repeat(32);
    let session = tze_hud_scene::scope::scoped_namespace("claude", "s1");
    let scene_for = |caption: Option<&str>| {
        let mut scene = SceneGraph::new(1280.0, 720.0);
        scene.register_zone(ZoneDefinition {
            id: SceneId::new(),
            name: "notification-area".to_owned(),
            description: "long caption test".to_owned(),
            geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 0.2,
                height_pct: 0.9,
            },
            accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
            rendering_policy: RenderingPolicy::default(),
            contention_policy: ContentionPolicy::Stack { max_depth: 5 },
            max_publishers: 8,
            auto_clear_ms: None,
            ephemeral: false,
            layer_attachment: LayerAttachment::Chrome,
        });
        scene.set_session_label(&session, caption.map(str::to_owned));
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    title: "Title".to_owned(),
                    text: "Body".to_owned(),
                    icon: String::new(),
                    urgency: 1,
                    ttl_ms: None,
                    actions: vec![NotificationAction {
                        label: "Go".to_owned(),
                        callback_id: Some("go".to_owned()),
                        ..Default::default()
                    }],
                }),
                &session,
                None,
                None,
                None,
            )
            .unwrap();
        scene
    };
    let style = crate::renderer::notification_style::NotificationStyle::resolve(
        &HashMap::new(),
        &RenderingPolicy::default(),
    );
    let line = style.body.line_height;
    let (labeled, plain) = (scene_for(Some(&label)), scene_for(None));
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    compositor.prime_truncation_cache(&labeled);
    let items = compositor.collect_text_items(&labeled, 1280.0, 720.0);
    let plain_items = compositor.collect_text_items(&plain, 1280.0, 720.0);
    let find = |items: &[crate::text::TextItem], text: &str| {
        items.iter().find(|i| &*i.text == text).unwrap().clone()
    };
    let caption = find(&items, &label);
    let key = crate::text::effective_truncation_key(&caption).expect("the caption ellipsizes");
    let rasterizer = compositor.text_rasterizer.as_mut().unwrap();
    let cut = rasterizer
        .truncation_cache
        .get_by_key(&key)
        .expect("primed")
        .clone();
    assert!(
        cut.was_truncated && cut.text.ends_with(crate::overflow::ELLIPSIS),
        "{}",
        cut.text
    );
    let drawn = rasterizer.measure_single_line(
        &cut.text,
        caption.font_size_px,
        caption.font_family,
        caption.font_weight,
    );
    assert!(
        drawn <= caption.bounds_width + 0.5,
        "{drawn} > {}",
        caption.bounds_width
    );
    let title = find(&items, "Title");
    assert_eq!(caption.bounds_width, title.bounds_width, "one text column");
    assert!((title.pixel_y - caption.pixel_y - line).abs() < 0.5);
    for text in [
        "Title",
        "Body",
        "Go",
        crate::renderer::notification_style::DISMISS_GLYPH,
    ] {
        let (moved, before) = (find(&items, text), find(&plain_items, text));
        assert!(
            (moved.pixel_y - before.pixel_y - line).abs() < 0.5,
            "{text} moves one line"
        );
        assert_eq!(
            moved.bounds_width, before.bounds_width,
            "{text} keeps its width"
        );
        assert_eq!(moved.pixel_x, before.pixel_x, "{text} keeps its x");
    }

    // Alert banners are not cards: no caption.
    let mut banner = SceneGraph::new(1280.0, 720.0);
    banner.zone_registry = ZoneRegistry::with_defaults();
    banner.set_session_label(&session, Some("tze-hud".to_owned()));
    banner
        .publish_to_zone(
            "alert-banner",
            ZoneContent::Notification(NotificationPayload {
                title: "Alert".to_owned(),
                text: "Banner".to_owned(),
                urgency: 2,
                ..Default::default()
            }),
            &session,
            None,
            None,
            None,
        )
        .unwrap();
    let items = compositor.collect_text_items(&banner, 1280.0, 720.0);
    assert!(
        items.iter().any(|i| &*i.text == "Alert"),
        "the banner draws"
    );
    assert!(
        !items.iter().any(|i| &*i.text == "tze-hud"),
        "a banner draws no caption"
    );
}
