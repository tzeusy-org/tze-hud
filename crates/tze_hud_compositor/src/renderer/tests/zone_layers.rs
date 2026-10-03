use super::*;

// ── ZoneContent::StaticImage rendering ───────────────────────────────────

/// render_zone_content with ZoneContent::StaticImage must emit a warm-gray
/// placeholder quad (R≈0.3, G≈0.3, B≈0.3) regardless of the zone's policy
/// backdrop color.
///
/// Full GPU texture upload (wgpu sampler pipeline) is deferred; this test
/// confirms the placeholder path is exercised.
#[tokio::test]
async fn test_static_image_zone_emits_warm_gray_placeholder() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "pip".to_owned(),
        description: "picture-in-picture zone".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.25,
        },
        accepted_media_types: vec![ZoneMediaType::StaticImage],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Replace,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    let resource_id = ResourceId::of(b"placeholder-image-bytes");
    scene
        .publish_to_zone(
            "pip",
            ZoneContent::StaticImage(resource_id),
            "test-agent",
            None,
            None,
            None,
        )
        .unwrap();

    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(&scene, &mut vertices, &mut Vec::new(), 1280.0, 720.0, None);

    // At least one backdrop quad must be emitted.
    assert!(
        !vertices.is_empty(),
        "StaticImage zone must emit backdrop vertices"
    );

    // The first vertex color must be warm-gray (R≈0.3, G≈0.3, B≈0.3, A≈1.0).
    let color = vertices[0].color;
    assert!(
        (color[0] - 0.3).abs() < 0.01,
        "StaticImage placeholder R must be ~0.3, got {}",
        color[0]
    );
    assert!(
        (color[1] - 0.3).abs() < 0.01,
        "StaticImage placeholder G must be ~0.3, got {}",
        color[1]
    );
    assert!(
        (color[2] - 0.3).abs() < 0.01,
        "StaticImage placeholder B must be ~0.3, got {}",
        color[2]
    );
    assert!(
        color[3] > 0.5,
        "StaticImage placeholder must be substantially opaque (A > 0.5), got {}",
        color[3]
    );
}

// ── LayerAttachment rendering order tests ─────────────────────────────────
//
// These tests verify that render_zone_content respects LayerAttachment when
// an only_layer filter is provided, and that the three-pass ordering
// (Background → Content → Chrome) is enforced by the layer filter.
//
// The approach: register zones with distinct SolidColor publishes, then call
// render_zone_content with each layer filter in sequence and verify which
// vertices are emitted.  rect_vertices emits 6 vertices per quad; the color
// fields let us identify which zone's vertices are which.

/// Background zones emit vertices only when the Background layer filter is used.
/// Content zones emit no vertices when filtered to Background only.
#[tokio::test]
async fn test_layer_filter_background_only_emits_background_vertices() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Background zone: solid dark blue (r=0.0, g=0.0, b=1.0).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "bg-zone".to_owned(),
        description: "background layer".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 1.0,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Background,
    });

    // Content zone: solid red (r=1.0, g=0.0, b=0.0).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "content-zone".to_owned(),
        description: "content layer".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.1,
            y_pct: 0.1,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    scene
        .publish_to_zone(
            "bg-zone",
            ZoneContent::SolidColor(Rgba::new(0.0, 0.0, 1.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "content-zone",
            ZoneContent::SolidColor(Rgba::new(1.0, 0.0, 0.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    // Filter: Background only — should emit bg-zone quads (6 verts), not content-zone quads.
    let mut bg_only: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(
        &scene,
        &mut bg_only,
        &mut Vec::new(),
        1280.0,
        720.0,
        Some(LayerAttachment::Background),
    );
    // rect_vertices emits 6 vertices; bg-zone should emit exactly 6.
    assert_eq!(
        bg_only.len(),
        6,
        "Background filter must emit exactly one quad (6 verts) for bg-zone"
    );
    // Verify the color is the bg-zone blue (r≈0.0, b≈1.0).
    let first_color = bg_only[0].color;
    assert!(
        first_color[0] < 0.1,
        "Background zone vertex R must be near 0.0 (blue); got {first_color:?}"
    );
    assert!(
        first_color[2] > 0.9,
        "Background zone vertex B must be near 1.0 (blue); got {first_color:?}"
    );

    // Filter: Content only — should emit content-zone quads, not bg-zone quads.
    let mut content_only: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(
        &scene,
        &mut content_only,
        &mut Vec::new(),
        1280.0,
        720.0,
        Some(LayerAttachment::Content),
    );
    assert_eq!(
        content_only.len(),
        6,
        "Content filter must emit exactly one quad (6 verts) for content-zone"
    );
    // Verify the color is the content-zone red (r≈1.0, b≈0.0).
    let content_color = content_only[0].color;
    assert!(
        content_color[0] > 0.9,
        "Content zone vertex R must be near 1.0 (red); got {content_color:?}"
    );
    assert!(
        content_color[2] < 0.1,
        "Content zone vertex B must be near 0.0 (red); got {content_color:?}"
    );
}

/// Chrome zones emit vertices only when the Chrome layer filter is used.
/// Using Chrome filter emits no Content zone vertices.
#[tokio::test]
async fn test_layer_filter_chrome_only_emits_chrome_vertices() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Content zone: solid green (r=0.0, g=1.0, b=0.0).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "content-zone".to_owned(),
        description: "content layer".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.1,
            y_pct: 0.1,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Chrome zone: solid yellow (r=1.0, g=1.0, b=0.0).
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "chrome-zone".to_owned(),
        description: "chrome layer".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.3,
            height_pct: 0.1,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene
        .publish_to_zone(
            "content-zone",
            ZoneContent::SolidColor(Rgba::new(0.0, 1.0, 0.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "chrome-zone",
            ZoneContent::SolidColor(Rgba::new(1.0, 1.0, 0.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    // Chrome filter: must emit only chrome-zone vertices.
    let mut chrome_only: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(
        &scene,
        &mut chrome_only,
        &mut Vec::new(),
        1280.0,
        720.0,
        Some(LayerAttachment::Chrome),
    );
    assert_eq!(
        chrome_only.len(),
        6,
        "Chrome filter must emit exactly one quad (6 verts) for chrome-zone"
    );
    // Verify the color is the chrome-zone yellow (r≈1.0, g≈1.0, b≈0.0).
    let chrome_color = chrome_only[0].color;
    assert!(
        chrome_color[0] > 0.9,
        "Chrome zone vertex R must be near 1.0 (yellow); got {chrome_color:?}"
    );
    assert!(
        chrome_color[1] > 0.9,
        "Chrome zone vertex G must be near 1.0 (yellow); got {chrome_color:?}"
    );
    assert!(
        chrome_color[2] < 0.1,
        "Chrome zone vertex B must be near 0.0 (yellow); got {chrome_color:?}"
    );

    // Content filter: must emit only content-zone vertices.
    let mut content_only: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.render_zone_content(
        &scene,
        &mut content_only,
        &mut Vec::new(),
        1280.0,
        720.0,
        Some(LayerAttachment::Content),
    );
    assert_eq!(
        content_only.len(),
        6,
        "Content filter must emit exactly one quad (6 verts) for content-zone"
    );
}

/// Three-pass ordering: Background vertices precede Content, Content precedes Chrome.
///
/// This test registers zones in Chrome→Background→Content order (reverse of
/// the canonical order) and verifies that manual three-pass rendering produces
/// the correct ordering regardless of registration order.
#[tokio::test]
async fn test_three_pass_ordering_independent_of_registration_order() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let mut scene = SceneGraph::new(1280.0, 720.0);

    // Register in REVERSE order: Chrome first, then Background, then Content.
    // The rendering order must still be Background → Content → Chrome.

    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "chrome-zone".to_owned(),
        description: "registered first but renders last".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.2,
            height_pct: 0.1,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });

    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "bg-zone".to_owned(),
        description: "registered second but renders first".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 1.0,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Background,
    });

    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "content-zone".to_owned(),
        description: "registered third, renders between bg and chrome".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.1,
            y_pct: 0.1,
            width_pct: 0.5,
            height_pct: 0.5,
        },
        accepted_media_types: vec![ZoneMediaType::SolidColor],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish distinct colors so we can identify each zone's vertices.
    // Background = blue (r=0, g=0, b=1), Content = red (r=1, g=0, b=0),
    // Chrome = yellow (r=1, g=1, b=0).
    scene
        .publish_to_zone(
            "chrome-zone",
            ZoneContent::SolidColor(Rgba::new(1.0, 1.0, 0.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "bg-zone",
            ZoneContent::SolidColor(Rgba::new(0.0, 0.0, 1.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .publish_to_zone(
            "content-zone",
            ZoneContent::SolidColor(Rgba::new(1.0, 0.0, 0.0, 1.0)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    // Perform three-pass rendering into a single vertex buffer.
    // Pass 1: Background.
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    let mut tex_cmds: Vec<TexturedDrawCmd> = Vec::new();
    compositor.render_zone_content(
        &scene,
        &mut vertices,
        &mut tex_cmds,
        1280.0,
        720.0,
        Some(LayerAttachment::Background),
    );
    let after_background = vertices.len();

    // Pass 2: Content.
    compositor.render_zone_content(
        &scene,
        &mut vertices,
        &mut tex_cmds,
        1280.0,
        720.0,
        Some(LayerAttachment::Content),
    );
    let after_content = vertices.len();

    // Pass 3: Chrome.
    compositor.render_zone_content(
        &scene,
        &mut vertices,
        &mut tex_cmds,
        1280.0,
        720.0,
        Some(LayerAttachment::Chrome),
    );
    let after_chrome = vertices.len();

    // Each zone produces exactly 6 vertices (one rect_vertices quad).
    assert_eq!(
        after_background, 6,
        "Background pass must emit 6 vertices; got {after_background}"
    );
    assert_eq!(
        after_content, 12,
        "After Content pass, total must be 12 vertices; got {after_content}"
    );
    assert_eq!(
        after_chrome, 18,
        "After Chrome pass, total must be 18 vertices; got {after_chrome}"
    );

    // Verify vertex colors are in the correct positional order:
    // indices 0–5 = Background (blue), 6–11 = Content (red), 12–17 = Chrome (yellow).
    let bg_r = vertices[0].color[0];
    let bg_b = vertices[0].color[2];
    assert!(
        bg_r < 0.1,
        "First quad (background) must be blue (R≈0.0); got R={bg_r}"
    );
    assert!(
        bg_b > 0.9,
        "First quad (background) must be blue (B≈1.0); got B={bg_b}"
    );

    let content_r = vertices[6].color[0];
    let content_b = vertices[6].color[2];
    assert!(
        content_r > 0.9,
        "Second quad (content) must be red (R≈1.0); got R={content_r}"
    );
    assert!(
        content_b < 0.1,
        "Second quad (content) must be red (B≈0.0); got B={content_b}"
    );

    let chrome_r = vertices[12].color[0];
    let chrome_g = vertices[12].color[1];
    assert!(
        chrome_r > 0.9,
        "Third quad (chrome) must be yellow (R≈1.0); got R={chrome_r}"
    );
    assert!(
        chrome_g > 0.9,
        "Third quad (chrome) must be yellow (G≈1.0); got G={chrome_g}"
    );
}

/// publication_fade_delay_ms derives the delay (delay until fade starts) from expires_at_wall_us
/// when present (highest priority), subtracting NOTIFICATION_FADE_OUT_MS so the
/// fade completes before the drain boundary.
///
/// For a 15 s warning: ttl_ms = 15_000 - 150 = 14_850.
/// For a 30 s critical: ttl_ms = 30_000 - 150 = 29_850.
#[test]
fn test_publication_ttl_ms_uses_expires_at_wall_us() {
    // Warning notification (urgency 2): published at t=0, expires at t=15s.
    // Expected: 15_000 ms - 150 ms fade = 14_850 ms until fade starts.
    let record_warning = ZonePublishRecord {
        lease_id: None,
        zone_name: "alert-banner".to_string(),
        publisher_namespace: "agent-warn".to_string(),
        content: ZoneContent::Notification(NotificationPayload {
            text: "Disk space low".to_owned(),
            icon: String::new(),
            urgency: 2,
            ttl_ms: None, // No per-notification TTL — urgency path sets expires_at
            title: String::new(),
            actions: Vec::new(),
        }),
        published_at_wall_us: 0,
        merge_key: None,
        expires_at_wall_us: Some(15_000_000), // 15 s in µs
        content_classification: None,
        breakpoints: Vec::new(),
    };
    let ttl =
        Compositor::publication_fade_delay_ms(&record_warning, record_warning.published_at_wall_us);
    assert_eq!(
        ttl,
        Some(14_850),
        "publication_fade_delay_ms must derive 14_850 ms (15_000 - 150 fade) for a 15s warning"
    );

    // Critical notification (urgency 3): published at t=0, expires at t=30s.
    // Expected: 30_000 ms - 150 ms fade = 29_850 ms until fade starts.
    let record_critical = ZonePublishRecord {
        lease_id: None,
        zone_name: "alert-banner".to_string(),
        publisher_namespace: "agent-crit".to_string(),
        content: ZoneContent::Notification(NotificationPayload {
            text: "System failure".to_owned(),
            icon: String::new(),
            urgency: 3,
            ttl_ms: None,
            title: String::new(),
            actions: Vec::new(),
        }),
        published_at_wall_us: 0,
        merge_key: None,
        expires_at_wall_us: Some(30_000_000), // 30 s in µs
        content_classification: None,
        breakpoints: Vec::new(),
    };
    let ttl_crit = Compositor::publication_fade_delay_ms(
        &record_critical,
        record_critical.published_at_wall_us,
    );
    assert_eq!(
        ttl_crit,
        Some(29_850),
        "publication_fade_delay_ms must derive 29_850 ms (30_000 - 150 fade) for a 30s critical"
    );

    // expires_at_wall_us takes priority over per-notification ttl_ms.
    // published=1s, expires=16s → duration=15s → 15_000 - 150 = 14_850 ms until fade.
    let record_both = ZonePublishRecord {
        lease_id: None,
        zone_name: "alert-banner".to_string(),
        publisher_namespace: "agent-both".to_string(),
        content: ZoneContent::Notification(NotificationPayload {
            text: "Both set".to_owned(),
            icon: String::new(),
            urgency: 2,
            ttl_ms: Some(5_000), // explicit 5 s TTL on the notification itself
            title: String::new(),
            actions: Vec::new(),
        }),
        published_at_wall_us: 1_000_000, // published at t=1s
        merge_key: None,
        expires_at_wall_us: Some(16_000_000), // expires at t=16s → 15 s duration
        content_classification: None,
        breakpoints: Vec::new(),
    };
    let ttl_both =
        Compositor::publication_fade_delay_ms(&record_both, record_both.published_at_wall_us);
    assert_eq!(
        ttl_both,
        Some(14_850),
        "publication_fade_delay_ms must prefer expires_at_wall_us over per-notification ttl_ms (14_850 ms = 15_000 - 150)"
    );

    // No expires_at: held until cleared, whatever the payload ttl_ms says.
    let record_info = ZonePublishRecord {
        lease_id: None,
        zone_name: "alert-banner".to_string(),
        publisher_namespace: "agent-info".to_string(),
        content: ZoneContent::Notification(NotificationPayload {
            text: "All good".to_owned(),
            icon: String::new(),
            urgency: 1,
            ttl_ms: Some(8_000),
            title: String::new(),
            actions: Vec::new(),
        }),
        published_at_wall_us: 0,
        merge_key: None,
        expires_at_wall_us: None,
        content_classification: None,
        breakpoints: Vec::new(),
    };
    let ttl_info = Compositor::publication_fade_delay_ms(&record_info, 0);
    assert_eq!(
        ttl_info, None,
        "a record with no expiry is held and has no fade delay"
    );
}

// ── Transition interrupt semantics [hud-hzub.2] ─────────────────────────

/// fade_in_from starts from a non-zero opacity.
///
/// Acceptance criterion: transition interrupt semantics must begin fade-in
/// from current composite opacity, not from zero.
#[test]
fn test_fade_in_from_starts_at_given_opacity() {
    // Simulate: fade-out was 50% complete → current_opacity = 0.5.
    // Start a fade_in_from(0.5) — should begin at 0.5.
    let state = ZoneAnimationState::fade_in_from(10_000, 0.5);
    let opacity = state.current_opacity();
    // Very shortly after creation, opacity should be ~0.5 (no time has elapsed).
    assert!(
        (opacity - 0.5).abs() < 0.05,
        "fade_in_from(0.5) should start at ~0.5 opacity, got {opacity}"
    );
    assert_eq!(state.target_opacity, 1.0, "fade_in_from target must be 1.0");
}

/// fade_in_from clamps from_opacity to [0.0, 1.0].
#[test]
fn test_fade_in_from_clamps_opacity() {
    let state_low = ZoneAnimationState::fade_in_from(1_000, -0.5);
    assert_eq!(state_low.from_opacity, 0.0, "negative opacity clamped to 0");
    let state_high = ZoneAnimationState::fade_in_from(1_000, 1.5);
    assert_eq!(
        state_high.from_opacity, 1.0,
        "overflow opacity clamped to 1"
    );
}

/// Transition interrupt: update_zone_animations starts fade-in from current
/// opacity when a new publish arrives during an active fade-out.
#[tokio::test]
async fn test_transition_interrupt_starts_fade_in_from_current_opacity() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "transition interrupt test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            transition_in_ms: Some(200),
            transition_out_ms: Some(150),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Step 1: publish content — this makes zone active.
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("First".to_owned()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    compositor.update_zone_animations(&scene);

    // Step 2: clear — marks zone inactive, starts fade-out.
    scene
        .zone_registry
        .active_publishes
        .get_mut("subtitle")
        .unwrap()
        .clear();
    compositor.update_zone_animations(&scene);

    // The zone animation state should now be a fade-out (target = 0).
    let has_fadeout = compositor
        .zone_animation_states
        .get("subtitle")
        .map(|s| s.target_opacity == 0.0)
        .unwrap_or(false);
    assert!(has_fadeout, "expected fade-out state after zone clear");

    // Inject a partially-complete fade-out (from_opacity=1, target=0, ~50% elapsed).
    // We simulate 50% opacity by creating a state with from_opacity=1.0 and checking
    // that after interrupt, from_opacity is NOT 0.0.
    let partial_opacity = compositor
        .zone_animation_states
        .get("subtitle")
        .map(|s| s.current_opacity())
        .unwrap_or(0.0);
    // At t=0 the fade-out just started, so opacity ≈ 1.0 still.
    assert!(
        partial_opacity > 0.5,
        "fade-out just started, opacity should be > 0.5, got {partial_opacity}"
    );

    // Step 3: re-publish during fade-out — interrupt semantics must apply.
    scene
        .publish_to_zone(
            "subtitle",
            ZoneContent::StreamText("Second".to_owned()),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();

    // Record fade-out opacity just before interrupt.
    let pre_interrupt_opacity = compositor
        .zone_animation_states
        .get("subtitle")
        .map(|s| s.current_opacity())
        .unwrap_or(0.0);

    compositor.update_zone_animations(&scene);

    // After interrupt: must be fade-in (target = 1.0).
    let state = compositor
        .zone_animation_states
        .get("subtitle")
        .expect("zone animation state must exist after interrupt fade-in");
    assert_eq!(
        state.target_opacity, 1.0,
        "transition interrupt must produce a fade-in state (target = 1.0)"
    );
    // from_opacity must be the interrupted fade-out opacity, not 0.
    // Pre-interrupt opacity is > 0.5 (fade-out just started), so from ≈ pre_interrupt.
    assert!(
        state.from_opacity > 0.0,
        "fade_in_from must start from current opacity (> 0), got {}",
        state.from_opacity
    );
    // The from_opacity should be ≈ the pre-interrupt value (fade-out just started).
    assert!(
        (state.from_opacity - pre_interrupt_opacity).abs() < 0.1,
        "fade_in_from must start from current fade-out opacity (~{pre_interrupt_opacity}), got {}",
        state.from_opacity
    );
}

// ── Streaming word-by-word reveal [hud-hzub.2] ──────────────────────────

/// StreamRevealState.visible_byte_offset returns usize::MAX when no breakpoints.
#[test]
fn test_stream_reveal_no_breakpoints_reveals_all() {
    let state = StreamRevealState::new(
        (1_000_000, "agent".to_owned()),
        vec![] as Vec<u64>, // no breakpoints
    );
    assert_eq!(
        state.visible_byte_offset(),
        usize::MAX,
        "empty breakpoints must reveal all text immediately"
    );
}

/// StreamRevealState starts at segment 0 and reveals first breakpoint.
#[test]
fn test_stream_reveal_starts_at_first_breakpoint() {
    let state = StreamRevealState::new(
        (1_000_000, "agent".to_owned()),
        vec![3, 9, 15], // "The" at 3, "The quick" at 9, etc.
    );
    assert_eq!(
        state.visible_byte_offset(),
        3,
        "initial visible_byte_offset must be breakpoints[0]=3"
    );
}

/// StreamRevealState.advance() progresses through breakpoints.
#[test]
fn test_stream_reveal_advance_progresses_breakpoints() {
    let mut state = StreamRevealState::new((1_000_000, "agent".to_owned()), vec![3, 9, 15]);
    assert_eq!(state.visible_byte_offset(), 3, "initially at breakpoint 0");

    // Advance STREAM_REVEAL_FRAMES_PER_SEGMENT times to move to next.
    for _ in 0..STREAM_REVEAL_FRAMES_PER_SEGMENT {
        state.advance();
    }
    assert_eq!(
        state.visible_byte_offset(),
        9,
        "after advance, at breakpoint 1"
    );

    for _ in 0..STREAM_REVEAL_FRAMES_PER_SEGMENT {
        state.advance();
    }
    assert_eq!(
        state.visible_byte_offset(),
        15,
        "after advance, at breakpoint 2"
    );

    for _ in 0..STREAM_REVEAL_FRAMES_PER_SEGMENT {
        state.advance();
    }
    assert_eq!(
        state.visible_byte_offset(),
        usize::MAX,
        "after all breakpoints revealed, must show full text (usize::MAX)"
    );
}

/// update_stream_reveals creates state for StreamText with breakpoints.
#[tokio::test]
async fn test_update_stream_reveals_creates_state() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "streaming test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // Publish StreamText with breakpoints via publish_to_zone_with_breakpoints.
    scene
        .publish_to_zone_with_breakpoints(
            "subtitle",
            ZoneContent::StreamText("The quick brown fox".to_owned()),
            "agent",
            None,
            None,
            None,
            vec![3, 9, 15],
        )
        .unwrap();

    compositor.update_stream_reveals(&scene);

    let reveal = compositor.stream_reveal_states.get("subtitle");
    assert!(
        reveal.is_some(),
        "stream_reveal_states must have an entry for subtitle"
    );
    let reveal = reveal.unwrap();
    assert_eq!(
        reveal.breakpoints,
        vec![3, 9, 15],
        "breakpoints must match the publish record"
    );
    assert_eq!(reveal.segment_idx, 0, "reveal starts at segment 0");
}

/// update_stream_reveals resets state when a new publication replaces old.
/// Verifies latest-wins cancels in-progress streaming reveal.
#[tokio::test]
async fn test_update_stream_reveals_resets_on_new_publish() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "streaming reset test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // First publish with breakpoints.
    scene
        .publish_to_zone_with_breakpoints(
            "subtitle",
            ZoneContent::StreamText("The quick brown fox".to_owned()),
            "agent",
            None,
            None,
            None,
            vec![3, 9, 15],
        )
        .unwrap();
    compositor.update_stream_reveals(&scene);

    // Advance a few frames to simulate partial reveal.
    for _ in 0..(STREAM_REVEAL_FRAMES_PER_SEGMENT + 1) {
        compositor.update_stream_reveals(&scene);
    }
    let partial_idx = compositor
        .stream_reveal_states
        .get("subtitle")
        .map(|s| s.segment_idx)
        .unwrap_or(0);
    assert!(partial_idx > 0, "reveal should have advanced beyond 0");

    // Second publish (different published_at_wall_us) — must reset reveal.
    scene
        .publish_to_zone_with_breakpoints(
            "subtitle",
            ZoneContent::StreamText("New content streaming".to_owned()),
            "agent",
            None,
            None,
            None,
            vec![4, 12],
        )
        .unwrap();
    compositor.update_stream_reveals(&scene);

    let new_reveal = compositor.stream_reveal_states.get("subtitle").unwrap();
    assert_eq!(
        new_reveal.segment_idx, 0,
        "replacement must reset reveal to segment 0 (latest-wins cancel)"
    );
    assert_eq!(
        new_reveal.breakpoints,
        vec![4, 12],
        "new breakpoints must be from the replacement publication"
    );
}

/// collect_text_items truncates text to current reveal byte offset.
#[tokio::test]
async fn test_collect_text_items_respects_stream_reveal() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);

    let mut scene = SceneGraph::new(1280.0, 720.0);
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "subtitle".to_owned(),
        description: "streaming text item test".to_owned(),
        geometry_policy: GeometryPolicy::EdgeAnchored {
            edge: DisplayEdge::Bottom,
            height_pct: 0.10,
            width_pct: 0.80,
            margin_px: 16.0,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy {
            text_color: Some(Rgba::WHITE),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Content,
    });

    // "The quick brown fox" — breakpoints at 3, 9, 15.
    // Initially reveals only "The" (3 bytes).
    scene
        .publish_to_zone_with_breakpoints(
            "subtitle",
            ZoneContent::StreamText("The quick brown fox".to_owned()),
            "agent",
            None,
            None,
            None,
            vec![3, 9, 15],
        )
        .unwrap();

    // Create reveal state at segment 0 (reveals "The").
    compositor.update_stream_reveals(&scene);

    let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
    assert!(!items.is_empty(), "must produce at least one TextItem");
    let visible_text = &*items[0].text;
    assert_eq!(
        visible_text, "The",
        "initial reveal must show only text up to first breakpoint (\"The\")"
    );
}
