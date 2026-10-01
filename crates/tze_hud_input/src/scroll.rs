//! Scroll local feedback — compositor-maintained, agent-free, < 4ms p99.
//!
//! Scroll is a local-first operation: the compositor maintains a scroll offset
//! per scrollable tile and updates it in the same frame the scroll event arrives,
//! without waiting for any agent response. Agents are notified asynchronously via
//! `ScrollOffsetChangedEvent` (non-transactional, coalesced).
//!
//! # Opt-in via ScrollConfig
//!
//! Tiles opt in to scroll behavior by attaching a `ScrollConfig`. A tile without
//! `ScrollConfig` ignores scroll events.
//!
//! # Priority rule
//!
//! If a user scroll event and an agent `SetScrollOffsetRequest` arrive in the same
//! frame, the **user scroll takes priority** and the agent request is
//! discarded. This is enforced in `ScrollTileState::commit_frame` and
//! `ScrollState::commit_all_frames`.
//!
//! # Latency invariant
//!
//! Scroll latency budget = input_to_local_ack p99 < 4ms — same as press state.
//! The scroll path executes entirely on the main thread with no locks or async.

use serde::{Deserialize, Serialize};
use tze_hud_scene::SceneId;

// ─── ScrollConfig ─────────────────────────────────────────────────────────────

/// Scroll behavior configuration for a scrollable tile.
///
/// Attached to a tile (keyed by tile_id in `ScrollState`). A tile without
/// `ScrollConfig` is not scrollable and scroll events pass through.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollConfig {
    /// Whether the tile scrolls horizontally.
    pub scrollable_x: bool,
    /// Whether the tile scrolls vertically.
    pub scrollable_y: bool,
    /// Content width in pixels (used to clamp scroll offset).
    /// `None` = no clamping in x.
    pub content_width: Option<f32>,
    /// Content height in pixels (used to clamp scroll offset).
    /// `None` = no clamping in y.
    pub content_height: Option<f32>,
}

impl ScrollConfig {
    /// Convenience constructor for a vertically-scrollable tile.
    pub fn vertical() -> Self {
        Self {
            scrollable_x: false,
            scrollable_y: true,
            content_width: None,
            content_height: None,
        }
    }

    /// Convenience constructor for a horizontally-scrollable tile.
    pub fn horizontal() -> Self {
        Self {
            scrollable_x: true,
            scrollable_y: false,
            content_width: None,
            content_height: None,
        }
    }

    /// Convenience constructor for a tile that scrolls in both directions.
    pub fn both() -> Self {
        Self {
            scrollable_x: true,
            scrollable_y: true,
            content_width: None,
            content_height: None,
        }
    }
}

// ─── ScrollEvent ──────────────────────────────────────────────────────────────

/// A raw scroll input event from the OS.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScrollEvent {
    /// X position of the pointer when the scroll occurred (display-space).
    pub x: f32,
    /// Y position of the pointer when the scroll occurred (display-space).
    pub y: f32,
    /// Horizontal scroll delta (pixels, positive = scroll right).
    pub delta_x: f32,
    /// Vertical scroll delta (pixels, positive = scroll down).
    pub delta_y: f32,
}

// ─── SetScrollOffsetRequest ───────────────────────────────────────────────────

/// An agent request to programmatically set the scroll offset of a tile.
///
/// If a user scroll event and a `SetScrollOffsetRequest` arrive in the same
/// frame, the user scroll takes priority and this request is discarded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetScrollOffsetRequest {
    pub tile_id: SceneId,
    /// Absolute offset_x to set (pixels from content origin).
    pub offset_x: f32,
    /// Absolute offset_y to set (pixels from content origin).
    pub offset_y: f32,
}

// ─── ScrollOffsetChangedEvent ─────────────────────────────────────────────────

/// Async notification sent to agents after scroll offset changes.
///
/// This event is non-transactional and coalesced: if many scroll events arrive
/// between agent polling cycles, only the final offset is delivered.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScrollOffsetChangedEvent {
    pub tile_id: SceneId,
    /// Current absolute scroll offset_x (pixels from content origin).
    pub offset_x: f32,
    /// Current absolute scroll offset_y (pixels from content origin).
    pub offset_y: f32,
}

// ─── FollowTailAnchor ─────────────────────────────────────────────────────────

/// The scroll-anchor state for a streaming transcript tile.
///
/// This type encodes the spec task 3.2 / 3.3 contract for the follow-tail
/// scroll model:
///
/// - **`AtTail`** — the viewport is currently at the tail of the content.
///   When new content is appended, the scroll offset advances by exactly N
///   whole lines (spec task 3.2: "follow-tail advances by whole lines").
///
/// - **`ScrolledBack`** — the user has scrolled back from the tail.  When new
///   content is appended, the scroll offset is **not changed** (spec task 3.3:
///   "append does not disturb a scrolled-back viewport").
///
/// # Transition rules
///
/// | Event | Before | After |
/// |---|---|---|
/// | User scrolls down to tail | any | `AtTail` |
/// | User scrolls back (up) | `AtTail` | `ScrolledBack` |
/// | Content appended at tail | `AtTail` | `AtTail` (offset updated) |
/// | Content appended at tail | `ScrolledBack` | `ScrolledBack` (offset unchanged) |
/// | Tile registered / reset | — | `AtTail` (default: new tiles start at tail) |
///
/// # Usage
///
/// `FollowTailAnchor` is stored in [`ScrollTileState`] alongside the existing
/// offset fields.  It is updated by [`ScrollTileState::queue_user_scroll`] and
/// consumed by [`ScrollTileState::notify_content_appended`].
#[derive(Clone, Debug, Copy, PartialEq, Eq, Default)]
pub enum FollowTailAnchor {
    /// The viewport is at the tail of the content (default for new tiles).
    #[default]
    AtTail,
    /// The user has scrolled back from the tail.
    ScrolledBack,
}

/// Compute the follow-tail scroll offset for a tile when content is appended.
///
/// Given the previous and new `content_height`, `viewport_height`, and
/// `line_height`, returns the offset that keeps the viewport at the tail after
/// the append, advancing by **whole lines only**.
///
/// If the new content height does not add at least one full line, no change is
/// made (returns the current offset unchanged).  This ensures the "whole-line
/// advancement" invariant from spec task 3.2.
///
/// # Parameters
///
/// - `current_offset_y` — the current scroll offset (pixels from content origin).
/// - `old_content_height` — content height before the append (pixels).
/// - `new_content_height` — content height after the append (pixels).
/// - `viewport_height` — visible tile height (pixels).
/// - `line_height` — line height (pixels); used to quantise the advancement.
///
/// # Returns
///
/// The new `offset_y` value (may equal `current_offset_y` if no whole line was
/// added, or be clamped to `new_content_height - viewport_height` at the tail).
pub fn follow_tail_offset(
    current_offset_y: f32,
    old_content_height: f32,
    new_content_height: f32,
    viewport_height: f32,
    line_height: f32,
) -> f32 {
    // Guard against NaN / infinite inputs: NaN comparisons return false and
    // would bypass the safety guards below; infinite values propagate through
    // arithmetic and cause floor()-to-usize casts to panic.
    if !current_offset_y.is_finite()
        || !old_content_height.is_finite()
        || !new_content_height.is_finite()
        || !viewport_height.is_finite()
        || !line_height.is_finite()
        || line_height <= 0.0
        || viewport_height <= 0.0
    {
        return current_offset_y;
    }

    // New lines added (as a count of whole lines).
    // Use a small tolerance (1/32 of a line) when rounding to defend against
    // floating-point representation errors: e.g. `5.0_f32 * 22.4_f32 = 112.0`
    // and `6.0_f32 * 22.4_f32 = 134.39999...`, so `delta = 22.39999...` which
    // would floor-divide to 0 without the tolerance bump.
    let delta_px = new_content_height - old_content_height;
    if delta_px < line_height * 0.5 {
        // Less than half a line was added — not yet a whole line, no advancement.
        return current_offset_y;
    }
    let tolerance = line_height / 32.0;
    let new_lines = ((delta_px + tolerance) / line_height).floor() as u32;
    if new_lines == 0 {
        return current_offset_y;
    }

    // Advance by exactly `new_lines` whole lines.
    let advanced = current_offset_y + new_lines as f32 * line_height;

    // Clamp to the tail (new_content_height − viewport_height).
    // A tile whose content is shorter than the viewport has no scrollable range;
    // the max meaningful offset is 0.
    let tail_offset = (new_content_height - viewport_height).max(0.0);
    advanced.min(tail_offset)
}

// ─── ScrollTileState ──────────────────────────────────────────────────────────

/// Current scroll state for a single tile.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScrollTileState {
    /// Current horizontal scroll offset (pixels from content origin).
    pub offset_x: f32,
    /// Current vertical scroll offset (pixels from content origin).
    pub offset_y: f32,
    /// Scroll configuration (None = not scrollable).
    pub config: Option<ScrollConfig>,
    /// Pending agent `SetScrollOffsetRequest` for this tile, if any.
    /// Cleared each frame after applying (or discarding due to user scroll).
    pending_agent_request: Option<SetScrollOffsetRequest>,
    /// Whether a user scroll was received this frame.
    user_scroll_this_frame: bool,
    /// Whether the offset changed this frame (set by queue_user_scroll or commit_frame).
    dirty: bool,
    /// Follow-tail anchor state for streaming transcript tiles.
    ///
    /// Defaults to `AtTail` for all new tiles: a freshly created tile starts
    /// with the viewport at the tail of the content.  Transitions to
    /// `ScrolledBack` when the user scrolls up, and back to `AtTail` when the
    /// user scrolls to the tail again.
    ///
    /// Private: external callers must use `ScrollState::follow_tail_anchor()`
    /// for read access.  Mutations must go through `queue_user_scroll` and
    /// `notify_content_appended` to preserve invariants.
    follow_tail: FollowTailAnchor,
    /// Total content height in **physical pixels** (not clamped to viewport).
    ///
    /// # Coordinate-system note
    ///
    /// `ScrollConfig::content_height` is the **maximum scroll offset** —
    /// i.e. `total_content_height_px - viewport_height` — because
    /// `clamp_offsets` uses it as an upper bound for `offset_y`.
    ///
    /// `total_content_height_px` stores the **total content size** (full height
    /// of all rendered lines) so that `follow_tail_offset` receives consistent
    /// TOTAL-PIXELS values for both old and new content.  The two fields must
    /// stay in sync:
    ///
    /// ```text
    /// config.content_height = (total_content_height_px - viewport_height).max(0)
    /// ```
    ///
    /// `notify_content_appended` is the sole mutator of both fields and
    /// maintains this invariant.
    total_content_height_px: f32,
}

impl ScrollTileState {
    pub fn new(config: ScrollConfig) -> Self {
        // When content_height is supplied on creation it is treated as
        // MAX-SCROLL-OFFSET (the external contract).  total_content_height_px
        // is not yet known (no content), so we leave it at 0.0.  The first
        // call to notify_content_appended will set both fields consistently.
        Self {
            offset_x: 0.0,
            offset_y: 0.0,
            config: Some(config),
            pending_agent_request: None,
            user_scroll_this_frame: false,
            dirty: false,
            follow_tail: FollowTailAnchor::AtTail,
            total_content_height_px: 0.0,
        }
    }

    /// Queue a user scroll delta for this frame.
    ///
    /// Updates the follow-tail anchor: a positive y-delta (scroll down) that
    /// brings the viewport to the tail transitions the anchor back to `AtTail`;
    /// any upward scroll transitions it to `ScrolledBack`.
    pub fn queue_user_scroll(&mut self, delta_x: f32, delta_y: f32) {
        if let Some(config) = &self.config {
            if config.scrollable_x {
                self.offset_x += delta_x;
            }
            if config.scrollable_y {
                self.offset_y += delta_y;
            }
            self.user_scroll_this_frame = true;
            self.dirty = true;
            self.clamp_offsets();
            // Update follow-tail anchor after clamping so we can compare against
            // the tail boundary.
            self.update_follow_tail_anchor_after_user_scroll(delta_y);
        }
    }

    /// Update the follow-tail anchor after a user scroll gesture.
    ///
    /// If the user scrolled backward (negative delta_y = up), transition to
    /// `ScrolledBack`.  If they scrolled forward (positive delta_y = down) and
    /// are now at the tail, transition back to `AtTail`.
    fn update_follow_tail_anchor_after_user_scroll(&mut self, delta_y: f32) {
        if let Some(config) = &self.config {
            if delta_y < 0.0 {
                // Scrolled up — user moved away from tail.
                self.follow_tail = FollowTailAnchor::ScrolledBack;
            } else if delta_y > 0.0 {
                // Scrolled down — check if we reached the tail.
                let tail_offset = config.content_height.map(|ch| ch.max(0.0)).unwrap_or(0.0);
                // If there is no content_height bound (free scroll), or we are at
                // or beyond the tail offset, mark as AtTail.
                if config.content_height.is_none() || self.offset_y >= tail_offset {
                    self.follow_tail = FollowTailAnchor::AtTail;
                }
            }
        }
    }

    /// Notify this tile that new content has been appended (e.g. a streaming
    /// transcript received new lines).
    ///
    /// Implements spec task 3.2 ("follow-tail advances by whole lines") and
    /// task 3.3 ("append does not disturb a scrolled-back viewport"):
    ///
    /// - When `self.follow_tail == AtTail`, the scroll offset advances by
    ///   whole lines to track the new tail.
    /// - When `self.follow_tail == ScrolledBack`, the scroll offset is
    ///   unchanged; only `content_height` is updated.
    ///
    /// # Coordinate-system contract
    ///
    /// `new_content_height` is the **total content height in physical pixels**
    /// (all rendered lines, including those above the viewport).
    ///
    /// Internally this updates two values that use different coordinate systems:
    ///
    /// * `self.total_content_height_px` ← `new_content_height` (total pixels)
    ///   Used by `follow_tail_offset` to compute whole-line advancement.
    ///
    /// * `config.content_height` ← `(new_content_height − viewport_height).max(0)`
    ///   (max scroll offset)  Used by `clamp_offsets` and
    ///   `update_follow_tail_anchor_after_user_scroll` to clamp `offset_y`.
    ///
    /// The invariant is:
    /// ```text
    /// config.content_height = (total_content_height_px - viewport_height).max(0)
    /// ```
    ///
    /// # Parameters
    ///
    /// - `new_content_height` — **total** content height (physical pixels) after
    ///   the append.  This is the full height of all rendered lines, not the
    ///   scrollable range.
    /// - `viewport_height` — visible tile height (pixels).
    /// - `line_height` — line height (pixels); used to quantise advancement.
    ///
    /// # Returns
    ///
    /// `true` if the scroll offset changed (dirty), `false` if the anchor was
    /// `ScrolledBack` and the offset was left unchanged.
    pub fn notify_content_appended(
        &mut self,
        new_content_height: f32,
        viewport_height: f32,
        line_height: f32,
    ) -> bool {
        // `old_total` is in TOTAL CONTENT PIXELS for follow_tail_offset.
        // We stored the previous total in self.total_content_height_px; on the
        // very first call (initial state = 0.0) that is correct: there was no
        // prior content.
        let old_total = self.total_content_height_px;

        // Update the total-pixels field (used by follow_tail_offset).
        self.total_content_height_px = new_content_height;

        // Update config.content_height as MAX-SCROLL-OFFSET so that clamp_offsets
        // and update_follow_tail_anchor_after_user_scroll use the correct bound.
        //
        // max_scroll_offset = total_content − viewport  (clamped to 0 when content ≤ viewport)
        let max_scroll_offset = (new_content_height - viewport_height).max(0.0);
        if let Some(config) = &mut self.config {
            config.content_height = Some(max_scroll_offset);
        }

        match self.follow_tail {
            FollowTailAnchor::ScrolledBack => {
                // Task 3.3: do NOT disturb the scrolled-back viewport.
                false
            }
            FollowTailAnchor::AtTail => {
                // Task 3.2: advance by whole lines.
                // follow_tail_offset receives TOTAL CONTENT PIXELS for both old and new.
                let new_offset = follow_tail_offset(
                    self.offset_y,
                    old_total,
                    new_content_height,
                    viewport_height,
                    line_height,
                );
                if (new_offset - self.offset_y).abs() > f32::EPSILON {
                    self.offset_y = new_offset;
                    self.dirty = true;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Notify this tile that leading (head) content has been removed.
    ///
    /// Head-trim operations (e.g. `PortalCadenceCoalescer` byte-cap or
    /// `visible_transcript_window` capping) remove the oldest content from the
    /// **start** of the buffer.  Without this notification, a `ScrolledBack`
    /// viewport holds a raw pixel offset that now points into empty space —
    /// silently jumping the viewport (scenario 3.3 violation).
    ///
    /// This method adjusts `offset_y` downward by `removed_height_px` so that a
    /// `ScrolledBack` viewport stays on the same logical content position after
    /// the trim.  The adjusted offset is clamped to `[0, max_scroll_offset]`.
    ///
    /// When the tile is `AtTail` no adjustment is needed: `follow_tail_offset`
    /// already positions the viewport correctly on the next `notify_content_appended`.
    ///
    /// # Parameters
    ///
    /// - `removed_height_px` — height (physical pixels) of the content dropped
    ///   from the head.  Must be non-negative.  Non-finite values are ignored.
    ///
    /// # Returns
    ///
    /// `true` if `offset_y` changed (i.e. the tile was `ScrolledBack` and the
    /// offset moved — including when it clamps to zero), `false` otherwise.
    pub fn notify_head_content_removed(&mut self, removed_height_px: f32) -> bool {
        if !removed_height_px.is_finite() || removed_height_px <= 0.0 {
            return false;
        }

        // Always update the content-size fields so that the next call to
        // `notify_content_appended` sees a correct `old_total` and so that
        // `clamp_offsets` / `update_follow_tail_anchor_after_user_scroll` use
        // the right max-scroll-offset bound.
        //
        // Invariant maintained:
        //   config.content_height = (total_content_height_px - viewport_height).max(0)
        //
        // We update `total_content_height_px` here.  `config.content_height` is
        // the max-scroll-offset and equals (total − viewport); we reduce it by
        // the same `removed_height_px` (clamped to 0) so the invariant holds.
        self.total_content_height_px = (self.total_content_height_px - removed_height_px).max(0.0);
        if let Some(config) = &mut self.config {
            if let Some(ch) = config.content_height {
                config.content_height = Some((ch - removed_height_px).max(0.0));
            }
        }

        match self.follow_tail {
            FollowTailAnchor::AtTail => {
                // AtTail viewports are re-positioned by the next
                // notify_content_appended call — no offset adjustment needed here.
                false
            }
            FollowTailAnchor::ScrolledBack => {
                let before = self.offset_y;
                // Shift offset down by the removed height, clamp to [0, max].
                self.offset_y = (self.offset_y - removed_height_px).max(0.0);
                // Re-apply the updated upper bound from config (max scroll offset).
                if let Some(config) = &self.config {
                    if let Some(ch) = config.content_height {
                        self.offset_y = self.offset_y.min(ch.max(0.0));
                    }
                }
                if (self.offset_y - before).abs() > f32::EPSILON {
                    self.dirty = true;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Current total content height in physical pixels (all rendered lines,
    /// not clamped to the viewport).  See the field doc for the
    /// total-content-height vs. max-scroll-offset coordinate-system note.
    pub fn total_content_height_px(&self) -> f32 {
        self.total_content_height_px
    }

    /// Force this tile's viewport to the tail, overriding any `ScrolledBack`
    /// anchor.
    ///
    /// Unlike [`Self::notify_content_appended`], which deliberately preserves
    /// a `ScrolledBack` viewport when ordinary (remote/streamed) content grows
    /// (spec task 3.3), this is for a deliberate LOCAL action — e.g. the
    /// viewer typing into or submitting their own composer — where staying
    /// scrolled away from the tail would strand them from their own live
    /// input (hud-qbcp8: input-history "reset to tail on typing/submit").
    ///
    /// Returns `true` if the offset or anchor changed.
    pub fn reset_to_tail(&mut self) -> bool {
        let Some(config) = &self.config else {
            return false;
        };
        let tail = config.content_height.unwrap_or(0.0).max(0.0);
        let changed = self.follow_tail != FollowTailAnchor::AtTail
            || (self.offset_y - tail).abs() > f32::EPSILON;
        self.offset_y = tail;
        self.follow_tail = FollowTailAnchor::AtTail;
        if changed {
            self.dirty = true;
        }
        changed
    }

    /// Queue an agent `SetScrollOffsetRequest`.
    ///
    /// Will be applied only if no user scroll event arrives this frame.
    pub fn queue_agent_request(&mut self, req: SetScrollOffsetRequest) {
        self.pending_agent_request = Some(req);
    }

    /// Commit the frame: apply pending agent request (if no user scroll this frame),
    /// then clear per-frame state. Returns `true` if the offset changed this frame.
    pub fn commit_frame(&mut self) -> bool {
        if !self.user_scroll_this_frame {
            // No user scroll — apply pending agent request if present
            if let Some(req) = self.pending_agent_request.take() {
                let before_x = self.offset_x;
                let before_y = self.offset_y;
                if let Some(config) = &self.config {
                    if config.scrollable_x {
                        self.offset_x = req.offset_x;
                    }
                    if config.scrollable_y {
                        self.offset_y = req.offset_y;
                    }
                    self.clamp_offsets();
                }
                if (self.offset_x - before_x).abs() > f32::EPSILON
                    || (self.offset_y - before_y).abs() > f32::EPSILON
                {
                    self.dirty = true;
                }
            }
        } else {
            // User scroll wins: discard pending agent request (spec: user takes priority)
            self.pending_agent_request = None;
        }

        let changed = self.dirty;
        self.user_scroll_this_frame = false;
        self.dirty = false;
        changed
    }

    /// Clamp offsets to [0, content_size] range.
    ///
    /// `content_width`/`content_height` in `ScrollConfig` represent the maximum
    /// scroll offset (i.e. the content boundary), not viewport-subtracted values.
    /// If viewport-aware clamping is needed in the future, `ScrollConfig` must
    /// carry viewport dimensions and this method updated accordingly.
    fn clamp_offsets(&mut self) {
        if let Some(config) = &self.config {
            self.offset_x = self.offset_x.max(0.0);
            self.offset_y = self.offset_y.max(0.0);
            if let Some(cw) = config.content_width {
                self.offset_x = self.offset_x.min(cw.max(0.0));
            }
            if let Some(ch) = config.content_height {
                self.offset_y = self.offset_y.min(ch.max(0.0));
            }
        }
    }

    /// Returns a `ScrollOffsetChangedEvent` for notifying agents.
    pub fn changed_event(&self, tile_id: SceneId) -> ScrollOffsetChangedEvent {
        ScrollOffsetChangedEvent {
            tile_id,
            offset_x: self.offset_x,
            offset_y: self.offset_y,
        }
    }
}

// ─── ScrollState ──────────────────────────────────────────────────────────────

/// Scroll state registry for all scrollable tiles.
///
/// Owned by the local scroll subsystem (compositor or input kernel). Scroll
/// events are applied here to update per-tile offsets; the caller is responsible
/// for encoding changed offsets as `ScrollOffsetUpdate` entries in the
/// `SceneLocalPatch` and for emitting `ScrollOffsetChangedEvent`s to agents.
#[derive(Default)]
pub struct ScrollState {
    tiles: std::collections::HashMap<SceneId, ScrollTileState>,
    pending_unregistered_requests: std::collections::HashMap<SceneId, SetScrollOffsetRequest>,
}

impl ScrollState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tile as scrollable with the given configuration.
    pub fn register_tile(&mut self, tile_id: SceneId, config: ScrollConfig) {
        let mut state = ScrollTileState::new(config);
        if let Some(req) = self.pending_unregistered_requests.remove(&tile_id) {
            state.queue_agent_request(req);
        }
        self.tiles.insert(tile_id, state);
    }

    /// Unregister a tile (e.g. tile destroyed).
    pub fn unregister_tile(&mut self, tile_id: SceneId) {
        self.tiles.remove(&tile_id);
        self.pending_unregistered_requests.remove(&tile_id);
    }

    /// Returns true if a tile is registered as scrollable.
    pub fn is_scrollable(&self, tile_id: SceneId) -> bool {
        self.tiles.contains_key(&tile_id)
    }

    /// Process a user scroll event for a specific tile.
    ///
    /// Returns the scroll deltas actually applied (respecting `ScrollConfig`
    /// axis locks), or `None` if the tile is not scrollable.
    pub fn apply_user_scroll(
        &mut self,
        tile_id: SceneId,
        delta_x: f32,
        delta_y: f32,
    ) -> Option<(f32, f32)> {
        let state = self.tiles.get_mut(&tile_id)?;
        let before_x = state.offset_x;
        let before_y = state.offset_y;
        state.queue_user_scroll(delta_x, delta_y);
        Some((state.offset_x - before_x, state.offset_y - before_y))
    }

    /// Queue an agent `SetScrollOffsetRequest`.
    ///
    /// The request will be applied at `commit_frame` unless a user scroll
    /// arrives in the same frame.
    pub fn queue_agent_request(&mut self, req: SetScrollOffsetRequest) {
        if let Some(state) = self.tiles.get_mut(&req.tile_id) {
            state.queue_agent_request(req);
        } else {
            self.pending_unregistered_requests.insert(req.tile_id, req);
        }
    }

    /// Commit a single tile frame and report if its offset changed.
    pub fn commit_tile_frame(&mut self, tile_id: SceneId) -> bool {
        self.tiles
            .get_mut(&tile_id)
            .map(ScrollTileState::commit_frame)
            .unwrap_or(false)
    }

    /// Commit all pending frames and return a list of tile IDs whose offsets
    /// actually changed (to be included in `ScrollOffsetChangedEvent`s for agents).
    pub fn commit_all_frames(&mut self) -> Vec<SceneId> {
        self.tiles
            .iter_mut()
            .filter_map(|(tile_id, state)| {
                if state.commit_frame() {
                    Some(*tile_id)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get the current scroll offset for a tile, or `(0.0, 0.0)` if not found.
    pub fn offset(&self, tile_id: SceneId) -> (f32, f32) {
        self.tiles
            .get(&tile_id)
            .map(|s| (s.offset_x, s.offset_y))
            .unwrap_or((0.0, 0.0))
    }

    /// Get a `ScrollOffsetChangedEvent` for a tile (for agent notification).
    pub fn changed_event(&self, tile_id: SceneId) -> Option<ScrollOffsetChangedEvent> {
        self.tiles.get(&tile_id).map(|s| s.changed_event(tile_id))
    }

    /// Notify a tile that content has been appended (e.g. new streaming lines).
    ///
    /// Implements spec task 3.2 / 3.3 at the registry level:
    /// - `AtTail` tiles advance their scroll offset by whole lines.
    /// - `ScrolledBack` tiles have their offset left unchanged.
    ///
    /// Returns `true` if the offset actually changed, `false` otherwise.
    /// No-op if the tile is not registered.
    pub fn notify_content_appended(
        &mut self,
        tile_id: SceneId,
        new_content_height: f32,
        viewport_height: f32,
        line_height: f32,
    ) -> bool {
        self.tiles
            .get_mut(&tile_id)
            .map(|s| s.notify_content_appended(new_content_height, viewport_height, line_height))
            .unwrap_or(false)
    }

    /// Notify a tile that leading (head) content has been removed.
    ///
    /// Delegates to [`ScrollTileState::notify_head_content_removed`].  See that
    /// method for the full contract.
    ///
    /// Returns `true` if the scroll offset changed, `false` otherwise.
    /// No-op if the tile is not registered.
    pub fn notify_head_content_removed(
        &mut self,
        tile_id: SceneId,
        removed_height_px: f32,
    ) -> bool {
        self.tiles
            .get_mut(&tile_id)
            .map(|s| s.notify_head_content_removed(removed_height_px))
            .unwrap_or(false)
    }

    /// Return the current follow-tail anchor state for a tile.
    ///
    /// Returns `AtTail` (the default) if the tile is not registered.
    pub fn follow_tail_anchor(&self, tile_id: SceneId) -> FollowTailAnchor {
        self.tiles
            .get(&tile_id)
            .map(|s| s.follow_tail)
            .unwrap_or(FollowTailAnchor::AtTail)
    }

    /// Return a tile's current total content height in physical pixels, or
    /// `0.0` if the tile is not registered.
    ///
    /// Callers that maintain their own running content-height total (rather
    /// than re-measuring it) can read this back before adding a delta and
    /// re-calling [`Self::notify_content_appended`], instead of tracking a
    /// duplicate counter (hud-qbcp8).
    pub fn total_content_height_px(&self, tile_id: SceneId) -> f32 {
        self.tiles
            .get(&tile_id)
            .map(ScrollTileState::total_content_height_px)
            .unwrap_or(0.0)
    }

    /// Force a tile's viewport to the tail, overriding any `ScrolledBack`
    /// anchor.  Delegates to [`ScrollTileState::reset_to_tail`]; see that
    /// method for the full contract.  No-op (`false`) if the tile is not
    /// registered.
    pub fn reset_to_tail(&mut self, tile_id: SceneId) -> bool {
        self.tiles
            .get_mut(&tile_id)
            .map(ScrollTileState::reset_to_tail)
            .unwrap_or(false)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scroll_vertical_updates_offset_y() {
        let mut state = ScrollTileState::new(ScrollConfig::vertical());
        state.queue_user_scroll(0.0, 30.0);
        assert!((state.offset_y - 30.0).abs() < f32::EPSILON);
        assert!((state.offset_x).abs() < f32::EPSILON); // x unchanged
    }

    #[test]
    fn test_scroll_horizontal_updates_offset_x() {
        let mut state = ScrollTileState::new(ScrollConfig::horizontal());
        state.queue_user_scroll(15.0, 0.0);
        assert!((state.offset_x - 15.0).abs() < f32::EPSILON);
        assert!((state.offset_y).abs() < f32::EPSILON);
    }

    #[test]
    fn test_scroll_axis_lock_ignores_locked_axis() {
        let mut state = ScrollTileState::new(ScrollConfig::vertical());
        state.queue_user_scroll(50.0, 20.0); // x should be ignored
        assert!((state.offset_x).abs() < f32::EPSILON);
        assert!((state.offset_y - 20.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_user_scroll_takes_priority_over_agent_request() {
        let tile_id = SceneId::new();
        let mut state = ScrollTileState::new(ScrollConfig::vertical());

        // Queue agent request first
        state.queue_agent_request(SetScrollOffsetRequest {
            tile_id,
            offset_x: 0.0,
            offset_y: 999.0,
        });

        // Then a user scroll arrives in the same frame
        state.queue_user_scroll(0.0, 20.0);

        state.commit_frame();

        // User scroll (20.0) wins; agent request (999.0) discarded
        assert!(
            (state.offset_y - 20.0).abs() < f32::EPSILON,
            "expected 20.0 got {}",
            state.offset_y
        );
    }

    #[test]
    fn test_agent_request_applied_when_no_user_scroll() {
        let tile_id = SceneId::new();
        let mut state = ScrollTileState::new(ScrollConfig::vertical());

        state.queue_agent_request(SetScrollOffsetRequest {
            tile_id,
            offset_x: 0.0,
            offset_y: 300.0,
        });

        state.commit_frame();

        // No user scroll — agent request applied
        assert!((state.offset_y - 300.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_content_height_clamp() {
        let config = ScrollConfig {
            scrollable_y: true,
            scrollable_x: false,
            content_width: None,
            content_height: Some(200.0),
        };
        let mut state = ScrollTileState::new(config);
        state.queue_user_scroll(0.0, 999.0);
        assert!(
            state.offset_y <= 200.0,
            "offset_y {} should be clamped to 200",
            state.offset_y
        );
    }

    #[test]
    fn test_scroll_offset_no_negative() {
        let mut state = ScrollTileState::new(ScrollConfig::vertical());
        state.queue_user_scroll(0.0, -50.0); // negative delta → clamp to 0
        assert!(state.offset_y >= 0.0);
    }

    #[test]
    fn test_scroll_state_register_and_apply() {
        let tile_id = SceneId::new();
        let mut scroll = ScrollState::new();
        scroll.register_tile(tile_id, ScrollConfig::vertical());

        assert!(scroll.is_scrollable(tile_id));

        let delta = scroll.apply_user_scroll(tile_id, 0.0, 50.0);
        assert!(delta.is_some());
        let (dx, dy) = delta.unwrap();
        assert!((dx).abs() < f32::EPSILON);
        assert!((dy - 50.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_scroll_state_non_scrollable_tile_returns_none() {
        let tile_id = SceneId::new();
        let mut scroll = ScrollState::new();
        // Not registered
        let delta = scroll.apply_user_scroll(tile_id, 0.0, 50.0);
        assert!(delta.is_none());
    }

    #[test]
    fn test_scroll_state_commit_reports_changed_tiles() {
        let tile_a = SceneId::new();
        let tile_b = SceneId::new();
        let mut scroll = ScrollState::new();
        scroll.register_tile(tile_a, ScrollConfig::vertical());
        scroll.register_tile(tile_b, ScrollConfig::horizontal());

        // Only scroll tile_a
        scroll.apply_user_scroll(tile_a, 0.0, 10.0);

        let changed = scroll.commit_all_frames();
        assert!(changed.contains(&tile_a), "tile_a should be changed");
        // tile_b did not scroll, so it should not be in changed (unless it had a pending agent req)
        // (tile_b had no scroll, so unchanged)
        assert!(!changed.contains(&tile_b), "tile_b was not scrolled");
    }

    #[test]
    fn test_scroll_state_user_wins_over_queued_agent_request() {
        let tile_id = SceneId::new();
        let mut scroll = ScrollState::new();
        scroll.register_tile(tile_id, ScrollConfig::vertical());

        scroll.queue_agent_request(SetScrollOffsetRequest {
            tile_id,
            offset_x: 0.0,
            offset_y: 999.0,
        });
        scroll.apply_user_scroll(tile_id, 0.0, 25.0);
        scroll.commit_all_frames();

        let (_, offset_y) = scroll.offset(tile_id);
        assert!(
            (offset_y - 25.0).abs() < f32::EPSILON,
            "user scroll should win, got {offset_y}"
        );
    }

    #[test]
    fn test_queue_request_before_registration_is_applied_after_register() {
        let tile_id = SceneId::new();
        let mut scroll = ScrollState::new();

        scroll.queue_agent_request(SetScrollOffsetRequest {
            tile_id,
            offset_x: 0.0,
            offset_y: 120.0,
        });

        scroll.register_tile(tile_id, ScrollConfig::vertical());
        let changed = scroll.commit_all_frames();
        assert!(
            changed.contains(&tile_id),
            "tile with queued request should be reported as changed"
        );
        let (_, offset_y) = scroll.offset(tile_id);
        assert!(
            (offset_y - 120.0).abs() < f32::EPSILON,
            "queued request should apply after registration, got {offset_y}"
        );
    }

    #[test]
    fn test_commit_tile_frame_does_not_consume_other_tile_updates() {
        let tile_a = SceneId::new();
        let tile_b = SceneId::new();
        let mut scroll = ScrollState::new();
        scroll.register_tile(tile_a, ScrollConfig::vertical());
        scroll.register_tile(tile_b, ScrollConfig::vertical());
        scroll.queue_agent_request(SetScrollOffsetRequest {
            tile_id: tile_b,
            offset_x: 0.0,
            offset_y: 55.0,
        });

        assert!(!scroll.commit_tile_frame(tile_a));

        let changed = scroll.commit_all_frames();
        assert!(
            changed.contains(&tile_b),
            "tile_b update must remain pending until its frame is committed"
        );
        let (_, offset_y) = scroll.offset(tile_b);
        assert!((offset_y - 55.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_scroll_changed_event_contains_current_offset() {
        let tile_id = SceneId::new();
        let mut scroll = ScrollState::new();
        scroll.register_tile(tile_id, ScrollConfig::vertical());
        scroll.apply_user_scroll(tile_id, 0.0, 42.0);

        let event = scroll.changed_event(tile_id);
        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.tile_id, tile_id);
        assert!((event.offset_y - 42.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_input_to_local_ack_p99_within_budget_scroll() {
        use std::time::Instant;
        use tze_hud_scene::perf_budget::{budgets, test_budget};

        let tile_id = SceneId::new();
        let mut scroll = ScrollState::new();
        scroll.register_tile(tile_id, ScrollConfig::vertical());

        let start = Instant::now();
        scroll.apply_user_scroll(tile_id, 0.0, 30.0);
        let elapsed_us = start.elapsed().as_micros() as u64;

        let budget = test_budget(budgets::INPUT_ACK_BUDGET_US);
        assert!(
            elapsed_us < budget,
            "scroll local_ack_us={elapsed_us}us exceeded calibrated budget {budget}us",
        );
    }

    // ── Spec task 3.2 — follow-tail advances by whole lines ──────────────────

    /// A tile starting at the tail should advance its offset by exactly N whole
    /// lines when content is appended, never by a fractional line.
    ///
    /// Spec task 3.2: "follow-tail advances by whole lines"
    #[test]
    fn follow_tail_advances_by_whole_lines_on_append() {
        let tile_id = SceneId::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32; // 5 visible lines
        let mut scroll = ScrollState::new();

        // Register with content_height = 5 lines initially (viewport is full).
        let config = ScrollConfig {
            scrollable_y: true,
            scrollable_x: false,
            content_width: None,
            content_height: Some(5.0 * line_h), // 100px
        };
        scroll.register_tile(tile_id, config);

        // Tile starts at AtTail with offset_y = 0 (content fits in viewport).
        assert_eq!(scroll.follow_tail_anchor(tile_id), FollowTailAnchor::AtTail);

        // Append 3 more lines: content grows from 100px to 160px.
        let new_content_height = 8.0 * line_h; // 160px
        let changed =
            scroll.notify_content_appended(tile_id, new_content_height, viewport_h, line_h);

        assert!(
            changed,
            "offset should have changed when at tail and content grew"
        );

        let (_, offset_y) = scroll.offset(tile_id);
        // Expected new offset: 160 - 100 = 60px (exactly 3 line-heights).
        // The viewport shows lines 3-7 (0-indexed), bottom-aligned to content end.
        assert!(
            (offset_y - 60.0).abs() < f32::EPSILON,
            "follow-tail should advance to offset 60.0 (3 new lines × 20px); got {offset_y}"
        );

        // Advancement is always a whole multiple of line_height.
        assert_eq!(
            (offset_y / line_h).fract(),
            0.0,
            "offset_y must be a whole multiple of line_height; got {offset_y}"
        );

        // Tile remains at tail anchor after content append.
        assert_eq!(
            scroll.follow_tail_anchor(tile_id),
            FollowTailAnchor::AtTail,
            "anchor must remain AtTail after follow-tail advancement"
        );
    }

    /// Single-line append from a follow-tail position advances by exactly one
    /// whole line.
    #[test]
    fn follow_tail_single_line_append_advances_exactly_one_line() {
        let tile_id = SceneId::new();
        let line_h = 22.4_f32;
        let viewport_h = 5.0 * line_h;
        let mut scroll = ScrollState::new();

        // Start: content exactly fills 5 lines; offset = 0 (no scrollable range).
        let config = ScrollConfig {
            scrollable_y: true,
            scrollable_x: false,
            content_width: None,
            content_height: Some(5.0 * line_h),
        };
        scroll.register_tile(tile_id, config);

        // Append exactly 1 line.
        let new_content = 6.0 * line_h;
        scroll.notify_content_appended(tile_id, new_content, viewport_h, line_h);

        let (_, offset_y) = scroll.offset(tile_id);
        // offset_y should be 1 × line_h to keep the 6th line visible.
        let expected = line_h; // one line height = 22.4px
        assert!(
            (offset_y - expected).abs() < 0.01,
            "single-line append must advance by exactly 1 × line_height ({expected:.2}px); \
             got {offset_y:.2}"
        );
    }

    // ── Spec task 3.3 — append does not disturb a scrolled-back viewport ─────

    /// When the user has scrolled back from the tail, appending new content
    /// must NOT change the scroll offset.
    ///
    /// Spec task 3.3: "append stability for scrolled-back viewports"
    #[test]
    fn scrolled_back_append_does_not_disturb_viewport() {
        let tile_id = SceneId::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32; // 5 visible lines
        let mut scroll = ScrollState::new();

        // Content: 20 lines = 400px.
        let config = ScrollConfig {
            scrollable_y: true,
            scrollable_x: false,
            content_width: None,
            content_height: Some(20.0 * line_h),
        };
        scroll.register_tile(tile_id, config);

        // Move viewport to the tail first (scroll down to end).
        scroll.apply_user_scroll(tile_id, 0.0, 300.0); // scroll to offset 300 (lines 15–20)
        assert_eq!(
            scroll.follow_tail_anchor(tile_id),
            FollowTailAnchor::AtTail,
            "after scrolling to tail offset should be AtTail"
        );

        // Now scroll back up.
        scroll.apply_user_scroll(tile_id, 0.0, -120.0); // back up 6 lines to offset 180
        assert_eq!(
            scroll.follow_tail_anchor(tile_id),
            FollowTailAnchor::ScrolledBack,
            "after scrolling up the anchor must be ScrolledBack"
        );
        let (_, offset_before) = scroll.offset(tile_id);

        // Append 5 more lines.
        let new_content = 25.0 * line_h;
        let changed = scroll.notify_content_appended(tile_id, new_content, viewport_h, line_h);

        assert!(
            !changed,
            "append must not dirty the offset when ScrolledBack"
        );

        let (_, offset_after) = scroll.offset(tile_id);
        assert!(
            (offset_before - offset_after).abs() < f32::EPSILON,
            "scrolled-back append must not change offset_y; before={offset_before} after={offset_after}"
        );

        // Anchor remains ScrolledBack.
        assert_eq!(
            scroll.follow_tail_anchor(tile_id),
            FollowTailAnchor::ScrolledBack,
            "anchor must remain ScrolledBack after content append"
        );
    }

    /// After scrolling back and then scrolling back to the tail, the anchor
    /// transitions back to AtTail and follow-tail behaviour resumes.
    #[test]
    fn scrolled_back_then_scroll_to_tail_resumes_follow_tail() {
        let tile_id = SceneId::new();
        let line_h = 20.0_f32;
        let content_h = 20.0 * line_h; // 400px
        let mut scroll = ScrollState::new();

        let config = ScrollConfig {
            scrollable_y: true,
            scrollable_x: false,
            content_width: None,
            content_height: Some(content_h),
        };
        scroll.register_tile(tile_id, config);

        // Scroll to tail.
        scroll.apply_user_scroll(tile_id, 0.0, content_h);
        assert_eq!(scroll.follow_tail_anchor(tile_id), FollowTailAnchor::AtTail);

        // Scroll back.
        scroll.apply_user_scroll(tile_id, 0.0, -60.0);
        assert_eq!(
            scroll.follow_tail_anchor(tile_id),
            FollowTailAnchor::ScrolledBack
        );

        // Scroll back to tail.
        scroll.apply_user_scroll(tile_id, 0.0, 300.0); // forward past the tail
        assert_eq!(
            scroll.follow_tail_anchor(tile_id),
            FollowTailAnchor::AtTail,
            "scrolling back to tail must restore AtTail anchor"
        );
    }

    // ── Spec task 3.3 — head-trim does not disturb a scrolled-back viewport ─────
    //
    // Defect #4 regression test: when head content is removed (byte-cap / window
    // trim), a ScrolledBack viewport must shift downward by the removed height so
    // it continues showing the same logical content.  Without the fix the raw pixel
    // offset stays unchanged, pointing into a gap left by the removed head content.

    /// Removing head content while ScrolledBack must shift the scroll offset
    /// downward by the removed height (keeping the viewport on the same logical
    /// content position).
    ///
    /// Scenario 3.3: "append does not disturb a scrolled-back viewport" — the
    /// head-trim variant.
    #[test]
    fn head_trim_scrolled_back_adjusts_offset_by_removed_height() {
        let tile_id = SceneId::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32; // 5 visible lines
        let mut scroll = ScrollState::new();

        // Content: 20 lines = 400px total; max scroll offset = 300px.
        let total_content = 20.0 * line_h; // 400px
        let max_scroll = total_content - viewport_h; // 300px
        let config = ScrollConfig {
            scrollable_y: true,
            scrollable_x: false,
            content_width: None,
            content_height: Some(max_scroll),
        };
        scroll.register_tile(tile_id, config);

        // Scroll to tail, then scroll back to line 10 (offset = 10 * line_h = 200px).
        scroll.apply_user_scroll(tile_id, 0.0, max_scroll); // → AtTail at 300px
        scroll.apply_user_scroll(tile_id, 0.0, -100.0); // → ScrolledBack at 200px
        assert_eq!(
            scroll.follow_tail_anchor(tile_id),
            FollowTailAnchor::ScrolledBack,
            "must be ScrolledBack after scrolling up"
        );
        let (_, offset_before) = scroll.offset(tile_id);
        assert!(
            (offset_before - 200.0).abs() < f32::EPSILON,
            "expected offset_y 200.0 before head-trim; got {offset_before}"
        );

        // Head-trim: remove 5 lines (100px) from the start.
        let removed_height = 5.0 * line_h; // 100px
        let changed = scroll.notify_head_content_removed(tile_id, removed_height);

        assert!(
            changed,
            "head-trim with non-zero offset must report changed=true"
        );

        // After trim, offset must decrease by the removed height.
        let (_, offset_after) = scroll.offset(tile_id);
        let expected_offset = offset_before - removed_height; // 200 - 100 = 100px
        assert!(
            (offset_after - expected_offset).abs() < f32::EPSILON,
            "head-trim must shift offset by removed_height ({removed_height}px); \
             before={offset_before} expected={expected_offset} got={offset_after}"
        );

        // Anchor stays ScrolledBack.
        assert_eq!(
            scroll.follow_tail_anchor(tile_id),
            FollowTailAnchor::ScrolledBack,
            "head-trim must not change the follow-tail anchor"
        );
    }

    /// Head-trim of an AtTail tile must NOT change the scroll offset.
    ///
    /// For AtTail tiles, `notify_content_appended` repositions the offset; a
    /// separate head-trim adjustment would double-count the removal.
    #[test]
    fn head_trim_at_tail_does_not_change_offset() {
        let tile_id = SceneId::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32;
        let mut scroll = ScrollState::new();

        let total_content = 20.0 * line_h;
        let max_scroll = total_content - viewport_h;
        let config = ScrollConfig {
            scrollable_y: true,
            scrollable_x: false,
            content_width: None,
            content_height: Some(max_scroll),
        };
        scroll.register_tile(tile_id, config);

        // Start at tail (AtTail is the default).
        scroll.apply_user_scroll(tile_id, 0.0, max_scroll);
        assert_eq!(scroll.follow_tail_anchor(tile_id), FollowTailAnchor::AtTail);
        let (_, offset_before) = scroll.offset(tile_id);

        // Head-trim: remove 3 lines (60px).
        let changed = scroll.notify_head_content_removed(tile_id, 3.0 * line_h);

        assert!(
            !changed,
            "head-trim on an AtTail tile must return changed=false"
        );
        let (_, offset_after) = scroll.offset(tile_id);
        assert!(
            (offset_after - offset_before).abs() < f32::EPSILON,
            "AtTail offset must be unchanged after head-trim; \
             before={offset_before} after={offset_after}"
        );
    }

    /// When offset_y is smaller than removed_height, the offset is clamped to 0
    /// rather than going negative.
    #[test]
    fn head_trim_clamps_offset_to_zero_when_removal_exceeds_offset() {
        let tile_id = SceneId::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32;
        let mut scroll = ScrollState::new();

        let total_content = 20.0 * line_h;
        let max_scroll = total_content - viewport_h;
        let config = ScrollConfig {
            scrollable_y: true,
            scrollable_x: false,
            content_width: None,
            content_height: Some(max_scroll),
        };
        scroll.register_tile(tile_id, config);

        // Scroll back just 1 line.
        scroll.apply_user_scroll(tile_id, 0.0, max_scroll);
        scroll.apply_user_scroll(tile_id, 0.0, -line_h); // offset = max_scroll - line_h
        let (_, offset_before) = scroll.offset(tile_id);
        assert!(offset_before > 0.0);

        // Head-trim removes MORE than the current offset.
        let removed_height = offset_before + 50.0;
        let changed = scroll.notify_head_content_removed(tile_id, removed_height);

        assert!(changed, "offset changed from non-zero to 0");
        let (_, offset_after) = scroll.offset(tile_id);
        assert_eq!(
            offset_after, 0.0,
            "offset must clamp to 0 when removal exceeds current offset; got {offset_after}"
        );
    }

    /// Head-trim must update `total_content_height_px` and `config.content_height`
    /// so that a subsequent `notify_content_appended` receives correct old/new
    /// totals and the follow-tail offset advances by the right number of lines.
    ///
    /// Regression guard for the Gemini-raised defect: without updating both
    /// fields, the next `notify_content_appended` uses a stale `old_total`,
    /// making `delta_px` negative and preventing follow-tail advancement.
    #[test]
    fn head_trim_updates_content_height_fields_for_correct_append_delta() {
        let tile_id = SceneId::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32; // 5 visible lines
        let mut scroll = ScrollState::new();

        // Content: 10 lines = 200px total; max scroll offset = 100px.
        let total_content = 10.0 * line_h; // 200px
        let max_scroll = total_content - viewport_h; // 100px
        let config = ScrollConfig {
            scrollable_y: true,
            scrollable_x: false,
            content_width: None,
            content_height: Some(max_scroll),
        };
        scroll.register_tile(tile_id, config);

        // Drive total_content_height_px to the known 200px value.
        scroll.notify_content_appended(tile_id, total_content, viewport_h, line_h);
        // Scroll to tail.
        scroll.apply_user_scroll(tile_id, 0.0, max_scroll);
        assert_eq!(scroll.follow_tail_anchor(tile_id), FollowTailAnchor::AtTail);

        // Head-trim: remove 3 lines (60px).
        let removed = 3.0 * line_h; // 60px
        scroll.notify_head_content_removed(tile_id, removed);
        // total_content_height_px should now be 200 - 60 = 140px.
        // config.content_height should now be 100 - 60 = 40px.

        // Append 2 new lines (40px): new total = 140 + 40 = 180px.
        // new max_scroll = 180 - 100 = 80px.
        // Expected delta = 180 - 140 = 40px = 2 lines → follow-tail advances.
        let new_total = 180.0_f32;
        let changed = scroll.notify_content_appended(tile_id, new_total, viewport_h, line_h);
        assert!(
            changed,
            "notify_content_appended must advance AtTail offset after head-trim+append; \
             changed=false suggests stale old_total causing negative delta"
        );
        let (_, offset_after) = scroll.offset(tile_id);
        // New max_scroll = 180 - 100 = 80px.  Offset should advance to 80px (tail).
        let expected_max_scroll = (new_total - viewport_h).max(0.0);
        assert!(
            (offset_after - expected_max_scroll).abs() < line_h,
            "offset should be near the new tail ({expected_max_scroll}px) after \
             head-trim+append; got {offset_after}"
        );
    }

    // ── follow_tail_offset unit tests ─────────────────────────────────────────

    #[test]
    fn follow_tail_offset_zero_delta_returns_unchanged() {
        // No new content: no advancement.
        let result = follow_tail_offset(50.0, 200.0, 200.0, 100.0, 20.0);
        assert!((result - 50.0).abs() < f32::EPSILON);
    }

    #[test]
    fn follow_tail_offset_one_line_advance() {
        // 1 new line (20px) added; advance by exactly 20px.
        let result = follow_tail_offset(0.0, 100.0, 120.0, 100.0, 20.0);
        assert!(
            (result - 20.0).abs() < f32::EPSILON,
            "expected 20.0 got {result}"
        );
    }

    #[test]
    fn follow_tail_offset_clamped_to_tail() {
        // Many lines added but we clamp to (new_content - viewport).
        let result = follow_tail_offset(0.0, 100.0, 500.0, 100.0, 20.0);
        // tail = 500 - 100 = 400; advanced = 0 + (500-100)/20*20 = 400; min(400, 400) = 400.
        assert!(
            (result - 400.0).abs() < f32::EPSILON,
            "expected 400.0 got {result}"
        );
    }

    #[test]
    fn follow_tail_offset_fractional_line_below_threshold_unchanged() {
        // 9px added; line_height = 20px; 9 < 10 (0.5 * 20) => no advancement.
        let result = follow_tail_offset(0.0, 100.0, 109.0, 100.0, 20.0);
        assert!(
            (result - 0.0).abs() < f32::EPSILON,
            "expected 0.0 got {result}"
        );
    }

    #[test]
    fn follow_tail_offset_zero_line_height_returns_unchanged() {
        let result = follow_tail_offset(50.0, 100.0, 200.0, 100.0, 0.0);
        assert!((result - 50.0).abs() < f32::EPSILON);
    }

    // ── reset_to_tail (hud-qbcp8) ──────────────────────────────────────────

    /// `reset_to_tail` on a `ScrolledBack` tile must force the offset to the
    /// tail (the tracked max-scroll-offset) and flip the anchor back to
    /// `AtTail`, reporting `changed = true`.
    #[test]
    fn reset_to_tail_forces_scrolled_back_tile_to_tail() {
        let tile_id = SceneId::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32;
        let mut scroll = ScrollState::new();

        let config = ScrollConfig {
            scrollable_y: true,
            scrollable_x: false,
            content_width: None,
            content_height: None,
        };
        scroll.register_tile(tile_id, config);
        // 20 lines of content (400px) — well past the 100px viewport.
        scroll.notify_content_appended(tile_id, 20.0 * line_h, viewport_h, line_h);
        assert_eq!(scroll.follow_tail_anchor(tile_id), FollowTailAnchor::AtTail);
        let (_, tail_offset) = scroll.offset(tile_id);
        assert!(tail_offset > 0.0, "tail offset should be nonzero here");

        // Scroll away from the tail.
        scroll.apply_user_scroll(tile_id, 0.0, -150.0);
        assert_eq!(
            scroll.follow_tail_anchor(tile_id),
            FollowTailAnchor::ScrolledBack
        );
        let (_, scrolled_offset) = scroll.offset(tile_id);
        assert!(scrolled_offset < tail_offset);

        let changed = scroll.reset_to_tail(tile_id);
        assert!(
            changed,
            "resetting a scrolled-back tile must report changed"
        );

        let (_, offset_after) = scroll.offset(tile_id);
        assert!(
            (offset_after - tail_offset).abs() < f32::EPSILON,
            "reset_to_tail must snap back to the tracked tail ({tail_offset}); got {offset_after}"
        );
        assert_eq!(
            scroll.follow_tail_anchor(tile_id),
            FollowTailAnchor::AtTail,
            "reset_to_tail must restore the AtTail anchor"
        );
    }

    /// Calling `reset_to_tail` on a tile already at the tail is a no-op
    /// (`changed = false`) — it must not spuriously mark the tile dirty.
    #[test]
    fn reset_to_tail_is_noop_when_already_at_tail() {
        let tile_id = SceneId::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32;
        let mut scroll = ScrollState::new();

        scroll.register_tile(tile_id, ScrollConfig::vertical());
        scroll.notify_content_appended(tile_id, 20.0 * line_h, viewport_h, line_h);
        assert_eq!(scroll.follow_tail_anchor(tile_id), FollowTailAnchor::AtTail);

        let changed = scroll.reset_to_tail(tile_id);
        assert!(
            !changed,
            "reset_to_tail on an already-at-tail tile must report no change"
        );
    }

    /// `reset_to_tail` on an unregistered tile is a no-op.
    #[test]
    fn reset_to_tail_unregistered_tile_is_noop() {
        let tile_id = SceneId::new();
        let mut scroll = ScrollState::new();
        assert!(!scroll.reset_to_tail(tile_id));
    }

    /// `total_content_height_px` reflects the running total tracked by
    /// `notify_content_appended`, and is `0.0` before any content has been
    /// recorded or for an unregistered tile.
    #[test]
    fn total_content_height_px_tracks_appended_content() {
        let tile_id = SceneId::new();
        let unregistered = SceneId::new();
        let line_h = 20.0_f32;
        let viewport_h = 100.0_f32;
        let mut scroll = ScrollState::new();

        assert_eq!(scroll.total_content_height_px(unregistered), 0.0);

        scroll.register_tile(tile_id, ScrollConfig::vertical());
        assert_eq!(scroll.total_content_height_px(tile_id), 0.0);

        scroll.notify_content_appended(tile_id, 5.0 * line_h, viewport_h, line_h);
        assert!((scroll.total_content_height_px(tile_id) - 5.0 * line_h).abs() < f32::EPSILON);

        scroll.notify_content_appended(tile_id, 9.0 * line_h, viewport_h, line_h);
        assert!((scroll.total_content_height_px(tile_id) - 9.0 * line_h).abs() < f32::EPSILON);
    }
}
