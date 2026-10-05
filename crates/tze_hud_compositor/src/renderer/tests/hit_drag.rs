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

    // Dismiss should be at the top-right of the slot.
    // Zone geometry: zx = 1920*0.75 = 1440, zw = 1920*0.24 = 460.8.
    // Dismiss x = zx + zw - 20 = 1880.8.
    let expected_x = sw * 0.75 + sw * 0.24 - 20.0;
    assert!(
        (region.bounds.x - expected_x).abs() < 1.0,
        "dismiss x must be at top-right (expected≈{expected_x:.1}, got {:.1})",
        region.bounds.x
    );
    assert!(
        region.bounds.y < 1.0,
        "dismiss y must be near top of slot (expected≈0, got {:.1})",
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
                        callback_id: "yes".to_string(),
                    },
                    NotificationAction {
                        label: "No".to_string(),
                        callback_id: "no".to_string(),
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
            tze_hud_scene::types::ZoneInteractionKind::Action { callback_id }
                if callback_id == "yes"
        ),
        "second region must be Action(yes)"
    );
    assert!(
        matches!(
            &scene.overlay.zone_hit_regions[2].kind,
            tze_hud_scene::types::ZoneInteractionKind::Action { callback_id }
                if callback_id == "no"
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
                        callback_id: "a".to_string(),
                    },
                    NotificationAction {
                        label: "B".to_string(),
                        callback_id: "b".to_string(),
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
async fn drag_handle_regions_cover_visible_tile_zone_and_widget() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

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
    assert!(
        kinds.contains(&DragHandleElementKind::Widget),
        "widget handle missing"
    );
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

/// Stale entries in `drag_handle_states` must be pruned when the
/// corresponding element is removed from the scene.
///
/// Verifies that the per-frame drag-handle population retains only the keys
/// that are still present in the current `drag_handle_hit_regions` set —
/// the zero-allocation `iter().any()` retain used after [hud-tdtr7] must
/// have identical semantics to the previous `HashSet`-based approach.
#[tokio::test]
async fn drag_handle_states_stale_entries_pruned_on_repopulate() {
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

    // Seed drag_handle_states: one live entry + one stale phantom that
    // never had a corresponding hit region.
    scene
        .overlay
        .drag_handle_states
        .entry(live_id.clone())
        .or_default()
        .hovered = true;
    scene.overlay.drag_handle_states.insert(
        "drag-handle:tile:ghost-never-existed".to_string(),
        Default::default(),
    );
    assert_eq!(scene.overlay.drag_handle_states.len(), 2);

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
        scene.overlay.drag_handle_states.is_empty(),
        "stale drag_handle_states must be pruned by drag-handle repopulation; \
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
        .drag_handle_states
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

    // No hover: nothing drawn, nothing to hit.
    let mut resting: Vec<crate::pipeline::RectVertex> = Vec::new();
    compositor.append_tile_close_button_vertices(&scene, &mut resting, 1280.0, 720.0);
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
    assert!(
        matches!(scene.hit_test(150.0, 350.0), HitResult::TileHit { .. }),
        "the rest of the tile is unchanged"
    );
}
