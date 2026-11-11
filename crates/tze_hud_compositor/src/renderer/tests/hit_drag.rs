use super::*;

// ── Zone interaction hit region tests (hud-ltgk.4) ────────────────────────
//
// These tests verify `populate_zone_hit_regions`: the pure-geometry path that
// computes dismiss (×) and action button pixel bounds for Stack zone
// notification publications.
//
// All tests are GPU-gated (require_gpu!) because populate_zone_hit_regions
// is a method on Compositor, which requires a GPU device at construction.
// The method itself is pure geometry — it does not issue GPU commands.

/// `populate_zone_hit_regions()` MUST produce exactly one dismiss region for a
/// single notification with no actions in a Stack zone.
#[tokio::test]
async fn zone_hit_single_notification_produces_dismiss_region() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab = scene.create_tab("Main", 0).unwrap();
    scene.register_zone(tze_hud_scene::types::ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: tze_hud_scene::types::RenderingPolicy {
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Hello".to_string(),
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

    compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);

    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        1,
        "single notification with no actions must produce exactly 1 hit region"
    );
    assert_eq!(
        scene.overlay.zone_hit_regions[0].kind,
        tze_hud_scene::types::ZoneInteractionKind::Dismiss,
        "single region must be a Dismiss button"
    );
    assert!(
        scene.overlay.zone_hit_regions[0]
            .interaction_id
            .contains("dismiss"),
        "interaction_id must contain 'dismiss': {}",
        scene.overlay.zone_hit_regions[0].interaction_id
    );
}

/// The dismiss region MUST be positioned at the top-right of the notification slot.
/// Zone: x_pct=0.75, width_pct=0.24 on a 1920×1080 screen.
/// Expected: dismiss.x ≈ 1920*0.75 + 1920*0.24 - 20 = 1440 + 460.8 - 20 = 1880.8.
#[tokio::test]
async fn zone_hit_dismiss_region_at_top_right_of_slot() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let sw = 1920.0f32;
    let sh = 1080.0f32;
    let mut scene = SceneGraph::new(sw, sh);
    let _tab = scene.create_tab("Main", 0).unwrap();
    scene.register_zone(tze_hud_scene::types::ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: tze_hud_scene::types::RenderingPolicy {
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Test".to_string(),
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

    compositor.populate_zone_hit_regions(&mut scene, sw, sh);

    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        1,
        "must have exactly 1 region"
    );
    let region = &scene.overlay.zone_hit_regions[0];

    // Dismiss should be at the top-right of the slot, inset 9 px.
    // Zone geometry: zx = 1920*0.75 = 1440, zw = 1920*0.24 = 460.8.
    // Dismiss x = zx + zw - 9 - 20 = 1871.8.
    let expected_x = sw * 0.75 + sw * 0.24 - 9.0 - 20.0;
    assert!(
        (region.bounds.x - expected_x).abs() < 1.0,
        "dismiss x must be at top-right (expected≈{expected_x:.1}, got {:.1})",
        region.bounds.x
    );
    assert!(
        (region.bounds.y - 9.0).abs() < 1.0,
        "dismiss y must be inset from the slot top (expected≈9, got {:.1})",
        region.bounds.y
    );
}

/// A notification with 2 actions MUST produce 3 regions: 1 dismiss + 2 actions.
#[tokio::test]
async fn zone_hit_notification_with_two_actions_produces_three_regions() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab = scene.create_tab("Main", 0).unwrap();
    scene.register_zone(tze_hud_scene::types::ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: tze_hud_scene::types::RenderingPolicy {
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Confirm?".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,

                title: String::new(),
                actions: vec![
                    NotificationAction {
                        label: "Yes".to_string(),
                        callback_id: Some("yes".to_string()),
                        url: None,
                    },
                    NotificationAction {
                        label: "No".to_string(),
                        callback_id: Some("no".to_string()),
                        url: None,
                    },
                ],
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);

    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        3,
        "1 dismiss + 2 action buttons = 3 regions"
    );

    assert_eq!(
        scene.overlay.zone_hit_regions[0].kind,
        tze_hud_scene::types::ZoneInteractionKind::Dismiss,
        "first region must be Dismiss"
    );
    assert!(
        matches!(
            &scene.overlay.zone_hit_regions[1].kind,
            tze_hud_scene::types::ZoneInteractionKind::Action { index: 0 }
        ),
        "second region must be Action(yes)"
    );
    assert!(
        matches!(
            &scene.overlay.zone_hit_regions[2].kind,
            tze_hud_scene::types::ZoneInteractionKind::Action { index: 1 }
        ),
        "third region must be Action(no)"
    );
}

/// Tab order MUST be sequential: dismiss=0, action[0]=1, action[1]=2.
#[tokio::test]
async fn zone_hit_tab_order_is_sequential() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab = scene.create_tab("Main", 0).unwrap();
    scene.register_zone(tze_hud_scene::types::ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: tze_hud_scene::types::RenderingPolicy {
            font_size_px: Some(16.0),
            ..Default::default()
        },
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Tab order test".to_string(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,

                title: String::new(),
                actions: vec![
                    NotificationAction {
                        label: "A".to_string(),
                        callback_id: Some("a".to_string()),
                        url: None,
                    },
                    NotificationAction {
                        label: "B".to_string(),
                        callback_id: Some("b".to_string()),
                        url: None,
                    },
                ],
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);

    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        3,
        "must produce 3 regions"
    );
    assert_eq!(
        scene.overlay.zone_hit_regions[0].tab_order, 0,
        "dismiss tab_order must be 0"
    );
    assert_eq!(
        scene.overlay.zone_hit_regions[1].tab_order, 1,
        "action[0] tab_order must be 1"
    );
    assert_eq!(
        scene.overlay.zone_hit_regions[2].tab_order, 2,
        "action[1] tab_order must be 2"
    );
}

/// Calling `populate_zone_hit_regions` twice MUST clear stale regions (no accumulation).
#[tokio::test]
async fn zone_hit_populate_clears_on_repeated_calls() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let _tab = scene.create_tab("Main", 0).unwrap();
    scene.register_zone(tze_hud_scene::types::ZoneDefinition {
        id: SceneId::new(),
        name: "notif".to_string(),
        description: "Stack zone".to_string(),
        geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
            x_pct: 0.75,
            y_pct: 0.0,
            width_pct: 0.24,
            height_pct: 0.30,
        },
        accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: tze_hud_scene::types::RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "notif",
            ZoneContent::Notification(NotificationPayload {
                text: "Once".to_string(),
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

    compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);
    let first_count = scene.overlay.zone_hit_regions.len();
    assert_eq!(first_count, 1, "first call must produce 1 region");

    compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);
    assert_eq!(
        scene.overlay.zone_hit_regions.len(),
        1,
        "second call must still produce 1 (not accumulate to 2)"
    );
}

/// hud-643dv: a portal frame tile (largest-area member of a scrollable lease)
/// gets a full-width HEADER-BAND drag handle (Windows-titlebar), while the panes
/// keep the legacy centered grip. The band height is token-driven with a sane
/// default consistent with the exemplar header height.
#[tokio::test]
async fn portal_frame_gets_full_width_header_band_handle() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("portal", 60_000);
    // Frame = the large anchor.
    let frame_id = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(100.0, 100.0, 600.0, 400.0),
            1,
        )
        .unwrap();
    // Scrollable pane inside the frame (makes the lease a portal group).
    let pane_id = scene
        .create_tile(
            tab,
            "portal",
            lease,
            Rect::new(110.0, 160.0, 200.0, 320.0),
            3,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(pane_id, tze_hud_scene::types::TileScrollConfig::vertical())
        .unwrap();

    let handles = compositor.collect_drag_handle_entries(&scene, 1920.0, 1080.0);

    let frame = handles
        .iter()
        .find(|h| h.element_id == frame_id)
        .expect("frame tile must have a drag handle");
    assert!(
        frame.is_header_band,
        "the portal frame must get a header-band drag handle"
    );
    // Full width of the frame, top-anchored, height = default band (52).
    assert_eq!(frame.bounds.x, 100.0);
    assert_eq!(frame.bounds.y, 100.0);
    assert_eq!(
        frame.bounds.width, 600.0,
        "band must span the full frame width"
    );
    assert_eq!(
        frame.bounds.height,
        tze_hud_scene::types::PORTAL_HEADER_DRAG_BAND_PX_DEFAULT,
        "band height must come from the token default, not a magic value"
    );

    let pane = handles
        .iter()
        .find(|h| h.element_id == pane_id)
        .expect("pane tile must still have a drag handle");
    assert!(
        !pane.is_header_band,
        "panes keep the legacy centered grip, not a band"
    );
    assert!(
        pane.bounds.width < 600.0,
        "the pane grip must be the small centered grip, not a full-width band"
    );
}

/// hud-ovjxu.1: the compositor applies the tile's viewer-local font-scale
/// multiplier, clamped to the token-default legible range, when resolving a
/// portal text node's effective font. GPU only builds the compositor (no readback).
#[tokio::test]
async fn portal_resize_scales_and_clamps_text_font() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent", 60_000);
    let tile = scene
        .create_tile(tab, "agent", lease, Rect::new(0.0, 0.0, 400.0, 300.0), 1)
        .unwrap();

    // No scale (default 1.0) → adapter-published font returned untouched.
    assert!((compositor.scaled_portal_font(16.0, tile, &scene) - 16.0).abs() < 1e-4);

    // Grow 2× → 32px, within the default legible range [9, 48].
    scene.set_tile_font_scale(tile, 2.0);
    assert!((compositor.scaled_portal_font(16.0, tile, &scene) - 32.0).abs() < 1e-4);

    // Grow far → clamp at the token-default max (48).
    scene.set_tile_font_scale(tile, 10.0);
    assert!((compositor.scaled_portal_font(16.0, tile, &scene) - 48.0).abs() < 1e-4);

    // Shrink far → clamp at the token-default min (9); further shrink only
    // reduces the content window (bounds), not the font.
    scene.set_tile_font_scale(tile, 0.1);
    assert!((compositor.scaled_portal_font(16.0, tile, &scene) - 9.0).abs() < 1e-4);
}

/// Run the drag-handle hit-region population exactly as `render_frame_headless`
/// does: collect the entries, then populate the scene overlay from them.
fn populate_drag_handles(compositor: &Compositor, scene: &mut SceneGraph, sw: f32, sh: f32) {
    let handles = compositor.collect_drag_handle_entries(scene, sw, sh);
    compositor.populate_drag_handle_hit_regions_from(scene, handles);
}

#[tokio::test]
async fn drag_handle_regions_cover_visible_tile_and_zone_but_not_widget() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000);
    let tile_id = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(100.0, 100.0, 320.0, 180.0),
            10,
        )
        .unwrap();

    let visible_zone_id = SceneId::new();
    scene.register_zone(ZoneDefinition {
        id: visible_zone_id,
        name: "drag-zone".to_string(),
        description: "zone with active content".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.55,
            y_pct: 0.10,
            width_pct: 0.30,
            height_pct: 0.20,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    });
    let empty_zone_id = SceneId::new();
    scene.register_zone(ZoneDefinition {
        id: empty_zone_id,
        name: "empty-zone".to_string(),
        description: "zone without active content".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.10,
            y_pct: 0.10,
            width_pct: 0.20,
            height_pct: 0.20,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            "drag-zone",
            ZoneContent::StreamText("active".to_string()),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();

    scene.widget_registry.register_definition(WidgetDefinition {
        id: "test-widget".to_string(),
        name: "Test Widget".to_string(),
        description: "test".to_string(),
        parameter_schema: vec![WidgetParameterDeclaration {
            audience: Default::default(),
            name: "level".to_string(),
            param_type: WidgetParamType::F32,
            default_value: WidgetParameterValue::F32(0.0),
            constraints: Some(WidgetParamConstraints {
                f32_min: Some(0.0),
                f32_max: Some(1.0),
                ..Default::default()
            }),
        }],
        layers: vec![],
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.20,
            y_pct: 0.55,
            width_pct: 0.18,
            height_pct: 0.12,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::LatestWins,
        max_publishers: WidgetDefinition::default_max_publishers(),
        ephemeral: false,
        hover_behavior: None,
    });
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "test-widget".to_string(),
        tab_id: tab,
        geometry_override: None,
        contention_override: None,
        instance_name: "test-widget-1".to_string(),
        current_params: std::collections::HashMap::new(),
        auto_clear_ms: None,
    });
    scene
        .publish_to_widget(
            "test-widget-1",
            std::collections::HashMap::new(),
            "agent-a",
            None,
            0,
            None,
        )
        .unwrap();

    populate_drag_handles(&compositor, &mut scene, 1920.0, 1080.0);

    let kinds: Vec<_> = scene
        .overlay
        .drag_handle_hit_regions
        .iter()
        .map(|r| r.element_kind)
        .collect();
    assert!(
        kinds.contains(&DragHandleElementKind::Tile),
        "tile handle missing"
    );
    assert!(
        kinds.contains(&DragHandleElementKind::Zone),
        "zone handle missing"
    );
    // One tile handle and one active-zone handle; the published widget gets
    // none (its geometry comes from the control panel's Layout card).
    assert_eq!(kinds.len(), 2, "unexpected drag handles: {kinds:?}");
    assert!(
        scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .any(|r| r.element_id == tile_id),
        "tile-id handle missing"
    );
    assert!(
        scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .any(|r| r.element_id == visible_zone_id),
        "active zone handle missing"
    );
    assert!(
        !scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .any(|r| r.element_id == empty_zone_id),
        "empty zones must not produce drag handles"
    );
    for region in &scene.overlay.drag_handle_hit_regions {
        assert!(
            region.interaction_id.starts_with("drag-handle:"),
            "interaction id must use drag-handle scheme"
        );
        assert!(
            region.hit_region.accepts_pointer,
            "drag handles must accept pointer"
        );
        assert!(
            !region.hit_region.auto_capture,
            "drag handles must not auto-capture before long-press activation"
        );
        assert!(
            !region.hit_region.accepts_focus,
            "drag handles must not participate in focus cycle"
        );
    }
    use crate::display::{DisplayLayout, DisplayRect};
    let default = scene.zone_registry.zones["drag-zone"].geometry_policy;
    let mut second = scene.zone_registry.zones["drag-zone"].clone();
    second.name = "other-zone".into();
    second.id = SceneId::new();
    let second_id = second.id;
    scene.register_zone(second);
    scene
        .publish_to_zone(
            "other-zone",
            ZoneContent::StreamText("other".into()),
            "agent-b",
            None,
            None,
            None,
        )
        .unwrap();
    scene
        .zone_registry
        .zones
        .get_mut("drag-zone")
        .unwrap()
        .accepted_media_types
        .push(ZoneMediaType::ShortTextWithIcon);
    scene.clear_zone("drag-zone").unwrap();
    scene
        .zone_registry
        .zones
        .get_mut("drag-zone")
        .unwrap()
        .contention_policy = ContentionPolicy::Stack { max_depth: 4 };
    scene
        .publish_to_zone(
            "drag-zone",
            ZoneContent::Notification(NotificationPayload {
                text: "content".into(),
                actions: vec![tze_hud_scene::NotificationAction {
                    label: "Open".into(),
                    callback_id: Some("open".into()),
                    url: None,
                }],
                ..Default::default()
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();
    let placement = GeometryPolicy::Relative {
        x_pct: 0.2,
        y_pct: 0.3,
        width_pct: 0.3,
        height_pct: 0.2,
    };
    for scale in [1.0, 1.5, 1.875] {
        compositor.set_token_map(HashMap::from([(
            tze_hud_config::tokens::PX_SCALE_KEY.into(),
            scale.to_string(),
        )]));
        for assigned in [None, Some("PRIMARY"), Some("MISSING"), Some("SECONDARY")] {
            let assignments = assigned
                .map(|name| HashMap::from([("drag-zone".into(), name.into())]))
                .unwrap_or_default();
            compositor.set_display_layout(DisplayLayout::new(
                vec![
                    DisplayRect {
                        name: "PRIMARY".into(),
                        rect: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                        primary: true,
                    },
                    DisplayRect {
                        name: "SECONDARY".into(),
                        rect: Rect::new(-1280.0, 80.0, 1280.0, 720.0),
                        primary: false,
                    },
                ],
                &assignments,
            ));
            scene.set_zone_placement(visible_zone_id, placement);
            populate_drag_handles(&compositor, &mut scene, 1920.0, 1080.0);
            compositor.populate_zone_hit_regions(&mut scene, 1920.0, 1080.0);
            let emitted = scene
                .overlay
                .drag_handle_hit_regions
                .iter()
                .find(|h| h.element_id == visible_zone_id);
            assert_eq!(emitted.is_some(), assigned != Some("SECONDARY"));
            assert!(
                scene
                    .overlay
                    .drag_handle_hit_regions
                    .iter()
                    .any(|h| h.element_id == second_id)
            );
            assert_eq!(
                scene.overlay.zone_hit_regions.len(),
                2,
                "{assigned:?} at {scale}x keeps dismiss/action controls"
            );
            let rect = compositor.zone_geometry("drag-zone", &placement, 1920.0, 1080.0);
            for region in &scene.overlay.zone_hit_regions {
                assert!(
                    region.bounds.x >= rect.0
                        && region.bounds.x + region.bounds.width <= rect.0 + rect.2 + 0.01
                );
            }
            if let Some(handle) = emitted {
                assert!(
                    (handle.bounds.x + handle.bounds.width / 2.0 - (rect.0 + rect.2 / 2.0)).abs()
                        < 0.01
                );
                assert_eq!(handle.element_kind, DragHandleElementKind::Zone);
            }
            assert!(scene.reset_zone_placement(visible_zone_id));
            assert_eq!(
                scene.effective_zone_geometry(&scene.zone_registry.zones["drag-zone"]),
                default
            );
        }
    }
    scene.set_owner_hidden(tze_hud_scene::graph::visibility::SurfaceClasses {
        zones: true,
        ..Default::default()
    });
    populate_drag_handles(&compositor, &mut scene, 1920.0, 1080.0);
    assert!(
        scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .all(|h| h.element_kind == DragHandleElementKind::Tile)
    );
}

#[tokio::test]
async fn drag_handle_hit_test_wins_on_passthrough_tile() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000);
    let tile_id = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(140.0, 180.0, 360.0, 220.0),
            10,
        )
        .unwrap();
    if let Some(tile) = scene.tiles.get_mut(&tile_id) {
        tile.input_mode = InputMode::Passthrough;
    }

    populate_drag_handles(&compositor, &mut scene, 1920.0, 1080.0);
    let handle = scene
        .overlay
        .drag_handle_hit_regions
        .iter()
        .find(|r| r.element_id == tile_id)
        .expect("tile drag handle must exist");
    let hx = handle.bounds.x + handle.bounds.width * 0.5;
    let hy = handle.bounds.y + handle.bounds.height * 0.5;

    let hit = scene.hit_test(hx, hy);
    match hit {
        HitResult::ZoneInteraction {
            kind:
                ZoneInteractionKind::DragHandle {
                    element_id,
                    element_kind,
                    ..
                },
            interaction_id,
            ..
        } => {
            assert_eq!(element_id, tile_id);
            assert_eq!(element_kind, DragHandleElementKind::Tile);
            assert_eq!(interaction_id, handle.interaction_id);
        }
        other => panic!("expected drag-handle hit, got {other:?}"),
    }
}

/// Stale entries in `chrome_states` must be pruned when the
/// corresponding element is removed from the scene.
///
/// Verifies that the per-frame drag-handle population retains only the keys
/// that are still present in the current `drag_handle_hit_regions` set —
/// the zero-allocation `iter().any()` retain used after [hud-tdtr7] must
/// have identical semantics to the previous `HashSet`-based approach.
#[tokio::test]
async fn chrome_states_stale_entries_pruned_on_repopulate() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000);
    let tile_id = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(100.0, 100.0, 300.0, 180.0),
            10,
        )
        .unwrap();

    // First populate: tile present → one drag handle and a live state entry.
    populate_drag_handles(&compositor, &mut scene, 1920.0, 1080.0);
    assert_eq!(
        scene.overlay.drag_handle_hit_regions.len(),
        1,
        "expected one drag handle before tile removal"
    );
    let live_id = scene.overlay.drag_handle_hit_regions[0]
        .interaction_id
        .clone();

    // Seed chrome_states: one live entry + one stale phantom that
    // never had a corresponding hit region.
    scene
        .overlay
        .chrome_states
        .entry(live_id.clone())
        .or_default()
        .hovered = true;
    scene.overlay.chrome_states.insert(
        "drag-handle:tile:ghost-never-existed".to_string(),
        Default::default(),
    );
    assert_eq!(scene.overlay.chrome_states.len(), 2);

    // Remove the tile so it no longer produces a drag handle.
    scene.delete_tile(tile_id, "agent-a").unwrap();

    // Second populate: no tiles → no drag handles.  Both state entries
    // (previously-live and phantom) must be pruned.
    populate_drag_handles(&compositor, &mut scene, 1920.0, 1080.0);
    assert!(
        scene.overlay.drag_handle_hit_regions.is_empty(),
        "no hit regions expected after tile removal"
    );
    assert!(
        scene.overlay.chrome_states.is_empty(),
        "stale chrome_states must be pruned by drag-handle repopulation; \
             previously-live id={live_id:?} and phantom must both be removed"
    );
}

#[tokio::test]
async fn drag_handle_opacity_switches_to_active_on_hover_state() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000);
    let _tile_id = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(120.0, 140.0, 320.0, 180.0),
            10,
        )
        .unwrap();

    let handles = compositor.collect_drag_handle_entries(&scene, 1920.0, 1080.0);
    let handle = handles.first().expect("must have at least one drag handle");

    let mut idle_vertices = Vec::new();
    compositor.append_drag_handle_vertices(
        &scene,
        std::slice::from_ref(handle),
        &mut idle_vertices,
        1920.0,
        1080.0,
    );
    let idle_alpha = idle_vertices[0].color[3];

    scene
        .overlay
        .chrome_states
        .entry(handle.interaction_id.clone())
        .or_default()
        .hovered = true;
    let mut active_vertices = Vec::new();
    compositor.append_drag_handle_vertices(
        &scene,
        std::slice::from_ref(handle),
        &mut active_vertices,
        1920.0,
        1080.0,
    );
    let active_alpha = active_vertices[0].color[3];

    assert!(
        active_alpha > idle_alpha,
        "hovered handle alpha must be greater than idle alpha"
    );
}

// ─── Drag visual feedback tests [hud-bs2q.5] ─────────────────────────────

/// During an active drag, `append_drag_handle_vertices` MUST emit the
/// v1-compatible visual feedback:
///
/// 1. **Z-order boost** (implicit: caller bumps z via `drag_active_elements`)
/// 2. **Opacity increase**: handle alpha must equal `opacity_active` (same as
///    hovered), not `opacity_idle`, when the element is in `drag_active_elements`.
/// 3. **2px highlight border**: one SDF border-only command on the element
///    bounds (`drag_highlight_cmds`), absent when idle.
#[tokio::test]
async fn drag_visual_feedback_applied_during_active_drag() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000);
    let tile_id = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(120.0, 140.0, 320.0, 180.0),
            10,
        )
        .unwrap();

    let handles = compositor.collect_drag_handle_entries(&scene, 1920.0, 1080.0);
    let handle = handles
        .iter()
        .find(|h| h.element_id == tile_id)
        .expect("must have tile drag handle");

    // Idle state — no drag active
    let mut idle_vertices = Vec::new();
    compositor.append_drag_handle_vertices(
        &scene,
        std::slice::from_ref(handle),
        &mut idle_vertices,
        1920.0,
        1080.0,
    );
    let idle_count = idle_vertices.len();
    let idle_alpha = idle_vertices[0].color[3];

    // Activate drag for this element
    scene.set_drag_active(tile_id);

    let mut drag_vertices = Vec::new();
    compositor.append_drag_handle_vertices(
        &scene,
        std::slice::from_ref(handle),
        &mut drag_vertices,
        1920.0,
        1080.0,
    );
    let drag_alpha = drag_vertices[0].color[3];

    // Opacity must be at the active level (same as hover) — not idle.
    assert!(
        drag_alpha > idle_alpha,
        "drag active handle alpha ({drag_alpha}) must be greater than idle alpha ({idle_alpha})"
    );

    // The highlight is a 2px SDF border on the element bounds, not grip quads.
    assert_eq!(
        drag_vertices.len(),
        idle_count,
        "highlight adds no flat quads"
    );
    let highlight = compositor.drag_highlight_cmds(&scene, std::slice::from_ref(handle));
    assert_eq!(highlight.len(), 1, "one highlight border while dragging");
    assert_eq!((highlight[0].x, highlight[0].width), (120.0, 320.0));
    assert_eq!(highlight[0].color, [0.0; 4], "border only");
    assert_eq!(
        highlight[0].border.map(|b| b.width),
        Some(tze_hud_input::DRAG_HIGHLIGHT_BORDER_PX)
    );

    // Clear and verify feedback is removed
    scene.clear_drag_active(tile_id);
    let mut cleared_vertices = Vec::new();
    compositor.append_drag_handle_vertices(
        &scene,
        std::slice::from_ref(handle),
        &mut cleared_vertices,
        1920.0,
        1080.0,
    );
    let cleared_count = cleared_vertices.len();
    assert_eq!(
        cleared_count, idle_count,
        "after clearing drag, vertex count must return to idle level"
    );

    let frame_bounds = scene.tiles[&tile_id].bounds;
    let pane = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(130.0, 150.0, 300.0, 160.0),
            12,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(pane, TileScrollConfig::vertical())
        .unwrap();
    let backstop = scene
        .create_tile(tab, "agent-a", lease, frame_bounds, 9)
        .unwrap();
    scene.tiles.get_mut(&backstop).unwrap().input_mode = tze_hud_scene::InputMode::Passthrough;
    let shield = scene
        .create_tile(
            tab,
            "agent-a",
            lease,
            Rect::new(1900.0, 1070.0, 1.0, 1.0),
            20,
        )
        .unwrap();
    let other_lease = scene.grant_lease("agent-b", 60_000);
    let peer = scene
        .create_tile(tab, "agent-b", other_lease, frame_bounds, 100)
        .unwrap();
    let original_order: Vec<_> = scene
        .visible_tiles()
        .iter()
        .map(|tile| (tile.id, tile.z_order))
        .collect();
    scene.set_drag_active(tile_id);
    let handles = compositor.collect_drag_handle_entries(&scene, 1920.0, 1080.0);
    let frame_handle = handles
        .iter()
        .find(|handle| handle.element_id == tile_id)
        .unwrap();
    assert!(frame_handle.is_header_band);
    let highlights = compositor.drag_highlight_cmds(&scene, &handles);
    assert_eq!(
        highlights.len(),
        1,
        "contained panes have no duplicate border"
    );
    assert_eq!(
        (
            highlights[0].x,
            highlights[0].y,
            highlights[0].width,
            highlights[0].height
        ),
        (
            frame_bounds.x,
            frame_bounds.y,
            frame_bounds.width,
            frame_bounds.height
        )
    );
    let sorted = Compositor::sort_tiles_with_drag_boost(scene.visible_tiles(), &scene);
    let ids: Vec<_> = sorted.iter().map(|tile| tile.id).collect();
    assert!(
        ids.iter().position(|id| *id == peer).unwrap()
            < ids.iter().position(|id| *id == backstop).unwrap()
    );
    assert!(
        ids.iter().position(|id| *id == shield).unwrap()
            < ids.iter().position(|id| *id == backstop).unwrap()
    );
    assert_eq!(
        ids.iter()
            .copied()
            .filter(|id| [backstop, tile_id, pane].contains(id))
            .collect::<Vec<_>>(),
        [backstop, tile_id, pane],
        "portal members retain their relative order"
    );
    for (id, z) in &original_order {
        assert_eq!(
            scene.tiles[id].z_order, *z,
            "boost never rewrites scene z-order"
        );
    }
    scene.clear_drag_active(tile_id);
    assert!(compositor.drag_highlight_cmds(&scene, &handles).is_empty());
    assert_eq!(
        Compositor::sort_tiles_with_drag_boost(scene.visible_tiles(), &scene)
            .iter()
            .map(|tile| (tile.id, tile.z_order))
            .collect::<Vec<_>>(),
        original_order
    );

    // The logical zone remains a large placement box; feedback outlines only
    // the slots the production layout actually admits, including their gaps.
    use tze_hud_scene::{GeometryPolicy, NotificationAction, ZoneDefinition};
    let mut compositor = compositor;
    let zone_id = SceneId::new();
    scene.register_zone(ZoneDefinition {
        id: zone_id,
        name: "notif".into(),
        description: "Feedback fixture".into(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.6,
            y_pct: 0.1,
            width_pct: 0.35,
            height_pct: 0.7,
        },
        accepted_media_types: vec![tze_hud_scene::ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    });
    for scale in [1.0, 1.5, 1.875] {
        compositor.set_token_map(HashMap::from([(
            "runtime.px_scale".into(),
            scale.to_string(),
        )]));
        scene.reset_zone_placement(zone_id);
        scene.clear_zone("notif").unwrap();
        for count in 1..=3 {
            scene
                .publish_to_zone(
                    "notif",
                    ZoneContent::Notification(NotificationPayload {
                        text: "Body".into(),
                        title: if count == 1 {
                            String::new()
                        } else {
                            "Title".into()
                        },
                        icon: String::new(),
                        urgency: 1,
                        ttl_ms: None,
                        actions: (0..if count == 3 { 4 } else { 0 })
                            .map(|n| NotificationAction {
                                label: format!("Action {n}"),
                                callback_id: (n != 0).then(|| format!("action-{n}")),
                                url: (n == 0).then(|| "https://example.com/scale".into()),
                            })
                            .collect(),
                    }),
                    "feedback-owner",
                    None,
                    None,
                    None,
                )
                .unwrap();
            compositor.prime_markdown_cache(&scene);
            compositor.prime_truncation_cache(&scene);
            let zone = &scene.zone_registry.zones["notif"];
            let (_, zy, zw, zh) = Compositor::resolve_zone_geometry(
                &scene.effective_zone_geometry(zone),
                1920.0,
                1080.0,
            );
            let slots = compositor.zone_slot_layout(
                "notif",
                &scene.zone_registry.active_publishes["notif"],
                &zone.rendering_policy,
                zh,
                zw,
            );
            let admitted: Vec<_> = slots.iter_visible(zy).collect();
            let bottom = admitted.last().map(|(_, y, h)| y + h).unwrap();
            assert!(bottom - zy < zh, "fixture has unused zone capacity");
            scene.set_drag_active(zone_id);
            let handles = compositor.collect_drag_handle_entries(&scene, 1920.0, 1080.0);
            let handle = handles.iter().find(|h| h.element_id == zone_id).unwrap();
            assert_eq!(
                handle.element_bounds.height, zh,
                "movement keeps the placement box"
            );
            let feedback = compositor.drag_highlight_cmds(&scene, std::slice::from_ref(handle));
            assert_eq!(feedback.len(), 1);
            assert!(
                (feedback[0].height - (bottom - zy)).abs() < 0.01,
                "notification feedback must end at the last visible slot, not reserved zone capacity"
            );
            scene.clear_drag_active(zone_id);
            assert!(compositor.drag_highlight_cmds(&scene, &handles).is_empty());
        }
        // A translated, capacity-limited stack must exclude non-admitted cards.
        scene.set_zone_placement(
            zone_id,
            GeometryPolicy::Relative {
                x_pct: 0.25,
                y_pct: 0.3,
                width_pct: 0.35,
                height_pct: 0.12,
            },
        );
        scene.set_drag_active(zone_id);
        let handles = compositor.collect_drag_handle_entries(&scene, 1920.0, 1080.0);
        let handle = handles.iter().find(|h| h.element_id == zone_id).unwrap();
        let zone = &scene.zone_registry.zones["notif"];
        let slots = compositor.zone_slot_layout(
            "notif",
            &scene.zone_registry.active_publishes["notif"],
            &zone.rendering_policy,
            handle.element_bounds.height,
            handle.element_bounds.width,
        );
        let admitted: Vec<_> = slots.iter_visible(handle.element_bounds.y).collect();
        assert!(admitted.len() < 3, "fixture excludes overflowing cards");
        let (_, y, h) = admitted.last().unwrap();
        let feedback = compositor.drag_highlight_cmds(&scene, std::slice::from_ref(handle));
        assert_eq!(
            (feedback[0].x, feedback[0].y),
            (handle.element_bounds.x, handle.element_bounds.y)
        );
        assert!((feedback[0].height - (y + h - handle.element_bounds.y)).abs() < 0.01);
        scene.clear_zone("notif").unwrap();
        let handles = compositor.collect_drag_handle_entries(&scene, 1920.0, 1080.0);
        assert!(!handles.iter().any(|h| h.element_id == zone_id));
        assert!(compositor.drag_highlight_cmds(&scene, &handles).is_empty());
        scene.clear_drag_active(zone_id);
    }
}

// ─── Drag z-order + opacity boost unit tests [hud-17c8p] ─────────────────

/// A tile in the `Activated` drag phase MUST be sorted last (highest
/// effective z-order, front-most) among tiles, regardless of its declared
/// `z_order` value.
///
/// Acceptance: `sort_tiles_with_drag_boost` places the dragged tile after
/// a tile with a higher declared `z_order` once the drag boost is applied.
#[test]
fn drag_z_order_boost_raises_tile_above_peers() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent-a", 60_000);

    // Tile A: lower z_order (renders behind by default).
    let tile_a = scene
        .create_tile(
            tab,
            "tile-a",
            lease,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            5, // z_order = 5
        )
        .unwrap();

    // Tile B: higher z_order (renders in front by default).
    let tile_b = scene
        .create_tile(
            tab,
            "tile-b",
            lease,
            Rect::new(50.0, 50.0, 100.0, 100.0),
            10, // z_order = 10
        )
        .unwrap();

    // Without any active drag: tile A (z=5) sorts before tile B (z=10).
    let sorted = Compositor::sort_tiles_with_drag_boost(scene.visible_tiles(), &scene);
    assert_eq!(
        sorted[0].id, tile_a,
        "without drag: lower z_order tile must sort first"
    );
    assert_eq!(
        sorted[1].id, tile_b,
        "without drag: higher z_order tile must sort last"
    );

    // Activate drag for tile A (z=5 → 5 + 0x1000 = 4101, exceeds tile B's z=10).
    scene.set_drag_active(tile_a);

    let sorted_with_drag = Compositor::sort_tiles_with_drag_boost(scene.visible_tiles(), &scene);
    assert_eq!(
        sorted_with_drag[0].id, tile_b,
        "with drag active on tile A: tile B (z=10) must sort first (further back)"
    );
    assert_eq!(
        sorted_with_drag[1].id, tile_a,
        "with drag active on tile A: tile A must sort last (front-most, boosted)"
    );

    // Clear drag: restore original order.
    scene.clear_drag_active(tile_a);
    let sorted_cleared = Compositor::sort_tiles_with_drag_boost(scene.visible_tiles(), &scene);
    assert_eq!(
        sorted_cleared[0].id, tile_a,
        "after drag cleared: original order restored"
    );
    assert_eq!(
        sorted_cleared[1].id, tile_b,
        "after drag cleared: original order restored"
    );
}

/// The viewer close button is drawn and hit-testable only on the hovered agent
/// tile, at one token-driven geometry for both, and a press on it resolves to
/// `DismissTile` before the tile's own regions. Draw-command level.
#[tokio::test]
async fn tile_close_button_draw_and_hit_region_share_token_geometry() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor
        .token_map
        .insert("tile.close_button.size_px".into(), "30".into());
    compositor
        .token_map
        .insert("tile.close_button.background".into(), "#336699".into());

    let mut scene = SceneGraph::new(1280.0, 720.0);
    let tab = scene.create_tab("Main", 0).unwrap();
    let lease = scene.grant_lease("agent", 60_000);
    let tile = scene
        .create_tile(
            tab,
            "agent",
            lease,
            Rect::new(100.0, 100.0, 400.0, 300.0),
            10,
        )
        .unwrap();

    let rendered = tze_hud_portal::render::render(
        &empty_portal_view("text-stream://close", "Close check"),
        &tze_hud_portal::tokens::PortalVisualTokens::default(),
        tze_hud_portal::render::Size {
            width: 400.0,
            height: 300.0,
        },
        0,
    );
    scene
        .set_portal_surface(tile, rendered.surface, "agent")
        .unwrap();
    scene
        .set_tile_root_tree(tile, rendered.root, rendered.children)
        .unwrap();

    // No hover: nothing drawn, nothing to hit.
    let mut resting: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.append_tile_close_button_vertices(&scene, &mut resting, 1280.0, 720.0);
    assert!(
        !compositor
            .collect_text_items(&scene, 1280.0, 720.0)
            .iter()
            .any(|item| &*item.text == crate::renderer::token_colors::TILE_CLOSE_GLYPH),
        "no x mark without hover"
    );
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    assert!(resting.is_empty());
    assert!(scene.overlay.zone_hit_regions.is_empty());

    compositor.tile_close_hover = Some(tile);
    let mut vertices: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.append_tile_close_button_vertices(&scene, &mut vertices, 1280.0, 720.0);
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);

    let region = scene
        .overlay
        .zone_hit_regions
        .iter()
        .find(|r| matches!(r.kind, ZoneInteractionKind::DismissTile { .. }))
        .expect("hovered tile must register a close hit region");
    let b = region.bounds;
    assert_eq!((b.width, b.height), (30.0, 30.0), "size comes from tokens");
    assert!(
        b.x + b.width <= 500.0 && b.y >= 100.0 && b.x > 100.0,
        "button sits inside the tile's top-right corner: {b:?}"
    );
    let tokens = crate::renderer::token_colors::resolve_tile_close_tokens(&compositor.token_map);
    let fill = compositor.gpu_color(tokens.background);
    let quad = crate::pipeline::rect_vertices(b.x, b.y, b.width, b.height, 1280.0, 720.0, fill);
    assert!(
        vertices.windows(6).any(|w| w
            .iter()
            .zip(&quad)
            .all(|(a, q)| a.position == q.position && a.color == q.color)),
        "token-filled quad must be drawn at the hit region bounds"
    );
    let glyph = compositor
        .collect_text_items(&scene, 1280.0, 720.0)
        .into_iter()
        .find(|item| &*item.text == crate::renderer::token_colors::TILE_CLOSE_GLYPH)
        .expect("hovered tile's close button draws its x mark as text");
    assert_eq!(
        (
            glyph.clip_pixel_x,
            glyph.clip_pixel_y,
            glyph.clip_bounds_width
        ),
        (b.x, b.y, b.width),
        "x mark is clipped to the button"
    );

    let handles = compositor.collect_drag_handle_entries(&scene, 1280.0, 720.0);
    compositor.populate_drag_handle_hit_regions_from(&mut scene, handles);
    let hit = scene.hit_test(b.x + b.width / 2.0, b.y + b.height / 2.0);
    assert!(
        matches!(
            hit,
            HitResult::ZoneInteraction {
                kind: ZoneInteractionKind::DismissTile { tile_id },
                ..
            } if tile_id == tile
        ),
        "press on the button resolves to DismissTile, got {hit:?}"
    );
    for (x, y) in [(100.0, 100.0), (870.0, 400.0)] {
        scene
            .update_tile_bounds(tile, Rect::new(x, y, 400.0, 300.0), "agent")
            .unwrap();
        let handles = compositor.collect_drag_handle_entries(&scene, 1280.0, 720.0);
        compositor.populate_drag_handle_hit_regions_from(&mut scene, handles);
        compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
        let close = scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|r| {
                matches!(r.kind,
            ZoneInteractionKind::DismissTile { tile_id } if tile_id == tile)
            })
            .unwrap();
        assert!(
            matches!(scene.hit_test(close.bounds.x + 15.0, close.bounds.y + 15.0),
            HitResult::ZoneInteraction { kind: ZoneInteractionKind::DismissTile { tile_id }, .. } if tile_id == tile)
        );
        assert!(
            matches!(scene.hit_test(x + 150.0, y + 10.0), HitResult::ZoneInteraction {
            kind: ZoneInteractionKind::DragHandle { element_id, is_header_band: true, .. }, .. } if element_id == tile)
        );
    }
    scene
        .update_tile_bounds(tile, Rect::new(100.0, 100.0, 400.0, 300.0), "agent")
        .unwrap();
    assert!(
        matches!(scene.hit_test(150.0, 350.0), HitResult::TileHit { .. }),
        "the rest of the tile is unchanged"
    );
}

/// Zoom (`ui.scale`) and the monitor scale reach hit testing through
/// `runtime.px_scale`: at 1.5 and 1.875 px per dp every dismiss and action
/// region's top and height (so the stack slot pitch), and the dismiss
/// button's size and right inset, are exactly that multiple of their 1x
/// values (hud-h51u7.3 AC1,
/// AC3). A zone on a secondary monitor uses that monitor's scale. Pure
/// geometry: runs without a GPU.
#[test]
fn zone_hit_regions_and_slot_pitch_scale_with_px_scale() {
    use tze_hud_config::tokens::PX_SCALE_KEY;
    use tze_hud_scene::types::NotificationAction;
    const ZONE_RIGHT: f32 = 1920.0 * 0.75 + 1920.0 * 0.24;

    /// Every region as (right inset, top, width, height), top to bottom.
    fn regions(tokens: &[(&str, &str)]) -> Vec<[f32; 4]> {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        scene.create_tab("Main", 0).unwrap();
        scene.register_zone(tze_hud_scene::types::ZoneDefinition {
            id: SceneId::new(),
            name: "notif".to_string(),
            description: "Stack zone".to_string(),
            geometry_policy: tze_hud_scene::types::GeometryPolicy::Relative {
                x_pct: 0.75,
                y_pct: 0.0,
                width_pct: 0.24,
                height_pct: 0.60,
            },
            accepted_media_types: vec![tze_hud_scene::types::ZoneMediaType::ShortTextWithIcon],
            // No sizes: the 16 dp font and 8 dp margin fallbacks apply.
            rendering_policy: tze_hud_scene::types::RenderingPolicy::default(),
            contention_policy: ContentionPolicy::Stack { max_depth: 5 },
            max_publishers: 8,
            auto_clear_ms: None,
            layer_attachment: tze_hud_scene::types::LayerAttachment::Chrome,
            ephemeral: false,
        });
        let action = |label: &str| NotificationAction {
            label: label.to_string(),
            callback_id: Some(label.to_lowercase()),
            url: None,
        };
        for (text, actions) in [
            ("first", vec![action("Open"), action("Snooze")]),
            ("second", Vec::new()),
        ] {
            scene
                .publish_to_zone(
                    "notif",
                    ZoneContent::Notification(NotificationPayload {
                        text: text.to_string(),
                        icon: String::new(),
                        urgency: 1,
                        ttl_ms: None,
                        title: String::new(),
                        actions,
                    }),
                    "agent-a",
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        let tokens: HashMap<String, String> = tokens
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        super::super::hit_regions::populate_notification_hit_regions(
            &mut scene, 1920.0, 1080.0, &tokens,
        );
        let original = scene.overlay.zone_hit_regions.clone();
        let zone = scene.zone_registry.zones["notif"].clone();
        let version = scene.version;
        let placement = GeometryPolicy::Relative {
            x_pct: 0.5,
            y_pct: 0.2,
            width_pct: 0.24,
            height_pct: 0.6,
        };
        assert!(scene.set_zone_placement(zone.id, placement));
        let moved_epoch = scene.geometry_epoch;
        assert!(!scene.set_zone_placement(zone.id, placement));
        assert_eq!(scene.geometry_epoch, moved_epoch);
        super::super::hit_regions::populate_notification_hit_regions(
            &mut scene, 1920.0, 1080.0, &tokens,
        );
        for (before, after) in original.iter().zip(&scene.overlay.zone_hit_regions) {
            assert_eq!(before.interaction_id, after.interaction_id);
            assert!((after.bounds.x - before.bounds.x + 480.0).abs() < 0.01);
            assert!((after.bounds.y - before.bounds.y - 216.0).abs() < 0.01);
            assert_eq!(
                (after.bounds.width, after.bounds.height),
                (before.bounds.width, before.bounds.height)
            );
        }
        assert!(scene.reset_zone_placement(zone.id));
        let reset_epoch = scene.geometry_epoch;
        assert!(!scene.reset_zone_placement(zone.id));
        assert_eq!(scene.geometry_epoch, reset_epoch);
        assert_eq!(scene.version, version);
        super::super::hit_regions::populate_notification_hit_regions(
            &mut scene, 1920.0, 1080.0, &tokens,
        );
        assert_eq!(scene.overlay.zone_hit_regions, original);
        let mut out: Vec<[f32; 4]> = scene
            .overlay
            .zone_hit_regions
            .iter()
            .map(|r| {
                let b = r.bounds;
                [ZONE_RIGHT - (b.x + b.width), b.y, b.width, b.height]
            })
            .collect();
        out.sort_by(|a, b| a[1].total_cmp(&b[1]).then(a[0].total_cmp(&b[0])));
        out
    }

    let one = regions(&[(PX_SCALE_KEY, "1")]);
    assert_eq!(one.len(), 4, "two dismisses and two actions: {one:?}");
    for factor in [1.5f32, 1.875] {
        let scaled = regions(&[(PX_SCALE_KEY, &factor.to_string())]);
        assert_eq!(scaled.len(), one.len());
        for (r1, rs) in one.iter().zip(&scaled) {
            // Action buttons split the zone's width (a display fraction),
            // so only the square dismiss button's width and right inset
            // scale.
            let dismiss = r1[2] == r1[3];
            let horizontal = if dismiss { &[0, 2][..] } else { &[] };
            for &i in [1, 3].iter().chain(horizontal) {
                assert!(
                    (rs[i] - r1[i] * factor).abs() < 0.01,
                    "at {factor}x: {rs:?} is not {r1:?} x {factor}"
                );
            }
        }
    }
    let secondary = regions(&[(PX_SCALE_KEY, "1.5"), ("runtime.px_scale@notif", "1")]);
    assert_eq!(secondary, one, "a 1x secondary ignores the 1.5x primary");
}

/// The published pip zone gets a resize grip in its corner toward the display
/// centre, beside its move handle; other zones get only the move handle.
#[tokio::test]
async fn pip_zone_gets_a_resize_grip_in_the_corner_toward_the_centre() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let (sw, sh) = (1920.0, 1080.0);
    let mut scene = SceneGraph::new(sw, sh);
    let mut ids = Vec::new();
    for (name, x_pct) in [(PIP_ZONE, 0.70), ("other-zone", 0.05)] {
        let id = SceneId::new();
        ids.push(id);
        scene.register_zone(ZoneDefinition {
            id,
            name: name.to_string(),
            description: name.to_string(),
            geometry_policy: GeometryPolicy::Relative {
                x_pct,
                y_pct: 0.70,
                width_pct: 0.25,
                height_pct: 0.25,
            },
            accepted_media_types: vec![ZoneMediaType::StreamText],
            rendering_policy: RenderingPolicy::default(),
            contention_policy: ContentionPolicy::LatestWins,
            max_publishers: 1,
            auto_clear_ms: None,
            layer_attachment: LayerAttachment::Chrome,
            ephemeral: false,
        });
        scene
            .publish_to_zone(
                name,
                ZoneContent::StreamText("shown".to_string()),
                "agent-a",
                None,
                None,
                None,
            )
            .unwrap();
    }
    populate_drag_handles(&compositor, &mut scene, sw, sh);

    let regions = &scene.overlay.drag_handle_hit_regions;
    let grips: Vec<_> = regions.iter().filter(|r| r.resize_grip.is_some()).collect();
    assert_eq!(grips.len(), 1, "only the pip has a grip: {regions:?}");
    let grip = grips[0];
    assert_eq!(grip.element_id, ids[0]);
    assert!(grip.interaction_id.starts_with(RESIZE_GRIP_PREFIX));
    // The pip sits right of and below the centre: its grip is top-left.
    assert_eq!(grip.resize_grip, Some((true, true)));
    let pip = Rect::new(0.70 * sw, 0.70 * sh, 0.25 * sw, 0.25 * sh);
    assert!((grip.bounds.x - pip.x).abs() < 0.5 && (grip.bounds.y - pip.y).abs() < 0.5);
    for id in &ids {
        assert_eq!(
            regions
                .iter()
                .filter(|r| r.element_id == *id && r.resize_grip.is_none())
                .count(),
            1,
            "each zone keeps one move handle"
        );
    }
}

/// One published pip at `(x_pct, y_pct)` (25%×25% of 1920×1080) with its
/// drag-handle regions populated.
fn pip_regions(
    compositor: &Compositor,
    scene: &mut SceneGraph,
    x_pct: f32,
    y_pct: f32,
) -> (SceneId, Rect) {
    let (sw, sh) = (1920.0, 1080.0);
    let id = SceneId::new();
    scene.register_zone(ZoneDefinition {
        id,
        name: PIP_ZONE.to_string(),
        description: "pip".to_string(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct,
            y_pct,
            width_pct: 0.25,
            height_pct: 0.25,
        },
        accepted_media_types: vec![ZoneMediaType::StreamText],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::LatestWins,
        max_publishers: 1,
        auto_clear_ms: None,
        layer_attachment: LayerAttachment::Chrome,
        ephemeral: false,
    });
    scene
        .publish_to_zone(
            PIP_ZONE,
            ZoneContent::StreamText("shown".to_string()),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();
    populate_drag_handles(compositor, scene, sw, sh);
    (id, Rect::new(x_pct * sw, y_pct * sh, 0.25 * sw, 0.25 * sh))
}

fn grip_region(scene: &SceneGraph) -> DragHandleHitRegion {
    scene
        .overlay
        .drag_handle_hit_regions
        .iter()
        .find(|r| r.resize_grip.is_some())
        .cloned()
        .expect("the pip has a grip")
}

/// In each display quadrant the grip takes the pip corner facing the centre,
/// its region sits in that corner, and its glyph fits inside the square.
#[tokio::test]
async fn pip_grip_takes_the_corner_facing_the_centre_in_every_quadrant() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    for ((x_pct, y_pct), corner) in [
        ((0.70, 0.70), (true, true)),   // bottom-right quadrant: grip top-left
        ((0.05, 0.05), (false, false)), // top-left quadrant: grip bottom-right
        ((0.70, 0.05), (true, false)),  // top-right quadrant: grip bottom-left
        ((0.05, 0.70), (false, true)),  // bottom-left quadrant: grip top-right
    ] {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let (_, pip) = pip_regions(&compositor, &mut scene, x_pct, y_pct);
        let grip = grip_region(&scene);
        assert_eq!(grip.resize_grip, Some(corner), "pip at {x_pct},{y_pct}");
        let b = grip.bounds;
        let (left, top) = corner;
        assert!(
            (if left {
                b.x - pip.x
            } else {
                b.x + b.width - (pip.x + pip.width)
            })
            .abs()
                < 0.5
        );
        assert!(
            (if top {
                b.y - pip.y
            } else {
                b.y + b.height - (pip.y + pip.height)
            })
            .abs()
                < 0.5
        );
        let dots: Vec<_> = Compositor::resize_grip_glyph(b, corner).collect();
        assert_eq!(dots.len(), 6);
        for (x, y, side) in dots {
            assert!(
                x >= b.x && y >= b.y && x + side <= b.x + b.width && y + side <= b.y + b.height,
                "glyph dot ({x},{y},{side}) escapes the grip {b:?}"
            );
        }
        // The staircase hugs the grip's own corner: that corner cell has a
        // dot, the opposite one has none.
        let cell = b.width / 3.0;
        let has_dot_in = |(l, t): (bool, bool)| {
            let (x0, y0) = (
                if l { b.x } else { b.x + 2.0 * cell },
                if t { b.y } else { b.y + 2.0 * cell },
            );
            Compositor::resize_grip_glyph(b, corner)
                .any(|(x, y, _)| x >= x0 && x < x0 + cell && y >= y0 && y < y0 + cell)
        };
        assert!(has_dot_in(corner), "corner cell");
        assert!(!has_dot_in((!left, !top)), "opposite cell");
    }
}

/// While its drag is active the grip keeps its emitted corner, even when the
/// pip's box has moved past the display centre.
#[tokio::test]
async fn pip_grip_keeps_its_corner_while_dragging() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let (id, _) = pip_regions(&compositor, &mut scene, 0.70, 0.70);
    assert_eq!(grip_region(&scene).resize_grip, Some((true, true)));
    // Grow the pip up-left past the centre (as a grip resize does).
    scene.set_zone_placement(
        id,
        GeometryPolicy::Relative {
            x_pct: 0.02,
            y_pct: 0.02,
            width_pct: 0.85,
            height_pct: 0.85,
        },
    );
    scene.set_drag_active(id);
    populate_drag_handles(&compositor, &mut scene, 1920.0, 1080.0);
    assert_eq!(
        grip_region(&scene).resize_grip,
        Some((true, true)),
        "frozen"
    );
    scene.clear_drag_active(id);
    populate_drag_handles(&compositor, &mut scene, 1920.0, 1080.0);
    assert_eq!(
        grip_region(&scene).resize_grip,
        Some((false, false)),
        "after release it faces the centre again"
    );
}
