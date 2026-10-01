//! Textured draw commands, drag handle entries, UV fit-mode calculations,
//! and animation-state types used by the compositor render passes.
//!
//! Moved from `renderer.rs` banner 5 (`// ─── Image fit mode UV calculations ───`)
//! and the animation-state block that immediately follows it, by Step R-1 of
//! the renderer module split (hud-fgryk).  No logic was changed; only visibility
//! modifiers were added where Rust's module-privacy rules require them.
//!
//! Animation-state types (`ZoneAnimationState`, `PublicationAnimationState`,
//! `StreamRevealState`) are placed here rather than a separate `anim_types.rs`
//! because they follow immediately in the original file with no banner seam
//! between them, and the whole block (banner 5 + anim types) has no
//! `impl Compositor` dependencies — keeping the one-banner-per-PR invariant
//! while avoiding a file that would be <50 lines.

use tze_hud_scene::types::*;

// ─── Image fit mode UV calculations ─────────────────────────────────────────

/// A textured draw command collected during scene traversal.
///
/// These are collected separately from color quads because they use a
/// different vertex layout and render pipeline.
pub(super) struct TexturedDrawCmd {
    pub(super) resource_id: ResourceId,
    /// Pixel-space position and size of the destination rectangle.
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) w: f32,
    pub(super) h: f32,
    /// UV sub-rectangle within the texture: `[u_min, v_min, u_max, v_max]`.
    pub(super) uv_rect: [f32; 4],
    /// Per-vertex tint (opacity, fade, etc.).
    pub(super) tint: [f32; 4],
}

/// Collected runtime drag handle geometry + style for one visible element.
#[derive(Clone, Debug)]
pub(super) struct DragHandleEntry {
    pub(super) element_id: SceneId,
    pub(super) element_kind: DragHandleElementKind,
    /// Bounds of the drag handle affordance itself.
    pub(super) bounds: Rect,
    /// Full display-space bounds of the element being dragged.
    ///
    /// Used to emit the 2px highlight border during active drag.
    pub(super) element_bounds: Rect,
    pub(super) interaction_id: String,
    pub(super) style: DragHandleStyle,
    /// When `true`, this entry is a portal header-band handle (full-width top
    /// strip of a portal frame) rather than the legacy small centered grip. Band
    /// handles yield to interactive controls under the point and render no visual
    /// of their own (the client draws the header) — hud-643dv.
    pub(super) is_header_band: bool,
}

/// Compute the UV rectangle and destination rectangle for a given fit mode.
///
/// Returns `(dest_x, dest_y, dest_w, dest_h, uv_rect)` where:
/// - `(dest_x, dest_y, dest_w, dest_h)` is the pixel-space quad to render
/// - `uv_rect` is `[u_min, v_min, u_max, v_max]` within the texture
///
/// All fit modes assume the texture contains the full image at `(img_w, img_h)`.
pub(super) fn compute_fit_mode(
    fit_mode: ImageFitMode,
    // Destination bounds in pixel space
    dest_x: f32,
    dest_y: f32,
    dest_w: f32,
    dest_h: f32,
    // Source image dimensions
    img_w: u32,
    img_h: u32,
) -> (f32, f32, f32, f32, [f32; 4]) {
    let iw = img_w as f32;
    let ih = img_h as f32;

    if iw <= 0.0 || ih <= 0.0 || dest_w <= 0.0 || dest_h <= 0.0 {
        return (dest_x, dest_y, dest_w, dest_h, [0.0, 0.0, 1.0, 1.0]);
    }

    let src_aspect = iw / ih;
    let dst_aspect = dest_w / dest_h;

    match fit_mode {
        ImageFitMode::Fill => {
            // Stretch to fill — full UV, full dest
            (dest_x, dest_y, dest_w, dest_h, [0.0, 0.0, 1.0, 1.0])
        }
        ImageFitMode::Contain => {
            // Scale uniformly so the entire image is visible; letterbox bars.
            let (rw, rh) = if src_aspect > dst_aspect {
                // Image is wider: fit width, letterbox top/bottom
                let rw = dest_w;
                let rh = dest_w / src_aspect;
                (rw, rh)
            } else {
                // Image is taller: fit height, letterbox left/right
                let rh = dest_h;
                let rw = dest_h * src_aspect;
                (rw, rh)
            };
            let rx = dest_x + (dest_w - rw) * 0.5;
            let ry = dest_y + (dest_h - rh) * 0.5;
            (rx, ry, rw, rh, [0.0, 0.0, 1.0, 1.0])
        }
        ImageFitMode::Cover => {
            // Scale uniformly to cover the entire dest; crop the excess via UV.
            let (u_min, v_min, u_max, v_max) = if src_aspect > dst_aspect {
                // Image is wider than dest: crop horizontal
                let visible_fraction = dst_aspect / src_aspect;
                let u_offset = (1.0 - visible_fraction) * 0.5;
                (u_offset, 0.0, u_offset + visible_fraction, 1.0)
            } else {
                // Image is taller than dest: crop vertical
                let visible_fraction = src_aspect / dst_aspect;
                let v_offset = (1.0 - visible_fraction) * 0.5;
                (0.0, v_offset, 1.0, v_offset + visible_fraction)
            };
            (dest_x, dest_y, dest_w, dest_h, [u_min, v_min, u_max, v_max])
        }
        ImageFitMode::ScaleDown => {
            // Like Contain but never scale up.
            if iw <= dest_w && ih <= dest_h {
                // Image fits at native size — center it.
                let rx = dest_x + (dest_w - iw) * 0.5;
                let ry = dest_y + (dest_h - ih) * 0.5;
                (rx, ry, iw, ih, [0.0, 0.0, 1.0, 1.0])
            } else {
                // Image is larger — use Contain logic.
                let (rw, rh) = if src_aspect > dst_aspect {
                    (dest_w, dest_w / src_aspect)
                } else {
                    (dest_h * src_aspect, dest_h)
                };
                let rx = dest_x + (dest_w - rw) * 0.5;
                let ry = dest_y + (dest_h - rh) * 0.5;
                (rx, ry, rw, rh, [0.0, 0.0, 1.0, 1.0])
            }
        }
    }
}

// ─── Animation-state types ───────────────────────────────────────────────────
//
// These types follow the image-fit-mode block in the original renderer.rs with
// no banner seam between them. They are included here (not in a separate
// anim_types.rs) because: (a) the whole block has no impl Compositor deps,
// (b) a dedicated file would be <60 lines, and (c) they will move together to
// animation.rs in step R-5 when the impl Compositor animation methods move.

/// Per-zone opacity animation state.
///
/// Tracks a fade-in or fade-out transition for a single zone.
/// When no transition is active, the zone is at full opacity (1.0).
///
/// Modeled after `WidgetAnimationState` in `crate::widget`.
pub struct ZoneAnimationState {
    /// Wall-clock time when the transition started.
    pub transition_start: std::time::Instant,
    /// Duration of the transition in milliseconds.
    pub duration_ms: u32,
    /// Opacity at the start of the transition.
    pub from_opacity: f32,
    /// Target opacity at the end of the transition (0.0 = fade-out, 1.0 = fade-in).
    pub target_opacity: f32,
}

impl ZoneAnimationState {
    /// Create a fade-in state (opacity 0 → 1) with the given duration.
    pub fn fade_in(duration_ms: u32) -> Self {
        Self {
            transition_start: std::time::Instant::now(),
            duration_ms,
            from_opacity: 0.0,
            target_opacity: 1.0,
        }
    }

    /// Create a fade-in state starting from `from_opacity` rather than 0.
    ///
    /// Used for **transition interrupt semantics**: when a new publish arrives
    /// during an active fade-out, the fade-out is cancelled and a fade-in begins
    /// from the current composite opacity (not from zero).  This prevents blank
    /// frames during rapid replacement.
    ///
    /// Per spec §Subtitle Contention Policy — Latest Wins, Transition interrupt
    /// semantics note: "the fade-out MUST be cancelled immediately and the new
    /// content MUST begin its transition_in_ms fade-in from the current composite
    /// opacity (not from zero)."
    pub fn fade_in_from(duration_ms: u32, from_opacity: f32) -> Self {
        Self {
            transition_start: std::time::Instant::now(),
            duration_ms,
            from_opacity: from_opacity.clamp(0.0, 1.0),
            target_opacity: 1.0,
        }
    }

    /// Create a fade-out state (opacity 1 → 0) with the given duration.
    pub fn fade_out(duration_ms: u32) -> Self {
        Self {
            transition_start: std::time::Instant::now(),
            duration_ms,
            from_opacity: 1.0,
            target_opacity: 0.0,
        }
    }

    /// Raw linear progress `∈ [0, 1]` derived from elapsed wall time.
    ///
    /// A `duration_ms` of `0` reports `1.0` (already complete). Split out so the
    /// time source and the interpolation math are independently testable.
    #[inline]
    pub fn linear_progress(&self) -> f32 {
        if self.duration_ms == 0 {
            return 1.0;
        }
        let elapsed_ms = self.transition_start.elapsed().as_millis() as f32;
        (elapsed_ms / self.duration_ms as f32).clamp(0.0, 1.0)
    }

    /// **Pure** opacity at the given (already eased or linear) progress `t`.
    ///
    /// `opacity_at(linear_progress())` is the linear fade; passing
    /// `easing.apply(linear_progress())` yields an eased fade.
    #[inline]
    pub fn opacity_at(&self, t: f32) -> f32 {
        self.from_opacity + (self.target_opacity - self.from_opacity) * t.clamp(0.0, 1.0)
    }

    /// Compute the current interpolated opacity (linear).
    ///
    /// Returns `target_opacity` once the transition has elapsed.
    pub fn current_opacity(&self) -> f32 {
        if self.duration_ms == 0 {
            return self.target_opacity;
        }
        self.opacity_at(self.linear_progress())
    }

    /// Compute the current interpolated opacity with an easing curve applied.
    ///
    /// Used by the portal tile transition path (hud-bq0gl.10) so collapse/expand
    /// fades accelerate/decelerate instead of ramping linearly. Zone subtitle
    /// fades keep [`current_opacity`](Self::current_opacity) (linear) so their
    /// contention/timing behavior is unchanged.
    pub fn current_opacity_eased(&self, easing: super::easing::Easing) -> f32 {
        if self.duration_ms == 0 {
            return self.target_opacity;
        }
        self.opacity_at(easing.apply(self.linear_progress()))
    }

    /// Returns `true` if the transition has fully completed.
    pub fn is_complete(&self) -> bool {
        self.transition_start.elapsed().as_millis() >= self.duration_ms as u128
    }
}

/// Stable key that uniquely identifies a single publication within a zone.
///
/// Composed of `(published_at_wall_us, publisher_namespace)`.  Because the
/// scene graph assigns `published_at_wall_us` from a monotonic wall clock, the
/// combination is unique per-zone across all practical usage.
pub(crate) type PubKey = (u64, String);

/// Per-publication opacity animation state for TTL-based fade-out.
///
/// Unlike [`ZoneAnimationState`] (which tracks zone-wide fade-in/fade-out),
/// `PublicationAnimationState` tracks the lifecycle of **one** publication
/// within a Stack zone.  Each notification gets its own instance.
///
/// Lifecycle:
/// 1. Created when the compositor first sees the publication.  `fade_start` is
///    `None` (publication is fully visible at opacity 1.0).
/// 2. When `first_seen.elapsed() >= ttl_ms`, the compositor sets
///    `fade_start = Some(Instant::now())` to begin the 150 ms fade-out.
/// 3. While fading: `current_opacity()` interpolates from 1.0 → 0.0 over
///    `NOTIFICATION_FADE_OUT_MS` ms.
/// 4. When `is_fade_complete()` returns `true`, the publication is removed
///    from `SceneGraph::zone_registry::active_publishes`.
///
/// The effective `ttl_ms` is derived by [`Compositor::publication_ttl_ms`] with
/// this priority order:
/// - `ZonePublishRecord.expires_at_wall_us` (urgency-derived, highest priority)
/// - `NotificationPayload.ttl_ms`
/// - Zone `auto_clear_ms` / `NOTIFICATION_DEFAULT_TTL_MS` (fallback)
pub struct PublicationAnimationState {
    /// Wall-clock instant when the compositor first rendered this publication.
    pub first_seen: std::time::Instant,
    /// Effective TTL in milliseconds.  Fade-out begins once this many ms
    /// have elapsed since `first_seen`.
    pub ttl_ms: u64,
    /// Instant when the fade-out transition started.  `None` means the
    /// publication is still fully visible (TTL has not yet expired).
    pub fade_start: Option<std::time::Instant>,
    /// Fade-out duration in milliseconds (always 150 for notifications).
    pub fade_duration_ms: u32,
}

/// Duration of the per-notification fade-out transition (ms).
pub(crate) const NOTIFICATION_FADE_OUT_MS: u32 = 150;

/// Default TTL used when no per-publication TTL is set and the zone has no
/// `auto_clear_ms`.  Matches the notification-area zone default (8 000 ms).
pub(crate) const NOTIFICATION_DEFAULT_TTL_MS: u64 = 8_000;

impl PublicationAnimationState {
    /// Create a new state for a freshly-seen publication.
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            first_seen: std::time::Instant::now(),
            ttl_ms,
            fade_start: None,
            fade_duration_ms: NOTIFICATION_FADE_OUT_MS,
        }
    }

    /// Check whether the TTL has expired and start the fade if so.
    ///
    /// Must be called once per frame per publication.  Idempotent after the
    /// fade has started.
    pub fn tick(&mut self) {
        if self.fade_start.is_none() && self.first_seen.elapsed().as_millis() as u64 >= self.ttl_ms
        {
            self.fade_start = Some(std::time::Instant::now());
        }
    }

    /// Returns the current effective opacity for this publication (0.0–1.0).
    ///
    /// Before fade: 1.0.
    /// During fade: linear interpolation from 1.0 → 0.0.
    /// After fade: 0.0.
    pub fn current_opacity(&self) -> f32 {
        let Some(start) = self.fade_start else {
            return 1.0;
        };
        if self.fade_duration_ms == 0 {
            return 0.0;
        }
        let elapsed_ms = start.elapsed().as_millis() as f32;
        let t = (elapsed_ms / self.fade_duration_ms as f32).clamp(0.0, 1.0);
        1.0 - t
    }

    /// Returns `true` when the fade-out transition has fully completed.
    pub fn is_fade_complete(&self) -> bool {
        let Some(start) = self.fade_start else {
            return false;
        };
        start.elapsed().as_millis() >= self.fade_duration_ms as u128
    }
}

/// How many frames to display each breakpoint segment before advancing.
///
/// At 60fps, 10 frames ≈ 167ms dwell per word — comfortably perceptible as
/// word-by-word reveal without feeling sluggish.
pub(super) const STREAM_REVEAL_FRAMES_PER_SEGMENT: u32 = 10;

/// Per-zone streaming reveal state.
///
/// Tracks progressive text reveal for `StreamText` publications that include
/// breakpoints.  Each frame, `advance()` is called; when the dwell counter
/// reaches [`STREAM_REVEAL_FRAMES_PER_SEGMENT`] the compositor moves to the
/// next breakpoint.
///
/// The `pub_key` ties this state to a specific publication.  When the
/// latest-wins publication changes, the old state is discarded and a new one
/// starts from breakpoint index 0.
pub struct StreamRevealState {
    /// The publication this state tracks.
    pub pub_key: PubKey,
    /// Byte-offset breakpoints copied from the publication record.
    /// Expected to be non-decreasing (callers should validate before constructing),
    /// but not enforced here — the compositor is safe regardless of order.
    pub breakpoints: Vec<usize>,
    /// Index into `breakpoints` of the currently-visible segment boundary.
    /// A value of `breakpoints.len()` means the full text is visible.
    pub segment_idx: usize,
    /// Frame counter within the current segment.
    pub frames_in_segment: u32,
}

impl StreamRevealState {
    /// Create reveal state for a new publication.
    ///
    /// Accepts `Vec<u64>` breakpoints (wire format from `ZonePublishRecord`) and
    /// converts to `Vec<usize>` for internal indexing arithmetic.
    pub fn new(pub_key: PubKey, breakpoints: Vec<u64>) -> Self {
        Self {
            pub_key,
            breakpoints: breakpoints.into_iter().map(|b| b as usize).collect(),
            segment_idx: 0,
            frames_in_segment: 0,
        }
    }

    /// Return the byte offset up to which text should be visible this frame.
    ///
    /// Returns `usize::MAX` (reveal all) when no breakpoints are set or all
    /// segments have been revealed.
    pub fn visible_byte_offset(&self) -> usize {
        if self.breakpoints.is_empty() || self.segment_idx >= self.breakpoints.len() {
            usize::MAX
        } else {
            self.breakpoints[self.segment_idx]
        }
    }

    /// Whether more breakpoint segments remain to be revealed.
    ///
    /// `true` while the word-by-word reveal is still progressing; `false` once
    /// every segment is visible (or there are no breakpoints). The idle render
    /// gate (hud-ilivg) treats an in-progress reveal as a reason to keep
    /// rendering so the animation never freezes mid-reveal.
    #[inline]
    pub fn is_revealing(&self) -> bool {
        self.segment_idx < self.breakpoints.len()
    }

    /// Advance the reveal state by one frame.
    ///
    /// Returns `true` if the reveal is still in progress (more segments remain).
    pub fn advance(&mut self) -> bool {
        if self.segment_idx >= self.breakpoints.len() {
            return false; // already fully revealed
        }
        self.frames_in_segment += 1;
        if self.frames_in_segment >= STREAM_REVEAL_FRAMES_PER_SEGMENT {
            self.frames_in_segment = 0;
            self.segment_idx += 1;
        }
        self.segment_idx < self.breakpoints.len()
    }
}

/// Per-portal-tile streaming-reveal state: fades **newly-appended** transcript
/// content into view segment-by-segment via [`StreamFadeRamp`], instead of
/// snapping the whole new chunk to full opacity in one frame.
///
/// Where [`StreamRevealState`] (the zone path) hard-*truncates* the visible text
/// at the current breakpoint — so each segment pops in at full opacity — this
/// keeps every byte laid out and instead ramps the **leading** (just-revealed)
/// segment's alpha from `0 → 1` across its dwell window. Already-revealed
/// segments render at full opacity; not-yet-revealed segments render fully
/// transparent (alpha `0`), so the reveal still advances word-by-word while the
/// active word *fades* rather than snaps (hud-bl7yi, deliverable #1/#3).
///
/// All offsets are byte offsets into [`Self::plain_text`] — the markdown
/// *plain-text* (post-strip) string the renderer actually lays out, so they line
/// up 1:1 with the `styled_runs` produced by
/// [`TextItem::from_text_markdown_cached`]. The state anchors to the exact
/// `plain_text` snapshot it was built from; the per-frame update re-anchors only
/// when that snapshot grows (a genuine append), so same-length churn (caret
/// blink, status edits) never re-triggers a fade.
#[derive(Clone, Debug)]
pub struct PortalTileStreamReveal {
    /// The plain-text snapshot this reveal is anchored to. Content growth is
    /// detected by diffing the next frame's plain-text against this.
    pub plain_text: std::sync::Arc<str>,
    /// Byte offset (into `plain_text`) where the revealing (fading) region
    /// starts — the common-prefix boundary with the previous snapshot. Bytes
    /// before this are pre-existing content and always render at full opacity.
    pub reveal_start: usize,
    /// Absolute byte offsets within `(reveal_start, plain_text.len()]`, one per
    /// word-segment boundary, strictly increasing, with the final entry equal to
    /// `plain_text.len()`. Empty ⇒ nothing to reveal (a settled tile).
    pub breakpoints: Vec<usize>,
    /// Index into `breakpoints` of the currently-fading (leading) segment.
    /// `breakpoints.len()` means the reveal is complete (steady state).
    pub segment_idx: usize,
    /// Frame counter within the current segment's dwell window.
    pub frames_in_segment: u32,
}

impl PortalTileStreamReveal {
    /// Build a reveal that fades the `[reveal_start, plain_text.len())` region in
    /// segment-by-segment, using `breakpoints` (absolute, increasing, last ==
    /// `plain_text.len()`).
    pub fn new(
        plain_text: std::sync::Arc<str>,
        reveal_start: usize,
        breakpoints: Vec<usize>,
    ) -> Self {
        Self {
            plain_text,
            reveal_start,
            breakpoints,
            segment_idx: 0,
            frames_in_segment: 0,
        }
    }

    /// Build a *settled* (fully-revealed, non-animating) anchor for `plain_text`.
    ///
    /// Used on first sight of a tile so pre-existing content is **not** faded in,
    /// and after a non-append change (edit/shrink) so the renderer shows the new
    /// content immediately. `is_revealing()` is `false`.
    pub fn settled(plain_text: std::sync::Arc<str>) -> Self {
        let len = plain_text.len();
        Self::new(plain_text, len, Vec::new())
    }

    /// Whether more segments remain to fade in.
    ///
    /// `true` while the per-segment reveal is still progressing; `false` once
    /// every segment is fully revealed (or there is nothing to reveal). The idle
    /// present-gate (#943) treats an in-flight reveal as a reason to keep
    /// rendering so the fade never freezes mid-reveal.
    #[inline]
    pub fn is_revealing(&self) -> bool {
        self.segment_idx < self.breakpoints.len()
    }

    /// Advance the reveal by one frame. Returns `true` while still in flight.
    ///
    /// Mirrors [`StreamRevealState::advance`]: dwell each segment for
    /// [`STREAM_REVEAL_FRAMES_PER_SEGMENT`] frames before moving to the next.
    pub fn advance(&mut self) -> bool {
        if self.segment_idx >= self.breakpoints.len() {
            return false; // already fully revealed
        }
        self.frames_in_segment += 1;
        if self.frames_in_segment >= STREAM_REVEAL_FRAMES_PER_SEGMENT {
            self.frames_in_segment = 0;
            self.segment_idx += 1;
        }
        self.segment_idx < self.breakpoints.len()
    }

    /// Eased alpha for the byte at `pos`, using `ramp` for the leading segment.
    ///
    /// - `pos < reveal_start` → `1.0` (pre-existing content, always opaque).
    /// - segment fully revealed → `1.0`.
    /// - leading (currently-fading) segment → `ramp.alpha(frames, window)`.
    /// - not-yet-revealed segment → `0.0` (laid out but invisible).
    ///
    /// Returns `1.0` for every byte once the reveal is complete, so a settled
    /// tile is byte-for-byte identical to the no-reveal path (deliverable #3).
    #[inline]
    pub fn alpha_for_byte(&self, pos: usize, ramp: super::easing::StreamFadeRamp) -> f32 {
        if !self.is_revealing() || pos < self.reveal_start {
            return 1.0;
        }
        // Segment index k owning `pos`: the count of breakpoints <= pos, since
        // segment k spans [prev_breakpoint, breakpoints[k]).
        let k = self.breakpoints.partition_point(|&b| b <= pos);
        if k < self.segment_idx {
            1.0
        } else if k == self.segment_idx {
            ramp.alpha(self.frames_in_segment, STREAM_REVEAL_FRAMES_PER_SEGMENT)
        } else {
            0.0
        }
    }
}

/// Length (bytes) of the longest common prefix of `a` and `b`, snapped back to a
/// UTF-8 character boundary in `a`.
///
/// Used to locate where a grown portal-tile transcript diverges from its prior
/// snapshot — everything from this offset to the new end is the freshly-appended
/// region that should fade in.
pub(super) fn common_prefix_len(a: &str, b: &str) -> usize {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let n = ab.len().min(bb.len());
    let mut i = 0;
    while i < n && ab[i] == bb[i] {
        i += 1;
    }
    // Back off to a char boundary so downstream slicing/offsets stay valid.
    while i > 0 && !a.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Derive word-segment breakpoints over `text[start..]`.
///
/// Emits one absolute byte offset at the end of each whitespace-delimited word
/// (the boundary *before* the trailing whitespace), plus a final entry equal to
/// `text.len()`. Leading whitespace before a word is absorbed into that word's
/// segment. All offsets fall on UTF-8 character boundaries (they come from
/// [`str::char_indices`]). Returns empty when `start >= text.len()`.
///
/// This is the portal-tile analogue of the publisher-supplied zone breakpoints:
/// the compositor derives them locally for the appended region since portal
/// transcript content carries no wire-level breakpoints.
pub(super) fn derive_word_breakpoints(text: &str, start: usize) -> Vec<usize> {
    let mut bps = Vec::new();
    // Defensively snap `start` back to a UTF-8 char boundary so the `text[start..]`
    // slice below can never panic, even if a future caller passes a mid-char
    // offset. (The current caller already snaps via `common_prefix_len`.)
    let mut start = start.min(text.len());
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    if start >= text.len() {
        return bps;
    }
    let mut in_word = false;
    for (i, ch) in text[start..].char_indices() {
        if ch.is_whitespace() {
            if in_word {
                bps.push(start + i);
                in_word = false;
            }
        } else {
            in_word = true;
        }
    }
    let end = text.len();
    if bps.last().copied() != Some(end) {
        bps.push(end);
    }
    bps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::easing::Easing;
    use crate::renderer::easing::StreamFadeRamp;

    const EPS: f32 = 1e-5;

    #[test]
    fn opacity_at_interpolates_between_endpoints() {
        let s = ZoneAnimationState::fade_in(100);
        assert!((s.opacity_at(0.0) - 0.0).abs() < EPS);
        assert!((s.opacity_at(1.0) - 1.0).abs() < EPS);
        assert!((s.opacity_at(0.5) - 0.5).abs() < EPS);
    }

    #[test]
    fn stream_reveal_is_revealing_tracks_progress() {
        // The idle render gate (hud-ilivg) treats a reveal as in-flight only
        // while breakpoint segments remain. Empty breakpoints reveal instantly
        // (never in flight); a non-empty reveal is in flight until fully revealed.
        let key: PubKey = (1, "agent".to_owned());

        // No breakpoints → reveal-all → never in flight.
        let none = StreamRevealState::new(key.clone(), vec![]);
        assert!(!none.is_revealing());

        // Two segments → in flight until both are revealed.
        let mut s = StreamRevealState::new(key, vec![3, 6]);
        assert!(
            s.is_revealing(),
            "fresh multi-segment reveal must be in flight"
        );
        // Advance through every dwell frame until fully revealed.
        let mut frames = 0;
        while s.advance() {
            frames += 1;
            assert!(frames < 1000, "reveal never completed");
        }
        assert!(
            !s.is_revealing(),
            "a fully-revealed stream must no longer be in flight"
        );
    }

    #[test]
    fn opacity_at_respects_from_and_target() {
        let s = ZoneAnimationState::fade_in_from(100, 0.4);
        // from=0.4, target=1.0 → at t=0.5 → 0.7.
        assert!((s.opacity_at(0.5) - 0.7).abs() < EPS);
        let out = ZoneAnimationState::fade_out(100);
        // from=1.0, target=0.0 → at t=0.25 → 0.75.
        assert!((out.opacity_at(0.25) - 0.75).abs() < EPS);
    }

    #[test]
    fn eased_opacity_shares_endpoints_but_curves_the_middle() {
        let s = ZoneAnimationState::fade_in(100);
        // Endpoints identical regardless of easing (pure-sampler equivalence).
        assert!((s.opacity_at(Easing::EaseInOut.apply(0.0)) - 0.0).abs() < EPS);
        assert!((s.opacity_at(Easing::EaseInOut.apply(1.0)) - 1.0).abs() < EPS);
        // EaseInOut is symmetric → midpoint equals linear midpoint (0.5)...
        assert!((s.opacity_at(Easing::EaseInOut.apply(0.5)) - 0.5).abs() < EPS);
        // ...but off-center it diverges from the linear ramp.
        let linear_quarter = s.opacity_at(0.25);
        let eased_quarter = s.opacity_at(Easing::EaseInOut.apply(0.25));
        assert!(
            eased_quarter < linear_quarter,
            "ease-in-out should lag linear in the first quarter: {eased_quarter} !< {linear_quarter}"
        );
    }

    #[test]
    fn zero_duration_reports_complete_and_target_opacity() {
        let s = ZoneAnimationState::fade_in(0);
        assert!((s.linear_progress() - 1.0).abs() < EPS);
        assert!((s.current_opacity() - 1.0).abs() < EPS);
        assert!((s.current_opacity_eased(Easing::EaseInOut) - 1.0).abs() < EPS);
        assert!(s.is_complete());
    }

    // ── PortalTileStreamReveal ──────────────────────────────────────────────

    #[test]
    fn common_prefix_len_basic_and_char_boundary() {
        assert_eq!(common_prefix_len("hello world", "hello there"), 6); // "hello "
        assert_eq!(common_prefix_len("abc", "abc"), 3);
        assert_eq!(common_prefix_len("", "abc"), 0);
        // Multi-byte: "é" is 2 bytes; a divergence mid-char must snap back.
        // "café" vs "cafx": bytes diverge inside 'é' → prefix snaps to 3 ("caf").
        assert_eq!(common_prefix_len("café", "cafx"), 3);
    }

    #[test]
    fn derive_word_breakpoints_splits_on_whitespace_and_ends_at_len() {
        let text = "alpha beta gamma";
        let bps = derive_word_breakpoints(text, 0);
        // Boundaries after "alpha" (5) and "beta" (10), final == len (16).
        assert_eq!(bps, vec![5, 10, text.len()]);
        assert_eq!(*bps.last().unwrap(), text.len());
    }

    #[test]
    fn derive_word_breakpoints_only_covers_region_after_start() {
        let text = "old new1 new2";
        // Reveal only the appended region starting at byte 4 ("new1 new2").
        let bps = derive_word_breakpoints(text, 4);
        assert!(bps.iter().all(|&b| b > 4), "all breakpoints after start");
        assert_eq!(*bps.last().unwrap(), text.len());
        // Empty when nothing to reveal.
        assert!(derive_word_breakpoints(text, text.len()).is_empty());
    }

    #[test]
    fn portal_reveal_settled_is_not_revealing_and_fully_opaque() {
        let r = PortalTileStreamReveal::settled("hello".into());
        assert!(!r.is_revealing());
        let ramp = StreamFadeRamp::default();
        for pos in 0..5 {
            assert!((r.alpha_for_byte(pos, ramp) - 1.0).abs() < EPS);
        }
    }

    #[test]
    fn portal_reveal_leading_segment_fades_others_snap() {
        let text = "old new1 new2";
        let plain: std::sync::Arc<str> = text.into();
        let bps = derive_word_breakpoints(text, 4); // region "new1 new2"
        let r = PortalTileStreamReveal::new(plain, 4, bps);
        let ramp = StreamFadeRamp::new(Easing::Linear);

        // Fresh reveal (frames_in_segment == 0): leading segment alpha == 0,
        // pre-existing prefix == 1, not-yet-revealed segment == 0.
        assert!(
            (r.alpha_for_byte(0, ramp) - 1.0).abs() < EPS,
            "prefix opaque"
        );
        assert!(r.alpha_for_byte(4, ramp).abs() < EPS, "leading starts at 0");
        // A byte in the second (not-yet-revealed) segment is fully hidden.
        let second = 9; // inside "new2"
        assert!(r.alpha_for_byte(second, ramp).abs() < EPS, "tail hidden");
    }

    #[test]
    fn portal_reveal_progress_increases_leading_alpha_then_completes() {
        let text = "x ab cd"; // start at 0: words "x","ab","cd"
        let plain: std::sync::Arc<str> = text.into();
        let bps = derive_word_breakpoints(text, 0);
        let mut r = PortalTileStreamReveal::new(plain, 0, bps);
        let ramp = StreamFadeRamp::new(Easing::Linear);

        let a0 = r.alpha_for_byte(0, ramp);
        // Advance a few frames within the first segment → leading alpha rises.
        r.advance();
        r.advance();
        let a1 = r.alpha_for_byte(0, ramp);
        assert!(
            a1 > a0,
            "leading-segment alpha must rise with progress: {a1} > {a0}"
        );

        // Drive to completion; every byte then renders fully opaque (steady).
        let mut guard = 0;
        while r.is_revealing() {
            r.advance();
            guard += 1;
            assert!(guard < 10_000, "reveal never completed");
        }
        for pos in 0..text.len() {
            assert!(
                (r.alpha_for_byte(pos, ramp) - 1.0).abs() < EPS,
                "settled reveal must be fully opaque at byte {pos}"
            );
        }
    }
}
