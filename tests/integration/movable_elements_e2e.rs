//! E2E integration tests for persistent movable elements.
//!
//! Tests the cross-session persistence, element discovery and reuse,
//! display resolution change invariance, and agent notification contracts
//! for user-dragged elements.
//!
//! All tests are headless Layer 0-1: no display server or GPU required.
//!
//! Source: openspec/changes/persistent-movable-elements/
//!
//! Test status overview:
//!   Test 1 — Cross-session persistence:          ACTIVE
//!   Test 2 — Element discovery and reuse:        ACTIVE
//!   Test 3 — Reset fallback chain:               ACTIVE (hud-zc7f merged)
//!   Test 4 — Zone with config override + reset:  ACTIVE (hud-zc7f merged)
//!   Test 5 — Display resolution change:          ACTIVE
//!   Test 6 — Agent notification:                 ACTIVE

use tze_hud_input::{
    DeviceDragState, DragEventOutcome, DragHandleElementKind, DragPhase, InputProcessor,
    PointerEvent, PointerEventKind,
};
use tze_hud_runtime::element_store::{from_toml_str, to_toml_string};
use tze_hud_scene::{
    ElementStore, ElementStoreEntry, ElementType, GeometryPolicy, Rect, SceneGraph, SceneId,
    geometry_policy_to_absolute_rect, rect_to_relative_geometry_policy,
};

// ── Display constants ──────────────────────────────────────────────────────────

const DISPLAY_W: f32 = 1920.0;
const DISPLAY_H: f32 = 1080.0;

// Element bounds used across tests
const ELEMENT_X: f32 = 200.0;
const ELEMENT_Y: f32 = 150.0;
const ELEMENT_W: f32 = 400.0;
const ELEMENT_H: f32 = 200.0;

// Drag handle sits above the element center
const HANDLE_X: f32 = ELEMENT_X + ELEMENT_W / 2.0 - 12.0;
const HANDLE_Y: f32 = ELEMENT_Y - 4.0;
const HANDLE_W: f32 = 24.0;
const HANDLE_H: f32 = 8.0;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn element_bounds() -> Rect {
    Rect::new(ELEMENT_X, ELEMENT_Y, ELEMENT_W, ELEMENT_H)
}

fn handle_center() -> (f32, f32) {
    (HANDLE_X + HANDLE_W / 2.0, HANDLE_Y + HANDLE_H / 2.0)
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

fn pointer_move(x: f32, y: f32) -> PointerEvent {
    PointerEvent {
        x,
        y,
        kind: PointerEventKind::Move,
        device_id: 0,
        timestamp: None,
    }
}

/// Build an InputProcessor that is in the Activated phase and ready to drag.
///
/// Uses a 1ms hold threshold so tests can activate immediately without
/// real-time sleeping.
fn build_activated_processor(element_id: SceneId) -> InputProcessor {
    let (cx, cy) = handle_center();
    let mut ip = InputProcessor::new();
    // Seed with a 1ms-threshold DeviceDragState that has already been activated.
    let mut state = DeviceDragState::new(
        "drag-handle:tile".to_string(),
        element_id,
        DragHandleElementKind::Tile,
        cx,
        cy,
        1,     // 1ms threshold
        false, // immediate (hud-cpjqe): legacy grip long-press path
    );
    // Manually advance phase to Activated so we can immediately produce Moved/Released.
    state.phase = DragPhase::Activated;
    ip.drag_states.insert(0, state);
    ip
}

/// Drive the element from Activated to Released, returning `(final_x, final_y)`.
///
/// Moves the element to the target `(new_x, new_y)` then releases. Snapping
/// and clamping may adjust the final position slightly.
fn drag_to_then_release(
    ip: &mut InputProcessor,
    element_id: SceneId,
    new_x: f32,
    new_y: f32,
) -> (f32, f32) {
    let grab_off_x = ip.drag_states[&0].grab_offset_x;
    let grab_off_y = ip.drag_states[&0].grab_offset_y;
    let ptr_x = new_x + grab_off_x;
    let ptr_y = new_y + grab_off_y;

    // Move to new position
    ip.process_drag_handle_pointer(
        &pointer_move(ptr_x, ptr_y),
        "drag-handle:tile",
        element_id,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    // Release
    let released = ip.process_drag_handle_pointer(
        &pointer_up(ptr_x, ptr_y),
        "drag-handle:tile",
        element_id,
        DragHandleElementKind::Tile,
        element_bounds(),
        DISPLAY_W,
        DISPLAY_H,
        false, // is_header_band (hud-cpjqe): legacy grip path, long-press
    );

    match released {
        DragEventOutcome::Released {
            final_x, final_y, ..
        } => (final_x, final_y),
        other => panic!("expected Released outcome, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 1 — Cross-session persistence
//
// Agent creates tile → user drags tile to new position → runtime restarts
// (ElementStore is serialized and deserialized) → agent reconnects (new scene) →
// tile appears at user-overridden position (geometry_override preserved across
// the TOML round-trip).
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn cross_session_persistence_preserves_user_geometry_override() {
    // ── Session A: agent creates tile and user drags it ───────────────────────
    let tile_namespace = "agent-persistent-tile";
    let tile_id = SceneId::new();

    // User drags the element to a new position.
    let mut ip = build_activated_processor(tile_id);
    let target_x = 600.0_f32;
    let target_y = 400.0_f32;
    let (final_x, final_y) = drag_to_then_release(&mut ip, tile_id, target_x, target_y);

    // Persist the drag geometry into an ElementStore entry.
    let mut store = ElementStore::default();
    store.entries.insert(
        tile_id,
        ElementStoreEntry {
            element_type: ElementType::Tile,
            namespace: tile_namespace.to_string(),
            created_at: 1_000,
            last_published_at: 2_000,
            z_order: 0,
            unseen_restarts: 0,
            geometry_override: None,
        },
    );

    InputProcessor::persist_drag_geometry(
        &mut store,
        ElementType::Tile,
        tile_namespace,
        final_x,
        final_y,
        ELEMENT_W,
        ELEMENT_H,
        DISPLAY_W,
        DISPLAY_H,
    );

    // Verify geometry_override was written.
    let entry_a = store.entries.get(&tile_id).expect("entry must exist");
    let override_a = entry_a
        .geometry_override
        .expect("geometry_override must be set after drag");

    // ── Persist → TOML → reload (simulate runtime restart) ───────────────────
    let toml_str = to_toml_string(&store).expect("ElementStore TOML serialization must succeed");
    let reloaded_store = from_toml_str(&toml_str).expect("TOML deserialization must succeed");

    // ── Session B: verify the override survived the round-trip ───────────────
    let entry_b = reloaded_store
        .entries
        .get(&tile_id)
        .expect("entry must survive restart");
    assert_eq!(
        entry_b.namespace, tile_namespace,
        "namespace must survive TOML round-trip"
    );
    assert_eq!(
        entry_b.element_type,
        ElementType::Tile,
        "element_type must survive TOML round-trip"
    );

    let override_b = entry_b
        .geometry_override
        .expect("geometry_override must survive runtime restart");

    // Override must be identical after round-trip.
    assert_eq!(
        override_b, override_a,
        "geometry_override must be byte-identical after TOML serialization → deserialization"
    );

    // The override must reflect the user-dragged position: x_pct and y_pct
    // should match final_x/DISPLAY_W and final_y/DISPLAY_H within tolerance.
    if let GeometryPolicy::Relative {
        x_pct,
        y_pct,
        width_pct,
        height_pct,
    } = override_b
    {
        let expected_x_pct = final_x / DISPLAY_W;
        let expected_y_pct = final_y / DISPLAY_H;
        assert!(
            (x_pct - expected_x_pct).abs() < 1e-4,
            "persisted x_pct ({x_pct}) must match final_x/DISPLAY_W ({expected_x_pct})"
        );
        assert!(
            (y_pct - expected_y_pct).abs() < 1e-4,
            "persisted y_pct ({y_pct}) must match final_y/DISPLAY_H ({expected_y_pct})"
        );
        assert!(
            (width_pct - ELEMENT_W / DISPLAY_W).abs() < 1e-4,
            "persisted width_pct must match element_w/display_w"
        );
        assert!(
            (height_pct - ELEMENT_H / DISPLAY_H).abs() < 1e-4,
            "persisted height_pct must match element_h/display_h"
        );
    } else {
        panic!("expected Relative geometry policy, got {override_b:?}");
    }

    // Session B: agent-requested bounds (the tile's Rect) must be SUPERSEDED by the
    // user override. Resolve using the four-tier chain:
    //   user_override > agent_requested > config_override > default.
    let agent_requested = rect_to_relative_geometry_policy(
        Rect::new(ELEMENT_X, ELEMENT_Y, ELEMENT_W, ELEMENT_H),
        DISPLAY_W,
        DISPLAY_H,
    );
    let resolved = tze_hud_scene::resolve_geometry_override_chain(
        Some(override_b),
        Some(agent_requested),
        None,
        None,
    )
    .expect("chain must resolve to the user override");

    assert_eq!(
        resolved, override_b,
        "user override must win over agent-requested geometry in the resolution chain"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 2 — Element discovery and reuse
//
// Agent creates tile (gets SceneId) → disconnects → reconnects → queries the
// ElementStore by namespace filter → finds the same SceneId → user-override is
// preserved (geometry_override unchanged by reconnect).
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn element_discovery_by_namespace_returns_correct_scene_id_with_override_preserved() {
    let tile_namespace = "agent-discoverable-tile";
    let tile_id = SceneId::new();

    // Simulate Session A: tile is created, user drags it, geometry is persisted.
    let user_override = GeometryPolicy::Relative {
        x_pct: 0.35,
        y_pct: 0.25,
        width_pct: 0.20,
        height_pct: 0.15,
    };

    let mut store = ElementStore::default();
    store.entries.insert(
        tile_id,
        ElementStoreEntry {
            element_type: ElementType::Tile,
            namespace: tile_namespace.to_string(),
            created_at: 5_000,
            last_published_at: 6_000,
            z_order: 0,
            unseen_restarts: 0,
            geometry_override: Some(user_override),
        },
    );

    // Also add an unrelated entry to ensure namespace filtering is scoped.
    let other_id = SceneId::new();
    store.entries.insert(
        other_id,
        ElementStoreEntry {
            element_type: ElementType::Tile,
            namespace: "other-agent-tile".to_string(),
            created_at: 5_001,
            last_published_at: 6_001,
            z_order: 0,
            unseen_restarts: 0,
            geometry_override: None,
        },
    );

    // ── Simulate runtime restart: persist → reload ────────────────────────────
    let toml_str = to_toml_string(&store).expect("serialize must succeed");
    let reloaded = from_toml_str(&toml_str).expect("deserialize must succeed");

    // ── Session B: agent reconnects and queries by namespace ──────────────────
    // find_id_by_type_namespace simulates the ListElements(namespace_filter="agent-discoverable-")
    // lookup the runtime performs when reconnecting.
    let found_id = reloaded
        .find_id_by_type_namespace(ElementType::Tile, tile_namespace)
        .expect("element must be discoverable by (type, namespace) after restart");

    assert_eq!(
        found_id, tile_id,
        "discovered element_id must match the original tile_id from Session A"
    );

    // Namespace filter must not return unrelated entries.
    let other_found = reloaded.find_id_by_type_namespace(ElementType::Tile, "other-agent-tile");
    assert!(
        other_found.is_some(),
        "other-agent-tile must also be findable (not silently dropped)"
    );
    assert_ne!(
        other_found.unwrap(),
        tile_id,
        "other-agent-tile must not be confused with agent-discoverable-tile"
    );

    // User override must be intact — reconnect must NOT clear geometry_override.
    let reloaded_entry = reloaded
        .entries
        .get(&found_id)
        .expect("entry must exist for found_id");
    let reloaded_override = reloaded_entry
        .geometry_override
        .expect("user geometry_override must survive reconnect");

    assert_eq!(
        reloaded_override, user_override,
        "geometry_override must be identical after reconnect (user position preserved)"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 3 — Reset fallback chain
//
// User drags element → user resets position → element returns to
// agent-requested bounds (geometry_override cleared).
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn reset_position_clears_user_override_and_restores_agent_bounds() {
    let tile_namespace = "agent-resettable-tile";
    let tile_id = SceneId::new();

    // Simulate post-drag state: geometry_override is set.
    let user_override = GeometryPolicy::Relative {
        x_pct: 0.6,
        y_pct: 0.5,
        width_pct: 0.20,
        height_pct: 0.15,
    };
    let agent_requested = rect_to_relative_geometry_policy(element_bounds(), DISPLAY_W, DISPLAY_H);

    let mut store = ElementStore::default();
    store.entries.insert(
        tile_id,
        ElementStoreEntry {
            element_type: ElementType::Tile,
            namespace: tile_namespace.to_string(),
            created_at: 1_000,
            last_published_at: 2_000,
            z_order: 0,
            unseen_restarts: 0,
            geometry_override: Some(user_override),
        },
    );

    // Confirm user override wins before reset.
    let pre_reset = tze_hud_scene::resolve_geometry_override_chain(
        Some(user_override),
        Some(agent_requested),
        None,
        None,
    );
    assert_eq!(
        pre_reset,
        Some(user_override),
        "user override must win before reset"
    );

    // User triggers reset: clear the geometry_override.
    let cleared = store.reset_geometry_override(tile_id);
    assert_eq!(
        cleared,
        Some(user_override),
        "reset_geometry_override must return the previously-set override"
    );

    // geometry_override must now be None.
    let entry = store.entries.get(&tile_id).expect("entry must exist");
    assert!(
        entry.geometry_override.is_none(),
        "geometry_override must be None after reset"
    );

    // Resolve chain after reset: agent-requested bounds must win.
    let post_reset = tze_hud_scene::resolve_geometry_override_chain(
        entry.geometry_override,
        Some(agent_requested),
        None,
        None,
    );
    assert_eq!(
        post_reset,
        Some(agent_requested),
        "after reset, agent-requested bounds must be returned by the fallback chain"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 4 — Zone with config override
//
// Zone with config-level geometry_override → user drags → user resets →
// zone returns to config override (NOT default geometry policy).
//
// After reset, the chain user_override=None, agent_requested=None,
// config_override=Some(X) must return X (not None / default).
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn zone_reset_falls_back_to_config_override_not_default_policy() {
    let zone_id = SceneId::new();

    // Config-level geometry override (set by zone profile, not the user).
    let config_override = GeometryPolicy::Relative {
        x_pct: 0.1,
        y_pct: 0.1,
        width_pct: 0.25,
        height_pct: 0.15,
    };

    // Default policy (lower priority than config_override).
    let default_policy = GeometryPolicy::Relative {
        x_pct: 0.0,
        y_pct: 0.0,
        width_pct: 0.5,
        height_pct: 0.5,
    };

    // User drags the zone: geometry_override is set in the store.
    let user_drag_override = GeometryPolicy::Relative {
        x_pct: 0.7,
        y_pct: 0.6,
        width_pct: 0.25,
        height_pct: 0.15,
    };

    let mut store = ElementStore::default();
    store.entries.insert(
        zone_id,
        ElementStoreEntry {
            element_type: ElementType::Zone,
            namespace: "zone-with-config-override".to_string(),
            created_at: 1_000,
            last_published_at: 2_000,
            z_order: 0,
            unseen_restarts: 0,
            geometry_override: Some(user_drag_override),
        },
    );

    // While user override is set, it wins over both config and default.
    let pre_reset = tze_hud_scene::resolve_geometry_override_chain(
        Some(user_drag_override),
        None,
        Some(config_override),
        Some(default_policy),
    );
    assert_eq!(
        pre_reset,
        Some(user_drag_override),
        "user drag override must win before reset"
    );

    // User resets: clear the geometry_override.
    let cleared = store.reset_geometry_override(zone_id);
    assert_eq!(
        cleared,
        Some(user_drag_override),
        "reset_geometry_override must return the previously-set override"
    );

    let entry = store.entries.get(&zone_id).expect("entry must exist");
    assert!(
        entry.geometry_override.is_none(),
        "geometry_override must be None after reset"
    );

    // After reset: config_override must win, NOT the default policy.
    let post_reset = tze_hud_scene::resolve_geometry_override_chain(
        entry.geometry_override,
        None,
        Some(config_override),
        Some(default_policy),
    );
    assert_eq!(
        post_reset,
        Some(config_override),
        "after reset, config_override must be returned (not default_policy)"
    );

    // Explicitly confirm default_policy is NOT returned.
    assert_ne!(
        post_reset,
        Some(default_policy),
        "default_policy must NOT win when config_override is present after reset"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 5 — Display resolution change
//
// User drags element to center of 1920×1080 (x_pct=0.5, y_pct=0.5) → display
// changes to 3840×2160 → element renders at (1920, 1080) — still center.
//
// The geometry_override is stored as relative percentages, so the element
// tracks the same proportional position regardless of display resolution.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn display_resolution_change_preserves_relative_center_position() {
    // ── Original display: 1920×1080 ───────────────────────────────────────────
    let original_w = 1920.0_f32;
    let original_h = 1080.0_f32;

    // Element size, stored as a relative percentage of the display dimensions.
    let elem_w = 384.0_f32; // 20% of 1920
    let elem_h = 216.0_f32; // 20% of 1080

    // User drags element to center of original display.
    // Center means top-left is at (display/2 - width/2, display/2 - height/2).
    let center_x = (original_w - elem_w) / 2.0; // 768.0
    let center_y = (original_h - elem_h) / 2.0; // 432.0

    // Persist the geometry override as a relative policy.
    let mut store = ElementStore::default();
    let tile_id = SceneId::new();
    store.entries.insert(
        tile_id,
        ElementStoreEntry {
            element_type: ElementType::Tile,
            namespace: "resolution-test-tile".to_string(),
            created_at: 1,
            last_published_at: 1,
            z_order: 0,
            unseen_restarts: 0,
            geometry_override: None,
        },
    );

    InputProcessor::persist_drag_geometry(
        &mut store,
        ElementType::Tile,
        "resolution-test-tile",
        center_x,
        center_y,
        elem_w,
        elem_h,
        original_w,
        original_h,
    );

    let entry = store.entries.get(&tile_id).expect("entry must exist");
    let override_policy = entry
        .geometry_override
        .expect("geometry_override must be set");

    // Verify the override was stored as x_pct=0.4, y_pct=0.4 (center_x/W = 768/1920 = 0.4).
    if let GeometryPolicy::Relative { x_pct, y_pct, .. } = override_policy {
        let expected_x_pct = center_x / original_w;
        let expected_y_pct = center_y / original_h;
        assert!(
            (x_pct - expected_x_pct).abs() < 1e-4,
            "x_pct must be center_x/original_w = {expected_x_pct}, got {x_pct}"
        );
        assert!(
            (y_pct - expected_y_pct).abs() < 1e-4,
            "y_pct must be center_y/original_h = {expected_y_pct}, got {y_pct}"
        );
    } else {
        panic!("expected Relative geometry policy, got {override_policy:?}");
    }

    // ── Display changes to 3840×2160 (2× HiDPI) ──────────────────────────────
    let new_w = 3840.0_f32;
    let new_h = 2160.0_f32;

    // Apply the same relative override policy to the new display size.
    let new_rect = geometry_policy_to_absolute_rect(override_policy, new_w, new_h);

    // Element top-left should now be at (1920, 1080) — the center of 3840×2160.
    let expected_new_x = center_x / original_w * new_w; // 768/1920 * 3840 = 1536
    let expected_new_y = center_y / original_h * new_h; // 432/1080 * 2160 = 864

    assert!(
        (new_rect.x - expected_new_x).abs() < 1.0,
        "at 3840×2160, element x must be {expected_new_x}px (same relative position), got {}",
        new_rect.x
    );
    assert!(
        (new_rect.y - expected_new_y).abs() < 1.0,
        "at 3840×2160, element y must be {expected_new_y}px (same relative position), got {}",
        new_rect.y
    );

    // Width and height must also scale proportionally.
    let expected_new_w = elem_w / original_w * new_w; // 384/1920 * 3840 = 768
    let expected_new_h = elem_h / original_h * new_h; // 216/1080 * 2160 = 432
    assert!(
        (new_rect.width - expected_new_w).abs() < 1.0,
        "element width must scale to {expected_new_w} at new resolution, got {}",
        new_rect.width
    );
    assert!(
        (new_rect.height - expected_new_h).abs() < 1.0,
        "element height must scale to {expected_new_h} at new resolution, got {}",
        new_rect.height
    );

    // Confirm proportional center: x_pct and y_pct of the result on the new display
    // must equal the original percentages.
    let result_policy = rect_to_relative_geometry_policy(new_rect, new_w, new_h);
    if let GeometryPolicy::Relative {
        x_pct: rx,
        y_pct: ry,
        ..
    } = result_policy
    {
        if let GeometryPolicy::Relative {
            x_pct: ox,
            y_pct: oy,
            ..
        } = override_policy
        {
            assert!(
                (rx - ox).abs() < 1e-4,
                "x_pct must be invariant under resolution change: expected {ox}, got {rx}"
            );
            assert!(
                (ry - oy).abs() < 1e-4,
                "y_pct must be invariant under resolution change: expected {oy}, got {ry}"
            );
        }
    }
}

/// Variant: element at exact 50% center of 1920×1080 renders at (1920, 1080) on 3840×2160.
///
/// This is the canonical example from the spec: x_pct=0.5, y_pct=0.5 on a
/// display that doubles in each dimension maps to (new_w/2, new_h/2).
#[test]
fn display_resolution_double_center_example_from_spec() {
    // Store a 50% / 50% relative override directly (no drag needed).
    let override_policy = GeometryPolicy::Relative {
        x_pct: 0.5,
        y_pct: 0.5,
        width_pct: 0.2,
        height_pct: 0.2,
    };

    let original_w = 1920.0_f32;
    let original_h = 1080.0_f32;

    // On the original display, this places the element at (960, 540).
    let orig_rect = geometry_policy_to_absolute_rect(override_policy, original_w, original_h);
    assert!(
        (orig_rect.x - 960.0).abs() < 0.5,
        "x on 1920×1080 must be 960 (50%), got {}",
        orig_rect.x
    );
    assert!(
        (orig_rect.y - 540.0).abs() < 0.5,
        "y on 1920×1080 must be 540 (50%), got {}",
        orig_rect.y
    );

    // On a 3840×2160 display the same percentages place the element at (1920, 1080).
    let new_w = 3840.0_f32;
    let new_h = 2160.0_f32;
    let new_rect = geometry_policy_to_absolute_rect(override_policy, new_w, new_h);

    assert!(
        (new_rect.x - 1920.0).abs() < 0.5,
        "x on 3840×2160 must be 1920 (50%), got {}",
        new_rect.x
    );
    assert!(
        (new_rect.y - 1080.0).abs() < 0.5,
        "y on 3840×2160 must be 1080 (50%), got {}",
        new_rect.y
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 6 — Agent notification
//
// Simulates the runtime path: persist drag geometry → emit_drag_repositioned_event
// → agent receives ElementRepositionedEvent with correct old_geometry and
// new_geometry.
//
// Uses HudSessionImpl directly (Layer 1 headless gRPC, no display server or GPU).
// ElementRepositionedEvent is broadcast via the public channel landed in hud-bs2q.6.
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn agent_receives_element_repositioned_event_with_old_and_new_geometry() {
    use tokio_stream::StreamExt as _;
    use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
    use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
    use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
    use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
    use tze_hud_protocol::proto::session::{ClientMessage, SessionInit};
    use tze_hud_protocol::session_server::HudSessionImpl;

    // ── Setup: bind gRPC server on a dynamic port ─────────────────────────────
    let scene = SceneGraph::new(DISPLAY_W, DISPLAY_H);
    let service = HudSessionImpl::new(scene, "test-key");
    let reposition_tx = service.element_repositioned_tx.clone();

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let _server = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    // ── Connect agent with SCENE_TOPOLOGY subscription ────────────────────────
    let mut client = HudSessionClient::connect(format!("http://[::1]:{}", addr.port()))
        .await
        .unwrap();

    let now_us = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    tx.send(ClientMessage {
        sequence: 1,
        timestamp_wall_us: now_us(),
        payload: Some(ClientPayload::SessionInit(SessionInit {
            agent_id: "test-agent-notify".to_string(),
            agent_display_name: "test-agent-notify".to_string(),
            pre_shared_key: "test-key".to_string(),
            requested_capabilities: vec![
                "create_tiles".to_string(),
                "read_scene_topology".to_string(),
            ],
            initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
            resume_token: vec![],
            min_protocol_version: 1000,
            max_protocol_version: 1001,
            auth_credential: None,
        })),
    })
    .await
    .unwrap();

    let mut stream = client.session(stream).await.unwrap().into_inner();

    // Drain SessionEstablished + SceneSnapshot + current DegradationNotice.
    stream.next().await;
    stream.next().await;
    stream.next().await;

    // Give the session handler time to complete subscription.
    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

    // ── Simulate drag: compute old and new geometry ───────────────────────────
    let tile_id = SceneId::new();

    // Agent-requested bounds (old geometry before drag).
    let agent_bounds = element_bounds();
    let old_geometry = rect_to_relative_geometry_policy(agent_bounds, DISPLAY_W, DISPLAY_H);

    // User drags to new position.
    let target_x = 600.0_f32;
    let target_y = 400.0_f32;
    let new_geometry = rect_to_relative_geometry_policy(
        Rect::new(target_x, target_y, ELEMENT_W, ELEMENT_H),
        DISPLAY_W,
        DISPLAY_H,
    );

    // ── Emit: simulate what the runtime does after persist_drag_geometry ──────
    // We broadcast via reposition_tx directly, mirroring what the runtime would
    // do after persist_drag_geometry writes the geometry_override (hud-bs2q.6).
    let event = tze_hud_protocol::proto::ElementRepositionedEvent {
        element_id: tile_id.as_uuid().as_bytes().to_vec(),
        new_geometry: Some(tze_hud_protocol::convert::geometry_policy_to_proto(
            &new_geometry,
        )),
        previous_geometry: Some(tze_hud_protocol::convert::geometry_policy_to_proto(
            &old_geometry,
        )),
    };
    let _ = reposition_tx.send(event);

    // ── Assert: agent receives ElementRepositionedEvent ───────────────────────
    let msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), stream.next())
        .await
        .expect("timed out waiting for ElementRepositionedEvent")
        .expect("stream must not close")
        .expect("must not error");

    drop(tx); // close agent stream

    match msg.payload {
        Some(ServerPayload::ElementRepositioned(ev)) => {
            // element_id must round-trip correctly.
            let expected_id = tile_id.as_uuid().as_bytes().to_vec();
            assert_eq!(ev.element_id, expected_id, "element_id must match");

            // new_geometry must be Relative and match the drag target.
            let ng = ev.new_geometry.expect("new_geometry must be set");
            match ng.policy {
                Some(tze_hud_protocol::proto::geometry_policy_proto::Policy::Relative(r)) => {
                    let expected_x_pct = target_x / DISPLAY_W;
                    let expected_y_pct = target_y / DISPLAY_H;
                    assert!(
                        (r.x_pct - expected_x_pct).abs() < 1e-4,
                        "new_geometry x_pct must be {expected_x_pct:.4}, got {:.4}",
                        r.x_pct
                    );
                    assert!(
                        (r.y_pct - expected_y_pct).abs() < 1e-4,
                        "new_geometry y_pct must be {expected_y_pct:.4}, got {:.4}",
                        r.y_pct
                    );
                }
                other => panic!("expected Relative new_geometry, got {other:?}"),
            }

            // previous_geometry must reflect the original agent-requested bounds.
            let pg = ev.previous_geometry.expect("previous_geometry must be set");
            match pg.policy {
                Some(tze_hud_protocol::proto::geometry_policy_proto::Policy::Relative(r)) => {
                    let expected_old_x = ELEMENT_X / DISPLAY_W;
                    let expected_old_y = ELEMENT_Y / DISPLAY_H;
                    assert!(
                        (r.x_pct - expected_old_x).abs() < 1e-4,
                        "previous_geometry x_pct must be {expected_old_x:.4}, got {:.4}",
                        r.x_pct
                    );
                    assert!(
                        (r.y_pct - expected_old_y).abs() < 1e-4,
                        "previous_geometry y_pct must be {expected_old_y:.4}, got {:.4}",
                        r.y_pct
                    );
                }
                other => panic!("expected Relative previous_geometry, got {other:?}"),
            }
        }
        other => panic!("expected ElementRepositioned, got {other:?}"),
    }
}
