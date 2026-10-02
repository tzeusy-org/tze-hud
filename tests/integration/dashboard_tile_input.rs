//! Dashboard tile HitRegionNode input capture, local feedback, and focus cycling tests.
//!
//! Implements acceptance criteria for `hud-i6yd.5` (tasks 7, 9 from
//! openspec/changes/exemplar-dashboard-tile/tasks.md):
//!
//! **Task 7 — Input Capture and Local Feedback:**
//! - PointerDown at Refresh bounds → NodeHit with interaction_id="refresh-button"
//! - pressed=true set within p99 < 4ms of PointerDownEvent (headless synthetic injection)
//! - hovered set on PointerEnter, cleared on PointerLeave for both buttons
//! - PointerUp with release_on_up=true clears pressed and releases pointer capture
//!
//! **Task 9 — Focus Cycling:**
//! - Focus ring rendered when focus transfers to HitRegionNode via Tab or click
//! - Tab cycles focus Refresh → Dismiss
//! - Shift+Tab cycles reverse (Dismiss → Refresh)
//! - FocusGainedEvent and FocusLostEvent dispatched on focus transitions
//! - Both buttons reachable and activatable without pointer (NAVIGATE_NEXT + ACTIVATE)
//!
//! All tests are headless Layer 0: no display server or GPU required.
//!
//! Source: openspec/changes/exemplar-dashboard-tile/specs/exemplar-dashboard-tile/spec.md
//!         §Requirement: HitRegionNode Local Feedback (lines 118-138)
//!         §Requirement: Focus Cycling Between Buttons (lines 166-182)

use tze_hud_input::{
    CaptureReleasedReason, CommandAction, CommandProcessor, CommandSource, FocusLostReason,
    FocusManager, FocusOwner, FocusSource, InputProcessor, PointerEvent, PointerEventKind,
    RawCommandEvent,
};
use tze_hud_scene::{
    Capability, HitRegionNode, Node, NodeData, Rect, SceneGraph, SceneId, SolidColorNode,
};

// ── Dashboard tile geometry (from spec.md lines 5-11) ──────────────────────────
//
// Tile bounds:    (50, 50, 400, 300) in display space
// Refresh button: bounds (16, 256, 176, 36) — tile-local
//                 display-space: x=(50+16)=66, y=(50+256)=306, w=176, h=36
// Dismiss button: bounds (208, 256, 176, 36) — tile-local
//                 display-space: x=(50+208)=258, y=(50+256)=306, w=176, h=36

const TILE_X: f32 = 50.0;
const TILE_Y: f32 = 50.0;
const TILE_W: f32 = 400.0;
const TILE_H: f32 = 300.0;

// Refresh button — tile-local bounds
const REFRESH_LOCAL_X: f32 = 16.0;
const REFRESH_LOCAL_Y: f32 = 256.0;
const REFRESH_W: f32 = 176.0;
const REFRESH_H: f32 = 36.0;

// Dismiss button — tile-local bounds
const DISMISS_LOCAL_X: f32 = 208.0;
const DISMISS_LOCAL_Y: f32 = 256.0;
const DISMISS_W: f32 = 176.0;
const DISMISS_H: f32 = 36.0;

// Display-space center of each button (for pointer events)
fn refresh_center_display() -> (f32, f32) {
    (
        TILE_X + REFRESH_LOCAL_X + REFRESH_W / 2.0,
        TILE_Y + REFRESH_LOCAL_Y + REFRESH_H / 2.0,
    )
}

fn dismiss_center_display() -> (f32, f32) {
    (
        TILE_X + DISMISS_LOCAL_X + DISMISS_W / 2.0,
        TILE_Y + DISMISS_LOCAL_Y + DISMISS_H / 2.0,
    )
}

// ── Scene setup helpers ────────────────────────────────────────────────────────

/// Create a scene with a single dashboard tile containing a background root node,
/// Refresh button, and Dismiss button (3-node tree mirroring the exemplar spec).
///
/// Returns (scene, tab_id, tile_id, refresh_node_id, dismiss_node_id).
fn setup_dashboard_tile_scene() -> (SceneGraph, SceneId, SceneId, SceneId, SceneId) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();
    let lease_id = scene.grant_lease(
        "dashboard-agent",
        60_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );

    let tile_id = scene
        .create_tile(
            tab_id,
            "dashboard-agent",
            lease_id,
            Rect::new(TILE_X, TILE_Y, TILE_W, TILE_H),
            100,
        )
        .unwrap();

    // Build the node tree: SolidColor bg root with Refresh and Dismiss as children.
    let refresh_id = SceneId::new();
    let dismiss_id = SceneId::new();
    let bg_id = SceneId::new();

    // Refresh button node
    scene.nodes.insert(
        refresh_id,
        Node {
            layout: Default::default(),
            id: refresh_id,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(REFRESH_LOCAL_X, REFRESH_LOCAL_Y, REFRESH_W, REFRESH_H),
                interaction_id: "refresh-button".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                auto_capture: true,
                release_on_up: true,
                ..Default::default()
            }),
        },
    );

    // Dismiss button node
    scene.nodes.insert(
        dismiss_id,
        Node {
            layout: Default::default(),
            id: dismiss_id,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(DISMISS_LOCAL_X, DISMISS_LOCAL_Y, DISMISS_W, DISMISS_H),
                interaction_id: "dismiss-button".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                auto_capture: true,
                release_on_up: true,
                ..Default::default()
            }),
        },
    );

    // Background root node (SolidColor, non-interactive)
    scene.nodes.insert(
        bg_id,
        Node {
            layout: Default::default(),
            id: bg_id,
            children: vec![refresh_id, dismiss_id],
            data: NodeData::SolidColor(SolidColorNode {
                color: tze_hud_scene::Rgba::new(0.07, 0.07, 0.07, 0.90),
                bounds: Rect::new(0.0, 0.0, TILE_W, TILE_H),
                radius: None,
            }),
        },
    );

    // Register local states for HitRegionNodes
    scene.hit_region_states.insert(
        refresh_id,
        tze_hud_scene::HitRegionLocalState::new(refresh_id),
    );
    scene.hit_region_states.insert(
        dismiss_id,
        tze_hud_scene::HitRegionLocalState::new(dismiss_id),
    );

    // Set bg as tile root
    scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(bg_id);

    (scene, tab_id, tile_id, refresh_id, dismiss_id)
}

fn raw_command(action: CommandAction) -> RawCommandEvent {
    RawCommandEvent {
        action,
        source: CommandSource::Keyboard,
        device_id: "keyboard-0".to_string(),
        timestamp_mono_us: tze_hud_scene::MonoUs(10_000),
    }
}

// ── Task 7: Input Capture and Local Feedback ───────────────────────────────────

/// Spec §Requirement: HitRegionNode Local Feedback, Scenario: Button pressed state on pointer down
///
/// WHEN a PointerDownEvent lands on the "Refresh" HitRegionNode
/// THEN the runtime SHALL set pressed = true on that node within p99 < 4ms
/// and apply the default press visual (multiply by 0.85 darkening) without
/// waiting for the agent.
#[test]
fn pointer_down_at_refresh_returns_node_hit_with_refresh_interaction_id() {
    let (mut scene, _tab_id, _tile_id, refresh_id, _dismiss_id) = setup_dashboard_tile_scene();
    let mut processor = InputProcessor::new();

    let (cx, cy) = refresh_center_display();
    let result = processor.process(
        &PointerEvent {
            x: cx,
            y: cy,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        },
        &mut scene,
    );

    // Must return a NodeHit with the refresh button's interaction_id
    assert!(
        result.hit.is_some(),
        "PointerDown at Refresh bounds must produce a hit"
    );
    assert_eq!(
        result.interaction_id,
        Some("refresh-button".to_string()),
        "interaction_id must be 'refresh-button'"
    );

    // pressed=true must be set on the refresh node
    assert!(
        scene.hit_region_states[&refresh_id].pressed,
        "pressed must be true after PointerDown on Refresh"
    );
}

// ─── Timing-assertion gate (hud-94vm5) ───────────────────────────────────────

/// Returns `true` when wall-clock / p99 latency hard assertions should run.
///
/// Set `TZE_HUD_PERF_ASSERT=1` to enable.  On the standard `test-unit` / blocking
/// CI lane this is unset; budget assertions are skipped to avoid
/// flakes from scheduler noise on shared runners.
fn perf_assert_enabled() -> bool {
    std::env::var("TZE_HUD_PERF_ASSERT")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Spec §Requirement: HitRegionNode Local Feedback, Scenario: Button pressed state on pointer down
///
/// local_ack_us (time from event arrival to pressed=true) must be within p99 < 4ms.
/// This is a headless Layer 0 performance test injecting synthetic PointerDown.
#[test]
fn pressed_state_set_within_4ms_p99_on_refresh() {
    use std::time::Instant;
    use tze_hud_scene::perf_budget::{budgets, test_budget};

    let ack_budget = test_budget(budgets::INPUT_ACK_BUDGET_US);

    let (cx, cy) = refresh_center_display();

    // Sample 100 synthetic PointerDown events, measure local_ack_us each time.
    let mut durations: Vec<u64> = (0..100)
        .map(|_| {
            // Re-create scene each time to avoid state pollution
            let (mut scene, _tab_id, _tile_id, _refresh_id, _dismiss_id) =
                setup_dashboard_tile_scene();
            let mut processor = InputProcessor::new();

            let _warmup = processor.process(
                &PointerEvent {
                    x: cx,
                    y: cy,
                    kind: PointerEventKind::Move,
                    device_id: 0,
                    timestamp: None,
                },
                &mut scene,
            );

            let start = Instant::now();
            let result = processor.process(
                &PointerEvent {
                    x: cx,
                    y: cy,
                    kind: PointerEventKind::Down,
                    device_id: 0,
                    timestamp: None,
                },
                &mut scene,
            );
            let elapsed_us = start.elapsed().as_micros() as u64;

            // Sanity check: pressed state was applied
            assert!(
                result.hit.is_some(),
                "event must hit refresh button each iteration"
            );
            elapsed_us
        })
        .collect();

    durations.sort_unstable();
    let p99 = durations[98]; // 99th percentile of 100 samples

    // Timing assertion: gated — wall-clock budget.  (hud-94vm5)
    if perf_assert_enabled() {
        assert!(
            p99 < ack_budget,
            "pressed state local_ack p99 was {}µs; budget is {}µs (base: {}µs)",
            p99,
            ack_budget,
            budgets::INPUT_ACK_BUDGET_US,
        );
    } else {
        eprintln!(
            "[SKIP-TIMING] pressed_state local_ack p99={p99}µs; \
             set TZE_HUD_PERF_ASSERT=1 to enforce budget"
        );
    }
}

/// Spec §Requirement: HitRegionNode Local Feedback, Scenario: Button hovered state on pointer enter
///
/// WHEN the pointer enters the bounds of the "Dismiss" HitRegionNode
/// THEN the runtime SHALL set hovered = true and apply the default hover visual
/// (0.1 white overlay) within p99 < 4ms.
#[test]
fn hover_state_set_on_pointer_enter_dismiss() {
    let (mut scene, _tab_id, _tile_id, refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut processor = InputProcessor::new();

    let (cx, cy) = dismiss_center_display();

    // Move over dismiss button — should set hovered=true
    let result = processor.process(
        &PointerEvent {
            x: cx,
            y: cy,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        },
        &mut scene,
    );

    assert!(
        result.hit.is_some(),
        "pointer should hit the Dismiss button"
    );
    assert_eq!(
        result.interaction_id,
        Some("dismiss-button".to_string()),
        "should have interaction_id 'dismiss-button'"
    );
    assert!(
        scene.hit_region_states[&dismiss_id].hovered,
        "hovered must be true for Dismiss after PointerEnter"
    );
    // Refresh must NOT be hovered
    assert!(
        !scene.hit_region_states[&refresh_id].hovered,
        "Refresh must not be hovered when Dismiss is hovered"
    );

    // The local patch must carry hovered=true for dismiss
    let hover_update = result
        .local_patch
        .node_updates
        .iter()
        .find(|u| u.node_id == dismiss_id && u.hovered == Some(true));
    assert!(
        hover_update.is_some(),
        "local_patch must contain hovered=true update for Dismiss"
    );
}

/// Spec §Requirement: HitRegionNode Local Feedback, Scenario: Button hovered state on pointer enter (Refresh)
#[test]
fn hover_state_set_on_pointer_enter_refresh() {
    let (mut scene, _tab_id, _tile_id, refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut processor = InputProcessor::new();

    let (cx, cy) = refresh_center_display();

    let result = processor.process(
        &PointerEvent {
            x: cx,
            y: cy,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        },
        &mut scene,
    );

    assert!(
        scene.hit_region_states[&refresh_id].hovered,
        "hovered must be true for Refresh after PointerEnter"
    );
    assert!(
        !scene.hit_region_states[&dismiss_id].hovered,
        "Dismiss must not be hovered when Refresh is hovered"
    );

    let hover_update = result
        .local_patch
        .node_updates
        .iter()
        .find(|u| u.node_id == refresh_id && u.hovered == Some(true));
    assert!(
        hover_update.is_some(),
        "local_patch must contain hovered=true update for Refresh"
    );
}

/// Spec §Requirement: HitRegionNode Local Feedback — hover cleared on leave
///
/// After hovering over Refresh, moving the pointer away must clear hovered=false.
#[test]
fn hover_cleared_on_pointer_leave_refresh() {
    let (mut scene, _tab_id, _tile_id, refresh_id, _dismiss_id) = setup_dashboard_tile_scene();
    let mut processor = InputProcessor::new();

    let (cx, cy) = refresh_center_display();

    // Enter
    processor.process(
        &PointerEvent {
            x: cx,
            y: cy,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        },
        &mut scene,
    );
    assert!(scene.hit_region_states[&refresh_id].hovered);

    // Move completely away from tile
    let result = processor.process(
        &PointerEvent {
            x: 5.0,
            y: 5.0,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        },
        &mut scene,
    );
    assert!(
        !scene.hit_region_states[&refresh_id].hovered,
        "hovered must be false after pointer leaves Refresh bounds"
    );

    let leave_update = result
        .local_patch
        .node_updates
        .iter()
        .find(|u| u.node_id == refresh_id && u.hovered == Some(false));
    assert!(
        leave_update.is_some(),
        "local_patch must contain hovered=false for Refresh on leave"
    );
}

/// Spec §Requirement: HitRegionNode Local Feedback — hover cleared on leave (Dismiss)
#[test]
fn hover_cleared_on_pointer_leave_dismiss() {
    let (mut scene, _tab_id, _tile_id, _refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut processor = InputProcessor::new();

    let (cx, cy) = dismiss_center_display();

    processor.process(
        &PointerEvent {
            x: cx,
            y: cy,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        },
        &mut scene,
    );
    assert!(scene.hit_region_states[&dismiss_id].hovered);

    let result = processor.process(
        &PointerEvent {
            x: 5.0,
            y: 5.0,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        },
        &mut scene,
    );
    assert!(
        !scene.hit_region_states[&dismiss_id].hovered,
        "hovered must be false after pointer leaves Dismiss bounds"
    );

    let leave_update = result
        .local_patch
        .node_updates
        .iter()
        .find(|u| u.node_id == dismiss_id && u.hovered == Some(false));
    assert!(
        leave_update.is_some(),
        "local_patch must contain hovered=false for Dismiss on leave"
    );
}

/// Spec §Requirement: HitRegionNode Local Feedback, Scenario: PointerUp clears pressed and releases capture
///
/// WHEN a PointerUpEvent occurs while the "Refresh" button has pressed=true
/// and release_on_up=true
/// THEN the runtime SHALL clear pressed=false and release pointer capture.
#[test]
fn pointer_up_clears_pressed_and_releases_capture_on_refresh() {
    let (mut scene, _tab_id, _tile_id, refresh_id, _dismiss_id) = setup_dashboard_tile_scene();
    let mut processor = InputProcessor::new();
    let device_id = 0u32;

    let (cx, cy) = refresh_center_display();

    // PointerDown — auto_capture=true so capture is acquired automatically
    processor.process(
        &PointerEvent {
            x: cx,
            y: cy,
            kind: PointerEventKind::Down,
            device_id,
            timestamp: None,
        },
        &mut scene,
    );

    // Verify pressed=true and capture active
    assert!(
        scene.hit_region_states[&refresh_id].pressed,
        "pressed must be true after PointerDown"
    );
    assert!(
        processor.capture.is_captured(device_id),
        "capture must be active after PointerDown with auto_capture=true"
    );
    assert_eq!(
        processor.capture.get(device_id).unwrap().node_id,
        refresh_id,
        "capture must be for the Refresh node"
    );
    assert!(
        processor.capture.get(device_id).unwrap().release_on_up,
        "release_on_up must be true (from HitRegionNode.release_on_up=true)"
    );

    // PointerUp — release_on_up=true, so pressed cleared and capture released
    let result = processor.process(
        &PointerEvent {
            x: cx,
            y: cy,
            kind: PointerEventKind::Up,
            device_id,
            timestamp: None,
        },
        &mut scene,
    );

    // pressed must be cleared
    assert!(
        !scene.hit_region_states[&refresh_id].pressed,
        "pressed must be false after PointerUp"
    );
    // capture must be released
    assert!(
        !processor.capture.is_captured(device_id),
        "capture must be released after PointerUp with release_on_up=true"
    );

    // Local patch must carry pressed=false
    let press_update = result
        .local_patch
        .node_updates
        .iter()
        .find(|u| u.node_id == refresh_id && u.pressed == Some(false));
    assert!(
        press_update.is_some(),
        "local_patch must contain pressed=false for Refresh on PointerUp"
    );

    // CaptureReleased must be in extra_dispatches with reason=PointerUp
    assert_eq!(
        result.extra_dispatches.len(),
        1,
        "CaptureReleased dispatch must be in extra_dispatches"
    );
    assert_eq!(
        result.extra_dispatches[0].capture_released_reason,
        Some(CaptureReleasedReason::PointerUp),
        "CaptureReleased reason must be PointerUp"
    );
}

/// Same test for the Dismiss button.
#[test]
fn pointer_up_clears_pressed_and_releases_capture_on_dismiss() {
    let (mut scene, _tab_id, _tile_id, _refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut processor = InputProcessor::new();
    let device_id = 0u32;

    let (cx, cy) = dismiss_center_display();

    processor.process(
        &PointerEvent {
            x: cx,
            y: cy,
            kind: PointerEventKind::Down,
            device_id,
            timestamp: None,
        },
        &mut scene,
    );

    assert!(scene.hit_region_states[&dismiss_id].pressed);
    assert!(processor.capture.is_captured(device_id));
    assert_eq!(
        processor.capture.get(device_id).unwrap().node_id,
        dismiss_id
    );

    processor.process(
        &PointerEvent {
            x: cx,
            y: cy,
            kind: PointerEventKind::Up,
            device_id,
            timestamp: None,
        },
        &mut scene,
    );

    assert!(!scene.hit_region_states[&dismiss_id].pressed);
    assert!(!processor.capture.is_captured(device_id));
}

// ── Task 9: Focus Cycling ──────────────────────────────────────────────────────

/// Spec §Requirement: Focus Cycling Between Buttons, Scenario: Tab advances Refresh → Dismiss
///
/// WHEN the "Refresh" HitRegionNode has focus and the user presses Tab
/// THEN focus SHALL transfer to the "Dismiss" HitRegionNode, dispatching
/// FocusLostEvent to Refresh and FocusGainedEvent(source=TabKey) to the agent for Dismiss.
#[test]
fn tab_advances_focus_from_refresh_to_dismiss() {
    let (scene, tab_id, tile_id, refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut fm = FocusManager::new();
    fm.add_tab(tab_id);

    // Give Refresh focus via click
    let click_t = fm.on_click(tab_id, tile_id, Some(refresh_id), &scene);
    assert!(click_t.gained.is_some(), "click should give Refresh focus");

    let owner = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        owner,
        FocusOwner::Node {
            tile_id,
            node_id: refresh_id
        },
        "focus must be on Refresh"
    );

    // Tab → Dismiss
    let t = fm.navigate_next(tab_id, &scene);

    let new_owner = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        new_owner,
        FocusOwner::Node {
            tile_id,
            node_id: dismiss_id
        },
        "Tab must advance focus to Dismiss"
    );

    // FocusLostEvent dispatched to Refresh (reason=TabKey)
    let (lost_ev, lost_ns) = t.lost.expect("FocusLostEvent must be dispatched");
    assert_eq!(
        lost_ev.reason,
        FocusLostReason::TabKey,
        "FocusLostEvent reason must be TabKey"
    );
    assert_eq!(
        lost_ev.tile_id, tile_id,
        "FocusLostEvent tile_id must be the dashboard tile"
    );
    assert_eq!(
        lost_ev.node_id,
        Some(refresh_id),
        "FocusLostEvent node_id must be Refresh"
    );
    assert_eq!(
        lost_ns, "dashboard-agent",
        "FocusLostEvent namespace must be 'dashboard-agent'"
    );

    // FocusGainedEvent dispatched for Dismiss (source=TabKey)
    let (gained_ev, gained_ns) = t.gained.expect("FocusGainedEvent must be dispatched");
    assert_eq!(
        gained_ev.source,
        FocusSource::TabKey,
        "FocusGainedEvent source must be TabKey"
    );
    assert_eq!(
        gained_ev.tile_id, tile_id,
        "FocusGainedEvent tile_id must be the dashboard tile"
    );
    assert_eq!(
        gained_ev.node_id,
        Some(dismiss_id),
        "FocusGainedEvent node_id must be Dismiss"
    );
    assert_eq!(
        gained_ns, "dashboard-agent",
        "FocusGainedEvent namespace must be 'dashboard-agent'"
    );
}

/// Spec §Requirement: Focus Cycling Between Buttons, Scenario: Tab wraps Dismiss → Refresh
///
/// WHEN the "Dismiss" HitRegionNode has focus and the user presses Tab
/// THEN focus SHALL wrap to the "Refresh" HitRegionNode
/// (in a single-tile scene with no other tiles).
#[test]
fn tab_wraps_focus_from_dismiss_back_to_refresh() {
    let (scene, tab_id, tile_id, refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut fm = FocusManager::new();
    fm.add_tab(tab_id);

    // Start at Refresh, Tab to Dismiss
    fm.on_click(tab_id, tile_id, Some(refresh_id), &scene);
    fm.navigate_next(tab_id, &scene);

    let owner = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        owner,
        FocusOwner::Node {
            tile_id,
            node_id: dismiss_id
        },
        "focus should be on Dismiss before wrap"
    );

    // Tab again — should wrap back to Refresh (only tile in scene)
    let t = fm.navigate_next(tab_id, &scene);

    let new_owner = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        new_owner,
        FocusOwner::Node {
            tile_id,
            node_id: refresh_id
        },
        "Tab from Dismiss must wrap back to Refresh"
    );

    // Must dispatch focus events for the transition
    assert!(
        t.lost.is_some(),
        "FocusLostEvent must be dispatched on wrap"
    );
    assert!(
        t.gained.is_some(),
        "FocusGainedEvent must be dispatched on wrap"
    );

    let (lost_ev, _) = t.lost.unwrap();
    assert_eq!(lost_ev.node_id, Some(dismiss_id));
    let (gained_ev, _) = t.gained.unwrap();
    assert_eq!(gained_ev.node_id, Some(refresh_id));
}

/// Spec §Requirement: Focus Cycling Between Buttons, Scenario: Shift+Tab reverses cycling
///
/// Shift+Tab cycles Dismiss → Refresh (NAVIGATE_PREV).
#[test]
fn shift_tab_cycles_focus_dismiss_to_refresh() {
    let (scene, tab_id, tile_id, refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut fm = FocusManager::new();
    fm.add_tab(tab_id);

    // Start on Dismiss
    fm.on_click(tab_id, tile_id, Some(dismiss_id), &scene);
    let owner = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        owner,
        FocusOwner::Node {
            tile_id,
            node_id: dismiss_id
        }
    );

    // Shift+Tab (NAVIGATE_PREV) → Refresh
    let t = fm.navigate_prev(tab_id, &scene);

    let new_owner = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        new_owner,
        FocusOwner::Node {
            tile_id,
            node_id: refresh_id
        },
        "Shift+Tab from Dismiss must go back to Refresh"
    );

    let (lost_ev, _) = t.lost.expect("FocusLostEvent must be dispatched");
    assert_eq!(
        lost_ev.reason,
        FocusLostReason::TabKey,
        "FocusLostEvent reason must be TabKey for Shift+Tab"
    );
    assert_eq!(lost_ev.node_id, Some(dismiss_id));

    let (gained_ev, _) = t.gained.expect("FocusGainedEvent must be dispatched");
    assert_eq!(
        gained_ev.source,
        FocusSource::TabKey,
        "FocusGainedEvent source must be TabKey for Shift+Tab"
    );
    assert_eq!(gained_ev.node_id, Some(refresh_id));
}

/// Spec §Requirement: Focus Cycling Between Buttons, Scenario: Shift+Tab wraps Refresh → Dismiss
#[test]
fn shift_tab_wraps_refresh_to_dismiss() {
    let (scene, tab_id, tile_id, refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut fm = FocusManager::new();
    fm.add_tab(tab_id);

    // Start on Refresh
    fm.on_click(tab_id, tile_id, Some(refresh_id), &scene);

    // Shift+Tab — wraps to Dismiss (last in cycle)
    let t = fm.navigate_prev(tab_id, &scene);

    let new_owner = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        new_owner,
        FocusOwner::Node {
            tile_id,
            node_id: dismiss_id
        },
        "Shift+Tab from Refresh must wrap to Dismiss"
    );

    assert!(t.gained.is_some());
    let (gained_ev, _) = t.gained.unwrap();
    assert_eq!(gained_ev.node_id, Some(dismiss_id));
}

/// Spec §Requirement: Focus Ring Visual Indication
///
/// WHEN focus transfers to the "Refresh" HitRegionNode via Tab key or click
/// THEN the runtime SHALL produce a FocusRingUpdate with bounds at the node's
/// display-space position (tile origin + node local bounds).
#[test]
fn focus_ring_bounds_computed_in_display_space_for_refresh() {
    let (scene, tab_id, tile_id, refresh_id, _dismiss_id) = setup_dashboard_tile_scene();
    let mut fm = FocusManager::new();
    fm.add_tab(tab_id);

    let t = fm.on_click(tab_id, tile_id, Some(refresh_id), &scene);

    let ring = t
        .ring_update
        .expect("FocusRingUpdate must be produced on click");
    let bounds = ring
        .bounds
        .expect("ring bounds must be set for HitRegionNode focus");

    // Display-space: tile_origin + node_local_bounds
    let expected_x = TILE_X + REFRESH_LOCAL_X;
    let expected_y = TILE_Y + REFRESH_LOCAL_Y;

    assert!(
        (bounds.x - expected_x).abs() < 0.1,
        "focus ring x must be tile_x + node_local_x: expected {expected_x}, got {}",
        bounds.x
    );
    assert!(
        (bounds.y - expected_y).abs() < 0.1,
        "focus ring y must be tile_y + node_local_y: expected {expected_y}, got {}",
        bounds.y
    );
    assert!(
        (bounds.width - REFRESH_W).abs() < 0.1,
        "focus ring width must match node width"
    );
    assert!(
        (bounds.height - REFRESH_H).abs() < 0.1,
        "focus ring height must match node height"
    );
}

/// Spec §Requirement: Focus Ring Visual Indication — Tab also produces ring update
#[test]
fn focus_ring_produced_on_tab_navigation_to_dismiss() {
    let (scene, tab_id, tile_id, refresh_id, _dismiss_id) = setup_dashboard_tile_scene();
    let mut fm = FocusManager::new();
    fm.add_tab(tab_id);

    fm.on_click(tab_id, tile_id, Some(refresh_id), &scene);

    let t = fm.navigate_next(tab_id, &scene);
    let ring = t
        .ring_update
        .expect("FocusRingUpdate must be produced on Tab");
    let bounds = ring
        .bounds
        .expect("ring bounds must be set after Tab to Dismiss");

    let expected_x = TILE_X + DISMISS_LOCAL_X;
    let expected_y = TILE_Y + DISMISS_LOCAL_Y;

    assert!(
        (bounds.x - expected_x).abs() < 0.1,
        "Dismiss focus ring x must be {expected_x}, got {}",
        bounds.x
    );
    assert!(
        (bounds.y - expected_y).abs() < 0.1,
        "Dismiss focus ring y must be {expected_y}, got {}",
        bounds.y
    );
}

/// Spec §Requirement: Focus Cycling Between Buttons, Scenario: Pointer-free activation
///
/// BOTH HitRegionNodes must be reachable and activatable without pointer
/// (NAVIGATE_NEXT + ACTIVATE dispatches same callback as click).
///
/// This test verifies:
/// 1. NAVIGATE_NEXT from None lands on Refresh (first focusable in z-order).
/// 2. NAVIGATE_NEXT again lands on Dismiss.
/// 3. ACTIVATE on a focused node sets pressed=true and returns a CommandDispatch
///    with the correct interaction_id.
#[test]
fn both_buttons_reachable_and_activatable_via_navigate_next_and_activate() {
    let (mut scene, tab_id, tile_id, refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut fm = FocusManager::new();
    fm.add_tab(tab_id);
    let proc = CommandProcessor::new();

    // NAVIGATE_NEXT from None → Refresh (first focusable node, lower child index)
    let t1 = fm.navigate_next(tab_id, &scene);
    let owner = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        owner,
        FocusOwner::Node {
            tile_id,
            node_id: refresh_id
        },
        "NAVIGATE_NEXT from None must land on Refresh (first focusable)"
    );
    assert!(
        t1.gained.is_some(),
        "FocusGainedEvent must be dispatched for Refresh"
    );
    let (gained_ev, _) = t1.gained.unwrap();
    assert_eq!(gained_ev.node_id, Some(refresh_id));

    // ACTIVATE on Refresh — sets pressed=true and produces CommandDispatch
    let activate_cmd = raw_command(CommandAction::Activate);
    // Extract namespace before the mutable borrow in CommandProcessor::process
    let ns_for_tile_id = scene.tiles.get(&tile_id).map(|t| t.namespace.clone());
    let dispatch = proc
        .process(&activate_cmd, &owner, &mut scene, |_tid| {
            ns_for_tile_id.clone()
        })
        .expect("ACTIVATE on focused Refresh must produce CommandDispatch");

    assert_eq!(
        dispatch.event.interaction_id, "refresh-button",
        "CommandInputEvent must carry interaction_id 'refresh-button'"
    );
    assert_eq!(
        dispatch.event.tile_id, tile_id,
        "CommandInputEvent tile_id must be the dashboard tile"
    );
    assert_eq!(
        dispatch.event.node_id,
        Some(refresh_id),
        "CommandInputEvent node_id must be Refresh"
    );
    assert!(
        dispatch.activate_pressed_state,
        "ACTIVATE must set activate_pressed_state=true"
    );
    assert!(
        dispatch.is_transactional,
        "CommandInputEvent must be transactional (never dropped)"
    );

    // Pressed state must be set on the node
    assert!(
        scene.hit_region_states[&refresh_id].pressed,
        "ACTIVATE must set pressed=true on Refresh via local feedback"
    );

    // NAVIGATE_NEXT again → Dismiss
    let _t2 = fm.navigate_next(tab_id, &scene);
    let owner2 = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        owner2,
        FocusOwner::Node {
            tile_id,
            node_id: dismiss_id
        },
        "Second NAVIGATE_NEXT must move to Dismiss"
    );

    // ACTIVATE on Dismiss
    let dispatch2 = proc
        .process(&activate_cmd, &owner2, &mut scene, |_tid| {
            ns_for_tile_id.clone()
        })
        .expect("ACTIVATE on focused Dismiss must produce CommandDispatch");

    assert_eq!(
        dispatch2.event.interaction_id, "dismiss-button",
        "CommandInputEvent must carry interaction_id 'dismiss-button'"
    );
    assert!(
        scene.hit_region_states[&dismiss_id].pressed,
        "ACTIVATE must set pressed=true on Dismiss via local feedback"
    );
}

/// Spec §Requirement: Focus Cycling Between Buttons, Scenario: Focus events dispatched
///
/// FocusGainedEvent and FocusLostEvent must be dispatched on every focus transition,
/// including click-to-focus.
#[test]
fn focus_gained_and_lost_events_dispatched_on_click_to_focus() {
    let (scene, tab_id, tile_id, refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut fm = FocusManager::new();
    fm.add_tab(tab_id);

    // Click on Refresh — should emit FocusGainedEvent(source=Click) for Refresh
    let t1 = fm.on_click(tab_id, tile_id, Some(refresh_id), &scene);
    assert!(t1.lost.is_none(), "no FocusLostEvent when no prior focus");
    let (gained_ev, gained_ns) = t1.gained.expect("FocusGainedEvent required");
    assert_eq!(gained_ev.source, FocusSource::Click);
    assert_eq!(gained_ev.node_id, Some(refresh_id));
    assert_eq!(gained_ns, "dashboard-agent");

    // Click on Dismiss — should emit FocusLostEvent for Refresh + FocusGainedEvent for Dismiss
    let t2 = fm.on_click(tab_id, tile_id, Some(dismiss_id), &scene);

    let (lost_ev, lost_ns) = t2.lost.expect("FocusLostEvent required on focus transfer");
    assert_eq!(
        lost_ev.node_id,
        Some(refresh_id),
        "lost node must be Refresh"
    );
    assert_eq!(
        lost_ev.reason,
        FocusLostReason::ClickElsewhere,
        "reason must be ClickElsewhere"
    );
    assert_eq!(lost_ns, "dashboard-agent");

    let (gained_ev2, gained_ns2) = t2.gained.expect("FocusGainedEvent required for Dismiss");
    assert_eq!(gained_ev2.source, FocusSource::Click);
    assert_eq!(gained_ev2.node_id, Some(dismiss_id));
    assert_eq!(gained_ns2, "dashboard-agent");
}

/// Cross-tile focus test: Tab from dashboard tile moves focus to first focusable
/// node in the next tile.
#[test]
fn tab_from_dismiss_crosses_to_next_tile() {
    let (mut scene, tab_id, tile_id, refresh_id, dismiss_id) = setup_dashboard_tile_scene();

    // Add a second tile with one focusable node (higher z-order)
    let lease_id2 = scene.grant_lease(
        "other-agent",
        60_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    let tile_id2 = scene
        .create_tile(
            tab_id,
            "other-agent",
            lease_id2,
            Rect::new(500.0, 50.0, 200.0, 100.0),
            200, // higher z-order than dashboard tile (100)
        )
        .unwrap();
    let other_node_id = SceneId::new();
    scene
        .set_tile_root(
            tile_id2,
            Node {
                layout: Default::default(),
                id: other_node_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                    interaction_id: "other-button".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        )
        .unwrap();

    let mut fm = FocusManager::new();
    fm.add_tab(tab_id);

    // Focus Refresh, tab to Dismiss
    fm.on_click(tab_id, tile_id, Some(refresh_id), &scene);
    fm.navigate_next(tab_id, &scene);

    // Dismiss is focused; Tab again moves to the other tile's node
    let t = fm.navigate_next(tab_id, &scene);

    let new_owner = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        new_owner,
        FocusOwner::Node {
            tile_id: tile_id2,
            node_id: other_node_id
        },
        "Tab from Dismiss must cross to next tile's first focusable node"
    );

    // Focus events must be dispatched
    let (lost_ev, _) = t
        .lost
        .expect("FocusLostEvent must be dispatched on cross-tile Tab");
    assert_eq!(lost_ev.node_id, Some(dismiss_id));
    assert_eq!(lost_ev.reason, FocusLostReason::TabKey);

    let (gained_ev, gained_ns) = t.gained.expect("FocusGainedEvent for next tile");
    assert_eq!(gained_ev.node_id, Some(other_node_id));
    assert_eq!(gained_ns, "other-agent");
}

/// Spec §Requirement: Focus Cycling Between Buttons, Scenario: Pointer-free activation via keyboard
///
/// Validates that both buttons can be activated via keyboard (NAVIGATE_NEXT + ACTIVATE)
/// without using a pointer, and that the interaction_ids are correct.
#[test]
fn keyboard_only_activation_produces_correct_interaction_ids_for_both_buttons() {
    let (mut scene, tab_id, tile_id, refresh_id, dismiss_id) = setup_dashboard_tile_scene();
    let mut fm = FocusManager::new();
    fm.add_tab(tab_id);
    let proc = CommandProcessor::new();

    let activate_cmd = raw_command(CommandAction::Activate);
    let namespace = scene.tiles.get(&tile_id).map(|t| t.namespace.clone());

    // Navigate to Refresh and activate
    fm.navigate_next(tab_id, &scene);
    let owner_refresh = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        owner_refresh,
        FocusOwner::Node {
            tile_id,
            node_id: refresh_id
        }
    );

    let dispatch_refresh = proc
        .process(&activate_cmd, &owner_refresh, &mut scene, |_tid| {
            namespace.clone()
        })
        .unwrap();
    assert_eq!(dispatch_refresh.event.interaction_id, "refresh-button");
    assert_eq!(dispatch_refresh.event.source, CommandSource::Keyboard);

    // Navigate to Dismiss and activate
    fm.navigate_next(tab_id, &scene);
    let owner_dismiss = fm.trees()[&tab_id].current().clone();
    assert_eq!(
        owner_dismiss,
        FocusOwner::Node {
            tile_id,
            node_id: dismiss_id
        }
    );

    let dispatch_dismiss = proc
        .process(&activate_cmd, &owner_dismiss, &mut scene, |_tid| {
            namespace.clone()
        })
        .unwrap();
    assert_eq!(dispatch_dismiss.event.interaction_id, "dismiss-button");
    assert_eq!(dispatch_dismiss.event.source, CommandSource::Keyboard);
}
