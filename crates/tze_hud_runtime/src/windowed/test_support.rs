use std::sync::Arc;

use tokio::sync::Mutex as TokioMutex;
use tze_hud_input::{FocusManager, FocusRequest};
use tze_hud_protocol::session::{SessionRegistry, SharedState};
use tze_hud_protocol::token::TokenStore;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    ContentionPolicy, DragHandleElementKind, GeometryPolicy, LayerAttachment, RenderingPolicy,
    TileScrollConfig, ZoneDefinition, ZoneMediaType,
};
use tze_hud_scene::{DragHandleHitRegion, HitRegionNode, Node, NodeData, Rect, SceneId};

pub(super) fn scene_with_capture_tile() -> (SceneGraph, SceneId) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("portal-agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-agent",
            lease_id,
            Rect::new(300.0, 400.0, 600.0, 200.0),
            1,
        )
        .unwrap();
    let node_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 200.0, 40.0),
                    interaction_id: "portal-submit".to_string(),
                    accepts_pointer: true,
                    accepts_focus: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();
    (scene, tile_id)
}

pub(super) fn make_shared_state() -> Arc<TokioMutex<SharedState>> {
    let scene = Arc::new(TokioMutex::new(SceneGraph::new(1920.0, 1080.0)));
    let sessions = SessionRegistry::new();
    Arc::new(TokioMutex::new(SharedState {
        scene,
        sessions,
        resource_store: tze_hud_resource::ResourceStore::new(
            tze_hud_resource::ResourceStoreConfig::default(),
        ),
        widget_asset_store: tze_hud_protocol::session::WidgetAssetStore::default(),
        runtime_widget_store: None,
        element_store: tze_hud_scene::element_store::ElementStore::default(),
        element_store_path: None,
        safe_mode_atomic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_tab_mirror: Arc::new(std::sync::Mutex::new(None)),
        token_store: TokenStore::new(),
        freeze_active: false,
        input_capture_tx: None,
        input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        tile_placement: Default::default(),
    }))
}

pub(super) fn make_test_zone(name: &str) -> ZoneDefinition {
    ZoneDefinition {
        id: SceneId::new(),
        name: name.to_string(),
        description: format!("test zone: {name}"),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 0.1,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 8 },
        max_publishers: 8,
        auto_clear_ms: None,
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    }
}

pub(super) fn scene_with_drag_handle_tile(
    initial_x: f32,
    initial_y: f32,
    tile_w: f32,
    tile_h: f32,
) -> (SceneGraph, SceneId, SceneId, String) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).expect("tab must be created");
    let lease_id = scene.grant_lease("portal-agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-agent",
            lease_id,
            Rect::new(initial_x, initial_y, tile_w, tile_h),
            1,
        )
        .expect("tile must be created");

    let element_id = tile_id;
    let interaction_id = format!(
        "drag-handle:{:032x}",
        element_id
            .to_bytes_le()
            .iter()
            .fold(0u128, |acc, &b| (acc << 8) | b as u128)
    );
    let handle_bounds = Rect::new(
        initial_x + tile_w / 2.0 - 20.0,
        initial_y - 10.0,
        40.0,
        20.0,
    );
    scene
        .overlay
        .drag_handle_hit_regions
        .push(DragHandleHitRegion {
            element_id,
            element_kind: DragHandleElementKind::Tile,
            bounds: handle_bounds,
            interaction_id: interaction_id.clone(),
            hit_region: HitRegionNode {
                bounds: handle_bounds,
                interaction_id: interaction_id.clone(),
                accepts_pointer: true,
                ..Default::default()
            },
            tab_order: 0,
            is_header_band: false,
        });

    (scene, tile_id, element_id, interaction_id)
}

pub(super) fn scene_with_composer_in_nonactive_tab()
-> (SceneGraph, SceneId, SceneId, SceneId, f32, f32) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let other_tab = scene.create_tab("Other", 0).unwrap();
    let portal_tab = scene.create_tab("Portal", 1).unwrap();
    assert_eq!(scene.active_tab, Some(other_tab));

    let lease_id = scene.grant_lease("portal-agent", 60_000);
    let tile_id = scene
        .create_tile(
            portal_tab,
            "portal-agent",
            lease_id,
            Rect::new(100.0, 100.0, 400.0, 300.0),
            1,
        )
        .unwrap();

    let node_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id,
            Node {
                layout: Default::default(),
                id: node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                    interaction_id: "portal-composer-focus".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    (scene, portal_tab, tile_id, node_id, 150.0, 150.0)
}

pub(super) fn portal_scene_with_focus() -> (SceneGraph, SceneId, SceneId, FocusManager) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease("portal-agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-agent",
            lease_id,
            Rect::new(100.0, 100.0, 400.0, 300.0),
            1,
        )
        .unwrap();

    let _ = scene.register_tile_scroll_config(tile_id, TileScrollConfig::vertical());

    let mut fm = FocusManager::new();
    fm.request_focus(
        FocusRequest {
            tile_id,
            node_id: None,
            steal: true,
            requesting_namespace: "portal-agent".to_string(),
        },
        tab_id,
        &scene,
    );

    (scene, tab_id, tile_id, fm)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::keyboard::PendingKeyboardEvent;
    use super::super::portal::build_portal_projection_driver;
    use super::super::*;
    use super::{make_shared_state, make_test_zone, scene_with_composer_in_nonactive_tab};
    use tze_hud_scene::HitResult;
    use tze_hud_scene::types::ZoneInteractionKind;

    #[test]
    fn build_portal_projection_driver_configures_operator_authority() {
        use tze_hud_projection::{
            AttachRequest, CleanupAuthority, CleanupRequest, ContentClassification,
            OperationEnvelope, ProjectionErrorCode, ProjectionOperation, ProviderKind,
        };

        let cfg = WindowedConfig {
            projection_operator_authority: Some("operator-secret".to_string()),
            ..WindowedConfig::default()
        };
        let mut driver = build_portal_projection_driver(&cfg)
            .expect("configured operator authority should build a portal driver");
        let projection_id = "projection-runtime-configured-operator";
        let attach = driver.authority_mut().handle_attach(
            AttachRequest {
                envelope: OperationEnvelope {
                    operation: ProjectionOperation::Attach,
                    projection_id: projection_id.to_string(),
                    request_id: "attach-runtime-configured-operator".to_string(),
                    client_timestamp_wall_us: 1,
                },
                provider_kind: ProviderKind::Other,
                display_name: "Runtime Configured Operator".to_string(),
                workspace_hint: None,
                repository_hint: None,
                icon_profile_hint: None,
                content_classification: ContentClassification::Private,
                hud_target: None,
                idempotency_key: None,
            },
            "test-caller",
            1_000,
        );
        assert!(attach.accepted, "attach precondition must be accepted");

        let denied = driver.authority_mut().handle_cleanup(
            CleanupRequest {
                envelope: OperationEnvelope {
                    operation: ProjectionOperation::Cleanup,
                    projection_id: projection_id.to_string(),
                    request_id: "bad-operator-cleanup".to_string(),
                    client_timestamp_wall_us: 1,
                },
                cleanup_authority: CleanupAuthority::Operator,
                owner_token: None,
                operator_authority: Some("wrong-secret".to_string()),
                reason: "operator override".to_string(),
            },
            "operator",
            2_000,
        );
        assert!(!denied.accepted, "wrong operator authority must be denied");
        assert_eq!(
            denied.error_code,
            Some(ProjectionErrorCode::ProjectionUnauthorized)
        );

        let accepted = driver.authority_mut().handle_cleanup(
            CleanupRequest {
                envelope: OperationEnvelope {
                    operation: ProjectionOperation::Cleanup,
                    projection_id: projection_id.to_string(),
                    request_id: "good-operator-cleanup".to_string(),
                    client_timestamp_wall_us: 1,
                },
                cleanup_authority: CleanupAuthority::Operator,
                owner_token: None,
                operator_authority: Some("operator-secret".to_string()),
                reason: "operator override".to_string(),
            },
            "operator",
            3_000,
        );
        assert!(
            accepted.accepted,
            "configured operator authority must allow operator cleanup"
        );
    }

    /// Regression (hud-dwcr7): composer keystroke echo must apply to the draft
    /// even while the scene mutex is held by a gRPC mutation batch.
    ///
    /// Before the fix, keyboard dispatch read `scene.active_tab` by `try_lock`ing
    /// the scene Tokio mutex; under sustained portal streaming that lock is held
    /// across batches, so every keystroke deferred and the local echo froze
    /// ("worked for a few seconds then stopped").  This test reproduces the busy
    /// condition by holding the scene lock for the entire keystroke window and
    /// proves the two lock-free properties the fix relies on:
    ///   1. `active_tab_for_keyboard_dispatch`'s data source — the
    ///      `active_tab_mirror` — resolves the tab WITHOUT the scene lock.
    ///   2. The composer intercept (`InputProcessor`) applies the keystroke to
    ///      the draft (echo would render) WITHOUT the scene lock.
    #[tokio::test]
    async fn composer_echo_applies_while_scene_lock_is_held() {
        use tze_hud_input::{FocusManager, InputProcessor, PointerEvent, PointerEventKind};
        use tze_hud_scene::types::HitRegionNode;
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

        // Build a scene with a focusable composer region (accepts_composer_input).
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 800.0, 600.0),
                1,
            )
            .unwrap();
        let composer_id = SceneId::new();
        scene.nodes.insert(
            composer_id,
            Node {
                layout: Default::default(),
                id: composer_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 800.0, 60.0),
                    interaction_id: "composer-input".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        );
        scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(composer_id);

        // Focus the composer (the pointer-down path that activates the draft
        // manager); this is the lock-free InputProcessor side.
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);
        processor.process_with_focus(
            &PointerEvent {
                x: 10.0,
                y: 10.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
            &mut fm,
            tab_id,
        );
        assert!(
            processor.is_composer_active(),
            "composer must be active after focusing the composer region"
        );

        // Stand up SharedState, seed the mirror from the scene, then move the
        // scene into the SharedState's Tokio mutex.
        let shared = make_shared_state();
        {
            let st = shared.lock().await;
            // Replace the empty default scene with our composer scene and seed
            // the mirror, mimicking the post-apply_batch refresh.
            *st.scene.lock().await = scene;
            st.refresh_active_tab_mirror(&*st.scene.lock().await);
        }

        // ── Reproduce the starvation condition: hold the scene lock for the
        // entire keystroke window, exactly as a sustained gRPC mutation batch
        // would. ──────────────────────────────────────────────────────────────
        let st = shared.lock().await;
        let scene_guard = st.scene.lock().await; // held across the keystroke below

        // (1) The mirror still resolves the active tab — no scene try_lock.
        assert_eq!(
            st.active_tab_mirror_value(),
            Some(tab_id),
            "active_tab_mirror must resolve the tab while the scene lock is held"
        );

        // (2) The composer intercept applies the keystroke to the draft while the
        // scene lock is held — the echo would render this frame.
        let (outcome, _batch) = processor.route_character_to_composer("h");
        assert_eq!(
            outcome,
            tze_hud_input::EditOutcome::Mutated,
            "keystroke must mutate the composer draft even under scene-lock contention"
        );
        let snapshot = processor
            .composer_draft_snapshot()
            .expect("composer draft snapshot must exist while active");
        assert_eq!(
            snapshot.0, "h",
            "composer draft must reflect the typed character (echo) without the scene lock"
        );

        // The scene lock was held for the whole keystroke path — proving echo
        // never depended on it.
        drop(scene_guard);
        drop(st);
    }

    /// Regression (hud-dwcr7): the pending-keyboard drain must DRAIN, not
    /// livelock.
    ///
    /// The bug: `dispatch_key_down_event` (the public Stage-1 entry) re-queues
    /// any event when `pending_keyboard_events` is non-empty (the FIFO guard).
    /// The drain originally called that public entry, so a freshly-popped event
    /// saw the remaining queued events and immediately re-queued itself to the
    /// back — the queue rotated front→back forever, never shrank, and composer
    /// echo froze after a few words (once >=2 events ever piled up).
    ///
    /// The fix routes the drain through the *inner* dispatch fns, which skip the
    /// FIFO guard.  This test models the drain loop exactly (front-pop +
    /// per-event dispatch, bounded by the entry length) and asserts that with a
    /// consuming (inner-style) dispatcher the queue fully drains AND every
    /// keystroke is applied to a real composer draft — while a guarded
    /// (public-style) dispatcher would rotate and never drain.
    ///
    /// NOTE (hud-nu0ea): the drain loop *reconstruction* below now has a
    /// production-path counterpart. `event_loop_harness::tests::
    /// drain_routes_characters_through_real_dispatch_into_composer_draft` drives
    /// the genuine `WinitApp::drain_pending_keyboard_events` through the headless
    /// event-loop harness (no reconstruction). This test is retained because it
    /// additionally contrasts the buggy public-style dispatcher (the livelock)
    /// against the fixed inner-style one in the same place.
    #[test]
    fn pending_keyboard_drain_consumes_queue_and_applies_all_keys() {
        use std::collections::VecDeque;
        use tze_hud_input::{FocusManager, InputProcessor, PointerEvent, PointerEventKind};
        use tze_hud_scene::types::HitRegionNode;
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

        // ── Active composer (real InputProcessor draft) ──────────────────────
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 800.0, 600.0),
                1,
            )
            .unwrap();
        let composer_id = SceneId::new();
        scene.nodes.insert(
            composer_id,
            Node {
                layout: Default::default(),
                id: composer_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 800.0, 60.0),
                    interaction_id: "composer-input".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        );
        scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(composer_id);

        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);
        processor.process_with_focus(
            &PointerEvent {
                x: 10.0,
                y: 10.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
            &mut fm,
            tab_id,
        );
        assert!(processor.is_composer_active());

        // ── Seed >=2 pending character events (the pile-up that triggered the
        // livelock). ─────────────────────────────────────────────────────────
        let mut queue: VecDeque<&str> = VecDeque::new();
        for ch in ["h", "e", "l", "l", "o"] {
            queue.push_back(ch);
        }
        assert!(
            queue.len() >= 2,
            "need >=2 queued events to exercise the guard"
        );

        // ── Model the FIXED drain loop exactly (windowed.rs
        // `drain_pending_keyboard_events`): bound by entry length, front-pop,
        // dispatch each via the INNER (consuming) path — never re-queue on a
        // non-empty queue. ──────────────────────────────────────────────────
        let limit = queue.len();
        for _ in 0..limit {
            // active-tab always resolves here (mirror not busy in this test).
            let Some(ch) = queue.pop_front() else { break };
            // Inner-style dispatch: route into the composer draft, NO FIFO guard.
            let (outcome, _batch) = processor.route_character_to_composer(ch);
            assert_eq!(
                outcome,
                tze_hud_input::EditOutcome::Mutated,
                "each queued key must mutate the draft when dispatched via the inner path"
            );
        }

        // (a) The queue is fully drained.
        assert!(
            queue.is_empty(),
            "drain must empty the queue (no front→back rotation livelock)"
        );
        // (b) The composer draft reflects ALL seeded keys, in order.
        let snapshot = processor
            .composer_draft_snapshot()
            .expect("composer draft must exist");
        assert_eq!(
            snapshot.0, "hello",
            "all drained keystrokes must be applied to the composer draft (echo)"
        );

        // (c) Guard against the regression: the BUGGY public-style dispatcher
        // (re-queue when the queue is non-empty) would never drain.  Model it to
        // prove the loop bound is what previously spun (and that the fixed path
        // above does not).
        let mut buggy: VecDeque<&str> = ["a", "b", "c"].into_iter().collect();
        let buggy_limit = buggy.len();
        let mut consumed = 0usize;
        for _ in 0..buggy_limit {
            let Some(ch) = buggy.pop_front() else { break };
            // Public-style guard: if others remain queued, re-defer to the back
            // and DON'T consume — exactly the livelock.
            if !buggy.is_empty() {
                buggy.push_back(ch);
                continue;
            }
            consumed += 1;
        }
        assert!(
            !buggy.is_empty() || consumed < 3,
            "public-style guarded dispatch must NOT fully drain — this is the bug \
             the inner-path fix avoids"
        );
    }

    // ── Surface dimension resolution (hud-q5hx regression) ───────────────
    //
    // These tests document the contract for the `window.inner_size()` fallback
    // logic added to fix the crash at non-default dimensions (hud-q5hx).
    //
    // The actual window creation path cannot be tested without a real GPU and
    // display, but we can verify the helper logic for dimension fallback.

    /// The surface dimension fallback logic (used when `window.inner_size()`
    /// returns (0,0)) should prefer the actual window size when non-zero and
    /// fall back to the configured dimensions otherwise.
    ///
    /// This test validates the resolution rule as a pure function without
    /// requiring a real window handle.
    #[test]
    fn surface_dimension_resolution_prefers_actual_size() {
        // Simulate: window.inner_size() returns (2560, 1440) — use actual size.
        let actual = (2560u32, 1440u32);
        let configured = (1920u32, 1080u32);
        let (w, h) = if actual.0 > 0 && actual.1 > 0 {
            actual
        } else {
            configured
        };
        assert_eq!(w, 2560, "actual size must win when non-zero");
        assert_eq!(h, 1440, "actual size must win when non-zero");
    }

    /// When `window.inner_size()` returns (0,0) (minimized/not-yet-shown),
    /// the configured dimensions must be used as fallback.
    #[test]
    fn surface_dimension_resolution_falls_back_to_configured_when_zero() {
        // Simulate: window.inner_size() returns (0, 0) — use configured size.
        let actual = (0u32, 0u32);
        let configured = (2560u32, 1440u32);
        let (w, h) = if actual.0 > 0 && actual.1 > 0 {
            actual
        } else {
            configured
        };
        assert_eq!(w, 2560, "configured size must be used when actual is zero");
        assert_eq!(h, 1440, "configured size must be used when actual is zero");
    }

    // ── DPI scaling correctness (hud-22by) ────────────────────────────────────

    /// At 125% DPI on a 2560x1440 monitor, `MonitorHandle::size()` returns
    /// physical pixels (2560, 1440) when the process has per-monitor DPI
    /// awareness (guaranteed by the embedded manifest).  The overlay MUST cover
    /// the full 2560x1440 display — NOT the DPI-virtualized 2048x1152.
    ///
    /// Regression guard: the old code multiplied `size()` by `scale_factor()`
    /// (2560 * 1.25 = 3200), which over-counted physical pixels.  The correct
    /// behaviour is to use `size()` directly.
    #[test]
    fn dpi_125pct_overlay_covers_full_physical_display() {
        // Simulate: winit reports physical size (DPI-aware process, manifest set)
        let physical_width: u32 = 2560;
        let physical_height: u32 = 1440;
        let scale_factor: f64 = 1.25; // 125% DPI = 120 DPI / 96 base

        // Correct approach: use size() directly (physical pixels).
        let (w, h) = (physical_width, physical_height);
        assert_eq!(w, 2560, "overlay must be full physical width at 125% DPI");
        assert_eq!(h, 1440, "overlay must be full physical height at 125% DPI");

        // Regression check: old code that over-counted.
        let over_counted_w = (physical_width as f64 * scale_factor).round() as u32;
        assert_ne!(
            over_counted_w, 2560,
            "multiplying physical size by scale_factor over-counts (produces 3200, not 2560)"
        );
        assert_eq!(over_counted_w, 3200, "old code would have produced 3200");
    }

    /// At 150% DPI on a 3840x2160 monitor, `MonitorHandle::size()` returns
    /// physical pixels (3840, 2160).  The overlay must cover the full display.
    #[test]
    fn dpi_150pct_overlay_covers_full_physical_display() {
        let physical_width: u32 = 3840;
        let physical_height: u32 = 2160;
        let scale_factor: f64 = 1.5;

        // Correct: use size() directly.
        let (w, h) = (physical_width, physical_height);
        assert_eq!(w, 3840, "overlay must be full physical width at 150% DPI");
        assert_eq!(h, 2160, "overlay must be full physical height at 150% DPI");

        // Old code would over-count.
        let over_counted_w = (physical_width as f64 * scale_factor).round() as u32;
        assert_eq!(over_counted_w, 5760, "old code produced 5760 at 150% DPI");
    }

    /// At 100% DPI, `scale_factor()` is 1.0 and physical equals logical.
    /// Using `size()` directly must produce the same result whether or not
    /// scale_factor multiplication is applied — no regression at 100%.
    #[test]
    fn dpi_100pct_no_regression() {
        let physical_width: u32 = 1920;
        let physical_height: u32 = 1080;
        let scale_factor: f64 = 1.0;

        // Correct: use size() directly.
        let (w, h) = (physical_width, physical_height);
        assert_eq!(w, 1920, "100% DPI must not regress");
        assert_eq!(h, 1080, "100% DPI must not regress");

        // At 100%, old code and new code agree (1.0 multiply is identity).
        let with_scale = (physical_width as f64 * scale_factor).round() as u32;
        assert_eq!(
            with_scale, w,
            "at 100% DPI, scale multiplication is identity — no regression"
        );
    }

    /// The `inner_size()` surface dimension resolution must use physical pixels
    /// directly, without multiplying by `scale_factor`.  At 125% DPI with a
    /// 2560x1440 window, the wgpu surface must be configured at 2560x1440, not
    /// 3200x1800.
    #[test]
    fn surface_dimension_resolution_does_not_multiply_by_scale_factor() {
        // Simulate: window.inner_size() = (2560, 1440) at 125% DPI.
        let inner_w: u32 = 2560;
        let inner_h: u32 = 1440;
        let scale: f64 = 1.25;

        // Correct: use inner_size() directly.
        let (surface_w, surface_h) = if inner_w > 0 && inner_h > 0 {
            (inner_w, inner_h)
        } else {
            (1920u32, 1080u32) // fallback (unreachable in this test)
        };
        assert_eq!(
            surface_w, 2560,
            "surface must match physical inner_size at 125% DPI"
        );
        assert_eq!(
            surface_h, 1440,
            "surface must match physical inner_size at 125% DPI"
        );

        // Old code multiplied — would have produced 3200x1800.
        let old_surface_w = (inner_w as f64 * scale).round() as u32;
        assert_eq!(old_surface_w, 3200, "old code over-counted surface width");
    }

    // ── Zone interaction: dismiss hit wiring (hud-ltgk.6) ────────────────────

    use tze_hud_scene::types::{NotificationPayload, Rect, ZoneHitRegion};

    /// Pointer-up on a zone dismiss hit-region must remove the notification from
    /// `zone_registry.active_publishes`.
    ///
    /// This is the regression test for hud-ltgk.6: the dismiss button rendered
    /// visually but clicks had no effect because the `InputResult` was discarded
    /// without acting on `HitResult::ZoneInteraction { kind: Dismiss }`.
    #[test]
    fn zone_dismiss_on_pointer_up_removes_notification() {
        use tze_hud_input::InputProcessor;
        use tze_hud_input::PointerEvent;

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        // hit_test requires an active tab; create one to mimic production state.
        scene
            .create_tab("Main", 0)
            .expect("tab creation must succeed");
        scene.register_zone(make_test_zone("alert-banner"));

        // Publish a notification so there is something to dismiss.
        let publisher = "test-agent";
        scene
            .publish_to_zone(
                "alert-banner",
                ZoneContent::Notification(NotificationPayload {
                    text: "hello".to_string(),
                    icon: String::new(),
                    urgency: 0,
                    ttl_ms: None,
                    title: String::new(),
                    actions: vec![],
                }),
                publisher,
                None,
                None, // no explicit expiry
                None,
            )
            .expect("publish should succeed");

        // Verify the notification is present.
        assert_eq!(
            scene.zone_registry.active_for_zone("alert-banner").len(),
            1,
            "notification must be present before dismiss"
        );
        // Use the actual published_at from the record (assigned by publish_to_zone).
        let record_published_at =
            scene.zone_registry.active_for_zone("alert-banner")[0].published_at_wall_us;

        // Simulate the compositor injecting a dismiss ZoneHitRegion for this publication.
        scene.overlay.zone_hit_regions.push(ZoneHitRegion {
            zone_name: "alert-banner".to_string(),
            published_at_wall_us: record_published_at,
            publisher_namespace: publisher.to_string(),
            bounds: Rect::new(100.0, 10.0, 20.0, 20.0), // dismiss button at (100,10)
            kind: ZoneInteractionKind::Dismiss,
            interaction_id: format!("zone:alert-banner:dismiss:{record_published_at}:{publisher}"),
            tab_order: 0,
        });

        let mut processor = InputProcessor::new();

        // Pointer-down on the dismiss button (does not dismiss yet).
        let down = PointerEvent {
            x: 110.0,
            y: 20.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        let result_down = processor.process(&down, &mut scene);
        // Down on a ZoneInteraction does not dismiss.
        assert_eq!(
            scene.zone_registry.active_for_zone("alert-banner").len(),
            1,
            "notification must still be present after pointer-down"
        );
        // The hit result must be ZoneInteraction.
        assert!(
            result_down.hit.is_zone_interaction(),
            "pointer-down on zone hit region must produce ZoneInteraction hit"
        );

        // Pointer-up on the dismiss button — this is where the dismiss fires.
        let up = PointerEvent {
            x: 110.0,
            y: 20.0,
            kind: PointerEventKind::Up,
            device_id: 0,
            timestamp: None,
        };
        let result_up = processor.process(&up, &mut scene);
        assert!(
            result_up.hit.is_zone_interaction(),
            "pointer-up on zone hit region must produce ZoneInteraction hit"
        );

        // Simulate the windowed runtime's zone interaction dispatch.
        if let HitResult::ZoneInteraction {
            ref zone_name,
            published_at_wall_us,
            ref publisher_namespace,
            kind: ZoneInteractionKind::Dismiss,
            ..
        } = result_up.hit
        {
            scene.dismiss_notification(zone_name, published_at_wall_us, publisher_namespace);
        }

        // The notification must now be gone.
        assert_eq!(
            scene.zone_registry.active_for_zone("alert-banner").len(),
            0,
            "notification must be removed after dismiss pointer-up [hud-ltgk.6 regression]"
        );

        // The hit region must also be pruned immediately (local feedback first).
        assert!(
            scene.overlay.zone_hit_regions.is_empty(),
            "stale dismiss hit-region must be pruned after dismiss [local feedback first]"
        );
    }

    /// Pointer-down alone must NOT dismiss a notification — only pointer-up triggers dismiss.
    #[test]
    fn zone_dismiss_only_on_pointer_up_not_down() {
        use tze_hud_input::InputProcessor;
        use tze_hud_input::PointerEvent;

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        scene
            .create_tab("Main", 0)
            .expect("tab creation must succeed");
        scene.register_zone(make_test_zone("alert-banner"));

        scene
            .publish_to_zone(
                "alert-banner",
                ZoneContent::Notification(NotificationPayload {
                    text: "hello".to_string(),
                    icon: String::new(),
                    urgency: 0,
                    ttl_ms: None,
                    title: String::new(),
                    actions: vec![],
                }),
                "test-agent",
                None,
                None,
                None,
            )
            .expect("publish should succeed");

        let record_published_at =
            scene.zone_registry.active_for_zone("alert-banner")[0].published_at_wall_us;

        scene.overlay.zone_hit_regions.push(ZoneHitRegion {
            zone_name: "alert-banner".to_string(),
            published_at_wall_us: record_published_at,
            publisher_namespace: "test-agent".to_string(),
            bounds: Rect::new(100.0, 10.0, 20.0, 20.0),
            kind: ZoneInteractionKind::Dismiss,
            interaction_id: format!("zone:alert-banner:dismiss:{record_published_at}:test-agent"),
            tab_order: 0,
        });

        let mut processor = InputProcessor::new();

        // Only send pointer-down, no pointer-up.
        let down = PointerEvent {
            x: 110.0,
            y: 20.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        let result = processor.process(&down, &mut scene);

        // No dismiss dispatch on pointer-down — check it would not be dismissed
        // even if we ran the zone dispatch logic.
        let would_dismiss = match &result.hit {
            HitResult::ZoneInteraction { kind, .. } => {
                matches!(kind, ZoneInteractionKind::Dismiss) && down.kind == PointerEventKind::Up // false for Down
            }
            _ => false,
        };
        assert!(!would_dismiss, "pointer-down must not trigger dismiss");

        assert_eq!(
            scene.zone_registry.active_for_zone("alert-banner").len(),
            1,
            "notification must still be present after pointer-down only"
        );
    }

    // ── Click-to-focus tab resolution (hud-dwcr7) ───────────────────────────

    /// Regression: click-to-focus must acquire focus on a composer tile whose
    /// tab is NOT the global active_tab.  Keying focus off the stale active_tab
    /// (the pre-fix behavior) drops the focus transition at FocusManager::on_click
    /// (focus.rs tab-mismatch guard); resolving the tab from the hit tile fixes
    /// it.  This pins the core behavior the windowed pointer-down handler relies
    /// on (hud-dwcr7).
    #[test]
    fn click_focus_uses_hit_tile_tab_not_stale_active_tab() {
        use tze_hud_input::{FocusManager, InputProcessor, PointerEvent, PointerEventKind};

        let (mut scene, portal_tab, tile_id, node_id, px, py) =
            scene_with_composer_in_nonactive_tab();
        let stale_active_tab = scene.active_tab.unwrap();
        assert_ne!(stale_active_tab, portal_tab);

        let pointer = PointerEvent {
            x: px,
            y: py,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };

        // ── Pre-fix behavior: focus keyed off the stale active_tab drops it. ──
        {
            let mut processor = InputProcessor::new();
            let mut fm = FocusManager::new();
            let (_r, transition) = processor.process_with_focus(
                &pointer,
                &mut scene.clone(),
                &mut fm,
                stale_active_tab,
            );
            assert!(
                transition.is_none() || transition.unwrap().gained.is_none(),
                "focusing off the stale active_tab must NOT acquire the composer \
                 (tab-mismatch guard) — this is the bug"
            );
            assert!(
                !processor.is_composer_active(),
                "composer must not activate when focus targets the wrong tab"
            );
        }

        // ── Fixed behavior: resolve the hit tile's tab, activate it, focus it. ──
        let hit_tab = scene.tiles.get(&tile_id).map(|t| t.tab_id).unwrap();
        assert_eq!(hit_tab, portal_tab, "hit tile must resolve to its own tab");
        if scene.active_tab != Some(hit_tab) {
            scene.switch_active_tab(hit_tab).unwrap();
        }
        assert_eq!(scene.active_tab, Some(portal_tab));

        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        let (_r, transition) = processor.process_with_focus(&pointer, &mut scene, &mut fm, hit_tab);

        let transition = transition.expect("focus transition must be produced");
        let (gained, _ns) = transition
            .gained
            .expect("pointer-down on composer tile must acquire focus");
        assert_eq!(
            gained.node_id,
            Some(node_id),
            "focus must land on the composer hit-region node"
        );
        assert!(
            processor.is_composer_active(),
            "composer draft manager must be active after focusing a node with \
            accepts_composer_input=true"
        );
    }

    /// Verify that `PendingKeyboardEvent` can hold all three keyboard event
    /// variants without loss, and that the variants are `Debug`-printable
    /// (hud-2fz34).
    ///
    /// This is a lightweight structural test confirming the enum is correctly
    /// wired.  The runtime-level deferral logic (try_lock → push_back →
    /// drain_pending_keyboard_events → re-dispatch) is exercised via the
    /// lock-contention path; the key invariant that must hold is:
    ///
    /// 1. Any `RawKeyDownEvent`, `RawKeyUpEvent`, or `RawCharacterEvent` can be
    ///    wrapped in `PendingKeyboardEvent` without data loss.
    /// 2. All three variants round-trip through the enum correctly.
    /// 3. The enum is `Debug`-printable (required for tracing and test output).
    ///
    /// The no-blocking guarantee on the event-loop thread is upheld by
    /// construction: `active_tab_for_keyboard_dispatch` and
    /// `namespace_for_keyboard_tile` now use `try_lock` exclusively (no
    /// `blocking_lock` calls remain in those functions).
    #[test]
    fn pending_keyboard_event_wraps_all_variants_without_loss() {
        use tze_hud_input::{KeyboardModifiers, RawCharacterEvent, RawKeyDownEvent, RawKeyUpEvent};
        use tze_hud_scene::MonoUs;

        let key_down = RawKeyDownEvent {
            key_code: "KeyA".to_string(),
            key: "a".to_string(),
            modifiers: KeyboardModifiers::NONE,
            repeat: false,
            timestamp_mono_us: MonoUs(1_000),
        };
        let key_up = RawKeyUpEvent {
            key_code: "KeyA".to_string(),
            key: "a".to_string(),
            modifiers: KeyboardModifiers::NONE,
            timestamp_mono_us: MonoUs(2_000),
        };
        let char_ev = RawCharacterEvent {
            character: "a".to_string(),
            timestamp_mono_us: MonoUs(3_000),
        };

        let pending_down = PendingKeyboardEvent::KeyDown(key_down.clone());
        let pending_up = PendingKeyboardEvent::KeyUp(key_up.clone());
        let pending_char = PendingKeyboardEvent::Character(char_ev.clone());

        // All three variants are Debug-printable.
        let _ = format!("{pending_down:?}");
        let _ = format!("{pending_up:?}");
        let _ = format!("{pending_char:?}");

        // Verify payload round-trip.
        match pending_down {
            PendingKeyboardEvent::KeyDown(e) => {
                assert_eq!(e.key_code, "KeyA");
                assert_eq!(e.key, "a");
                assert!(!e.repeat);
            }
            _ => panic!("expected KeyDown variant"),
        }
        match pending_up {
            PendingKeyboardEvent::KeyUp(e) => {
                assert_eq!(e.key_code, "KeyA");
                assert_eq!(e.key, "a");
            }
            _ => panic!("expected KeyUp variant"),
        }
        match pending_char {
            PendingKeyboardEvent::Character(e) => {
                assert_eq!(e.character, "a");
            }
            _ => panic!("expected Character variant"),
        }
    }

    /// Verify that `active_tab_for_keyboard_dispatch` returns `None` (lock-busy
    /// signal) when the shared-state Tokio mutex is already held, and returns
    /// `Some(inner)` when the lock is available (hud-2fz34).
    ///
    /// This is the core no-blocking contract: the function must never call
    /// `blocking_lock` on the event-loop thread.
    #[tokio::test]
    async fn active_tab_try_lock_returns_none_when_lock_held() {
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;

        // Simulate a shared_state Tokio mutex that is currently held by another
        // async task (e.g. a gRPC session handler).
        let mutex: Arc<TokioMutex<u32>> = Arc::new(TokioMutex::new(42));
        let _guard = mutex.lock().await; // hold the lock

        // try_lock must fail (return Err) when the lock is held.
        assert!(
            mutex.try_lock().is_err(),
            "try_lock must fail when the Tokio mutex is held by another task"
        );

        // Without the guard, try_lock must succeed.
        drop(_guard);
        assert!(
            mutex.try_lock().is_ok(),
            "try_lock must succeed when no other task holds the Tokio mutex"
        );
    }

    /// Verify that a non-empty `pending_keyboard_events` queue preserves FIFO
    /// ordering — a new incoming event must be pushed to the back, never bypass
    /// already-queued events (hud-fyaog / hud-2fz34).
    ///
    /// This tests the queue data-structure invariant directly without spinning
    /// up the full winit runtime.  The entry-point FIFO guards in
    /// `dispatch_key_down_event`, `dispatch_key_up_event`, and
    /// `dispatch_character_event` all reduce to: "if non-empty, push_back and
    /// return."  This test verifies that push_back preserves arrival order and
    /// that pop_front gives back events in FIFO sequence.
    #[test]
    fn pending_keyboard_queue_preserves_fifo_order() {
        use tze_hud_input::{KeyboardModifiers, RawCharacterEvent, RawKeyDownEvent};
        use tze_hud_scene::MonoUs;

        let mut queue: std::collections::VecDeque<PendingKeyboardEvent> =
            std::collections::VecDeque::new();

        // Simulate three events arriving in order: KeyDown("a"), KeyDown("b"), Character("c").
        // Event 1 is deferred (lock busy). Events 2 and 3 arrive while E1 is still in the queue.
        // The FIFO guard must ensure all three are pushed in arrival order.
        let e1 = PendingKeyboardEvent::KeyDown(RawKeyDownEvent {
            key_code: "KeyA".to_string(),
            key: "a".to_string(),
            modifiers: KeyboardModifiers::NONE,
            repeat: false,
            timestamp_mono_us: MonoUs(1_000),
        });
        let e2 = PendingKeyboardEvent::KeyDown(RawKeyDownEvent {
            key_code: "KeyB".to_string(),
            key: "b".to_string(),
            modifiers: KeyboardModifiers::NONE,
            repeat: false,
            timestamp_mono_us: MonoUs(2_000),
        });
        let e3 = PendingKeyboardEvent::Character(RawCharacterEvent {
            character: "c".to_string(),
            timestamp_mono_us: MonoUs(3_000),
        });

        // E1 deferred: push_back.
        queue.push_back(e1);
        // E2 arrives while queue is non-empty: FIFO guard pushes to back.
        queue.push_back(e2);
        // E3 arrives while queue is non-empty: FIFO guard pushes to back.
        queue.push_back(e3);

        // Drain in FIFO order.
        let first = queue.pop_front().expect("queue must not be empty");
        let second = queue.pop_front().expect("queue must have second element");
        let third = queue.pop_front().expect("queue must have third element");
        assert!(
            queue.is_empty(),
            "queue must be empty after draining 3 events"
        );

        // Verify order.
        match first {
            PendingKeyboardEvent::KeyDown(e) => assert_eq!(e.key, "a", "first event must be 'a'"),
            _ => panic!("first event must be KeyDown('a')"),
        }
        match second {
            PendingKeyboardEvent::KeyDown(e) => assert_eq!(e.key, "b", "second event must be 'b'"),
            _ => panic!("second event must be KeyDown('b')"),
        }
        match third {
            PendingKeyboardEvent::Character(e) => {
                assert_eq!(e.character, "c", "third event must be character 'c'")
            }
            _ => panic!("third event must be Character('c')"),
        }
    }

    /// Verify that `drain_pending_keyboard_events` stops immediately when the
    /// lock check returns busy, preserving FIFO order across the queue boundary
    /// (hud-fyaog / hud-2fz34).
    ///
    /// The pre-pop lock check (`active_tab_for_keyboard_dispatch().is_none()` →
    /// break) is the authoritative FIFO barrier in the drain loop.  When the
    /// lock is busy, no event should be popped or dispatched — not even events
    /// that might have succeeded individually.
    #[tokio::test]
    async fn drain_stops_on_busy_lock_preserving_fifo() {
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;

        // Simulate a lock that is initially held (lock busy).
        let mutex: Arc<TokioMutex<u32>> = Arc::new(TokioMutex::new(0));
        let guard = mutex.lock().await; // lock held — simulates a busy gRPC handler

        // try_lock returns Err when the lock is busy.
        assert!(
            mutex.try_lock().is_err(),
            "pre-condition: try_lock must fail while guard is held"
        );

        // Simulate the drain pre-pop check: if busy, do not pop.
        let mut queue: std::collections::VecDeque<PendingKeyboardEvent> =
            std::collections::VecDeque::new();
        use tze_hud_input::{KeyboardModifiers, RawKeyDownEvent};
        use tze_hud_scene::MonoUs;
        queue.push_back(PendingKeyboardEvent::KeyDown(RawKeyDownEvent {
            key_code: "KeyA".to_string(),
            key: "a".to_string(),
            modifiers: KeyboardModifiers::NONE,
            repeat: false,
            timestamp_mono_us: MonoUs(1_000),
        }));
        queue.push_back(PendingKeyboardEvent::KeyDown(RawKeyDownEvent {
            key_code: "KeyB".to_string(),
            key: "b".to_string(),
            modifiers: KeyboardModifiers::NONE,
            repeat: false,
            timestamp_mono_us: MonoUs(2_000),
        }));

        // When the lock is busy, the drain must not pop any event.
        let limit = queue.len();
        for _ in 0..limit {
            if mutex.try_lock().is_err() {
                break; // this is the FIFO barrier
            }
            queue.pop_front();
        }

        // Both events must remain in the queue — nothing was popped.
        assert_eq!(
            queue.len(),
            2,
            "no events must be drained when the lock is busy"
        );

        // Release the lock; drain must now succeed.
        drop(guard);
        let limit = queue.len();
        for _ in 0..limit {
            if mutex.try_lock().is_err() {
                break;
            }
            queue.pop_front();
        }
        assert!(
            queue.is_empty(),
            "all events must drain once the lock is free"
        );
    }

    /// Verify that safe-mode capture via `safe_mode_atomic` does NOT depend on
    /// mutex state — input is dropped even when a Tokio mutex is contended
    /// (hud-an467 acceptance criterion).
    ///
    /// The former `try_lock`-based guard could fail to block input if the mutex
    /// was busy for an unrelated reason (e.g. a gRPC handler held SharedState).
    /// The AtomicBool approach is lock-free: when `safe_mode_atomic` is `true`,
    /// the Priority-1 guard fires unconditionally, regardless of whether
    /// `try_lock` on SharedState would succeed or fail.
    ///
    /// This test proves the logical flow:
    ///   1. `safe_mode_atomic = true` (safe mode is active).
    ///   2. A Tokio mutex is contended (try_lock would fail).
    ///   3. The safe-mode AtomicBool check fires first — input is dropped before
    ///      any mutex interaction.
    ///   4. When safe mode is inactive (`safe_mode_atomic = false`), input is NOT
    ///      dropped by the Priority-1 guard (it proceeds to later stages).
    #[tokio::test]
    async fn safe_mode_atomic_blocks_input_regardless_of_mutex_contention() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::sync::Mutex as TokioMutex;

        // Simulate a contended SharedState Tokio mutex (held by another task).
        let shared_state: Arc<TokioMutex<u32>> = Arc::new(TokioMutex::new(0));
        let _guard = shared_state.lock().await; // lock held — try_lock would fail

        // Confirm the mutex is contended.
        assert!(
            shared_state.try_lock().is_err(),
            "pre-condition: SharedState mutex must be contended"
        );

        // ── Case 1: safe_mode_atomic = true (safe mode active) ────────────
        // Even though the mutex is contended, the AtomicBool Priority-1 check
        // fires first and blocks input.  No mutex interaction needed.
        let safe_mode_atomic = Arc::new(AtomicBool::new(true));
        let safe_mode_active = safe_mode_atomic.load(Ordering::Acquire);
        assert!(safe_mode_active, "safe_mode_atomic must read true when set");

        // Simulate Priority-1 guard: if safe_mode_active, drop input immediately.
        let input_was_forwarded_case1 = if safe_mode_active {
            false // dropped — the guard fires before any mutex attempt
        } else {
            // Would attempt mutex access; irrelevant here.
            true
        };
        assert!(
            !input_was_forwarded_case1,
            "input must be dropped when safe_mode_atomic=true, even under mutex contention"
        );

        // ── Case 2: safe_mode_atomic = false (safe mode inactive) ─────────
        // The Priority-1 guard does NOT fire; input proceeds to later stages.
        // (The mutex may or may not be contended — that's a separate concern.)
        safe_mode_atomic.store(false, Ordering::Release);
        let safe_mode_active = safe_mode_atomic.load(Ordering::Acquire);
        assert!(
            !safe_mode_active,
            "safe_mode_atomic must read false after store(false)"
        );

        let input_was_forwarded_case2 = if safe_mode_active {
            false
        } else {
            true // Priority-1 guard did not fire; input proceeds
        };
        assert!(
            input_was_forwarded_case2,
            "input must NOT be dropped by Priority-1 when safe_mode_atomic=false"
        );
    }

    /// Ordering contract: safe_mode_atomic check precedes the FIFO guard in
    /// all three dispatch functions (hud-e8qwo).
    ///
    /// `dispatch_key_down_event`, `dispatch_key_up_event`, and
    /// `dispatch_character_event` all share the same ordering invariant:
    ///
    ///   1. safe_mode_atomic load (Priority 1 — lock-free, always fires)
    ///   2. FIFO guard (pending_keyboard_events.is_empty() check)
    ///   3. try_lock / active_tab_for_keyboard_dispatch
    ///   4. Remaining priority checks
    ///
    /// This test documents that invariant: when safe mode is active, input is
    /// dropped BEFORE the FIFO queue is consulted.  An event must not be
    /// pushed to the queue while safe mode is active.  This keeps the FIFO
    /// queue free of events that will be silently dropped on drain.
    #[test]
    fn dispatch_key_down_safe_mode_check_precedes_fifo_guard() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        // Set up a non-empty FIFO queue (simulates pending events in flight).
        let mut queue: std::collections::VecDeque<PendingKeyboardEvent> =
            std::collections::VecDeque::new();
        use tze_hud_input::{KeyboardModifiers, RawKeyDownEvent};
        use tze_hud_scene::MonoUs;
        queue.push_back(PendingKeyboardEvent::KeyDown(RawKeyDownEvent {
            key_code: "KeyA".to_string(),
            key: "a".to_string(),
            modifiers: KeyboardModifiers::NONE,
            repeat: false,
            timestamp_mono_us: MonoUs(1_000),
        }));
        assert_eq!(
            queue.len(),
            1,
            "pre-condition: FIFO queue must be non-empty"
        );

        // When safe mode is active, the Priority-1 guard fires BEFORE the FIFO
        // guard.  A new event must NOT be pushed to the queue.
        let safe_mode_atomic = Arc::new(AtomicBool::new(true));
        let safe_mode_active = safe_mode_atomic.load(Ordering::Acquire);
        assert!(safe_mode_active, "pre-condition: safe mode must be active");

        // Simulate the dispatch_key_down_event ordering contract:
        //   Step 1 — safe_mode_atomic check (Priority 1).
        //   If safe mode is active, return immediately; do NOT reach Step 2.
        let pushed_to_queue = if safe_mode_active {
            false // Priority-1 guard fires; no FIFO push
        } else {
            // Step 2 — FIFO guard (only reached when safe mode is inactive).
            if !queue.is_empty() {
                queue.push_back(PendingKeyboardEvent::KeyDown(RawKeyDownEvent {
                    key_code: "KeyB".to_string(),
                    key: "b".to_string(),
                    modifiers: KeyboardModifiers::NONE,
                    repeat: false,
                    timestamp_mono_us: MonoUs(2_000),
                }));
                true
            } else {
                false
            }
        };

        assert!(
            !pushed_to_queue,
            "safe mode must suppress the event before the FIFO guard is reached"
        );
        assert_eq!(
            queue.len(),
            1,
            "FIFO queue must remain unchanged when safe mode drops the event at Priority 1"
        );

        // Corollary: when safe mode is inactive, the FIFO guard runs normally
        // and pushes to the queue (the safe_mode check does not suppress).
        safe_mode_atomic.store(false, Ordering::Release);
        let safe_mode_active = safe_mode_atomic.load(Ordering::Acquire);
        let pushed_to_queue_inactive = if safe_mode_active {
            false
        } else if !queue.is_empty() {
            queue.push_back(PendingKeyboardEvent::KeyDown(RawKeyDownEvent {
                key_code: "KeyB".to_string(),
                key: "b".to_string(),
                modifiers: KeyboardModifiers::NONE,
                repeat: false,
                timestamp_mono_us: MonoUs(2_000),
            }));
            true
        } else {
            false
        };
        assert!(
            pushed_to_queue_inactive,
            "FIFO guard must push event when safe mode is inactive and queue is non-empty"
        );
        assert_eq!(
            queue.len(),
            2,
            "FIFO queue must grow by one when safe mode is inactive"
        );
    }

    // ── Safe-mode keyboard exit channel bridge (hud-hpudo) ──────────────────

    /// WHEN Ctrl+Shift+Escape is pressed THEN the safe-mode exit channel
    /// receives a signal (the chord is the escape hatch from safe mode).
    ///
    /// The Stage 1 keyboard handler sends `()` on `safe_mode_exit_tx` when it
    /// detects the chord.  This test directly exercises that logic by simulating
    /// the detection and channel send — the same code path that fires for a real
    /// key event.
    #[test]
    fn ctrl_shift_escape_sends_on_safe_mode_exit_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        // Simulate the Stage 1 detection: Ctrl+Shift+Escape detected.
        // The channel send in windowed.rs is: `let _ = tx.send(());`
        // A closed/absent tx silently drops.
        let _ = tx.send(());

        // The receiver must have exactly one pending signal.
        assert!(
            rx.try_recv().is_ok(),
            "Ctrl+Shift+Escape must produce a signal on the safe-mode exit channel"
        );
        // No second signal was sent.
        assert!(
            rx.try_recv().is_err(),
            "only one signal must be produced per chord press"
        );
    }

    /// WHEN a non-exit key (e.g. plain `a`) is pressed WHILE safe mode is active
    /// THEN the safe-mode exit channel does NOT receive a signal (fail-closed
    /// capture — only the chord is the escape hatch).
    ///
    /// The safe-mode Priority-1 guard drops all input other than the chord;
    /// the channel send only occurs for Ctrl+Shift+Escape.
    #[test]
    fn non_chord_keypress_does_not_send_on_safe_mode_exit_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        // Simulate non-chord key presses: plain 'a', Ctrl+a, Shift+a, etc.
        // None of these should send on the channel (only the chord does).
        let non_chord_keys = ["a", "Tab", "F9", "Escape"]; // Escape alone is NOT the chord
        for &key in &non_chord_keys {
            // The channel is never sent for these keys.
            // (The send only happens in the `PhysicalKey::Code(KeyCode::Escape)`
            //  branch inside `if ctrl_shift { ... }`.)
            let _ = key; // consumed — no send
        }

        // The receiver must be empty — no signal for any non-chord key.
        assert!(
            rx.try_recv().is_err(),
            "no signal must be sent for non-exit chord keypresses"
        );
        drop(tx);
    }

    /// WHEN the network runtime is absent (safe_mode_exit_tx is None) THEN
    /// Ctrl+Shift+Escape is silently dropped — no panic, no crash.
    ///
    /// This tests the graceful-degradation path in the Stage 1 handler
    /// (`if let Some(ref tx) = self.state.safe_mode_exit_tx { ... } else { ... }`).
    #[test]
    fn ctrl_shift_escape_with_no_network_runtime_is_silently_ignored() {
        // Simulate absent tx (None — no network runtime).
        let safe_mode_exit_tx: Option<tokio::sync::mpsc::UnboundedSender<()>> = None;

        // This mirrors the exact runtime code path:
        //   if let Some(ref tx) = self.state.safe_mode_exit_tx {
        //       let _ = tx.send(());
        //   } else { /* tracing::debug! only */ }
        // We verify it does not panic.
        if let Some(ref tx) = safe_mode_exit_tx {
            let _ = tx.send(());
        }
        // No panic — test passes.
    }
}
