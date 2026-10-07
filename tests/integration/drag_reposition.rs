//! Integration tests for the long-press drag-to-reposition interaction.
//!
//! Tests the full lifecycle:
//!   PointerDown on drag handle → Accumulating → Activated → Moved → Released → Persisted
//!
//! All tests are headless Layer 0: no display server or GPU required.
//!
//! Source: openspec/changes/persistent-movable-elements/specs/drag-to-reposition/spec.md

use tze_hud_input::{
    DragEventOutcome, DragHandleElementKind, DragPhase, InputProcessor, PointerEvent,
    PointerEventKind,
};
use tze_hud_scene::{
    ElementStore, ElementStoreEntry, ElementType, GeometryPolicy, Rect, SceneGraph, SceneId,
};

// ── Display geometry ──────────────────────────────────────────────────────────

const DISPLAY_W: f32 = 1920.0;
const DISPLAY_H: f32 = 1080.0;

// Element positioned at 10%,20% of display, 30% wide, 20% tall
const ELEMENT_X: f32 = 192.0;
const ELEMENT_Y: f32 = 216.0;
const ELEMENT_W: f32 = 576.0;
const ELEMENT_H: f32 = 216.0;

// Drag handle sits above the element centre
const HANDLE_X: f32 = ELEMENT_X + ELEMENT_W / 2.0 - 12.0;
const HANDLE_Y: f32 = ELEMENT_Y - 4.0;
const HANDLE_W: f32 = 24.0;
const HANDLE_H: f32 = 8.0;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn element_id() -> SceneId {
    // Deterministic: create once and reuse within a test
    SceneId::new()
}

fn build_processor() -> InputProcessor {
    InputProcessor::new()
}

fn pointer_down(x: f32, y: f32) -> PointerEvent {
    PointerEvent {
        x,
        y,
        kind: PointerEventKind::Down,
        device_id: 0,
        timestamp: None,
    }
}

fn pointer_move(x: f32, y: f32) -> PointerEvent {
    PointerEvent {
        x,
        y,
        kind: PointerEventKind::Move,
        device_id: 0,
        timestamp: None,
    }
}

fn pointer_up(x: f32, y: f32) -> PointerEvent {
    PointerEvent {
        x,
        y,
        kind: PointerEventKind::Up,
        device_id: 0,
        timestamp: None,
    }
}

fn element_bounds() -> Rect {
    Rect::new(ELEMENT_X, ELEMENT_Y, ELEMENT_W, ELEMENT_H)
}

fn handle_center() -> (f32, f32) {
    (HANDLE_X + HANDLE_W / 2.0, HANDLE_Y + HANDLE_H / 2.0)
}

// ── Long-press activation ─────────────────────────────────────────────────────

#[test]
fn long_press_starts_accumulating_on_pointer_down() {
    let mut ip = build_processor();
    let eid = element_id();
    let (cx, cy) = handle_center();

    let outcome = ip.process_drag_handle_pointer(
        &pointer_down(cx, cy),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    assert_eq!(
        outcome,
        DragEventOutcome::Accumulating { progress: 0.0 },
        "PointerDown must start Accumulating at progress=0"
    );
    assert_eq!(
        ip.drag_states.get(&0).map(|s| s.phase),
        Some(DragPhase::Accumulating),
        "device state must be in Accumulating phase"
    );
}

#[test]
fn long_press_cancelled_if_pointer_moves_beyond_tolerance() {
    let mut ip = build_processor();
    let eid = element_id();
    let (cx, cy) = handle_center();

    ip.process_drag_handle_pointer(
        &pointer_down(cx, cy),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    // Move 15dp beyond tolerance (threshold is 10dp)
    let outcome = ip.process_drag_handle_pointer(
        &pointer_move(cx + 15.0, cy),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    assert_eq!(outcome, DragEventOutcome::Cancelled);
    assert!(
        !ip.drag_states.contains_key(&0),
        "drag state must be cleared after cancellation"
    );
}

#[test]
fn long_press_not_cancelled_within_movement_tolerance() {
    let mut ip = build_processor();
    let eid = element_id();
    let (cx, cy) = handle_center();

    ip.process_drag_handle_pointer(
        &pointer_down(cx, cy),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    // Move 5dp — below 10dp threshold
    let outcome = ip.process_drag_handle_pointer(
        &pointer_move(cx + 5.0, cy),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    assert!(
        matches!(outcome, DragEventOutcome::Accumulating { .. }),
        "5dp move must still be Accumulating, got {outcome:?}"
    );
}

#[test]
fn long_press_activates_after_threshold_met_on_move() {
    let mut ip = build_processor();
    let eid = element_id();
    let (cx, cy) = handle_center();

    // Use a 1ms threshold so we can reliably trip it in tests
    ip.drag_states.insert(
        0,
        tze_hud_input::DeviceDragState::new(
            "drag-handle:aabb".to_string(),
            eid,
            DragHandleElementKind::Tile,
            cx,
            cy,
            1,     // 1ms threshold
            false, // immediate (hud-cpjqe): legacy grip long-press path
        ),
    );

    // Wait for threshold to pass
    std::thread::sleep(std::time::Duration::from_millis(5));

    let outcome = ip.process_drag_handle_pointer(
        &pointer_move(cx, cy), // no movement — threshold should fire
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    assert_eq!(
        outcome,
        DragEventOutcome::Activated {
            element_id: eid,
            element_kind: DragHandleElementKind::Tile,
        },
        "after threshold, next move must produce Activated"
    );
    assert_eq!(
        ip.drag_states.get(&0).map(|s| s.phase),
        Some(DragPhase::Activated),
        "phase must be Activated"
    );
}

// ── Multi-touch independence ──────────────────────────────────────────────────

#[test]
fn two_devices_have_independent_drag_state() {
    let mut ip = build_processor();
    let eid1 = SceneId::new();
    let eid2 = SceneId::new();
    let (cx, cy) = handle_center();

    // Device 0 starts accumulating
    ip.process_drag_handle_pointer(
        &PointerEvent {
            x: cx,
            y: cy,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        },
        "drag-handle:aabb",
        eid1,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    // Device 1 starts accumulating on a different element
    let outcome = ip.process_drag_handle_pointer(
        &PointerEvent {
            x: cx + 100.0,
            y: cy + 100.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        },
        "drag-handle:ccdd",
        eid2,
        DragHandleElementKind::Zone,
        Rect::new(ELEMENT_X + 100.0, ELEMENT_Y + 100.0, 200.0, 100.0),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    assert_eq!(outcome, DragEventOutcome::Accumulating { progress: 0.0 });
    assert!(ip.drag_states.contains_key(&0), "device 0 must have state");
    assert!(ip.drag_states.contains_key(&1), "device 1 must have state");
    assert_ne!(
        ip.drag_states[&0].element_id, ip.drag_states[&1].element_id,
        "each device tracks its own element"
    );
}

// ── Snap grid quantisation ────────────────────────────────────────────────────

#[test]
fn snap_grid_quantises_move_position() {
    use tze_hud_input::drag::snap_to_grid;

    let snap_pct = 0.02_f32;
    let dim = DISPLAY_W;
    let cell = dim * snap_pct; // 38.4px

    // 1.4 cells rounds to 1 cell
    let p = snap_to_grid(cell * 1.4, dim, snap_pct);
    assert!(
        (p - cell).abs() < 0.5,
        "1.4 cells must snap to 1 cell (expected ~{cell}, got {p})"
    );

    // 1.6 cells rounds to 2 cells
    let p2 = snap_to_grid(cell * 1.6, dim, snap_pct);
    assert!(
        (p2 - 2.0 * cell).abs() < 0.5,
        "1.6 cells must snap to 2 cells (expected ~{}, got {p2})",
        2.0 * cell
    );
}

// ── Boundary clamping ─────────────────────────────────────────────────────────

#[test]
fn drag_clamps_to_left_edge() {
    use tze_hud_input::drag::clamp_to_display;

    let (cx, _cy) = clamp_to_display(-50.0, 100.0, ELEMENT_W, ELEMENT_H, DISPLAY_W, DISPLAY_H);
    assert_eq!(cx, 0.0, "left edge must clamp to 0");
}

#[test]
fn drag_clamps_to_right_edge() {
    use tze_hud_input::drag::clamp_to_display;

    let (cx, _) = clamp_to_display(1900.0, 100.0, ELEMENT_W, ELEMENT_H, DISPLAY_W, DISPLAY_H);
    assert_eq!(
        cx,
        DISPLAY_W - ELEMENT_W,
        "right edge: x must not exceed display - element_w"
    );
}

#[test]
fn drag_clamps_to_top_edge() {
    use tze_hud_input::drag::clamp_to_display;

    let (_, cy) = clamp_to_display(100.0, -10.0, ELEMENT_W, ELEMENT_H, DISPLAY_W, DISPLAY_H);
    assert_eq!(cy, 0.0, "top edge must clamp to 0");
}

#[test]
fn drag_clamps_to_bottom_edge() {
    use tze_hud_input::drag::clamp_to_display;

    let (_, cy) = clamp_to_display(100.0, 1050.0, ELEMENT_W, ELEMENT_H, DISPLAY_W, DISPLAY_H);
    assert_eq!(
        cy,
        DISPLAY_H - ELEMENT_H,
        "bottom edge: y must not exceed display - element_h"
    );
}

// ── Full drag flow: press → hold → drag → release → persisted ────────────────

#[test]
fn full_drag_flow_persists_geometry_on_release() {
    let mut ip = build_processor();
    let eid = element_id();
    let (cx, cy) = handle_center();

    // Seed drag state with 1ms threshold so we can reliably activate
    ip.drag_states.insert(
        0,
        tze_hud_input::DeviceDragState::new(
            "drag-handle:aabb".to_string(),
            eid,
            DragHandleElementKind::Tile,
            cx,
            cy,
            1,
            false, // immediate (hud-cpjqe): legacy grip long-press path
        ),
    );
    std::thread::sleep(std::time::Duration::from_millis(5));

    // Simulate the activation via move
    let act = ip.process_drag_handle_pointer(
        &pointer_move(cx, cy),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );
    assert_eq!(
        act,
        DragEventOutcome::Activated {
            element_id: eid,
            element_kind: DragHandleElementKind::Tile
        }
    );

    // Move element to new position (with snap + clamp)
    let move_x = 400.0_f32;
    let move_y = 300.0_f32;
    let grab_off_x = ip.drag_states[&0].grab_offset_x;
    let grab_off_y = ip.drag_states[&0].grab_offset_y;
    let pointer_x = move_x + grab_off_x;
    let pointer_y = move_y + grab_off_y;

    let moved = ip.process_drag_handle_pointer(
        &pointer_move(pointer_x, pointer_y),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );
    assert!(
        matches!(moved, DragEventOutcome::Moved { .. }),
        "after activation, move must produce Moved, got {moved:?}"
    );

    // Release
    let released = ip.process_drag_handle_pointer(
        &pointer_up(pointer_x, pointer_y),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    let (final_x, final_y) = match released {
        DragEventOutcome::Released {
            element_id,
            element_kind,
            final_x,
            final_y,
        } => {
            assert_eq!(element_id, eid);
            assert_eq!(element_kind, DragHandleElementKind::Tile);
            // Must be within display bounds
            assert!(final_x >= 0.0 && final_x + ELEMENT_W <= DISPLAY_W + 0.1);
            assert!(final_y >= 0.0 && final_y + ELEMENT_H <= DISPLAY_H + 0.1);
            (final_x, final_y)
        }
        other => panic!("expected Released, got {other:?}"),
    };

    // Drag state must be cleared
    assert!(
        !ip.drag_states.contains_key(&0),
        "drag state must be cleared after release"
    );

    // Persist to element store
    let mut store = ElementStore::default();
    let entry_id = SceneId::new();
    store.entries.insert(
        entry_id,
        ElementStoreEntry {
            element_type: ElementType::Tile,
            namespace: "test-tile".to_string(),
            created_at: 0,
            last_published_at: 0,
            z_order: 0,
            unseen_restarts: 0,
            geometry_override: None,
        },
    );

    InputProcessor::persist_drag_geometry(
        &mut store,
        ElementType::Tile,
        "test-tile",
        final_x,
        final_y,
        ELEMENT_W,
        ELEMENT_H,
        DISPLAY_W,
        DISPLAY_H,
    );

    let entry = store.entries.get(&entry_id).unwrap();
    let persisted = entry
        .geometry_override
        .expect("geometry_override must be set after persist");

    if let GeometryPolicy::Relative {
        x_pct,
        y_pct,
        width_pct,
        height_pct,
    } = persisted
    {
        // Relative coords must be in [0, 1] range
        assert!((0.0..=1.0).contains(&x_pct));
        assert!((0.0..=1.0).contains(&y_pct));
        assert!((0.0..=1.0).contains(&width_pct));
        assert!((0.0..=1.0).contains(&height_pct));
        // Width/height fractions must match original
        assert!(
            (width_pct - ELEMENT_W / DISPLAY_W).abs() < 1e-4,
            "width_pct mismatch"
        );
        assert!(
            (height_pct - ELEMENT_H / DISPLAY_H).abs() < 1e-4,
            "height_pct mismatch"
        );
    } else {
        panic!("expected Relative policy, got {persisted:?}");
    }
}

// ── ElementRepositionedEvent emission (hud-bs2q.6) ───────────────────────────

/// GIVEN a completed drag (geometry_override persisted)
/// WHEN emit_drag_repositioned_event is called
/// THEN the resulting broadcast carries the correct element_id, new_geometry,
///      and previous_geometry (all fields present and values match).
///
/// This is a headless Layer 0 test — no gRPC session required.
/// Delivery to subscribed agents is covered by session_server.rs tests.
#[test]
fn drag_completion_emits_element_repositioned_event_with_correct_fields() {
    use tze_hud_protocol::session_server::HudSessionImpl;

    let scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let service = HudSessionImpl::new(scene, "test-key");

    // Subscribe to the broadcast BEFORE emitting.
    let mut rx = service.element_repositioned_tx.subscribe();

    let element_id = SceneId::new();
    let new_policy = GeometryPolicy::Relative {
        x_pct: 0.3,
        y_pct: 0.2,
        width_pct: 0.25,
        height_pct: 0.15,
    };
    let old_policy = GeometryPolicy::Relative {
        x_pct: 0.1,
        y_pct: 0.1,
        width_pct: 0.25,
        height_pct: 0.15,
    };

    service.emit_drag_repositioned_event(element_id, &new_policy, Some(&old_policy));

    let event = rx.try_recv().expect("event must be broadcast immediately");

    // element_id must be 16 bytes (UUID) and non-zero.
    assert_eq!(event.element_id.len(), 16, "element_id must be 16 bytes");
    assert!(
        event.element_id.iter().any(|&b| b != 0),
        "element_id must be non-zero"
    );

    // new_geometry must reflect new_policy.
    let ng = event.new_geometry.expect("new_geometry must be set");
    match ng.policy {
        Some(tze_hud_protocol::proto::geometry_policy_proto::Policy::Relative(r)) => {
            assert!((r.x_pct - 0.3_f32).abs() < 1e-4, "new x_pct mismatch");
            assert!((r.y_pct - 0.2_f32).abs() < 1e-4, "new y_pct mismatch");
        }
        other => panic!("expected Relative new_geometry, got {other:?}"),
    }

    // previous_geometry must reflect old_policy.
    let pg = event
        .previous_geometry
        .expect("previous_geometry must be set");
    match pg.policy {
        Some(tze_hud_protocol::proto::geometry_policy_proto::Policy::Relative(r)) => {
            assert!((r.x_pct - 0.1_f32).abs() < 1e-4, "prev x_pct mismatch");
        }
        other => panic!("expected Relative previous_geometry, got {other:?}"),
    }
}

/// GIVEN a completed drag with no prior geometry override
/// WHEN emit_drag_repositioned_event is called with previous_geometry=None
/// THEN previous_geometry field is absent (None) in the broadcast event.
#[test]
fn drag_completion_event_has_absent_previous_geometry_when_no_prior_override() {
    use tze_hud_protocol::session_server::HudSessionImpl;

    let scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let service = HudSessionImpl::new(scene, "test-key");
    let mut rx = service.element_repositioned_tx.subscribe();

    let element_id = SceneId::new();
    let new_policy = GeometryPolicy::Relative {
        x_pct: 0.5,
        y_pct: 0.5,
        width_pct: 0.3,
        height_pct: 0.2,
    };

    service.emit_drag_repositioned_event(element_id, &new_policy, None);

    let event = rx.try_recv().expect("event must be broadcast");
    assert!(
        event.previous_geometry.is_none(),
        "previous_geometry must be absent when no prior override"
    );
}

// ── Scene-graph drag_active_elements bookkeeping ──────────────────────────────

#[test]
fn scene_drag_active_elements_set_cleared_correctly() {
    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let eid = element_id();

    assert!(!scene.is_drag_active(eid), "initially not active");
    let version = scene.version;
    let epoch = scene.geometry_epoch;
    scene.set_drag_active(eid);
    assert!(scene.is_drag_active(eid), "must be active after set");
    assert_eq!(scene.geometry_epoch, epoch + 1);
    scene.set_drag_active(eid);
    assert_eq!(
        scene.geometry_epoch,
        epoch + 1,
        "setting an existing marker is idempotent"
    );
    scene.clear_drag_active(eid);
    assert!(!scene.is_drag_active(eid), "must be inactive after clear");
    assert_eq!(scene.geometry_epoch, epoch + 2);
    scene.clear_drag_active(eid);
    assert_eq!(
        scene.geometry_epoch,
        epoch + 2,
        "clearing an absent marker is idempotent"
    );
    assert_eq!(
        scene.version, version,
        "feedback must not invalidate content caches"
    );
}

// ── Reset-to-default (hud-zc7f) ──────────────────────────────────────────────
//
// All reset tests use HudSessionImpl::reset_element_geometry (async), the
// authoritative reset path wired to the context menu action.
//
// Layer 0: headless, no GPU, no gRPC server required.

/// Build a minimal HudSessionImpl with a Tile in the scene at known agent bounds
/// and a geometry override injected into the element store.  Returns the service
/// and the tile_id.
///
/// Helper shared by multiple reset-to-default tests.
async fn setup_service_with_tile_override(
    override_policy: GeometryPolicy,
) -> (tze_hud_protocol::session_server::HudSessionImpl, SceneId) {
    use tze_hud_protocol::session_server::HudSessionImpl;

    let mut scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let tab_id = scene.create_tab("Main", 0).expect("create tab");
    let lease_id = scene.grant_lease("test-agent", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test-agent",
            lease_id,
            Rect::new(ELEMENT_X, ELEMENT_Y, ELEMENT_W, ELEMENT_H),
            1,
        )
        .expect("create tile");

    let service = HudSessionImpl::new(scene, "test-key");
    {
        let mut st = service.state.lock().await;
        st.element_store.entries.insert(
            tile_id,
            ElementStoreEntry {
                element_type: ElementType::Tile,
                namespace: "test-agent".to_string(),
                created_at: 1000,
                last_published_at: 2000,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: Some(override_policy),
            },
        );
    }
    (service, tile_id)
}

/// GIVEN element with a user geometry override
/// WHEN reset_element_geometry is called
/// THEN override is cleared (returns true) and element store reflects cleared state
#[tokio::test]
async fn reset_removes_override_and_returns_true() {
    let override_policy = GeometryPolicy::Relative {
        x_pct: 0.8,
        y_pct: 0.8,
        width_pct: 0.2,
        height_pct: 0.1,
    };
    let (service, tile_id) = setup_service_with_tile_override(override_policy).await;

    let result = service.reset_element_geometry(tile_id).await;
    assert!(result, "reset must return true when override was cleared");

    let st = service.state.lock().await;
    let entry = st.element_store.entries.get(&tile_id).unwrap();
    assert!(
        entry.geometry_override.is_none(),
        "geometry_override must be None after reset"
    );
}

/// GIVEN element without any geometry override
/// WHEN reset_element_geometry is called
/// THEN returns false (no-op) and no ElementRepositionedEvent is broadcast
#[tokio::test]
async fn reset_is_noop_when_no_override() {
    use tze_hud_protocol::session_server::HudSessionImpl;

    let scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let service = HudSessionImpl::new(scene, "test-key");
    let mut rx = service.element_repositioned_tx.subscribe();

    let unknown_id = SceneId::new(); // Not in the element store.

    let result = service.reset_element_geometry(unknown_id).await;
    assert!(!result, "reset must return false when no override exists");
    assert!(
        rx.try_recv().is_err(),
        "no ElementRepositionedEvent must be emitted on no-op reset"
    );
}

/// GIVEN element with override
/// WHEN reset_element_geometry is called
/// THEN ElementRepositionedEvent is broadcast with correct element_id,
///      previous_geometry == override, new_geometry set
#[tokio::test]
async fn reset_broadcasts_element_repositioned_event() {
    let override_policy = GeometryPolicy::Relative {
        x_pct: 0.5,
        y_pct: 0.5,
        width_pct: 0.3,
        height_pct: 0.2,
    };
    let (service, tile_id) = setup_service_with_tile_override(override_policy).await;
    let mut rx = service.element_repositioned_tx.subscribe();

    let result = service.reset_element_geometry(tile_id).await;
    assert!(result, "reset must return true");

    let event = rx
        .try_recv()
        .expect("ElementRepositionedEvent must be broadcast after reset");

    assert_eq!(
        event.element_id,
        tile_id.as_uuid().as_bytes().to_vec(),
        "element_id in event must match the reset tile (big-endian UUID bytes)"
    );

    let pg = event
        .previous_geometry
        .expect("previous_geometry must be present");
    match pg.policy {
        Some(tze_hud_protocol::proto::geometry_policy_proto::Policy::Relative(r)) => {
            assert!(
                (r.x_pct - 0.5_f32).abs() < 1e-4,
                "previous_geometry x_pct must match the override value"
            );
        }
        other => panic!("expected Relative previous_geometry, got {other:?}"),
    }

    assert!(
        event.new_geometry.is_some(),
        "new_geometry must be present in the event"
    );
}

/// GIVEN element with override and no active agent sessions (SUSPENDED scenario)
/// WHEN reset_element_geometry is called
/// THEN reset succeeds (returns true) even with no broadcast subscribers
#[tokio::test]
async fn reset_succeeds_with_no_active_sessions() {
    use tze_hud_protocol::session_server::HudSessionImpl;

    let scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let service = HudSessionImpl::new(scene, "test-key");
    // No subscribers to the broadcast channel.

    let tile_id = SceneId::new();
    {
        let mut st = service.state.lock().await;
        st.element_store.entries.insert(
            tile_id,
            ElementStoreEntry {
                element_type: ElementType::Tile,
                namespace: "test-agent".to_string(),
                created_at: 1000,
                last_published_at: 2000,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: Some(GeometryPolicy::Relative {
                    x_pct: 0.1,
                    y_pct: 0.1,
                    width_pct: 0.3,
                    height_pct: 0.2,
                }),
            },
        );
    }

    let result = service.reset_element_geometry(tile_id).await;
    assert!(
        result,
        "reset must succeed (return true) even with no subscribers"
    );

    let st = service.state.lock().await;
    assert!(
        st.element_store
            .entries
            .get(&tile_id)
            .unwrap()
            .geometry_override
            .is_none(),
        "override must be cleared regardless of subscriber count"
    );
}

/// GIVEN element that was drag-repositioned (override set after a drag)
/// WHEN reset_element_geometry is called
/// THEN new_geometry in the event reflects the original agent-requested bounds
#[tokio::test]
async fn reset_returns_element_to_agent_bounds_after_drag() {
    // Drag override: bottom-right corner.
    let drag_override = GeometryPolicy::Relative {
        x_pct: 0.9,
        y_pct: 0.9,
        width_pct: ELEMENT_W / DISPLAY_W,
        height_pct: ELEMENT_H / DISPLAY_H,
    };
    let (service, tile_id) = setup_service_with_tile_override(drag_override).await;
    let mut rx = service.element_repositioned_tx.subscribe();

    let result = service.reset_element_geometry(tile_id).await;
    assert!(result, "reset must succeed after drag");

    let event = rx.try_recv().expect("event must be broadcast");
    let ng = event.new_geometry.expect("new_geometry must be set");

    // The fallback should resolve to the agent's original tile bounds.
    match ng.policy {
        Some(tze_hud_protocol::proto::geometry_policy_proto::Policy::Relative(r)) => {
            let expected_x_pct = ELEMENT_X / DISPLAY_W;
            let expected_y_pct = ELEMENT_Y / DISPLAY_H;
            assert!(
                (r.x_pct - expected_x_pct).abs() < 1e-3,
                "new_geometry x_pct must match agent bounds after reset \
                 (got {:.4}, expected {:.4})",
                r.x_pct,
                expected_x_pct,
            );
            assert!(
                (r.y_pct - expected_y_pct).abs() < 1e-3,
                "new_geometry y_pct must match agent bounds after reset \
                 (got {:.4}, expected {:.4})",
                r.y_pct,
                expected_y_pct,
            );
        }
        other => panic!("expected Relative new_geometry, got {other:?}"),
    }
}
