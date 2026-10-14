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
/// hit regions register, as outlined SDF controls in token-styled fill (the
/// hover/pressed fills when `chrome_states` says so), with their labels. Two
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
    let region_id = |callback: &str| {
        scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|r| matches!(&r.kind, tze_hud_scene::types::ZoneInteractionKind::Action { callback_id } if callback_id == callback))
            .map(|r| r.interaction_id.clone())
            .unwrap()
    };
    let (ship, hold) = (region_id("ship"), region_id("hold"));
    scene.set_chrome_hovered(&ship, true);
    scene.set_chrome_pressed(&hold, true);

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert_eq!(
        vertices.len(),
        12,
        "the flat pass draws only the two backdrops; buttons are SDF controls"
    );
    let cmds = compositor
        .collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0)
        .chrome;
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);

    let tokens =
        crate::renderer::token_colors::resolve_notification_action_tokens(&compositor.token_map);
    let mut checked = 0;
    for region in &scene.overlay.zone_hit_regions {
        let tze_hud_scene::types::ZoneInteractionKind::Action { callback_id } = &region.kind else {
            continue;
        };
        let b = region.bounds;
        let fill = match callback_id.as_str() {
            "ship" => tokens.background_hover,
            "hold" => tokens.background_pressed,
            _ => tokens.background,
        };
        assert!(
            cmds.iter().any(
                |c| (c.x, c.y, c.width, c.height) == (b.x, b.y, b.width, b.height)
                    && c.color == compositor.gpu_color(fill)
                    && c.border.map(|border| border.width) == Some(1.0)
            ),
            "no outlined control in the {callback_id} state fill at its hit region: {b:?}"
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

/// hud-mskx5: a notification with actions reserves its button row in the
/// slot height, so stacked one- and two-line notifications draw their text
/// strictly above their buttons; a notification without actions keeps its
/// text-only height.
#[tokio::test]
async fn notification_action_row_is_reserved_below_the_text() {
    use crate::renderer::token_colors::{
        NOTIFICATION_ACTION_BUTTON_H_PX, NOTIFICATION_ACTION_ROW_GAP_PX,
    };
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);

    let policy = RenderingPolicy {
        font_size_px: Some(16.0),
        backdrop: Some(Rgba::new(0.0, 0.0, 0.0, 1.0)),
        ..Default::default()
    };
    let action = |label: &str| tze_hud_scene::types::NotificationAction {
        label: label.to_owned(),
        callback_id: label.to_lowercase(),
    };
    let payload = |title: &str, text: &str, labels: &[&str]| NotificationPayload {
        text: text.to_owned(),
        icon: String::new(),
        urgency: 1,
        ttl_ms: None,
        title: title.to_owned(),
        actions: labels.iter().map(|l| action(l)).collect(),
    };

    // Slot height: actions add exactly the reserved row; no actions add nothing.
    for title in ["", "Title"] {
        let plain = payload(title, "body", &[]);
        let with_actions = payload(title, "body", &["Approve", "Reject"]);
        let h = |p: &NotificationPayload| {
            Compositor::notification_slot_height(
                p,
                &policy,
                NOTIFICATION_BODY_SCALE,
                NOTIFICATION_INTER_LINE_GAP,
            )
        };
        if title.is_empty() {
            assert_eq!(h(&plain), Compositor::stack_slot_height(&policy));
        }
        assert_eq!(
            h(&with_actions),
            h(&plain) + NOTIFICATION_ACTION_BUTTON_H_PX + NOTIFICATION_ACTION_ROW_GAP_PX,
            "title={title:?}: actions reserve the button row"
        );
    }

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
            height_pct: 0.8,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: policy.clone(),
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 4,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    // The owner's Windows case (title + one-line body), a body-only card, and
    // a card without actions, stacked.
    for (ns, p) in [
        (
            "a",
            payload(
                "Click test 2",
                "Hover each button, then click Approve once.",
                &["Approve", "Reject"],
            ),
        ),
        ("b", payload("", "Body only", &["Open"])),
        ("c", payload("Plain", "No actions here", &[])),
    ] {
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(p),
                ns,
                None,
                None,
                None,
            )
            .unwrap();
    }

    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    let text_bottom = |text: &str| {
        let i = items
            .iter()
            .find(|i| &*i.text == text)
            .unwrap_or_else(|| panic!("no text item {text:?}"));
        i.pixel_y + i.bounds_height
    };
    let button_top = |callback: &str| {
        scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|r| matches!(&r.kind, tze_hud_scene::types::ZoneInteractionKind::Action { callback_id } if callback_id == callback))
            .unwrap_or_else(|| panic!("no hit region for {callback}"))
            .bounds
            .y
    };
    for (text, callback) in [
        ("Hover each button, then click Approve once.", "approve"),
        ("Hover each button, then click Approve once.", "reject"),
        ("Click test 2", "approve"),
        ("Body only", "open"),
    ] {
        assert!(
            text_bottom(text) <= button_top(callback),
            "{text:?} bounds end at {} but the {callback} button starts at {}",
            text_bottom(text),
            button_top(callback)
        );
    }
}

/// Pixel check (hud-paslt): on a rounded Chrome-layer card the action button
/// reads as a button over the card fill rather than vanishing under it, and
/// hovering it brightens it with no agent round trip.
#[tokio::test]
async fn notification_action_button_is_visible_and_hover_brightens_it() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut scene = bordered_notification_scene(Some(12.0));
    scene.zone_registry.active_publishes.clear();
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "Approve the deploy?".to_owned(),
                icon: String::new(),
                urgency: 0,
                ttl_ms: None,
                title: "Deploy".to_owned(),
                actions: vec![tze_hud_scene::types::NotificationAction {
                    label: "Approve".to_owned(),
                    callback_id: "approve".to_owned(),
                }],
            }),
            "test",
            None,
            None,
            None,
        )
        .unwrap();
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let button = scene
        .overlay
        .zone_hit_regions
        .iter()
        .find(|r| {
            matches!(
                r.kind,
                tze_hud_scene::types::ZoneInteractionKind::Action { .. }
            )
        })
        .cloned()
        .expect("the action has a hit region");

    let render = |compositor: &mut Compositor, scene: &mut SceneGraph| {
        compositor.prime_markdown_cache(scene);
        compositor.prime_truncation_cache(scene);
        compositor.render_frame_headless(scene, &surface);
        let pixels = surface.read_pixels(&compositor.device);
        let px = move |x: f32, y: f32| -> [u8; 4] {
            let i = (y as usize * 1280 + x as usize) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        // A label-free spot inside the button (left edge, past the outline),
        // and the card fill in the inset left of the button row.
        let b = button.bounds;
        (
            px(b.x + 3.0, b.y + b.height * 0.5),
            px(b.x - 5.0, b.y + b.height * 0.5),
        )
    };
    let luma = |p: [u8; 4]| p[0] as u32 + p[1] as u32 + p[2] as u32;

    let (idle, card) = render(&mut compositor, &mut scene);
    assert!(
        luma(idle) > luma(card) + 20,
        "the idle button must stand out from the card fill: button {idle:?}, card {card:?}"
    );

    scene.set_chrome_hovered(&button.interaction_id, true);
    let (hovered, _) = render(&mut compositor, &mut scene);
    assert!(
        luma(hovered) > luma(idle) + 20,
        "hover must brighten the button: idle {idle:?}, hovered {hovered:?}"
    );
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

/// Notification urgency colours: urgency above 3 clamps to critical, and a
/// `color.notification.urgency.*` token overrides the fallback (sRGB hex decoded
/// to linear).
#[test]
fn test_notification_urgency_color_tokens() {
    let no_tokens = HashMap::new();
    let critical = urgency_to_notification_color(3, &no_tokens);
    for urgency in [4, 100] {
        let clamped = urgency_to_notification_color(urgency, &no_tokens);
        assert_eq!(
            (clamped.r, clamped.g, clamped.b),
            (critical.r, critical.g, critical.b),
            "urgency={urgency} must clamp to critical"
        );
    }

    // (urgency, token, hex, expected linear rgb)
    let overrides = [
        (0, "low", "#00FFFF", [0.0, 1.0, 1.0]),
        (3, "critical", "#450612", [0.0595, 0.0018, 0.0060]),
    ];
    for (urgency, name, hex, expected) in overrides {
        let tokens = HashMap::from([(
            format!("color.notification.urgency.{name}"),
            hex.to_string(),
        )]);
        let c = urgency_to_notification_color(urgency, &tokens);
        for (got, want) in [c.r, c.g, c.b].into_iter().zip(expected) {
            assert!((got - want).abs() < 0.004, "{name} token {hex}: got {c:?}");
        }
    }
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

/// A Stack `notification-area` scene with one urgency-0 notification, for the
/// border tests. `radius` sets `backdrop_radius` (None = flat backdrop).
fn bordered_notification_scene(radius: Option<f32>) -> SceneGraph {
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
            backdrop_radius: radius,
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
    scene
}

/// The notification card border and dismiss outline are SDF borders, not flat
/// quads. On a flat backdrop (no radius) the flat pass emits only the backdrop
/// and the SDF pass adds a border-only card shape plus the dismiss outline; on
/// a rounded backdrop the border rides on the backdrop's own SDF shape.
#[tokio::test]
async fn test_notification_card_border_is_an_sdf_border() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let flat = bordered_notification_scene(None);
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&flat, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);
    assert_eq!(vertices.len(), 6, "flat pass: backdrop quad only");

    let (x, y, w) = (960.0, 14.4, 307.2);
    let cmds = compositor
        .collect_all_rounded_rect_cmds(&flat, 1280.0, 720.0)
        .chrome;
    assert_eq!(cmds.len(), 2, "card border + dismiss outline: {cmds:?}");
    let card = &cmds[0];
    assert_eq!(card.color, [0.0; 4], "border-only over the flat backdrop");
    assert_eq!(card.radius, 0.0);
    assert!((card.x - x).abs() < 0.01 && (card.y - y).abs() < 0.01);
    assert!((card.width - w).abs() < 0.01, "card spans the slot width");
    assert_eq!(card.border.map(|b| b.width), Some(1.0));
    let dismiss = &cmds[1];
    assert_eq!(dismiss.color, [0.0; 4]);
    assert_eq!(dismiss.border.map(|b| b.width), Some(1.0));
    assert!(
        (dismiss.x + dismiss.width - (x + w)).abs() < 0.01,
        "dismiss at the right edge"
    );

    let rounded = bordered_notification_scene(Some(12.0));
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(
        &rounded,
        &mut vertices,
        &mut Vec::new(),
        1280.0,
        720.0,
        None,
    );
    assert!(
        vertices.is_empty(),
        "rounded backdrop and borders all go to the SDF pass"
    );
    let cmds = compositor
        .collect_all_rounded_rect_cmds(&rounded, 1280.0, 720.0)
        .chrome;
    assert_eq!(cmds.len(), 2, "card + dismiss outline: {cmds:?}");
    assert_eq!(cmds[0].radius, 12.0);
    assert!(cmds[0].color[3] > 0.7, "card carries its fill");
    assert_eq!(cmds[0].border.map(|b| b.width), Some(1.0));
}

/// Pixel check of the SDF border on a rounded card (fullscreen, opaque border
/// token): just inside the rounded corner is border colour, the middle is
/// fill, and the bounding-box corner outside the curve stays clear.
#[tokio::test]
async fn test_rounded_card_border_follows_the_corner() {
    let built = make_compositor_and_surface(1280, 720).await;
    if std::env::var("TZE_HUD_REQUIRE_GPU").as_deref() == Ok("1") {
        assert!(
            built.is_some(),
            "required hairline GPU constructor must succeed"
        );
    }
    let (mut compositor, surface) = require_gpu!(built);
    let mut tokens = HashMap::new();
    tokens.insert("color.border.default".to_string(), "#00FF00".to_string());
    tokens.insert(
        "color.notification.urgency.low".to_string(),
        "#FF0000".to_string(),
    );
    compositor.set_token_map(tokens);
    let mut scene = bordered_notification_scene(Some(16.0));
    compositor.prime_markdown_cache(&scene);
    compositor.prime_truncation_cache(&scene);
    compositor.render_frame_headless(&mut scene, &surface);

    let pixels = surface.read_pixels(&compositor.device);
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 1280 + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    };
    // Zone top-left (960, 14.4), radius 16. In the corner box, the pixel whose
    // centre is closest to 0.5 px inside the curve (mid-border, 1 px wide) on
    // the curved part of the edge (both offsets past 4 px) is border.
    let (x0, y0, r) = (960.0_f32, 14.4_f32, 16.0_f32);
    let sdf = |cx: f32, cy: f32| {
        let (qx, qy) = (x0 + r - cx, y0 + r - cy);
        (qx * qx + qy * qy).sqrt() - r
    };
    let border_px = (964..976)
        .flat_map(|x| (19..30).map(move |y| (x, y)))
        .min_by(|a, b| {
            let da = (sdf(a.0 as f32 + 0.5, a.1 as f32 + 0.5) + 0.5).abs();
            let db = (sdf(b.0 as f32 + 0.5, b.1 as f32 + 0.5) + 0.5).abs();
            da.total_cmp(&db)
        })
        .unwrap();
    let border = px(border_px.0, border_px.1);
    assert!(
        border[1] > 150 && border[1] > border[0] + 60,
        "corner pixel {border_px:?} inside the radius must be border green, got {border:?}"
    );
    let slot_h = compositor
        .collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0)
        .chrome[0]
        .height;
    let fill = px(960 + 184, (y0 + slot_h * 0.5) as usize);
    assert!(
        fill[0] > 150 && fill[1] < 100,
        "card middle must be fill red, got {fill:?}"
    );
    let outside = px(960, 14 + 1);
    let clear = px(10, 700);
    assert_eq!(outside, clear, "outside the curve stays clear");

    // The straight top edge is one device-pixel row at fractional y=14.4.
    // Read away from corner AA, text and dismiss chrome; the existing middle
    // fill oracle supplies the exact fill-only RGBA reference.
    let top_border = px(1144, 14);
    let below_border = px(1144, 15);
    let interior = px(1144, 16);
    eprintln!(
        "[SDF-HAIRLINE-RGBA] x1144 row14={top_border:?} row15={below_border:?} row16={interior:?} fill={fill:?} actual_gpu_readback=true"
    );
    assert!(
        below_border
            .iter()
            .zip(interior)
            .all(|(actual, expected)| actual.abs_diff(expected) <= 3),
        "row15 must match row16 fill only, not a second hairline row: {below_border:?}, row16 {interior:?}"
    );
    assert!(
        interior
            .iter()
            .zip(fill)
            .all(|(actual, expected)| actual.abs_diff(expected) <= 3),
        "row16 must retain fill RGBA: {interior:?}, fill {fill:?}"
    );
    assert!(
        top_border
            .iter()
            .zip([0, 255, 0, 255])
            .all(|(actual, expected)| actual.abs_diff(expected) <= 3),
        "row14 must cover the single green device-pixel hairline: {top_border:?}"
    );
}

/// alert-banner gets no border — the card border is only for
/// non-alert-banner notification zones.
#[tokio::test]
async fn test_alert_banner_has_no_border() {
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

    // alert-banner: only 6 vertices (one backdrop quad) and no SDF border.
    assert_eq!(
        vertices.len(),
        6,
        "alert-banner must emit exactly 6 vertices (backdrop only), got {}",
        vertices.len()
    );
    let rr = compositor.collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0);
    assert!(
        rr.chrome.is_empty(),
        "alert-banner must emit no border: {:?}",
        rr.chrome
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

    let scene = bordered_notification_scene(None);
    let cmds = compositor
        .collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0)
        .chrome;
    let border = cmds[0].border.expect("card border").color;
    assert!(
        border[0] < 0.1 && border[1] > 0.9 && border[2] > 0.9,
        "card border must be the cyan token, got {border:?}"
    );
}

// ── Token-resolved severity color tests ───────────────────────────────────

/// `color.severity.*` tokens override the fallback constants (info covers urgency
/// 0 and 1); an invalid token value is ignored and the constant stays.
#[test]
fn test_severity_color_tokens_override_or_fall_back() {
    // (urgencies, token, value, expected linear rgb within 0.1)
    let cases: [(&[u32], &str, &str, [f32; 3]); 4] = [
        (&[2], "warning", "#00FF00", [0.0, 1.0, 0.0]),
        (&[3], "critical", "#FF00FF", [1.0, 0.0, 1.0]),
        (&[0, 1], "info", "#FF0000", [1.0, 0.0, 0.0]),
        // Fallback SEVERITY_WARNING (#FFB800): linear R 1.0, G ~0.72, B 0.0.
        (&[2], "warning", "not-a-color", [1.0, 0.72, 0.0]),
    ];
    for (urgencies, name, value, expected) in cases {
        let tokens = HashMap::from([(format!("color.severity.{name}"), value.to_string())]);
        for &urgency in urgencies {
            let c = urgency_to_severity_color(urgency, &tokens);
            for (got, want) in [c.r, c.g, c.b].into_iter().zip(expected) {
                assert!(
                    (got - want).abs() < 0.1,
                    "{name}={value} urgency={urgency}: got {c:?}"
                );
            }
        }
    }
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
    let now = std::time::Instant::now();
    let state = ZoneAnimationState::fade_in_at(10_000, now);
    let opacity = state.current_opacity_at(now);
    assert!(
        (0.0..=0.1).contains(&opacity),
        "fade-in opacity shortly after start should be near 0, got {opacity}"
    );
    assert!(
        !state.is_complete_at(now),
        "10s fade-in should not be complete immediately"
    );
    let midpoint = now + std::time::Duration::from_millis(5_000);
    assert_eq!(state.current_opacity_at(midpoint), 0.5);
    let interrupted =
        ZoneAnimationState::fade_in_from_at(100, state.current_opacity_at(midpoint), midpoint);
    assert_eq!(interrupted.current_opacity_at(midpoint), 0.5);
    assert_eq!(
        interrupted.current_opacity_at(midpoint + std::time::Duration::from_millis(50)),
        0.75
    );
    let end = now + std::time::Duration::from_millis(10_000);
    assert_eq!(state.current_opacity_at(end), 1.0);
    assert!(state.is_complete_at(end));
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

    // Notification borders use the SDF pass, separate from flat backdrop vertices.
    let rounded_rects = compositor.collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0);
    assert!(
        rounded_rects.background.is_empty()
            && rounded_rects.content.is_empty()
            && rounded_rects.chrome.is_empty(),
        "no rounded-rect commands should be rendered when policy.backdrop is None"
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
