//! Integration tests for the long-press drag-to-reposition interaction.
//!
//! Tests the full lifecycle:
//!   PointerDown on drag handle → Accumulating → Activated → Moved → Released → Persisted
//!
//! All tests are headless Layer 0: no display server or GPU required.
//!

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

/// Touch pointer (device_id != 0): a grip still needs a long press, while a
/// mouse (device 0) drags a grip on its first move.
fn touch(mut ev: PointerEvent) -> PointerEvent {
    ev.device_id = 1;
    ev
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
        false, // is_header_band : legacy grip path, long-press
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
        &touch(pointer_down(cx, cy)),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band : legacy grip path, long-press
    );

    // Move 15dp beyond tolerance (threshold is 10dp)
    let outcome = ip.process_drag_handle_pointer(
        &touch(pointer_move(cx + 15.0, cy)),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band : legacy grip path, long-press
    );

    assert_eq!(outcome, DragEventOutcome::Cancelled);
    assert!(
        !ip.drag_states.contains_key(&1),
        "drag state must be cleared after cancellation"
    );
}

#[test]
fn long_press_not_cancelled_within_movement_tolerance() {
    let mut ip = build_processor();
    let eid = element_id();
    let (cx, cy) = handle_center();

    ip.process_drag_handle_pointer(
        &touch(pointer_down(cx, cy)),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band : legacy grip path, long-press
    );

    // Move 5dp — below 10dp threshold
    let outcome = ip.process_drag_handle_pointer(
        &touch(pointer_move(cx + 5.0, cy)),
        "drag-handle:aabb",
        eid,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band : legacy grip path, long-press
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
            false, // immediate : legacy grip long-press path
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
        false, // is_header_band : legacy grip path, long-press
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
        false, // is_header_band : legacy grip path, long-press
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
        false, // is_header_band : legacy grip path, long-press
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
            false, // immediate : legacy grip long-press path
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
        false, // is_header_band : legacy grip path, long-press
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
        false, // is_header_band : legacy grip path, long-press
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
        false, // is_header_band : legacy grip path, long-press
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
