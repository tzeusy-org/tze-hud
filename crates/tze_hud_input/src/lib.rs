//! # tze_hud_input
//!
//! Input pipeline for tze_hud. Processes pointer events, performs hit-testing,
//! updates local feedback state (hover/pressed/focused), and dispatches events
//! to agents. Local feedback happens synchronously in < 4ms — no agent roundtrip.
//!
//! ## Module structure
//!
//! - `lib.rs` — `InputProcessor`, pointer event processing, `AgentDispatch`
//! - `focus_tree` — per-tab focus tree data structure and history
//! - `focus`      — focus manager (lifecycle, cycling, events, ring metadata)
//! - `keyboard`   — keyboard event types and dispatch (KeyDownEvent, KeyUpEvent,
//!   CharacterEvent) per RFC 0004 §7.4
//! - `command`    — abstract command input model (NAVIGATE_NEXT … SCROLL_DOWN)
//!   per RFC 0004 §10
//! - [`composer_draft`] — runtime-owned bounded plain-text draft buffer for
//!   portal composer regions with local echo, caret/selection/word-delete/capped
//!   paste, coalescible state-stream notifications, and transactional submit
//!   (hud-5jbra.4)
//! - [`pointer`] — rich pointer event types (PointerDownEvent, ClickEvent, etc.)
//! - [`events`] — HitTestResult, RouteTarget, SceneLocalPatch, InputEnvelope, EventBatch
//! - [`hit_test`] — headless-testable hit-test pipeline
//! - `local_feedback` — `LocalFeedbackStyle`, `ResolvedFeedbackStyle`, rollback tracker
//! - `scroll` — `ScrollConfig`, `ScrollState`, scroll-local-first processing
//! - `capture`    — pointer capture manager (RFC 0004 §2)
//! - [`envelope`] — full 19-variant `InputEnvelope` for the batching/coalescing pipeline
//! - [`event_queue`] — per-agent event queue with backpressure and transactional protection
//! - [`coalescing`] — in-place queue coalescing and per-frame `FrameCoalescer`
//! - [`batching`] — `EventBatchAssembler` for per-frame batch assembly
//!
//! ## Pointer Capture
//!
//! The [`capture`] module implements RFC 0004 §2 pointer capture semantics:
//! - Only one node holds capture globally at a time, per device.
//! - Capture can be explicit (CaptureRequest) or automatic (auto_capture=true).
//! - While captured, all events from the captured device route to the owning
//!   node, bypassing normal hit-testing.
//! - Capture is released by: explicit CaptureReleaseRequest, PointerUpEvent
//!   (when release_on_up=true), or runtime theft (Alt+Tab, lease revocation,
//!   tab switch).

pub mod batching;
pub mod coalescing;
pub mod envelope;
pub mod event_queue;

pub use batching::EventBatchAssembler;
pub use coalescing::{CoalesceResult, EventCoalescer, FrameCoalescer};
pub use envelope::{
    CharacterData,
    ClickData,
    CommandInputData,
    FocusGainedData,
    FocusLostData,
    // CaptureReleasedData and CaptureReleasedReason are defined in lib.rs
    // (from the pointer capture module); re-exporting envelope:: versions
    // would create duplicate definitions.
    KeyDownData,
    KeyUpData,
    PointerCancelData,
    PointerDownData,
    PointerEnterData,
    PointerLeaveData,
    PointerMoveData,
    PointerUpData,
    ScrollOffsetChangedData,
};
pub use event_queue::{AgentEventQueue, DEFAULT_QUEUE_DEPTH, HARD_CAP_QUEUE_DEPTH};

pub mod capture;
pub mod events;
pub mod hit_test;
pub mod local_feedback;
pub mod pointer;
pub mod scroll;

pub use events::{
    HitTestResult, LocalStateUpdate, RouteTarget, SceneLocalPatch, ScrollOffsetUpdate,
};
pub use hit_test::hit_test;
pub use local_feedback::{
    DEFAULT_FOCUS_RING_COLOR, DEFAULT_FOCUS_RING_WIDTH_PX, DEFAULT_HOVER_TINT,
    DEFAULT_PRESS_DARKEN, LocalFeedbackStyle, ROLLBACK_ANIMATION_MS, ResolvedFeedbackStyle,
    RollbackTracker,
};
pub use pointer::{
    CancelReason, ClickEvent, ContextMenuEvent, DoubleClickEvent, Modifiers, PointerButton,
    PointerCancelEvent, PointerDownEvent, PointerEnterEvent, PointerFields, PointerLeaveEvent,
    PointerMoveEvent, PointerUpEvent, RawPointerEvent, RawPointerEventKind,
};
pub use scroll::{
    FollowTailAnchor, ScrollConfig, ScrollEvent, ScrollOffsetChangedEvent, ScrollState,
    SetScrollOffsetRequest, follow_tail_offset,
};

pub mod command;
pub mod composer_draft;
pub mod drag;
pub mod focus;
pub mod focus_tree;
pub mod jump_to_latest;
pub mod keyboard;
pub mod portal_resize;
pub mod scroll_indicator;

pub use command::{
    CommandAction, CommandDispatch, CommandInputEvent, CommandProcessor, CommandSource,
    RawCommandEvent,
};
pub use composer_draft::{
    ComposerDraft, ComposerDraftManager, ComposerInputBoxGeometry, ComposerVisualLayout,
    ComposerVisualLine, DEFAULT_DRAFT_CAP, DraftCancel, DraftNotificationBatch, DraftScheduler,
    DraftStateNotification, DraftSubmission, EditOutcome, MAX_DRAFT_BYTES, Selection,
};
pub use focus::{
    FocusGainedEvent, FocusLostEvent, FocusLostReason, FocusManager, FocusRequest, FocusResult,
    FocusRingBounds, FocusRingUpdate, FocusSource, FocusTransition,
};
pub use focus_tree::{FocusOwner, FocusTree};
pub use jump_to_latest::{
    JumpToLatestGeometry, JumpToLatestTokens, compute_jump_to_latest_pill,
    jump_to_latest_badge_label,
};
pub use keyboard::{
    KeyboardDispatch, KeyboardDispatchKind, KeyboardModifiers, KeyboardProcessor,
    RawCharacterEvent, RawKeyDownEvent, RawKeyUpEvent,
};
pub use portal_resize::{
    CursorIconTracker, DeviceResizeState, GeometrySnapshot, GestureAuthorityError,
    HotkeyResizeAxis, HotkeyResizeDir, HotkeyResizeOutcome, PortalCursor, PortalRect,
    PortalResizeState, PortalWindowTokens, ResizeBounds, ResizeEdge, ResizeOutcome, ResizePhase,
    ShellReservedShortcut, apply_hotkey_resize, apply_hotkey_resize_axis, hit_affordance,
    portal_hover_cursor,
};
pub use scroll_indicator::{
    ScrollIndicatorGeometry, ScrollIndicatorTokens, compute_scroll_indicator,
};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::{ElementStore, ElementType, HitResult, NodeData, SceneId, ZoneInteractionKind};

pub use drag::{
    DEFAULT_SNAP_GRID_PCT, DRAG_HIGHLIGHT_BORDER_PX, DRAG_OPACITY_BOOST, DRAG_Z_ORDER_BOOST,
    DeviceDragState, DragConfig, DragEventOutcome, DragPhase, LONG_PRESS_MOVEMENT_TOLERANCE_DP,
    LONG_PRESS_POINTER_THRESHOLD_MS, LONG_PRESS_TOUCH_THRESHOLD_MS,
};
pub use tze_hud_scene::DragHandleElementKind;

/// Scroll step (pixels) applied per PgUp or PgDn keypress on a portal tile.
///
/// This is the keyboard scroll step for the OS keyboard path wired in hud-6bbe.
/// One PgUp/PgDn keypress scrolls 160px — approximately four line-scroll steps
/// (cf. wheel: LineDelta * 40px). Callers may override this constant when
/// computing the `delta_y` passed to [`InputProcessor::process_keyboard_scroll`].
pub const KEYBOARD_PAGE_SCROLL_PX: f32 = 160.0;

/// Raw pointer input event from the OS.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PointerEvent {
    pub x: f32,
    pub y: f32,
    pub kind: PointerEventKind,
    /// Device identifier — differentiates mouse, touch points, stylus, etc.
    /// Defaults to 0 (primary pointer device).
    #[serde(default)]
    pub device_id: u32,
    /// Monotonic timestamp (microseconds since process start).
    #[serde(skip)]
    pub timestamp: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerEventKind {
    Move,
    Down,
    Up,
}

/// Result of processing a pointer event — what changed locally.
#[derive(Clone, Debug)]
pub struct InputResult {
    /// Full hit-test result for this event.
    pub hit: HitResult,
    /// The interaction_id of the hit region (if a HitRegionNode was hit).
    pub interaction_id: Option<String>,
    /// Whether this was an activation (press then release on the same hit region).
    pub activated: bool,
    /// Time taken for local acknowledgement (microseconds).
    pub local_ack_us: u64,
    /// Time taken for hit-test (microseconds).
    pub hit_test_us: u64,
    /// Primary agent dispatch descriptor, if an event should be forwarded to an agent.
    pub dispatch: Option<AgentDispatch>,
    /// Additional agent dispatches to deliver after `dispatch` (in order).
    ///
    /// Used when a single pointer event produces multiple protocol-level events —
    /// for example, `PointerUp` + `CaptureReleased` when `release_on_up=true`.
    /// Callers MUST deliver all entries in `extra_dispatches` in order after `dispatch`.
    pub extra_dispatches: Vec<AgentDispatch>,
    /// Local state patch to forward to the compositor immediately.
    ///
    /// Non-empty when pressed/hovered/focused state changed. This patch MUST be
    /// sent to the compositor via the dedicated local-patch channel before the
    /// next frame to guarantee `input_to_next_present p99 < 33ms`.
    pub local_patch: SceneLocalPatch,
}

/// Information needed to dispatch this input event to the owning agent.
///
/// Callers (e.g. the runtime kernel) use this to call into the protocol layer
/// without the input crate needing a direct dependency on tze_hud_protocol.
#[derive(Clone, Debug)]
pub struct AgentDispatch {
    /// Namespace (agent name) of the tile owner.
    pub namespace: String,
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub interaction_id: String,
    /// Pointer position in tile-local coordinates.
    pub local_x: f32,
    pub local_y: f32,
    /// Pointer position in display-space coordinates.
    pub display_x: f32,
    pub display_y: f32,
    /// Device that generated this event.
    pub device_id: u32,
    pub kind: AgentDispatchKind,
    /// Populated when `kind == CaptureReleased`.
    pub capture_released_reason: Option<CaptureReleasedReason>,
}

/// Which type of input event to deliver to the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentDispatchKind {
    PointerMove,
    PointerDown,
    PointerUp,
    PointerEnter,
    PointerLeave,
    /// Activation: press + release on the same hit region.
    Activated,
    /// Pointer capture was granted (response to CaptureRequest or auto-capture).
    CaptureGranted,
    /// Pointer capture request was denied (another node holds capture for this device).
    CaptureDenied,
    /// Pointer capture was released.  Carry `capture_released_reason` in AgentDispatch.
    CaptureReleased,
    /// Pointer interaction was cancelled (e.g., runtime capture theft via Alt+Tab).
    /// The agent MUST treat this as terminal — no further PointerUp/Activated expected.
    PointerCancel,
}

/// Why pointer capture was released.
///
/// Carried in `AgentDispatch` when `kind == AgentDispatchKind::CaptureReleased`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureReleasedReason {
    /// The owning agent sent an explicit CaptureReleaseRequest.
    AgentReleased,
    /// PointerUpEvent arrived while `release_on_up=true`.
    PointerUp,
    /// Runtime revoked capture unconditionally (Alt+Tab, system notification,
    /// tab switch, or other system event).
    RuntimeRevoked,
    /// Agent lease was revoked; capture is implicitly released.
    LeaseRevoked,
}

/// A request from an agent to acquire pointer capture.
///
/// The runtime evaluates this after a PointerDownEvent has been dispatched.
/// Capture is only granted in response to PointerDown (spec lines 104-106).
#[derive(Clone, Debug)]
pub struct CaptureRequest {
    /// The tile whose node is requesting capture.
    pub tile_id: SceneId,
    /// The node requesting capture.
    pub node_id: SceneId,
    /// The pointer device to capture.
    pub device_id: u32,
}

/// A request from an agent to release pointer capture.
#[derive(Clone, Debug)]
pub struct CaptureReleaseRequest {
    /// The device whose capture should be released.
    pub device_id: u32,
}

/// Classification of the currently focused element for portal keyboard
/// typing-recovery decisions (hud-2v8br).
///
/// A keyboard-only viewer must never be stranded: when Tab moves focus off the
/// composer onto a portal control (minimize / restore / submit), typed text must
/// still reach the composer instead of being silently dispatched to the agent as
/// raw key events. The runtime classifies the focus owner via
/// [`InputProcessor::classify_portal_focus`] and uses the result to route
/// keystrokes: printable typing and Escape recover to the composer, while
/// Enter / Space activate the focused control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortalFocusTarget {
    /// Focus rests on a composer-input region — normal editing, no recovery.
    Composer,
    /// Focus rests on a non-composer focusable control that shares a tile with a
    /// composer node. `composer_node` is the recovery target for typing / Escape;
    /// `node_id` / `interaction_id` identify the control for Enter / Space
    /// activation.
    Control {
        tile_id: SceneId,
        node_id: SceneId,
        interaction_id: String,
        /// The composer node in the same tile to redirect typing / Escape to.
        composer_node: SceneId,
    },
    /// Any other focus state (none, chrome, tile-level, or a focusable node whose
    /// tile has no composer) — typing-recovery does not apply.
    Other,
}

/// The input processor. Tracks state across events for local feedback.
pub struct InputProcessor {
    /// Currently hovered node.
    current_hover: Option<(SceneId, SceneId)>, // (tile_id, node_id)
    /// Currently pressed node.
    current_press: Option<(SceneId, SceneId)>, // (tile_id, node_id)
    /// Currently hovered chrome drag handle interaction id.
    current_drag_handle_hover: Option<String>,
    /// Currently pressed chrome drag handle interaction id.
    current_drag_handle_press: Option<String>,
    /// Per-device long-press drag state.
    ///
    /// Keyed by `device_id`.  Each entry is independent so multi-touch devices
    /// can have simultaneous independent drag interactions on different handles.
    pub drag_states: HashMap<u32, DeviceDragState>,
    /// Drag configuration (snap grid, etc.).
    pub drag_config: DragConfig,
    /// Rollback animation tracker (agent-rejection-triggered).
    rollback_tracker: RollbackTracker,
    /// Pointer capture manager.
    pub capture: capture::PointerCaptureManager,
    /// Local-first tile scroll state.
    scroll_state: ScrollState,
    /// Runtime-owned composer draft manager.
    ///
    /// Owns the active `ComposerDraft` when a region with
    /// `accepts_composer_input = true` is focused.  Wired into the focus
    /// transition path in [`Self::process_with_focus`] and driven by
    /// [`Self::route_key_down_to_composer`] / [`Self::route_character_to_composer`]
    /// on every keystroke while a composer region is focused.
    ///
    /// Spec: §4.1 — runtime-owned draft attached to focused composer regions.
    composer_draft_manager: ComposerDraftManager,
    /// Holds the batch flushed during a focus-lost transition (blur) so it can
    /// be drained on the next [`Self::try_flush_composer_draft`] call at the
    /// frame settle point.
    ///
    /// This bridge is necessary because `on_focus_lost` drains the scheduler
    /// immediately (blur is a settle point), but the caller's settle loop runs
    /// after `process_with_focus` returns.  Without this field the terminal
    /// draft state at blur would be permanently lost.
    ///
    /// Cleared on focus-gained so stale state never leaks across boundaries.
    pending_flushed_batch: Option<DraftNotificationBatch>,
}

impl InputProcessor {
    pub fn new() -> Self {
        Self {
            current_hover: None,
            current_press: None,
            current_drag_handle_hover: None,
            current_drag_handle_press: None,
            drag_states: HashMap::new(),
            drag_config: DragConfig::default(),
            rollback_tracker: RollbackTracker::new(),
            capture: capture::PointerCaptureManager::new(),
            scroll_state: ScrollState::new(),
            composer_draft_manager: ComposerDraftManager::new(),
            pending_flushed_batch: None,
        }
    }

    /// Queue an adapter-driven absolute scroll offset request.
    ///
    /// Requests are applied on `commit_scroll_updates`. If a user scroll for the
    /// same tile lands in the same frame, the user offset remains authoritative.
    pub fn queue_set_scroll_offset(&mut self, req: SetScrollOffsetRequest) {
        self.scroll_state.queue_agent_request(req);
    }

    /// Process a user-originated scroll event through the runtime-owned scroll path.
    ///
    /// Returns the changed tile and absolute offset when the event updates a
    /// scrollable tile. Returns `None` for unknown or unscrollable focus targets.
    pub fn process_scroll_event(
        &mut self,
        event: &ScrollEvent,
        scene: &mut SceneGraph,
    ) -> Option<ScrollOffsetChangedEvent> {
        let hit = scene.hit_test(event.x, event.y);
        let tile_id = match hit {
            HitResult::NodeHit { tile_id, .. } | HitResult::TileHit { tile_id } => tile_id,
            _ => return None,
        };
        self.process_scroll_for_tile(tile_id, event.delta_x, event.delta_y, scene)
    }

    /// Register `tile_id` in `scroll_state` from its scene `TileScrollConfig`
    /// (no-op if the scene has none). Entries are created lazily, so this is
    /// also where entries for destroyed tiles (the scene drops their config on
    /// destroy) are swept, keeping both registration paths leak-free.
    fn register_scroll_tile(&mut self, tile_id: SceneId, scene: &SceneGraph) {
        let Some(config) = scene.tile_scroll_config(tile_id) else {
            return;
        };
        self.scroll_state
            .retain_tiles(|id| scene.tile_scroll_config(id).is_some());
        self.scroll_state.register_tile(
            tile_id,
            ScrollConfig {
                scrollable_x: config.scrollable_x,
                scrollable_y: config.scrollable_y,
                content_width: config.content_width,
                content_height: config.content_height,
            },
        );
    }

    fn process_scroll_for_tile(
        &mut self,
        tile_id: SceneId,
        delta_x: f32,
        delta_y: f32,
        scene: &mut SceneGraph,
    ) -> Option<ScrollOffsetChangedEvent> {
        scene.tile_scroll_config(tile_id)?;
        if !self.scroll_state.is_scrollable(tile_id) {
            self.register_scroll_tile(tile_id, scene);
        }

        self.scroll_state
            .apply_user_scroll(tile_id, delta_x, delta_y)?;

        // Commit only this tile so local user feedback does not consume
        // pending adapter requests for other tiles.
        if !self.scroll_state.commit_tile_frame(tile_id) {
            return None;
        }
        let (offset_x, offset_y) = self.scroll_state.offset(tile_id);
        let _ = scene.set_tile_scroll_offset_local(tile_id, offset_x, offset_y);

        // Sync follow-tail anchor state to scene after user scroll.
        // A user scroll-back transitions AtTail → ScrolledBack, which must
        // be reflected in the scene so the compositor primes with HeadAnchored
        // truncation (spec §3.3 — viewport stability after scroll-back).
        let at_tail = self.scroll_state.follow_tail_anchor(tile_id)
            == crate::scroll::FollowTailAnchor::AtTail;
        scene.set_tile_follow_tail_at_tail(tile_id, at_tail);

        self.scroll_state.changed_event(tile_id)
    }

    /// Process a keyboard-originated scroll event through the runtime-owned scroll path.
    ///
    /// This is the command-input path for PgUp/PgDn on portal tiles. It applies
    /// the same local offset update as [`process_scroll_event`] but targets the
    /// tile resolved by keyboard focus, independent of pointer position.
    ///
    /// Returns the changed tile and absolute offset when the event updates a
    /// scrollable tile. Returns `None` for passthrough / unscrollable hits.
    ///
    /// # Local-first invariant
    ///
    /// The offset is applied synchronously in < 4ms p99 on the main thread
    /// without waiting for any agent response, matching the wheel-scroll contract.
    pub fn process_keyboard_scroll(
        &mut self,
        focused_tile_id: SceneId,
        delta_y: f32,
        scene: &mut SceneGraph,
    ) -> Option<ScrollOffsetChangedEvent> {
        self.process_scroll_for_tile(focused_tile_id, 0.0, delta_y, scene)
    }

    /// Notify the input processor that a scrollable tile's content has grown.
    ///
    /// This is the primary wiring point for portal/stream-text append events.
    /// It delegates to [`ScrollState::notify_content_appended`] to advance
    /// the scroll offset when the tile is at-tail (spec §3.2), or leave it
    /// unchanged when the user has scrolled back (spec §3.3).
    ///
    /// After updating the scroll offset, the method syncs the tile's
    /// follow-tail anchor state into the scene so `prime_truncation_cache`
    /// can select `TailAnchored` vs `HeadAnchored` truncation correctly.
    ///
    /// # Parameters
    ///
    /// - `tile_id`: the tile whose content grew.
    /// - `new_content_height_px`: **total** content height in physical pixels
    ///   (NOT the max-scroll-offset; those differ by viewport height).
    /// - `viewport_height_px`: the tile's visible viewport height in physical pixels.
    /// - `line_height_px`: logical line height (used to snap advancement to whole lines).
    ///
    /// # Returns
    ///
    /// `true` if the scroll offset changed (i.e. the tile was at-tail and
    /// advanced), `false` if the viewport was stable (ScrolledBack or no
    /// change).
    pub fn notify_tile_content_appended(
        &mut self,
        tile_id: SceneId,
        new_content_height_px: f32,
        viewport_height_px: f32,
        line_height_px: f32,
        scene: &mut SceneGraph,
    ) -> bool {
        // Auto-register the tile in scroll_state if it has a TileScrollConfig in
        // the scene but has not yet been registered (e.g. first append before the
        // user has scrolled).  This mirrors the auto-registration in
        // `process_scroll_event`.
        if !self.scroll_state.is_scrollable(tile_id) {
            // content_height starts at 0 (total pixels); the caller's
            // new_content_height_px drives the first update below.
            self.register_scroll_tile(tile_id, scene);
        }

        let changed = self.scroll_state.notify_content_appended(
            tile_id,
            new_content_height_px,
            viewport_height_px,
            line_height_px,
        );

        if changed {
            // Apply the updated offset to the scene for compositor use.
            let (offset_x, offset_y) = self.scroll_state.offset(tile_id);
            let _ = scene.set_tile_scroll_offset_local(tile_id, offset_x, offset_y);
        }

        // Sync follow-tail anchor to the scene, but only if the tile is actually
        // registered as scrollable.  Calling this for a non-scrollable tile would
        // wrongly force tile_follow_tail_at_tail = true (the default for unregistered
        // ScrollState entries) and switch its ellipsis truncation to TailAnchored.
        if self.scroll_state.is_scrollable(tile_id) {
            let at_tail = self.scroll_state.follow_tail_anchor(tile_id)
                == crate::scroll::FollowTailAnchor::AtTail;
            scene.set_tile_follow_tail_at_tail(tile_id, at_tail);
        }

        changed
    }

    /// Return a tile's current total content height in physical pixels (see
    /// [`ScrollState::total_content_height_px`]), or `0.0` if the tile has no
    /// tracked scroll state yet.
    ///
    /// Lets a caller that appends content incrementally (e.g. one runtime
    /// -authored history entry at a time) read back the running total and add
    /// its own delta before re-calling [`Self::notify_tile_content_appended`],
    /// rather than maintaining a duplicate counter (hud-qbcp8).
    pub fn tile_total_content_height_px(&self, tile_id: SceneId) -> f32 {
        self.scroll_state.total_content_height_px(tile_id)
    }

    /// Force a scrollable tile's viewport back to the tail, overriding any
    /// `ScrolledBack` anchor, and sync the result to the scene.
    ///
    /// Unlike [`Self::notify_tile_content_appended`], which deliberately
    /// leaves a `ScrolledBack` viewport undisturbed when ordinary content
    /// grows (spec §3.3), this is for a deliberate LOCAL action — the viewer
    /// typing into or submitting their own composer — where staying scrolled
    /// away from the tail would strand them from their own live input
    /// (hud-qbcp8). No-op if the tile has no tracked scroll state.
    pub fn reset_tile_scroll_to_tail(&mut self, tile_id: SceneId, scene: &mut SceneGraph) -> bool {
        let changed = self.scroll_state.reset_to_tail(tile_id);
        if changed {
            let (offset_x, offset_y) = self.scroll_state.offset(tile_id);
            let _ = scene.set_tile_scroll_offset_local(tile_id, offset_x, offset_y);
            scene.set_tile_follow_tail_at_tail(tile_id, true);
        }
        changed
    }

    /// Notify a tile that leading (head) content has been removed.
    ///
    /// Called when the transcript head is trimmed — either by the 64 KiB
    /// `PortalCadenceCoalescer` payload cap or the 16 KiB `visible_transcript_window`
    /// cap — so that a scrolled-back viewport stays visually stable rather than
    /// jumping into the gap left by the removed content (spec §3.3 / hud-pkg2g).
    ///
    /// Delegates to [`ScrollState::notify_head_content_removed`].
    ///
    /// # Parameters
    ///
    /// - `tile_id` — the tile whose scroll state should be adjusted.
    /// - `removed_height_px` — height (physical pixels) of the content dropped
    ///   from the head.  Non-positive or non-finite values are ignored.
    ///
    /// # Returns
    ///
    /// `true` if `offset_y` changed (tile was `ScrolledBack` and offset moved),
    /// `false` otherwise (tile was `AtTail` or no registered scroll state).
    ///
    /// Note: this method does NOT propagate the updated offset to the scene graph
    /// because head-trim notifications are always paired with a subsequent
    /// `notify_tile_content_appended` call that will sync the offset at that point.
    /// The caller is responsible for order: call this BEFORE
    /// `notify_tile_content_appended` so that `ScrollTileState` content-height
    /// fields are correct when the follow-tail bound is recomputed.
    pub fn notify_head_content_removed(
        &mut self,
        tile_id: SceneId,
        removed_height_px: f32,
    ) -> bool {
        self.scroll_state
            .notify_head_content_removed(tile_id, removed_height_px)
    }

    /// Commit queued adapter-driven scroll requests and apply local offsets.
    pub fn commit_scroll_updates(
        &mut self,
        scene: &mut SceneGraph,
    ) -> Vec<ScrollOffsetChangedEvent> {
        let changed = self.scroll_state.commit_all_frames();
        let mut events = Vec::with_capacity(changed.len());
        for tile_id in changed {
            let (offset_x, offset_y) = self.scroll_state.offset(tile_id);
            let _ = scene.set_tile_scroll_offset_local(tile_id, offset_x, offset_y);
            if let Some(ev) = self.scroll_state.changed_event(tile_id) {
                events.push(ev);
            }
        }
        events
    }

    /// Apply an agent rejection for an in-progress interaction.
    ///
    /// Spec §Local Feedback Rollback: "If an agent explicitly rejects an
    /// interaction, the local feedback SHALL be reverted with a 100ms reverse
    /// animation. Rollback SHALL only occur on explicit agent rejection, not on
    /// agent latency or silence."
    ///
    /// Returns a `SceneLocalPatch` containing the rollback state update to
    /// forward to the compositor. The compositor drives the 100ms animation.
    pub fn apply_agent_rejection(
        &mut self,
        node_id: SceneId,
        scene: &mut SceneGraph,
    ) -> SceneLocalPatch {
        // Clear pressed state in the scene graph
        if let Some(state) = scene.hit_region_states.get_mut(&node_id) {
            state.pressed = false;
        }
        // Clear current_press if this node was being tracked
        if let Some((_, pressed_node)) = self.current_press {
            if pressed_node == node_id {
                self.current_press = None;
            }
        }
        // Begin rollback animation tracking
        self.rollback_tracker.begin_rollback(node_id);

        // Produce rollback patch for compositor
        let mut patch = SceneLocalPatch::new();
        patch.push_state(
            LocalStateUpdate::new(node_id)
                .with_pressed(false)
                .with_rollback(),
        );
        patch
    }

    /// Returns a reference to the rollback tracker (e.g. for compositor queries).
    pub fn rollback_tracker(&self) -> &RollbackTracker {
        &self.rollback_tracker
    }

    /// Process a pointer event against the scene graph, applying click-to-focus.
    ///
    /// Updates hit-region local state for immediate visual feedback.
    /// Returns the result including timing measurements and an optional
    /// `AgentDispatch` descriptor for forwarding the event to the owning agent.
    ///
    /// In this variant, click-to-focus is applied **before** the pointer
    /// event is forwarded to the agent, using the provided `focus_manager`
    /// and `tab_id`, per spec §1.2 (lines 27-29). The returned
    /// `FocusTransition` (if any) must be dispatched to agents before the
    /// `AgentDispatch` payload.
    pub fn process_with_focus(
        &mut self,
        event: &PointerEvent,
        scene: &mut SceneGraph,
        focus_manager: &mut FocusManager,
        tab_id: SceneId,
    ) -> (InputResult, Option<FocusTransition>) {
        let focus_transition = if event.kind == PointerEventKind::Down {
            let hit = scene.hit_test(event.x, event.y);
            if let HitResult::NodeHit {
                tile_id, node_id, ..
            } = hit
            {
                let transition = focus_manager.on_click(tab_id, tile_id, Some(node_id), scene);
                self.apply_focus_transition_side_effects(&transition, scene);
                if transition.gained.is_some() || transition.lost.is_some() {
                    Some(transition)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let result = self.process(event, scene);
        (result, focus_transition)
    }

    /// Apply the local-state side effects of a [`FocusTransition`] to the scene
    /// and composer draft manager.
    ///
    /// Shared by the pointer click path ([`Self::process_with_focus`]) and the
    /// keyboard focus-traversal path ([`Self::navigate_focus`]) so both routes
    /// keep `hit_region_states.focused` and composer activation/flush in sync.
    ///
    /// - The node that lost focus has its `focused` flag cleared; if it was the
    ///   active composer region, `on_focus_lost` flushes any pending draft state
    ///   (blur is a settle point) into `pending_flushed_batch` for the next
    ///   `try_flush_composer_draft`.
    /// - The node that gained focus has its `focused` flag set; if it accepts
    ///   composer input, the draft manager is activated via `on_focus_gained`
    ///   (`suspended = false`; safe-mode governance is applied separately), and
    ///   the node's `composer_placeholder` config (if any) is seeded into the
    ///   manager via `set_focused_placeholder` so `composer_draft_snapshot`
    ///   can report a per-composer override (hud-se6hs).
    fn apply_focus_transition_side_effects(
        &mut self,
        transition: &FocusTransition,
        scene: &mut SceneGraph,
    ) {
        if let Some((lost_ev, _)) = &transition.lost {
            if let Some(lost_node_id) = lost_ev.node_id {
                if let Some(state) = scene.hit_region_states.get_mut(&lost_node_id) {
                    state.focused = false;
                }
                if self.composer_draft_manager.focused_node() == Some(lost_node_id) {
                    self.pending_flushed_batch = self.composer_draft_manager.on_focus_lost();
                }
            }
        }
        if let Some((gained_ev, _)) = &transition.gained {
            if let Some(gained_node_id) = gained_ev.node_id {
                if let Some(state) = scene.hit_region_states.get_mut(&gained_node_id) {
                    state.focused = true;
                }
                if node_accepts_composer_input(scene, gained_node_id) {
                    self.composer_draft_manager
                        .on_focus_gained(gained_node_id, false);
                    self.composer_draft_manager
                        .set_focused_placeholder(node_composer_placeholder(scene, gained_node_id));
                }
            }
        }
    }

    /// Advance keyboard focus to the next (`reverse = false`) or previous
    /// (`reverse = true`) focusable affordance and synchronize composer +
    /// hit-region-focus side effects.
    ///
    /// This is the no-pointer analogue of [`Self::process_with_focus`]: it drives
    /// the already-implemented focus cycling
    /// ([`FocusManager::navigate_next`] / [`FocusManager::navigate_prev`],
    /// RFC 0004 §1.3) and then applies the same composer-activation /
    /// hit-region-focus bookkeeping the pointer click path applies, so the
    /// composer (and any focusable region) is reachable and editable without a
    /// pointer — e.g. on smart glasses / a Mobile Presence Node.
    ///
    /// Returns the resulting [`FocusTransition`]; the caller broadcasts its
    /// `FocusGained`/`FocusLost` events over the focus-events channel exactly as
    /// the pointer path does.
    ///
    /// Spec: `portal-composer-interaction-completeness` — "Transcript Interaction
    /// Contract", scenario "composer is focusable without a pointer".
    pub fn navigate_focus(
        &mut self,
        focus_manager: &mut FocusManager,
        scene: &mut SceneGraph,
        tab_id: SceneId,
        reverse: bool,
    ) -> FocusTransition {
        let transition = if reverse {
            focus_manager.navigate_prev(tab_id, scene)
        } else {
            focus_manager.navigate_next(tab_id, scene)
        };
        self.apply_focus_transition_side_effects(&transition, scene);
        transition
    }

    /// Classify the current keyboard-focus `owner` for portal typing-recovery
    /// (hud-2v8br).
    ///
    /// - [`PortalFocusTarget::Composer`] when the focused node accepts composer
    ///   input (normal editing).
    /// - [`PortalFocusTarget::Control`] when the focused node is a *non-composer*
    ///   focusable `HitRegionNode` whose owning tile also contains a composer
    ///   node — i.e. a portal control (minimize / restore / submit) from which a
    ///   keyboard user must be able to recover to the input box. Carries the
    ///   sibling composer node as the recovery target.
    /// - [`PortalFocusTarget::Other`] otherwise (no focus, chrome, tile-level, or
    ///   a focusable node whose tile has no composer).
    ///
    /// This is a pure query — it does not mutate focus. The caller decides how to
    /// route the keystroke based on the result.
    pub fn classify_portal_focus(
        &self,
        owner: &FocusOwner,
        scene: &SceneGraph,
    ) -> PortalFocusTarget {
        let FocusOwner::Node { tile_id, node_id } = owner else {
            return PortalFocusTarget::Other;
        };
        let Some(node) = scene.nodes.get(node_id) else {
            return PortalFocusTarget::Other;
        };
        let NodeData::HitRegion(hr) = &node.data else {
            return PortalFocusTarget::Other;
        };
        if hr.accepts_composer_input {
            return PortalFocusTarget::Composer;
        }
        if !hr.accepts_focus {
            return PortalFocusTarget::Other;
        }
        // A non-composer focusable control: only a recovery target when the tile
        // actually has a composer to redirect typing into.
        let Some(tile) = scene.tiles.get(tile_id) else {
            return PortalFocusTarget::Other;
        };
        match find_composer_node(tile.root_node, &scene.nodes) {
            Some(composer_node) => PortalFocusTarget::Control {
                tile_id: *tile_id,
                node_id: *node_id,
                interaction_id: hr.interaction_id.clone(),
                composer_node,
            },
            None => PortalFocusTarget::Other,
        }
    }

    /// Redirect keyboard focus to a tile's composer node (typing-recovery,
    /// hud-2v8br) and apply the composer-activation side effects.
    ///
    /// Called when the user types (or presses Escape) while a non-composer portal
    /// control holds focus, so the keystroke lands in the input box instead of
    /// being swallowed. After this returns, [`Self::is_composer_active`] is `true`
    /// and the caller may route the triggering character into the draft via the
    /// normal composer path.
    ///
    /// Returns the [`FocusTransition`] so the caller broadcasts gained/lost events
    /// exactly as the Tab / click paths do.
    pub fn recover_composer_focus(
        &mut self,
        focus_manager: &mut FocusManager,
        scene: &mut SceneGraph,
        tab_id: SceneId,
        tile_id: SceneId,
        composer_node: SceneId,
    ) -> FocusTransition {
        let transition =
            focus_manager.focus_node_via_command(tab_id, tile_id, composer_node, scene);
        self.apply_focus_transition_side_effects(&transition, scene);
        transition
    }

    /// Route a raw key-down event to the composer draft manager if a composer
    /// region is focused.
    ///
    /// Returns `(consumed, Option<DraftNotificationBatch>)`.
    /// - `consumed = true` means the keystroke was handled by the draft buffer
    ///   and MUST NOT be forwarded to the agent as a raw `KeyDownEvent`.
    /// - The returned `DraftNotificationBatch` is `Some` only on transactional
    ///   events (submit / cancel); the normal coalesced delivery path is driven
    ///   by [`Self::try_flush_composer_draft`] at the frame settle point.
    ///
    /// If no composer region is focused, returns `(false, None)` and the caller
    /// SHOULD forward the event to the agent via the normal keyboard path.
    ///
    /// Spec: §4.2, §4.4 (editing keystrokes are never forwarded as raw key events).
    pub fn route_key_down_to_composer(
        &mut self,
        key_code: &str,
        key: &str,
        shift: bool,
        ctrl: bool,
        alt: bool,
    ) -> (bool, Option<DraftNotificationBatch>) {
        self.composer_draft_manager
            .route_key_down(key_code, key, shift, ctrl, alt)
    }

    /// Update the composer's wrapped-line layout for visual-row vertical caret
    /// movement (hud-21o6x). The runtime forwards the compositor's latest layout
    /// here before dispatching an ArrowUp/ArrowDown; `None` reverts to
    /// hard-newline vertical movement.
    pub fn set_composer_visual_layout(&mut self, layout: Option<ComposerVisualLayout>) {
        self.composer_draft_manager.set_visual_layout(layout);
    }

    /// Route a post-IME character event to the composer draft manager if a
    /// composer region is focused.
    ///
    /// Returns `(EditOutcome, Option<DraftNotificationBatch>)`.
    /// When the manager is active (`accepts_composer_input` region focused),
    /// the character is inserted into the draft buffer and MUST NOT be forwarded
    /// to the agent as a raw `CharacterEvent`.
    ///
    /// If no composer region is focused, returns `(EditOutcome::Unchanged, None)`
    /// and the caller SHOULD forward the character to the agent.
    ///
    /// Spec: §4.1 — keystroke routing into the runtime-owned draft buffer.
    pub fn route_character_to_composer(
        &mut self,
        character: &str,
    ) -> (EditOutcome, Option<DraftNotificationBatch>) {
        self.composer_draft_manager.route_character(character)
    }

    /// Route a pointer-down event to the composer draft manager to position
    /// the cursor in the active draft buffer.
    ///
    /// `anchor` and `cursor` are byte offsets into the draft text computed by
    /// the caller from the pointer's tile-local position.  Both are clamped
    /// and snapped to UTF-8 boundaries inside `ComposerDraft::set_pointer_selection`.
    ///
    /// Returns `EditOutcome::Unchanged` when no composer region is focused.
    ///
    /// Spec: §4.1 — pointer-driven cursor positioning (hud-083az).
    pub fn route_pointer_selection_to_composer(
        &mut self,
        anchor: usize,
        cursor: usize,
    ) -> EditOutcome {
        self.composer_draft_manager
            .route_pointer_selection(anchor, cursor)
    }

    /// Flush pending coalesced draft notifications at a frame settle point.
    ///
    /// Must be called once per frame (or per settle window) to guarantee the
    /// terminal draft state is delivered to downstream consumers.  The
    /// `DraftScheduler` inside `ComposerDraftManager` coalesces rapid keystrokes
    /// into a single latest-snapshot; this call forces delivery of any pending
    /// coalesced state.
    ///
    /// Also drains the `pending_flushed_batch` produced by a blur transition
    /// (stored by `process_with_focus` when focus leaves a composer region).
    /// The two batches are merged: if the manager's batch contains a cancel, any
    /// `latest`/`submission` in the pending batch are cleared to avoid delivering
    /// contradictory state alongside a cancel event.
    ///
    /// Returns `Some(batch)` when there is pending state to deliver, `None` when
    /// the draft has been idle since the last flush (no-op coalescing window).
    ///
    /// Spec: §4.3 — flush guarantee for the coalesced state-stream delivery.
    pub fn try_flush_composer_draft(&mut self) -> Option<DraftNotificationBatch> {
        let manager_batch = self.composer_draft_manager.try_flush();
        match (self.pending_flushed_batch.take(), manager_batch) {
            (Some(mut pending), Some(manager)) => {
                // A cancel from the manager supersedes any accumulated latest/submission
                // in the pending (blur) batch; discard those to avoid contradictory state.
                if let Some(cancel) = manager.cancel {
                    pending.latest = None;
                    pending.submission = None;
                    pending.cancel = Some(cancel);
                } else {
                    if let Some(latest) = manager.latest {
                        pending.latest = Some(latest);
                    }
                    if let Some(sub) = manager.submission {
                        pending.submission = Some(sub);
                    }
                }
                Some(pending)
            }
            (Some(pending), None) => Some(pending),
            (None, manager_batch) => manager_batch,
        }
    }

    /// Returns `true` when a composer region is currently focused (draft active).
    pub fn is_composer_active(&self) -> bool {
        self.composer_draft_manager.is_active()
    }

    /// Returns the `SceneId` of the currently focused composer node, if any.
    ///
    /// `None` when no composer region is focused.  Use this to resolve the
    /// owning tile/namespace for outbound proto delivery.
    pub fn composer_focused_node(&self) -> Option<tze_hud_scene::SceneId> {
        self.composer_draft_manager.focused_node()
    }

    /// The focused composer's currently selected draft text, or `None` when no
    /// composer is focused or the selection is empty.
    ///
    /// The runtime uses this to snapshot the selection for the OS clipboard on
    /// Ctrl+C / Ctrl+X BEFORE routing the keystroke (a cut mutates the buffer, so
    /// the copy must be read first). Kept out of the input layer because clipboard
    /// access is an OS concern owned by the windowed runtime.
    pub fn composer_selected_text(&self) -> Option<String> {
        let draft = self.composer_draft_manager.draft()?;
        if !draft.has_selection() {
            return None;
        }
        Some(draft.selected_text().to_string())
    }

    /// Snapshot current composer draft state for local echo rendering.
    ///
    /// Returns
    /// `(text, cursor_byte, selection_anchor, at_capacity, focused_node_id, placeholder_override)`
    /// when a composer region is focused, `None` otherwise.
    ///
    /// `selection_anchor` equals `cursor_byte` when no selection is active.
    /// When they differ, the selected region spans
    /// `[min(cursor_byte, selection_anchor), max(cursor_byte, selection_anchor)]`
    /// as byte offsets into `text`.
    ///
    /// `placeholder_override` mirrors `HitRegionNode::composer_placeholder`'s
    /// three-state convention, resolved for the focused node at focus-gain
    /// time: `None` — no per-composer override configured (caller should fall
    /// back to its own default hint); `Some("")` — explicit opt-out; `Some(text)`
    /// — this composer's own hint copy (hud-se6hs, follow-up to hud-evk0j).
    ///
    /// Used by the windowed runtime to push a [`LocalComposerState`]-equivalent
    /// snapshot to the compositor thread after every composer mutation.
    ///
    /// Spec: §4.1 local feedback first — the snapshot is pushed WITHOUT an
    /// adapter round-trip so the compositor can render the echo on the next
    /// frame.
    pub fn composer_draft_snapshot(
        &self,
    ) -> Option<(
        String,
        usize,
        usize,
        bool,
        tze_hud_scene::SceneId,
        Option<String>,
    )> {
        let node_id = self.composer_draft_manager.focused_node()?;
        let draft = self.composer_draft_manager.draft()?;
        Some((
            draft.text().to_owned(),
            draft.cursor(),
            draft.selection_anchor(),
            draft.is_at_capacity(),
            node_id,
            self.composer_draft_manager.focused_placeholder(),
        ))
    }

    /// Process a pointer event against the scene graph.
    ///
    /// Updates hit-region local state for immediate visual feedback.
    /// Returns the result including timing measurements, an optional
    /// `AgentDispatch` descriptor for forwarding the event to the owning agent,
    /// and a `SceneLocalPatch` to forward to the compositor immediately.
    ///
    /// ## Local feedback contract
    /// The `local_patch` in the returned `InputResult` MUST be forwarded to the
    /// compositor via the dedicated local-patch channel before the next frame.
    /// This ensures `input_to_next_present p99 < 33ms` and satisfies the
    /// doctrinal guarantee: "Local feedback first."
    ///
    /// ## Capture Routing
    ///
    /// If a node holds pointer capture for `event.device_id`, all pointer events
    /// from that device are routed to the capturing node, bypassing normal
    /// hit-testing.  (Spec lines 104-106, 110-111)
    pub fn process(&mut self, event: &PointerEvent, scene: &mut SceneGraph) -> InputResult {
        let start = Instant::now();

        let device_id = event.device_id;

        // ── Capture check: does this device have an active capture? ───────
        // If so, bypass hit-test and route to the capturing node.
        if let Some(capture_state) = self.capture.get(device_id) {
            let (cap_tile_id, cap_node_id) = (capture_state.tile_id, capture_state.node_id);
            let release_on_up = capture_state.release_on_up;

            // Resolve metadata for the capturing node
            let (captured_namespace, captured_interaction_id) =
                resolve_node_meta(scene, cap_tile_id, cap_node_id);
            // Pre-extract interaction_id string before the match may consume it.
            let cap_interaction_id_str = captured_interaction_id.clone().unwrap_or_default();

            let (local_x, local_y) = display_to_local(scene, cap_tile_id, event.x, event.y);
            let hit_test_us = 0; // capture bypasses hit-test

            let mut dispatches: Vec<AgentDispatch> = Vec::new();
            let mut capture_local_patch = SceneLocalPatch::new();

            match event.kind {
                PointerEventKind::Move => {
                    if let Some(ns) = captured_namespace {
                        dispatches.push(AgentDispatch {
                            namespace: ns,
                            tile_id: cap_tile_id,
                            node_id: cap_node_id,
                            interaction_id: captured_interaction_id.unwrap_or_default(),
                            local_x,
                            local_y,
                            display_x: event.x,
                            display_y: event.y,
                            device_id,
                            kind: AgentDispatchKind::PointerMove,
                            capture_released_reason: None,
                        });
                    }
                }
                PointerEventKind::Up => {
                    if release_on_up {
                        // Release capture first, then deliver PointerUp, then CaptureReleased
                        self.capture.release(device_id);

                        if let Some(state) = scene.hit_region_states.get_mut(&cap_node_id) {
                            state.pressed = false;
                        }
                        self.current_press = None;
                        // Emit pressed=false in local patch so compositor updates immediately
                        capture_local_patch
                            .push_state(LocalStateUpdate::new(cap_node_id).with_pressed(false));

                        if let Some(ns) = &captured_namespace {
                            // PointerUp
                            dispatches.push(AgentDispatch {
                                namespace: ns.clone(),
                                tile_id: cap_tile_id,
                                node_id: cap_node_id,
                                interaction_id: captured_interaction_id.clone().unwrap_or_default(),
                                local_x,
                                local_y,
                                display_x: event.x,
                                display_y: event.y,
                                device_id,
                                kind: AgentDispatchKind::PointerUp,
                                capture_released_reason: None,
                            });
                            // CaptureReleased(reason=POINTER_UP)
                            dispatches.push(AgentDispatch {
                                namespace: ns.clone(),
                                tile_id: cap_tile_id,
                                node_id: cap_node_id,
                                interaction_id: captured_interaction_id.unwrap_or_default(),
                                local_x,
                                local_y,
                                display_x: event.x,
                                display_y: event.y,
                                device_id,
                                kind: AgentDispatchKind::CaptureReleased,
                                capture_released_reason: Some(CaptureReleasedReason::PointerUp),
                            });
                        }
                    } else {
                        // Keep capture; just deliver PointerUp
                        if let Some(state) = scene.hit_region_states.get_mut(&cap_node_id) {
                            state.pressed = false;
                        }
                        self.current_press = None;
                        // Emit pressed=false in local patch
                        capture_local_patch
                            .push_state(LocalStateUpdate::new(cap_node_id).with_pressed(false));

                        if let Some(ns) = captured_namespace {
                            dispatches.push(AgentDispatch {
                                namespace: ns,
                                tile_id: cap_tile_id,
                                node_id: cap_node_id,
                                interaction_id: captured_interaction_id.unwrap_or_default(),
                                local_x,
                                local_y,
                                display_x: event.x,
                                display_y: event.y,
                                device_id,
                                kind: AgentDispatchKind::PointerUp,
                                capture_released_reason: None,
                            });
                        }
                    }
                }
                PointerEventKind::Down => {
                    // Capture already active — still deliver PointerDown to capturing node
                    if let Some(state) = scene.hit_region_states.get_mut(&cap_node_id) {
                        state.pressed = true;
                    }
                    // Emit pressed=true in local patch (mirrors non-capture path)
                    capture_local_patch
                        .push_state(LocalStateUpdate::new(cap_node_id).with_pressed(true));
                    if let Some(ns) = captured_namespace {
                        dispatches.push(AgentDispatch {
                            namespace: ns,
                            tile_id: cap_tile_id,
                            node_id: cap_node_id,
                            interaction_id: captured_interaction_id.unwrap_or_default(),
                            local_x,
                            local_y,
                            display_x: event.x,
                            display_y: event.y,
                            device_id,
                            kind: AgentDispatchKind::PointerDown,
                            capture_released_reason: None,
                        });
                    }
                }
            }

            let local_ack_us = start.elapsed().as_micros() as u64;
            let mut dispatches_iter = dispatches.into_iter();
            let dispatch = dispatches_iter.next();
            let extra_dispatches: Vec<AgentDispatch> = dispatches_iter.collect();
            let interaction_id_for_result = if cap_interaction_id_str.is_empty() {
                None
            } else {
                Some(cap_interaction_id_str.clone())
            };

            return InputResult {
                hit: HitResult::NodeHit {
                    tile_id: cap_tile_id,
                    node_id: cap_node_id,
                    interaction_id: cap_interaction_id_str,
                },
                interaction_id: interaction_id_for_result,
                activated: false,
                local_ack_us,
                hit_test_us,
                dispatch,
                extra_dispatches,
                local_patch: capture_local_patch,
            };
        }

        // ── Stage 2: Hit test ─────────────────────────────────────────────
        let hit_start = Instant::now();
        let hit = scene.hit_test(event.x, event.y);
        let hit_test_us = hit_start.elapsed().as_micros() as u64;

        let mut interaction_id: Option<String> = None;
        let mut activated = false;
        let mut dispatch: Option<AgentDispatch> = None;
        // Extra dispatches produced by a single pointer event (e.g. PointerLeave + PointerEnter
        // on a hover transition, or PointerUp + CaptureReleased).  Callers MUST deliver these
        // in order after `dispatch`.
        let mut extra_dispatches: Vec<AgentDispatch> = Vec::new();
        // Accumulate local state changes for the SceneLocalPatch
        let mut local_patch = SceneLocalPatch::new();

        // Decompose HitResult into (tile_id, node_id) where applicable.
        let (hit_tile_id, hit_node_id): (Option<SceneId>, Option<SceneId>) = match &hit {
            HitResult::NodeHit {
                tile_id,
                node_id,
                interaction_id: iid,
            } => {
                interaction_id = Some(iid.clone());
                (Some(*tile_id), Some(*node_id))
            }
            HitResult::TileHit { tile_id } => (Some(*tile_id), None),
            HitResult::Passthrough => (None, None),
            // ZoneInteraction hits are handled by the zone interaction layer,
            // not by the tile/node dispatch path.  No tile or node ID is associated.
            HitResult::ZoneInteraction {
                interaction_id: iid,
                ..
            } => {
                interaction_id = Some(iid.clone());
                (None, None)
            }
        };
        let hit_drag_handle_id: Option<String> = match &hit {
            HitResult::ZoneInteraction {
                interaction_id: iid,
                kind: ZoneInteractionKind::DragHandle { .. },
                ..
            } => Some(iid.clone()),
            _ => None,
        };

        // ── Stage 2: Update hover state ───────────────────────────────────
        let prev_hover_node = self.current_hover.map(|(_, n)| n);
        let new_hover_node = hit_node_id;

        if prev_hover_node != new_hover_node {
            // Un-hover the old node
            if let Some(old_id) = prev_hover_node {
                if let Some(state) = scene.hit_region_states.get_mut(&old_id) {
                    state.hovered = false;
                }
                // Emit local patch for hover-off
                local_patch.push_state(LocalStateUpdate::new(old_id).with_hovered(false));
                // Dispatch pointer_leave to previous owning agent
                if let Some((old_tile_id, _)) = self.current_hover {
                    if let Some(namespace) = tile_namespace(scene, old_tile_id) {
                        let leave_interaction_id = scene
                            .nodes
                            .get(&old_id)
                            .and_then(|n| {
                                if let NodeData::HitRegion(hr) = &n.data {
                                    Some(hr.interaction_id.clone())
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        dispatch = Some(AgentDispatch {
                            namespace,
                            tile_id: old_tile_id,
                            node_id: old_id,
                            interaction_id: leave_interaction_id,
                            local_x: 0.0,
                            local_y: 0.0,
                            display_x: event.x,
                            display_y: event.y,
                            device_id,
                            kind: AgentDispatchKind::PointerLeave,
                            capture_released_reason: None,
                        });
                    }
                }
            }
            // Hover the new node
            if let Some(new_id) = new_hover_node {
                if let Some(state) = scene.hit_region_states.get_mut(&new_id) {
                    state.hovered = true;
                }
                // Emit local patch for hover-on
                local_patch.push_state(LocalStateUpdate::new(new_id).with_hovered(true));
                // Dispatch pointer_enter to new owning agent.  If dispatch already holds a
                // PointerLeave for the old agent, preserve it — the enter goes into
                // extra_dispatches so both events are delivered in order (leave first).
                if let Some(tile_id) = hit_tile_id {
                    if let Some(namespace) = tile_namespace(scene, tile_id) {
                        let (local_x, local_y) = display_to_local(scene, tile_id, event.x, event.y);
                        let enter = AgentDispatch {
                            namespace,
                            tile_id,
                            node_id: new_id,
                            interaction_id: interaction_id.clone().unwrap_or_default(),
                            local_x,
                            local_y,
                            display_x: event.x,
                            display_y: event.y,
                            device_id,
                            kind: AgentDispatchKind::PointerEnter,
                            capture_released_reason: None,
                        };
                        if dispatch.is_some() {
                            // PointerLeave already occupies the primary slot; queue enter after.
                            extra_dispatches.push(enter);
                        } else {
                            dispatch = Some(enter);
                        }
                    }
                }
            }
            self.current_hover = hit_tile_id.and_then(|t| hit_node_id.map(|n| (t, n)));
        }

        // ── Stage 2: Update chrome drag-handle hover state ───────────────
        if self.current_drag_handle_hover != hit_drag_handle_id {
            if let Some(old) = self.current_drag_handle_hover.take() {
                scene.set_drag_handle_hovered(&old, false);
            }
            if let Some(new_id) = &hit_drag_handle_id {
                scene.set_drag_handle_hovered(new_id, true);
            }
            self.current_drag_handle_hover = hit_drag_handle_id.clone();
        }

        // ── Stage 2: Handle press/release ─────────────────────────────────
        match event.kind {
            PointerEventKind::Down => {
                if let Some(ref drag_id) = hit_drag_handle_id {
                    scene.set_drag_handle_pressed(drag_id, true);
                    self.current_drag_handle_press = Some(drag_id.clone());
                }
                if let (Some(tile_id), Some(node_id)) = (hit_tile_id, hit_node_id) {
                    if let Some(state) = scene.hit_region_states.get_mut(&node_id) {
                        state.pressed = true;
                    }
                    // Emit local patch for press-on — this is the critical 4ms path
                    local_patch.push_state(LocalStateUpdate::new(node_id).with_pressed(true));
                    self.current_press = Some((tile_id, node_id));

                    // ── Auto-capture: acquire capture automatically if auto_capture=true ─
                    let auto_cap = scene
                        .nodes
                        .get(&node_id)
                        .map(|n| {
                            if let NodeData::HitRegion(hr) = &n.data {
                                hr.auto_capture
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);

                    let release_on_up = scene
                        .nodes
                        .get(&node_id)
                        .map(|n| {
                            if let NodeData::HitRegion(hr) = &n.data {
                                hr.release_on_up
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);

                    if auto_cap {
                        // Try to acquire capture; succeeds unless another node already
                        // holds capture for this device (which shouldn't happen at Down
                        // if the pre-existing capture was already released or routed above).
                        let _ = self
                            .capture
                            .acquire(device_id, tile_id, node_id, release_on_up);
                    }

                    if let Some(namespace) = tile_namespace(scene, tile_id) {
                        let (local_x, local_y) = display_to_local(scene, tile_id, event.x, event.y);
                        dispatch = Some(AgentDispatch {
                            namespace,
                            tile_id,
                            node_id,
                            interaction_id: interaction_id.clone().unwrap_or_default(),
                            local_x,
                            local_y,
                            display_x: event.x,
                            display_y: event.y,
                            device_id,
                            kind: AgentDispatchKind::PointerDown,
                            capture_released_reason: None,
                        });
                    }
                }
            }
            PointerEventKind::Up => {
                if let Some(drag_id) = self.current_drag_handle_press.take() {
                    scene.set_drag_handle_pressed(&drag_id, false);
                }
                if let Some((pressed_tile_id, pressed_node_id)) = self.current_press.take() {
                    if let Some(state) = scene.hit_region_states.get_mut(&pressed_node_id) {
                        state.pressed = false;
                    }
                    // Emit local patch for press-off
                    local_patch
                        .push_state(LocalStateUpdate::new(pressed_node_id).with_pressed(false));
                    // Activation: press and release on the same node
                    if hit_node_id == Some(pressed_node_id) {
                        activated = true;
                        if let Some(namespace) = tile_namespace(scene, pressed_tile_id) {
                            let (local_x, local_y) =
                                display_to_local(scene, pressed_tile_id, event.x, event.y);
                            dispatch = Some(AgentDispatch {
                                namespace,
                                tile_id: pressed_tile_id,
                                node_id: pressed_node_id,
                                interaction_id: interaction_id.clone().unwrap_or_default(),
                                local_x,
                                local_y,
                                display_x: event.x,
                                display_y: event.y,
                                device_id,
                                kind: AgentDispatchKind::Activated,
                                capture_released_reason: None,
                            });
                        }
                    } else {
                        // Released outside the pressed node — dispatch pointer_up
                        if let Some(namespace) = tile_namespace(scene, pressed_tile_id) {
                            let (local_x, local_y) =
                                display_to_local(scene, pressed_tile_id, event.x, event.y);
                            let up_interaction_id = scene
                                .nodes
                                .get(&pressed_node_id)
                                .and_then(|n| {
                                    if let NodeData::HitRegion(hr) = &n.data {
                                        Some(hr.interaction_id.clone())
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_default();
                            dispatch = Some(AgentDispatch {
                                namespace,
                                tile_id: pressed_tile_id,
                                node_id: pressed_node_id,
                                interaction_id: up_interaction_id,
                                local_x,
                                local_y,
                                display_x: event.x,
                                display_y: event.y,
                                device_id,
                                kind: AgentDispatchKind::PointerUp,
                                capture_released_reason: None,
                            });
                        }
                    }
                }
            }
            PointerEventKind::Move => {
                // If hovering, dispatch pointer_move (overrides enter/leave set above)
                if let (Some(tile_id), Some(node_id)) = (hit_tile_id, hit_node_id) {
                    if prev_hover_node == new_hover_node {
                        // Already hovering this node — plain move
                        if let Some(namespace) = tile_namespace(scene, tile_id) {
                            let (local_x, local_y) =
                                display_to_local(scene, tile_id, event.x, event.y);
                            dispatch = Some(AgentDispatch {
                                namespace,
                                tile_id,
                                node_id,
                                interaction_id: interaction_id.clone().unwrap_or_default(),
                                local_x,
                                local_y,
                                display_x: event.x,
                                display_y: event.y,
                                device_id,
                                kind: AgentDispatchKind::PointerMove,
                                capture_released_reason: None,
                            });
                        }
                    }
                    // If prev != new (handled above by enter/leave dispatch), no extra move
                }
            }
        }

        let local_ack_us = start.elapsed().as_micros() as u64;

        InputResult {
            hit,
            interaction_id,
            activated,
            local_ack_us,
            hit_test_us,
            dispatch,
            extra_dispatches,
            local_patch,
        }
    }

    /// Process an explicit capture request from an agent.
    ///
    /// Returns `Ok(AgentDispatch)` with `kind=CaptureGranted` if the capture was
    /// acquired, or `Ok(AgentDispatch)` with `kind=CaptureDenied` if another node
    /// already holds capture for the requested device.
    ///
    /// Per the spec (lines 104-106): capture can ONLY be granted in response to
    /// a PointerDownEvent.  The caller is responsible for only calling this after
    /// confirming a Down event just fired.
    pub fn request_capture(
        &mut self,
        req: &CaptureRequest,
        scene: &SceneGraph,
        release_on_up: bool,
    ) -> Option<AgentDispatch> {
        let namespace = tile_namespace(scene, req.tile_id)?;
        let interaction_id = scene
            .nodes
            .get(&req.node_id)
            .and_then(|n| {
                if let NodeData::HitRegion(hr) = &n.data {
                    Some(hr.interaction_id.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let kind = if self
            .capture
            .acquire(req.device_id, req.tile_id, req.node_id, release_on_up)
            .is_ok()
        {
            AgentDispatchKind::CaptureGranted
        } else {
            AgentDispatchKind::CaptureDenied
        };

        Some(AgentDispatch {
            namespace,
            tile_id: req.tile_id,
            node_id: req.node_id,
            interaction_id,
            local_x: 0.0,
            local_y: 0.0,
            display_x: 0.0,
            display_y: 0.0,
            device_id: req.device_id,
            kind,
            capture_released_reason: None,
        })
    }

    /// Process an explicit capture release request from an agent.
    ///
    /// If the named device has active capture, releases it and returns a
    /// `CaptureReleased(reason=AGENT_RELEASED)` dispatch.  Returns `None` if
    /// there was no active capture for the device.
    pub fn release_capture(
        &mut self,
        req: &CaptureReleaseRequest,
        scene: &SceneGraph,
    ) -> Option<AgentDispatch> {
        let state = self.capture.get(req.device_id)?;
        let (tile_id, node_id) = (state.tile_id, state.node_id);
        let namespace = tile_namespace(scene, tile_id)?;
        let interaction_id = scene
            .nodes
            .get(&node_id)
            .and_then(|n| {
                if let NodeData::HitRegion(hr) = &n.data {
                    Some(hr.interaction_id.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        self.capture.release(req.device_id);

        Some(AgentDispatch {
            namespace,
            tile_id,
            node_id,
            interaction_id,
            local_x: 0.0,
            local_y: 0.0,
            display_x: 0.0,
            display_y: 0.0,
            device_id: req.device_id,
            kind: AgentDispatchKind::CaptureReleased,
            capture_released_reason: Some(CaptureReleasedReason::AgentReleased),
        })
    }

    /// Runtime capture theft (Alt+Tab, system notification, tab switch, lease revocation).
    ///
    /// Per the spec (lines 130-132): sends PointerCancelEvent first, then
    /// CaptureReleasedEvent(reason).  Returns both dispatches in order, or an
    /// empty vec if no capture was active for the device.
    pub fn steal_capture(
        &mut self,
        device_id: u32,
        reason: CaptureReleasedReason,
        scene: &SceneGraph,
    ) -> Vec<AgentDispatch> {
        let state = match self.capture.get(device_id) {
            Some(s) => *s,
            None => return vec![],
        };
        let (tile_id, node_id) = (state.tile_id, state.node_id);
        let namespace = match tile_namespace(scene, tile_id) {
            Some(ns) => ns,
            None => {
                self.capture.release(device_id);
                return vec![];
            }
        };
        let interaction_id = scene
            .nodes
            .get(&node_id)
            .and_then(|n| {
                if let NodeData::HitRegion(hr) = &n.data {
                    Some(hr.interaction_id.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        self.capture.release(device_id);

        vec![
            // 1. PointerCancelEvent — agent MUST treat this as terminal
            AgentDispatch {
                namespace: namespace.clone(),
                tile_id,
                node_id,
                interaction_id: interaction_id.clone(),
                local_x: 0.0,
                local_y: 0.0,
                display_x: 0.0,
                display_y: 0.0,
                device_id,
                kind: AgentDispatchKind::PointerCancel,
                capture_released_reason: None,
            },
            // 2. CaptureReleasedEvent
            AgentDispatch {
                namespace,
                tile_id,
                node_id,
                interaction_id,
                local_x: 0.0,
                local_y: 0.0,
                display_x: 0.0,
                display_y: 0.0,
                device_id,
                kind: AgentDispatchKind::CaptureReleased,
                capture_released_reason: Some(reason),
            },
        ]
    }

    /// Process a pointer event that hit a chrome drag handle.
    ///
    /// This implements the compositor-internal long-press drag state machine
    /// (RFC 0004 §3.0 carve-out). Returns a [`DragEventOutcome`] that the
    /// caller MUST act on:
    ///
    /// - `Accumulating { progress }` — update visual progress on the handle.
    /// - `Activated` — apply visual feedback (z-boost, opacity, 2px border);
    ///   runtime MUST track the element and call `process_drag_move` on
    ///   subsequent `PointerMove` events for the same `device_id`.
    /// - `Moved { element_id, new_x, new_y }` — update the element's bounds.
    /// - `Released { element_id, final_x, final_y }` — persist the geometry.
    /// - `Cancelled` — clear visual feedback.
    /// - `Idle` — no action.
    ///
    /// ## Touch vs pointer thresholds
    ///
    /// `device_id == 0` (primary pointer / mouse) uses the 250 ms threshold.
    /// Any other `device_id` is treated as a touch contact and uses 1000 ms.
    // All arguments are required: pointer event, hit zone, element identity,
    // element kind, element bounds, and display dimensions.  They cannot be
    // collapsed into fewer parameters without losing information or creating a
    // purpose-built context struct for this single call site.
    #[allow(clippy::too_many_arguments)]
    pub fn process_drag_handle_pointer(
        &mut self,
        event: &PointerEvent,
        hit_interaction_id: &str,
        element_id: SceneId,
        element_kind: DragHandleElementKind,
        element_bounds: tze_hud_scene::Rect,
        display_width: f32,
        display_height: f32,
        is_header_band: bool,
    ) -> DragEventOutcome {
        let now = Instant::now();
        let device_id = event.device_id;

        // Select threshold: primary pointer = 250ms, touch = 1000ms.
        let threshold_ms = if device_id == 0 {
            drag::LONG_PRESS_POINTER_THRESHOLD_MS
        } else {
            drag::LONG_PRESS_TOUCH_THRESHOLD_MS
        };

        match event.kind {
            PointerEventKind::Down => {
                // Start accumulating for this device.
                let state = DeviceDragState::new_at(
                    hit_interaction_id.to_string(),
                    element_id,
                    element_kind,
                    event.x,
                    event.y,
                    threshold_ms,
                    is_header_band,
                    now,
                );
                self.drag_states.insert(device_id, state);
                DragEventOutcome::Accumulating { progress: 0.0 }
            }
            PointerEventKind::Move => {
                let Some(state) = self.drag_states.get_mut(&device_id) else {
                    return DragEventOutcome::Idle;
                };
                match state.phase {
                    DragPhase::Idle => DragEventOutcome::Idle,
                    DragPhase::Accumulating => {
                        // Immediate (portal header-band / titlebar) handles engage
                        // on the FIRST move — no long-press hold, no early-movement
                        // cancel (hud-cpjqe). This is what makes a fast titlebar
                        // drag reliable; the grip path below keeps the hysteresis.
                        if state.immediate {
                            state.phase = DragPhase::Activated;
                            state.grab_offset_x = event.x - element_bounds.x;
                            state.grab_offset_y = event.y - element_bounds.y;
                            return DragEventOutcome::Activated {
                                element_id: state.element_id,
                                element_kind: state.element_kind,
                            };
                        }
                        // Check movement cancellation.
                        if state.has_exceeded_movement_tolerance(event.x, event.y) {
                            let _ = self.drag_states.remove(&device_id);
                            return DragEventOutcome::Cancelled;
                        }
                        // Check threshold.
                        if state.is_threshold_met_at(now) {
                            // Activate drag: record grab offset.
                            state.phase = DragPhase::Activated;
                            state.grab_offset_x = event.x - element_bounds.x;
                            state.grab_offset_y = event.y - element_bounds.y;
                            return DragEventOutcome::Activated {
                                element_id: state.element_id,
                                element_kind: state.element_kind,
                            };
                        }
                        // Accumulating — update progress.
                        let progress = state.progress_at(now);
                        state.last_progress = progress;
                        DragEventOutcome::Accumulating { progress }
                    }
                    DragPhase::Activated => {
                        // Compute raw new top-left.
                        let raw_x = event.x - state.grab_offset_x;
                        let raw_y = event.y - state.grab_offset_y;
                        let (new_x, new_y) = drag::quantise_and_clamp(
                            raw_x,
                            raw_y,
                            element_bounds.width,
                            element_bounds.height,
                            display_width,
                            display_height,
                            self.drag_config.snap_grid_pct,
                        );
                        DragEventOutcome::Moved {
                            element_id: state.element_id,
                            element_kind: state.element_kind,
                            new_x,
                            new_y,
                        }
                    }
                }
            }
            PointerEventKind::Up => {
                let Some(state) = self.drag_states.remove(&device_id) else {
                    return DragEventOutcome::Idle;
                };
                match state.phase {
                    DragPhase::Idle | DragPhase::Accumulating => {
                        // Short press (tap) — not a drag; no geometry change.
                        DragEventOutcome::Cancelled
                    }
                    DragPhase::Activated => {
                        let raw_x = event.x - state.grab_offset_x;
                        let raw_y = event.y - state.grab_offset_y;
                        let (final_x, final_y) = drag::quantise_and_clamp(
                            raw_x,
                            raw_y,
                            element_bounds.width,
                            element_bounds.height,
                            display_width,
                            display_height,
                            self.drag_config.snap_grid_pct,
                        );
                        DragEventOutcome::Released {
                            element_id: state.element_id,
                            element_kind: state.element_kind,
                            final_x,
                            final_y,
                        }
                    }
                }
            }
        }
    }

    /// Persist the final drag geometry to the element store.
    ///
    /// This is a thin wrapper around [`drag::persist_geometry_override`] for
    /// callers that have already resolved the `interaction_key` and know the
    /// element type.
    ///
    /// The caller (windowed/headless runtime) MUST call this on
    /// [`DragEventOutcome::Released`], then atomically save the element store
    /// to disk.
    // All arguments are required geometry primitives (final position, dimensions,
    // display dimensions) along with the element store, type, and interaction key.
    // No natural grouping reduces the count without inventing a single-use struct.
    #[allow(clippy::too_many_arguments)]
    pub fn persist_drag_geometry(
        store: &mut ElementStore,
        element_type: ElementType,
        interaction_key: &str,
        final_x: f32,
        final_y: f32,
        width: f32,
        height: f32,
        display_width: f32,
        display_height: f32,
    ) {
        let geometry = drag::final_position_to_geometry(
            final_x,
            final_y,
            width,
            height,
            display_width,
            display_height,
        );
        drag::persist_geometry_override(store, element_type, interaction_key, geometry);
    }
}

impl Default for InputProcessor {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────

/// Get the namespace (agent name) of the tile owner, or None if tile not found.
fn tile_namespace(scene: &SceneGraph, tile_id: SceneId) -> Option<String> {
    scene.tiles.get(&tile_id).map(|t| t.namespace.clone())
}

/// Convert display-space coordinates to tile-local coordinates.
fn display_to_local(scene: &SceneGraph, tile_id: SceneId, x: f32, y: f32) -> (f32, f32) {
    if let Some(tile) = scene.tiles.get(&tile_id) {
        (x - tile.bounds.x, y - tile.bounds.y)
    } else {
        (x, y)
    }
}

/// Return `true` when the given node is a `HitRegionNode` with
/// `accepts_composer_input = true`.
///
/// Used by `process_with_focus` to determine whether a focus-gained event
/// should activate the `ComposerDraftManager`.
fn node_accepts_composer_input(scene: &SceneGraph, node_id: SceneId) -> bool {
    scene.nodes.get(&node_id).is_some_and(|n| {
        if let NodeData::HitRegion(hr) = &n.data {
            hr.accepts_composer_input
        } else {
            false
        }
    })
}

/// Resolve the composer-placeholder override configured on a `HitRegionNode`
/// (`HitRegionNode::composer_placeholder`), if any.
///
/// Mirrors the field's three-state `Option<String>` convention: `None` means
/// no override is configured (the caller should fall back to its own
/// default); `Some("")` is an explicit opt-out; `Some(text)` is a custom hint.
/// Returns `None` for a missing node or a non-`HitRegion` node.
///
/// Used by `apply_focus_transition_side_effects` to seed the draft manager's
/// per-focus placeholder override at focus-gain time (hud-se6hs).
fn node_composer_placeholder(scene: &SceneGraph, node_id: SceneId) -> Option<String> {
    let node = scene.nodes.get(&node_id)?;
    let NodeData::HitRegion(hr) = &node.data else {
        return None;
    };
    (*hr.composer_placeholder).clone()
}

/// Depth-first search for the first composer node (a `HitRegionNode` with
/// `accepts_composer_input = true`) reachable from `root`.
///
/// Used by [`InputProcessor::classify_portal_focus`] to find a control's sibling
/// composer as the typing-recovery target (hud-2v8br).
fn find_composer_node(
    root: Option<SceneId>,
    nodes: &std::collections::HashMap<SceneId, tze_hud_scene::types::Node>,
) -> Option<SceneId> {
    let root_id = root?;
    let node = nodes.get(&root_id)?;
    if let NodeData::HitRegion(hr) = &node.data {
        if hr.accepts_composer_input {
            return Some(root_id);
        }
    }
    for child in &node.children {
        if let Some(found) = find_composer_node(Some(*child), nodes) {
            return Some(found);
        }
    }
    None
}

/// Resolve the namespace and interaction_id for a tile/node pair.
fn resolve_node_meta(
    scene: &SceneGraph,
    tile_id: SceneId,
    node_id: SceneId,
) -> (Option<String>, Option<String>) {
    let namespace = tile_namespace(scene, tile_id);
    let interaction_id = scene.nodes.get(&node_id).and_then(|n| {
        if let NodeData::HitRegion(hr) = &n.data {
            Some(hr.interaction_id.clone())
        } else {
            None
        }
    });
    (namespace, interaction_id)
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::*;

    fn setup_scene_with_hit_region() -> (SceneGraph, SceneId, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("test", 60_000);

        let tile_id = scene
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(100.0, 100.0, 400.0, 300.0),
                1,
            )
            .unwrap();

        let hr_node_id = SceneId::new();
        let hit_node = Node {
            layout: Default::default(),
            id: hr_node_id,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(50.0, 50.0, 200.0, 100.0),
                interaction_id: "test-button".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                ..Default::default()
            }),
        };
        scene.set_tile_root(tile_id, hit_node).unwrap();

        (scene, tile_id, hr_node_id)
    }

    fn setup_scrollable_scene() -> (SceneGraph, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("test", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(100.0, 100.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(tile_id, tze_hud_scene::TileScrollConfig::vertical())
            .unwrap();
        (scene, tile_id)
    }

    #[test]
    fn test_process_scroll_event_updates_scene_local_offset() {
        let (mut scene, tile_id) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();

        let changed = processor.process_scroll_event(
            &ScrollEvent {
                x: 150.0,
                y: 150.0,
                delta_x: 0.0,
                delta_y: 24.0,
            },
            &mut scene,
        );

        assert!(changed.is_some(), "scroll on a configured tile must update");
        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (offset_y - 24.0).abs() < f32::EPSILON,
            "expected local offset_y=24.0, got {offset_y}"
        );
    }

    #[test]
    fn scroll_state_of_destroyed_tile_is_swept() {
        let (mut scene, tile_a) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();
        let wheel = |x| ScrollEvent {
            x,
            y: 150.0,
            delta_x: 0.0,
            delta_y: 24.0,
        };
        processor.process_scroll_event(&wheel(150.0), &mut scene);
        assert!(processor.scroll_state.is_scrollable(tile_a));

        scene.delete_tile(tile_a, "test").unwrap();
        let tab = scene.active_tab.unwrap();
        let lease = scene.grant_lease("test", 60_000);
        let tile_b = scene
            .create_tile(tab, "test", lease, Rect::new(600.0, 100.0, 400.0, 300.0), 1)
            .unwrap();
        scene
            .register_tile_scroll_config(tile_b, tze_hud_scene::TileScrollConfig::vertical())
            .unwrap();
        processor.process_scroll_event(&wheel(650.0), &mut scene);

        assert!(processor.scroll_state.is_scrollable(tile_b));
        assert!(!processor.scroll_state.is_scrollable(tile_a));
    }

    #[test]
    fn scroll_state_of_destroyed_tile_is_swept_on_append_path() {
        let (mut scene, tile_a) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();
        processor.notify_tile_content_appended(tile_a, 500.0, 300.0, 20.0, &mut scene);
        assert!(processor.scroll_state.is_scrollable(tile_a));

        scene.delete_tile(tile_a, "test").unwrap();
        let tab = scene.active_tab.unwrap();
        let lease = scene.grant_lease("test", 60_000);
        let tile_b = scene
            .create_tile(tab, "test", lease, Rect::new(600.0, 100.0, 400.0, 300.0), 1)
            .unwrap();
        scene
            .register_tile_scroll_config(tile_b, tze_hud_scene::TileScrollConfig::vertical())
            .unwrap();
        processor.notify_tile_content_appended(tile_b, 500.0, 300.0, 20.0, &mut scene);

        assert!(processor.scroll_state.is_scrollable(tile_b));
        assert!(!processor.scroll_state.is_scrollable(tile_a));
    }

    #[test]
    fn test_user_scroll_is_authoritative_over_queued_adapter_offset() {
        let (mut scene, tile_id) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();

        processor.queue_set_scroll_offset(SetScrollOffsetRequest {
            tile_id,
            offset_x: 0.0,
            offset_y: 200.0,
        });

        let changed = processor.process_scroll_event(
            &ScrollEvent {
                x: 150.0,
                y: 150.0,
                delta_x: 0.0,
                delta_y: 18.0,
            },
            &mut scene,
        );

        assert!(changed.is_some(), "user scroll should still update offset");
        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (offset_y - 18.0).abs() < f32::EPSILON,
            "user scroll must win over queued adapter request, got {offset_y}"
        );
    }

    #[test]
    fn test_hover_state_updates() {
        let (mut scene, _, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Move pointer over hit region (tile at 100,100; region at 50,50 within)
        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        assert!(result.hit.is_some());
        assert_eq!(result.interaction_id, Some("test-button".to_string()));
        assert!(scene.hit_region_states[&hr_node_id].hovered);

        // Move pointer away
        let result = processor.process(
            &PointerEvent {
                x: 10.0,
                y: 10.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        assert!(result.hit.is_none());
        assert!(!scene.hit_region_states[&hr_node_id].hovered);
    }

    #[test]
    fn test_press_and_activate() {
        let (mut scene, _, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Press on hit region
        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        assert!(scene.hit_region_states[&hr_node_id].pressed);
        assert!(!result.activated);

        // Release on hit region — should activate
        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Up,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        assert!(!scene.hit_region_states[&hr_node_id].pressed);
        assert!(result.activated);
        assert_eq!(result.interaction_id, Some("test-button".to_string()));
    }

    #[test]
    fn test_local_ack_under_4ms() {
        use tze_hud_scene::perf_budget::{budgets, test_budget};

        let (mut scene, _, _) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        let ack_budget = test_budget(budgets::INPUT_ACK_BUDGET_US);
        let hit_budget = test_budget(budgets::HIT_TEST_BUDGET_US);

        // local_ack should be within calibrated budget (hardware-normalized)
        assert!(
            result.local_ack_us < ack_budget,
            "local_ack_us was {}us, calibrated budget is {}us (base: {}us)",
            result.local_ack_us,
            ack_budget,
            budgets::INPUT_ACK_BUDGET_US,
        );
        // hit_test should be within calibrated budget
        assert!(
            result.hit_test_us < hit_budget,
            "hit_test_us was {}us, calibrated budget is {}us (base: {}us)",
            result.hit_test_us,
            hit_budget,
            budgets::HIT_TEST_BUDGET_US,
        );
    }

    // ── New tests for AgentDispatch ─────────────────────────────────────

    #[test]
    fn test_dispatch_pointer_enter_on_first_hover() {
        let (mut scene, tile_id, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // First move onto the hit region — should produce PointerEnter dispatch
        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        let dispatch = result.dispatch.expect("expected AgentDispatch on enter");
        assert_eq!(dispatch.kind, AgentDispatchKind::PointerEnter);
        assert_eq!(dispatch.namespace, "test");
        assert_eq!(dispatch.tile_id, tile_id);
        assert_eq!(dispatch.node_id, hr_node_id);
        assert_eq!(dispatch.interaction_id, "test-button");
        // local coords: display(200,180) - tile_origin(100,100) = (100,80)
        assert!((dispatch.local_x - 100.0).abs() < 0.01);
        assert!((dispatch.local_y - 80.0).abs() < 0.01);
    }

    #[test]
    fn test_dispatch_pointer_leave_on_exit() {
        let (mut scene, tile_id, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Enter
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        // Leave
        let result = processor.process(
            &PointerEvent {
                x: 10.0,
                y: 10.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        let dispatch = result.dispatch.expect("expected AgentDispatch on leave");
        assert_eq!(dispatch.kind, AgentDispatchKind::PointerLeave);
        assert_eq!(dispatch.namespace, "test");
        assert_eq!(dispatch.tile_id, tile_id);
        assert_eq!(dispatch.node_id, hr_node_id);
        assert_eq!(dispatch.interaction_id, "test-button");
    }

    // ── Regression: pointer_leave must not be dropped on node-to-node hover transition ──
    //
    // Before the fix, moving the pointer directly from a hit region on tile A to a hit region
    // on tile B in a single Move event produced only PointerEnter(B) — the PointerLeave(A) was
    // silently overwritten in the single-slot `dispatch` field.  After the fix both events are
    // present: dispatch=PointerLeave(A), extra_dispatches=[PointerEnter(B)].
    #[test]
    fn test_pointer_leave_not_dropped_on_node_to_node_transition() {
        // Two non-overlapping tiles placed side-by-side, each with a hit region.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();

        // Tile A — left half
        let lease_a = scene.grant_lease("agent-a", 60_000);
        let tile_a = scene
            .create_tile(
                tab_id,
                "agent-a",
                lease_a,
                Rect::new(0.0, 0.0, 400.0, 400.0),
                1,
            )
            .unwrap();
        let node_a = SceneId::new();
        scene
            .set_tile_root(
                tile_a,
                Node {
                    layout: Default::default(),
                    id: node_a,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 400.0, 400.0),
                        interaction_id: "node-a".to_string(),
                        accepts_pointer: true,
                        accepts_focus: false,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        // Tile B — right half
        let lease_b = scene.grant_lease("agent-b", 60_000);
        let tile_b = scene
            .create_tile(
                tab_id,
                "agent-b",
                lease_b,
                Rect::new(400.0, 0.0, 400.0, 400.0),
                1,
            )
            .unwrap();
        let node_b = SceneId::new();
        scene
            .set_tile_root(
                tile_b,
                Node {
                    layout: Default::default(),
                    id: node_b,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 400.0, 400.0),
                        interaction_id: "node-b".to_string(),
                        accepts_pointer: true,
                        accepts_focus: false,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();

        // Move into tile A — establishes hover on node_a
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 200.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        // Move directly into tile B in one event — must produce PointerLeave(A) + PointerEnter(B)
        let result = processor.process(
            &PointerEvent {
                x: 600.0,
                y: 200.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        let primary = result
            .dispatch
            .expect("expected PointerLeave as primary dispatch on A→B transition");
        assert_eq!(
            primary.kind,
            AgentDispatchKind::PointerLeave,
            "primary dispatch must be PointerLeave for the old node"
        );
        assert_eq!(primary.node_id, node_a, "PointerLeave must target node A");
        assert_eq!(
            primary.interaction_id, "node-a",
            "PointerLeave must carry node A's interaction_id"
        );

        assert_eq!(
            result.extra_dispatches.len(),
            1,
            "extra_dispatches must contain exactly one event (PointerEnter for node B)"
        );
        let enter = &result.extra_dispatches[0];
        assert_eq!(
            enter.kind,
            AgentDispatchKind::PointerEnter,
            "extra dispatch must be PointerEnter for the new node"
        );
        assert_eq!(enter.node_id, node_b, "PointerEnter must target node B");
        assert_eq!(
            enter.interaction_id, "node-b",
            "PointerEnter must carry node B's interaction_id"
        );
    }

    #[test]
    fn test_dispatch_pointer_down() {
        let (mut scene, tile_id, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        let dispatch = result.dispatch.expect("expected AgentDispatch on down");
        assert_eq!(dispatch.kind, AgentDispatchKind::PointerDown);
        assert_eq!(dispatch.tile_id, tile_id);
        assert_eq!(dispatch.node_id, hr_node_id);
    }

    #[test]
    fn test_dispatch_activated_on_press_release() {
        let (mut scene, tile_id, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Down
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        // Up on same node — Activated
        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Up,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        assert!(result.activated);
        let dispatch = result
            .dispatch
            .expect("expected AgentDispatch on activation");
        assert_eq!(dispatch.kind, AgentDispatchKind::Activated);
        assert_eq!(dispatch.tile_id, tile_id);
        assert_eq!(dispatch.node_id, hr_node_id);
        assert_eq!(dispatch.interaction_id, "test-button");
    }

    #[test]
    fn test_dispatch_pointer_up_outside_pressed_node() {
        let (mut scene, tile_id, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Press inside hit region
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        // Release outside — PointerUp (not Activated)
        let result = processor.process(
            &PointerEvent {
                x: 10.0,
                y: 10.0,
                kind: PointerEventKind::Up,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        assert!(!result.activated);
        let dispatch = result
            .dispatch
            .expect("expected AgentDispatch on up-outside");
        assert_eq!(dispatch.kind, AgentDispatchKind::PointerUp);
        assert_eq!(dispatch.tile_id, tile_id);
        assert_eq!(dispatch.node_id, hr_node_id);
        assert_eq!(dispatch.interaction_id, "test-button");
    }

    #[test]
    fn test_no_dispatch_when_no_hit() {
        let (mut scene, _, _) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

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

        assert!(result.hit.is_none());
        assert!(result.dispatch.is_none());
    }

    #[test]
    fn test_dispatch_move_while_hovering() {
        let (mut scene, tile_id, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Enter
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        // Move within the same hit region — PointerMove
        let result = processor.process(
            &PointerEvent {
                x: 210.0,
                y: 185.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        let dispatch = result.dispatch.expect("expected AgentDispatch on move");
        assert_eq!(dispatch.kind, AgentDispatchKind::PointerMove);
        assert_eq!(dispatch.tile_id, tile_id);
        assert_eq!(dispatch.node_id, hr_node_id);
        assert!((dispatch.local_x - 110.0).abs() < 0.01);
        assert!((dispatch.local_y - 85.0).abs() < 0.01);
    }

    // ── SceneLocalPatch integration tests ────────────────────────────────

    #[test]
    fn test_local_patch_produced_on_pointer_down() {
        let (mut scene, _, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        // SceneLocalPatch must contain a pressed=true update for the hit node
        assert!(
            !result.local_patch.is_empty(),
            "local_patch should not be empty after Down"
        );
        let pressed_update = result
            .local_patch
            .node_updates
            .iter()
            .find(|u| u.node_id == hr_node_id && u.pressed.is_some())
            .expect("expected pressed state update for hr_node_id");
        assert_eq!(pressed_update.pressed, Some(true));
        assert!(!pressed_update.rollback);
    }

    #[test]
    fn test_local_patch_produced_on_pointer_up() {
        let (mut scene, _, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Press first
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        // Up
        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Up,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        assert!(
            !result.local_patch.is_empty(),
            "local_patch should not be empty after Up"
        );
        let state_update = result
            .local_patch
            .node_updates
            .iter()
            .find(|u| u.node_id == hr_node_id)
            .expect("expected state update for hr_node_id");
        assert_eq!(state_update.pressed, Some(false));
    }

    #[test]
    fn test_local_patch_hover_on_enter() {
        let (mut scene, _, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        assert!(
            !result.local_patch.is_empty(),
            "local_patch should contain hover update"
        );
        let state_update = result
            .local_patch
            .node_updates
            .iter()
            .find(|u| u.node_id == hr_node_id)
            .expect("expected state update for hr_node_id");
        assert_eq!(state_update.hovered, Some(true));
    }

    #[test]
    fn test_local_patch_hover_off_on_leave() {
        let (mut scene, _, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Enter
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Move,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );

        // Leave
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
            !result.local_patch.is_empty(),
            "local_patch should contain hover-off update"
        );
        let state_update = result
            .local_patch
            .node_updates
            .iter()
            .find(|u| u.node_id == hr_node_id)
            .expect("expected state update for hr_node_id");
        assert_eq!(state_update.hovered, Some(false));
    }

    #[test]
    fn test_local_patch_empty_when_no_state_change() {
        let (mut scene, _, _) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Move in empty space — no hit, no state change
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
            result.local_patch.is_empty(),
            "no state changed, patch should be empty"
        );
    }

    #[test]
    fn test_apply_agent_rejection_produces_rollback_patch() {
        let (mut scene, _, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Press to set up pressed state
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );
        assert!(scene.hit_region_states[&hr_node_id].pressed);

        // Agent rejects the interaction
        let rollback_patch = processor.apply_agent_rejection(hr_node_id, &mut scene);

        // Pressed state cleared in scene graph immediately
        assert!(!scene.hit_region_states[&hr_node_id].pressed);

        // Patch contains rollback=true state update
        assert!(!rollback_patch.is_empty());
        let update = rollback_patch
            .node_updates
            .iter()
            .find(|u| u.node_id == hr_node_id)
            .expect("expected rollback state update");
        assert_eq!(update.pressed, Some(false));
        assert!(update.rollback, "rollback flag must be set");

        // Rollback tracker should record the animation
        assert!(processor.rollback_tracker().is_rolling_back(hr_node_id));
    }

    #[test]
    fn test_agent_silence_does_not_rollback() {
        let (mut scene, _, hr_node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // Press — starts pressed
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id: 0,
                timestamp: None,
            },
            &mut scene,
        );
        assert!(scene.hit_region_states[&hr_node_id].pressed);

        // Agent does NOT respond (silence) — pressed remains true
        // (no apply_agent_rejection called)
        assert!(
            scene.hit_region_states[&hr_node_id].pressed,
            "pressed should remain true on agent silence per spec"
        );
        assert!(
            !processor.rollback_tracker().is_rolling_back(hr_node_id),
            "rollback should NOT be triggered by agent silence"
        );
    }

    // ── Pointer Capture Protocol Tests ─────────────────────────────────
    // These tests cover the acceptance scenarios from issue rig-vzf0.

    /// Helper: build a scene with TWO tiles, each with a hit region.
    /// Tile T1: bounds (100,100,300,200), hit region (0,0,300,200) — "node-t1"
    /// Tile T2: bounds (500,100,300,200), hit region (0,0,300,200) — "node-t2"
    fn setup_two_tile_scene() -> (SceneGraph, SceneId, SceneId, SceneId, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease1 = scene.grant_lease("agent1", 60_000);
        let lease2 = scene.grant_lease("agent2", 60_000);

        let t1 = scene
            .create_tile(
                tab_id,
                "agent1",
                lease1,
                Rect::new(100.0, 100.0, 300.0, 200.0),
                1,
            )
            .unwrap();
        let n1 = SceneId::new();
        scene
            .set_tile_root(
                t1,
                Node {
                    layout: Default::default(),
                    id: n1,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
                        interaction_id: "node-t1".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        let t2 = scene
            .create_tile(
                tab_id,
                "agent2",
                lease2,
                Rect::new(500.0, 100.0, 300.0, 200.0),
                2,
            )
            .unwrap();
        let n2 = SceneId::new();
        scene
            .set_tile_root(
                t2,
                Node {
                    layout: Default::default(),
                    id: n2,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
                        interaction_id: "node-t2".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        (scene, t1, n1, t2, n2)
    }

    /// WHEN node N1 in tile T1 holds pointer capture for device D1 and the pointer
    /// moves outside T1's bounds THEN all pointer events from D1 are routed to N1
    /// regardless of which tile the pointer is visually over. (spec line 110)
    #[test]
    fn test_capture_routes_events_outside_capturing_tile_bounds() {
        let (mut scene, t1, n1, _t2, _n2) = setup_two_tile_scene();
        let mut processor = InputProcessor::new();
        let device_id = 0u32;

        // Press on T1 to establish press state
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Down,
                device_id,
                timestamp: None,
            },
            &mut scene,
        );

        // Acquire capture for N1/T1 for device 0
        let req = CaptureRequest {
            tile_id: t1,
            node_id: n1,
            device_id,
        };
        let response = processor.request_capture(&req, &scene, false).unwrap();
        assert_eq!(response.kind, AgentDispatchKind::CaptureGranted);
        assert!(processor.capture.is_captured(device_id));

        // Move pointer to T2's territory (x=600, which is inside T2 bounds at 500-800)
        let result = processor.process(
            &PointerEvent {
                x: 600.0,
                y: 180.0,
                kind: PointerEventKind::Move,
                device_id,
                timestamp: None,
            },
            &mut scene,
        );

        // Event MUST be routed to N1 (T1's owner), not T2
        let dispatch = result.dispatch.expect("should dispatch during capture");
        assert_eq!(dispatch.kind, AgentDispatchKind::PointerMove);
        assert_eq!(
            dispatch.tile_id, t1,
            "captured events must route to capturing tile"
        );
        assert_eq!(
            dispatch.node_id, n1,
            "captured events must route to capturing node"
        );
        assert_eq!(dispatch.namespace, "agent1");
    }

    /// WHEN node N1 holds capture for device D1 and node N2 requests capture for
    /// the same device D1 THEN the runtime responds with CaptureResponse(result=DENIED).
    /// (spec line 114)
    #[test]
    fn test_capture_denied_when_device_already_captured() {
        let (scene, t1, n1, t2, n2) = setup_two_tile_scene();
        let mut processor = InputProcessor::new();
        let device_id = 0u32;

        // N1 acquires capture
        let req1 = CaptureRequest {
            tile_id: t1,
            node_id: n1,
            device_id,
        };
        let response1 = processor.request_capture(&req1, &scene, false).unwrap();
        assert_eq!(response1.kind, AgentDispatchKind::CaptureGranted);

        // N2 tries to capture the same device
        let req2 = CaptureRequest {
            tile_id: t2,
            node_id: n2,
            device_id,
        };
        let response2 = processor.request_capture(&req2, &scene, false).unwrap();
        assert_eq!(
            response2.kind,
            AgentDispatchKind::CaptureDenied,
            "second capture request for same device must be denied"
        );

        // N1 still holds capture
        let state = processor.capture.get(device_id).unwrap();
        assert_eq!(state.node_id, n1, "original capture must remain");
    }

    /// WHEN a node with release_on_up=true holds capture and a PointerUpEvent arrives
    /// for the captured device THEN capture is released and
    /// CaptureReleasedEvent(reason=POINTER_UP) is dispatched. (spec line 125)
    #[test]
    fn test_capture_released_on_pointer_up_when_release_on_up_true() {
        let (mut scene, t1, n1, _t2, _n2) = setup_two_tile_scene();
        let mut processor = InputProcessor::new();
        let device_id = 0u32;

        // Acquire capture with release_on_up=true
        let req = CaptureRequest {
            tile_id: t1,
            node_id: n1,
            device_id,
        };
        let response = processor.request_capture(&req, &scene, true).unwrap();
        assert_eq!(response.kind, AgentDispatchKind::CaptureGranted);
        assert!(processor.capture.get(device_id).unwrap().release_on_up);

        // Send PointerUp — should release capture
        let result = processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Up,
                device_id,
                timestamp: None,
            },
            &mut scene,
        );

        // Capture should be released
        assert!(
            !processor.capture.is_captured(device_id),
            "capture must be released on PointerUp"
        );

        // The primary dispatch returned should be PointerUp
        let dispatch = result.dispatch.expect("should dispatch on up");
        assert_eq!(dispatch.kind, AgentDispatchKind::PointerUp);
        assert_eq!(dispatch.tile_id, t1);

        // The secondary dispatch (extra_dispatches) must contain CaptureReleased(POINTER_UP)
        // per spec line 125: "CaptureReleasedEvent(reason=POINTER_UP) SHALL be dispatched"
        assert_eq!(
            result.extra_dispatches.len(),
            1,
            "CaptureReleased must be delivered as extra_dispatch after PointerUp"
        );
        let cap_released = &result.extra_dispatches[0];
        assert_eq!(cap_released.kind, AgentDispatchKind::CaptureReleased);
        assert_eq!(
            cap_released.capture_released_reason,
            Some(CaptureReleasedReason::PointerUp)
        );
        assert_eq!(cap_released.tile_id, t1);
    }

    /// WHEN a node holds pointer capture and the user presses Alt+Tab THEN the runtime
    /// sends PointerCancelEvent to the capturing node, followed by
    /// CaptureReleasedEvent(reason=RUNTIME_REVOKED). (spec line 136)
    #[test]
    fn test_capture_theft_sends_cancel_then_released() {
        let (scene, t1, n1, _t2, _n2) = setup_two_tile_scene();
        let mut processor = InputProcessor::new();
        let device_id = 0u32;

        // Acquire capture
        let req = CaptureRequest {
            tile_id: t1,
            node_id: n1,
            device_id,
        };
        processor.request_capture(&req, &scene, false).unwrap();

        // Runtime steals capture (Alt+Tab scenario)
        let dispatches =
            processor.steal_capture(device_id, CaptureReleasedReason::RuntimeRevoked, &scene);

        assert_eq!(
            dispatches.len(),
            2,
            "theft must produce exactly 2 dispatches"
        );

        // First: PointerCancelEvent
        assert_eq!(
            dispatches[0].kind,
            AgentDispatchKind::PointerCancel,
            "first dispatch must be PointerCancelEvent"
        );
        assert_eq!(dispatches[0].tile_id, t1);
        assert_eq!(dispatches[0].node_id, n1);

        // Second: CaptureReleasedEvent(reason=RUNTIME_REVOKED)
        assert_eq!(
            dispatches[1].kind,
            AgentDispatchKind::CaptureReleased,
            "second dispatch must be CaptureReleasedEvent"
        );
        assert_eq!(
            dispatches[1].capture_released_reason,
            Some(CaptureReleasedReason::RuntimeRevoked),
        );

        // Capture is cleared
        assert!(!processor.capture.is_captured(device_id));
    }

    /// WHEN PointerDownEvent hits a HitRegionNode with auto_capture=true THEN the
    /// runtime acquires capture for that node and device without the agent sending
    /// CaptureRequest. (spec line 147)
    #[test]
    fn test_auto_capture_on_pointer_down() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease = scene.grant_lease("agent1", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent1",
                lease,
                Rect::new(100.0, 100.0, 300.0, 200.0),
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
                        bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
                        interaction_id: "auto-cap-button".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        auto_capture: true,
                        release_on_up: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();
        let device_id = 0u32;

        // Before: no capture
        assert!(!processor.capture.is_captured(device_id));

        // PointerDown on auto_capture=true node
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 200.0,
                kind: PointerEventKind::Down,
                device_id,
                timestamp: None,
            },
            &mut scene,
        );

        // Capture must be automatically acquired
        assert!(
            processor.capture.is_captured(device_id),
            "auto_capture=true must acquire capture on PointerDown without explicit request"
        );
        let state = processor.capture.get(device_id).unwrap();
        assert_eq!(state.tile_id, tile_id);
        assert_eq!(state.node_id, node_id);
        assert!(
            state.release_on_up,
            "release_on_up from HitRegionNode must be preserved"
        );
    }

    /// WHEN node holds capture and agent sends explicit CaptureReleaseRequest THEN
    /// capture is released with reason=AGENT_RELEASED. (spec line 120)
    #[test]
    fn test_explicit_capture_release_by_agent() {
        let (scene, t1, n1, _t2, _n2) = setup_two_tile_scene();
        let mut processor = InputProcessor::new();
        let device_id = 0u32;

        // Acquire capture
        let req = CaptureRequest {
            tile_id: t1,
            node_id: n1,
            device_id,
        };
        processor.request_capture(&req, &scene, false).unwrap();

        // Agent releases explicitly
        let release_req = CaptureReleaseRequest { device_id };
        let dispatch = processor
            .release_capture(&release_req, &scene)
            .expect("release must produce dispatch when capture was active");

        assert_eq!(dispatch.kind, AgentDispatchKind::CaptureReleased);
        assert_eq!(
            dispatch.capture_released_reason,
            Some(CaptureReleasedReason::AgentReleased)
        );
        assert_eq!(dispatch.tile_id, t1);
        assert_eq!(dispatch.node_id, n1);
        assert!(!processor.capture.is_captured(device_id));
    }

    /// WHEN capture is NOT active and agent sends CaptureReleaseRequest THEN
    /// release_capture returns None (no spurious events).
    #[test]
    fn test_release_when_no_capture_returns_none() {
        let (scene, _t1, _n1, _t2, _n2) = setup_two_tile_scene();
        let mut processor = InputProcessor::new();
        let result = processor.release_capture(&CaptureReleaseRequest { device_id: 0 }, &scene);
        assert!(
            result.is_none(),
            "releasing when no capture must return None"
        );
    }

    /// WHEN two different devices are captured by different nodes THEN releasing one
    /// does NOT affect the other.
    #[test]
    fn test_two_devices_captured_independently() {
        let (_scene, t1, n1, t2, n2) = setup_two_tile_scene();
        let mut processor = InputProcessor::new();

        processor.capture.acquire(0, t1, n1, false).unwrap();
        processor.capture.acquire(1, t2, n2, true).unwrap();

        assert_eq!(processor.capture.active_count(), 2);

        processor.capture.release(0);
        assert!(!processor.capture.is_captured(0));
        assert!(
            processor.capture.is_captured(1),
            "releasing device 0 must not affect device 1"
        );
    }

    /// PointerUp with release_on_up=FALSE keeps capture active.
    #[test]
    fn test_capture_not_released_on_up_when_flag_false() {
        let (mut scene, t1, n1, _t2, _n2) = setup_two_tile_scene();
        let mut processor = InputProcessor::new();
        let device_id = 0u32;

        // Acquire with release_on_up=false
        let req = CaptureRequest {
            tile_id: t1,
            node_id: n1,
            device_id,
        };
        processor.request_capture(&req, &scene, false).unwrap();

        // Send PointerUp — capture should remain
        processor.process(
            &PointerEvent {
                x: 200.0,
                y: 180.0,
                kind: PointerEventKind::Up,
                device_id,
                timestamp: None,
            },
            &mut scene,
        );

        assert!(
            processor.capture.is_captured(device_id),
            "capture must remain when release_on_up=false"
        );
    }

    // ── Keyboard scroll (hud-6bbe) ────────────────────────────────────────────

    /// PgDn (positive delta_y) scrolls down on a scrollable portal tile.
    #[test]
    fn test_process_keyboard_scroll_pgdn_scrolls_down() {
        let (mut scene, tile_id) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();

        let ev = processor.process_keyboard_scroll(tile_id, KEYBOARD_PAGE_SCROLL_PX, &mut scene);

        assert!(
            ev.is_some(),
            "PgDn must return a changed event for a scrollable tile"
        );
        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (offset_y - KEYBOARD_PAGE_SCROLL_PX).abs() < 1e-4,
            "offset_y must equal KEYBOARD_PAGE_SCROLL_PX after PgDn; got {offset_y}"
        );
    }

    /// PgUp (negative delta_y) scrolls up — clamped to 0 when already at top.
    #[test]
    fn test_process_keyboard_scroll_pgup_clamped_at_zero() {
        let (mut scene, tile_id) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();

        // Starting at offset 0, PgUp should clamp to 0 (no-op on offset).
        let ev = processor.process_keyboard_scroll(tile_id, -KEYBOARD_PAGE_SCROLL_PX, &mut scene);

        // Scroll event fires (tile is scrollable and hit), but offset stays 0.
        // queue_user_scroll sets dirty=true before clamping, so commit_frame returns
        // true and process_scroll_event returns Some even when the clamped offset is
        // unchanged.  The return value is not load-bearing for correctness here.
        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        assert_eq!(
            offset_y, 0.0,
            "PgUp at zero offset must result in exactly 0.0 scroll offset; got {offset_y}"
        );
        let _ = ev; // return value is Some (dirty flag set before clamp) — not asserted
    }

    /// PgUp after PgDn returns toward origin.
    #[test]
    fn test_process_keyboard_scroll_pgdn_then_pgup() {
        let (mut scene, tile_id) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();

        // Scroll down by two page steps.
        processor.process_keyboard_scroll(tile_id, KEYBOARD_PAGE_SCROLL_PX, &mut scene);
        processor.process_keyboard_scroll(tile_id, KEYBOARD_PAGE_SCROLL_PX, &mut scene);

        let (_, before) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (before - 2.0 * KEYBOARD_PAGE_SCROLL_PX).abs() < 1e-4,
            "expected 2x page scroll; got {before}"
        );

        // Scroll back up by one page step.
        processor.process_keyboard_scroll(tile_id, -KEYBOARD_PAGE_SCROLL_PX, &mut scene);

        let (_, after) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (after - KEYBOARD_PAGE_SCROLL_PX).abs() < 1e-4,
            "one PgUp from 2x should leave offset at 1x page; got {after}"
        );
    }

    /// Keyboard scroll for an unknown focus target returns None.
    #[test]
    fn test_process_keyboard_scroll_unknown_focused_tile_returns_none() {
        let (mut scene, _tile_id) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();

        let ev =
            processor.process_keyboard_scroll(SceneId::new(), KEYBOARD_PAGE_SCROLL_PX, &mut scene);
        assert!(
            ev.is_none(),
            "keyboard scroll with no matching focused tile must return None"
        );
    }

    /// Keyboard scroll on a non-scrollable tile returns None.
    #[test]
    fn test_process_keyboard_scroll_non_scrollable_tile_returns_none() {
        let (mut scene, tile_id, _node_id) = setup_scene_with_hit_region();
        let mut processor = InputProcessor::new();

        // The scene_with_hit_region tile has no scroll config registered.
        let ev = processor.process_keyboard_scroll(tile_id, KEYBOARD_PAGE_SCROLL_PX, &mut scene);
        assert!(
            ev.is_none(),
            "keyboard scroll on a non-scrollable tile must return None"
        );
    }

    /// Keyboard scroll coalesces with wheel scroll under the same frame — user wins.
    #[test]
    fn test_keyboard_scroll_coalesces_like_wheel_scroll() {
        let (mut scene, tile_id) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();

        // Wheel scroll first.
        processor.process_scroll_event(
            &ScrollEvent {
                x: 150.0,
                y: 150.0,
                delta_x: 0.0,
                delta_y: 40.0,
            },
            &mut scene,
        );
        // Then keyboard scroll (same frame — no commit between).
        processor.process_keyboard_scroll(tile_id, KEYBOARD_PAGE_SCROLL_PX, &mut scene);

        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        let expected = 40.0 + KEYBOARD_PAGE_SCROLL_PX;
        assert!(
            (offset_y - expected).abs() < 1e-4,
            "combined wheel + keyboard scroll must accumulate; expected {expected}, got {offset_y}"
        );
    }

    // ── hud-8lpu: local-first invariant before agent dispatch ─────────────────

    /// AC §4 (hud-8lpu): local scroll offset is updated synchronously before
    /// the `ScrollOffsetChangedEvent` is returned for agent dispatch.
    ///
    /// `process_scroll_event` must:
    /// 1. Update the scene graph tile offset (local-first, < 4ms p99).
    /// 2. Return a `ScrollOffsetChangedEvent` whose fields match the new offset.
    ///
    /// The windowed runtime uses the returned event to build an `EventBatch`
    /// for injection via `input_event_tx`; this test verifies that the event
    /// carries the correct committed offset (not a stale or zero value).
    #[test]
    fn test_scroll_local_update_before_agent_event() {
        let (mut scene, tile_id) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();

        let ev = processor.process_scroll_event(
            &ScrollEvent {
                x: 150.0,
                y: 150.0,
                delta_x: 0.0,
                delta_y: 80.0,
            },
            &mut scene,
        );

        // process_scroll_event must return Some for a scrollable tile hit.
        let ev = ev.expect(
            "process_scroll_event must return ScrollOffsetChangedEvent for a scrollable tile",
        );

        // The returned event must carry the committed offset, not zero.
        assert!(
            (ev.offset_y - 80.0).abs() < 1e-4,
            "event.offset_y must equal the applied delta (80.0), got {}",
            ev.offset_y
        );
        assert!(
            ev.offset_x.abs() < 1e-4,
            "event.offset_x must be 0.0 for y-only scroll, got {}",
            ev.offset_x
        );

        // The scene tile scroll offset must also reflect the update (local-first).
        let (scene_x, scene_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (scene_y - 80.0).abs() < 1e-4,
            "scene tile offset_y must be 80.0 after wheel scroll, got {scene_y}"
        );
        assert!(
            scene_x.abs() < 1e-4,
            "scene tile offset_x must be 0.0 for y-only scroll, got {scene_x}"
        );
    }

    /// AC §4 (hud-8lpu): keyboard scroll (PgDn) also updates local offset before
    /// returning the notification event.
    #[test]
    fn test_keyboard_scroll_local_update_before_agent_event() {
        let (mut scene, tile_id) = setup_scrollable_scene();
        let mut processor = InputProcessor::new();

        let ev = processor.process_keyboard_scroll(tile_id, KEYBOARD_PAGE_SCROLL_PX, &mut scene);

        let ev = ev.expect(
            "process_keyboard_scroll must return ScrollOffsetChangedEvent for a scrollable tile",
        );

        assert!(
            (ev.offset_y - KEYBOARD_PAGE_SCROLL_PX).abs() < 1e-4,
            "event.offset_y must equal KEYBOARD_PAGE_SCROLL_PX, got {}",
            ev.offset_y
        );

        let (_, scene_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (scene_y - KEYBOARD_PAGE_SCROLL_PX).abs() < 1e-4,
            "scene tile offset_y must equal KEYBOARD_PAGE_SCROLL_PX, got {scene_y}"
        );
    }

    // ── Spec task 3.2 / 3.3 end-to-end behavioural tests ──────────────────────

    /// Spec task 3.2 — at-tail tile advances by whole lines on append.
    ///
    /// `notify_tile_content_appended` on an at-tail tile must:
    /// 1. Advance the scroll offset by `floor(delta / line_h) * line_h`.
    /// 2. Update `scene.tile_follow_tail_at_tail(tile_id)` to `true` (at-tail preserved).
    /// 3. Apply the new offset to the scene for the compositor.
    #[test]
    fn spec_3_2_at_tail_tile_advances_by_whole_lines_on_append() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("test", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "test",
                lease_id,
                // viewport_height = 300px (5 × 60px lines = room for 5 lines)
                Rect::new(0.0, 0.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        // Register with content_height=0 (no content yet; first append populates it).
        scene
            .register_tile_scroll_config(
                tile_id,
                tze_hud_scene::TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: None,
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();
        let line_h = 60.0_f32;
        let viewport_h = 300.0_f32; // 5 lines
        let new_content = 8.0 * line_h; // 8 lines total (3 new lines overflow)

        let changed = processor.notify_tile_content_appended(
            tile_id,
            new_content,
            viewport_h,
            line_h,
            &mut scene,
        );

        assert!(
            changed,
            "spec 3.2: offset must change when at-tail and new content overflows the viewport"
        );

        // Offset must have advanced by exactly 3 whole lines (8 lines - 5 visible = 3).
        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        let expected_offset = 3.0 * line_h; // 180px
        assert!(
            (offset_y - expected_offset).abs() < 1.0,
            "spec 3.2: at-tail offset must advance to {expected_offset}px; got {offset_y}px"
        );

        // Tile remains at-tail in the scene.
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "spec 3.2: scene must reflect AtTail after at-tail append"
        );
    }

    /// Spec task 3.3 — append does not disturb a scrolled-back viewport.
    ///
    /// After the user scrolls back, `notify_tile_content_appended` must:
    /// 1. Leave the scroll offset unchanged.
    /// 2. Update `scene.tile_follow_tail_at_tail(tile_id)` to `false` (ScrolledBack preserved).
    #[test]
    fn spec_3_3_scrolled_back_append_does_not_disturb_viewport() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("test", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "test",
                lease_id,
                // viewport = 100px (5 × 20px lines)
                Rect::new(100.0, 100.0, 400.0, 100.0),
                1,
            )
            .unwrap();
        // Start with 20 lines of content.
        scene
            .register_tile_scroll_config(
                tile_id,
                tze_hud_scene::TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: Some(300.0), // max-scroll = 400px - 100px viewport = 300px
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32;

        // Scroll to the tail (offset 300 = max-scroll for 20 lines × 20px - 100px viewport).
        let _ = processor.process_scroll_event(
            &ScrollEvent {
                x: 200.0,
                y: 150.0,
                delta_x: 0.0,
                delta_y: 300.0,
            },
            &mut scene,
        );
        // Scroll back up 120px (6 lines) so anchor becomes ScrolledBack.
        let _ = processor.process_scroll_event(
            &ScrollEvent {
                x: 200.0,
                y: 150.0,
                delta_x: 0.0,
                delta_y: -120.0,
            },
            &mut scene,
        );

        let (_, offset_before) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            !scene.tile_follow_tail_at_tail(tile_id),
            "spec 3.3: anchor must be ScrolledBack after user scrolled up"
        );

        // Append 5 more lines.
        let new_content = 25.0 * line_h;
        let changed = processor.notify_tile_content_appended(
            tile_id,
            new_content,
            viewport_h,
            line_h,
            &mut scene,
        );

        assert!(
            !changed,
            "spec 3.3: offset must NOT change when ScrolledBack and content grows"
        );

        let (_, offset_after) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (offset_after - offset_before).abs() < 1.0,
            "spec 3.3: scroll offset must be stable after append; \
             before={offset_before}px, after={offset_after}px"
        );

        // Still scrolled-back in the scene.
        assert!(
            !scene.tile_follow_tail_at_tail(tile_id),
            "spec 3.3: scene must still reflect ScrolledBack after append"
        );
    }

    // ── reset_tile_scroll_to_tail (hud-qbcp8) ─────────────────────────────────

    /// A viewer typing/submitting their OWN input is a deliberate local action,
    /// distinct from ordinary remote content growth: `reset_tile_scroll_to_tail`
    /// must force a `ScrolledBack` tile back to the tail and publish both the
    /// offset and the follow-tail flag to the scene, unlike
    /// `notify_tile_content_appended` (spec 3.3), which leaves it alone.
    #[test]
    fn reset_tile_scroll_to_tail_forces_scrolled_back_tile_and_syncs_scene() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("test", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(100.0, 100.0, 400.0, 100.0), // viewport = 100px
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(tile_id, tze_hud_scene::TileScrollConfig::vertical())
            .unwrap();

        let mut processor = InputProcessor::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32;

        // 20 lines of content (400px) — tail = 400 - 100 = 300px.
        processor.notify_tile_content_appended(
            tile_id,
            20.0 * line_h,
            viewport_h,
            line_h,
            &mut scene,
        );
        let (_, tail_offset) = scene.tile_scroll_offset_local(tile_id);
        assert!(tail_offset > 0.0, "tail offset should be nonzero here");

        // Scroll back up.
        let _ = processor.process_scroll_event(
            &ScrollEvent {
                x: 200.0,
                y: 150.0,
                delta_x: 0.0,
                delta_y: -120.0,
            },
            &mut scene,
        );
        assert!(!scene.tile_follow_tail_at_tail(tile_id));

        let changed = processor.reset_tile_scroll_to_tail(tile_id, &mut scene);
        assert!(changed, "reset must report the offset changed");

        let (_, offset_after) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (offset_after - tail_offset).abs() < f32::EPSILON,
            "reset must snap the scene offset back to the tail ({tail_offset}); got {offset_after}"
        );
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "reset must publish AtTail to the scene"
        );
    }

    /// `reset_tile_scroll_to_tail` on a tile with no tracked scroll state is a
    /// no-op and must not touch the scene.
    #[test]
    fn reset_tile_scroll_to_tail_unregistered_tile_is_noop() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("test", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(0.0, 0.0, 400.0, 100.0),
                1,
            )
            .unwrap();

        let mut processor = InputProcessor::new();
        assert!(!processor.reset_tile_scroll_to_tail(tile_id, &mut scene));
        assert_eq!(processor.tile_total_content_height_px(tile_id), 0.0);
    }

    /// `tile_total_content_height_px` lets a caller read back the running
    /// total tracked by `notify_tile_content_appended` and add its own delta,
    /// instead of maintaining a duplicate counter (hud-qbcp8's incremental
    /// input-history height bookkeeping).
    #[test]
    fn tile_total_content_height_px_supports_incremental_append_callers() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("test", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(0.0, 0.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(tile_id, tze_hud_scene::TileScrollConfig::vertical())
            .unwrap();

        let mut processor = InputProcessor::new();
        let line_h = 21.0_f32;
        let viewport_h = 300.0_f32;

        // First entry: 3 lines.
        let total = processor.tile_total_content_height_px(tile_id) + 3.0 * line_h;
        processor.notify_tile_content_appended(tile_id, total, viewport_h, line_h, &mut scene);
        assert!((processor.tile_total_content_height_px(tile_id) - 3.0 * line_h).abs() < 0.01);

        // Second entry: 2 more lines, read back and added incrementally.
        let total = processor.tile_total_content_height_px(tile_id) + 2.0 * line_h;
        processor.notify_tile_content_appended(tile_id, total, viewport_h, line_h, &mut scene);
        assert!((processor.tile_total_content_height_px(tile_id) - 5.0 * line_h).abs() < 0.01);
    }

    /// Coordinate reconciliation: `total_content_height_px` is stored correctly
    /// so that repeated appends produce correct offsets.
    ///
    /// `ScrollConfig.content_height` = MAX-SCROLL-OFFSET (total - viewport).
    /// `follow_tail_offset` uses TOTAL CONTENT PIXELS.
    /// Mixing these would produce wrong offsets on the second append.
    #[test]
    fn coordinate_reconciliation_total_vs_max_scroll_offset() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("test", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(0.0, 0.0, 400.0, 100.0),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(
                tile_id,
                tze_hud_scene::TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: None,
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32; // 5 lines

        // First append: 8 lines (160px total). Tile is at-tail, so offset advances to
        // 8*20 - 100 = 60px (3 lines above viewport bottom).
        processor.notify_tile_content_appended(
            tile_id,
            8.0 * line_h,
            viewport_h,
            line_h,
            &mut scene,
        );
        let (_, offset_1) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (offset_1 - 3.0 * line_h).abs() < 1.0,
            "first append: expected offset 60px; got {offset_1}px"
        );

        // Second append: 3 more lines (11 lines = 220px total).
        // With correct coordinate tracking, the new offset should be 220-100 = 120px.
        processor.notify_tile_content_appended(
            tile_id,
            11.0 * line_h,
            viewport_h,
            line_h,
            &mut scene,
        );
        let (_, offset_2) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (offset_2 - 6.0 * line_h).abs() < 1.0,
            "second append: expected offset 120px (6 lines × 20px); got {offset_2}px"
        );
    }

    // ─── Composer draft wiring ────────────────────────────────────────────
    //
    // Spec: §4.1 — runtime-owned draft attached to focused composer regions.
    // Spec: §4.3 — coalesced state-stream notifications.
    // Spec: §4.4 — editing keystrokes are NOT forwarded to agent.
    //
    // These tests validate the end-to-end wiring of `ComposerDraftManager` into
    // `InputProcessor::process_with_focus` and the keyboard routing methods.

    /// Build a scene with two hit regions: one with `accepts_composer_input = true`
    /// and one with `accepts_composer_input = false`.  Returns
    /// `(scene, tab_id, tile_id, composer_node_id, plain_node_id)`.
    fn setup_composer_scene() -> (SceneGraph, SceneId, SceneId, SceneId, SceneId) {
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

        // Composer node (top half of tile)
        let composer_id = SceneId::new();
        let plain_id = SceneId::new();
        let composer_node = Node {
            layout: Default::default(),
            id: composer_id,
            children: vec![plain_id],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 0.0, 800.0, 60.0),
                interaction_id: "composer-input".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                accepts_composer_input: true,
                ..Default::default()
            }),
        };
        // Plain focusable node (bottom half of tile)
        let plain_node = Node {
            layout: Default::default(),
            id: plain_id,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 100.0, 800.0, 100.0),
                interaction_id: "plain-button".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                accepts_composer_input: false,
                ..Default::default()
            }),
        };
        scene.nodes.insert(composer_id, composer_node);
        scene.nodes.insert(plain_id, plain_node);
        scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(composer_id);

        (scene, tab_id, tile_id, composer_id, plain_id)
    }

    /// Focus a composer region via `process_with_focus` pointer-down and verify
    /// the `ComposerDraftManager` is activated.
    #[test]
    fn composer_focus_gained_activates_manager() {
        let (mut scene, tab_id, _tile_id, composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        assert!(
            !processor.is_composer_active(),
            "manager must be idle before focus"
        );

        // Pointer-down inside the composer region (bounds: 0,0 → 800,60)
        let event = PointerEvent {
            x: 100.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&event, &mut scene, &mut fm, tab_id);

        assert!(
            processor.is_composer_active(),
            "manager must be active after focusing composer region"
        );
        assert_eq!(
            processor.composer_draft_manager.focused_node(),
            Some(composer_id),
            "focused_node must be the composer node_id"
        );
    }

    /// hud-se6hs: a composer node with no `composer_placeholder` override
    /// reports `None` in the `composer_draft_snapshot` tuple's placeholder
    /// slot — the caller (windowed runtime) falls back to its own default.
    #[test]
    fn composer_draft_snapshot_reports_no_override_when_unset() {
        let (mut scene, tab_id, _tile_id, _composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let event = PointerEvent {
            x: 100.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&event, &mut scene, &mut fm, tab_id);

        let snapshot = processor
            .composer_draft_snapshot()
            .expect("composer must be active after focus");
        assert_eq!(
            snapshot.5, None,
            "no per-composer override configured — must report None (inherit default)"
        );
    }

    /// hud-se6hs: a composer node whose `HitRegionNode::composer_placeholder`
    /// carries a custom hint threads that hint end-to-end through
    /// `process_with_focus` → `composer_draft_snapshot`, overriding the
    /// runtime's global default.
    #[test]
    fn composer_draft_snapshot_reports_custom_per_node_override() {
        let (mut scene, tab_id, _tile_id, composer_id, _plain_id) = setup_composer_scene();
        // Configure a non-chat composer's own hint copy on the owning node.
        if let Some(node) = scene.nodes.get_mut(&composer_id) {
            if let NodeData::HitRegion(hr) = &mut node.data {
                hr.composer_placeholder = Box::new(Some("Search…".to_string()));
            }
        }
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let event = PointerEvent {
            x: 100.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&event, &mut scene, &mut fm, tab_id);

        let snapshot = processor
            .composer_draft_snapshot()
            .expect("composer must be active after focus");
        assert_eq!(
            snapshot.5,
            Some("Search…".to_string()),
            "the owning node's composer_placeholder must reach the snapshot verbatim"
        );
    }

    /// hud-se6hs: `Some("")` on `HitRegionNode::composer_placeholder` is an
    /// explicit opt-out, distinct from the unset (`None`) case above — it
    /// must round-trip through the snapshot as `Some("")`, not `None`, so the
    /// windowed runtime can tell "no config" apart from "explicitly disabled".
    #[test]
    fn composer_draft_snapshot_reports_explicit_opt_out_distinctly() {
        let (mut scene, tab_id, _tile_id, composer_id, _plain_id) = setup_composer_scene();
        if let Some(node) = scene.nodes.get_mut(&composer_id) {
            if let NodeData::HitRegion(hr) = &mut node.data {
                hr.composer_placeholder = Box::new(Some(String::new()));
            }
        }
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let event = PointerEvent {
            x: 100.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&event, &mut scene, &mut fm, tab_id);

        let snapshot = processor
            .composer_draft_snapshot()
            .expect("composer must be active after focus");
        assert_eq!(
            snapshot.5,
            Some(String::new()),
            "explicit opt-out must round-trip as Some(\"\"), distinct from unset None"
        );
    }

    /// Regression (hud-v0cal): the composer must be focusable and editable
    /// WITHOUT a pointer.  A Tab key-down (keyboard focus traversal) must route
    /// focus onto the composer region and activate the draft manager, and a
    /// subsequent character key must edit the draft — exactly the pointerless
    /// flow the `portal-composer-interaction-completeness` change requires for
    /// no-pointer surfaces (smart glasses / Mobile Presence Node).
    ///
    /// Spec change `portal-composer-interaction-completeness`, requirement
    /// "Transcript Interaction Contract", scenario "composer is focusable
    /// without a pointer".
    #[test]
    fn tab_traversal_focuses_composer_without_pointer() {
        let (mut scene, tab_id, _tile_id, composer_id, plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        assert!(
            !processor.is_composer_active(),
            "composer must be idle before any focus traversal"
        );

        // NO pointer event has been processed — only a Tab key-down, which the
        // windowed keyboard path maps to navigate_focus(reverse = false).
        let transition = processor.navigate_focus(&mut fm, &mut scene, tab_id, false);

        let (gained, _ns) = transition
            .gained
            .expect("Tab must move focus onto the first focusable element");
        assert_eq!(
            gained.node_id,
            Some(composer_id),
            "Tab focus must land on the composer node (first focusable in z-order/DFS)"
        );
        assert_eq!(
            gained.source,
            FocusSource::TabKey,
            "keyboard traversal must report FocusSource::TabKey"
        );
        assert!(
            processor.is_composer_active(),
            "composer draft manager must activate from keyboard focus (no pointer)"
        );

        // A subsequent character key edits the draft (the composer is now the
        // keystroke sink — §4.1 keystroke routing).
        let (outcome, _batch) = processor.route_character_to_composer("x");
        assert_eq!(
            outcome,
            EditOutcome::Mutated,
            "a character key must edit the composer draft after Tab focus"
        );
        let snapshot = processor
            .composer_draft_snapshot()
            .expect("composer draft snapshot must exist while active");
        assert_eq!(
            snapshot.0, "x",
            "the typed character must appear in the composer draft (pointerless edit)"
        );

        // Scoping: a second Tab advances to the next focusable region (the plain
        // node, which does NOT accept composer input), so the composer
        // deactivates — traversal respects per-node focus scoping.
        let next = processor.navigate_focus(&mut fm, &mut scene, tab_id, false);
        let (gained2, _ns2) = next
            .gained
            .expect("a second Tab must advance focus to the next focusable element");
        assert_eq!(
            gained2.node_id,
            Some(plain_id),
            "second Tab must advance to the next focusable (non-composer) region"
        );
        assert!(
            !processor.is_composer_active(),
            "composer must deactivate once focus leaves the composer region"
        );
    }

    // ─── Portal typing-recovery (hud-2v8br) ──────────────────────────────
    //
    // A keyboard-only viewer must never be stranded: when Tab moves focus off
    // the composer onto a portal control, typed text must recover to the
    // composer instead of being swallowed. These tests pin the classification
    // and recovery primitives the runtime keyboard path drives.

    /// A non-composer focusable control that shares a tile with a composer node
    /// classifies as `Control`, carrying the sibling composer as the recovery
    /// target.
    #[test]
    fn classify_portal_focus_control_with_composer_sibling() {
        let (mut scene, tab_id, tile_id, composer_id, plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Tab twice: composer → plain control.
        processor.navigate_focus(&mut fm, &mut scene, tab_id, false);
        processor.navigate_focus(&mut fm, &mut scene, tab_id, false);

        let owner = fm.current_owner(tab_id).clone();
        let target = processor.classify_portal_focus(&owner, &scene);
        assert_eq!(
            target,
            PortalFocusTarget::Control {
                tile_id,
                node_id: plain_id,
                interaction_id: "plain-button".to_string(),
                composer_node: composer_id,
            },
            "a non-composer control sharing a tile with a composer must classify as Control"
        );
    }

    /// Focus on the composer region classifies as `Composer` (normal editing).
    #[test]
    fn classify_portal_focus_composer_is_normal_editing() {
        let (mut scene, tab_id, _tile_id, _composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        processor.navigate_focus(&mut fm, &mut scene, tab_id, false); // → composer
        let owner = fm.current_owner(tab_id).clone();
        assert_eq!(
            processor.classify_portal_focus(&owner, &scene),
            PortalFocusTarget::Composer,
            "focus on a composer-input region must classify as Composer"
        );
    }

    /// Recovery redirects focus to the composer, activates the draft manager, and
    /// the triggering character then edits the draft — the "typing refocuses the
    /// composer and applies the keystroke" policy end-to-end.
    #[test]
    fn recover_composer_focus_activates_and_accepts_typing() {
        let (mut scene, tab_id, tile_id, composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Land on the plain control (2nd stop) and confirm the composer is idle.
        processor.navigate_focus(&mut fm, &mut scene, tab_id, false);
        processor.navigate_focus(&mut fm, &mut scene, tab_id, false);
        assert!(
            !processor.is_composer_active(),
            "composer must be idle while a non-composer control holds focus"
        );

        // Recover to the composer (as the runtime does on a printable keystroke).
        let transition =
            processor.recover_composer_focus(&mut fm, &mut scene, tab_id, tile_id, composer_id);
        assert_eq!(
            transition.gained.as_ref().and_then(|(g, _)| g.node_id),
            Some(composer_id),
            "recovery must move focus onto the composer node"
        );
        assert_eq!(
            transition.gained.as_ref().map(|(g, _)| g.source),
            Some(FocusSource::CommandInput),
            "recovery focus source must be CommandInput"
        );
        assert!(
            processor.is_composer_active(),
            "recovery must activate the composer draft manager"
        );

        // The character that triggered recovery now edits the draft.
        let (outcome, _batch) = processor.route_character_to_composer("h");
        assert_eq!(outcome, EditOutcome::Mutated);
        assert_eq!(
            processor.composer_draft_snapshot().map(|s| s.0),
            Some("h".to_string()),
            "the typed character must land in the composer draft after recovery"
        );
    }

    /// Cycle integrity: from any stop, repeated Tab returns focus to the composer
    /// within the cycle length — a keyboard user is never permanently stranded.
    #[test]
    fn tab_cycle_returns_to_composer_within_cycle_length() {
        let (mut scene, tab_id, _tile_id, composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Start on the composer.
        processor.navigate_focus(&mut fm, &mut scene, tab_id, false);
        assert_eq!(fm.current_owner(tab_id).node_id(), Some(composer_id));

        // From anywhere, pressing Tab at most `bound` times must revisit the
        // composer (the cycle wraps). `bound` is a generous upper limit on the
        // number of focusable stops in this scene.
        let bound = 8;
        let mut returned = false;
        for _ in 0..bound {
            processor.navigate_focus(&mut fm, &mut scene, tab_id, false);
            if fm.current_owner(tab_id).node_id() == Some(composer_id) {
                returned = true;
                break;
            }
        }
        assert!(
            returned,
            "repeated Tab must return focus to the composer within {bound} presses"
        );
    }

    /// Click recovery (hud-2v8br regression): clicking the composer region while
    /// a non-composer control holds keyboard focus must restore composer focus
    /// and re-activate the draft (the pointer click-to-focus path from #981).
    #[test]
    fn click_composer_recovers_focus_from_control() {
        let (mut scene, tab_id, _tile_id, composer_id, plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Park focus on the plain control via Tab traversal.
        processor.navigate_focus(&mut fm, &mut scene, tab_id, false);
        processor.navigate_focus(&mut fm, &mut scene, tab_id, false);
        assert_eq!(fm.current_owner(tab_id).node_id(), Some(plain_id));
        assert!(
            !processor.is_composer_active(),
            "composer must be idle while the control holds focus"
        );

        // Click inside the composer region (bounds: 0,0 → 800,60).
        let event = PointerEvent {
            x: 100.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&event, &mut scene, &mut fm, tab_id);

        assert_eq!(
            fm.current_owner(tab_id).node_id(),
            Some(composer_id),
            "clicking the composer must move focus back onto it"
        );
        assert!(
            processor.is_composer_active(),
            "clicking the composer must re-activate the draft manager"
        );
    }

    /// Focus a plain (non-composer) region: manager must remain idle.
    #[test]
    fn non_composer_focus_does_not_activate_manager() {
        let (mut scene, tab_id, tile_id, _composer_id, plain_id) = setup_composer_scene();

        // Move the plain node to the root so it can be hit-tested directly.
        // Re-root the tile to the plain node so the pointer-down in its bounds hits it.
        let plain_node_for_root = Node {
            layout: Default::default(),
            id: plain_id,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
                interaction_id: "plain-button".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                accepts_composer_input: false,
                ..Default::default()
            }),
        };
        scene.nodes.insert(plain_id, plain_node_for_root);
        scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(plain_id);

        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let event = PointerEvent {
            x: 100.0,
            y: 200.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&event, &mut scene, &mut fm, tab_id);

        assert!(
            !processor.is_composer_active(),
            "manager must NOT be active for a non-composer region"
        );
    }

    /// Focus composer → feed characters → verify draft buffer and coalesced batch.
    ///
    /// Spec §4.1, §4.3: characters routed into draft buffer; adapter receives a
    /// coalesced state-stream notification (not per-keystroke events).
    #[test]
    fn composer_character_routing_fills_draft_and_flushes_notification() {
        let (mut scene, tab_id, _tile_id, _composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Focus the composer region.
        let down = PointerEvent {
            x: 50.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&down, &mut scene, &mut fm, tab_id);
        assert!(processor.is_composer_active());

        // Route three characters — no flush yet.
        let (outcome_h, _) = processor.route_character_to_composer("h");
        let (outcome_i, _) = processor.route_character_to_composer("i");
        let (outcome_excl, _) = processor.route_character_to_composer("!");

        assert_eq!(outcome_h, EditOutcome::Mutated);
        assert_eq!(outcome_i, EditOutcome::Mutated);
        assert_eq!(outcome_excl, EditOutcome::Mutated);

        // Before flush: no batch has been delivered.
        // Draft text is buffered locally.
        assert_eq!(
            processor.composer_draft_manager.draft().map(|d| d.text()),
            Some("hi!"),
            "draft buffer must contain all inserted characters"
        );

        // Flush at settle point — must deliver the coalesced notification.
        let batch = processor
            .try_flush_composer_draft()
            .expect("flush at settle must produce a batch when edits are pending");

        let notif = batch
            .latest
            .expect("batch must contain a state notification");
        assert_eq!(
            notif.text, "hi!",
            "notification text must match draft buffer"
        );
        assert_eq!(notif.cursor, 3, "cursor at end of text");
        assert!(!notif.at_capacity, "draft not at capacity");
        assert_eq!(batch.submission, None, "no submission yet");
        assert_eq!(batch.cancel, None, "no cancel yet");
    }

    /// hud-cpjqe: a portal header BAND drags on the first pointer-move with NO
    /// 250 ms long-press hold and NO early-movement cancel (Windows-titlebar) —
    /// the fix for "clicking top to drag fails half the time". The legacy grip
    /// keeps the long-press hysteresis (a fast move before the hold cancels).
    #[test]
    fn header_band_drag_activates_immediately_grip_still_requires_long_press() {
        use tze_hud_scene::{DragHandleElementKind, Rect};
        let eid = SceneId::new();
        let ev = |x: f32, kind| PointerEvent {
            x,
            y: 120.0,
            kind,
            device_id: 0,
            timestamp: None,
        };

        // ── BAND: down, then a large move immediately → Activated, then Moved ──
        let mut proc_band = InputProcessor::new();
        let band_bounds = Rect::new(100.0, 100.0, 600.0, 52.0);
        let down = proc_band.process_drag_handle_pointer(
            &ev(400.0, PointerEventKind::Down),
            "drag-handle:band",
            eid,
            DragHandleElementKind::Tile,
            band_bounds,
            1920.0,
            1080.0,
            true,
        );
        assert!(matches!(down, DragEventOutcome::Accumulating { .. }));
        // 60dp move with zero hold — a legacy grip would cancel here.
        let activated = proc_band.process_drag_handle_pointer(
            &ev(460.0, PointerEventKind::Move),
            "drag-handle:band",
            eid,
            DragHandleElementKind::Tile,
            band_bounds,
            1920.0,
            1080.0,
            true,
        );
        assert!(
            matches!(activated, DragEventOutcome::Activated { .. }),
            "the header band must activate on the first move, got {activated:?}"
        );
        let moved = proc_band.process_drag_handle_pointer(
            &ev(500.0, PointerEventKind::Move),
            "drag-handle:band",
            eid,
            DragHandleElementKind::Tile,
            band_bounds,
            1920.0,
            1080.0,
            false,
        );
        assert!(
            matches!(moved, DragEventOutcome::Moved { .. }),
            "the header band must move once active, got {moved:?}"
        );

        // ── GRIP: same fast move before the hold → Cancelled (unchanged) ──────
        let mut proc_grip = InputProcessor::new();
        let grip_bounds = Rect::new(100.0, 100.0, 24.0, 8.0);
        proc_grip.process_drag_handle_pointer(
            &ev(112.0, PointerEventKind::Down),
            "drag-handle:grip",
            eid,
            DragHandleElementKind::Tile,
            grip_bounds,
            1920.0,
            1080.0,
            false,
        );
        let cancelled = proc_grip.process_drag_handle_pointer(
            &ev(172.0, PointerEventKind::Move),
            "drag-handle:grip",
            eid,
            DragHandleElementKind::Tile,
            grip_bounds,
            1920.0,
            1080.0,
            false,
        );
        assert!(
            matches!(cancelled, DragEventOutcome::Cancelled),
            "the legacy grip must still cancel a fast move before the long-press, got {cancelled:?}"
        );
    }

    /// Feed characters to a non-composer region: characters must NOT go into the
    /// draft buffer.  `route_character_to_composer` returns Unchanged when no
    /// composer is active.
    ///
    /// Spec §4.4: editing keystrokes are never terminal/provider input; conversely,
    /// the manager must not intercept characters when no composer is focused.
    #[test]
    fn no_composer_focus_route_character_returns_unchanged() {
        let mut processor = InputProcessor::new();

        // No focus at all — manager is idle.
        let (outcome, batch) = processor.route_character_to_composer("a");
        assert_eq!(
            outcome,
            EditOutcome::Unchanged,
            "no-op when manager is idle"
        );
        assert!(batch.is_none(), "no batch when manager is idle");
        assert!(processor.composer_draft_manager.draft().is_none());
    }

    /// Focus composer → submit via Enter → verify transactional batch with submission.
    ///
    /// Spec §4.3: submission is transactional; post-submit clear notification is
    /// emitted in the same batch.
    #[test]
    fn composer_submit_via_enter_produces_transactional_batch() {
        let (mut scene, tab_id, _tile_id, _composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Focus.
        let down = PointerEvent {
            x: 50.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&down, &mut scene, &mut fm, tab_id);

        // Type "send".
        processor.route_character_to_composer("s");
        processor.route_character_to_composer("e");
        processor.route_character_to_composer("n");
        processor.route_character_to_composer("d");

        // Submit via Enter.
        let (consumed, batch_opt) =
            processor.route_key_down_to_composer("Enter", "Enter", false, false, false);

        assert!(consumed, "Enter must be consumed by the composer");
        let batch = batch_opt.expect("Enter must produce an immediate transactional batch");
        let sub = batch
            .submission
            .as_ref()
            .expect("batch must contain a DraftSubmission");
        assert_eq!(sub.text, "send", "submission text must match typed content");
        // Post-submit clear: latest notification should show empty text.
        let clear = batch
            .latest
            .as_ref()
            .expect("post-submit clear notification must be present");
        assert!(
            clear.text.is_empty(),
            "post-submit clear must have empty text"
        );
        assert!(
            clear.sequence > sub.sequence,
            "clear sequence must be > submission sequence"
        );
    }

    /// Focus composer → blur → verify on_focus_lost is called and the pending
    /// notification is flushed.
    ///
    /// Spec §4.3: blur is a settle point; the terminal draft state is delivered.
    #[test]
    fn composer_focus_lost_on_blur_flushes_pending_state() {
        let (mut scene, tab_id, tile_id, _composer_id, plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Focus composer.
        let down_composer = PointerEvent {
            x: 50.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&down_composer, &mut scene, &mut fm, tab_id);
        assert!(processor.is_composer_active());

        // Type a character without flushing.
        processor.route_character_to_composer("z");

        // Now click the plain node — focus moves to it, triggering on_focus_lost
        // internally in process_with_focus.
        let plain_node_for_root = Node {
            layout: Default::default(),
            id: plain_id,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 100.0, 800.0, 100.0),
                interaction_id: "plain-button".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                accepts_composer_input: false,
                ..Default::default()
            }),
        };
        // Temporarily add the plain node at the top level so hit_test can find it.
        // Set it as root (replaces composer as root for the test hit).
        scene.nodes.insert(plain_id, plain_node_for_root);
        scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(plain_id);

        let down_plain = PointerEvent {
            x: 400.0,
            y: 150.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&down_plain, &mut scene, &mut fm, tab_id);

        // After blur, the manager must be deactivated.
        assert!(
            !processor.is_composer_active(),
            "manager must be deactivated after blurring the composer region"
        );

        // Spec §4.3: blur is a settle point.  The terminal draft state ("z")
        // typed before the blur must be delivered via the next flush call —
        // this is the core guarantee that process_with_focus stores the
        // on_focus_lost() batch in pending_flushed_batch.
        let flush_batch = processor
            .try_flush_composer_draft()
            .expect("flush at settle must deliver the batch from the blur transition");
        let notif = flush_batch
            .latest
            .expect("batch must contain a state notification for the typed text");
        assert_eq!(
            notif.text, "z",
            "flushed notification must carry the draft text that was pending at blur"
        );
    }

    /// Verify that `route_key_down_to_composer` does not consume unknown keys
    /// (e.g. F5) when a composer is active.
    ///
    /// Non-composer keys must fall through to the normal agent dispatch path.
    #[test]
    fn composer_does_not_consume_unknown_keys() {
        let (mut scene, tab_id, _tile_id, _composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let down = PointerEvent {
            x: 50.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&down, &mut scene, &mut fm, tab_id);
        assert!(processor.is_composer_active());

        let (consumed, batch) =
            processor.route_key_down_to_composer("F5", "F5", false, false, false);

        assert!(!consumed, "F5 must NOT be consumed by the composer manager");
        assert!(batch.is_none(), "no batch for an unconsumed key");
    }

    // ── hud-083az: focused-composer multiline paste and pointer selection ─────

    /// Multiline/control text routed to a focused composer via
    /// `route_character_to_composer` must land in the draft buffer (sanitised)
    /// and return a non-Unchanged outcome so the caller does NOT forward it to
    /// the agent's raw character stream.
    ///
    /// Spec §4.4: editing keystrokes are never terminal input while a composer
    /// is focused. A Ctrl+V paste containing "\n" must be consumed by the
    /// composer and must NOT leak to the agent.
    #[test]
    fn focused_composer_multiline_paste_lands_in_draft_not_agent_stream() {
        let (mut scene, tab_id, _tile_id, _composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Focus the composer region.
        let down = PointerEvent {
            x: 50.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&down, &mut scene, &mut fm, tab_id);
        assert!(processor.is_composer_active(), "composer must be active");

        // Simulate a multiline clipboard paste (Ctrl+V content).
        let multiline = "hello\nworld";
        let (outcome, _batch) = processor.route_character_to_composer(multiline);

        // The outcome must NOT be Unchanged — callers use Unchanged as the
        // signal to forward the character to the agent stream.
        assert_ne!(
            outcome,
            EditOutcome::Unchanged,
            "multiline paste must be consumed by the composer (non-Unchanged outcome)"
        );

        // The draft must contain the sanitised text (newline stripped).
        let draft_text = processor
            .composer_draft_manager
            .draft()
            .map(|d| d.text().to_owned())
            .expect("draft must be present after paste");
        assert_eq!(
            draft_text, "helloworld",
            "draft must contain sanitised paste content (newlines stripped)"
        );
        assert!(
            !draft_text.contains('\n') && !draft_text.contains('\r'),
            "draft must not contain any newline characters"
        );
    }

    /// Pure control-character text (e.g. a tab or bell) routed to a focused
    /// composer must similarly be consumed (not leak to agent stream), and the
    /// draft must contain the sanitised result (control chars stripped).
    ///
    /// Regression test for hud-60hgf: `dispatch_character_event` in the
    /// runtime was incorrectly conditioned on `outcome != Unchanged` to decide
    /// whether to block the agent path.  For an all-control-char clipboard
    /// payload the sanitised string is empty, so `paste("")` returns
    /// `EditOutcome::Unchanged` — but the event MUST still not reach the agent
    /// (spec §4.4).  The fix adds an unconditional early-return whenever
    /// `is_composer_active()` is true, regardless of outcome.
    ///
    /// This test verifies:
    /// 1. `is_composer_active()` remains true after the paste (so the runtime
    ///    guard fires correctly).
    /// 2. The outcome IS `Unchanged` — confirming this is the exact case that
    ///    previously leaked.
    /// 3. The draft text is not mutated by the all-control paste.
    #[test]
    fn focused_composer_control_char_paste_is_consumed() {
        let (mut scene, tab_id, _tile_id, _composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let down = PointerEvent {
            x: 50.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&down, &mut scene, &mut fm, tab_id);
        assert!(processor.is_composer_active());

        // Pre-seed the draft with plain text.
        processor.route_character_to_composer("abc");

        // Attempt to route a string that is pure control characters.
        let ctrl_text = "\x01\x07\x1b";
        let (outcome, _batch) = processor.route_character_to_composer(ctrl_text);

        // The draft must be unchanged — the control chars sanitise to empty,
        // so nothing was inserted.
        let draft_text = processor
            .composer_draft_manager
            .draft()
            .map(|d| d.text().to_owned())
            .expect("draft present");
        assert_eq!(
            draft_text, "abc",
            "draft must be unchanged for all-control paste (nothing to insert)"
        );

        // The outcome for an empty sanitised paste is Unchanged (nothing mutated).
        // This is the exact case that triggered the hud-60hgf bug: the runtime's
        // `dispatch_character_event` had `if outcome != Unchanged { return }` which
        // fell through to the agent path when outcome was Unchanged.  The fix adds
        // an unconditional `return` after the composer block whenever
        // `is_composer_active()`.  Assert Unchanged here so any future regression
        // in the sanitise→paste path is immediately visible.
        assert_eq!(
            outcome,
            EditOutcome::Unchanged,
            "all-control-char paste must return Unchanged (empty sanitised text → no mutation)"
        );

        // The composer must still be active after the no-op paste (no terminal
        // event was routed), so the runtime guard can fire.
        assert!(
            processor.is_composer_active(),
            "composer must remain active after all-control paste (no submit/cancel occurred)"
        );
    }

    /// Ctrl+V KeyDown must be consumed by the composer draft manager so it is
    /// never forwarded to the agent as a raw KeyDownEvent (hud-083az).
    #[test]
    fn ctrl_v_key_down_consumed_by_focused_composer() {
        let (mut scene, tab_id, _tile_id, _composer_id, _plain_id) = setup_composer_scene();
        let mut processor = InputProcessor::new();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let down = PointerEvent {
            x: 50.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        processor.process_with_focus(&down, &mut scene, &mut fm, tab_id);
        assert!(processor.is_composer_active());

        // Route Ctrl+V KeyDown — must be consumed.
        let (consumed, batch) =
            processor.route_key_down_to_composer("KeyV", "v", false, true, false);

        assert!(
            consumed,
            "Ctrl+V KeyDown must be consumed by the composer (not forwarded to agent)"
        );
        assert!(
            batch.is_none(),
            "Ctrl+V KeyDown must not produce a draft notification batch"
        );
    }

    /// Pointer selection routing: a pointer-down on a focused composer node
    /// must position the cursor via `route_pointer_selection_to_composer`.
    #[test]
    fn route_pointer_selection_positions_draft_cursor() {
        let mut draft =
            crate::composer_draft::ComposerDraft::new(crate::composer_draft::DEFAULT_DRAFT_CAP);
        // Pre-fill draft with 10 ASCII bytes so we can test cursor positioning.
        for ch in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"] {
            draft.insert(ch);
        }
        assert_eq!(draft.text(), "abcdefghij");

        // Position cursor at byte 4.
        let outcome = draft.set_pointer_selection(4, 4);
        assert_eq!(
            outcome,
            EditOutcome::Mutated,
            "set_pointer_selection must return Mutated when cursor position changes"
        );
        assert_eq!(draft.cursor(), 4, "cursor must be positioned at byte 4");
        assert_eq!(
            draft.selection_anchor(),
            4,
            "anchor must equal cursor for a click (no drag selection)"
        );

        // Calling set_pointer_selection with the same values returns Unchanged.
        let outcome2 = draft.set_pointer_selection(4, 4);
        assert_eq!(
            outcome2,
            EditOutcome::Unchanged,
            "no-op when position is already set"
        );

        // Route via InputProcessor.
        let mut manager = crate::composer_draft::ComposerDraftManager::new();
        // Activate manager with a dummy SceneId.
        let dummy_id = tze_hud_scene::SceneId::new();
        manager.on_focus_gained(dummy_id, false);
        // Insert text.
        manager.route_character("hello");
        // Move cursor to position 3.
        let sel_outcome = manager.route_pointer_selection(3, 3);
        assert_eq!(sel_outcome, EditOutcome::Mutated);
        assert_eq!(
            manager.draft().map(|d| d.cursor()),
            Some(3),
            "draft cursor must be at byte 3 after route_pointer_selection"
        );
    }

    /// `route_pointer_selection_to_composer` returns Unchanged when no composer
    /// is focused (idle manager).
    #[test]
    fn route_pointer_selection_unchanged_when_no_composer() {
        let mut processor = InputProcessor::new();
        let outcome = processor.route_pointer_selection_to_composer(0, 0);
        assert_eq!(
            outcome,
            EditOutcome::Unchanged,
            "pointer selection must return Unchanged when no composer is focused"
        );
    }
}
