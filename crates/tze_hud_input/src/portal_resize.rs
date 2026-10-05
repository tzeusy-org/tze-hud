//! Portal window management — resize affordances, focus-scoped hotkeys,
//! min/max clamping, and coalescible geometry snapshots.
//!
//! Implements §6b of `text-stream-portal-phase1/tasks.md` (amendment 2026-06-10):
//!
//! - §6b.1 Pointer-driven resize affordances on the portal frame (corner/edge
//!   capture regions, content layer) with local-first geometry feedback.
//! - §6b.2 Focus-scoped resize hotkeys (Ctrl+`+`/`=` grow, Ctrl+`-` shrink)
//!   with token-defined step; unfocused portals never consume them.
//! - §6b.3 Min/max clamping: legibility minimum from tokens; lease-bounds /
//!   scene-budget maximum passed by caller; no partially-clipped glyphs at any
//!   intermediate geometry (the overflow contract is not relaxed during resize).
//! - §6b.4 Coalescible state-stream geometry snapshots delivered to the
//!   owning adapter; gesture is authoritative over adapter publishes until
//!   gesture end.
//!
//! ## Invariants
//!
//! - Gesture authority: during an active resize gesture the adapter's
//!   `publish_geometry` requests MUST be dropped. Only `GeometrySnapshot`
//!   events emitted by this module are applied. This is enforced through two
//!   complementary mechanisms:
//!   1. `PortalResizeState::gesture_active()` — advisory flag for callers.
//!   2. `PortalResizeState::accept_adapter_publish()` — **real enforcement
//!      point** using a `gesture_epoch` counter. Adapters obtain the current
//!      epoch before dispatching a geometry publish; the epoch is checked on
//!      arrival and the publish is rejected if the gesture epoch has advanced
//!      (gesture started or ended) since the epoch was sampled.
//! - Local feedback first: geometry updates happen immediately in the same
//!   frame as the pointer event — no adapter roundtrip.
//! - Token-defined bounds: `min_width_px`, `min_height_px`, and
//!   `resize_step_px` come exclusively from `PortalWindowTokens`, never from
//!   inline constants.
//! - Coalescible snapshots: `GeometrySnapshot` is a state-stream payload.
//!   The transport MAY deliver only the latest snapshot per adapter delivery
//!   window (latest-wins).
//!
//! ## Input precedence order (§6b.2 + §6b.6 authority scenarios)
//!
//! The following priority order is enforced in `dispatch_key_down_event` (in
//! `tze_hud_runtime::windowed`):
//!
//! 1. **Safe-mode capture** — when safe mode is active, ALL input is captured
//!    by the chrome layer. Portal resize hotkeys are NOT reached.
//! 2. **Shell/chrome-reserved shortcuts** — shortcuts in
//!    [`ShellReservedShortcut::is_reserved`] win over portal resize hotkeys.
//!    A reserved key is never consumed by a portal.
//! 3. **Portal resize hotkey** — `Ctrl++`/`Ctrl+=` (grow) and `Ctrl+-` (shrink)
//!    on the focused portal tile, including when focus is inside that portal's
//!    composer surface.
//! 4. **Composer draft routing** — non-resize composer keys are routed to the
//!    active composer before normal agent key forwarding.
//! 5. **Normal routing** — key forwarded to the owning agent.
//!
//! ## Performance contract
//!
//! Each state update runs in O(1) with no allocations on the hot path.
//! Per the engineering bar: gesture recognizer update < 50 µs per event;
//! input to local ack < 4 ms.

use serde::{Deserialize, Serialize};

// ─── Shell/chrome-reserved shortcut check (§6b.2) ────────────────────────────

/// Classifier for shell/chrome-reserved keyboard shortcuts.
///
/// Reserved shortcuts are owned by the chrome/shell layer and MUST win over
/// portal resize hotkeys.  A reserved key is **never** consumed by a portal —
/// it must either be dispatched to the chrome layer or suppressed, but portal
/// resize MUST NOT consume it.
///
/// The reserved set mirrors `ChromeShortcut` (in
/// `tze_hud_runtime::shell::chrome`) and the pairing shortcut
/// (`Ctrl+Shift+P`) that is handled at the OS-event stage.  It is
/// replicated here so that `tze_hud_input` can classify a key before any
/// portal resize attempt without depending on the runtime crate.
///
/// # Why replicate instead of calling into the runtime?
///
/// `tze_hud_input` is a dependency of `tze_hud_runtime`, not the other way
/// around.  Importing the runtime classification from input would create a
/// circular dependency.  The set is small and stable; keep the two in sync
/// when new chrome shortcuts are added.
pub struct ShellReservedShortcut;

impl ShellReservedShortcut {
    /// Returns `true` if the key+modifier combination is a shell/chrome-reserved
    /// shortcut that MUST win over portal resize hotkeys.
    ///
    /// # Arguments
    ///
    /// * `key` — DOM `KeyboardEvent.key` string (logical key, e.g. `"Tab"`, `"1"`).
    /// * `ctrl` — Ctrl modifier held.
    /// * `shift` — Shift modifier held.
    /// * `alt` — Alt modifier held (reserved shortcuts NEVER require Alt).
    ///
    /// # Reserved set
    ///
    /// | Shortcut | Notes |
    /// |----------|-------|
    /// | `Ctrl+Tab` | NextTab |
    /// | `Ctrl+Shift+Tab` | PrevTab |
    /// | `Ctrl+1` … `Ctrl+8` | GotoTab(1..8) |
    /// | `Ctrl+9` | LastTab |
    /// | `Ctrl+Shift+M` | MuteToggle (v1-reserved) |
    /// | `Ctrl+Shift+Escape` | SafeMode toggle |
    /// | `Ctrl+Shift+P` | Show a pairing code |
    pub fn is_reserved(key: &str, ctrl: bool, shift: bool, alt: bool) -> bool {
        // Reserved shortcuts never require Alt.
        if !ctrl || alt {
            return false;
        }
        match (key, shift) {
            // Tab navigation
            ("Tab", false) => true, // Ctrl+Tab → NextTab
            ("Tab", true) => true,  // Ctrl+Shift+Tab → PrevTab
            // Numbered tab jump (Ctrl+1..9)
            ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9", false) => true,
            // Mute toggle (v1-reserved noop, but still consumed)
            ("m" | "M", true) => true, // Ctrl+Shift+M
            // Safe mode toggle
            ("Escape", true) => true, // Ctrl+Shift+Escape
            // Pairing (also returned early at the OS stage, but model here so
            // the reserved set is complete for in-process callers)
            ("p" | "P", true) => true, // Ctrl+Shift+P
            _ => false,
        }
    }
}

// ─── Token-resolved window geometry bounds ────────────────────────────────────

/// Token-resolved window geometry bounds for a portal.
///
/// Constructed from the portal token map at startup (or on hot-reload).
/// All numeric fields are already parsed — callers use these values directly.
///
/// The **max** bounds come from the lease/scene budget at call time, not from
/// tokens, and are passed into clamping helpers as arguments.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PortalWindowTokens {
    /// Legibility minimum width in pixels (§6b.3).
    pub min_width_px: f32,
    /// Legibility minimum height in pixels (§6b.3).
    pub min_height_px: f32,
    /// Pixels per Ctrl+`+`/`=`/`-` hotkey step (§6b.2).
    pub resize_step_px: f32,
    /// Width/height of the pointer capture region on frame edges/corners (§6b.1).
    pub affordance_px: f32,
}

impl Default for PortalWindowTokens {
    fn default() -> Self {
        // These defaults must match `portal_tokens::defaults::WINDOW_*` in
        // tze_hud_config. There is no compile-time link (the crates are
        // intentionally independent); update both when changing a default.
        Self {
            min_width_px: 240.0,
            min_height_px: 160.0,
            resize_step_px: 32.0,
            affordance_px: 8.0,
        }
    }
}

// ─── Resize edge / corner ─────────────────────────────────────────────────────

/// Which edge or corner of the portal frame is being resized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResizeEdge {
    /// Left edge — dragging changes x and width (width ↑ when pointer moves left).
    Left,
    /// Right edge — dragging changes width only.
    Right,
    /// Top edge — dragging changes y and height.
    Top,
    /// Bottom edge — dragging changes height only.
    Bottom,
    /// Top-left corner.
    TopLeft,
    /// Top-right corner.
    TopRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom-right corner.
    BottomRight,
}

impl ResizeEdge {
    /// Whether this edge/corner affects the left boundary (and therefore x).
    pub fn affects_left(&self) -> bool {
        matches!(self, Self::Left | Self::TopLeft | Self::BottomLeft)
    }

    /// Whether this edge/corner affects the right boundary (and therefore width).
    pub fn affects_right(&self) -> bool {
        matches!(self, Self::Right | Self::TopRight | Self::BottomRight)
    }

    /// Whether this edge/corner affects the top boundary (and therefore y).
    pub fn affects_top(&self) -> bool {
        matches!(self, Self::Top | Self::TopLeft | Self::TopRight)
    }

    /// Whether this edge/corner affects the bottom boundary (and therefore height).
    pub fn affects_bottom(&self) -> bool {
        matches!(self, Self::Bottom | Self::BottomLeft | Self::BottomRight)
    }

    /// The OS resize cursor that communicates the drag axis of this edge/corner.
    ///
    /// Backend-agnostic ([`PortalCursor`] carries no winit dependency); the
    /// windowed runtime maps the result onto `winit::window::CursorIcon`. The
    /// mapping follows the standard bidirectional-arrow convention:
    /// - left/right edges → horizontal (`EwResize`),
    /// - top/bottom edges → vertical (`NsResize`),
    /// - top-left / bottom-right corners → `NwseResize` (`╲` diagonal),
    /// - top-right / bottom-left corners → `NeswResize` (`╱` diagonal).
    pub fn cursor(&self) -> PortalCursor {
        match self {
            Self::Left | Self::Right => PortalCursor::EwResize,
            Self::Top | Self::Bottom => PortalCursor::NsResize,
            Self::TopLeft | Self::BottomRight => PortalCursor::NwseResize,
            Self::TopRight | Self::BottomLeft => PortalCursor::NeswResize,
        }
    }
}

// ─── Portal cursor feedback ───────────────────────────────────────────────────

/// Backend-agnostic cursor shape for portal resize/move affordances.
///
/// Decouples the affordance hit-test (pure, here in `tze_hud_input`) from the
/// windowing backend: the windowed runtime maps each variant onto the
/// corresponding `winit::window::CursorIcon` before calling `set_cursor`.
/// `Default` is the ordinary arrow shown when the pointer is over no affordance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PortalCursor {
    /// Ordinary arrow — pointer is not over any portal affordance.
    #[default]
    Default,
    /// Horizontal double-arrow — left/right edge resize.
    EwResize,
    /// Vertical double-arrow — top/bottom edge resize.
    NsResize,
    /// `╱` diagonal double-arrow — top-right / bottom-left corner resize.
    NeswResize,
    /// `╲` diagonal double-arrow — top-left / bottom-right corner resize.
    NwseResize,
    /// Open hand — hovering a move/drag handle (header) before grabbing.
    Grab,
    /// Closed hand — an active drag-to-move gesture is in progress.
    Grabbing,
}

/// Resolve the cursor shape for the current pointer location over the focused
/// portal, applying gesture and hover precedence.
///
/// Precedence (highest first):
/// 1. **Active resize gesture** (`active_edge`): keep the edge's resize cursor
///    pinned for the whole drag, even if the pointer leaves the affordance band.
/// 2. **Active drag-to-move** (`drag_active`): show the closed-hand `Grabbing`.
/// 3. **Hover over a resize affordance** of the focused portal: the edge cursor.
/// 4. **Hover over a move/drag handle** (`over_drag_handle`): the open-hand `Grab`.
/// 5. Otherwise the ordinary arrow (`Default`).
///
/// Edge hover takes priority over the drag handle so the thin 8px resize band at
/// the top of a header still resizes (matching OS window behavior) rather than
/// being masked by the move affordance underneath it.
///
/// `focused_portal` is `None` when no portal is focused; in that case only the
/// active-gesture cases (which do not depend on a hovered rect) can apply.
pub fn portal_hover_cursor(
    x: f32,
    y: f32,
    focused_portal: Option<PortalRect>,
    affordance_px: f32,
    over_drag_handle: bool,
    active_edge: Option<ResizeEdge>,
    drag_active: bool,
) -> PortalCursor {
    if let Some(edge) = active_edge {
        return edge.cursor();
    }
    if drag_active {
        return PortalCursor::Grabbing;
    }
    if let Some(rect) = focused_portal {
        if let Some(edge) = hit_affordance(x, y, &rect, affordance_px) {
            return edge.cursor();
        }
    }
    if over_drag_handle {
        return PortalCursor::Grab;
    }
    PortalCursor::Default
}

/// Hysteresis gate for OS cursor-icon updates.
///
/// winit's `set_cursor` issues a platform call on every invocation, so the
/// runtime must avoid re-setting the same icon on every `PointerMove`. The
/// tracker remembers the last applied shape and yields a new icon only when it
/// actually changes (including the restore-to-`Default` transition when the
/// pointer leaves an affordance).
#[derive(Clone, Copy, Debug, Default)]
pub struct CursorIconTracker {
    last: PortalCursor,
}

impl CursorIconTracker {
    /// Construct a tracker whose last-applied shape is the ordinary arrow.
    pub fn new() -> Self {
        Self {
            last: PortalCursor::Default,
        }
    }

    /// The most recently applied cursor shape.
    pub fn current(&self) -> PortalCursor {
        self.last
    }

    /// Record `desired` as the new target shape.
    ///
    /// Returns `Some(desired)` (and updates internal state) only when it differs
    /// from the last applied shape — the caller should issue the OS cursor call
    /// exactly when this returns `Some`. Returns `None` when the shape is
    /// unchanged, so redundant platform calls are suppressed.
    pub fn update(&mut self, desired: PortalCursor) -> Option<PortalCursor> {
        if self.last == desired {
            return None;
        }
        self.last = desired;
        Some(desired)
    }
}

// ─── Portal geometry ──────────────────────────────────────────────────────────

/// Axis-aligned portal bounding rectangle in display pixels.
///
/// All fields are in display pixels from the top-left of the display.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortalRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PortalRect {
    /// Clamp the rect so width/height stay within `[min_*_px, max_*_px]` and
    /// the portal stays on-screen within `(display_w, display_h)`.
    ///
    /// When `affects_left`/`affects_top` would push the origin off-screen,
    /// the origin is clamped and the size is adjusted to keep the portal
    /// against the boundary.
    pub fn clamped(self, bounds: &ResizeBounds) -> Self {
        // Sanitize bounds: if the lease/scene-budget max is smaller than the
        // token-defined minimum (e.g. a pathological lease), clamp max up to
        // min so f32::clamp never receives min > max (which panics).
        let min_w = bounds.tokens.min_width_px;
        let max_w = bounds.max_width_px.max(min_w);
        let min_h = bounds.tokens.min_height_px;
        let max_h = bounds.max_height_px.max(min_h);

        let w = self.width.clamp(min_w, max_w);
        let h = self.height.clamp(min_h, max_h);

        // Clamp origin so the portal stays fully on-screen.
        let x = self.x.clamp(0.0, (bounds.display_w - w).max(0.0));
        let y = self.y.clamp(0.0, (bounds.display_h - h).max(0.0));

        Self {
            x,
            y,
            width: w,
            height: h,
        }
    }
}

/// Groups clamping bounds for resize operations, reducing function argument count.
///
/// Passed by reference to avoid copying across every call site on the hot path.
/// Constructed once per frame (or on geometry change) from the token map and
/// the lease/scene budget.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizeBounds {
    /// Resolved token bounds (min size, step, affordance).
    pub tokens: PortalWindowTokens,
    /// Lease/scene-budget maximum portal width in pixels (§6b.3).
    pub max_width_px: f32,
    /// Lease/scene-budget maximum portal height in pixels (§6b.3).
    pub max_height_px: f32,
    /// Display width in pixels (on-screen clamp).
    pub display_w: f32,
    /// Display height in pixels (on-screen clamp).
    pub display_h: f32,
}

// ─── Coalescible geometry snapshot (§6b.4) ────────────────────────────────────

/// Coalescible state-stream geometry snapshot delivered to the owning adapter.
///
/// Message class: **state-stream**. The transport MUST drop older snapshots
/// when a newer one arrives within the same delivery window (latest-wins).
///
/// The snapshot carries the portal's display-pixel bounds after clamping.
/// Adapter publishes that attempt to update the same portal while a gesture is
/// active MUST be rejected; the snapshot is the authoritative geometry until
/// gesture end.
///
/// Spec §6b.4: "gesture remains authoritative over adapter publishes until
/// gesture end."
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GeometrySnapshot {
    /// Portal ID (opaque string key matching the adapter's portal attachment).
    /// In the raw-tile pilot this is the portal's `interaction_id`.
    pub portal_id_hash: u64,
    /// Final clamped portal bounds after this gesture step.
    pub rect: PortalRect,
    /// True during an active gesture (gesture is authoritative).
    /// False on gesture end (adapter may resume publishing).
    pub gesture_active: bool,
    /// Monotonic sequence counter — allows the adapter to detect skipped
    /// snapshots when the transport does not deliver every event.
    pub sequence: u64,
}

// ─── Pointer resize state machine (§6b.1) ─────────────────────────────────────

/// Phase of the per-device pointer resize interaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizePhase {
    /// No resize in progress.
    Idle,
    /// Resize gesture active (pointer is held on an affordance and moving).
    Active,
}

/// Per-device pointer resize state.
///
/// One instance per active pointer device that has pressed an affordance region.
/// Instances are created on `PointerDown` on an affordance and removed on `PointerUp`.
#[derive(Clone, Debug)]
pub struct DeviceResizeState {
    /// Current phase.
    pub phase: ResizePhase,
    /// Which edge/corner is being dragged.
    pub edge: ResizeEdge,
    /// Display-space X where the pointer pressed down.
    pub press_x: f32,
    /// Display-space Y where the pointer pressed down.
    pub press_y: f32,
    /// Portal rect at the time the gesture started.
    pub initial_rect: PortalRect,
}

impl DeviceResizeState {
    /// Begin a resize gesture.
    pub fn new(edge: ResizeEdge, press_x: f32, press_y: f32, initial_rect: PortalRect) -> Self {
        Self {
            phase: ResizePhase::Active,
            edge,
            press_x,
            press_y,
            initial_rect,
        }
    }

    /// Compute the new rect given the current pointer position, clamped to bounds.
    ///
    /// Uses the initial rect and the total delta from the press origin so that
    /// floating-point error does not accumulate frame-by-frame.
    pub fn compute_rect(
        &self,
        pointer_x: f32,
        pointer_y: f32,
        bounds: &ResizeBounds,
    ) -> PortalRect {
        let dx = pointer_x - self.press_x;
        let dy = pointer_y - self.press_y;

        let mut w = self.initial_rect.width;
        let mut h = self.initial_rect.height;

        // Apply delta to the affected dimension.
        if self.edge.affects_right() {
            w += dx;
        } else if self.edge.affects_left() {
            w -= dx;
        }
        if self.edge.affects_bottom() {
            h += dy;
        } else if self.edge.affects_top() {
            h -= dy;
        }

        // Clamp dimensions first so origin computation below uses the final
        // (clamped) size — this keeps the opposite edge stationary when the
        // minimum size is hit while dragging a left or top edge.
        let min_w = bounds.tokens.min_width_px;
        let max_w = bounds.max_width_px.max(min_w);
        let min_h = bounds.tokens.min_height_px;
        let max_h = bounds.max_height_px.max(min_h);
        let w_clamped = w.clamp(min_w, max_w);
        let h_clamped = h.clamp(min_h, max_h);

        // For left/top edges the origin shifts to maintain the opposite edge.
        let mut x = self.initial_rect.x;
        let mut y = self.initial_rect.y;
        if self.edge.affects_left() {
            x = (self.initial_rect.x + self.initial_rect.width) - w_clamped;
        }
        if self.edge.affects_top() {
            y = (self.initial_rect.y + self.initial_rect.height) - h_clamped;
        }

        PortalRect {
            x,
            y,
            width: w_clamped,
            height: h_clamped,
        }
        .clamped(bounds)
    }
}

// ─── Resize event outcome ─────────────────────────────────────────────────────

/// Outcome of processing a pointer or hotkey event through the resize recognizer.
#[derive(Clone, Debug, PartialEq)]
pub enum ResizeOutcome {
    /// No resize action.
    Idle,
    /// Gesture started. Caller MUST mark the portal as gesture-authoritative.
    GestureStarted { snapshot: GeometrySnapshot },
    /// Gesture in progress — geometry changed. Emit the snapshot downstream.
    GestureUpdate { snapshot: GeometrySnapshot },
    /// Gesture ended (pointer up). Final geometry in snapshot. Caller MUST
    /// mark the portal as no longer gesture-authoritative.
    GestureEnded { snapshot: GeometrySnapshot },
    /// Hotkey resize applied (not a pointer gesture). Always gesture_active=false.
    HotkeyApplied { snapshot: GeometrySnapshot },
}

// ─── Portal resize state (per-portal) ────────────────────────────────────────

/// Error returned when an adapter geometry publish is rejected by the gesture
/// authority enforcement point.
///
/// See [`PortalResizeState::accept_adapter_publish`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GestureAuthorityError {
    /// A local resize gesture is currently active.  The adapter's geometry
    /// publish is dropped; the gesture is authoritative (§6b.4).
    GestureActive,
    /// The epoch offered by the adapter is stale (the gesture epoch has
    /// advanced since the adapter sampled it, meaning a gesture started or
    /// ended between the sample and the publish).  The publish is dropped to
    /// prevent a race where the adapter "wins" against a just-finished gesture.
    StaleEpoch,
}

/// Per-portal resize management — tracks active pointer gestures and emits
/// coalescible geometry snapshots.
///
/// One instance per portal. Callers should hold this alongside the portal's
/// current geometry and call the appropriate handler on each input event.
///
/// Use [`PortalResizeState::new`] to construct — `Default` is intentionally
/// not derived to enforce explicit initialization with a valid `portal_id_hash`.
///
/// ## Gesture-authority enforcement (§6b.4)
///
/// The `gesture_epoch` field provides a real enforcement point beyond the
/// advisory `gesture_active()` flag.  The epoch monotonically increments on
/// every gesture **start** AND every gesture **end** so that a publish that
/// was in-flight when a gesture started or ended is unambiguously stale.
///
/// Adapters MUST sample the epoch before dispatching a geometry publish, then
/// present it to [`PortalResizeState::accept_adapter_publish`] on arrival.
/// The method returns an error and the publish is discarded if:
/// - a gesture is currently active, OR
/// - the offered epoch does not match the current epoch (gesture lifecycle
///   changed since the sample).
#[derive(Debug)]
pub struct PortalResizeState {
    /// Per-device resize gestures (usually only one device at a time).
    device_states: std::collections::HashMap<u32, DeviceResizeState>,
    /// Monotonic sequence counter for snapshots.
    sequence: u64,
    /// Hash of the portal ID (used in snapshots; callers set this at creation).
    portal_id_hash: u64,
    /// Gesture epoch — monotonically incremented on every gesture start and
    /// every gesture end.  Adapters sample this before dispatching a geometry
    /// publish and present it on arrival; a mismatch means the gesture
    /// lifecycle changed in the interval and the publish must be rejected.
    ///
    /// Starts at 0 (no gesture ever started, adapter may publish freely).
    /// Even values: no gesture in progress (adapter may publish if epoch matches).
    /// Odd values: gesture in progress (adapter MUST NOT publish).
    gesture_epoch: u64,
}

impl PortalResizeState {
    /// Create a new per-portal resize state.
    ///
    /// `portal_id_hash` is a stable hash of the portal's opaque ID string,
    /// used to identify the snapshot's owner without copying a String on the
    /// hot path.
    pub fn new(portal_id_hash: u64) -> Self {
        Self {
            // Pre-allocate for the common case of one active device so the
            // first on_pointer_down does not trigger a re-allocation.
            // (The module performance contract: no allocations on the hot path.)
            device_states: std::collections::HashMap::with_capacity(1),
            sequence: 0,
            portal_id_hash,
            gesture_epoch: 0,
        }
    }

    /// Returns true while any pointer device has an active resize gesture.
    ///
    /// Advisory flag.  For the **enforcement point** that actually rejects
    /// adapter publishes, use [`accept_adapter_publish`].
    ///
    /// When this is true, the caller MUST reject adapter geometry publishes
    /// (gesture is authoritative — §6b.4).
    pub fn gesture_active(&self) -> bool {
        self.device_states
            .values()
            .any(|s| s.phase == ResizePhase::Active)
    }

    /// The edge/corner of the first active resize gesture, if any.
    ///
    /// Used by the windowed runtime to keep the OS resize cursor pinned to the
    /// gesture's affordance while the pointer drags (the pointer can leave the
    /// 8px affordance band mid-resize, so hover hit-testing alone would flicker
    /// the cursor back to the arrow).
    pub fn active_edge(&self) -> Option<ResizeEdge> {
        self.device_states
            .values()
            .find(|s| s.phase == ResizePhase::Active)
            .map(|s| s.edge)
    }

    /// Sample the current gesture epoch.
    ///
    /// Adapters MUST call this **before** dispatching an adapter geometry
    /// publish and then pass the sampled value to [`accept_adapter_publish`]
    /// on arrival.  A publish that does not present a matching epoch will be
    /// rejected, guarding against races around gesture start/end.
    ///
    /// Epoch semantics:
    /// - `0` on construction (no gesture ever started).
    /// - Incremented (odd) on gesture **start** → adapter publishes rejected.
    /// - Incremented again (even) on gesture **end** → adapter may publish if
    ///   epoch still matches.
    #[inline]
    pub fn current_gesture_epoch(&self) -> u64 {
        self.gesture_epoch
    }

    /// Enforcement point for adapter geometry publishes (§6b.4).
    ///
    /// Returns `Ok(())` if the publish may proceed; returns
    /// `Err(GestureAuthorityError)` if it must be dropped.
    ///
    /// A publish is rejected when:
    /// 1. A local gesture is currently active (`gesture_active() == true`), OR
    /// 2. `offered_epoch` does not match the current epoch (the gesture
    ///    lifecycle changed since the adapter sampled the epoch — a race
    ///    around gesture start or end).
    ///
    /// # Example
    ///
    /// ```
    /// use tze_hud_input::portal_resize::{PortalResizeState, GestureAuthorityError};
    ///
    /// let mut state = PortalResizeState::new(0xdeadbeef);
    ///
    /// // Sample the epoch before the publish is dispatched.
    /// let epoch = state.current_gesture_epoch();
    ///
    /// // … adapter dispatches the publish; on arrival: …
    /// assert!(state.accept_adapter_publish(epoch).is_ok());
    /// ```
    pub fn accept_adapter_publish(&self, offered_epoch: u64) -> Result<(), GestureAuthorityError> {
        // Check gesture first: an active gesture always rejects, regardless of epoch.
        if self.gesture_active() {
            return Err(GestureAuthorityError::GestureActive);
        }
        // Epoch mismatch means the gesture lifecycle changed since the adapter sampled.
        if offered_epoch != self.gesture_epoch {
            return Err(GestureAuthorityError::StaleEpoch);
        }
        Ok(())
    }

    fn next_sequence(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }

    fn snapshot(&mut self, rect: PortalRect, gesture_active: bool) -> GeometrySnapshot {
        GeometrySnapshot {
            portal_id_hash: self.portal_id_hash,
            rect,
            gesture_active,
            sequence: self.next_sequence(),
        }
    }

    /// Process a pointer-down event on a resize affordance.
    ///
    /// Returns `ResizeOutcome::GestureStarted` with the clamped initial rect.
    ///
    /// On idle→active transition (first pointer down on this portal), increments
    /// the gesture epoch to an odd value (odd → active) so that any in-flight
    /// adapter publish sampled before this call is rejected by
    /// [`accept_adapter_publish`].  Subsequent pointer-downs while already active
    /// (multi-device) do not change the epoch so the even/odd invariant holds.
    pub fn on_pointer_down(
        &mut self,
        device_id: u32,
        edge: ResizeEdge,
        press_x: f32,
        press_y: f32,
        current_rect: PortalRect,
        bounds: &ResizeBounds,
    ) -> ResizeOutcome {
        // Advance epoch only on idle→active transition (no prior active gesture).
        // This preserves the even/odd invariant: even = idle, odd = active.
        // With multiple devices, only the first pointer-down advances the epoch
        // (idle → active); subsequent pointer-downs while already active do not
        // change parity.
        let was_idle = !self.gesture_active();
        let initial = current_rect.clamped(bounds);
        self.device_states.insert(
            device_id,
            DeviceResizeState::new(edge, press_x, press_y, initial),
        );
        if was_idle {
            // Epoch was even (idle); advance to odd (active).
            self.gesture_epoch = self.gesture_epoch.wrapping_add(1);
        }
        let snap = self.snapshot(initial, true);
        ResizeOutcome::GestureStarted { snapshot: snap }
    }

    /// Process a pointer-move event during an active resize gesture.
    ///
    /// Returns `ResizeOutcome::GestureUpdate` when a gesture is active for
    /// `device_id`, or `ResizeOutcome::Idle` when no gesture is in progress.
    pub fn on_pointer_move(
        &mut self,
        device_id: u32,
        pointer_x: f32,
        pointer_y: f32,
        bounds: &ResizeBounds,
    ) -> ResizeOutcome {
        let Some(state) = self.device_states.get(&device_id) else {
            return ResizeOutcome::Idle;
        };
        if state.phase != ResizePhase::Active {
            return ResizeOutcome::Idle;
        }
        let rect = state.compute_rect(pointer_x, pointer_y, bounds);
        let snap = self.snapshot(rect, true);
        ResizeOutcome::GestureUpdate { snapshot: snap }
    }

    /// Process a pointer-up event, ending any active resize gesture for `device_id`.
    ///
    /// Returns `ResizeOutcome::GestureEnded` with the final clamped rect, or
    /// `ResizeOutcome::Idle` if no gesture was active.
    ///
    /// On active→idle transition (last device gesture ends), increments the
    /// gesture epoch to an even value (even → idle) so that adapter publishes
    /// sampled before this call continue to be rejected until the adapter
    /// re-samples the new epoch.  This prevents a stale publish that was
    /// in-flight during the gesture from slipping through immediately after
    /// gesture end.  Intermediate pointer-ups while other devices are still
    /// active (multi-device) do not change the epoch so the even/odd invariant
    /// holds.
    pub fn on_pointer_up(
        &mut self,
        device_id: u32,
        pointer_x: f32,
        pointer_y: f32,
        bounds: &ResizeBounds,
    ) -> ResizeOutcome {
        let Some(state) = self.device_states.remove(&device_id) else {
            return ResizeOutcome::Idle;
        };
        let rect = state.compute_rect(pointer_x, pointer_y, bounds);
        // gesture_active reflects remaining devices after removal.
        let still_active = self.gesture_active();
        // Advance epoch only on active→idle transition (last active device ended).
        // This preserves the even/odd invariant: odd = active, even = idle.
        // With multiple devices, only the final pointer-up advances the epoch
        // (active → idle); intermediate pointer-ups while other devices are still
        // active do not change parity.
        if !still_active {
            // Epoch was odd (active); advance to even (idle).
            self.gesture_epoch = self.gesture_epoch.wrapping_add(1);
        }
        let snap = self.snapshot(rect, still_active);
        ResizeOutcome::GestureEnded { snapshot: snap }
    }
}

// ─── Focus-scoped hotkey resize (§6b.2) ──────────────────────────────────────

/// Outcome of a hotkey resize event.
#[derive(Clone, Debug, PartialEq)]
pub enum HotkeyResizeOutcome {
    /// The portal is not focused; hotkey was NOT consumed.
    NotFocused,
    /// Hotkey consumed and geometry updated. Caller MUST deliver snapshot.
    Applied { snapshot: GeometrySnapshot },
}

/// Direction of a hotkey resize step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyResizeDir {
    /// Grow (Ctrl+`+` or Ctrl+`=`).
    Grow,
    /// Shrink (Ctrl+`-`).
    Shrink,
}

impl HotkeyResizeDir {
    /// Parse from a DOM `KeyboardEvent.key` string **with Ctrl modifier check**.
    ///
    /// Returns `Some(Grow)` for `"+"` or `"="` **only when `ctrl` is true**,
    /// `Some(Shrink)` for `"-"` **only when `ctrl` is true**,
    /// `None` for anything else or when `ctrl` is false.
    ///
    /// Spec §6b.2: resize shortcuts are Ctrl-scoped. Bare `+`, `=`, and `-`
    /// (without Ctrl held) MUST NOT trigger a resize — they may be content input
    /// or other application shortcuts.
    pub fn from_key(key: &str, ctrl: bool) -> Option<Self> {
        if !ctrl {
            return None;
        }
        match key {
            "+" | "=" => Some(Self::Grow),
            "-" => Some(Self::Shrink),
            _ => None,
        }
    }

    /// Parse from a **physical** `KeyCode` string (e.g. `"Equal"`, `"Minus"`,
    /// `"NumpadAdd"`, `"NumpadSubtract"`) with Ctrl modifier check.
    ///
    /// This is the layout- and modifier-independent fallback for [`from_key`].
    /// Matching the logical key string alone (`from_key`) is fragile under
    /// modified chords: with Ctrl held, winit on Windows does not reliably
    /// resolve the logical key to bare `"="`/`"-"`/`"+"`, and `"+"` requires
    /// Shift on most layouts. The physical key position is stable regardless of
    /// keyboard layout or held modifiers, so resolving the resize direction from
    /// the physical `KeyCode` makes `Ctrl+=`/`Ctrl+-` deterministic (root cause
    /// of hud-v4k1h: Ctrl resize hotkeys had no visible effect on live Windows
    /// because the logical-key match never fired).
    ///
    /// The `Equal` physical key carries both `=` and (shifted) `+`, so it maps
    /// to `Grow`; `Minus` maps to `Shrink`. The numpad `+`/`-` keys
    /// (`NumpadAdd`/`NumpadSubtract`) map the same way. Ctrl is still required,
    /// matching the spec §6b.2 focus-scoped Ctrl-chord contract.
    pub fn from_key_code(key_code: &str, ctrl: bool) -> Option<Self> {
        if !ctrl {
            return None;
        }
        match key_code {
            "Equal" | "NumpadAdd" => Some(Self::Grow),
            "Minus" | "NumpadSubtract" => Some(Self::Shrink),
            _ => None,
        }
    }
}

/// Which axis (or axes) a hotkey resize step grows/shrinks.
///
/// The symmetric Ctrl+`+`/`-` chord resizes [`Both`](Self::Both) axes at once
/// (§6b.2). The directional Ctrl+Shift+Arrow chord (hud-csrmf) resizes a single
/// axis so a keyboard viewer — and the autopilot live-verify harness — can drive
/// a **width-only** whole-portal resize without pointer injection. Width is the
/// text-wrap axis the compositor reads from `TextMarkdownNode::bounds.width`, so
/// a width step is exactly what exercises the dynamic re-wrap / reconcile path
/// (hud-rpmwt) on a multi-monitor console where the 22px pointer resize handle is
/// not injector-reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HotkeyResizeAxis {
    /// Grow/shrink both width and height by the same step (symmetric — the
    /// Ctrl+`+`/`-` chord).
    #[default]
    Both,
    /// Grow/shrink width only; height is left unchanged (Ctrl+Shift+Left/Right).
    Width,
    /// Grow/shrink height only; width is left unchanged (Ctrl+Shift+Up/Down).
    Height,
}

/// Apply a focus-scoped **symmetric** hotkey resize step to the current rect.
///
/// Convenience wrapper over [`apply_hotkey_resize_axis`] with
/// [`HotkeyResizeAxis::Both`] — the Ctrl+`+`/`-` chord (§6b.2). See that function
/// for the full argument and anchoring contract.
pub fn apply_hotkey_resize(
    focused: bool,
    dir: HotkeyResizeDir,
    current_rect: PortalRect,
    bounds: &ResizeBounds,
    state: &mut PortalResizeState,
) -> HotkeyResizeOutcome {
    apply_hotkey_resize_axis(
        focused,
        dir,
        HotkeyResizeAxis::Both,
        current_rect,
        bounds,
        state,
    )
}

/// Apply a focus-scoped hotkey resize step to the current rect along `axis`.
///
/// # Arguments
///
/// * `focused` — true if the portal holds keyboard focus. If false, the
///   hotkey is NOT consumed and `HotkeyResizeOutcome::NotFocused` is returned.
/// * `dir` — grow or shrink direction.
/// * `axis` — which axis/axes to step ([`HotkeyResizeAxis::Both`] for the
///   symmetric Ctrl+`+`/`-` chord; [`Width`](HotkeyResizeAxis::Width) /
///   [`Height`](HotkeyResizeAxis::Height) for the directional Ctrl+Shift+Arrow
///   chord, hud-csrmf).
/// * `current_rect` — portal bounds before the step.
/// * `bounds` — clamping bounds (tokens, max size, display size).
/// * `state` — portal resize state (for sequence counter).
///
/// Grows/shrinks the selected axis by `resize_step_px`. The result is clamped by
/// `clamped()` to satisfy the overflow contract. Axes not selected by `axis` keep
/// their current extent (still passed through `clamped()`, which is a no-op for
/// an already-legible extent).
pub fn apply_hotkey_resize_axis(
    focused: bool,
    dir: HotkeyResizeDir,
    axis: HotkeyResizeAxis,
    current_rect: PortalRect,
    bounds: &ResizeBounds,
    state: &mut PortalResizeState,
) -> HotkeyResizeOutcome {
    if !focused {
        return HotkeyResizeOutcome::NotFocused;
    }

    let step = bounds.tokens.resize_step_px;
    let delta = match dir {
        HotkeyResizeDir::Grow => step,
        HotkeyResizeDir::Shrink => -step,
    };

    let min_w = bounds.tokens.min_width_px;
    let max_w = bounds.max_width_px.max(min_w);
    let min_h = bounds.tokens.min_height_px;
    let max_h = bounds.max_height_px.max(min_h);
    // Step only the axis/axes selected by `axis`; the other keeps its extent.
    let w_delta = if matches!(axis, HotkeyResizeAxis::Both | HotkeyResizeAxis::Width) {
        delta
    } else {
        0.0
    };
    let h_delta = if matches!(axis, HotkeyResizeAxis::Both | HotkeyResizeAxis::Height) {
        delta
    } else {
        0.0
    };
    let new_w = (current_rect.width + w_delta).clamp(min_w, max_w);
    let new_h = (current_rect.height + h_delta).clamp(min_h, max_h);

    // Anchor the TOP-LEFT corner: the origin stays put and only width/height
    // change, so grow/shrink extends toward the bottom-right (hud-v4k1h
    // follow-up). The earlier center-anchored behavior shifted the origin by
    // half the size delta, making the portal appear to grow radially in all
    // four directions, which was disorienting. `clamped()` below still nudges
    // the origin back on-screen if a grow would push the bottom/right edge
    // past the display bound.
    let new_x = current_rect.x;
    let new_y = current_rect.y;

    let new_rect = PortalRect {
        x: new_x,
        y: new_y,
        width: new_w,
        height: new_h,
    }
    .clamped(bounds);

    // Always advance the sequence so the state-stream coalescer (latest-wins
    // by sequence number) never drops this snapshot. Keeping the sequence
    // stale would cause `AdapterGeometryBatch::coalesce` to silently discard
    // the snapshot when an earlier one with the same sequence is already
    // present — the adapter would then miss a clamped-at-boundary attempt.
    let snap = GeometrySnapshot {
        portal_id_hash: state.portal_id_hash,
        rect: new_rect,
        gesture_active: false,
        sequence: state.next_sequence(),
    };
    HotkeyResizeOutcome::Applied { snapshot: snap }
}

// ─── Affordance hit test (§6b.1) ──────────────────────────────────────────────

/// Determine which resize edge or corner a pointer position hits, given the
/// portal rect and the affordance region width.
///
/// Returns `None` if the pointer is not in any affordance region.
/// Corner affordances take priority over edge affordances (corners are at
/// the intersection of two edge capture regions).
///
/// The content layer is inside the affordance capture regions; pointer events
/// within the inner content area do not trigger resize.
pub fn hit_affordance(
    pointer_x: f32,
    pointer_y: f32,
    rect: &PortalRect,
    affordance_px: f32,
) -> Option<ResizeEdge> {
    let in_left = pointer_x >= rect.x && pointer_x < rect.x + affordance_px;
    // Use >= for the start of right/bottom bands (matching left/top) to avoid
    // a 1-px dead zone at exactly `edge - affordance_px` where the pointer
    // would be inside the rect but hit no affordance.
    let in_right =
        pointer_x >= rect.x + rect.width - affordance_px && pointer_x <= rect.x + rect.width;
    let in_top = pointer_y >= rect.y && pointer_y < rect.y + affordance_px;
    let in_bottom =
        pointer_y >= rect.y + rect.height - affordance_px && pointer_y <= rect.y + rect.height;

    // Must be within the portal rect at all
    let in_rect = pointer_x >= rect.x
        && pointer_x <= rect.x + rect.width
        && pointer_y >= rect.y
        && pointer_y <= rect.y + rect.height;

    if !in_rect {
        return None;
    }

    // Corners first (intersection of two edge bands)
    match (in_top, in_bottom, in_left, in_right) {
        (true, _, true, _) => Some(ResizeEdge::TopLeft),
        (true, _, _, true) => Some(ResizeEdge::TopRight),
        (_, true, true, _) => Some(ResizeEdge::BottomLeft),
        (_, true, _, true) => Some(ResizeEdge::BottomRight),
        (true, _, _, _) => Some(ResizeEdge::Top),
        (_, true, _, _) => Some(ResizeEdge::Bottom),
        (_, _, true, _) => Some(ResizeEdge::Left),
        (_, _, _, true) => Some(ResizeEdge::Right),
        _ => None, // inside content area
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_tokens() -> PortalWindowTokens {
        PortalWindowTokens::default()
    }

    fn default_bounds() -> ResizeBounds {
        let tokens = default_tokens();
        ResizeBounds {
            tokens,
            max_width_px: 2000.0,
            max_height_px: 2000.0,
            display_w: 3840.0,
            display_h: 2160.0,
        }
    }

    // ─── PortalRect::clamped ───────────────────────────────────────────────

    #[test]
    fn clamped_rect_within_bounds_unchanged() {
        let bounds = default_bounds();
        let r = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let c = r.clamped(&bounds);
        assert_eq!(c, r, "rect within bounds must be unchanged by clamping");
    }

    #[test]
    fn clamped_rect_enforces_min_width() {
        let bounds = default_bounds(); // min_width = 240
        let r = PortalRect {
            x: 0.0,
            y: 0.0,
            width: 50.0,
            height: 300.0,
        };
        let c = r.clamped(&bounds);
        assert!(
            c.width >= bounds.tokens.min_width_px,
            "width must be at least min_width_px after clamping"
        );
    }

    #[test]
    fn clamped_rect_enforces_min_height() {
        let bounds = default_bounds(); // min_height = 160
        let r = PortalRect {
            x: 0.0,
            y: 0.0,
            width: 400.0,
            height: 50.0,
        };
        let c = r.clamped(&bounds);
        assert!(
            c.height >= bounds.tokens.min_height_px,
            "height must be at least min_height_px after clamping"
        );
    }

    #[test]
    fn clamped_rect_stays_on_screen() {
        let bounds = default_bounds();
        // rect extends beyond right and bottom edges
        let r = PortalRect {
            x: 3700.0,
            y: 2000.0,
            width: 400.0,
            height: 300.0,
        };
        let c = r.clamped(&bounds);
        assert!(
            c.x + c.width <= bounds.display_w + f32::EPSILON,
            "right edge must stay on screen"
        );
        assert!(
            c.y + c.height <= bounds.display_h + f32::EPSILON,
            "bottom edge must stay on screen"
        );
    }

    #[test]
    fn clamped_rect_enforces_max_bounds() {
        let tokens = default_tokens();
        let bounds = ResizeBounds {
            tokens,
            max_width_px: 600.0,
            max_height_px: 400.0,
            display_w: 3840.0,
            display_h: 2160.0,
        };
        let r = PortalRect {
            x: 0.0,
            y: 0.0,
            width: 900.0,
            height: 700.0,
        };
        let c = r.clamped(&bounds);
        assert!(
            c.width <= bounds.max_width_px,
            "width must be at most max_width after clamping"
        );
        assert!(
            c.height <= bounds.max_height_px,
            "height must be at most max_height after clamping"
        );
    }

    // ─── ResizeEdge helpers ────────────────────────────────────────────────

    #[test]
    fn resize_edge_affects_correct_sides() {
        assert!(ResizeEdge::Left.affects_left());
        assert!(!ResizeEdge::Left.affects_right());
        assert!(ResizeEdge::Right.affects_right());
        assert!(!ResizeEdge::Right.affects_left());
        assert!(ResizeEdge::TopLeft.affects_left());
        assert!(ResizeEdge::TopLeft.affects_top());
        assert!(!ResizeEdge::TopLeft.affects_right());
        assert!(!ResizeEdge::TopLeft.affects_bottom());
        assert!(ResizeEdge::BottomRight.affects_right());
        assert!(ResizeEdge::BottomRight.affects_bottom());
    }

    // ─── DeviceResizeState::compute_rect ─────────────────────────────────

    #[test]
    fn right_edge_resize_grows_width_rightward() {
        let bounds = default_bounds();
        let initial = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let state = DeviceResizeState::new(ResizeEdge::Right, 500.0, 250.0, initial);
        // drag 50px to the right
        let result = state.compute_rect(550.0, 250.0, &bounds);
        assert!(
            (result.width - 450.0).abs() < 1.0,
            "right-drag 50px must grow width by 50px"
        );
        assert!(
            (result.x - initial.x).abs() < 1.0,
            "right-drag must not change x"
        );
    }

    #[test]
    fn left_edge_resize_grows_width_leftward() {
        let bounds = default_bounds();
        let initial = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let state = DeviceResizeState::new(ResizeEdge::Left, 100.0, 250.0, initial);
        // drag 50px to the left
        let result = state.compute_rect(50.0, 250.0, &bounds);
        assert!(
            (result.width - 450.0).abs() < 1.0,
            "left-drag 50px must grow width by 50px"
        );
        assert!(
            (result.x - 50.0).abs() < 1.0,
            "left-drag must move x leftward"
        );
    }

    #[test]
    fn resize_clamps_to_min_width_when_dragging_too_far() {
        let bounds = default_bounds(); // min_width = 240
        let initial = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let state = DeviceResizeState::new(ResizeEdge::Right, 500.0, 250.0, initial);
        // drag 400px to the left (would make width=-100)
        let result = state.compute_rect(100.0, 250.0, &bounds);
        assert!(
            result.width >= bounds.tokens.min_width_px,
            "resize past minimum must clamp to min_width_px (no clipped glyphs)"
        );
    }

    #[test]
    fn resize_delta_uses_initial_rect_not_accumulated() {
        // Verifies that compute_rect always uses the initial rect + total delta,
        // not the accumulated delta — preventing floating-point drift.
        let bounds = default_bounds();
        let initial = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let state = DeviceResizeState::new(ResizeEdge::Right, 500.0, 250.0, initial);

        let result_a = state.compute_rect(550.0, 250.0, &bounds);
        let result_b = state.compute_rect(600.0, 250.0, &bounds);

        // Each call uses the initial rect, so result_b should be 50px wider than result_a
        assert!(
            (result_b.width - result_a.width - 50.0).abs() < 1.0,
            "second compute must be 50px wider than first (no accumulated drift)"
        );
    }

    // ─── PortalResizeState gesture lifecycle ──────────────────────────────

    #[test]
    fn gesture_not_active_by_default() {
        let state = PortalResizeState::new(0xdeadbeef);
        assert!(
            !state.gesture_active(),
            "no gesture should be active at construction"
        );
    }

    #[test]
    fn gesture_active_after_pointer_down() {
        let mut state = PortalResizeState::new(0xdeadbeef);
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let outcome = state.on_pointer_down(1, ResizeEdge::Right, 500.0, 250.0, rect, &bounds);
        assert!(
            state.gesture_active(),
            "gesture must be active after pointer-down"
        );
        matches!(outcome, ResizeOutcome::GestureStarted { .. });
    }

    #[test]
    fn gesture_inactive_after_pointer_up() {
        let mut state = PortalResizeState::new(0xdeadbeef);
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        state.on_pointer_down(1, ResizeEdge::Right, 500.0, 250.0, rect, &bounds);
        let outcome = state.on_pointer_up(1, 550.0, 250.0, &bounds);
        assert!(
            !state.gesture_active(),
            "gesture must be inactive after pointer-up"
        );
        matches!(outcome, ResizeOutcome::GestureEnded { .. });
    }

    #[test]
    fn snapshot_sequence_is_monotonically_increasing() {
        let mut state = PortalResizeState::new(0xdeadbeef);
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };

        let start = state.on_pointer_down(1, ResizeEdge::Right, 500.0, 250.0, rect, &bounds);
        let mid = state.on_pointer_move(1, 520.0, 250.0, &bounds);
        let end = state.on_pointer_up(1, 540.0, 250.0, &bounds);

        let seq_start = match start {
            ResizeOutcome::GestureStarted { ref snapshot } => snapshot.sequence,
            _ => panic!("expected GestureStarted"),
        };
        let seq_mid = match mid {
            ResizeOutcome::GestureUpdate { ref snapshot } => snapshot.sequence,
            _ => panic!("expected GestureUpdate"),
        };
        let seq_end = match end {
            ResizeOutcome::GestureEnded { ref snapshot } => snapshot.sequence,
            _ => panic!("expected GestureEnded"),
        };

        assert!(
            seq_start < seq_mid,
            "sequence must be monotonically increasing: start < mid"
        );
        assert!(
            seq_mid < seq_end,
            "sequence must be monotonically increasing: mid < end"
        );
    }

    #[test]
    fn gesture_snapshot_has_gesture_active_false_on_end() {
        let mut state = PortalResizeState::new(0xdeadbeef);
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };

        state.on_pointer_down(1, ResizeEdge::Right, 500.0, 250.0, rect, &bounds);
        let end = state.on_pointer_up(1, 550.0, 250.0, &bounds);

        let snap = match end {
            ResizeOutcome::GestureEnded { snapshot } => snapshot,
            _ => panic!("expected GestureEnded"),
        };
        assert!(
            !snap.gesture_active,
            "gesture_active must be false in GestureEnded snapshot"
        );
    }

    #[test]
    fn move_event_without_active_gesture_returns_idle() {
        let mut state = PortalResizeState::new(0xdeadbeef);
        let bounds = default_bounds();
        let outcome = state.on_pointer_move(1, 500.0, 250.0, &bounds);
        assert_eq!(
            outcome,
            ResizeOutcome::Idle,
            "move without active gesture must return Idle"
        );
    }

    // ─── Hotkey resize ────────────────────────────────────────────────────

    #[test]
    fn hotkey_grow_increases_size() {
        let bounds = default_bounds(); // step = 32px
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let mut state = PortalResizeState::new(0xdeadbeef);

        let result = apply_hotkey_resize(true, HotkeyResizeDir::Grow, rect, &bounds, &mut state);

        let snap = match result {
            HotkeyResizeOutcome::Applied { snapshot } => snapshot,
            _ => panic!("expected Applied"),
        };
        assert!(snap.rect.width > rect.width, "grow must increase width");
        assert!(snap.rect.height > rect.height, "grow must increase height");
        // hud-v4k1h: top-left anchored — the origin must NOT move on grow, so
        // the portal extends toward the bottom-right rather than radially.
        assert_eq!(snap.rect.x, rect.x, "grow must keep the left edge anchored");
        assert_eq!(snap.rect.y, rect.y, "grow must keep the top edge anchored");
        assert!(
            !snap.gesture_active,
            "hotkey resize must never set gesture_active"
        );
    }

    #[test]
    fn hotkey_shrink_keeps_top_left_anchored() {
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 600.0,
            height: 400.0,
        };
        let mut state = PortalResizeState::new(0xdeadbeef);

        let result = apply_hotkey_resize(true, HotkeyResizeDir::Shrink, rect, &bounds, &mut state);
        let snap = match result {
            HotkeyResizeOutcome::Applied { snapshot } => snapshot,
            _ => panic!("expected Applied"),
        };
        // Shrink also anchors top-left: the origin stays put and the bottom-
        // right edge moves inward (no radial/centered shrink).
        assert_eq!(
            snap.rect.x, rect.x,
            "shrink must keep the left edge anchored"
        );
        assert_eq!(
            snap.rect.y, rect.y,
            "shrink must keep the top edge anchored"
        );
        assert!(snap.rect.width < rect.width, "shrink must decrease width");
        assert!(
            snap.rect.height < rect.height,
            "shrink must decrease height"
        );
    }

    #[test]
    fn hotkey_shrink_decreases_size() {
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 600.0,
            height: 400.0,
        };
        let mut state = PortalResizeState::new(0xdeadbeef);

        let result = apply_hotkey_resize(true, HotkeyResizeDir::Shrink, rect, &bounds, &mut state);

        let snap = match result {
            HotkeyResizeOutcome::Applied { snapshot } => snapshot,
            _ => panic!("expected Applied"),
        };
        assert!(snap.rect.width < rect.width, "shrink must decrease width");
        assert!(
            snap.rect.height < rect.height,
            "shrink must decrease height"
        );
    }

    #[test]
    fn hotkey_not_consumed_when_portal_not_focused() {
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let mut state = PortalResizeState::new(0xdeadbeef);

        let result = apply_hotkey_resize(false, HotkeyResizeDir::Grow, rect, &bounds, &mut state);
        assert_eq!(
            result,
            HotkeyResizeOutcome::NotFocused,
            "hotkey must not be consumed by unfocused portal"
        );
    }

    #[test]
    fn hotkey_shrink_clamped_to_min_at_boundary() {
        let bounds = default_bounds(); // min=240x160, step=32
        // Portal already at or near minimum size
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: bounds.tokens.min_width_px,
            height: bounds.tokens.min_height_px,
        };
        let mut state = PortalResizeState::new(0xdeadbeef);

        let result = apply_hotkey_resize(true, HotkeyResizeDir::Shrink, rect, &bounds, &mut state);

        let snap = match result {
            HotkeyResizeOutcome::Applied { snapshot } => snapshot,
            _ => panic!("expected Applied"),
        };
        assert!(
            snap.rect.width >= bounds.tokens.min_width_px,
            "shrink at minimum must clamp to min_width (no clipped glyphs)"
        );
        assert!(
            snap.rect.height >= bounds.tokens.min_height_px,
            "shrink at minimum must clamp to min_height (no clipped glyphs)"
        );
    }

    // ─── Directional (axis-specific) hotkey resize (hud-csrmf) ────────────

    #[test]
    fn hotkey_width_axis_changes_width_only() {
        let bounds = default_bounds(); // step = 32px
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let mut state = PortalResizeState::new(0xdeadbeef);

        let result = apply_hotkey_resize_axis(
            true,
            HotkeyResizeDir::Grow,
            HotkeyResizeAxis::Width,
            rect,
            &bounds,
            &mut state,
        );
        let snap = match result {
            HotkeyResizeOutcome::Applied { snapshot } => snapshot,
            _ => panic!("expected Applied"),
        };
        assert!(
            snap.rect.width > rect.width,
            "width-axis grow must increase width"
        );
        assert_eq!(
            snap.rect.height, rect.height,
            "width-axis grow must leave height unchanged (the re-wrap axis is width only)"
        );
        // Top-left anchored: origin stays put.
        assert_eq!(
            snap.rect.x, rect.x,
            "width grow keeps the left edge anchored"
        );
        assert_eq!(
            snap.rect.y, rect.y,
            "width grow keeps the top edge anchored"
        );
        assert!(
            !snap.gesture_active,
            "hotkey resize must never set gesture_active"
        );
    }

    #[test]
    fn hotkey_height_axis_changes_height_only() {
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let mut state = PortalResizeState::new(0xdeadbeef);

        let result = apply_hotkey_resize_axis(
            true,
            HotkeyResizeDir::Shrink,
            HotkeyResizeAxis::Height,
            rect,
            &bounds,
            &mut state,
        );
        let snap = match result {
            HotkeyResizeOutcome::Applied { snapshot } => snapshot,
            _ => panic!("expected Applied"),
        };
        assert_eq!(
            snap.rect.width, rect.width,
            "height-axis shrink must leave width unchanged"
        );
        assert!(
            snap.rect.height < rect.height,
            "height-axis shrink must decrease height"
        );
        assert_eq!(
            snap.rect.x, rect.x,
            "height shrink keeps the left edge anchored"
        );
        assert_eq!(
            snap.rect.y, rect.y,
            "height shrink keeps the top edge anchored"
        );
    }

    #[test]
    fn hotkey_width_axis_not_consumed_when_unfocused() {
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let mut state = PortalResizeState::new(0xdeadbeef);

        let result = apply_hotkey_resize_axis(
            false,
            HotkeyResizeDir::Grow,
            HotkeyResizeAxis::Width,
            rect,
            &bounds,
            &mut state,
        );
        assert_eq!(
            result,
            HotkeyResizeOutcome::NotFocused,
            "directional hotkey must not be consumed by an unfocused portal"
        );
    }

    #[test]
    fn hotkey_width_axis_clamped_to_min_at_boundary() {
        let bounds = default_bounds(); // min=240x160, step=32
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: bounds.tokens.min_width_px,
            height: 300.0,
        };
        let mut state = PortalResizeState::new(0xdeadbeef);

        let result = apply_hotkey_resize_axis(
            true,
            HotkeyResizeDir::Shrink,
            HotkeyResizeAxis::Width,
            rect,
            &bounds,
            &mut state,
        );
        let snap = match result {
            HotkeyResizeOutcome::Applied { snapshot } => snapshot,
            _ => panic!("expected Applied"),
        };
        assert!(
            snap.rect.width >= bounds.tokens.min_width_px,
            "width shrink at minimum must clamp to min_width (no clipped glyphs)"
        );
        assert_eq!(
            snap.rect.height, rect.height,
            "width-axis shrink must not touch height even at the width min boundary"
        );
    }

    #[test]
    fn hotkey_width_axis_clamped_to_display_edge() {
        // A display-bounded max: grow past the on-screen limit must clamp width
        // and keep the portal on-screen (top-left anchored).
        let tokens = default_tokens();
        let bounds = ResizeBounds {
            tokens,
            max_width_px: 100_000.0, // lease budget effectively unbounded
            max_height_px: 100_000.0,
            display_w: 800.0,
            display_h: 600.0,
        };
        let rect = PortalRect {
            x: 700.0, // right edge already near the 800px display width
            y: 100.0,
            width: 90.0,
            height: 300.0,
        };
        let mut state = PortalResizeState::new(0xdeadbeef);

        let result = apply_hotkey_resize_axis(
            true,
            HotkeyResizeDir::Grow,
            HotkeyResizeAxis::Width,
            rect,
            &bounds,
            &mut state,
        );
        let snap = match result {
            HotkeyResizeOutcome::Applied { snapshot } => snapshot,
            _ => panic!("expected Applied"),
        };
        assert!(
            snap.rect.x + snap.rect.width <= bounds.display_w + f32::EPSILON,
            "width grow must clamp the right edge to the display bound (got x={} w={})",
            snap.rect.x,
            snap.rect.width
        );
    }

    #[test]
    fn hotkey_default_axis_is_symmetric() {
        // The `apply_hotkey_resize` wrapper and `HotkeyResizeAxis::Both` agree:
        // both grow width AND height (the Ctrl+`+`/`-` chord).
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let mut a = PortalResizeState::new(0x1);
        let mut b = PortalResizeState::new(0x1);
        let via_wrapper = apply_hotkey_resize(true, HotkeyResizeDir::Grow, rect, &bounds, &mut a);
        let via_axis = apply_hotkey_resize_axis(
            true,
            HotkeyResizeDir::Grow,
            HotkeyResizeAxis::Both,
            rect,
            &bounds,
            &mut b,
        );
        let (wa, wb) = match (via_wrapper, via_axis) {
            (
                HotkeyResizeOutcome::Applied { snapshot: sa },
                HotkeyResizeOutcome::Applied { snapshot: sb },
            ) => (sa.rect, sb.rect),
            _ => panic!("expected Applied"),
        };
        assert_eq!(wa, wb, "wrapper must equal HotkeyResizeAxis::Both");
        assert!(wa.width > rect.width && wa.height > rect.height);
    }

    #[test]
    fn hotkey_key_parser_from_key_requires_ctrl() {
        // Ctrl held: Grow/Shrink keys are recognised.
        assert_eq!(
            HotkeyResizeDir::from_key("+", true),
            Some(HotkeyResizeDir::Grow),
            "Ctrl+'+' must map to Grow"
        );
        assert_eq!(
            HotkeyResizeDir::from_key("=", true),
            Some(HotkeyResizeDir::Grow),
            "Ctrl+'=' must map to Grow"
        );
        assert_eq!(
            HotkeyResizeDir::from_key("-", true),
            Some(HotkeyResizeDir::Shrink),
            "Ctrl+'-' must map to Shrink"
        );
        assert_eq!(
            HotkeyResizeDir::from_key("a", true),
            None,
            "Ctrl+'a' must return None (not a resize key)"
        );
        assert_eq!(
            HotkeyResizeDir::from_key("Enter", true),
            None,
            "Ctrl+Enter must return None"
        );

        // Bare (no Ctrl): MUST NOT trigger resize regardless of key (§6b.2).
        assert_eq!(
            HotkeyResizeDir::from_key("+", false),
            None,
            "bare '+' without Ctrl MUST NOT trigger resize"
        );
        assert_eq!(
            HotkeyResizeDir::from_key("=", false),
            None,
            "bare '=' without Ctrl MUST NOT trigger resize"
        );
        assert_eq!(
            HotkeyResizeDir::from_key("-", false),
            None,
            "bare '-' without Ctrl MUST NOT trigger resize"
        );
    }

    #[test]
    fn hotkey_key_parser_from_key_code_is_layout_independent() {
        // hud-v4k1h: the physical-KeyCode fallback must resolve the resize
        // direction even when the logical key string never yields "="/"-"/"+"
        // (Ctrl held on Windows). Equal carries both '=' and shifted '+'.
        assert_eq!(
            HotkeyResizeDir::from_key_code("Equal", true),
            Some(HotkeyResizeDir::Grow),
            "Ctrl+Equal (physical) must map to Grow"
        );
        assert_eq!(
            HotkeyResizeDir::from_key_code("Minus", true),
            Some(HotkeyResizeDir::Shrink),
            "Ctrl+Minus (physical) must map to Shrink"
        );
        assert_eq!(
            HotkeyResizeDir::from_key_code("NumpadAdd", true),
            Some(HotkeyResizeDir::Grow),
            "Ctrl+NumpadAdd must map to Grow"
        );
        assert_eq!(
            HotkeyResizeDir::from_key_code("NumpadSubtract", true),
            Some(HotkeyResizeDir::Shrink),
            "Ctrl+NumpadSubtract must map to Shrink"
        );
        assert_eq!(
            HotkeyResizeDir::from_key_code("KeyA", true),
            None,
            "Ctrl+KeyA must return None (not a resize key)"
        );
        // Bare (no Ctrl): physical fallback MUST also stay Ctrl-scoped (§6b.2),
        // otherwise pressing '=' as content would resize a focused portal.
        assert_eq!(
            HotkeyResizeDir::from_key_code("Equal", false),
            None,
            "bare Equal without Ctrl MUST NOT trigger resize"
        );
        assert_eq!(
            HotkeyResizeDir::from_key_code("Minus", false),
            None,
            "bare Minus without Ctrl MUST NOT trigger resize"
        );
    }

    // ─── Affordance hit test ───────────────────────────────────────────────

    #[test]
    fn hit_affordance_right_edge() {
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let affordance = 8.0;
        // right edge: x=492..500
        let edge = hit_affordance(496.0, 250.0, &rect, affordance);
        assert_eq!(edge, Some(ResizeEdge::Right));
    }

    #[test]
    fn hit_affordance_left_edge() {
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let affordance = 8.0;
        // left edge: x=100..108
        let edge = hit_affordance(104.0, 250.0, &rect, affordance);
        assert_eq!(edge, Some(ResizeEdge::Left));
    }

    #[test]
    fn hit_affordance_top_edge() {
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let affordance = 8.0;
        let edge = hit_affordance(300.0, 104.0, &rect, affordance);
        assert_eq!(edge, Some(ResizeEdge::Top));
    }

    #[test]
    fn hit_affordance_bottom_edge() {
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let affordance = 8.0;
        // bottom edge: y=392..400
        let edge = hit_affordance(300.0, 396.0, &rect, affordance);
        assert_eq!(edge, Some(ResizeEdge::Bottom));
    }

    #[test]
    fn hit_affordance_top_left_corner() {
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let affordance = 8.0;
        let edge = hit_affordance(104.0, 104.0, &rect, affordance);
        assert_eq!(edge, Some(ResizeEdge::TopLeft));
    }

    #[test]
    fn hit_affordance_bottom_right_corner() {
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let affordance = 8.0;
        let edge = hit_affordance(496.0, 396.0, &rect, affordance);
        assert_eq!(edge, Some(ResizeEdge::BottomRight));
    }

    #[test]
    fn hit_affordance_content_area_returns_none() {
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let affordance = 8.0;
        // centre of the content area
        let edge = hit_affordance(300.0, 250.0, &rect, affordance);
        assert_eq!(edge, None, "content area must not hit any affordance");
    }

    #[test]
    fn hit_affordance_outside_rect_returns_none() {
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let affordance = 8.0;
        let edge = hit_affordance(50.0, 250.0, &rect, affordance);
        assert_eq!(
            edge, None,
            "pointer outside rect must not hit any affordance"
        );
    }

    // ─── Adapter authority (§6b.4) ────────────────────────────────────────

    /// During an active gesture, `gesture_active()` returns true, which the
    /// caller uses to reject adapter geometry publishes (advisory path).
    #[test]
    fn adapter_publishes_must_be_rejected_during_gesture() {
        let mut state = PortalResizeState::new(0xdeadbeef);
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };

        // Before gesture: adapter may publish
        assert!(
            !state.gesture_active(),
            "adapter may publish before gesture starts"
        );

        state.on_pointer_down(1, ResizeEdge::Bottom, 300.0, 400.0, rect, &bounds);

        // During gesture: adapter must NOT publish
        assert!(
            state.gesture_active(),
            "adapter must be blocked during active gesture (gesture is authoritative)"
        );

        state.on_pointer_up(1, 300.0, 450.0, &bounds);

        // After gesture: adapter may publish again
        assert!(
            !state.gesture_active(),
            "adapter may publish after gesture ends"
        );
    }

    // ─── Gesture-authority enforcement point: accept_adapter_publish ─────

    /// `accept_adapter_publish` accepts a publish with a fresh epoch when no
    /// gesture is active.
    #[test]
    fn accept_adapter_publish_ok_outside_gesture() {
        let state = PortalResizeState::new(0xdeadbeef);
        let epoch = state.current_gesture_epoch();
        assert_eq!(
            state.accept_adapter_publish(epoch),
            Ok(()),
            "publish with matching epoch must be accepted when no gesture is active"
        );
    }

    /// `accept_adapter_publish` rejects during an active gesture, regardless
    /// of epoch.  This is the real enforcement point (§6b.4).
    #[test]
    fn accept_adapter_publish_rejected_during_gesture() {
        let mut state = PortalResizeState::new(0xdeadbeef);
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };

        // Sample epoch before the gesture starts — this is the "in-flight" case.
        let epoch_before = state.current_gesture_epoch();

        state.on_pointer_down(1, ResizeEdge::Right, 500.0, 250.0, rect, &bounds);

        // Offering the pre-gesture epoch must be rejected (gesture active).
        assert_eq!(
            state.accept_adapter_publish(epoch_before),
            Err(GestureAuthorityError::GestureActive),
            "in-flight publish with pre-gesture epoch must be rejected during gesture"
        );

        // Offering the current epoch (odd — gesture active) must also be rejected.
        let epoch_during = state.current_gesture_epoch();
        assert_eq!(
            state.accept_adapter_publish(epoch_during),
            Err(GestureAuthorityError::GestureActive),
            "publish with current (odd) epoch must be rejected while gesture is active"
        );
    }

    /// After gesture end the publish is accepted only with the new (post-gesture)
    /// epoch.  A publish that sampled the epoch *before* gesture start and arrives
    /// after gesture end presents a stale epoch and MUST be rejected.
    #[test]
    fn accept_adapter_publish_stale_epoch_after_gesture_end() {
        let mut state = PortalResizeState::new(0xdeadbeef);
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };

        // Adapter samples the epoch before gesture start and dispatches a publish.
        let epoch_sampled_before_gesture = state.current_gesture_epoch(); // 0

        // Gesture starts and ends — epoch advances twice (0 → 1 → 2).
        state.on_pointer_down(1, ResizeEdge::Bottom, 300.0, 400.0, rect, &bounds);
        state.on_pointer_up(1, 300.0, 450.0, &bounds);

        // Gesture is over; adapter publish now arrives.
        // Offering the pre-gesture epoch (0) must be rejected as stale.
        assert_eq!(
            state.accept_adapter_publish(epoch_sampled_before_gesture),
            Err(GestureAuthorityError::StaleEpoch),
            "publish sampled before gesture start must be rejected as stale after gesture end"
        );

        // Adapter re-samples and publishes with the new epoch.
        let fresh_epoch = state.current_gesture_epoch(); // 2 (even — no gesture)
        assert_eq!(
            state.accept_adapter_publish(fresh_epoch),
            Ok(()),
            "publish with fresh post-gesture epoch must be accepted"
        );
    }

    /// A publish that sampled the epoch *during* a gesture (odd epoch) must be
    /// rejected after the gesture ends, even though no gesture is active.
    #[test]
    fn accept_adapter_publish_in_flight_during_gesture_rejected_after_end() {
        let mut state = PortalResizeState::new(0xdeadbeef);
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };

        state.on_pointer_down(1, ResizeEdge::Right, 500.0, 250.0, rect, &bounds);

        // Adapter is blocked — but suppose it somehow sampled the odd epoch.
        let epoch_during_gesture = state.current_gesture_epoch(); // odd

        state.on_pointer_up(1, 550.0, 250.0, &bounds);

        // Now gesture is over; the publish arrives with the in-gesture epoch.
        // It must be rejected as stale (epoch advanced on pointer-up).
        assert_eq!(
            state.accept_adapter_publish(epoch_during_gesture),
            Err(GestureAuthorityError::StaleEpoch),
            "publish sampled during gesture must be rejected as stale after gesture end"
        );
    }

    // ─── Shell/chrome-reserved shortcut classifier (§6b.2) ───────────────

    #[test]
    fn shell_reserved_ctrl_tab_navigation() {
        assert!(
            ShellReservedShortcut::is_reserved("Tab", true, false, false),
            "Ctrl+Tab must be reserved (NextTab)"
        );
        assert!(
            ShellReservedShortcut::is_reserved("Tab", true, true, false),
            "Ctrl+Shift+Tab must be reserved (PrevTab)"
        );
    }

    #[test]
    fn shell_reserved_ctrl_digit_tab_jump() {
        for digit in &["1", "2", "3", "4", "5", "6", "7", "8", "9"] {
            assert!(
                ShellReservedShortcut::is_reserved(digit, true, false, false),
                "Ctrl+{digit} must be reserved (GotoTab)",
            );
        }
    }

    #[test]
    fn shell_reserved_ctrl_shift_m_mute() {
        assert!(
            ShellReservedShortcut::is_reserved("m", true, true, false),
            "Ctrl+Shift+m must be reserved (MuteToggle)"
        );
        assert!(
            ShellReservedShortcut::is_reserved("M", true, true, false),
            "Ctrl+Shift+M must be reserved (MuteToggle)"
        );
    }

    #[test]
    fn shell_reserved_ctrl_shift_escape_safe_mode() {
        assert!(
            ShellReservedShortcut::is_reserved("Escape", true, true, false),
            "Ctrl+Shift+Escape must be reserved (SafeMode toggle)"
        );
    }

    #[test]
    fn shell_reserved_ctrl_shift_p_pairing_and_no_monitor_cycle() {
        assert!(
            ShellReservedShortcut::is_reserved("P", true, true, false),
            "Ctrl+Shift+P must be reserved (pairing)"
        );
        for key in ["F8", "F9"] {
            assert!(
                !ShellReservedShortcut::is_reserved(key, true, true, false),
                "Ctrl+Shift+{key} is no longer a shell shortcut"
            );
        }
    }

    #[test]
    fn shell_reserved_does_not_claim_portal_resize_keys() {
        // Portal resize keys (Ctrl+`+`, Ctrl+`=`, Ctrl+`-`) must NOT be
        // in the reserved set — they are handled at Priority 4.
        assert!(
            !ShellReservedShortcut::is_reserved("+", true, false, false),
            "Ctrl+'+' must NOT be reserved (portal resize key)"
        );
        assert!(
            !ShellReservedShortcut::is_reserved("=", true, false, false),
            "Ctrl+'=' must NOT be reserved (portal resize key)"
        );
        assert!(
            !ShellReservedShortcut::is_reserved("-", true, false, false),
            "Ctrl+'-' must NOT be reserved (portal resize key)"
        );
    }

    #[test]
    fn shell_reserved_alt_modifier_never_reserved() {
        // Alt is never part of the reserved set.
        assert!(
            !ShellReservedShortcut::is_reserved("Tab", true, false, true),
            "Ctrl+Alt+Tab must NOT be reserved"
        );
        assert!(
            !ShellReservedShortcut::is_reserved("1", true, false, true),
            "Ctrl+Alt+1 must NOT be reserved"
        );
    }

    #[test]
    fn shell_reserved_bare_keys_without_ctrl_not_reserved() {
        // Without Ctrl, nothing is reserved.
        assert!(
            !ShellReservedShortcut::is_reserved("Tab", false, false, false),
            "bare Tab must NOT be reserved"
        );
        assert!(
            !ShellReservedShortcut::is_reserved("Escape", false, true, false),
            "bare Shift+Escape must NOT be reserved"
        );
    }

    #[test]
    fn shell_reserved_arbitrary_key_not_reserved() {
        assert!(
            !ShellReservedShortcut::is_reserved("a", true, false, false),
            "Ctrl+a must NOT be reserved (not in the chrome set)"
        );
        assert!(
            !ShellReservedShortcut::is_reserved("Enter", true, false, false),
            "Ctrl+Enter must NOT be reserved"
        );
    }

    // ─── Cursor feedback: edge → cursor mapping ──────────────────────────────

    #[test]
    fn edge_cursor_mapping_covers_all_eight_zones() {
        // Edges: horizontal vs vertical double-arrows.
        assert_eq!(ResizeEdge::Left.cursor(), PortalCursor::EwResize);
        assert_eq!(ResizeEdge::Right.cursor(), PortalCursor::EwResize);
        assert_eq!(ResizeEdge::Top.cursor(), PortalCursor::NsResize);
        assert_eq!(ResizeEdge::Bottom.cursor(), PortalCursor::NsResize);
        // Corners: the two diagonals. TopLeft/BottomRight share the `╲` axis;
        // TopRight/BottomLeft share the `╱` axis.
        assert_eq!(ResizeEdge::TopLeft.cursor(), PortalCursor::NwseResize);
        assert_eq!(ResizeEdge::BottomRight.cursor(), PortalCursor::NwseResize);
        assert_eq!(ResizeEdge::TopRight.cursor(), PortalCursor::NeswResize);
        assert_eq!(ResizeEdge::BottomLeft.cursor(), PortalCursor::NeswResize);
    }

    #[test]
    fn opposite_edges_share_a_cursor_axis() {
        // Property: an edge and its geometric opposite are dragged along the
        // same axis, so they must present the same double-arrow cursor.
        for (a, b) in [
            (ResizeEdge::Left, ResizeEdge::Right),
            (ResizeEdge::Top, ResizeEdge::Bottom),
            (ResizeEdge::TopLeft, ResizeEdge::BottomRight),
            (ResizeEdge::TopRight, ResizeEdge::BottomLeft),
        ] {
            assert_eq!(
                a.cursor(),
                b.cursor(),
                "{a:?} and {b:?} are opposite edges and must share a cursor axis"
            );
        }
    }

    // ─── Cursor feedback: hover precedence ───────────────────────────────────

    fn portal_400x300() -> PortalRect {
        PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        }
    }

    #[test]
    fn hover_over_right_edge_yields_ew_resize() {
        let rect = portal_400x300();
        // Right edge band is x in [492, 500), vertically interior.
        let cursor = portal_hover_cursor(497.0, 250.0, Some(rect), 8.0, false, None, false);
        assert_eq!(cursor, PortalCursor::EwResize);
    }

    #[test]
    fn hover_over_top_left_corner_yields_nwse_resize() {
        let rect = portal_400x300();
        // Top-left corner: both x and y inside the 8px band of the origin.
        let cursor = portal_hover_cursor(102.0, 102.0, Some(rect), 8.0, false, None, false);
        assert_eq!(cursor, PortalCursor::NwseResize);
    }

    #[test]
    fn hover_interior_with_no_affordance_yields_default_arrow() {
        let rect = portal_400x300();
        // Dead center — no affordance, no handle.
        let cursor = portal_hover_cursor(300.0, 250.0, Some(rect), 8.0, false, None, false);
        assert_eq!(cursor, PortalCursor::Default);
    }

    #[test]
    fn hover_over_drag_handle_without_edge_yields_grab() {
        let rect = portal_400x300();
        // Interior point reported as over the drag handle (header move region).
        let cursor = portal_hover_cursor(300.0, 250.0, Some(rect), 8.0, true, None, false);
        assert_eq!(cursor, PortalCursor::Grab);
    }

    #[test]
    fn resize_edge_takes_priority_over_drag_handle() {
        let rect = portal_400x300();
        // On the top edge AND flagged over the drag handle: the thin resize band
        // must win so the header's top edge still resizes (OS convention).
        let cursor = portal_hover_cursor(300.0, 102.0, Some(rect), 8.0, true, None, false);
        assert_eq!(cursor, PortalCursor::NsResize);
    }

    #[test]
    fn active_resize_gesture_pins_edge_cursor_even_off_band() {
        // Pointer dragged far outside the portal during an active right-edge
        // resize: the cursor stays EwResize (no flicker back to arrow).
        let cursor = portal_hover_cursor(
            2000.0,
            2000.0,
            None,
            8.0,
            false,
            Some(ResizeEdge::Right),
            false,
        );
        assert_eq!(cursor, PortalCursor::EwResize);
    }

    #[test]
    fn active_resize_gesture_outranks_drag_active() {
        // If both an active resize and an active drag were somehow signalled,
        // resize (the more specific affordance gesture) wins.
        let cursor = portal_hover_cursor(
            300.0,
            250.0,
            Some(portal_400x300()),
            8.0,
            true,
            Some(ResizeEdge::TopLeft),
            true,
        );
        assert_eq!(cursor, PortalCursor::NwseResize);
    }

    #[test]
    fn active_drag_yields_grabbing() {
        // Drag-to-move in progress, no resize gesture: closed-hand cursor.
        let cursor = portal_hover_cursor(300.0, 250.0, None, 8.0, false, None, true);
        assert_eq!(cursor, PortalCursor::Grabbing);
    }

    #[test]
    fn no_focused_portal_and_no_gesture_yields_default() {
        // Nothing focused, nothing active: ordinary arrow.
        let cursor = portal_hover_cursor(300.0, 250.0, None, 8.0, false, None, false);
        assert_eq!(cursor, PortalCursor::Default);
    }

    // ─── Cursor feedback: hysteresis tracker ─────────────────────────────────

    #[test]
    fn tracker_starts_at_default_and_suppresses_redundant_default() {
        let mut tracker = CursorIconTracker::new();
        assert_eq!(tracker.current(), PortalCursor::Default);
        // Re-applying Default (the starting shape) must not issue an OS call.
        assert_eq!(tracker.update(PortalCursor::Default), None);
    }

    #[test]
    fn tracker_emits_only_on_change() {
        let mut tracker = CursorIconTracker::new();
        // First non-default shape emits.
        assert_eq!(
            tracker.update(PortalCursor::EwResize),
            Some(PortalCursor::EwResize)
        );
        // Same shape again is suppressed.
        assert_eq!(tracker.update(PortalCursor::EwResize), None);
        // A different shape emits.
        assert_eq!(
            tracker.update(PortalCursor::NsResize),
            Some(PortalCursor::NsResize)
        );
        assert_eq!(tracker.current(), PortalCursor::NsResize);
    }

    #[test]
    fn tracker_restores_to_default_exactly_once() {
        let mut tracker = CursorIconTracker::new();
        assert_eq!(
            tracker.update(PortalCursor::NwseResize),
            Some(PortalCursor::NwseResize)
        );
        // Leaving the affordance restores the arrow — emitted once.
        assert_eq!(
            tracker.update(PortalCursor::Default),
            Some(PortalCursor::Default)
        );
        // Still on the arrow — suppressed.
        assert_eq!(tracker.update(PortalCursor::Default), None);
    }

    // ─── Cursor feedback: active-edge accessor ───────────────────────────────

    #[test]
    fn active_edge_none_when_idle_then_some_while_gesturing() {
        let mut state = PortalResizeState::new(7);
        assert_eq!(state.active_edge(), None, "idle portal has no active edge");
        let bounds = default_bounds();
        let rect = PortalRect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        state.on_pointer_down(1, ResizeEdge::Right, 500.0, 250.0, rect, &bounds);
        assert_eq!(
            state.active_edge(),
            Some(ResizeEdge::Right),
            "active gesture exposes its edge for cursor pinning"
        );
        state.on_pointer_up(1, 510.0, 250.0, &bounds);
        assert_eq!(
            state.active_edge(),
            None,
            "edge clears once the gesture ends"
        );
    }
}
