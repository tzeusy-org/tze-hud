//! Long-press drag interaction for chrome drag handles.
//!
//! Implements the compositor-internal drag state machine described in RFC 0004
//! §3.0 (V1 chrome interaction carve-out) and the drag-to-reposition spec in
//! `openspec/changes/persistent-movable-elements/specs/drag-to-reposition/`.
//!
//! ## State machine (per device_id)
//!
//! ```text
//! Idle ──[PointerDown on drag handle]──► Accumulating
//!                                             │
//!              [move > 10dp threshold]──► Cancelled → Idle
//!                                             │
//!              [hold ≥ threshold ms]──► Activated
//!                                             │
//!              [PointerMove]──► move element (snap + clamp)
//!                                             │
//!              [PointerUp]──► persist geometry → Idle
//! ```
//!
//! ## Activation thresholds (per RFC 0004 §3.0 + design.md §2)
//!
//! - Pointer/mouse: 250 ms
//! - Touch (device_id ≥ TOUCH_DEVICE_ID_START): 1000 ms
//!
//! ## Performance contract
//!
//! Each state update runs in O(1) — no allocations on the hot path after
//! initial state construction.
//!
//! Per the engineering bar: gesture recognizer update < 50 µs per event.

use std::time::{Duration, Instant};

use tze_hud_scene::{DragHandleElementKind, ElementStore, ElementType, GeometryPolicy, SceneId};

/// Activation hold duration for pointer/mouse drag handles.
pub const LONG_PRESS_POINTER_THRESHOLD_MS: u64 = 250;
/// Activation hold duration for touch drag handles.
pub const LONG_PRESS_TOUCH_THRESHOLD_MS: u64 = 1000;

/// Movement tolerance before long-press is cancelled (density-independent pixels).
///
/// Per RFC 0004 §3.4 and issue scope: 10dp movement tolerance.
pub const LONG_PRESS_MOVEMENT_TOLERANCE_DP: f32 = 10.0;

/// Default snap grid as a fraction of the display dimension (2% = 0.02).
pub const DEFAULT_SNAP_GRID_PCT: f32 = 0.02;

/// Z-order boost applied to the dragged element during active drag.
///
/// Added on top of the element's current z-order during the `Activated` phase.
pub const DRAG_Z_ORDER_BOOST: u32 = 0x1000;

/// Opacity multiplier applied to the dragged element during active drag (1.0 = no change).
///
/// Must be ≥ 1.0; the compositor clamps the result to 1.0.
pub const DRAG_OPACITY_BOOST: f32 = 1.0;

/// Width of the 2px highlight border applied during active drag.
pub const DRAG_HIGHLIGHT_BORDER_PX: f32 = 2.0;

/// Phase of a per-device long-press drag recognizer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragPhase {
    /// No drag interaction in progress for this device.
    Idle,
    /// Pointer is held down; accumulating towards long-press threshold.
    Accumulating,
    /// Long-press threshold met; drag is active.
    Activated,
}

/// Configuration for drag behaviour (snap grid, etc.).
///
/// The `snap_grid_pct` is expressed as a fraction of the display dimension
/// (e.g., 0.02 = 2% → positions snap to 2% increments of the display width
/// and height respectively). Default is [`DEFAULT_SNAP_GRID_PCT`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragConfig {
    /// Snap grid granularity as a fraction of the display dimension.
    pub snap_grid_pct: f32,
}

impl Default for DragConfig {
    fn default() -> Self {
        Self {
            snap_grid_pct: DEFAULT_SNAP_GRID_PCT,
        }
    }
}

/// Per-device long-press drag state.
///
/// One instance exists per active pointer device that has touched a drag handle.
/// Instances are created on `PointerDown` and removed when the interaction ends.
#[derive(Clone, Debug)]
pub struct DeviceDragState {
    /// Current phase of this interaction.
    pub phase: DragPhase,
    /// Drag-handle interaction_id that initiated this interaction.
    pub interaction_id: String,
    /// Element targeted by the drag handle.
    pub element_id: SceneId,
    /// Element class (Tile / Zone / Widget).
    pub element_kind: DragHandleElementKind,
    /// Instant the pointer pressed down.
    pub press_start: Instant,
    /// Display-space X where the pointer pressed down.
    pub press_x: f32,
    /// Display-space Y where the pointer pressed down.
    pub press_y: f32,
    /// X offset from the element's top-left to the grab point (display pixels).
    ///
    /// Set when the drag activates so the element tracks the pointer at a
    /// stable position rather than jumping to a centre-relative offset.
    pub grab_offset_x: f32,
    /// Y offset from the element's top-left to the grab point (display pixels).
    pub grab_offset_y: f32,
    /// Hold-duration threshold in milliseconds (device-class dependent).
    pub threshold_ms: u64,
    /// Long-press progress last emitted (0.0–1.0).
    pub last_progress: f32,
    /// Immediate-activation handle (hud-cpjqe): a portal header BAND drags on the
    /// first pointer-move with no long-press hold and no early-movement cancel —
    /// Windows-titlebar semantics. `false` for the legacy grip, which keeps the
    /// long-press hysteresis to distinguish tap-to-focus from drag.
    pub immediate: bool,
}

impl DeviceDragState {
    /// Construct a new state for a pointer-down event.
    ///
    /// `threshold_ms` is 250 for pointer/mouse or 1000 for touch. `immediate`
    /// marks a header-band handle that activates on the first move (hud-cpjqe).
    pub fn new(
        interaction_id: String,
        element_id: SceneId,
        element_kind: DragHandleElementKind,
        press_x: f32,
        press_y: f32,
        threshold_ms: u64,
        immediate: bool,
    ) -> Self {
        Self::new_at(
            interaction_id,
            element_id,
            element_kind,
            press_x,
            press_y,
            threshold_ms,
            immediate,
            Instant::now(),
        )
    }

    /// Construct pointer-down state at the supplied monotonic timestamp.
    // The existing pointer-down values plus its timestamp describe one state;
    // a separate parameter object would only hide this single construction.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_at(
        interaction_id: String,
        element_id: SceneId,
        element_kind: DragHandleElementKind,
        press_x: f32,
        press_y: f32,
        threshold_ms: u64,
        immediate: bool,
        now: Instant,
    ) -> Self {
        Self {
            phase: DragPhase::Accumulating,
            interaction_id,
            element_id,
            element_kind,
            press_start: now,
            press_x,
            press_y,
            grab_offset_x: 0.0,
            grab_offset_y: 0.0,
            threshold_ms,
            last_progress: 0.0,
            immediate,
        }
    }

    /// Compute progress toward activation (0.0–1.0).
    pub fn progress(&self) -> f32 {
        self.progress_at(Instant::now())
    }

    /// Compute progress using the same timestamp as the activation check.
    pub(crate) fn progress_at(&self, now: Instant) -> f32 {
        let elapsed_ms = now.saturating_duration_since(self.press_start).as_millis() as f32;
        let threshold = self.threshold_ms as f32;
        (elapsed_ms / threshold).clamp(0.0, 1.0)
    }

    /// Whether the activation threshold has been met.
    pub fn is_threshold_met(&self) -> bool {
        self.is_threshold_met_at(Instant::now())
    }

    /// Whether the hold has reached its deadline at the supplied timestamp.
    pub(crate) fn is_threshold_met_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.press_start) >= Duration::from_millis(self.threshold_ms)
    }

    /// Whether the pointer has moved beyond the cancellation tolerance.
    ///
    /// Returns `true` if the Euclidean distance from `press_x/y` to `(x, y)`
    /// exceeds [`LONG_PRESS_MOVEMENT_TOLERANCE_DP`].
    pub fn has_exceeded_movement_tolerance(&self, x: f32, y: f32) -> bool {
        let dx = x - self.press_x;
        let dy = y - self.press_y;
        (dx * dx + dy * dy).sqrt() > LONG_PRESS_MOVEMENT_TOLERANCE_DP
    }
}

/// Outcome of processing a pointer event through the drag recognizer.
#[derive(Clone, Debug, PartialEq)]
pub enum DragEventOutcome {
    /// No drag-related action needed.
    Idle,
    /// Long-press accumulating; progress from 0.0 to 1.0.
    Accumulating { progress: f32 },
    /// Long-press cancelled (pointer moved beyond tolerance).
    Cancelled,
    /// Drag activated — runtime MUST move the element to follow the pointer.
    Activated {
        element_id: SceneId,
        element_kind: DragHandleElementKind,
    },
    /// Element moved to a new display-space position (snapped + clamped).
    Moved {
        element_id: SceneId,
        element_kind: DragHandleElementKind,
        /// New element top-left X in display pixels.
        new_x: f32,
        /// New element top-left Y in display pixels.
        new_y: f32,
    },
    /// Drag released; caller MUST persist the final geometry.
    Released {
        element_id: SceneId,
        element_kind: DragHandleElementKind,
        /// Final element top-left X in display pixels (snapped + clamped).
        final_x: f32,
        /// Final element top-left Y in display pixels (snapped + clamped).
        final_y: f32,
    },
}

/// Quantise a position to the snap grid.
///
/// `pos_px` is the absolute pixel coordinate.
/// `display_dim` is the full display width or height in pixels.
/// `snap_pct` is the grid cell size as a fraction of `display_dim` (e.g. 0.02).
pub fn snap_to_grid(pos_px: f32, display_dim: f32, snap_pct: f32) -> f32 {
    if display_dim <= 0.0 || snap_pct <= 0.0 {
        return pos_px;
    }
    let cell_px = display_dim * snap_pct;
    (pos_px / cell_px).round() * cell_px
}

/// Clamp element bounds so the element stays fully within the display.
///
/// Returns the clamped `(x, y)` top-left while preserving `width` and `height`.
pub fn clamp_to_display(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    display_width: f32,
    display_height: f32,
) -> (f32, f32) {
    let clamped_x = x.clamp(0.0, (display_width - width).max(0.0));
    let clamped_y = y.clamp(0.0, (display_height - height).max(0.0));
    (clamped_x, clamped_y)
}

/// Apply snap-grid quantisation and boundary clamping to a new element position.
///
/// Returns the final `(x, y)` that should be used for the element.
pub fn quantise_and_clamp(
    raw_x: f32,
    raw_y: f32,
    width: f32,
    height: f32,
    display_width: f32,
    display_height: f32,
    snap_grid_pct: f32,
) -> (f32, f32) {
    let snapped_x = snap_to_grid(raw_x, display_width, snap_grid_pct);
    let snapped_y = snap_to_grid(raw_y, display_height, snap_grid_pct);
    clamp_to_display(
        snapped_x,
        snapped_y,
        width,
        height,
        display_width,
        display_height,
    )
}

/// Convert final pixel position to a `GeometryPolicy::Relative` override.
pub fn final_position_to_geometry(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    display_width: f32,
    display_height: f32,
) -> GeometryPolicy {
    if display_width <= 0.0 || display_height <= 0.0 {
        return GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 1.0,
        };
    }
    GeometryPolicy::Relative {
        x_pct: x / display_width,
        y_pct: y / display_height,
        width_pct: width / display_width,
        height_pct: height / display_height,
    }
}

/// Persist the final geometry of a dragged element into the element store.
///
/// Finds the entry matching `(element_type, interaction_key)` in the store and
/// writes `geometry_override`. If no matching entry exists, this is a no-op
/// (the element has not been registered in the store yet).
pub fn persist_geometry_override(
    store: &mut ElementStore,
    element_type: ElementType,
    interaction_key: &str,
    geometry: GeometryPolicy,
) {
    for entry in store.entries.values_mut() {
        if entry.element_type == element_type && entry.namespace == interaction_key {
            entry.geometry_override = Some(geometry);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::ElementStoreEntry;

    // ── snap_to_grid ──────────────────────────────────────────────────────────

    #[test]
    fn snap_to_grid_quantises_to_nearest_cell() {
        // 2% of 1920 = 38.4px cell → pos=57 snaps to 38.4, pos=60 snaps to 76.8
        let snap = 0.02_f32;
        let dim = 1920.0_f32;
        let cell = dim * snap; // 38.4

        // position at 1.4× cell → rounds to 1
        let p = snap_to_grid(cell * 1.4, dim, snap);
        assert!((p - cell).abs() < 0.1, "expected ~{cell}, got {p}");
        // position at 1.6× cell → rounds to 2
        let p2 = snap_to_grid(cell * 1.6, dim, snap);
        assert!(
            (p2 - 2.0 * cell).abs() < 0.1,
            "expected ~{}, got {p2}",
            2.0 * cell
        );
    }

    #[test]
    fn snap_to_grid_zero_cell_passthrough() {
        let p = snap_to_grid(100.0, 1920.0, 0.0);
        assert_eq!(p, 100.0, "zero snap_pct must return position unchanged");
    }

    #[test]
    fn snap_to_grid_zero_display_passthrough() {
        let p = snap_to_grid(100.0, 0.0, 0.02);
        assert_eq!(p, 100.0, "zero display dim must return position unchanged");
    }

    // ── clamp_to_display ─────────────────────────────────────────────────────

    #[test]
    fn clamp_to_display_within_bounds_unchanged() {
        let (cx, cy) = clamp_to_display(100.0, 200.0, 300.0, 150.0, 1920.0, 1080.0);
        assert_eq!(cx, 100.0);
        assert_eq!(cy, 200.0);
    }

    #[test]
    fn clamp_to_display_left_edge() {
        let (cx, cy) = clamp_to_display(-50.0, 100.0, 200.0, 100.0, 1920.0, 1080.0);
        assert_eq!(cx, 0.0, "left edge must clamp to 0");
        assert_eq!(cy, 100.0);
    }

    #[test]
    fn clamp_to_display_right_edge() {
        let (cx, _) = clamp_to_display(1800.0, 100.0, 200.0, 100.0, 1920.0, 1080.0);
        assert_eq!(
            cx,
            1920.0 - 200.0,
            "right edge: x must clamp so element fits"
        );
    }

    #[test]
    fn clamp_to_display_top_edge() {
        let (_, cy) = clamp_to_display(100.0, -10.0, 200.0, 100.0, 1920.0, 1080.0);
        assert_eq!(cy, 0.0, "top edge must clamp to 0");
    }

    #[test]
    fn clamp_to_display_bottom_edge() {
        let (_, cy) = clamp_to_display(100.0, 1050.0, 200.0, 100.0, 1920.0, 1080.0);
        assert_eq!(
            cy,
            1080.0 - 100.0,
            "bottom edge: y must clamp so element fits"
        );
    }

    // ── long press state machine ──────────────────────────────────────────────

    #[test]
    fn device_drag_state_initial_phase_is_accumulating() {
        let state = DeviceDragState::new(
            "drag-handle:aabb".to_string(),
            SceneId::new(),
            DragHandleElementKind::Tile,
            100.0,
            200.0,
            250,
            false,
        );
        assert_eq!(state.phase, DragPhase::Accumulating);
        assert!(state.last_progress == 0.0);
    }

    #[test]
    fn device_drag_state_threshold_not_met_at_start() {
        let state = DeviceDragState::new(
            "drag-handle:aabb".to_string(),
            SceneId::new(),
            DragHandleElementKind::Tile,
            100.0,
            200.0,
            250,
            false,
        );
        // Brand-new state — threshold not met
        assert!(!state.is_threshold_met());
    }

    #[test]
    fn device_drag_state_threshold_met_after_duration() {
        let start = Instant::now();
        for threshold_ms in [
            LONG_PRESS_POINTER_THRESHOLD_MS,
            LONG_PRESS_TOUCH_THRESHOLD_MS,
        ] {
            let mut state = DeviceDragState::new_at(
                "drag-handle:aabb".to_string(),
                SceneId::new(),
                DragHandleElementKind::Tile,
                100.0,
                200.0,
                threshold_ms,
                false,
                start,
            );
            assert_eq!(state.press_start, start);
            assert!(!state.is_threshold_met_at(start));
            assert_eq!(state.progress_at(start), 0.0);
            assert_eq!(state.progress_at(start), 0.0);

            let before = start + Duration::from_millis(threshold_ms - 1);
            assert!(!state.is_threshold_met_at(before));
            let progress = state.progress_at(before);
            assert_eq!(progress, (threshold_ms - 1) as f32 / threshold_ms as f32);
            assert_eq!(state.progress_at(before), progress);

            let deadline = start + Duration::from_millis(threshold_ms);
            assert!(state.is_threshold_met_at(deadline));
            assert_eq!(state.progress_at(deadline), 1.0);
            let after = deadline + Duration::from_millis(1);
            assert!(state.is_threshold_met_at(after));
            assert_eq!(state.progress_at(after), 1.0);
            state.phase = DragPhase::Activated;
            assert_eq!(state.phase, DragPhase::Activated);
        }
    }

    #[test]
    fn movement_within_tolerance_does_not_exceed() {
        let state = DeviceDragState::new(
            "drag-handle:aabb".to_string(),
            SceneId::new(),
            DragHandleElementKind::Tile,
            100.0,
            200.0,
            250,
            false,
        );
        // 5dp movement — below 10dp threshold
        assert!(
            !state.has_exceeded_movement_tolerance(105.0, 200.0),
            "5dp movement must not exceed tolerance"
        );
    }

    #[test]
    fn movement_beyond_tolerance_exceeds() {
        let state = DeviceDragState::new(
            "drag-handle:aabb".to_string(),
            SceneId::new(),
            DragHandleElementKind::Tile,
            100.0,
            200.0,
            250,
            false,
        );
        // 15dp movement — above 10dp threshold
        assert!(
            state.has_exceeded_movement_tolerance(115.0, 200.0),
            "15dp movement must exceed tolerance"
        );
    }

    // ── final_position_to_geometry ────────────────────────────────────────────

    #[test]
    fn final_position_roundtrips_via_geometry() {
        let (x, y, w, h) = (192.0_f32, 108.0_f32, 384.0_f32, 216.0_f32);
        let (dw, dh) = (1920.0_f32, 1080.0_f32);
        let policy = final_position_to_geometry(x, y, w, h, dw, dh);
        if let GeometryPolicy::Relative {
            x_pct,
            y_pct,
            width_pct,
            height_pct,
        } = policy
        {
            assert!((x_pct - x / dw).abs() < 1e-5);
            assert!((y_pct - y / dh).abs() < 1e-5);
            assert!((width_pct - w / dw).abs() < 1e-5);
            assert!((height_pct - h / dh).abs() < 1e-5);
        } else {
            panic!("expected GeometryPolicy::Relative");
        }
    }

    // ── persist_geometry_override ────────────────────────────────────────────

    #[test]
    fn persist_geometry_override_updates_matching_entry() {
        let mut store = ElementStore::default();
        let id = SceneId::new();
        store.entries.insert(
            id,
            ElementStoreEntry {
                element_type: ElementType::Tile,
                namespace: "my-tile".to_string(),
                created_at: 0,
                last_published_at: 0,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: None,
            },
        );

        let policy = GeometryPolicy::Relative {
            x_pct: 0.1,
            y_pct: 0.2,
            width_pct: 0.3,
            height_pct: 0.4,
        };
        persist_geometry_override(&mut store, ElementType::Tile, "my-tile", policy);

        let entry = store.entries.get(&id).unwrap();
        assert_eq!(entry.geometry_override, Some(policy));
    }

    #[test]
    fn persist_geometry_override_noop_when_no_match() {
        let mut store = ElementStore::default();
        // Store is empty — no-op expected
        persist_geometry_override(
            &mut store,
            ElementType::Tile,
            "nonexistent",
            GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 1.0,
                height_pct: 1.0,
            },
        );
        assert!(store.entries.is_empty());
    }
}
