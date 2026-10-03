use tze_hud_input::{
    DragEventOutcome, InputProcessor, PointerEvent, PointerEventKind, PortalRect,
    PortalResizeState, PortalWindowTokens, ResizeBounds, ResizeEdge, ResizeOutcome, hit_affordance,
};
use tze_hud_scene::HitResult;
use tze_hud_scene::types::{DragHandleElementKind, TileScrollConfig, ZoneInteractionKind};
use unicode_width::UnicodeWidthChar;

use super::WinitApp;
use super::input_dispatch::{deliver_composer_batch, dispatch_portal_geometry_event};
use super::keyboard::ComposerDeliveryContext;
use super::lifecycle::{INTERACTION_LOCK_BUDGET, spin_acquire};

/// Result of one best-effort portal projection drain.
///
/// A busy scene is distinct from a completed no-op: the windowed scheduler
/// must install its coalesced availability wake only for the former. Treating
/// both as `false` loses the information needed to avoid retrying a past
/// portal deadline on a frame cadence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PortalProjectionDrain {
    Deferred,
    Completed { scene_changed: bool },
}

impl PortalProjectionDrain {
    pub(super) const fn scene_changed(self) -> bool {
        match self {
            Self::Deferred => false,
            Self::Completed { scene_changed } => scene_changed,
        }
    }

    pub(super) const fn is_deferred(self) -> bool {
        matches!(self, Self::Deferred)
    }
}

/// Approximate line height (physical px) used ONLY for the input-pane
/// history's scroll-content-height bookkeeping (hud-qbcp8).
///
/// Mirrors the compositor's default viewer-echo metrics
/// (`VIEWER_ECHO_DEFAULT_FONT_SIZE_PX` 15.0px × `LINE_HEIGHT_MULTIPLIER` 1.4,
/// see `crates/tze_hud_compositor/src/renderer/token_colors.rs` and
/// `crates/tze_hud_compositor/src/text.rs`) — see
/// [`WinitApp::append_raw_tile_viewer_echo`] for why an approximation is
/// used here instead of the compositor's wrap-accurate measurement.
const INPUT_HISTORY_APPROX_LINE_HEIGHT_PX: f32 = 21.0;

/// Approximate history font size (physical px) the compositor renders input-pane
/// history at — mirrors `VIEWER_ECHO_DEFAULT_FONT_SIZE_PX` (15.0) in
/// `crates/tze_hud_compositor/src/renderer/token_colors.rs`. Used only to derive
/// the conservative per-character advance below (hud-3y7va).
const INPUT_HISTORY_APPROX_FONT_SIZE_PX: f32 = 15.0;

/// Conservative per-character advance (physical px) used to OVER-estimate how
/// many wrapped visual rows a history entry occupies (hud-3y7va).
///
/// A larger-than-average advance narrows the estimated columns-per-row, biasing
/// the wrapped-row count UP so the seeded scroll content-height reaches or
/// exceeds the compositor's `real_max_scrollback` and the tail bottom-aligns
/// exactly. The `0.75` factor is deliberately wider than the composer
/// exemplar's own `0.57` monospace estimate (`COMPOSER_WRAP_CHAR_W`) so the
/// over-count holds for mixed sans-serif history text; the compositor's exact
/// per-frame clamp remains the visual authority, so over-counting only
/// guarantees the tail-pin and never clips the newest reply.
const INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX: f32 = INPUT_HISTORY_APPROX_FONT_SIZE_PX * 0.75;

/// Conservative per-character advance (physical px) for FULL-WIDTH glyphs —
/// East-Asian Wide/Fullwidth codepoints and emoji (`UnicodeWidthChar::width ==
/// Some(2)`) — used to OVER-estimate wrapped rows for CJK/emoji-heavy history
/// (hud-kaw7z).
///
/// The compositor shapes these to roughly one em (~15px), far wider than the
/// 0.75em narrow-Latin advance above. Classifying them with the narrow advance
/// over-estimates the columns-per-row, so the wrapped-row count UNDER-counts and
/// the seeded scroll offset undershoots `real_max_scrollback` — the newest
/// just-submitted line clips at rest (a narrow recurrence of the hud-3y7va
/// tail-clip). A full 1em bound restores the over-count for full-width content.
/// Full-width glyphs are 1em by definition and CJK/emoji line-break
/// per-character (no word-wrap slack to absorb, unlike Latin prose), so 1em is a
/// tight, safe upper bound. Keeping it separate from the narrow advance means
/// narrow Latin text is unchanged — no wider scroll dead-zone for the common
/// case.
const INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX: f32 = INPUT_HISTORY_APPROX_FONT_SIZE_PX;

/// Runtime scroll-seed metrics derived from the same viewer-local portal scale
/// the compositor applies to INPUT text.  Keeping the line pitch and both
/// character advances together prevents a resize from updating only part of the
/// conservative tail approximation.
#[derive(Clone, Copy, Debug)]
struct InputHistorySeedMetrics {
    line_height_px: f32,
    narrow_char_advance_px: f32,
    wide_char_advance_px: f32,
}

impl InputHistorySeedMetrics {
    fn for_tile_font_scale(
        tile_font_scale: f32,
        token_map: &std::collections::HashMap<String, String>,
    ) -> Self {
        // SceneGraph only accepts positive finite scales, but preserve the
        // unscaled baseline if this helper is ever called before validation.
        let scale = if tile_font_scale.is_finite() && tile_font_scale > 0.0 {
            tile_font_scale
        } else {
            1.0
        };
        // Mirror the compositor's `scaled_portal_font` contract for the default
        // 15px viewer-history font.  Runtime tail bookkeeping has no rasterizer,
        // but it must still use the FONT the compositor will actually shape —
        // including profile token overrides and the legibility clamp.
        let min = token_map
            .get(tze_hud_scene::types::PORTAL_TEXT_MIN_FONT_PX_TOKEN)
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(tze_hud_scene::types::PORTAL_TEXT_MIN_FONT_PX_DEFAULT);
        let max = token_map
            .get(tze_hud_scene::types::PORTAL_TEXT_MAX_FONT_PX_TOKEN)
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(tze_hud_scene::types::PORTAL_TEXT_MAX_FONT_PX_DEFAULT);
        let (min, max) = if min <= max { (min, max) } else { (max, min) };
        let effective_font_px = if (scale - 1.0).abs() < f32::EPSILON {
            INPUT_HISTORY_APPROX_FONT_SIZE_PX
        } else {
            (INPUT_HISTORY_APPROX_FONT_SIZE_PX * scale).clamp(min, max)
        };
        let effective_font_ratio = effective_font_px / INPUT_HISTORY_APPROX_FONT_SIZE_PX;
        Self {
            line_height_px: INPUT_HISTORY_APPROX_LINE_HEIGHT_PX * effective_font_ratio,
            narrow_char_advance_px: INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX
                * effective_font_ratio,
            wide_char_advance_px: INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX * effective_font_ratio,
        }
    }
}

/// Geometry used when the raw-tile history aggregate was last seeded.
///
/// The runtime intentionally does not retain the submitted text, but it must
/// remember enough of the old conservative measurement to bound a later
/// resize-induced rewrap before appending the next local echo.
#[derive(Clone, Copy, Debug)]
pub(super) struct InputHistorySeedState {
    line_height_px: f32,
    narrow_char_advance_px: f32,
    wide_char_advance_px: f32,
    wrap_width_px: f32,
}

impl InputHistorySeedState {
    fn new(metrics: InputHistorySeedMetrics, wrap_width_px: f32) -> Self {
        Self {
            line_height_px: metrics.line_height_px,
            narrow_char_advance_px: metrics.narrow_char_advance_px,
            wide_char_advance_px: metrics.wide_char_advance_px,
            wrap_width_px,
        }
    }

    /// Conservative physical-height multiplier for retained aggregate rows.
    ///
    /// For either glyph class, a prior estimated row can turn into at most
    /// `ceil(advance_ratio * old_wrap_width / new_wrap_width)` new rows. The
    /// ceiling is required because an old one-row logical line can cross a new
    /// boundary even when the aggregate density ratio is fractional. Taking the
    /// larger narrow/wide ratio protects mixed and CJK history without retaining
    /// historical text; the compositor still applies the exact visual clamp.
    fn conservative_rebase_multiplier(
        self,
        metrics: InputHistorySeedMetrics,
        wrap_width_px: f32,
    ) -> f32 {
        let valid = self.line_height_px.is_finite()
            && self.line_height_px > 0.0
            && self.narrow_char_advance_px.is_finite()
            && self.narrow_char_advance_px > 0.0
            && self.wide_char_advance_px.is_finite()
            && self.wide_char_advance_px > 0.0
            && self.wrap_width_px.is_finite()
            && self.wrap_width_px > 0.0
            && wrap_width_px.is_finite()
            && wrap_width_px > 0.0;
        if !valid {
            return 1.0;
        }

        let line_ratio = metrics.line_height_px / self.line_height_px;
        let advance_ratio = (metrics.narrow_char_advance_px / self.narrow_char_advance_px)
            .max(metrics.wide_char_advance_px / self.wide_char_advance_px);
        let row_multiplier = (advance_ratio * self.wrap_width_px / wrap_width_px)
            .ceil()
            .max(1.0);
        let multiplier = (line_ratio * row_multiplier).max(1.0);
        if multiplier.is_finite() {
            multiplier
        } else {
            1.0
        }
    }
}

/// Composer/history horizontal text margin (physical px) mirroring the
/// compositor's `COMPOSER_TEXT_MARGIN` in
/// `crates/tze_hud_compositor/src/renderer/tile_render.rs`, used to derive the
/// history band/wrap geometry the runtime seeds against (hud-3y7va).
const INPUT_COMPOSER_TEXT_MARGIN_PX: f32 = 6.0;

/// Stand-in for the compositor's viewer-echo timestamp prefix — the fixed
/// `HH:MM␠␠` (always 7 narrow columns) that `viewer_echo_display_text` prepends
/// to the FIRST line of every timestamped entry before wrapping
/// (`viewer_echo_timestamp_prefix` in
/// `crates/tze_hud_compositor/src/renderer/tile_render.rs`, hud-7ic89).
///
/// The wrapped-row seed must charge the same prefix or it under-counts a
/// full-width entry that nearly fills a row: the prefix spills it onto an extra
/// wrapped row the seed misses, and across several entries the seeded offset
/// undershoots `real_max_scrollback` and re-clips the newest reply — a narrow
/// recurrence of the hud-kaw7z CJK/emoji tail-clip. Every real prefix glyph
/// (digits, `:`, spaces) is narrow (Unicode width 1), so a 7-space stand-in
/// reproduces the prefix's wrap advance exactly without the runtime having to
/// duplicate the compositor's clock formatting.
const INPUT_HISTORY_TIMESTAMP_PREFIX_STANDIN: &str = "       ";

/// Conservative over-estimate of the number of wrapped visual rows the
/// compositor lays a single history entry into (hud-3y7va).
///
/// The runtime has no text rasterizer, so it cannot measure the wrap-accurate
/// row count `Compositor::prime_viewer_echo_layout` computes per frame. It
/// approximates by summing a conservative per-character advance across each
/// logical (`\n`-delimited) line and rounding UP — deliberately biased to
/// OVER-count (per-character upper-bound advances, per-line ceil, an empty line
/// counted as one row). Over-counting is safe and intended: the seeded
/// content-height must reach or exceed the compositor's `real_max_scrollback` so
/// the tail pins exactly, and the compositor's exact per-frame clamp is the
/// visual authority.
///
/// The advance is width-aware (hud-kaw7z): full-width glyphs (CJK/emoji) shape
/// far wider than narrow Latin, so a single flat advance either under-counts
/// full-width content (clipping the tail) or over-inflates narrow text (a wider
/// scroll dead-zone). Charging each glyph the wider of the two advances per its
/// Unicode width keeps the over-count safe for both.
fn approx_wrapped_visual_rows(
    text: &str,
    wrap_width_px: f32,
    narrow_advance_px: f32,
    wide_advance_px: f32,
) -> usize {
    let wrap_width_px = wrap_width_px.max(1.0);
    text.split('\n')
        .map(|line| {
            let line_width_px: f32 = line
                .chars()
                .map(|c| char_advance_px(c, narrow_advance_px, wide_advance_px))
                .sum();
            (line_width_px / wrap_width_px).ceil().max(1.0) as usize
        })
        .sum::<usize>()
        .max(1)
}

/// Conservative per-character advance (physical px): full-width glyphs
/// (East-Asian Wide/Fullwidth codepoints and emoji, `UnicodeWidthChar::width ==
/// Some(2)`) get the wide advance; everything else gets the narrow one
/// (hud-kaw7z).
///
/// The bias is deliberately toward wide: over-classifying a narrow glyph only
/// widens the (harmless, per-frame-re-clamped) scroll dead-zone, whereas
/// under-classifying a wide glyph under-counts its wrapped rows and clips the
/// newest reply at rest. Unicode width is a cell metric, not a pixel advance,
/// but it cleanly separates the ~1em full-width scripts/emoji this over-estimate
/// must cover from the sub-em glyphs the narrow advance already bounds.
fn char_advance_px(c: char, narrow_advance_px: f32, wide_advance_px: f32) -> f32 {
    if UnicodeWidthChar::width(c) == Some(2) {
        wide_advance_px
    } else {
        narrow_advance_px
    }
}

/// Conservative UNDER-estimate of the compositor's history band height (region
/// top → composer-box top) from the tile's full height (hud-3y7va).
///
/// The scrollable viewport for input history is the band ABOVE the composer box,
/// not the whole tile. Under-estimating the band (a smaller reference viewport)
/// biases the seeded scroll offset UP toward the tail, matching the intent that
/// the runtime seed must never undershoot `real_max_scrollback`. The composer
/// box at rest is one line plus symmetric vertical padding, mirroring
/// `Compositor::composer_input_box` with `visible_lines == 1`; the result is
/// floored at one line so the reference viewport is never zero or negative.
fn input_history_band_height_px(tile_height_px: f32, line_height_px: f32) -> f32 {
    // The compositor scales text-derived line height with the portal but keeps
    // its 6px content inset in physical pixels.  Mirror that mixed-unit contract
    // exactly so the conservative runtime seed reaches the visual tail.
    let line_height_px = line_height_px.max(1.0);
    let composer_box_px = line_height_px + INPUT_COMPOSER_TEXT_MARGIN_PX * 2.0;
    (tile_height_px - composer_box_px).max(line_height_px)
}

/// Wrap width (physical px) the compositor word-wraps history at — the tile
/// width minus symmetric text margins, mirroring `viewer_echo_zone_width` in
/// `crates/tze_hud_compositor/src/renderer/tile_render.rs` (hud-3y7va).
fn input_history_wrap_width_px(tile_width_px: f32) -> f32 {
    (tile_width_px - INPUT_COMPOSER_TEXT_MARGIN_PX * 2.0).max(1.0)
}

// ── Drag-to-move: data carried out of the scene-lock for post-lock work ──────

/// Payload returned by [`apply_drag_handle_pointer_event`] when a drag is
/// completed and the geometry must be persisted outside the scene lock.
pub(super) struct DragReleasedData {
    /// Scene-level ID of the element that was dragged.
    element_id: tze_hud_scene::SceneId,
    /// Final snapped+clamped top-left X in display pixels.
    final_x: f32,
    /// Final snapped+clamped top-left Y in display pixels.
    final_y: f32,
    /// Element width in display pixels (unchanged during drag).
    width: f32,
    /// Element height in display pixels (unchanged during drag).
    height: f32,
    /// Display width at time of release, used for `GeometryPolicy::Relative` normalisation.
    display_width: f32,
    /// Display height at time of release.
    display_height: f32,
    /// Agent namespace that owns the tile, used for `ElementRepositionedEvent` routing.
    namespace: String,
    /// When the release ended a **whole-portal** drag, the final bounds of every
    /// group member (including the grabbed tile), keyed by SceneId. Empty for a
    /// plain single-tile move.
    ///
    /// A text-stream portal is N tiles that share one namespace, so the
    /// namespace-matched `persist_drag_geometry` path reaches only one arbitrary
    /// member. To give *every* member a durable geometry override — so
    /// `list_elements` reports `has_user_override=true` for all of them and an
    /// adapter republish cannot pull any member back post-restart — the release
    /// carries each member's id + final rect for an id-keyed persist (hud-8vejp).
    group_members: Vec<(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)>,
}

// ── Portal-resize pointer: geometry carried out of scene lock ────────────────

/// Per-member geometry update produced by a whole-portal resize step.
///
/// A portal is composed of several constituent surfaces; a single resize step
/// scales all of them, so the outcome carries one snapshot per member for the
/// caller to broadcast.
pub(super) struct PortalMemberGeometry {
    /// Constituent-surface tile whose bounds were updated.
    pub(super) tile_id: tze_hud_scene::SceneId,
    /// Geometry snapshot carrying this member's scaled rect.
    pub(super) snapshot: tze_hud_input::GeometrySnapshot,
}

/// Outcome of a pointer-driven portal resize step, carried out of the scene
/// lock so that [`dispatch_portal_geometry_event`] can be called without
/// holding locks (fire-and-forget gRPC send).
pub(super) struct PortalResizePointerOutcome {
    /// Per-member geometry updates to broadcast (includes the anchor/frame,
    /// whose snapshot carries the whole-portal rect).
    pub(super) members: Vec<PortalMemberGeometry>,
    /// Display width at the time of the event (for geometry normalisation).
    pub(super) display_w: f32,
    /// Display height at the time of the event.
    pub(super) display_h: f32,
    /// Whether this step should durably persist each member's geometry override
    /// to the element store (hud-8vejp). Set only on the terminal PointerUp so
    /// the disk write fires once per gesture; intermediate down/move steps keep
    /// the group coherent via the in-session lock without touching disk.
    pub(super) persist: bool,
}

// The input processor, pointer event, hit result, scene, and display dimensions
// are all separate concerns that cannot be bundled into a context struct without
// creating an artificial abstraction; the argument count is genuinely necessary.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_drag_handle_pointer_event(
    input_processor: &mut InputProcessor,
    pointer_event: &PointerEvent,
    result_hit: &HitResult,
    scene: &mut tze_hud_scene::graph::SceneGraph,
    display_width: f32,
    display_height: f32,
) -> Option<DragReleasedData> {
    let device_id = pointer_event.device_id;

    // Determine which drag handle (if any) was hit on this event.
    let hit_drag_info: Option<(&str, tze_hud_scene::SceneId, DragHandleElementKind, bool)> =
        match result_hit {
            HitResult::ZoneInteraction {
                interaction_id,
                kind:
                    ZoneInteractionKind::DragHandle {
                        element_id,
                        element_kind,
                        is_header_band,
                    },
                ..
            } => Some((
                interaction_id.as_str(),
                *element_id,
                *element_kind,
                *is_header_band,
            )),
            _ => None,
        };

    // On PointerDown on a drag handle, start accumulating.
    if pointer_event.kind == PointerEventKind::Down {
        if let Some((interaction_id, element_id, element_kind, is_header_band)) = hit_drag_info {
            let element_bounds = scene
                .tiles
                .get(&element_id)
                .map(|t| t.bounds)
                .unwrap_or_else(|| tze_hud_scene::Rect::new(0.0, 0.0, 0.0, 0.0));
            let outcome = input_processor.process_drag_handle_pointer(
                pointer_event,
                interaction_id,
                element_id,
                element_kind,
                element_bounds,
                display_width,
                display_height,
                is_header_band,
            );
            tracing::trace!(
                element_id = %element_id,
                x = pointer_event.x,
                y = pointer_event.y,
                ?outcome,
                "drag-handle: PointerDown accumulating"
            );
        }
        return None;
    }

    // On PointerMove or PointerUp, check for an in-flight drag on this device.
    let drag_info = input_processor
        .drag_states
        .get(&device_id)
        .map(|s| (s.interaction_id.clone(), s.element_id, s.element_kind));

    let Some((interaction_id, element_id, element_kind)) = drag_info else {
        // No drag in progress for this device — nothing to do.
        return None;
    };

    // Snapshot element bounds; element_id is the tile being dragged.
    let element_bounds = scene
        .tiles
        .get(&element_id)
        .map(|t| t.bounds)
        .unwrap_or_else(|| tze_hud_scene::Rect::new(0.0, 0.0, 0.0, 0.0));

    // The drag is already in flight for this device; its DeviceDragState already
    // carries the immediate/band flag, so the value passed here is unused on
    // Move/Up (it only seeds a new state on Down).
    let outcome = input_processor.process_drag_handle_pointer(
        pointer_event,
        &interaction_id,
        element_id,
        element_kind,
        element_bounds,
        display_width,
        display_height,
        false,
    );

    match outcome {
        DragEventOutcome::Idle | DragEventOutcome::Accumulating { .. } => {
            // Nothing to do locally.
            None
        }
        DragEventOutcome::Activated { element_id, .. } => {
            tracing::debug!(
                element_id = %element_id,
                "drag-handle: drag activated — element follows pointer"
            );
            None
        }
        DragEventOutcome::Cancelled => {
            tracing::trace!(
                element_id = %element_id,
                "drag-handle: drag cancelled (tap or moved beyond tolerance)"
            );
            None
        }
        DragEventOutcome::Moved {
            element_id: eid,
            new_x,
            new_y,
            ..
        } => {
            // Update tile bounds directly (chrome-layer bypass — no lease check).
            // For a text-stream portal the drag moves the WHOLE portal as one
            // unit (hud-lyqun): translate every group member by the same delta so
            // the constituents never fracture. Falls back to a single-tile move
            // for non-portal drags.
            let old = scene.tiles.get(&eid).map(|t| t.bounds);
            if let Some(old) = old {
                let dx = new_x - old.x;
                let dy = new_y - old.y;
                if !translate_portal_group_on_drag(scene, eid, dx, dy) {
                    let moved = if let Some(tile) = scene.tiles.get_mut(&eid) {
                        tile.bounds.x = new_x;
                        tile.bounds.y = new_y;
                        true
                    } else {
                        false
                    };
                    if moved {
                        // Position-only single-tile move: geometry epoch, not
                        // version — same reasoning as the group path (hud-uyhpn).
                        scene.bump_geometry_epoch();
                    }
                }
                tracing::trace!(
                    element_id = %eid,
                    old_x = old.x,
                    old_y = old.y,
                    new_x,
                    new_y,
                    "drag-handle: tile moved"
                );
            }
            None
        }
        DragEventOutcome::Released {
            element_id: eid,
            final_x,
            final_y,
            element_kind: _,
        } => {
            let (old_x, old_y, width, height) = scene
                .tiles
                .get(&eid)
                .map(|t| (t.bounds.x, t.bounds.y, t.bounds.width, t.bounds.height))
                .unwrap_or((0.0, 0.0, 0.0, 0.0));

            // Apply the final position. As with move, a portal drag relocates the
            // whole group coherently (hud-lyqun); non-portal drags move the single
            // tile.
            let dx = final_x - old_x;
            let dy = final_y - old_y;
            let group_members: Vec<(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)> =
                if translate_portal_group_on_drag(scene, eid, dx, dy) {
                    // Whole-portal move applied: snapshot every member's final
                    // bounds so the caller can persist a durable per-member
                    // geometry override keyed by SceneId (hud-8vejp). Without
                    // this only the grabbed member's namespace-matched entry
                    // gets an override.
                    resolve_portal_group(scene, eid)
                        .map(|group| {
                            group
                                .member_ids
                                .iter()
                                .filter_map(|id| scene.tiles.get(id).map(|t| (*id, t.bounds)))
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    let moved = if let Some(tile) = scene.tiles.get_mut(&eid) {
                        tile.bounds.x = final_x;
                        tile.bounds.y = final_y;
                        true
                    } else {
                        false
                    };
                    if moved {
                        // Position-only single-tile move: geometry epoch, not
                        // version — same reasoning as the group path (hud-uyhpn).
                        scene.bump_geometry_epoch();
                    }
                    Vec::new()
                };

            let namespace = scene
                .tiles
                .get(&eid)
                .map(|t| t.namespace.clone())
                .unwrap_or_default();

            tracing::debug!(
                element_id = %eid,
                final_x,
                final_y,
                width,
                height,
                "drag-handle: drag released — persisting geometry"
            );

            // Return data the caller will use to persist after releasing locks.
            Some(DragReleasedData {
                element_id: eid,
                final_x,
                final_y,
                width,
                height,
                display_width,
                display_height,
                namespace,
                group_members,
            })
        }
    }
}

/// Compute the maximum resize dimensions for a portal tile, combining the
/// display boundary clamp with the tile's lease-granted spatial budget.
///
/// The display boundary (`display_w`, `display_h`) is always the hard outer
/// clamp.  When the tile's lease carries a non-zero `max_tile_width_px` /
/// `max_tile_height_px`, whichever is smaller wins (most-restrictive policy).
///
/// `lease_max_width_px` and `lease_max_height_px` should be sourced from
/// the `Lease.resource_budget` in `scene.leases` (the authoritative location
/// for spatial budget limits).  Pass `0.0` for each to indicate unconstrained.
///
/// A lease budget value of `0.0` means unconstrained: only the display boundary
/// applies.
///
/// `min_*` values are from the portal tokens and serve as the floor (a portal
/// can always be grown to at least the minimum size even if the lease limit is
/// somehow smaller — the lease limit is not allowed to shrink a portal below the
/// token minimum, per §6b design intent).
///
/// ## Why `display_w`, not `display_w − tile.x`
///
/// `PortalRect::clamped` enforces that the portal fits on-screen by adjusting
/// both the size **and** the origin together: `x.clamp(0, display_w − w)`.
/// If the portal is at x=500 on a 1000px display, it can still grow to
/// 1000px wide — `clamped` will shift the origin left to x=0 so the right
/// edge lands exactly at the screen edge.  Using `display_w − tile.x` as the
/// max would prevent this shift and cap the portal at half the display width,
/// which is incorrect.
fn compute_portal_max_dims(
    lease_max_width_px: f32,
    lease_max_height_px: f32,
    display_w: f32,
    display_h: f32,
    min_width_px: f32,
    min_height_px: f32,
) -> (f32, f32) {
    // Display boundary: the portal cannot be wider/taller than the display.
    // The origin is clamped separately by PortalRect::clamped, so we do not
    // subtract tile.x / tile.y here (that was the pre-fix incorrect bound).
    let display_max_w = display_w.max(min_width_px);
    let display_max_h = display_h.max(min_height_px);

    // Lease budget: intersect with display boundary (most restrictive wins).
    // A lease value of 0.0 means unconstrained; skip the intersection.
    let max_w = if lease_max_width_px > 0.0 {
        display_max_w.min(lease_max_width_px).max(min_width_px)
    } else {
        display_max_w
    };
    let max_h = if lease_max_height_px > 0.0 {
        display_max_h.min(lease_max_height_px).max(min_height_px)
    } else {
        display_max_h
    };
    (max_w, max_h)
}

// ── Portal-group resolution (hud-fb3en) ──────────────────────────────────────

/// A resolved portal group: the constituent scene tiles that move and resize as
/// one coherent unit.
///
/// A text-stream portal is composed of several independent scene tiles that
/// share a single lease (frame, transcript/output pane, composer/input pane,
/// minimized icon, capture backstop, drag shield). The runtime has no explicit
/// portal-group field on [`tze_hud_scene::types::Tile`], so the group is resolved
/// structurally: the frame is the largest-area lease member (the portal-sized
/// frame / capture backstop), and its top-left is the fixed anchor for
/// grow/shrink (matching the top-left anchor semantics from PR #981). Members
/// are the lease's tiles whose bounds lie within the frame rect.
///
/// This deliberately EXCLUDES the drag shield, which the client parks in a far
/// display corner and which must not scale with the portal — mirroring the
/// client-side `portal_bounds_mutations` layout, which omits the drag shield
/// from the visible-portal geometry.
///
/// A single-tile lease resolves to a one-member group (the tile is its own
/// anchor), preserving the pre-fix single-surface behavior.
pub(super) struct PortalGroup {
    /// The frame/anchor tile — its bounds are the whole-portal rect.
    pub(super) anchor_tile_id: tze_hud_scene::SceneId,
    /// Whole-portal rect (anchor frame bounds) at resolution time.
    pub(super) portal_rect: PortalRect,
    /// All member tile ids that scale with the portal (includes the anchor).
    pub(super) member_ids: Vec<tze_hud_scene::SceneId>,
    /// Stable per-portal hash (from the anchor tile) for `PortalResizeState`.
    pub(super) portal_id_hash: u64,
}

/// Return true when `inner` lies within `outer`, allowing a small epsilon so
/// sub-pixel rounding at pane edges does not drop a legitimate member.
pub(super) fn rect_contains(
    outer: &tze_hud_scene::Rect,
    inner: &tze_hud_scene::Rect,
    eps: f32,
) -> bool {
    inner.x >= outer.x - eps
        && inner.y >= outer.y - eps
        && inner.x + inner.width <= outer.x + outer.width + eps
        && inner.y + inner.height <= outer.y + outer.height + eps
}

/// Resolve the whole-portal group that owns `member_tile_id`.
///
/// Works from ANY member tile — a focused pane on the hotkey / pointer-down
/// path, or the stored anchor tile on the pointer-move / pointer-up path: it
/// looks up the shared lease, picks the largest-area lease member as the
/// frame/anchor, then collects the lease members spatially contained within the
/// frame rect (plus the seed tile itself, defensively).
///
/// Returns `None` if the tile does not exist.
pub(super) fn resolve_portal_group(
    scene: &tze_hud_scene::graph::SceneGraph,
    member_tile_id: tze_hud_scene::SceneId,
) -> Option<PortalGroup> {
    let seed = scene.tiles.get(&member_tile_id)?;
    let lease_id = seed.lease_id;

    // Largest-area lease member is the frame/anchor (ties → lowest id). Delegated
    // to the scene-crate resolver so the frame pick is a single source of truth
    // shared with the compositor's header-band drag handle (hud-643dv).
    let anchor_id = scene.portal_anchor_tile(member_tile_id)?;
    let anchor_bounds = scene.tiles.get(&anchor_id)?.bounds;

    // Members = lease tiles spatially within the frame rect. The far-corner
    // drag shield falls outside the frame and is excluded. The seed and anchor
    // are always included.
    let eps = 1.0_f32;
    let mut member_ids: Vec<tze_hud_scene::SceneId> = scene
        .tiles
        .iter()
        .filter(|(id, tile)| {
            tile.lease_id == lease_id
                && (**id == member_tile_id
                    || **id == anchor_id
                    || rect_contains(&anchor_bounds, &tile.bounds, eps))
        })
        .map(|(id, _)| *id)
        .collect();
    member_ids.sort();

    let portal_rect = PortalRect {
        x: anchor_bounds.x,
        y: anchor_bounds.y,
        width: anchor_bounds.width,
        height: anchor_bounds.height,
    };
    let portal_id_hash = anchor_id.as_uuid().as_u128() as u64;

    Some(PortalGroup {
        anchor_tile_id: anchor_id,
        portal_rect,
        member_ids,
        portal_id_hash,
    })
}

/// Compute new bounds for every group member under a top-left-anchored uniform
/// scale from `old_rect` → `new_rect`.
///
/// The portal's top-left (`new_rect` origin) is the fixed anchor. Each member's
/// offset from the anchor and its size scale by the per-axis ratio, preserving
/// relative layout. Returns `(tile_id, new_rect)` pairs for members that still
/// exist in the scene.
fn scale_portal_members(
    scene: &tze_hud_scene::graph::SceneGraph,
    group: &PortalGroup,
    old_rect: PortalRect,
    new_rect: PortalRect,
) -> Vec<(tze_hud_scene::SceneId, tze_hud_scene::Rect)> {
    let r_w = if old_rect.width > 0.0 {
        new_rect.width / old_rect.width
    } else {
        1.0
    };
    let r_h = if old_rect.height > 0.0 {
        new_rect.height / old_rect.height
    } else {
        1.0
    };
    let mut updates = Vec::with_capacity(group.member_ids.len());
    for &tile_id in &group.member_ids {
        let Some(tile) = scene.tiles.get(&tile_id) else {
            continue;
        };
        // The anchor/frame IS the whole-portal rect: assign it exactly rather
        // than round-tripping through the scale ratio. This keeps a single-tile
        // portal bit-identical to the new rect (no float drift), which matters
        // for the `scene.version` size-change guard driving the compositor's
        // truncation-cache re-prime cadence.
        let new_bounds = if tile_id == group.anchor_tile_id {
            tze_hud_scene::Rect::new(new_rect.x, new_rect.y, new_rect.width, new_rect.height)
        } else {
            let b = tile.bounds;
            tze_hud_scene::Rect::new(
                new_rect.x + (b.x - old_rect.x) * r_w,
                new_rect.y + (b.y - old_rect.y) * r_h,
                b.width * r_w,
                b.height * r_h,
            )
        };
        updates.push((tile_id, new_bounds));
    }
    updates
}

/// Scale a tile's node tree in place by the per-axis ratio `(r_w, r_h)`.
///
/// Node bounds are **tile-local** (origin + extent relative to the tile's
/// top-left) and independent of `Tile::bounds` — nothing derives them from the
/// tile size. So when the viewer resizes a tile, the node tree must scale by the
/// same ratio or content keeps laying out to the old geometry. In particular the
/// compositor wraps `TextMarkdownNode` text to `node.bounds.width` (the layout
/// column in `TextItem::from_text_markdown_cached` / `from_text_markdown_node`),
/// so without this the transcript/composer text keeps its attach-time wrap width
/// and does not re-flow to the resized pane (hud-rpmwt). Font size is untouched —
/// text reflows to the new width, it does not zoom.
fn scale_tile_node_tree(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    tile_id: tze_hud_scene::SceneId,
    r_w: f32,
    r_h: f32,
) {
    let Some(root) = scene.tiles.get(&tile_id).and_then(|t| t.root_node) else {
        return;
    };
    // Collect the subtree ids with an immutable walk first, then mutate — a
    // tile's node tree is a small DAG-free tree (≤ MAX_NODES_PER_TILE) so this
    // avoids aliasing the node store while descending `children`.
    let mut stack = vec![root];
    let mut ids = Vec::new();
    while let Some(id) = stack.pop() {
        let Some(node) = scene.nodes.get(&id) else {
            continue;
        };
        ids.push(id);
        stack.extend(node.children.iter().copied());
    }
    for id in ids {
        if let Some(node) = scene.nodes.get_mut(&id) {
            let b = node.data.bounds_mut();
            b.x *= r_w;
            b.y *= r_h;
            b.width *= r_w;
            b.height *= r_h;
        }
    }
}

/// Scale a tile's first-class [`PortalSurface`] part bounds in lock-step with a
/// whole-portal resize (hud-s4lrw).
///
/// `PortalPart::bounds` is tile-local surface geometry — the per-part clip band
/// the compositor uses to keep one part's content (e.g. an overlong transcript)
/// from painting over a sibling part's region. When the viewer resizes the
/// portal, [`scale_tile_node_tree`] re-flows the part *nodes* to the new pane;
/// the surface descriptor's declared part bounds must scale by the same per-axis
/// ratio or the render-side clip band drifts off the content and the per-part
/// overflow invariant no longer holds at the new geometry.
///
/// Part bounds are tile-local (origin relative to the tile top-left), so — like
/// the node tree — they scale by the raw ratio; a whole-portal *drag* leaves
/// them untouched (only `tile.bounds` moves). No-op when the tile carries no
/// surface. Mutates the public `overlay.portal_surfaces` state directly (no
/// lease/capability gate): this is viewer-driven geometry authority, the same
/// path that writes `tile.bounds` above.
fn scale_portal_surface_parts(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    tile_id: tze_hud_scene::SceneId,
    r_w: f32,
    r_h: f32,
) {
    let Some(surface) = scene.overlay.portal_surfaces.get_mut(&tile_id) else {
        return;
    };
    for part in &mut surface.parts {
        part.bounds.x *= r_w;
        part.bounds.y *= r_h;
        part.bounds.width *= r_w;
        part.bounds.height *= r_h;
    }
}

/// Apply a resolved whole-portal resize to the scene: write each member's scaled
/// bounds, bump the scene version once if any geometry changed, and build the
/// per-member geometry snapshots to broadcast.
///
/// `primary` is the whole-portal geometry snapshot produced by the resize state
/// machine (its `rect` is the new anchor/frame rect). Each returned member
/// snapshot carries the same sequence / gesture flag but with that member's own
/// scaled rect, so per-member `ElementRepositionedEvent`s report correct
/// geometry.
/// Write a durable `Relative` geometry override into the element store for each
/// `(SceneId, rect)` member, keyed by its stable scene id (hud-8vejp).
///
/// Shared by the whole-portal resize persist path
/// ([`WinitApp::persist_portal_member_overrides`]) and the whole-portal drag
/// persist path ([`WinitApp::persist_drag_release`]). Keying by id — not by the
/// shared portal namespace — is what gives *every* member its own durable
/// override, so `list_elements` reports `has_user_override=true` for all members
/// (not just the drag-release/namespace one) and an adapter republish can never
/// reposition a member post-restart (the stored override is authoritative at the
/// publish ingress). `rect` is absolute display-pixel bounds, normalised to a
/// `Relative` policy against `display_w`/`display_h` before storage.
///
/// Members with no matching store entry are skipped (the id-keyed setter is a
/// no-op) — a member tile must be registered in the store before it can carry a
/// durable override.
fn write_member_geometry_overrides(
    store: &mut tze_hud_scene::element_store::ElementStore,
    members: &[(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)],
    display_w: f32,
    display_h: f32,
) {
    for (id, bounds) in members {
        let policy =
            tze_hud_scene::types::rect_to_relative_geometry_policy(*bounds, display_w, display_h);
        store.set_geometry_override(*id, policy);
    }
}

fn commit_portal_group_resize(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    group: &PortalGroup,
    old_rect: PortalRect,
    primary: tze_hud_input::GeometrySnapshot,
) -> Vec<PortalMemberGeometry> {
    let new_rect = primary.rect;
    let updates = scale_portal_members(scene, group, old_rect, new_rect);
    let mut any_changed = false;
    // Whole-portal WIDTH ratio — the single deterministic ratio the spec mandates
    // for text scaling (never anisotropic): the viewer-local per-tile font-scale
    // multiplier accumulates this factor across resize steps so transcript and
    // composer text grow/shrink with the portal (hud-ovjxu.1). Applied per member
    // below (gated on a real change); the compositor clamps the effective font to
    // the token-defined legible min/max at render, never mutating the
    // adapter-published `font_size_px`.
    let font_ratio = if old_rect.width > 0.0 {
        new_rect.width / old_rect.width
    } else {
        1.0
    };
    let mut members = Vec::with_capacity(updates.len());
    for (tile_id, new_bounds) in updates {
        let old_tile_bounds = scene.tiles.get(&tile_id).map(|t| t.bounds);
        if let Some(tile) = scene.tiles.get_mut(&tile_id) {
            if tile.bounds.width != new_bounds.width || tile.bounds.height != new_bounds.height {
                any_changed = true;
            }
            tile.bounds = new_bounds;
        }
        // Scale the tile's node tree in lock-step with the tile so tile-local
        // node geometry — and the text wrap width the compositor reads from
        // `TextMarkdownNode::bounds.width` — re-resolves to the new pane. Use
        // each tile's OWN size ratio (not the whole-portal ratio) so the nodes
        // track exactly the tile they live in. Without this the frame scales but
        // the transcript/composer text stays wrapped at the old width (hud-rpmwt).
        if let Some(old) = old_tile_bounds {
            let node_r_w = if old.width > 0.0 {
                new_bounds.width / old.width
            } else {
                1.0
            };
            let node_r_h = if old.height > 0.0 {
                new_bounds.height / old.height
            } else {
                1.0
            };
            if node_r_w != 1.0 || node_r_h != 1.0 {
                scale_tile_node_tree(scene, tile_id, node_r_w, node_r_h);
                // Keep any first-class portal-surface part bounds in lock-step
                // with the re-flowed node tree so the compositor's per-part clip
                // band stays aligned at the new geometry (hud-s4lrw). No-op for
                // legacy raw-tile portals with no surface descriptor.
                scale_portal_surface_parts(scene, tile_id, node_r_w, node_r_h);
            }
        }
        // The viewer now owns this member's geometry: take authority so an
        // adapter republishing its stale client-side layout on the next content
        // publish or drag cannot stomp the member back and fracture the portal
        // group (hud-lyqun). Viewer-driven resize writes `tile.bounds` directly
        // (above), so this only gates adapter-originated `UpdateTileBounds`.
        scene.lock_viewer_geometry(tile_id);
        // Accumulate the whole-portal width ratio into this member's viewer-local
        // font-scale (hud-ovjxu.1). Gated on a real ratio so a clamped-at-boundary
        // press does not drift the scale. Uniform across members (all get the same
        // portal ratio) so text stays consistent portal-wide.
        if (font_ratio - 1.0).abs() > f32::EPSILON {
            let next = scene.tile_font_scale(tile_id) * font_ratio;
            scene.set_tile_font_scale(tile_id, next);
        }
        let snapshot = tze_hud_input::GeometrySnapshot {
            rect: PortalRect {
                x: new_bounds.x,
                y: new_bounds.y,
                width: new_bounds.width,
                height: new_bounds.height,
            },
            ..primary
        };
        members.push(PortalMemberGeometry { tile_id, snapshot });
    }
    // The scene version drives the compositor's truncation-cache re-prime at the
    // new (intermediate) geometry; bump once per whole-portal step, guarded on a
    // real size change so a clamped-at-boundary press does not churn the cache.
    if any_changed {
        scene.version += 1;
    }
    members
}

/// Translate a whole portal group by `(dx, dy)` when the viewer drags one of its
/// constituent surfaces, preserving the group's relative layout and taking
/// viewer geometry authority over every member (hud-lyqun).
///
/// A text-stream portal is N independent tiles (frame + scrollable panes + drag
/// shield). Before this, the chrome drag handler moved only the single grabbed
/// tile, so dragging a portal fractured it (and, after a prior whole-portal
/// resize, the grabbed surface floated away from the rest). Here the dragged
/// tile's motion delta is applied to every group member so the portal moves as
/// one coherent unit — the completion of the whole-unit gesture work started for
/// resize in PR #984.
///
/// Gated to real portals: the resolved group must have more than one member and
/// contain at least one scrollable constituent surface. A plain single tile /
/// widget / zone drag resolves to a lone or non-portal group and is left to the
/// single-element move path (returns `false`). The far-corner drag shield is
/// excluded by `resolve_portal_group` and stays parked.
///
/// Returns `true` when a whole-portal translate was applied.
fn translate_portal_group_on_drag(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    dragged_tile_id: tze_hud_scene::SceneId,
    dx: f32,
    dy: f32,
) -> bool {
    let Some(group) = resolve_portal_group(scene, dragged_tile_id) else {
        return false;
    };
    let is_portal = group.member_ids.len() > 1
        && group
            .member_ids
            .iter()
            .any(|id| scene.tile_scroll_config(*id).is_some());
    if !is_portal {
        return false;
    }

    for &tile_id in &group.member_ids {
        if let Some(tile) = scene.tiles.get_mut(&tile_id) {
            tile.bounds.x += dx;
            tile.bounds.y += dy;
        }
        // Viewer geometry authority — same as the resize path — so an adapter
        // republish cannot pull a member back to its stale layout.
        scene.lock_viewer_geometry(tile_id);
    }
    // Position-only mutation: bump the geometry epoch (re-arms the present-gate
    // so every member paints at its new position this frame) but NOT
    // scene.version — a translation changes no content and no size, so the
    // compositor's version-gated markdown/truncation caches must NOT re-prime.
    // Bumping version here forced a full per-frame re-hash/re-shape and made the
    // live drag low-fps / flickery (hud-uyhpn). #986 group coherence and #989
    // resize reflow both key off size and are unaffected.
    scene.bump_geometry_epoch();
    true
}

/// Pointer-driven portal resize state machine step.
///
/// Called from [`WinitApp::enqueue_pointer_event`] while the scene lock is held.
/// Drives `PortalResizeState` through the pointer-down / pointer-move / pointer-up
/// lifecycle for resize affordances (§6b.1 pointer resize scenario).
///
/// On **PointerDown**: resolves the portal from the tile UNDER THE POINTER
/// (falling back to the focused tile only when the pointer is over no tile) and
/// hit-tests its resize affordances.  Resolving from the pointer rather than the
/// focused tile is deliberate: the initiating click-to-focus moves focus onto
/// the non-scrollable frame under the affordance corner, so a focus-gated resize
/// never starts under real OS pointer input (hud-yno2r).  If the pointer lands on
/// an affordance, starts the gesture and returns a [`PortalResizePointerOutcome`]
/// with the initial snapshot so the caller can apply local bounds and broadcast
/// the geometry event.
///
/// On **PointerMove**: if a gesture is active for `device_id`, computes the new
/// intermediate rect and applies it to the scene immediately (local-first).
///
/// On **PointerUp**: ends the gesture, applies the final clamped rect, and
/// returns an outcome the caller must broadcast.
///
/// Returns `None` when there is nothing to do (no portal under the pointer,
/// pointer outside affordances, no gesture active, or lock contention).
///
/// ## Gesture authority
///
/// The gesture epoch is advanced inside `PortalResizeState::on_pointer_down` (to
/// odd, blocking adapter publishes) and `on_pointer_up` (back to even, releasing
/// the block after the gesture ends).  The caller MUST propagate snapshots with
/// `gesture_active = true` to prevent adapter geometry from stomping the in-flight
/// resize.
// The scene, pointer, focus, resize-state, display-dims, and token arguments
// are all necessary and unrelated — merging them into a struct would create an
// ad-hoc context object with no benefit.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_portal_resize_pointer_event(
    pointer_event: &PointerEvent,
    portal_resize_states: &mut std::collections::HashMap<tze_hud_scene::SceneId, PortalResizeState>,
    active_tab: Option<tze_hud_scene::SceneId>,
    focus_manager: &tze_hud_input::FocusManager,
    scene: &mut tze_hud_scene::graph::SceneGraph,
    display_w: f32,
    display_h: f32,
    tokens: PortalWindowTokens,
) -> Option<PortalResizePointerOutcome> {
    let device_id = pointer_event.device_id;
    let x = pointer_event.x;
    let y = pointer_event.y;

    // Build the clamping bounds for a whole-portal rect owned by `anchor_lease`.
    let resize_bounds_for_lease =
        |scene: &tze_hud_scene::graph::SceneGraph, anchor_lease: tze_hud_scene::SceneId| {
            let (lease_max_w, lease_max_h) = scene
                .leases
                .get(&anchor_lease)
                .map(|l| {
                    (
                        l.spatial_budget.max_tile_width_px,
                        l.spatial_budget.max_tile_height_px,
                    )
                })
                .unwrap_or((0.0, 0.0));
            let (max_width_px, max_height_px) = compute_portal_max_dims(
                lease_max_w,
                lease_max_h,
                display_w,
                display_h,
                tokens.min_width_px,
                tokens.min_height_px,
            );
            ResizeBounds {
                tokens,
                max_width_px,
                max_height_px,
                display_w,
                display_h,
            }
        };

    match pointer_event.kind {
        PointerEventKind::Down => {
            // Resolve the portal from the tile UNDER THE POINTER, not the
            // *focused* tile. The resize affordance sits on the frame's
            // bottom-right corner — a non-scrollable tile — and the very same
            // pointer-down first runs click-to-focus (`process_with_focus` in
            // `enqueue_pointer_event`), which moves keyboard focus onto that
            // frame tile before this handler runs. Gating the gesture on the
            // *focused* tile being scrollable therefore made whole-portal
            // resize inert under real OS pointer input: the focus the gate
            // required had already been pulled off the scrollable pane by the
            // initiating click (hud-yno2r). Mirror the whole-portal move path
            // (`translate_portal_group_on_drag`), which resolves the group from
            // the tile under the pointer and gates portal-ness across the whole
            // group rather than the focused tile. Fall back to the focused tile
            // only when the pointer is not over any tile (e.g. injected events
            // in tests that pre-seed focus without a hittable frame).
            let seed_tile_id = match scene.hit_test(x, y) {
                HitResult::NodeHit { tile_id, .. } | HitResult::TileHit { tile_id } => {
                    Some(tile_id)
                }
                _ => active_tab.and_then(|tab| focus_manager.current_owner(tab).tile_id()),
            }?;
            // Resolve the whole portal; the affordance strip lives on the frame.
            let group = resolve_portal_group(scene, seed_tile_id)?;
            // Portal-ness gate: the resolved group must contain at least one
            // scrollable constituent surface (same check as the move path).
            // This keeps a plain widget / zone from acting as a resize target
            // without requiring the *focused* tile to be the scrollable one.
            if !group
                .member_ids
                .iter()
                .any(|id| scene.tile_scroll_config(*id).is_some())
            {
                return None;
            }
            let old_rect = group.portal_rect;
            let edge: ResizeEdge = hit_affordance(x, y, &old_rect, tokens.affordance_px)?;

            // Spatial budget from the anchor/frame's lease (the whole-portal
            // rect clamps against the portal's lease budget).
            let anchor_lease = scene.tiles.get(&group.anchor_tile_id)?.lease_id;
            let resize_bounds = resize_bounds_for_lease(scene, anchor_lease);

            let resize_state = portal_resize_states
                .entry(group.anchor_tile_id)
                .or_insert_with(|| PortalResizeState::new(group.portal_id_hash));

            let outcome =
                resize_state.on_pointer_down(device_id, edge, x, y, old_rect, &resize_bounds);
            let snapshot = match outcome {
                ResizeOutcome::GestureStarted { snapshot } => snapshot,
                _ => return None,
            };
            let gesture_epoch = resize_state.current_gesture_epoch();

            // Apply initial (clamped) rect to the whole portal — local-first.
            let members = commit_portal_group_resize(scene, &group, old_rect, snapshot);

            tracing::debug!(
                anchor_tile_id = ?group.anchor_tile_id,
                members = members.len(),
                ?edge,
                x,
                y,
                gesture_epoch,
                "portal resize: pointer-down on affordance — whole-portal gesture started"
            );

            Some(PortalResizePointerOutcome {
                members,
                display_w,
                display_h,
                persist: false,
            })
        }

        PointerEventKind::Move => {
            let mut active_gesture = None;
            for (&anchor_id, resize_state) in portal_resize_states.iter_mut() {
                if !resize_state.gesture_active() {
                    continue;
                }
                let Some(anchor_lease) = scene.tiles.get(&anchor_id).map(|t| t.lease_id) else {
                    continue;
                };
                let resize_bounds = resize_bounds_for_lease(scene, anchor_lease);
                if let ResizeOutcome::GestureUpdate { snapshot } =
                    resize_state.on_pointer_move(device_id, x, y, &resize_bounds)
                {
                    active_gesture = Some((anchor_id, snapshot));
                    break;
                }
            }

            let (anchor_id, snapshot) = active_gesture?;
            let group = resolve_portal_group(scene, anchor_id)?;
            let old_rect = group.portal_rect;

            // Apply the updated whole-portal rect immediately (local-first).
            let members = commit_portal_group_resize(scene, &group, old_rect, snapshot);

            tracing::trace!(
                anchor_tile_id = ?group.anchor_tile_id,
                members = members.len(),
                x,
                y,
                new_w = snapshot.rect.width,
                new_h = snapshot.rect.height,
                "portal resize: pointer-move — whole-portal bounds updated locally"
            );

            Some(PortalResizePointerOutcome {
                members,
                display_w,
                display_h,
                persist: false,
            })
        }

        PointerEventKind::Up => {
            let mut active_gesture = None;
            for (&anchor_id, resize_state) in portal_resize_states.iter_mut() {
                if !resize_state.gesture_active() {
                    continue;
                }
                let Some(anchor_lease) = scene.tiles.get(&anchor_id).map(|t| t.lease_id) else {
                    continue;
                };
                let resize_bounds = resize_bounds_for_lease(scene, anchor_lease);
                if let ResizeOutcome::GestureEnded { snapshot } =
                    resize_state.on_pointer_up(device_id, x, y, &resize_bounds)
                {
                    active_gesture =
                        Some((anchor_id, snapshot, resize_state.current_gesture_epoch()));
                    break;
                }
            }

            let (anchor_id, snapshot, gesture_epoch) = active_gesture?;
            let group = resolve_portal_group(scene, anchor_id)?;
            let old_rect = group.portal_rect;

            // Apply final clamped whole-portal rect (local-first).
            let members = commit_portal_group_resize(scene, &group, old_rect, snapshot);

            tracing::debug!(
                anchor_tile_id = ?group.anchor_tile_id,
                members = members.len(),
                x,
                y,
                final_w = snapshot.rect.width,
                final_h = snapshot.rect.height,
                gesture_epoch,
                "portal resize: pointer-up — whole-portal gesture ended, final bounds applied"
            );

            Some(PortalResizePointerOutcome {
                members,
                display_w,
                display_h,
                persist: true,
            })
        }
    }
}

impl WinitApp {
    pub(super) fn route_and_deliver_composer_batch(
        &mut self,
        context: ComposerDeliveryContext,
        batch: tze_hud_input::DraftNotificationBatch,
    ) {
        let authority_handled = self.route_portal_composer_batch(context.tile_id, &batch);

        // Runtime-authored viewer reply echo (hud-nx7yq.3): on the raw-tile pilot
        // path (a portal NOT attached to the projection authority, so
        // `route_portal_composer_batch` returned false and no `append_viewer_echo`
        // fired), the submitted text would otherwise vanish on Enter. Author a
        // kind-distinct viewer entry at submit time so the reply bubbles into the
        // transcript. Authority-attached portals already echo via
        // `submit_portal_input`, so we skip them to avoid a double.
        if !authority_handled {
            if let Some(submission) = batch.submission.as_ref() {
                if !submission.text.trim().is_empty() {
                    self.append_raw_tile_viewer_echo(context.tile_id, submission.text.clone());
                }
            }
        }

        deliver_composer_batch(
            &self.state.input_event_tx,
            context.namespace,
            &context.node_id_bytes,
            context.tile_id.as_uuid().as_bytes(),
            batch,
        );
    }

    /// Push a runtime-authored viewer echo entry onto the shared queue the
    /// compositor drains (hud-nx7yq.3).
    ///
    /// Pure local presentation: it never touches unread counts or attention
    /// state, and it is authored by the runtime — an adapter cannot forge a
    /// viewer entry through this path (the output-publication contract's viewer
    /// rejection is unchanged).
    ///
    /// Also makes wheel-scroll through the input-pane history LIVE (hud-qbcp8).
    /// The rendering foundation (hud-acfvp/#1044) reads the tile's displayed
    /// scroll offset once a `TileScrollConfig` is registered, but nothing
    /// registered one or fed it content growth, so scrolling was inert.
    /// Register the tile as scrollable and advance its follow-tail content
    /// height the same way the authority-attached OUTPUT/transcript pane does
    /// (`InputProcessor::notify_tile_content_appended`, wired from
    /// `portal_projection_driver.rs`) — the runtime plays the "adapter" role
    /// here because raw-tile viewer echoes are runtime-authored, not
    /// adapter-published. A fresh `ScrollTileState` starts at content height 0
    /// with `FollowTailAnchor::AtTail`, so the very first call below seeds the
    /// offset directly to the tail — no separate seeding step needed. The
    /// explicit `reset_tile_scroll_to_tail` afterward then forces back to the
    /// tail even if the viewer had scrolled up, matching "submitting your own
    /// reply reveals it" (don't strand the viewer scrolled-up).
    fn append_raw_tile_viewer_echo(&mut self, tile_id: tze_hud_scene::SceneId, text: String) {
        let submitted_at_wall_us = self.state.portal_projection_driver.now_wall_us();

        if let Ok(state) = self.state.shared_state.try_lock()
            && let Ok(mut scene) = state.scene.try_lock()
        {
            let _ = scene.register_tile_scroll_config(tile_id, TileScrollConfig::vertical());
            let tile_bounds = scene
                .tiles
                .get(&tile_id)
                .map(|t| (t.bounds.width, t.bounds.height));
            if let Some((tile_width_px, tile_height_px)) = tile_bounds {
                let metrics = InputHistorySeedMetrics::for_tile_font_scale(
                    scene.tile_font_scale(tile_id),
                    &self.state.global_tokens,
                );
                // Seed the scroll clamp against the history BAND (region top →
                // composer-box top), NOT the whole tile, and against a
                // conservative wrapped-visual-row OVER-estimate of the content
                // height (hud-3y7va). The compositor's `input_history_block_top`
                // clamps the displayed offset against
                // `real_max_scrollback = wrap_accurate_block_height − band_height`
                // and only bottom-aligns the tail when the offset reaches that
                // bound. Seeding against the full tile height plus a `\n`-only
                // line count under-estimated the offset on both counts, so the
                // newest just-submitted lines clipped at rest — worse than the
                // pre-scroll tail pin. Under-estimating the band and over-counting
                // wrapped rows guarantees the seeded offset reaches or exceeds
                // `real_max_scrollback`, so the compositor bottom-aligns exactly
                // to the tail. Over-estimating is safe: the compositor's exact
                // per-frame clamp is the visual authority, and it re-clamps the
                // displayed position every frame regardless of this value's
                // precision.
                let band_height_px =
                    input_history_band_height_px(tile_height_px, metrics.line_height_px);
                let wrap_width_px = input_history_wrap_width_px(tile_width_px);
                // The compositor renders each entry as `viewer_echo_display_text`
                // — the `HH:MM␠␠` timestamp prefix (present whenever
                // `submitted_at_wall_us != 0`, which this path always sets)
                // followed by the text — and wraps THAT. Seed the row estimate
                // against the same prefixed text so a full-width entry that nearly
                // fills a row is not under-counted by the prefix spilling it onto
                // an extra wrapped row (hud-kaw7z).
                let seed_text = if submitted_at_wall_us != 0 {
                    format!("{INPUT_HISTORY_TIMESTAMP_PREFIX_STANDIN}{text}")
                } else {
                    text.clone()
                };
                let added_rows = approx_wrapped_visual_rows(
                    &seed_text,
                    wrap_width_px,
                    metrics.narrow_char_advance_px,
                    metrics.wide_char_advance_px,
                );
                let added_height_px = added_rows as f32 * metrics.line_height_px;
                let prior_seed_state = self.state.input_history_seed_states.get(&tile_id).copied();
                let previous_total_height_px = self
                    .state
                    .input_processor
                    .tile_total_content_height_px(tile_id);
                // Existing raw-tile history was seeded in physical line units.
                // We cannot rewrap retained raw entries here, so use their last
                // conservative seed geometry to bound both a line-pitch change
                // and a narrower-width reflow before adding the new entry.
                let rebased_total_height_px = prior_seed_state
                    .map(|prior| {
                        previous_total_height_px
                            * prior.conservative_rebase_multiplier(metrics, wrap_width_px)
                    })
                    .unwrap_or(previous_total_height_px);
                let new_total_height_px = rebased_total_height_px + added_height_px;
                self.state.input_processor.notify_tile_content_appended(
                    tile_id,
                    new_total_height_px,
                    band_height_px,
                    metrics.line_height_px,
                    &mut scene,
                );
                self.state
                    .input_history_seed_states
                    .insert(tile_id, InputHistorySeedState::new(metrics, wrap_width_px));
                self.state
                    .input_processor
                    .reset_tile_scroll_to_tail(tile_id, &mut scene);
            }
        }

        if let Ok(mut queue) = self.state.viewer_echo_queue.lock() {
            queue.push(tze_hud_compositor::ViewerEchoAppend {
                tile_id,
                text,
                submitted_at_wall_us,
            });
        }
    }

    /// Route submitted focused-portal composer text into the in-process
    /// projection authority before the legacy namespace broadcast is emitted.
    ///
    /// Returns `true` when the tile is owned by an attached in-process projection
    /// (the authority consumed the batch and echoed on its own path), `false`
    /// for a raw-tile pilot portal — the caller uses this to decide whether a
    /// runtime-authored viewer echo is needed (hud-nx7yq.3).
    fn route_portal_composer_batch(
        &mut self,
        tile_id: tze_hud_scene::SceneId,
        batch: &tze_hud_input::DraftNotificationBatch,
    ) -> bool {
        let submitted_at_wall_us = self.state.portal_projection_driver.now_wall_us();

        if let Some(feedback) = self
            .state
            .portal_projection_driver
            .submit_composer_batch_for_tile(
                tile_id,
                batch,
                submitted_at_wall_us.max(1),
                None,
                tze_hud_projection::ContentClassification::Private,
            )
        {
            tracing::debug!(
                tile_id = ?tile_id,
                pending_input_count = feedback.pending_input_count,
                feedback_state = ?feedback.feedback_state,
                "composer: routed portal submission to projection authority"
            );
            true
        } else {
            false
        }
    }

    /// Persist the geometry override for a completed drag and broadcast an
    /// `ElementRepositionedEvent`.
    ///
    /// Called after all scene locks are released to avoid holding locks during
    /// disk I/O (element store atomic write + fsync).
    pub(super) fn persist_drag_release(&mut self, released: DragReleasedData) {
        use tze_hud_input::InputProcessor;
        use tze_hud_scene::element_store::ElementType;

        let (store_snapshot, persist_path, new_geometry) = {
            let Ok(mut state) = self.state.shared_state.try_lock() else {
                tracing::warn!("persist_drag_release: could not acquire shared_state lock");
                return;
            };

            let new_geometry = tze_hud_input::drag::final_position_to_geometry(
                released.final_x,
                released.final_y,
                released.width,
                released.height,
                released.display_width,
                released.display_height,
            );

            if released.group_members.is_empty() {
                // Plain single-tile move: match the store entry by namespace and
                // write its geometry override (the pre-existing path).
                InputProcessor::persist_drag_geometry(
                    &mut state.element_store,
                    ElementType::Tile,
                    &released.namespace,
                    released.final_x,
                    released.final_y,
                    released.width,
                    released.height,
                    released.display_width,
                    released.display_height,
                );
            } else {
                // Whole-portal move (hud-8vejp): a portal's constituent tiles
                // share one namespace, so a namespace match would give only one
                // arbitrary member a durable override. Write a Relative override
                // for EVERY member keyed by its own SceneId so `list_elements`
                // reports `has_user_override=true` for all of them and an adapter
                // republish can never pull a member back — in-session or after a
                // restart (the override is authoritative at the publish ingress).
                write_member_geometry_overrides(
                    &mut state.element_store,
                    &released.group_members,
                    released.display_width,
                    released.display_height,
                );
            }

            let store_snapshot = state.element_store.clone();
            let persist_path = state.element_store_path.clone();
            (store_snapshot, persist_path, new_geometry)
        };

        // Persist element store on a background thread (avoids blocking the
        // winit event loop with sync disk I/O).
        if let Some(path) = persist_path {
            std::thread::spawn(move || {
                if let Err(e) =
                    crate::element_store::persist_element_store_to_path(&store_snapshot, &path)
                {
                    tracing::warn!(
                        error = %e,
                        "persist_drag_release: element store persist failed"
                    );
                }
            });
        }

        // Broadcast ElementRepositionedEvent so gRPC subscribers are notified.
        if let Some(ref tx) = self.state.element_repositioned_tx {
            let event = tze_hud_protocol::proto::ElementRepositionedEvent {
                element_id: released.element_id.as_uuid().as_bytes().to_vec(),
                new_geometry: Some(tze_hud_protocol::convert::geometry_policy_to_proto(
                    &new_geometry,
                )),
                previous_geometry: None,
            };
            // Errors (no receivers, channel lagged) are silently ignored —
            // ElementRepositionedEvent is an ephemeral-realtime notification;
            // subscribers that are not present at the moment of a drag release
            // will learn the final geometry on next subscription refresh.
            tx.send(event).unwrap_or_default();
            tracing::debug!(
                element_id = %released.element_id,
                final_x = released.final_x,
                final_y = released.final_y,
                "ElementRepositionedEvent broadcast after drag release"
            );
        }
    }

    /// Persist a durable, id-keyed geometry override for each `(SceneId, rect)`
    /// pair and flush the element store to disk on a background thread.
    ///
    /// Used by the whole-portal resize path (hud-8vejp) to give every group
    /// member its own durable override after a viewer resize — the drag path
    /// persists inline in [`Self::persist_drag_release`]. `rect` is absolute
    /// display-pixel bounds; it is normalised to a `Relative` policy against the
    /// supplied display size before being stored, matching how drag-release and
    /// adapter republishes resolve geometry.
    ///
    /// A no-op (no lock, no disk write) when `members` is empty. Lock contention
    /// on `shared_state` is logged and skipped: the transient in-session
    /// `viewer_geometry_locked` set already keeps the group coherent this
    /// session, and the next viewer gesture will retry the durable write.
    pub(super) fn persist_portal_member_overrides(
        &mut self,
        members: &[(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)],
        display_w: f32,
        display_h: f32,
    ) {
        if members.is_empty() {
            return;
        }

        let (store_snapshot, persist_path) = {
            let Ok(mut state) = self.state.shared_state.try_lock() else {
                tracing::warn!(
                    "persist_portal_member_overrides: could not acquire shared_state lock"
                );
                return;
            };
            write_member_geometry_overrides(
                &mut state.element_store,
                members,
                display_w,
                display_h,
            );
            (
                state.element_store.clone(),
                state.element_store_path.clone(),
            )
        };

        if let Some(path) = persist_path {
            std::thread::spawn(move || {
                if let Err(e) =
                    crate::element_store::persist_element_store_to_path(&store_snapshot, &path)
                {
                    tracing::warn!(
                        error = %e,
                        "persist_portal_member_overrides: element store persist failed"
                    );
                }
            });
        }
    }

    /// Run the in-process portal projection drain loop (hud-2iup7).
    ///
    /// Drain pending [`PortalOp`] messages from the MCP channel (hud-bq0gl.2).
    ///
    /// Called from `about_to_wait` BEFORE `drain_portal_projection` so that
    /// content published in the same event-loop tick is fed into the cadence
    /// coalescer and materialised by the immediately-following drain call.
    ///
    /// Uses `try_recv` in a non-blocking loop — never blocks the event-loop
    /// thread.  Each dispatched op calls `InProcessPortalDriver::dispatch_portal_op`
    /// which synchronously feeds the operation into `ProjectionAuthority`.
    pub(super) fn drain_portal_ops(&mut self) {
        let Some(ref mut rx) = self.state.portal_op_rx else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(op) => {
                    self.state.portal_projection_driver.dispatch_portal_op(op);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    tracing::warn!(
                        "portal_op channel disconnected — MCP portal tools will no longer function"
                    );
                    // The ingress that feeds portal ops went away without per-
                    // projection clean Detach ops — an ungraceful upstream drop.
                    // Latch every still-attached projection to the degraded
                    // treatment so the surfaces stop looking live (hud-5i16d).
                    self.state
                        .portal_projection_driver
                        .mark_all_projections_disconnected();
                    self.state.portal_op_rx = None;
                    break;
                }
            }
        }
    }

    /// Called from `about_to_wait` after composer-draft flush.  Drives
    /// `InProcessPortalDriver::drain` which calls
    /// `InputProcessor::notify_tile_content_appended` for every `RenderPortal`
    /// drain record that carries append geometry (spec §3.2 / §3.3).
    ///
    /// Uses `try_lock` on the shared scene to avoid blocking the main thread.
    /// If the scene lock is busy, returns [`PortalProjectionDrain::Deferred`]
    /// so `about_to_wait` can arrange one release-driven availability wake.
    pub(super) fn drain_portal_projection(&mut self) -> PortalProjectionDrain {
        let Ok(state) = self.state.shared_state.try_lock() else {
            tracing::trace!("portal drain deferred: shared_state lock busy");
            return PortalProjectionDrain::Deferred;
        };
        let Ok(mut scene) = state.scene.try_lock() else {
            tracing::trace!("portal drain deferred: scene lock busy");
            return PortalProjectionDrain::Deferred;
        };
        let tab_id = scene.active_tab;
        let scene_version_before = scene.version;
        let geometry_epoch_before = scene.geometry_epoch;
        self.state.portal_projection_driver.drain(
            &mut scene,
            &mut self.state.input_processor,
            tab_id,
        );
        let scene_changed =
            scene.version != scene_version_before || scene.geometry_epoch != geometry_epoch_before;
        // The driver activates a tab when a cooperative portal needs to render
        // and none was active (hud-obw3q). Keep the lock-free keyboard-dispatch
        // mirror in sync so keyboard routing targets the newly active tab.
        if scene.active_tab != tab_id {
            state.refresh_active_tab_mirror(&scene);
        }
        PortalProjectionDrain::Completed { scene_changed }
    }

    /// Remove stale per-tile resize and raw-history state for tiles that no
    /// longer exist in the scene.
    ///
    /// Called once per `about_to_wait` iteration.  Uses a two-phase approach:
    ///
    /// 1. **Eager drain** — drains `SceneGraph::recently_removed_tile_ids`
    ///    (populated by `remove_tile_and_nodes` on every `DeleteTile` mutation)
    ///    and removes each returned ID from both maps immediately. This is
    ///    O(removed) and requires only the scene lock.
    ///
    /// 2. **Fallback sweep** — if `portal_resize_states` is still non-empty
    ///    after the drain (e.g. entries that predated the drain queue, or
    ///    entries whose tiles were removed while the lock was busy), a full
    ///    `retain` sweep validates all remaining entries against the live tile
    ///    set.  Uses `try_lock`; deferred to next iteration if lock is busy.
    ///
    /// The eager path handles the common case (each `DeleteTile` generates
    /// exactly one removal notification).  The fallback prevents unbounded
    /// accumulation in pathological cases.
    pub(super) fn prune_portal_resize_states(&mut self) {
        let Ok(state) = self.state.shared_state.try_lock() else {
            tracing::trace!("portal resize prune deferred: shared_state lock busy");
            return;
        };
        let Ok(mut scene) = state.scene.try_lock() else {
            tracing::trace!("portal resize prune deferred: scene lock busy");
            return;
        };

        // Phase 1: eager drain — O(removed), handles the common `DeleteTile` path.
        let removed_ids = scene.drain_removed_tile_ids();
        let mut eagerly_removed = 0usize;
        for tile_id in removed_ids {
            let resize_removed = self.state.portal_resize_states.remove(&tile_id).is_some();
            let history_removed = self
                .state
                .input_history_seed_states
                .remove(&tile_id)
                .is_some();
            if resize_removed || history_removed {
                eagerly_removed += 1;
            }
        }
        if eagerly_removed > 0 {
            tracing::debug!(
                removed = eagerly_removed,
                resize_remaining = self.state.portal_resize_states.len(),
                history_remaining = self.state.input_history_seed_states.len(),
                "portal resize: eagerly pruned stale per-tile state for removed tiles"
            );
        }

        // Phase 2: fallback sweep — catches any entries that slipped through
        // (e.g. tiles removed before the drain queue existed, or during a
        // prior lock-busy deferral).
        if self.state.portal_resize_states.is_empty()
            && self.state.input_history_seed_states.is_empty()
        {
            return;
        }
        let resize_before = self.state.portal_resize_states.len();
        let history_before = self.state.input_history_seed_states.len();
        self.state
            .portal_resize_states
            .retain(|tile_id, _| scene.tiles.contains_key(tile_id));
        self.state
            .input_history_seed_states
            .retain(|tile_id, _| scene.tiles.contains_key(tile_id));
        let swept = (resize_before - self.state.portal_resize_states.len())
            + (history_before - self.state.input_history_seed_states.len());
        if swept > 0 {
            tracing::debug!(
                removed = swept,
                resize_remaining = self.state.portal_resize_states.len(),
                history_remaining = self.state.input_history_seed_states.len(),
                "portal resize: sweep-pruned stale per-tile state for removed tiles"
            );
        }
    }
    /// Apply a Ctrl-gated portal resize hotkey to the focused portal tile.
    ///
    /// Looks up the currently focused tile in `tab_id`. If the focused tile is
    /// a portal tile (has a registered scroll config), applies the resize step
    /// locally (local-first per §6b.2), updates the scene tile bounds, and
    /// broadcasts an `ElementRepositionedEvent` on the `SCENE_TOPOLOGY` channel
    /// via `element_repositioned_tx` so gRPC subscribers receive the updated
    /// portal geometry (relative %).
    ///
    /// Returns `true` when the hotkey was consumed (applied to a focused portal
    /// tile) so the caller knows to stop propagating the key event.
    /// Returns `false` when no focused portal tile was found (the key must not
    /// be consumed and should fall through to the normal dispatch path).
    ///
    /// `axis` selects which axis/axes the step grows/shrinks:
    /// [`HotkeyResizeAxis::Both`](tze_hud_input::HotkeyResizeAxis::Both) for the
    /// symmetric Ctrl+`+`/`-` chord, or
    /// [`Width`](tze_hud_input::HotkeyResizeAxis::Width) /
    /// [`Height`](tze_hud_input::HotkeyResizeAxis::Height) for the directional
    /// Ctrl+Shift+Arrow chord (hud-csrmf). A width step reflows the transcript
    /// text (it changes the wrap column) and takes the viewer-geometry-lock, so
    /// it drives the dynamic hud-rpmwt reconcile-on-republish path without pointer
    /// injection.
    pub(super) fn apply_portal_resize_hotkey(
        &mut self,
        tab_id: tze_hud_scene::SceneId,
        dir: tze_hud_input::HotkeyResizeDir,
        axis: tze_hud_input::HotkeyResizeAxis,
    ) -> bool {
        // Resolve the focused tile from the focus manager.
        let focused_tile_id = match self.state.focus_manager.current_owner(tab_id).tile_id() {
            Some(id) => id,
            None => return false,
        };

        let display_w = self.state.config.window.width as f32;
        let display_h = self.state.config.window.height as f32;

        // Acquire scene + check if the focused tile is a portal surface (has a
        // scroll config), resolve the whole-portal group, and build the bounds
        // for clamping the whole-portal rect.
        let (group, old_rect, bounds) = {
            // A resize hotkey is a deliberate user action that must produce
            // visible feedback this frame; acquire with a bounded spin rather
            // than a single try_lock so a contended scene lock (compositor /
            // streaming publish) cannot silently swallow the resize.
            let Some(state) = spin_acquire(&self.state.shared_state, INTERACTION_LOCK_BUDGET)
            else {
                return false;
            };
            let Some(scene) = spin_acquire(&state.scene, INTERACTION_LOCK_BUDGET) else {
                return false;
            };
            // Only scrollable portal tiles accept resize hotkeys.
            if scene.tile_scroll_config(focused_tile_id).is_none() {
                return false;
            }
            // Resolve the whole portal (frame anchor + constituent surfaces)
            // that owns the focused surface, so the step scales the unit rather
            // than the single focused tile (hud-fb3en).
            let Some(group) = resolve_portal_group(&scene, focused_tile_id) else {
                return false;
            };
            let Some(anchor_lease) = scene.tiles.get(&group.anchor_tile_id).map(|t| t.lease_id)
            else {
                return false;
            };
            let old_rect = group.portal_rect;
            let portal_part = tze_hud_config::resolve_portal_tokens(&self.state.global_tokens);
            let tokens = PortalWindowTokens {
                min_width_px: portal_part.window_min_width_px,
                min_height_px: portal_part.window_min_height_px,
                resize_step_px: portal_part.window_resize_step_px,
                affordance_px: portal_part.window_resize_affordance_px,
            };
            // Resolve spatial budget from the anchor/frame's lease (the
            // whole-portal rect clamps against the portal's lease budget).
            let (lease_max_w, lease_max_h) = scene
                .leases
                .get(&anchor_lease)
                .map(|l| {
                    (
                        l.spatial_budget.max_tile_width_px,
                        l.spatial_budget.max_tile_height_px,
                    )
                })
                .unwrap_or((0.0, 0.0));
            let (max_width_px, max_height_px) = compute_portal_max_dims(
                lease_max_w,
                lease_max_h,
                display_w,
                display_h,
                tokens.min_width_px,
                tokens.min_height_px,
            );
            let resize_bounds = ResizeBounds {
                tokens,
                max_width_px,
                max_height_px,
                display_w,
                display_h,
            };
            (group, old_rect, resize_bounds)
        };

        // Get or lazily create the per-portal resize state, keyed by the anchor
        // (frame) tile so one gesture state tracks the whole portal.
        let resize_state = self
            .state
            .portal_resize_states
            .entry(group.anchor_tile_id)
            .or_insert_with(|| PortalResizeState::new(group.portal_id_hash));

        // Apply the hotkey resize to the whole-portal rect (O(1) on the hot path).
        let outcome = tze_hud_input::apply_hotkey_resize_axis(
            true, // portal is focused (checked above)
            dir,
            axis,
            old_rect,
            &bounds,
            resize_state,
        );

        let snapshot = match outcome {
            tze_hud_input::HotkeyResizeOutcome::Applied { snapshot } => snapshot,
            tze_hud_input::HotkeyResizeOutcome::NotFocused => return false,
        };

        // Local-first feedback: scale every constituent surface immediately in
        // the scene (same frame, no adapter roundtrip) per §6b.2 /
        // local-feedback-first, preserving relative layout around the top-left
        // anchor. `commit_portal_group_resize` bumps `scene.version` once when
        // the geometry actually changes, so the compositor re-primes the
        // truncation cache at the new geometry (hud-ghhxa — spec §6b.3) without
        // churning at a clamped boundary.
        let (members, portal_ids) = {
            let Some(state) = spin_acquire(&self.state.shared_state, INTERACTION_LOCK_BUDGET)
            else {
                return true; // hotkey consumed even if local update fails
            };
            let Some(mut scene) = spin_acquire(&state.scene, INTERACTION_LOCK_BUDGET) else {
                return true;
            };
            let members = commit_portal_group_resize(&mut scene, &group, old_rect, snapshot);
            // Resolve each member's declared portal-surface identity while the
            // scene lock is already held (hud-s62vv): a bridged (first-class-
            // surface) member has no in-process tile, so
            // `push_geometry_snapshot_for_tile`'s plain tile-id reverse lookup
            // alone cannot find its projection below. Only the (small) identity
            // string is extracted — never the whole `SceneGraph`.
            let portal_ids: std::collections::HashMap<tze_hud_scene::SceneId, String> = members
                .iter()
                .filter_map(|member| {
                    scene
                        .portal_surface(member.tile_id)
                        .map(|s| (member.tile_id, s.identity.session_id.clone()))
                })
                .collect();
            (members, portal_ids)
        };

        // Broadcast a geometry snapshot per constituent surface to gRPC
        // subscribers via ElementRepositionedEvent (§6b.4: coalescible
        // state-stream delivery), and mirror each into the in-process projection
        // authority so the drain loop sees the updated bounds next cycle.
        let mut member_bounds: Vec<(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)> =
            Vec::with_capacity(members.len());
        for member in &members {
            dispatch_portal_geometry_event(
                &self.state.element_repositioned_tx,
                member.tile_id,
                &member.snapshot,
                display_w,
                display_h,
            );
            self.state
                .portal_projection_driver
                .push_geometry_snapshot_for_tile(
                    member.tile_id,
                    member.snapshot,
                    portal_ids.get(&member.tile_id).map(String::as_str),
                );
            let r = member.snapshot.rect;
            member_bounds.push((
                member.tile_id,
                tze_hud_scene::types::Rect::new(r.x, r.y, r.width, r.height),
            ));
        }

        // Durably record every member's post-resize geometry as an id-keyed
        // override (hud-8vejp). Unlike the pointer path there is no terminal
        // PointerUp — each hotkey press is a discrete committed resize step — so
        // persist on every applied step; the write only fires when the geometry
        // actually changed (a clamped-at-boundary press applies nothing new).
        self.persist_portal_member_overrides(&member_bounds, display_w, display_h);

        tracing::debug!(
            anchor_tile_id = ?group.anchor_tile_id,
            members = members.len(),
            new_width = snapshot.rect.width,
            new_height = snapshot.rect.height,
            sequence = snapshot.sequence,
            "portal resize: hotkey applied — whole-portal bounds updated locally"
        );

        true // hotkey consumed
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};

    use tze_hud_input::{
        FocusManager, FocusRequest, InputProcessor, KeyboardProcessor, PointerEvent,
        PointerEventKind, PortalResizeState, PortalWindowTokens, RawKeyDownEvent, RawKeyUpEvent,
    };
    use tze_hud_telemetry::TelemetryCollector;

    use super::super::lifecycle::pointer_down_starts_guaranteed_feedback_gesture;
    use super::super::test_support::{
        make_shared_state, portal_scene_with_focus, scene_with_capture_tile,
        scene_with_drag_handle_tile,
    };
    use super::super::{WindowedConfig, WindowedRuntimeState, WinitApp};
    use super::*;
    use crate::channels::{INPUT_EVENT_CAPACITY, frame_ready_channel};
    use crate::pipeline::FramePipeline;
    use crate::runtime_context::RuntimeContext;
    use crate::threads::ShutdownToken;
    use crate::window::WindowMode;

    fn make_windowed_keyboard_test_app(
        scene: tze_hud_scene::graph::SceneGraph,
        focus_manager: FocusManager,
        input_processor: InputProcessor,
    ) -> (
        WinitApp,
        tze_hud_protocol::session_server::InputEventReceiver,
    ) {
        let cfg = WindowedConfig::default();
        let shared_state = make_shared_state();
        let (input_capture_tx, input_capture_rx) = tokio::sync::mpsc::unbounded_channel();
        let (frame_ready_tx, frame_ready_rx) = frame_ready_channel();
        let input_event_tx = tze_hud_protocol::session_server::InputEventSender::new(8);
        let input_event_rx = input_event_tx.subscribe_all();
        let safe_mode_atomic = {
            let state = shared_state
                .try_lock()
                .expect("shared state must be uncontended in test setup");
            let mut scene_guard = state
                .scene
                .try_lock()
                .expect("scene must be uncontended in test setup");
            *scene_guard = scene;
            state.refresh_active_tab_mirror(&scene_guard);
            Arc::clone(&state.safe_mode_atomic)
        };
        let active_tab_mirror = {
            let state = shared_state
                .try_lock()
                .expect("shared state must be uncontended after scene setup");
            Arc::clone(&state.active_tab_mirror)
        };

        let state = WindowedRuntimeState {
            wake: crate::windowed::wake::WindowedWake::disconnected(),
            scheduled_main_deadline: None,
            cursor_poll: crate::windowed::wake::CursorPollState::default(),
            config: cfg,
            compositor_handle: None,
            network_rt: None,
            network_handles: Vec::new(),
            runtime_context: Arc::new(RuntimeContext::headless_default()),
            _runtime_widget_store: None,
            shared_state,
            safe_mode_atomic,
            active_tab_mirror,
            chrome_state: Arc::new(std::sync::RwLock::new(crate::shell::ChromeState::new())),
            system_card: crate::shell::system_card::SystemCardHandle::default(),
            input_ring: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(INPUT_EVENT_CAPACITY),
            )),
            pending_input_latency: Arc::new(StdMutex::new(VecDeque::new())),
            frame_ready_rx,
            frame_ready_tx: Some(frame_ready_tx),
            frame_presented_tx: None,
            degradation_notices: None,
            lease_expirations: None,
            compositor: None,
            window_surface: None,
            input_processor,
            input_capture_rx,
            pending_input_capture_commands: std::collections::VecDeque::new(),
            focus_manager,
            keyboard_processor: KeyboardProcessor::new(),
            telemetry: TelemetryCollector::new(),
            pipeline: FramePipeline::new(),
            shutdown: ShutdownToken::new(),
            benchmark_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            terminal_surface_recovery_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            quiescent_efficiency: None,
            cursor_x: 0.0,
            cursor_y: 0.0,
            left_button_down: false,
            cursor_tracker: tze_hud_input::CursorIconTracker::new(),
            window: None,
            effective_mode: WindowMode::Fullscreen,
            hit_regions: Vec::new(),
            overlay_capturing: false,
            static_hit_regions: Vec::new(),
            widget_hover_trackers: std::collections::HashMap::new(),
            pending_mode_switch: None,
            pending_widget_svgs: Vec::new(),
            modifiers: winit::keyboard::ModifiersState::empty(),
            current_monitor_index: 0,
            global_tokens: std::collections::HashMap::new(),
            element_repositioned_tx: None,
            input_event_tx: Some(input_event_tx),
            pending_blur_delivery_context: None,
            composer_pointer_drag_anchor: None,
            portal_resize_states: std::collections::HashMap::new(),
            consumed_portal_resize_keydowns: std::collections::HashSet::new(),
            consumed_shell_shortcut_keydowns: std::collections::HashSet::new(),
            keyboard_activation_nodes: std::collections::HashMap::new(),
            consumed_command_keydowns: std::collections::HashSet::new(),
            local_composer_state: Arc::new(StdMutex::new(None)),
            viewer_echo_queue: Arc::new(StdMutex::new(Vec::new())),
            input_history_seed_states: std::collections::HashMap::new(),
            focus_ring_owner_state: Arc::new(StdMutex::new(None)),
            resize_grip_hover_state: Arc::new(StdMutex::new(None)),
            tile_close_hover_state: Arc::new(StdMutex::new(None)),
            cursor_left_window: false,
            composer_visual_layout: Arc::new(StdMutex::new(None)),
            portal_projection_driver: crate::portal_projection_driver::InProcessPortalDriver::new(),
            portal_op_rx: None,
            capture_inbox: crate::operator::screenshot::CaptureInbox::detached(),
            pending_keyboard_events: VecDeque::new(),
            interaction_feedback_lock_misses: std::sync::atomic::AtomicU64::new(0),
        };

        drop(input_capture_tx);

        (WinitApp { state }, input_event_rx)
    }

    fn make_windowed_app_with_pending_portal_publish(projection_id: &str) -> WinitApp {
        use tze_hud_projection::{
            AttachRequest, ContentClassification, OperationEnvelope, OutputKind,
            ProjectionOperation, ProviderKind, PublishOutputRequest,
        };

        let mut driver = crate::portal_projection_driver::InProcessPortalDriver::new();
        let envelope = |operation, request_id: &str| OperationEnvelope {
            operation,
            projection_id: projection_id.to_string(),
            request_id: request_id.to_string(),
            client_timestamp_wall_us: 1,
        };
        let attach = driver.authority_mut().handle_attach(
            AttachRequest {
                envelope: envelope(ProjectionOperation::Attach, "attach-wake"),
                provider_kind: ProviderKind::Claude,
                display_name: "Wake test".to_string(),
                workspace_hint: None,
                repository_hint: None,
                icon_profile_hint: None,
                content_classification: ContentClassification::Private,
                hud_target: None,
                idempotency_key: None,
            },
            "wake-test-caller",
            1_000,
        );
        assert!(attach.accepted);
        let owner_token = attach.owner_token.expect("owner token after attach");
        driver.attach_projection(projection_id, Vec::new());
        let published = driver.authority_mut().handle_publish_output(
            PublishOutputRequest {
                envelope: envelope(ProjectionOperation::PublishOutput, "publish-wake"),
                owner_token,
                output_text: "actual portal mutation".to_string(),
                output_kind: OutputKind::Assistant,
                content_classification: ContentClassification::Private,
                logical_unit_id: Some("wake-unit".to_string()),
                coalesce_key: None,
                expects_reply: false,
            },
            "wake-test-caller",
            1_001,
        );
        assert!(published.accepted);

        let mut scene = tze_hud_scene::SceneGraph::new(1920.0, 1080.0);
        scene.create_tab("Main", 0).unwrap();
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());
        app.state.portal_projection_driver = driver;
        app
    }

    #[test]
    fn empty_portal_ingress_drain_does_not_create_a_compositor_generation() {
        let mut scene = tze_hud_scene::SceneGraph::new(1920.0, 1080.0);
        scene.create_tab("Main", 0).unwrap();
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());
        app.state.wake.main_work_notifier().notify();
        let checkpoint = app.state.wake.main_work_checkpoint();
        let compositor_generation = app.state.wake.compositor().checkpoint();

        let drain = app.drain_portal_projection();
        let scene_changed = drain.scene_changed();

        assert!(!scene_changed);
        assert!(!drain.is_deferred());
        assert!(
            !app.state
                .wake
                .finish_main_work_after_settle(checkpoint, scene_changed)
        );
        assert_eq!(
            app.state.wake.compositor().checkpoint(),
            compositor_generation
        );
    }

    /// Portal work deferred by a held scene lock must wait for the one
    /// availability completion, not manufacture periodic retry wakes from its
    /// already-due portal deadline.
    #[test]
    fn held_scene_lock_retries_portal_drain_only_after_availability() {
        use std::time::{Duration, Instant};

        let mut app = make_windowed_app_with_pending_portal_publish("proj-availability-wake");
        let shared_state = Arc::clone(&app.state.shared_state);
        let guard = shared_state
            .try_lock()
            .expect("shared scene must be free before inducing contention");
        let compositor_before = app.state.wake.compositor().checkpoint();
        let main_work_before = app.state.wake.main_work_generation();

        let drain = app.drain_portal_projection();
        assert_eq!(drain, PortalProjectionDrain::Deferred);
        app.schedule_shared_scene_availability_wake();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            app.state.wake.main_work_generation(),
            main_work_before,
            "held scene lock must cause zero periodic portal retry wakes"
        );
        assert_eq!(
            app.state.wake.compositor().checkpoint(),
            compositor_before,
            "held scene lock must not create compositor retry work"
        );

        drop(guard);

        let deadline = Instant::now() + Duration::from_secs(1);
        while app.state.wake.main_work_generation() == main_work_before {
            assert!(
                Instant::now() < deadline,
                "availability release must schedule the deferred portal drain"
            );
            std::thread::yield_now();
        }

        let drain = app.drain_portal_projection();
        assert_eq!(
            drain,
            PortalProjectionDrain::Completed {
                scene_changed: true
            }
        );
        let checkpoint = app.state.wake.main_work_checkpoint();
        assert!(app.state.wake.finish_main_work(checkpoint));
        assert_eq!(
            app.state.wake.compositor().checkpoint(),
            compositor_before + 1,
            "the release-driven portal drain owns exactly one compositor wake"
        );
    }

    #[test]
    fn main_only_portal_publish_drains_to_one_post_mutation_scene_wake() {
        let mut app = make_windowed_app_with_pending_portal_publish("proj-scene-wake");
        app.state.wake.main_work_notifier().notify();
        let checkpoint = app.state.wake.main_work_checkpoint();
        let compositor_generation = app.state.wake.compositor().checkpoint();
        let (scene_version_before, tile_count_before) = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            (scene.version, scene.tiles.len())
        };

        let drain = app.drain_portal_projection();
        let scene_changed = drain.scene_changed();

        assert!(scene_changed);
        assert!(!drain.is_deferred());
        {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            assert!(scene.version > scene_version_before);
            assert!(scene.tiles.len() > tile_count_before);
        }
        assert!(
            app.state
                .wake
                .finish_main_work_after_settle(checkpoint, scene_changed)
        );
        assert_eq!(
            app.state.wake.compositor().checkpoint(),
            compositor_generation + 1
        );
        let observed = app
            .state
            .wake
            .compositor()
            .wait(compositor_generation, None);
        assert_eq!(
            observed.source,
            crate::idle_efficiency::RuntimeWakeupSource::SceneChange
        );
    }

    #[test]
    fn typed_portal_deadline_owns_the_single_post_mutation_wake() {
        let mut app = make_windowed_app_with_pending_portal_publish("proj-typed-wake");
        app.state
            .wake
            .mark_main_work_pending(crate::idle_efficiency::RuntimeWakeupSource::TtlDeadline);
        let checkpoint = app.state.wake.main_work_checkpoint();
        let compositor_generation = app.state.wake.compositor().checkpoint();

        let drain = app.drain_portal_projection();
        let scene_changed = drain.scene_changed();

        assert!(scene_changed);
        assert!(!drain.is_deferred());
        assert!(
            app.state
                .wake
                .finish_main_work_after_settle(checkpoint, scene_changed)
        );
        assert_eq!(
            app.state.wake.compositor().checkpoint(),
            compositor_generation + 1
        );
        let observed = app
            .state
            .wake
            .compositor()
            .wait(compositor_generation, None);
        assert_eq!(
            observed.source,
            crate::idle_efficiency::RuntimeWakeupSource::TtlDeadline
        );
    }

    #[test]
    fn cancelled_wait_claims_a_deadline_crossed_before_settle_checkpoint() {
        let mut scene = tze_hud_scene::SceneGraph::new(1920.0, 1080.0);
        scene.create_tab("Main", 0).unwrap();
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());
        let deadline_at = std::time::Instant::now() + std::time::Duration::from_millis(10);
        app.state.scheduled_main_deadline = Some(crate::windowed::wake::Deadline::new(
            deadline_at,
            crate::idle_efficiency::RuntimeWakeupSource::TtlDeadline,
        ));
        let counters_before = app.state.wake.counters().snapshot();

        assert!(
            app.claim_due_scheduled_main_deadline(
                deadline_at - std::time::Duration::from_millis(1)
            )
            .is_none(),
            "a WaitCancelled turn just before T must leave the deadline scheduled"
        );
        assert!(
            app.claim_due_scheduled_main_deadline(
                deadline_at + std::time::Duration::from_millis(1)
            )
            .is_some()
        );
        assert_eq!(
            app.state.wake.counters().snapshot().main_loop_wakeups,
            counters_before.main_loop_wakeups,
            "a deadline piggybacked on a cancelled wait must not count a second main wake"
        );
        let checkpoint = app.state.wake.main_work_checkpoint();
        let compositor_generation = app.state.wake.compositor().checkpoint();
        assert!(
            app.state
                .wake
                .finish_main_work_after_settle(checkpoint, false)
        );
        assert_eq!(
            app.state.wake.compositor().checkpoint(),
            compositor_generation + 1
        );
        let observed = app
            .state
            .wake
            .compositor()
            .wait(compositor_generation, None);
        assert_eq!(
            observed.source,
            crate::idle_efficiency::RuntimeWakeupSource::TtlDeadline
        );
        assert!(app.state.scheduled_main_deadline.is_none());
    }

    // ── Drag-to-move: long-press drag moves a text stream portal tile [hud-9yfce] ──

    /// A long-press drag on a tile's drag handle must move the tile's bounds and
    /// return a `DragReleasedData` payload when the pointer is released.
    ///
    /// Acceptance criteria for hud-9yfce:
    /// - `Moved` outcome during pointer-move updates `tile.bounds` immediately.
    /// - `Released` outcome on pointer-up produces persist data.
    /// - Click-focus is unaffected: a short tap (no long-press) produces no move.
    #[test]
    fn drag_to_move_long_press_moves_tile_bounds() {
        use std::thread;
        use std::time::Duration;
        use tze_hud_input::{InputProcessor, PointerEvent};

        let (mut scene, tile_id, element_id, _interaction_id) =
            scene_with_drag_handle_tile(400.0, 300.0, 600.0, 200.0);

        // The drag handle was placed at:
        //   x = 400 + 600/2 - 20 = 680, y = 300 - 10 = 290, w=40, h=20
        // So the handle spans x: 680..720, y: 290..310.
        let handle_cx = 700.0_f32; // centre of the handle
        let handle_cy = 300.0_f32;

        let mut processor = InputProcessor::new();

        // ── Step 1: PointerDown on the drag handle ────────────────────────────
        let down = PointerEvent {
            x: handle_cx,
            y: handle_cy,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        // process() produces the HitResult for the drag handle.
        let result_down = processor.process(&down, &mut scene);
        assert!(
            result_down.hit.is_zone_interaction(),
            "pointer-down on drag handle must produce ZoneInteraction hit"
        );

        // Drive the drag state machine — should start accumulating.
        let released_on_down = super::apply_drag_handle_pointer_event(
            &mut processor,
            &down,
            &result_down.hit,
            &mut scene,
            1920.0,
            1080.0,
        );
        assert!(
            released_on_down.is_none(),
            "PointerDown must not trigger release"
        );
        assert!(
            processor.drag_states.contains_key(&0),
            "drag state must be created for device 0 after PointerDown on handle"
        );

        // ── Step 2: Wait for long-press threshold (250 ms) ───────────────────
        thread::sleep(Duration::from_millis(260));

        // ── Step 3: PointerMove — first move activates the drag, second moves tile ──
        //
        // The state machine on the first PointerMove after the threshold transitions
        // Accumulating → Activated (returns `Activated`, not `Moved` yet).  The
        // grab offset is recorded at activation.  Subsequent PointerMove events
        // return `Moved` and update the tile bounds.
        let move1 = PointerEvent {
            x: handle_cx + 5.0, // small nudge triggers Activated
            y: handle_cy,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        };
        let result_move1 = processor.process(&move1, &mut scene);
        let released_on_move1 = super::apply_drag_handle_pointer_event(
            &mut processor,
            &move1,
            &result_move1.hit,
            &mut scene,
            1920.0,
            1080.0,
        );
        assert!(
            released_on_move1.is_none(),
            "first PointerMove (Activated) must not trigger release"
        );

        // Second PointerMove — now in Activated phase, returns Moved.
        let move2 = PointerEvent {
            x: handle_cx + 100.0,
            y: handle_cy + 50.0,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        };
        let result_move2 = processor.process(&move2, &mut scene);
        let released_on_move2 = super::apply_drag_handle_pointer_event(
            &mut processor,
            &move2,
            &result_move2.hit,
            &mut scene,
            1920.0,
            1080.0,
        );
        assert!(
            released_on_move2.is_none(),
            "PointerMove must not trigger release"
        );

        // The tile must have moved from its original position.
        let tile_after_move = scene.tiles.get(&tile_id).expect("tile must exist");
        assert_ne!(
            tile_after_move.bounds.x, 400.0,
            "tile X must change after drag move"
        );
        assert_ne!(
            tile_after_move.bounds.y, 300.0,
            "tile Y must change after drag move"
        );

        // ── Step 4: PointerUp — should release and return persist data ────────
        // Release at the same position as the last move.
        let up = PointerEvent {
            x: handle_cx + 100.0,
            y: handle_cy + 50.0,
            kind: PointerEventKind::Up,
            device_id: 0,
            timestamp: None,
        };
        let result_up = processor.process(&up, &mut scene);
        let released_on_up = super::apply_drag_handle_pointer_event(
            &mut processor,
            &up,
            &result_up.hit,
            &mut scene,
            1920.0,
            1080.0,
        );

        let released =
            released_on_up.expect("PointerUp after activated drag must return released data");
        assert_eq!(
            released.element_id, element_id,
            "released element_id must match the dragged tile"
        );
        assert!(
            released.final_x >= 0.0 && released.final_x + released.width <= 1920.0,
            "final X must be within display bounds"
        );
        assert!(
            released.final_y >= 0.0 && released.final_y + released.height <= 1080.0,
            "final Y must be within display bounds"
        );

        // Drag state must be cleaned up after release.
        assert!(
            !processor.drag_states.contains_key(&0),
            "drag state must be removed after PointerUp"
        );
    }

    /// A quick tap (PointerDown immediately followed by PointerUp, no long-press)
    /// on a drag handle must NOT move the tile — the click-to-focus path must be
    /// unaffected.
    ///
    /// Hysteresis: the 250 ms threshold ensures taps are recognised as clicks,
    /// not drags.
    #[test]
    fn drag_to_move_quick_tap_does_not_move_tile() {
        use tze_hud_input::{InputProcessor, PointerEvent};

        let (mut scene, tile_id, _element_id, _interaction_id) =
            scene_with_drag_handle_tile(400.0, 300.0, 600.0, 200.0);

        // Same drag handle position as drag_to_move_long_press_moves_tile_bounds:
        //   x: 680..720, y: 290..310.
        let handle_cx = 700.0_f32; // centre of the handle
        let handle_cy = 300.0_f32;

        let mut processor = InputProcessor::new();

        // PointerDown.
        let down = PointerEvent {
            x: handle_cx,
            y: handle_cy,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        let result_down = processor.process(&down, &mut scene);
        let _ = super::apply_drag_handle_pointer_event(
            &mut processor,
            &down,
            &result_down.hit,
            &mut scene,
            1920.0,
            1080.0,
        );

        // PointerUp immediately — no long-press threshold met.
        let up = PointerEvent {
            x: handle_cx,
            y: handle_cy,
            kind: PointerEventKind::Up,
            device_id: 0,
            timestamp: None,
        };
        let result_up = processor.process(&up, &mut scene);
        let released_on_up = super::apply_drag_handle_pointer_event(
            &mut processor,
            &up,
            &result_up.hit,
            &mut scene,
            1920.0,
            1080.0,
        );

        // Must NOT return release data — this was a tap, not a drag.
        assert!(
            released_on_up.is_none(),
            "quick tap must not trigger drag release [click-focus coexistence]"
        );

        // Tile bounds must be unchanged.
        let tile = scene.tiles.get(&tile_id).expect("tile must exist");
        assert_eq!(
            tile.bounds.x, 400.0,
            "tile X must not change after a tap on the drag handle"
        );
        assert_eq!(
            tile.bounds.y, 300.0,
            "tile Y must not change after a tap on the drag handle"
        );
    }

    #[test]
    fn pointer_down_on_drag_handle_requests_guaranteed_feedback_from_snapshot_gate() {
        let (scene, _tile_id, _element_id, _interaction_id) =
            scene_with_drag_handle_tile(400.0, 300.0, 600.0, 200.0);
        let snapshot = crate::pipeline::HitTestSnapshot::from_scene(&scene);

        assert!(
            pointer_down_starts_guaranteed_feedback_gesture(
                &snapshot,
                700.0,
                300.0,
                PortalWindowTokens::default(),
            ),
            "PointerDown on a drag handle must spin-acquire so the drag state can start under contention"
        );
    }

    #[test]
    fn pointer_down_on_resize_affordance_requests_guaranteed_feedback_from_snapshot_gate() {
        let (scene, _tab_id, _tile_id, _fm) = portal_scene_with_focus();
        let snapshot = crate::pipeline::HitTestSnapshot::from_scene(&scene);

        assert!(
            pointer_down_starts_guaranteed_feedback_gesture(
                &snapshot,
                496.0,
                250.0,
                PortalWindowTokens::default(),
            ),
            "PointerDown on a portal resize affordance must spin-acquire so the resize gesture can start under contention"
        );
    }

    /// hud-yno2r: the pre-lock guaranteed-feedback predicate must also fire when
    /// the pointer is on the resize corner of a NON-scrollable portal *frame*
    /// that contains scrollable panes — the first-class / multi-surface layout.
    /// Before the fix this keyed off the focused tile being scrollable, so a
    /// frame-corner Down fell back to a one-shot `try_lock` and could be dropped
    /// under scene-lock contention, leaving the new resize path intermittently
    /// inert.
    #[test]
    fn pointer_down_on_multi_surface_frame_corner_requests_guaranteed_feedback() {
        let (scene, _tab_id, _frame_id, _transcript_id, _composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();
        let snapshot = crate::pipeline::HitTestSnapshot::from_scene(&scene);

        // Frame is (100,100,400,300) → bottom-right corner (500,400); (498,398)
        // sits in the affordance band. The frame itself is NOT scrollable.
        assert!(
            pointer_down_starts_guaranteed_feedback_gesture(
                &snapshot,
                498.0,
                398.0,
                PortalWindowTokens::default(),
            ),
            "PointerDown on a multi-surface portal frame's resize corner must spin-acquire \
             even though the frame tile is not itself scrollable (hud-yno2r)"
        );
    }

    #[test]
    fn ordinary_pointer_down_does_not_request_guaranteed_feedback_spin() {
        let (scene, _tile_id) = scene_with_capture_tile();
        let snapshot = crate::pipeline::HitTestSnapshot::from_scene(&scene);

        assert!(
            !pointer_down_starts_guaranteed_feedback_gesture(
                &snapshot,
                320.0,
                420.0,
                PortalWindowTokens::default(),
            ),
            "ordinary content PointerDown must stay on the single try_lock path to preserve click-to-focus latency"
        );
    }

    #[test]
    fn ctrl_resize_hotkey_resizes_focused_portal_while_composer_active() {
        use tze_hud_input::{FocusManager, InputProcessor, KeyboardModifiers};
        use tze_hud_scene::types::{HitRegionNode, TileScrollConfig};
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

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
        scene
            .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
            .unwrap();

        let composer_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: composer_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 400.0, 60.0),
                        interaction_id: "portal-composer".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        accepts_composer_input: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();
        let mut focus_manager = FocusManager::new();
        focus_manager.add_tab(tab_id);
        let (_input_result, transition) = processor.process_with_focus(
            &tze_hud_input::PointerEvent {
                x: 110.0,
                y: 110.0,
                kind: tze_hud_input::PointerEventKind::Down,
                device_id: 1,
                timestamp: None,
            },
            &mut scene,
            &mut focus_manager,
            tab_id,
        );
        assert!(
            transition.and_then(|t| t.gained).is_some(),
            "pointer down must focus the composer node"
        );
        assert!(
            processor.is_composer_active(),
            "composer must be active after focusing the portal composer"
        );

        let (mut app, mut input_event_rx) =
            make_windowed_keyboard_test_app(scene, focus_manager, processor);

        let bounds = |app: &WinitApp| {
            let shared = app
                .state
                .shared_state
                .try_lock()
                .expect("shared state must be available during key dispatch test");
            let scene = shared
                .scene
                .try_lock()
                .expect("scene must be available during key dispatch test");
            scene.tiles.get(&tile_id).unwrap().bounds
        };

        let dispatch_ctrl_key =
            |app: &mut WinitApp, key_code: &str, key: &str, shift: bool, timestamp: u64| {
                app.dispatch_key_down_event_inner(
                    &RawKeyDownEvent {
                        key_code: key_code.to_string(),
                        key: key.to_string(),
                        modifiers: KeyboardModifiers {
                            ctrl: true,
                            shift,
                            ..KeyboardModifiers::NONE
                        },
                        repeat: false,
                        timestamp_mono_us: tze_hud_scene::MonoUs(timestamp),
                    },
                    Some(tab_id),
                );
            };
        let dispatch_ctrl_key_up =
            |app: &mut WinitApp, key_code: &str, key: &str, shift: bool, timestamp: u64| {
                app.dispatch_key_up_event_inner(
                    &RawKeyUpEvent {
                        key_code: key_code.to_string(),
                        key: key.to_string(),
                        modifiers: KeyboardModifiers {
                            ctrl: true,
                            shift,
                            ..KeyboardModifiers::NONE
                        },
                        timestamp_mono_us: tze_hud_scene::MonoUs(timestamp),
                    },
                    Some(tab_id),
                );
            };

        let before_equal = bounds(&app);
        dispatch_ctrl_key(&mut app, "Equal", "=", false, 1);
        let after_equal = bounds(&app);

        assert!(
            after_equal.width > before_equal.width,
            "Ctrl+= must grow the focused portal even when the composer is active"
        );
        assert!(
            after_equal.height > before_equal.height,
            "Ctrl+= must grow the focused portal vertically as well"
        );

        // The matching KeyUp for a KeyDown that already resized must be swallowed
        // by the dedup set, NOT applied as a second resize (hud-v4k1h).
        dispatch_ctrl_key_up(&mut app, "Equal", "=", false, 2);
        let after_equal_release = bounds(&app);
        assert_eq!(
            after_equal_release.width, after_equal.width,
            "matching Ctrl+= KeyUp must not apply a second horizontal resize after KeyDown already resized"
        );
        assert_eq!(
            after_equal_release.height, after_equal.height,
            "matching Ctrl+= KeyUp must not apply a second vertical resize after KeyDown already resized"
        );

        dispatch_ctrl_key(&mut app, "Equal", "+", true, 3);
        let after_plus = bounds(&app);
        assert!(
            after_plus.width > after_equal_release.width,
            "Ctrl++ must grow the focused portal even when the composer is active"
        );
        assert!(
            after_plus.height > after_equal_release.height,
            "Ctrl++ must grow the focused portal vertically as well"
        );

        dispatch_ctrl_key(&mut app, "Minus", "-", false, 4);
        let after_minus = bounds(&app);
        assert!(
            after_minus.width < after_plus.width,
            "Ctrl+- must shrink the focused portal even when the composer is active"
        );
        assert!(
            after_minus.height < after_plus.height,
            "Ctrl+- must shrink the focused portal vertically as well"
        );
        assert!(
            input_event_rx.try_recv().is_err(),
            "resize hotkey must be consumed locally, not forwarded as agent keyboard input"
        );
    }

    /// hud-02sp5: focus acquired via keyboard Tab traversal (no pointer) must
    /// enable the same keyboard operations as click-to-focus. A keyboard-only
    /// viewer (Mobile Presence Node / glasses) Tabs onto the portal composer and
    /// then the Ctrl+= resize chord must resolve and resize the focused portal —
    /// exactly as it does when focus was acquired by a click.
    #[test]
    fn ctrl_resize_hotkey_resizes_portal_focused_via_tab_without_pointer() {
        use tze_hud_input::{FocusManager, InputProcessor, KeyboardModifiers};
        use tze_hud_scene::types::{HitRegionNode, TileScrollConfig};
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

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
        scene
            .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
            .unwrap();

        // Focusable composer affordance — the only Tab stop in this portal.
        let composer_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: composer_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 400.0, 60.0),
                        interaction_id: "portal-composer".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        accepts_composer_input: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        // NB: no pointer event is ever synthesized. Focus is acquired purely
        // through the windowed Tab key path.
        let processor = InputProcessor::new();
        let mut focus_manager = FocusManager::new();
        focus_manager.add_tab(tab_id);

        let (mut app, _input_event_rx) =
            make_windowed_keyboard_test_app(scene, focus_manager, processor);

        let bounds = |app: &WinitApp| {
            let shared = app
                .state
                .shared_state
                .try_lock()
                .expect("shared state must be available during key dispatch test");
            let scene = shared
                .scene
                .try_lock()
                .expect("scene must be available during key dispatch test");
            scene.tiles.get(&tile_id).unwrap().bounds
        };

        // Bare Tab (no modifiers) advances focus onto the composer — the
        // no-pointer analogue of a click. This is the wiring proven by
        // hud-v0cal (PR #980); here we assert it unlocks the resize chord.
        app.dispatch_key_down_event_inner(
            &RawKeyDownEvent {
                key_code: "Tab".to_string(),
                key: "Tab".to_string(),
                modifiers: KeyboardModifiers::NONE,
                repeat: false,
                timestamp_mono_us: tze_hud_scene::MonoUs(1),
            },
            Some(tab_id),
        );
        assert!(
            app.state.input_processor.is_composer_active(),
            "bare Tab must focus the portal composer without any pointer event"
        );

        let dispatch_ctrl_key =
            |app: &mut WinitApp, key_code: &str, key: &str, shift: bool, timestamp: u64| {
                app.dispatch_key_down_event_inner(
                    &RawKeyDownEvent {
                        key_code: key_code.to_string(),
                        key: key.to_string(),
                        modifiers: KeyboardModifiers {
                            ctrl: true,
                            shift,
                            ..KeyboardModifiers::NONE
                        },
                        repeat: false,
                        timestamp_mono_us: tze_hud_scene::MonoUs(timestamp),
                    },
                    Some(tab_id),
                );
            };

        let before = bounds(&app);
        dispatch_ctrl_key(&mut app, "Equal", "=", false, 2);
        let after = bounds(&app);
        assert!(
            after.width > before.width && after.height > before.height,
            "Ctrl+= must grow the portal whose composer was focused via Tab \
             (Tab-acquired focus must enable the same keyboard ops as a click)"
        );
    }

    /// Regression for hud-v4k1h: on live Windows the OS (SendInput) can deliver
    /// ONLY the `KeyUp` for the Equal/Minus chord — the `KeyDown` never arrives
    /// while Ctrl is held. A key-down-only resize intercept therefore silently
    /// does nothing. The key-up fallback in `dispatch_key_up_event_inner` must
    /// resize on the release in that case.
    #[test]
    fn ctrl_resize_keyup_fallback_resizes_when_live_windows_omits_keydown() {
        use tze_hud_input::{FocusManager, InputProcessor, KeyboardModifiers};
        use tze_hud_scene::types::{HitRegionNode, TileScrollConfig};
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

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
        scene
            .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
            .unwrap();

        let composer_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: composer_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 400.0, 60.0),
                        interaction_id: "portal-composer".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        accepts_composer_input: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();
        let mut focus_manager = FocusManager::new();
        focus_manager.add_tab(tab_id);
        let (_input_result, transition) = processor.process_with_focus(
            &tze_hud_input::PointerEvent {
                x: 110.0,
                y: 110.0,
                kind: tze_hud_input::PointerEventKind::Down,
                device_id: 1,
                timestamp: None,
            },
            &mut scene,
            &mut focus_manager,
            tab_id,
        );
        assert!(
            transition.and_then(|t| t.gained).is_some(),
            "pointer down must focus the composer node"
        );

        let (mut app, mut input_event_rx) =
            make_windowed_keyboard_test_app(scene, focus_manager, processor);

        let bounds = |app: &WinitApp| {
            let shared = app
                .state
                .shared_state
                .try_lock()
                .expect("shared state must be available during key dispatch test");
            let scene = shared
                .scene
                .try_lock()
                .expect("scene must be available during key dispatch test");
            scene.tiles.get(&tile_id).unwrap().bounds
        };

        let dispatch_ctrl_key_up =
            |app: &mut WinitApp, key_code: &str, key: &str, shift: bool, timestamp: u64| {
                app.dispatch_key_up_event_inner(
                    &RawKeyUpEvent {
                        key_code: key_code.to_string(),
                        key: key.to_string(),
                        modifiers: KeyboardModifiers {
                            ctrl: true,
                            shift,
                            ..KeyboardModifiers::NONE
                        },
                        timestamp_mono_us: tze_hud_scene::MonoUs(timestamp),
                    },
                    Some(tab_id),
                );
            };

        // No KeyDown was ever consumed, so each KeyUp must drive the resize.
        let before_equal = bounds(&app);
        dispatch_ctrl_key_up(&mut app, "Equal", "=", false, 1);
        let after_equal = bounds(&app);
        assert!(
            after_equal.width > before_equal.width,
            "Ctrl+= release fallback must grow the focused portal when the live OS stream omitted Equal KeyDown"
        );
        assert!(
            after_equal.height > before_equal.height,
            "Ctrl+= release fallback must grow the focused portal vertically as well"
        );

        dispatch_ctrl_key_up(&mut app, "Minus", "-", false, 2);
        let after_minus = bounds(&app);
        assert!(
            after_minus.width < after_equal.width,
            "Ctrl+- release fallback must shrink the focused portal when the live OS stream omitted Minus KeyDown"
        );
        assert!(
            after_minus.height < after_equal.height,
            "Ctrl+- release fallback must shrink the focused portal vertically as well"
        );
        assert!(
            input_event_rx.try_recv().is_err(),
            "release-fallback resize hotkey must be consumed locally, not forwarded as agent keyboard input"
        );
    }

    #[test]
    fn ctrl_resize_hotkey_ignores_unfocused_portal_on_windowed_dispatch_path() {
        use tze_hud_input::{FocusManager, FocusRequest, InputProcessor, KeyboardModifiers};
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;
        use tze_hud_scene::types::TileScrollConfig;
        use tze_hud_scene::{Rect, SceneGraph};

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        let portal_tile_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(100.0, 100.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(portal_tile_id, TileScrollConfig::vertical())
            .unwrap();

        let plain_tile_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(700.0, 100.0, 240.0, 160.0),
                2,
            )
            .unwrap();

        let mut focus_manager = FocusManager::new();
        let (focus_result, _transition) = focus_manager.request_focus(
            FocusRequest {
                tile_id: plain_tile_id,
                node_id: None,
                steal: true,
                requesting_namespace: "portal-agent".to_string(),
            },
            tab_id,
            &scene,
        );
        assert_eq!(
            focus_result,
            tze_hud_input::FocusResult::Granted,
            "test setup must focus the non-portal tile"
        );

        let (mut app, mut input_event_rx) =
            make_windowed_keyboard_test_app(scene, focus_manager, InputProcessor::new());

        let portal_bounds = |app: &WinitApp| {
            let shared = app
                .state
                .shared_state
                .try_lock()
                .expect("shared state must be available during key dispatch test");
            let scene = shared
                .scene
                .try_lock()
                .expect("scene must be available during key dispatch test");
            scene.tiles.get(&portal_tile_id).unwrap().bounds
        };

        let before = portal_bounds(&app);
        app.dispatch_key_down_event_inner(
            &RawKeyDownEvent {
                key_code: "Equal".to_string(),
                key: "=".to_string(),
                modifiers: KeyboardModifiers {
                    ctrl: true,
                    ..KeyboardModifiers::NONE
                },
                repeat: false,
                timestamp_mono_us: tze_hud_scene::MonoUs(1),
            },
            Some(tab_id),
        );
        let after = portal_bounds(&app);

        assert_eq!(
            after, before,
            "Ctrl+= must not resize a portal that does not hold focus"
        );
        assert!(
            app.state.portal_resize_states.is_empty(),
            "unfocused portal hotkey must not create resize state"
        );
        let (namespace, batch) = input_event_rx
            .try_recv()
            .expect("non-consumed hotkey should continue to normal keyboard routing");
        assert_eq!(namespace, "portal-agent");
        match batch
            .events
            .first()
            .and_then(|envelope| envelope.event.as_ref())
        {
            Some(InputEvent::KeyDown(ev)) => {
                assert_eq!(ev.key, "=");
                assert!(ev.ctrl, "forwarded key event must preserve Ctrl");
            }
            other => panic!("expected forwarded KeyDown event, got {other:?}"),
        }
    }

    // ── Portal control keyboard recovery / activation (hud-2v8br) ──────────
    //
    // Build a portal tile whose root composer (accepts_composer_input) has a
    // non-composer minimize control as a focusable child, land Tab focus on the
    // control, and assert the runtime never strands the keyboard user.

    /// Construct a portal scene: one tile rooted at a composer node with a
    /// focusable non-composer control child. Returns
    /// `(scene, tab_id, tile_id, composer_id, control_id)`.
    fn portal_scene_with_control() -> (
        tze_hud_scene::graph::SceneGraph,
        tze_hud_scene::SceneId,
        tze_hud_scene::SceneId,
        tze_hud_scene::SceneId,
        tze_hud_scene::SceneId,
    ) {
        use tze_hud_scene::types::{HitRegionNode, TileScrollConfig};
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

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
        scene
            .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
            .unwrap();

        let composer_id = SceneId::new();
        let control_id = SceneId::new();
        scene.nodes.insert(
            composer_id,
            Node {
                layout: Default::default(),
                id: composer_id,
                children: vec![control_id],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 400.0, 60.0),
                    interaction_id: "portal-composer".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        );
        scene.nodes.insert(
            control_id,
            Node {
                layout: Default::default(),
                id: control_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(10.0, 80.0, 40.0, 40.0),
                    interaction_id: "portal-minimize".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: false,
                    ..Default::default()
                }),
            },
        );
        scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(composer_id);
        (scene, tab_id, tile_id, composer_id, control_id)
    }

    /// Enter on a Tab-focused portal control activates it by broadcasting a
    /// synthetic PointerDown (+PointerUp) carrying the control's interaction_id,
    /// so the owning agent's click handler fires — the control is not a dead stop.
    #[test]
    fn enter_on_focused_portal_control_activates_via_synthetic_pointer() {
        use tze_hud_input::KeyboardModifiers;
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

        let (mut scene, tab_id, _tile_id, composer_id, control_id) = portal_scene_with_control();
        let mut processor = InputProcessor::new();
        let mut focus_manager = FocusManager::new();
        focus_manager.add_tab(tab_id);
        // Tab onto the composer, then Tab again onto the control.
        processor.navigate_focus(&mut focus_manager, &mut scene, tab_id, false);
        processor.navigate_focus(&mut focus_manager, &mut scene, tab_id, false);
        assert_eq!(
            focus_manager.current_owner(tab_id).node_id(),
            Some(control_id),
            "test setup: focus must rest on the non-composer control"
        );
        assert!(
            !processor.is_composer_active(),
            "composer must be inactive while the control holds focus"
        );

        let (mut app, mut input_event_rx) =
            make_windowed_keyboard_test_app(scene, focus_manager, processor);

        app.dispatch_key_down_event_inner(
            &RawKeyDownEvent {
                key_code: "Enter".to_string(),
                key: "Enter".to_string(),
                modifiers: KeyboardModifiers::NONE,
                repeat: false,
                timestamp_mono_us: tze_hud_scene::MonoUs(1),
            },
            Some(tab_id),
        );

        // First broadcast must be a synthetic PointerDown on the control.
        let (namespace, batch) = input_event_rx
            .try_recv()
            .expect("Enter on a focused control must broadcast a synthetic pointer event");
        assert_eq!(namespace, "portal-agent");
        match batch.events.first().and_then(|e| e.event.as_ref()) {
            Some(InputEvent::PointerDown(ev)) => {
                assert_eq!(
                    ev.interaction_id, "portal-minimize",
                    "activation must target the focused control's interaction_id"
                );
            }
            other => panic!("expected synthetic PointerDown, got {other:?}"),
        }
        // A matching PointerUp completes the synthetic click.
        let (_ns, up_batch) = input_event_rx
            .try_recv()
            .expect("activation must also broadcast a PointerUp");
        assert!(
            matches!(
                up_batch.events.first().and_then(|e| e.event.as_ref()),
                Some(InputEvent::PointerUp(_))
            ),
            "second synthetic event must be a PointerUp"
        );
        // Focus stays on the control (activation is not a focus move).
        let _ = composer_id;
    }

    /// Typing a printable character while a portal control holds focus recovers
    /// to the composer and inserts the character — a keyboard user is never
    /// stranded with dead typing.
    #[test]
    fn typing_on_focused_portal_control_recovers_to_composer() {
        use tze_hud_input::RawCharacterEvent;

        let (mut scene, tab_id, _tile_id, composer_id, control_id) = portal_scene_with_control();
        let mut processor = InputProcessor::new();
        let mut focus_manager = FocusManager::new();
        focus_manager.add_tab(tab_id);
        processor.navigate_focus(&mut focus_manager, &mut scene, tab_id, false);
        processor.navigate_focus(&mut focus_manager, &mut scene, tab_id, false);
        assert_eq!(
            focus_manager.current_owner(tab_id).node_id(),
            Some(control_id),
            "test setup: focus must rest on the control"
        );

        let (mut app, _input_event_rx) =
            make_windowed_keyboard_test_app(scene, focus_manager, processor);

        app.dispatch_character_event_inner(
            &RawCharacterEvent {
                character: "h".to_string(),
                timestamp_mono_us: tze_hud_scene::MonoUs(1),
            },
            Some(tab_id),
        );

        assert!(
            app.state.input_processor.is_composer_active(),
            "typing on a control must recover focus to the composer (draft active)"
        );
        assert_eq!(
            app.state.focus_manager.current_owner(tab_id).node_id(),
            Some(composer_id),
            "recovery must move focus onto the composer node"
        );
        assert_eq!(
            app.state
                .input_processor
                .composer_draft_snapshot()
                .map(|s| s.0),
            Some("h".to_string()),
            "the typed character must land in the composer draft after recovery"
        );
    }

    /// Escape on a Tab-focused portal control recovers focus to the composer.
    #[test]
    fn escape_on_focused_portal_control_recovers_to_composer() {
        use tze_hud_input::KeyboardModifiers;

        let (mut scene, tab_id, _tile_id, composer_id, control_id) = portal_scene_with_control();
        let mut processor = InputProcessor::new();
        let mut focus_manager = FocusManager::new();
        focus_manager.add_tab(tab_id);
        processor.navigate_focus(&mut focus_manager, &mut scene, tab_id, false);
        processor.navigate_focus(&mut focus_manager, &mut scene, tab_id, false);
        assert_eq!(
            focus_manager.current_owner(tab_id).node_id(),
            Some(control_id)
        );

        let (mut app, _input_event_rx) =
            make_windowed_keyboard_test_app(scene, focus_manager, processor);

        app.dispatch_key_down_event_inner(
            &RawKeyDownEvent {
                key_code: "Escape".to_string(),
                key: "Escape".to_string(),
                modifiers: KeyboardModifiers::NONE,
                repeat: false,
                timestamp_mono_us: tze_hud_scene::MonoUs(1),
            },
            Some(tab_id),
        );

        assert_eq!(
            app.state.focus_manager.current_owner(tab_id).node_id(),
            Some(composer_id),
            "Escape on a control must recover focus to the composer"
        );
        assert!(
            app.state.input_processor.is_composer_active(),
            "composer draft must be active after Escape recovery"
        );
    }

    /// hud-k6yvb: Escape on a composer-less focus stop (a focusable node whose
    /// tile has no composer) clears focus to None — the keyboard user is never
    /// stranded — and broadcasts a FocusLost for the released node.
    #[test]
    fn escape_on_composerless_stop_clears_focus_to_none() {
        use tze_hud_input::{FocusManager, InputProcessor, KeyboardModifiers};
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;
        use tze_hud_scene::types::HitRegionNode;
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(100.0, 100.0, 300.0, 200.0),
                1,
            )
            .unwrap();
        // A plain focusable control — NO composer in the tile.
        let node_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: node_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 120.0, 40.0),
                        interaction_id: "plain".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        accepts_composer_input: false,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();
        let mut focus_manager = FocusManager::new();
        focus_manager.add_tab(tab_id);
        processor.navigate_focus(&mut focus_manager, &mut scene, tab_id, false);
        assert_eq!(focus_manager.current_owner(tab_id).node_id(), Some(node_id));

        let (mut app, mut rx) = make_windowed_keyboard_test_app(scene, focus_manager, processor);

        app.dispatch_key_down_event_inner(
            &RawKeyDownEvent {
                key_code: "Escape".to_string(),
                key: "Escape".to_string(),
                modifiers: KeyboardModifiers::NONE,
                repeat: false,
                timestamp_mono_us: tze_hud_scene::MonoUs(1),
            },
            Some(tab_id),
        );

        assert_eq!(
            *app.state.focus_manager.current_owner(tab_id),
            tze_hud_input::FocusOwner::None,
            "Escape on a composer-less stop must clear focus to None"
        );
        // A FocusLost event was broadcast for the released node. CANCEL is
        // intentionally delivered first while the old owner is still the
        // command target, so inspect the whole resulting event sequence.
        let mut events = Vec::new();
        while let Ok((_namespace, batch)) = rx.try_recv() {
            events.extend(batch.events.into_iter().filter_map(|event| event.event));
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, InputEvent::FocusLost(_))),
            "the broadcasts must include FocusLost for the released control; got {events:?}"
        );
    }

    // ── Pilot-path viewer history (hud-nx7yq.3) ────────────────────────────

    fn viewer_echo_context(tile_id: tze_hud_scene::SceneId) -> ComposerDeliveryContext {
        ComposerDeliveryContext {
            namespace: "portal-agent".to_string(),
            node_id_bytes: [0u8; 16],
            tile_id,
        }
    }

    /// An accepted submission on a raw-tile portal (not authority-attached)
    /// enqueues a runtime-authored viewer echo carrying the submitted text.
    #[test]
    fn accepted_raw_tile_submission_enqueues_viewer_echo() {
        let (scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        let mut batch = tze_hud_input::DraftNotificationBatch::new();
        batch.record_submission(tze_hud_input::DraftSubmission {
            text: "hello from the viewer".to_string(),
            sequence: 1,
        });

        app.route_and_deliver_composer_batch(viewer_echo_context(tile_id), batch);

        let queue = app.state.viewer_echo_queue.lock().unwrap();
        assert_eq!(queue.len(), 1, "one viewer echo must be enqueued on submit");
        assert_eq!(queue[0].tile_id, tile_id);
        assert_eq!(queue[0].text, "hello from the viewer");
    }

    /// A batch with no transactional submission (draft state only, or empty)
    /// enqueues nothing — rejected/non-submissions are never echoed.
    #[test]
    fn non_submission_batch_enqueues_no_viewer_echo() {
        let (scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        // An empty batch (no submission) — e.g. a cancel or a plain state update.
        let batch = tze_hud_input::DraftNotificationBatch::new();
        app.route_and_deliver_composer_batch(viewer_echo_context(tile_id), batch);

        assert!(
            app.state.viewer_echo_queue.lock().unwrap().is_empty(),
            "no submission => no viewer echo (rejected submissions append nothing)"
        );
    }

    /// A whitespace-only submission is not echoed (empty content is a no-op).
    #[test]
    fn whitespace_only_submission_enqueues_no_viewer_echo() {
        let (scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        let mut batch = tze_hud_input::DraftNotificationBatch::new();
        batch.record_submission(tze_hud_input::DraftSubmission {
            text: "   ".to_string(),
            sequence: 1,
        });
        app.route_and_deliver_composer_batch(viewer_echo_context(tile_id), batch);

        assert!(
            app.state.viewer_echo_queue.lock().unwrap().is_empty(),
            "whitespace-only submission must not enqueue a viewer echo"
        );
    }

    /// A submission whose history overflows the band above the composer
    /// (hud-qbcp8) must register the tile as scrollable and seed the scroll
    /// offset directly to the tail — not to `0.0` (which would show the
    /// OLDEST line, per `input_history_block_top`'s convention).
    #[test]
    fn overflowing_raw_tile_submission_registers_scroll_and_seeds_tail() {
        let (scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        // Tile viewport is 300px tall; 20 lines at ~21px/line is ~420px —
        // comfortably past the viewport, so the tail offset must be nonzero.
        let text = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut batch = tze_hud_input::DraftNotificationBatch::new();
        batch.record_submission(tze_hud_input::DraftSubmission { text, sequence: 1 });
        app.route_and_deliver_composer_batch(viewer_echo_context(tile_id), batch);

        let shared = app.state.shared_state.try_lock().unwrap();
        let scene = shared.scene.try_lock().unwrap();
        assert!(
            scene.tile_scroll_config(tile_id).is_some(),
            "the input tile must be registered as scrollable after a submission"
        );
        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            offset_y > 0.0,
            "the first overflowing submission must seed a nonzero tail offset, got {offset_y}"
        );
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "a freshly-seeded history must be at-tail"
        );
    }

    /// After the viewer scrolls up through their own history and then submits
    /// another reply, the scroll offset must snap back to the (new) tail
    /// rather than staying scrolled away from the just-submitted reply
    /// (hud-qbcp8: "don't strand the viewer scrolled-up").
    #[test]
    fn resubmitting_after_scroll_back_snaps_history_to_tail() {
        let (scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        let first_text = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut batch = tze_hud_input::DraftNotificationBatch::new();
        batch.record_submission(tze_hud_input::DraftSubmission {
            text: first_text,
            sequence: 1,
        });
        app.route_and_deliver_composer_batch(viewer_echo_context(tile_id), batch);

        let tail_after_first = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tile_scroll_offset_local(tile_id).1
        };
        assert!(tail_after_first > 0.0);

        // Scroll the viewer back up (away from the tail), inside the tile
        // (tile bounds are 100,100 .. 500,400; this point misses both the
        // composer and control hit regions, hitting the tile background).
        {
            let shared = app.state.shared_state.try_lock().unwrap();
            let mut scene = shared.scene.try_lock().unwrap();
            let _ = app.state.input_processor.process_scroll_event(
                &tze_hud_input::ScrollEvent {
                    x: 300.0,
                    y: 250.0,
                    delta_x: 0.0,
                    delta_y: -50.0,
                },
                &mut scene,
            );
            assert!(
                !scene.tile_follow_tail_at_tail(tile_id),
                "test setup: scrolling up must leave the tile ScrolledBack"
            );
        }

        // Submit a second reply — must force the viewport back to the tail.
        let mut batch = tze_hud_input::DraftNotificationBatch::new();
        batch.record_submission(tze_hud_input::DraftSubmission {
            text: "one more reply".to_string(),
            sequence: 2,
        });
        app.route_and_deliver_composer_batch(viewer_echo_context(tile_id), batch);

        let shared = app.state.shared_state.try_lock().unwrap();
        let scene = shared.scene.try_lock().unwrap();
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "submitting a new reply must force the history back to the tail"
        );
        let (_, offset_after_resubmit) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            offset_after_resubmit >= tail_after_first,
            "the post-resubmit tail must be at or past the pre-scroll-back tail \
             ({tail_after_first}); got {offset_after_resubmit}"
        );
    }

    /// Typing into the composer while the input-pane history is scrolled back
    /// must snap the history's scroll offset back to the tail (hud-qbcp8:
    /// "don't strand the viewer scrolled-up" while they're actively drafting).
    #[test]
    fn typing_while_scrolled_back_resets_history_scroll_to_tail() {
        use tze_hud_input::RawCharacterEvent;

        let (mut scene, tab_id, tile_id, composer_id, _control_id) = portal_scene_with_control();
        let mut processor = InputProcessor::new();
        let mut focus_manager = FocusManager::new();
        focus_manager.add_tab(tab_id);
        processor.navigate_focus(&mut focus_manager, &mut scene, tab_id, false);
        assert_eq!(
            focus_manager.current_owner(tab_id).node_id(),
            Some(composer_id),
            "test setup: focus must rest on the composer"
        );
        assert!(
            processor.is_composer_active(),
            "test setup: landing focus on the composer must activate the draft"
        );

        let (mut app, _input_event_rx) =
            make_windowed_keyboard_test_app(scene, focus_manager, processor);

        // Seed an overflowing history and scroll away from the tail (same setup
        // as `resubmitting_after_scroll_back_snaps_history_to_tail`).
        let text = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut batch = tze_hud_input::DraftNotificationBatch::new();
        batch.record_submission(tze_hud_input::DraftSubmission { text, sequence: 1 });
        app.route_and_deliver_composer_batch(viewer_echo_context(tile_id), batch);
        {
            let shared = app.state.shared_state.try_lock().unwrap();
            let mut scene = shared.scene.try_lock().unwrap();
            let _ = app.state.input_processor.process_scroll_event(
                &tze_hud_input::ScrollEvent {
                    x: 300.0,
                    y: 250.0,
                    delta_x: 0.0,
                    delta_y: -50.0,
                },
                &mut scene,
            );
            assert!(
                !scene.tile_follow_tail_at_tail(tile_id),
                "test setup: scrolling up must leave the tile ScrolledBack"
            );
        }

        // Type a character while the history is scrolled back.
        app.dispatch_character_event_inner(
            &RawCharacterEvent {
                character: "h".to_string(),
                timestamp_mono_us: tze_hud_scene::MonoUs(1),
            },
            Some(tab_id),
        );

        let shared = app.state.shared_state.try_lock().unwrap();
        let scene = shared.scene.try_lock().unwrap();
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "typing into the focused composer must snap the input-pane history \
             back to the tail"
        );
    }

    // ── Input-history scroll seed: exact tail-pin (hud-3y7va) ──────────────────

    /// A long reply with NO hard newlines soft-wraps into several visual rows the
    /// compositor lays out, but the pre-fix `split('\n')` estimate counted it as
    /// ONE row. `approx_wrapped_visual_rows` must OVER-count it into multiple
    /// rows so the seeded scroll content-height reaches the compositor's
    /// wrap-accurate block height (hud-3y7va).
    #[test]
    fn approx_wrapped_visual_rows_over_counts_soft_wrapped_line() {
        let wrap_width = 388.0_f32; // a 400px tile − 2×6px margin
        let long_line = "x".repeat(120);

        // Regression baseline: the pre-fix count was exactly one.
        assert_eq!(
            long_line.split('\n').count(),
            1,
            "a 120-char unbroken line has a single logical line"
        );

        let rows = approx_wrapped_visual_rows(
            &long_line,
            wrap_width,
            INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
            INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX,
        );
        assert!(
            rows > 1,
            "a 120-char unbroken line must be counted as >1 wrapped visual rows, got {rows}"
        );

        // Each hard-newline line contributes at least one row (empty lines too).
        assert!(
            approx_wrapped_visual_rows(
                "a\nb\nc",
                wrap_width,
                INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
                INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX,
            ) >= 3,
            "three hard-newline lines occupy at least three rows"
        );
        assert_eq!(
            approx_wrapped_visual_rows(
                "hi",
                wrap_width,
                INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
                INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX,
            ),
            1,
            "a short line occupies exactly one row"
        );
    }

    /// Full-width content (CJK / emoji) shapes to ~1em, far wider than the 0.75em
    /// narrow advance, so a flat narrow-only estimate over-counts columns-per-row
    /// and UNDER-counts wrapped rows — undershooting `real_max_scrollback` and
    /// clipping the newest reply (hud-kaw7z). The width-aware estimate must
    /// charge full-width glyphs the wider advance so the count stays an
    /// OVER-estimate of the compositor's real per-character CJK/emoji wrapping.
    #[test]
    fn approx_wrapped_visual_rows_over_counts_wide_glyphs() {
        let wrap_width = 388.0_f32; // a 400px tile − 2×6px margin

        // 60 full-width CJK ideographs on one logical line. The compositor breaks
        // CJK per-character (no word-wrap slack) at the real ~1em advance, so a
        // faithful lower bound on its rows is ceil(60 × 15 / 388).
        let cjk_line: String = "字".repeat(60);
        let compositor_rows =
            ((60.0 * INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX) / wrap_width).ceil() as usize;

        let width_aware = approx_wrapped_visual_rows(
            &cjk_line,
            wrap_width,
            INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
            INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX,
        );
        assert!(
            width_aware >= compositor_rows,
            "width-aware estimate {width_aware} must reach or exceed the \
             compositor's real CJK wrapping {compositor_rows} (never clip the tail)"
        );

        // The pre-fix flat narrow-only estimate (both advances = narrow) is the
        // regression: it under-counts the same content below the compositor rows.
        let narrow_only = approx_wrapped_visual_rows(
            &cjk_line,
            wrap_width,
            INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
            INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
        );
        assert!(
            narrow_only < compositor_rows,
            "regression baseline: a narrow-only estimate {narrow_only} under-counts \
             the compositor's CJK wrapping {compositor_rows}, clipping the tail"
        );
        assert!(
            width_aware > narrow_only,
            "the width-aware fix must count more rows than the narrow-only \
             estimate for full-width content ({width_aware} > {narrow_only})"
        );

        // Emoji are also width-2: a line of emoji must over-count the same way and
        // never collapse back to the narrow count.
        let emoji_line: String = "😀".repeat(60);
        let emoji_rows = approx_wrapped_visual_rows(
            &emoji_line,
            wrap_width,
            INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
            INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX,
        );
        assert!(
            emoji_rows >= compositor_rows,
            "an emoji-dominated line {emoji_rows} must over-count like CJK \
             {compositor_rows} (never clip the tail)"
        );

        // Narrow Latin text is unchanged — no wider scroll dead-zone for the
        // common case. A 60-char Latin line lands on the same count whether or not
        // the wide advance is in play.
        let latin_line = "x".repeat(60);
        assert_eq!(
            approx_wrapped_visual_rows(
                &latin_line,
                wrap_width,
                INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
                INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX,
            ),
            approx_wrapped_visual_rows(
                &latin_line,
                wrap_width,
                INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
                INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
            ),
            "narrow Latin text must be unaffected by the wide-glyph advance"
        );
    }

    /// The seeded band-height reference must stay strictly under the full tile
    /// height (so the seeded offset never undershoots the tail) and never
    /// collapse to zero for a tiny tile (hud-3y7va).
    #[test]
    fn input_history_band_height_under_estimates_tile_height() {
        let band = input_history_band_height_px(300.0, INPUT_HISTORY_APPROX_LINE_HEIGHT_PX);
        assert!(
            band < 300.0,
            "band {band} must stay under the full tile height (composer box excluded)"
        );
        assert!(band > 0.0, "band {band} must be positive");
        assert_eq!(
            input_history_band_height_px(5.0, INPUT_HISTORY_APPROX_LINE_HEIGHT_PX),
            INPUT_HISTORY_APPROX_LINE_HEIGHT_PX,
            "a tiny tile floors the reference viewport at one line"
        );
        let scaled_line_height = INPUT_HISTORY_APPROX_LINE_HEIGHT_PX * 1.4;
        assert!(
            (input_history_band_height_px(300.0, scaled_line_height)
                - (300.0 - scaled_line_height - INPUT_COMPOSER_TEXT_MARGIN_PX * 2.0))
                .abs()
                < 0.01,
            "a resized band must scale its line but retain the fixed 6px inset on each side"
        );
    }

    /// A whole-portal width grow also raises the viewer-local font scale.  The
    /// raw-tile history seed must scale every text-derived metric with that
    /// factor — line pitch, composer-box/band height, and narrow + full-width
    /// advances — while retaining the compositor's fixed 6px inset on each
    /// side.  Otherwise the tail seed falls below the compositor's exact
    /// `real_max_scrollback` and the newest reply is not bottom-aligned on the
    /// first resized frame (hud-t8c3h).
    #[test]
    fn resized_raw_history_tail_seed_scales_line_band_and_character_advances() {
        const SCALE: f32 = 1.4;
        const TILE_WIDTH_PX: f32 = 560.0;
        const TILE_HEIGHT_PX: f32 = 300.0;

        let (mut scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        {
            let tile = scene
                .tiles
                .get_mut(&tile_id)
                .expect("portal fixture must retain its tile");
            tile.bounds.width = TILE_WIDTH_PX;
            tile.bounds.height = TILE_HEIGHT_PX;
        }
        scene.set_tile_font_scale(tile_id, SCALE);
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        // Mixed narrow + full-width text makes an unscaled 0.75em/1em estimate
        // visibly under-count after the portal widens.  The prefix is included
        // because `append_raw_tile_viewer_echo` always timestamps this path.
        let text = format!("{}{}", "n".repeat(240), "字".repeat(120));
        let seed_text = format!("{INPUT_HISTORY_TIMESTAMP_PREFIX_STANDIN}{text}");
        let scaled_rows = approx_wrapped_visual_rows(
            &seed_text,
            input_history_wrap_width_px(TILE_WIDTH_PX),
            INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX * SCALE,
            INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX * SCALE,
        );
        let unscaled_rows = approx_wrapped_visual_rows(
            &seed_text,
            input_history_wrap_width_px(TILE_WIDTH_PX),
            INPUT_HISTORY_CONSERVATIVE_CHAR_ADVANCE_PX,
            INPUT_HISTORY_WIDE_CHAR_ADVANCE_PX,
        );
        assert!(
            scaled_rows > unscaled_rows,
            "test setup: a widened portal must require scaled narrow and wide advances \
             (scaled={scaled_rows}, unscaled={unscaled_rows})"
        );

        let scaled_line_height = INPUT_HISTORY_APPROX_LINE_HEIGHT_PX * SCALE;
        let expected_total_height = scaled_rows as f32 * scaled_line_height;
        // The compositor scales its text line/box but leaves the 6px content
        // inset fixed, so the band subtracts `scaled_line + 2*6`, not a scaled
        // 12px margin.
        let expected_band_height = (TILE_HEIGHT_PX
            - (scaled_line_height + INPUT_COMPOSER_TEXT_MARGIN_PX * 2.0))
            .max(scaled_line_height);
        let expected_tail_offset = (expected_total_height - expected_band_height).max(0.0);
        assert!(
            expected_tail_offset > 0.0,
            "test setup: scaled history must overflow its scaled input-history band"
        );

        app.append_raw_tile_viewer_echo(tile_id, text);

        let actual_total_height = app
            .state
            .input_processor
            .tile_total_content_height_px(tile_id);
        let (actual_offset, at_tail) = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            (
                scene.tile_scroll_offset_local(tile_id).1,
                scene.tile_follow_tail_at_tail(tile_id),
            )
        };
        assert!(
            at_tail,
            "a local submission must retain its follow-tail state"
        );
        assert!(
            (actual_total_height - expected_total_height).abs() < 0.01,
            "resized seed must charge scaled rows × scaled line height: \
             actual={actual_total_height}, expected={expected_total_height}"
        );
        assert!(
            (actual_offset - expected_tail_offset).abs() < 0.01,
            "resized tail reset must use the scaled composer band with an unscaled 6px inset: \
             actual={actual_offset}, expected={expected_tail_offset}"
        );
    }

    /// The runtime's conservative seed must use the same *effective* history
    /// font as the compositor, including the shared 9px/48px legibility clamp.
    /// A raw multiplier below the floor would under-seed a shrunken portal;
    /// above the ceiling it would drift from the rendered line pitch.
    #[test]
    fn input_history_seed_metrics_follow_portal_font_clamp() {
        let tokens = std::collections::HashMap::new();
        let min = InputHistorySeedMetrics::for_tile_font_scale(0.1, &tokens);
        let max = InputHistorySeedMetrics::for_tile_font_scale(10.0, &tokens);
        let min_font = tze_hud_scene::types::PORTAL_TEXT_MIN_FONT_PX_DEFAULT;
        let max_font = tze_hud_scene::types::PORTAL_TEXT_MAX_FONT_PX_DEFAULT;

        assert!(
            (min.line_height_px - min_font * 1.4).abs() < 0.01
                && (min.narrow_char_advance_px - min_font * 0.75).abs() < 0.01
                && (min.wide_char_advance_px - min_font).abs() < 0.01,
            "below the scale floor, every history metric must use the compositor's effective {min_font}px font: {min:?}"
        );
        assert!(
            (max.line_height_px - max_font * 1.4).abs() < 0.01
                && (max.narrow_char_advance_px - max_font * 0.75).abs() < 0.01
                && (max.wide_char_advance_px - max_font).abs() < 0.01,
            "above the scale ceiling, every history metric must use the compositor's effective {max_font}px font: {max:?}"
        );

        let profile_tokens = std::collections::HashMap::from([
            (
                tze_hud_scene::types::PORTAL_TEXT_MIN_FONT_PX_TOKEN.to_owned(),
                "13".to_owned(),
            ),
            (
                tze_hud_scene::types::PORTAL_TEXT_MAX_FONT_PX_TOKEN.to_owned(),
                "17".to_owned(),
            ),
        ]);
        let profile_max = InputHistorySeedMetrics::for_tile_font_scale(2.0, &profile_tokens);
        assert!(
            (profile_max.line_height_px - 17.0 * 1.4).abs() < 0.01
                && (profile_max.narrow_char_advance_px - 17.0 * 0.75).abs() < 0.01
                && (profile_max.wide_char_advance_px - 17.0).abs() < 0.01,
            "history metrics must honor the same profile clamp tokens as the compositor: {profile_max:?}"
        );
    }

    /// Existing raw-tile history is measured in physical pixels.  When the
    /// portal is resized, a later local submission must rebase that retained
    /// seed into the new effective line units before resetting to tail; adding
    /// only the newest entry in resized units leaves the old rows short and
    /// fails to reach the compositor's true tail (hud-t8c3h).
    #[test]
    fn resized_raw_history_rebases_existing_seed_before_tail_reset() {
        const RESIZED_SCALE: f32 = 1.4;
        const BASE_WIDTH_PX: f32 = 400.0;
        const RESIZED_WIDTH_PX: f32 = 560.0;
        const TILE_HEIGHT_PX: f32 = 300.0;

        let (mut scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        scene.tiles.get_mut(&tile_id).unwrap().bounds.width = BASE_WIDTH_PX;
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        let old_entries: Vec<String> = (0..4)
            .map(|_| format!("{}{}", "n".repeat(200), "字".repeat(80)))
            .collect();
        for entry in &old_entries {
            app.append_raw_tile_viewer_echo(tile_id, entry.clone());
        }

        {
            let shared = app.state.shared_state.try_lock().unwrap();
            let mut scene = shared.scene.try_lock().unwrap();
            scene.tiles.get_mut(&tile_id).unwrap().bounds.width = RESIZED_WIDTH_PX;
            scene.set_tile_font_scale(tile_id, RESIZED_SCALE);
        }
        let newest = format!("{}{}", "n".repeat(200), "字".repeat(80));
        app.append_raw_tile_viewer_echo(tile_id, newest.clone());

        let resized_metrics =
            InputHistorySeedMetrics::for_tile_font_scale(RESIZED_SCALE, &app.state.global_tokens);
        let wrap_width = input_history_wrap_width_px(RESIZED_WIDTH_PX);
        let expected_total_height: f32 = old_entries
            .iter()
            .chain(std::iter::once(&newest))
            .map(|entry| {
                let prefixed = format!("{INPUT_HISTORY_TIMESTAMP_PREFIX_STANDIN}{entry}");
                let rows = approx_wrapped_visual_rows(
                    &prefixed,
                    wrap_width,
                    resized_metrics.narrow_char_advance_px,
                    resized_metrics.wide_char_advance_px,
                );
                rows as f32 * resized_metrics.line_height_px
            })
            .sum();
        let expected_band_height =
            input_history_band_height_px(TILE_HEIGHT_PX, resized_metrics.line_height_px);
        let expected_tail_offset = (expected_total_height - expected_band_height).max(0.0);

        let actual_total_height = app
            .state
            .input_processor
            .tile_total_content_height_px(tile_id);
        let (actual_offset, at_tail) = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            (
                scene.tile_scroll_offset_local(tile_id).1,
                scene.tile_follow_tail_at_tail(tile_id),
            )
        };
        assert!(
            at_tail,
            "new local input must reset the resized history to tail"
        );
        assert!(
            actual_total_height >= expected_total_height,
            "existing history must be rebased into resized units before appending: \
             actual={actual_total_height}, expected_at_least={expected_total_height}"
        );
        assert!(
            actual_offset >= expected_tail_offset,
            "tail reset must reach the resized compositor tail: \
             actual={actual_offset}, expected_at_least={expected_tail_offset}"
        );
    }

    /// A severe width shrink can force retained CJK rows to wrap several times
    /// more even when the font itself is held at the compositor's minimum.
    /// Rebasing only the physical line pitch loses that reflow, so the next
    /// local echo must conservatively expand the old aggregate before tail reset.
    #[test]
    fn shrunk_clamped_raw_history_rebases_wrap_before_tail_reset() {
        const BASE_WIDTH_PX: f32 = 1_000.0;
        const SHRUNK_WIDTH_PX: f32 = 240.0;
        const SHRUNK_SCALE: f32 = 0.24;
        const TILE_HEIGHT_PX: f32 = 300.0;

        let (mut scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        scene.tiles.get_mut(&tile_id).unwrap().bounds.width = BASE_WIDTH_PX;
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        let old_entries: Vec<String> = (0..4).map(|_| "字".repeat(200)).collect();
        for entry in &old_entries {
            app.append_raw_tile_viewer_echo(tile_id, entry.clone());
        }

        {
            let shared = app.state.shared_state.try_lock().unwrap();
            let mut scene = shared.scene.try_lock().unwrap();
            scene.tiles.get_mut(&tile_id).unwrap().bounds.width = SHRUNK_WIDTH_PX;
            scene.set_tile_font_scale(tile_id, SHRUNK_SCALE);
        }
        let newest = "字".repeat(200);
        app.append_raw_tile_viewer_echo(tile_id, newest.clone());

        let metrics =
            InputHistorySeedMetrics::for_tile_font_scale(SHRUNK_SCALE, &app.state.global_tokens);
        assert!(
            (metrics.wide_char_advance_px - tze_hud_scene::types::PORTAL_TEXT_MIN_FONT_PX_DEFAULT)
                .abs()
                < 0.01,
            "test setup: the shrunken portal must be held at the 9px font floor"
        );
        let wrap_width = input_history_wrap_width_px(SHRUNK_WIDTH_PX);
        let expected_total_height: f32 = old_entries
            .iter()
            .chain(std::iter::once(&newest))
            .map(|entry| {
                let prefixed = format!("{INPUT_HISTORY_TIMESTAMP_PREFIX_STANDIN}{entry}");
                let rows = approx_wrapped_visual_rows(
                    &prefixed,
                    wrap_width,
                    metrics.narrow_char_advance_px,
                    metrics.wide_char_advance_px,
                );
                rows as f32 * metrics.line_height_px
            })
            .sum();
        let expected_band_height =
            input_history_band_height_px(TILE_HEIGHT_PX, metrics.line_height_px);
        let expected_tail_offset = (expected_total_height - expected_band_height).max(0.0);

        let actual_total_height = app
            .state
            .input_processor
            .tile_total_content_height_px(tile_id);
        let (actual_offset, at_tail) = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            (
                scene.tile_scroll_offset_local(tile_id).1,
                scene.tile_follow_tail_at_tail(tile_id),
            )
        };
        assert!(at_tail, "new local input must retain follow-tail state");
        assert!(
            actual_total_height >= expected_total_height,
            "retained rows must be rebased for the shrunken wrap width: \
             actual={actual_total_height}, expected_at_least={expected_total_height}"
        );
        assert!(
            actual_offset >= expected_tail_offset,
            "tail reset must reach the clamped-font shrunken history tail: \
             actual={actual_offset}, expected_at_least={expected_tail_offset}"
        );
    }

    /// Representative soft-wrapping history must seed a resting scroll offset that
    /// reaches or exceeds the compositor's `real_max_scrollback`, so the
    /// compositor clamp bottom-aligns EXACTLY to the tail rather than clipping the
    /// newest just-submitted lines at rest (hud-3y7va, the regression from #1045).
    ///
    /// Asserted at the offset/content-height/clamp layer against a REALISTIC,
    /// independent compositor model (average sans-serif glyph advance, resting
    /// 1-line composer box), not pixels.
    #[test]
    fn soft_wrapped_history_seeds_offset_past_compositor_real_max_scrollback() {
        let (scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        // `portal_scene_with_control` tile bounds are 400 × 300 (see its body).
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        // Long single-line replies (no hard newlines): each soft-wraps into
        // several visual rows the compositor lays out but `split('\n')` sees as
        // one — the exact case that regressed.
        let entries: Vec<String> = (0..10)
            .map(|i| format!("reply {i}: {}", "wrapping viewer text ".repeat(6)))
            .collect();
        for entry in &entries {
            app.append_raw_tile_viewer_echo(tile_id, entry.clone());
        }

        let seeded_offset = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            assert!(
                scene.tile_follow_tail_at_tail(tile_id),
                "a freshly-seeded history must rest at the tail"
            );
            scene.tile_scroll_offset_local(tile_id).1
        };

        // Compositor model (independent of the runtime's conservative estimators):
        // real_max_scrollback = wrap_accurate_block_height − band_height, and the
        // compositor bottom-aligns the tail only when the displayed offset reaches
        // that bound (see `input_history_block_top` / renderer/tests.rs
        // `input_history_block_honors_tile_scroll_offset`).
        const LINE_H: f32 = 21.0; // viewer-echo font 15 × line-height 1.4
        const REALISTIC_ADVANCE_PX: f32 = 7.5; // ≈ 15 × 0.5 average glyph advance
        let wrap_width = 400.0_f32 - 12.0; // tile width − 2×margin (viewer_echo_zone_width)
        let cols = (wrap_width / REALISTIC_ADVANCE_PX).floor().max(1.0) as usize;
        let realistic_rows: usize = entries
            .iter()
            .map(|e| {
                e.split('\n')
                    .map(|l| l.chars().count().max(1).div_ceil(cols))
                    .sum::<usize>()
            })
            .sum();
        let block_height = realistic_rows as f32 * LINE_H;
        let band_height = 300.0_f32 - (LINE_H + 12.0); // region top → 1-line composer box top
        let real_max_scrollback = (block_height - band_height).max(0.0);

        assert!(
            real_max_scrollback > 0.0,
            "test setup: the history must overflow the band (real_max={real_max_scrollback})"
        );
        assert!(
            seeded_offset >= real_max_scrollback,
            "seeded offset {seeded_offset} must reach or exceed the compositor's \
             real_max_scrollback {real_max_scrollback} so the tail bottom-aligns \
             exactly; a smaller offset clips the newest reply at rest (hud-3y7va)"
        );
    }

    /// CJK/emoji history shapes to ~1em per glyph — wider than the 0.75em narrow
    /// advance the seed used to assume — so the pre-fix estimate under-counted
    /// wrapped rows, undershot `real_max_scrollback`, and clipped the newest reply
    /// (hud-kaw7z, a narrow recurrence of the hud-3y7va tail-clip). The width-aware
    /// seed must still reach or exceed the compositor's real full-width wrapping.
    ///
    /// Asserted at the offset/content-height/clamp layer against an INDEPENDENT
    /// compositor model whose full-width advance is exactly one em (the defining
    /// advance of a full-width glyph), not against the runtime's own constants.
    #[test]
    fn cjk_history_seeds_offset_past_compositor_real_max_scrollback() {
        let (scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        // `portal_scene_with_control` tile bounds are 400 × 300 (see its body).
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        // Full-width CJK replies that soft-wrap: cosmic-text line-breaks CJK
        // per-character, so each of these overflows the wrap width into several
        // visual rows at the real ~1em advance.
        let entries: Vec<String> = (0..10).map(|_| "字".repeat(30)).collect();
        for entry in &entries {
            app.append_raw_tile_viewer_echo(tile_id, entry.clone());
        }

        let seeded_offset = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            assert!(
                scene.tile_follow_tail_at_tail(tile_id),
                "a freshly-seeded CJK history must rest at the tail"
            );
            scene.tile_scroll_offset_local(tile_id).1
        };

        // Compositor model (independent of the runtime's estimators): a full-width
        // glyph advances exactly one em, and CJK breaks per-character, so a
        // faithful lower bound on the wrapped rows is ceil(chars × em / wrap).
        const LINE_H: f32 = 21.0; // viewer-echo font 15 × line-height 1.4
        const FULLWIDTH_EM_PX: f32 = 15.0; // one em at the 15px viewer-echo font
        let wrap_width = 400.0_f32 - 12.0; // tile width − 2×margin (viewer_echo_zone_width)
        let realistic_rows: usize = entries
            .iter()
            .map(|e| ((e.chars().count() as f32 * FULLWIDTH_EM_PX) / wrap_width).ceil() as usize)
            .sum();
        let block_height = realistic_rows as f32 * LINE_H;
        let band_height = 300.0_f32 - (LINE_H + 12.0); // region top → 1-line composer box top
        let real_max_scrollback = (block_height - band_height).max(0.0);

        assert!(
            real_max_scrollback > 0.0,
            "test setup: the CJK history must overflow the band (real_max={real_max_scrollback})"
        );
        assert!(
            seeded_offset >= real_max_scrollback,
            "seeded CJK offset {seeded_offset} must reach or exceed the compositor's \
             real_max_scrollback {real_max_scrollback}; the pre-fix narrow-advance \
             seed undershot this and clipped the newest reply (hud-kaw7z)"
        );
    }

    /// The compositor prepends a fixed `HH:MM␠␠` timestamp prefix (7 narrow
    /// columns) to the FIRST line of every viewer-echo entry before wrapping
    /// (`viewer_echo_display_text`, hud-7ic89). A full-width entry sized to sit
    /// JUST under one wrap row on its own is spilled onto a SECOND row by that
    /// prefix. A prefix-blind seed (estimating rows from the raw entry text)
    /// under-counts by a row per entry, so the seeded offset undershoots the
    /// compositor's `real_max_scrollback` and the newest reply re-clips at rest —
    /// the exact hud-kaw7z failure the width-aware seed set out to eliminate.
    ///
    /// Asserted at the offset/clamp layer against an INDEPENDENT compositor model:
    /// 25 full-width glyphs advance 25 em (375px ≤ 388px wrap → one row alone),
    /// but the nonempty timestamp prefix (7 glyphs, far more than the 13px of
    /// remaining slack) forces a second row, so each entry occupies ≥ 2 rows.
    #[test]
    fn cjk_history_seed_accounts_for_timestamp_prefix_wrap() {
        // Lock the stand-in to the compositor's real 7-column prefix width.
        assert_eq!(
            INPUT_HISTORY_TIMESTAMP_PREFIX_STANDIN.chars().count(),
            7,
            "timestamp prefix stand-in must mirror the compositor's 7-column \
             `HH:MM␠␠` prefix"
        );

        let (scene, _tab_id, tile_id, _composer_id, _control_id) = portal_scene_with_control();
        // `portal_scene_with_control` tile bounds are 400 × 300 (see its body).
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());

        const LINE_H: f32 = 21.0; // viewer-echo font 15 × line-height 1.4
        const FULLWIDTH_EM_PX: f32 = 15.0; // one em at the 15px viewer-echo font
        let wrap_width = 400.0_f32 - 12.0; // tile width − 2×margin (viewer_echo_zone_width)

        // 25 full-width glyphs = 375px ≤ 388px: exactly one wrap row on their own,
        // with only 13px of slack — less than any nonempty prefix.
        const FULLWIDTH_CHARS: usize = 25;
        assert!(
            (FULLWIDTH_CHARS as f32 * FULLWIDTH_EM_PX) <= wrap_width,
            "test setup: {FULLWIDTH_CHARS} full-width glyphs must fit one row \
             WITHOUT the prefix"
        );
        let entries: Vec<String> = (0..20).map(|_| "字".repeat(FULLWIDTH_CHARS)).collect();
        for entry in &entries {
            app.append_raw_tile_viewer_echo(tile_id, entry.clone());
        }

        let seeded_offset = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            assert!(
                scene.tile_follow_tail_at_tail(tile_id),
                "a freshly-seeded CJK history must rest at the tail"
            );
            scene.tile_scroll_offset_local(tile_id).1
        };

        // Compositor model (independent of the runtime's estimators): each entry's
        // first line carries the prefix, so 25 full-width glyphs plus a nonempty
        // prefix exceed the 388px wrap and occupy at least 2 rows.
        let realistic_rows: usize = entries.len() * 2;
        let block_height = realistic_rows as f32 * LINE_H;
        let band_height = 300.0_f32 - (LINE_H + 12.0); // region top → 1-line composer box top
        let real_max_scrollback = (block_height - band_height).max(0.0);

        assert!(
            real_max_scrollback > 0.0,
            "test setup: the prefixed CJK history must overflow the band \
             (real_max={real_max_scrollback})"
        );
        assert!(
            seeded_offset >= real_max_scrollback,
            "seeded CJK offset {seeded_offset} must reach or exceed the compositor's \
             real_max_scrollback {real_max_scrollback}; a prefix-blind seed \
             under-counts each entry by a row and re-clips the newest reply \
             (hud-kaw7z timestamp-prefix recurrence)"
        );
    }

    /// An authority-attached submission routes to the ProjectionAuthority, so the
    /// raw-tile echo path (which resets on its own) is SKIPPED. The keyboard
    /// submit-terminal reset must still snap a scrolled-back input-history back to
    /// the tail so the just-submitted reply is revealed (hud-npcdf).
    #[test]
    fn authority_attached_submit_snaps_scrolled_back_history_to_tail() {
        use tze_hud_input::{FocusManager, InputProcessor, RawCharacterEvent};
        use tze_hud_projection::{
            AttachRequest, ContentClassification, OperationEnvelope, OutputKind,
            ProjectionOperation, ProviderKind, PublishOutputRequest,
        };
        use tze_hud_scene::types::HitRegionNode;
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

        // Attach a projection and drain a publish to MATERIALISE an
        // authority-backed portal tile (binding it in the driver). A submission on
        // this tile is routed to the authority, so `route_portal_composer_batch`
        // returns true and `append_raw_tile_viewer_echo` never runs — isolating
        // the keyboard submit-terminal reset as the only thing that can pin the
        // tail.
        let mut driver = crate::portal_projection_driver::InProcessPortalDriver::new();
        let projection_id = "proj-npcdf";
        let envelope = |op: ProjectionOperation, request_id: &str| OperationEnvelope {
            operation: op,
            projection_id: projection_id.to_string(),
            request_id: request_id.to_string(),
            client_timestamp_wall_us: 1,
        };
        let attach = driver.authority_mut().handle_attach(
            AttachRequest {
                envelope: envelope(ProjectionOperation::Attach, "attach-1"),
                provider_kind: ProviderKind::Claude,
                display_name: "Test".to_string(),
                workspace_hint: None,
                repository_hint: None,
                icon_profile_hint: None,
                content_classification: ContentClassification::Private,
                hud_target: None,
                idempotency_key: None,
            },
            "test-caller",
            1000,
        );
        assert!(attach.accepted, "attach must be accepted");
        let token = attach.owner_token.expect("owner_token after attach");
        driver.attach_projection(projection_id, Vec::new());

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        let published = driver.authority_mut().handle_publish_output(
            PublishOutputRequest {
                envelope: envelope(ProjectionOperation::PublishOutput, "pub-1"),
                owner_token: token.clone(),
                output_text: "assistant ready".to_string(),
                output_kind: OutputKind::Assistant,
                content_classification: ContentClassification::Private,
                logical_unit_id: Some("unit-1".to_string()),
                coalesce_key: None,
                expects_reply: false,
            },
            "test-caller",
            100,
        );
        assert!(published.accepted, "publish must be accepted");
        driver.drain(&mut scene, &mut processor, Some(tab_id));

        let tile_id = *scene
            .tiles
            .keys()
            .next()
            .expect("drain must materialise a portal tile");
        let tile_bounds = scene.tiles.get(&tile_id).unwrap().bounds;

        // Give the materialised tile a focusable composer so Enter routes to the
        // draft (the driver-created tile carries no composer node of its own).
        let composer_id = SceneId::new();
        scene.nodes.insert(
            composer_id,
            Node {
                layout: Default::default(),
                id: composer_id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 400.0, 60.0),
                    interaction_id: "portal-composer".to_string(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    accepts_composer_input: true,
                    ..Default::default()
                }),
            },
        );
        scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(composer_id);

        let mut focus_manager = FocusManager::new();
        focus_manager.add_tab(tab_id);
        processor.navigate_focus(&mut focus_manager, &mut scene, tab_id, false);
        assert!(
            processor.is_composer_active(),
            "test setup: focusing the composer must activate the draft"
        );

        // Seed an overflowing history so there is a nonzero scrollable range
        // (mirrors the authority echo growing the transcript).
        processor.notify_tile_content_appended(tile_id, 30.0 * 21.0, 200.0, 21.0, &mut scene);

        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, focus_manager, processor);
        app.state.portal_projection_driver = driver;

        // Type a character (non-empty draft required for a real submission). This
        // resets to the tail, matching a viewer actively drafting.
        app.dispatch_character_event_inner(
            &RawCharacterEvent {
                character: "h".to_string(),
                timestamp_mono_us: tze_hud_scene::MonoUs(1),
            },
            Some(tab_id),
        );

        // Scroll the viewer back up, away from the tail (a point in the tile body,
        // below the composer's 60px-tall hit region).
        {
            let shared = app.state.shared_state.try_lock().unwrap();
            let mut scene = shared.scene.try_lock().unwrap();
            let _ = app.state.input_processor.process_scroll_event(
                &tze_hud_input::ScrollEvent {
                    x: tile_bounds.x + 200.0,
                    y: tile_bounds.y + tile_bounds.height - 20.0,
                    delta_x: 0.0,
                    delta_y: -50.0,
                },
                &mut scene,
            );
            assert!(
                !scene.tile_follow_tail_at_tail(tile_id),
                "test setup: scrolling up must leave the tile ScrolledBack"
            );
        }

        // Submit with Enter. The submission routes to the authority (echo path
        // skipped), so only the keyboard submit-terminal reset can pin the tail.
        app.dispatch_key_down_event_inner(
            &RawKeyDownEvent {
                key_code: "Enter".to_string(),
                key: "Enter".to_string(),
                modifiers: tze_hud_input::KeyboardModifiers::NONE,
                repeat: false,
                timestamp_mono_us: tze_hud_scene::MonoUs(2),
            },
            Some(tab_id),
        );

        assert!(
            app.state.viewer_echo_queue.lock().unwrap().is_empty(),
            "an authority-attached submission must NOT enqueue a raw-tile echo — \
             confirming the tail reset came from the keyboard submit-terminal path, \
             not append_raw_tile_viewer_echo"
        );
        let shared = app.state.shared_state.try_lock().unwrap();
        let scene = shared.scene.try_lock().unwrap();
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "an authority-attached submission must snap the input-pane history back \
             to the tail on submit (hud-npcdf)"
        );
    }

    #[test]
    fn shell_reserved_ctrl_tab_does_not_resize_focused_portal() {
        use tze_hud_input::{InputProcessor, KeyboardModifiers};

        let (scene, tab_id, tile_id, focus_manager) = portal_scene_with_focus();
        let (mut app, _input_event_rx) =
            make_windowed_keyboard_test_app(scene, focus_manager, InputProcessor::new());

        let bounds = |app: &WinitApp| {
            let shared = app
                .state
                .shared_state
                .try_lock()
                .expect("shared state must be available during key dispatch test");
            let scene = shared
                .scene
                .try_lock()
                .expect("scene must be available during key dispatch test");
            scene.tiles.get(&tile_id).unwrap().bounds
        };

        let before = bounds(&app);
        app.dispatch_key_down_event_inner(
            &RawKeyDownEvent {
                key_code: "Tab".to_string(),
                key: "Tab".to_string(),
                modifiers: KeyboardModifiers {
                    ctrl: true,
                    ..KeyboardModifiers::NONE
                },
                repeat: false,
                timestamp_mono_us: tze_hud_scene::MonoUs(1),
            },
            Some(tab_id),
        );
        let after = bounds(&app);

        assert_eq!(
            after, before,
            "shell-reserved Ctrl+Tab must not be consumed as a portal resize hotkey"
        );
        assert!(
            app.state.portal_resize_states.is_empty(),
            "shell-reserved shortcut must not create portal resize state"
        );
    }

    #[test]
    fn ctrl_resize_hotkey_is_captured_by_safe_mode_before_resizing() {
        use std::sync::atomic::Ordering;

        use tze_hud_input::{FocusManager, InputProcessor, KeyboardModifiers};
        use tze_hud_scene::types::{HitRegionNode, TileScrollConfig};
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

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
        scene
            .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
            .unwrap();

        let composer_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: composer_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 400.0, 60.0),
                        interaction_id: "portal-composer".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        accepts_composer_input: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();
        let mut focus_manager = FocusManager::new();
        let (_input_result, transition) = processor.process_with_focus(
            &tze_hud_input::PointerEvent {
                x: 110.0,
                y: 110.0,
                kind: tze_hud_input::PointerEventKind::Down,
                device_id: 1,
                timestamp: None,
            },
            &mut scene,
            &mut focus_manager,
            tab_id,
        );
        assert!(
            transition.and_then(|t| t.gained).is_some(),
            "pointer down must focus the composer node"
        );

        let (mut app, mut input_event_rx) =
            make_windowed_keyboard_test_app(scene, focus_manager, processor);
        app.state.safe_mode_atomic.store(true, Ordering::Release);

        let bounds = |app: &WinitApp| {
            let shared = app
                .state
                .shared_state
                .try_lock()
                .expect("shared state must be available during key dispatch test");
            let scene = shared
                .scene
                .try_lock()
                .expect("scene must be available during key dispatch test");
            scene.tiles.get(&tile_id).unwrap().bounds
        };

        let before = bounds(&app);
        app.dispatch_key_down_event(&RawKeyDownEvent {
            key_code: "Equal".to_string(),
            key: "=".to_string(),
            modifiers: KeyboardModifiers {
                ctrl: true,
                ..KeyboardModifiers::NONE
            },
            repeat: false,
            timestamp_mono_us: tze_hud_scene::MonoUs(1),
        });
        let after = bounds(&app);

        assert_eq!(
            after, before,
            "safe mode must capture Ctrl+= before portal resize can mutate bounds"
        );
        assert!(
            app.state.portal_resize_states.is_empty(),
            "safe-mode-captured resize hotkey must not create resize state"
        );
        assert!(
            input_event_rx.try_recv().is_err(),
            "safe-mode-captured resize hotkey must not be forwarded to the agent"
        );
    }

    // ── Pointer-affordance portal resize ─────────────────────────────────────

    /// A pointer-down on a resize affordance starts the gesture:
    ///   - `gesture_active()` becomes true.
    ///   - An outcome (snapshot) is returned.
    ///   - The snapshot's `gesture_active` flag is true.
    ///   - Tile bounds are unchanged (clamped initial = current).
    #[test]
    fn pointer_down_on_affordance_starts_gesture() {
        let (mut scene, tab_id, tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Tile is at (100, 100, 400, 300). Affordance strip is 8px.
        // Pointer at (496, 250) is on the right edge (100+400-8 = 492 ≤ 496 ≤ 500).
        let event = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };

        let outcome = apply_portal_resize_pointer_event(
            &event,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            outcome.is_some(),
            "pointer-down on affordance must return an outcome"
        );
        let outcome = outcome.unwrap();
        assert_eq!(
            outcome.members[0].tile_id, tile_id,
            "outcome must reference the focused portal tile"
        );
        assert!(
            outcome.members[0].snapshot.gesture_active,
            "snapshot must have gesture_active=true on pointer-down"
        );

        // Resize state map must now have an entry with gesture_active=true.
        let resize_state = resize_states
            .get(&tile_id)
            .expect("resize state must be created on pointer-down");
        assert!(
            resize_state.gesture_active(),
            "gesture_active() must be true after pointer-down on affordance"
        );
    }

    /// A pointer-move during an active gesture applies local bounds immediately
    /// (local-first feedback), and the tile width grows as the pointer moves right.
    #[test]
    fn pointer_move_during_gesture_updates_tile_bounds_locally() {
        let (mut scene, tab_id, tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Start gesture on the right edge.
        let down = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &down,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        let width_before = scene.tiles[&tile_id].bounds.width;

        // Move pointer 20px to the right → right edge should grow by 20px.
        let mv = PointerEvent {
            x: 516.0,
            y: 250.0,
            kind: PointerEventKind::Move,
            device_id: 1,
            timestamp: None,
        };
        let outcome = apply_portal_resize_pointer_event(
            &mv,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            outcome.is_some(),
            "pointer-move during gesture must return an outcome"
        );
        let new_width = scene.tiles[&tile_id].bounds.width;
        assert!(
            new_width > width_before,
            "tile width must grow when pointer moves right on right edge: before={width_before}, after={new_width}"
        );
    }

    /// A pointer-up ends the gesture and broadcasts the final geometry snapshot.
    ///   - After pointer-up, `gesture_active()` becomes false.
    ///   - The final snapshot's `gesture_active` is false.
    ///   - Tile bounds reflect the final clamped position.
    ///   - A geometry event outcome is returned (for broadcasting).
    #[test]
    fn pointer_up_ends_gesture_and_returns_geometry_event() {
        let (mut scene, tab_id, tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Start gesture on the right edge.
        let down = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &down,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        // Move to establish a drag delta.
        let mv = PointerEvent {
            x: 530.0,
            y: 250.0,
            kind: PointerEventKind::Move,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &mv,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        // Release pointer.
        let up = PointerEvent {
            x: 530.0,
            y: 250.0,
            kind: PointerEventKind::Up,
            device_id: 1,
            timestamp: None,
        };
        let outcome = apply_portal_resize_pointer_event(
            &up,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            outcome.is_some(),
            "pointer-up must return a geometry event outcome"
        );
        let outcome = outcome.unwrap();
        assert!(
            !outcome.members[0].snapshot.gesture_active,
            "snapshot gesture_active must be false after pointer-up"
        );
        assert_eq!(
            outcome.members[0].tile_id, tile_id,
            "outcome tile_id must match the resized portal"
        );

        // gesture_active() must be false after the last device lifts.
        let resize_state = resize_states
            .get(&tile_id)
            .expect("resize state must exist");
        assert!(
            !resize_state.gesture_active(),
            "gesture_active() must be false after pointer-up"
        );

        // Final tile width should reflect the drag delta (496→530 = +34px on right edge).
        let final_width = scene.tiles[&tile_id].bounds.width;
        assert!(
            final_width > 400.0,
            "tile width must be larger than initial 400px after rightward drag: {final_width}"
        );
    }

    /// Move/up routing must find the active resize gesture for the current
    /// `device_id`, not merely the first portal that has any active gesture.
    #[test]
    fn multi_device_resize_move_and_up_route_to_matching_device_gesture() {
        use tze_hud_scene::Rect;
        use tze_hud_scene::types::TileScrollConfig;

        let (mut scene, tab_id, tile_a, mut fm) = portal_scene_with_focus();
        let lease_b = scene.grant_lease("portal-agent-b", 60_000);
        let tile_b = scene
            .create_tile(
                tab_id,
                "portal-agent-b",
                lease_b,
                Rect::new(700.0, 100.0, 400.0, 300.0),
                2,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(tile_b, TileScrollConfig::vertical())
            .unwrap();

        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;
        let tokens = PortalWindowTokens::default();

        let down_a = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        let outcome_a = apply_portal_resize_pointer_event(
            &down_a,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("device 1 down must start a resize on tile A");
        assert_eq!(outcome_a.members[0].tile_id, tile_a);

        let (focus_result, _) = fm.request_focus(
            FocusRequest {
                tile_id: tile_b,
                node_id: None,
                steal: true,
                requesting_namespace: "portal-agent-b".to_string(),
            },
            tab_id,
            &scene,
        );
        assert_eq!(
            focus_result,
            tze_hud_input::FocusResult::Granted,
            "test setup must focus the second portal tile"
        );

        let down_b = PointerEvent {
            x: 1096.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 2,
            timestamp: None,
        };
        let outcome_b = apply_portal_resize_pointer_event(
            &down_b,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("device 2 down must start a resize on tile B");
        assert_eq!(outcome_b.members[0].tile_id, tile_b);

        let active_order = resize_states
            .iter()
            .filter(|(_, state)| state.gesture_active())
            .map(|(&tile_id, _)| tile_id)
            .collect::<Vec<_>>();
        assert_eq!(
            active_order.len(),
            2,
            "test setup must have two concurrently active portal resize gestures"
        );

        let first_iterated_tile = active_order[0];
        let target_tile = active_order[1];
        let (target_device_id, target_left_x, other_tile) = if target_tile == tile_a {
            (1, 100.0, tile_b)
        } else {
            (2, 700.0, tile_a)
        };

        let target_width_before = scene.tiles[&target_tile].bounds.width;
        let other_width_before = scene.tiles[&other_tile].bounds.width;
        let mv = PointerEvent {
            x: target_left_x + 430.0,
            y: 250.0,
            kind: PointerEventKind::Move,
            device_id: target_device_id,
            timestamp: None,
        };
        let move_outcome = apply_portal_resize_pointer_event(
            &mv,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("move must find the portal whose gesture belongs to this device");

        assert_eq!(
            move_outcome.members[0].tile_id, target_tile,
            "move must update the active portal for the current device"
        );
        assert!(
            scene.tiles[&target_tile].bounds.width > target_width_before,
            "target portal width must grow after moving its right-edge gesture"
        );
        assert_eq!(
            scene.tiles[&other_tile].bounds.width, other_width_before,
            "move for one device must not mutate the other active portal"
        );

        let up = PointerEvent {
            x: target_left_x + 430.0,
            y: 250.0,
            kind: PointerEventKind::Up,
            device_id: target_device_id,
            timestamp: None,
        };
        let up_outcome = apply_portal_resize_pointer_event(
            &up,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("up must end the portal gesture for the current device");

        assert_eq!(
            up_outcome.members[0].tile_id, target_tile,
            "up must end the active portal for the current device"
        );
        assert!(
            !resize_states[&target_tile].gesture_active(),
            "target portal gesture must end after pointer-up"
        );
        assert!(
            resize_states[&first_iterated_tile].gesture_active(),
            "the other portal gesture must remain active"
        );
    }

    /// gesture_active() is true during the drag, making adapter publishes
    /// rejectable — the primary reason this code path must exist in production
    /// (hud-o0st9 acceptance criterion: gesture_active becomes reachable).
    #[test]
    fn gesture_active_is_true_during_pointer_drag() {
        let (mut scene, tab_id, tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Before any gesture, no entry exists — gesture_active is implicitly false.
        assert!(
            resize_states
                .get(&tile_id)
                .is_none_or(|s: &PortalResizeState| !s.gesture_active()),
            "gesture must not be active before pointer-down"
        );

        let down = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &down,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            resize_states[&tile_id].gesture_active(),
            "gesture_active() must be true between pointer-down and pointer-up"
        );

        let up = PointerEvent {
            x: 510.0,
            y: 250.0,
            kind: PointerEventKind::Up,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &up,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            !resize_states[&tile_id].gesture_active(),
            "gesture_active() must be false after pointer-up"
        );
    }

    /// A pointer-down outside any affordance (in the content area) must NOT
    /// start a gesture.
    #[test]
    fn pointer_down_in_content_area_does_not_start_gesture() {
        let (mut scene, tab_id, tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Tile is at (100, 100, 400, 300). Content area center is at (300, 250).
        let event = PointerEvent {
            x: 300.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };

        let outcome = apply_portal_resize_pointer_event(
            &event,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            outcome.is_none(),
            "pointer-down in content area must not start a gesture"
        );
        assert!(
            resize_states
                .get(&tile_id)
                .is_none_or(|s| !s.gesture_active()),
            "no gesture must be active after content-area pointer-down"
        );
    }

    // ── Lease-bound maxima (hud-kgu8u / hud-zleu2) ───────────────────────────

    /// `compute_portal_max_dims` with no lease constraint (0.0) uses the full
    /// display dimensions, not `display_w - tile_x`.
    ///
    /// A portal at (100, 100) on a 1920×1080 display must be allowed to grow
    /// up to 1920×1080 — `PortalRect::clamped` shifts the origin as needed to
    /// keep the portal on-screen.
    #[test]
    fn compute_portal_max_dims_unconstrained_uses_display_boundary() {
        // 0.0 = unconstrained (no lease spatial budget)
        let (max_w, max_h) = compute_portal_max_dims(0.0, 0.0, 1920.0, 1080.0, 50.0, 50.0);

        // Display boundary: full display width/height, not display - tile.origin.
        assert_eq!(
            max_w, 1920.0,
            "unconstrained width must equal display_w (not display_w - tile_x)"
        );
        assert_eq!(
            max_h, 1080.0,
            "unconstrained height must equal display_h (not display_h - tile_y)"
        );
    }

    /// A portal at x=500 on a 1000px-wide display must be allowed to grow to
    /// 1000px wide.  `PortalRect::clamped` shifts the origin to x=0 so the
    /// right edge lands at the screen edge.  The pre-fix code produced 500px
    /// (display_w - tile.x), capping the portal at half the screen width.
    #[test]
    fn compute_portal_max_dims_uses_display_w_not_display_w_minus_tile_x() {
        // Scenario from PR #691 review: 1000px-wide display; the function must
        // return the full display width, not display_w - tile.x.
        let (max_w, max_h) = compute_portal_max_dims(0.0, 0.0, 1000.0, 800.0, 50.0, 50.0);

        assert_eq!(
            max_w, 1000.0,
            "portal at x=500 on 1000px display must be allowed to reach full display width (1000), \
             not be capped at display_w - tile.x = 500"
        );
        assert_eq!(
            max_h, 800.0,
            "portal at y=200 on 800px display must be allowed to reach full display height (800), \
             not be capped at display_h - tile.y = 600"
        );
    }

    /// When the lease budget is tighter than the display boundary, the lease
    /// budget wins (most-restrictive policy).
    #[test]
    fn compute_portal_max_dims_lease_budget_is_more_restrictive() {
        let (max_w, max_h) = compute_portal_max_dims(500.0, 400.0, 1920.0, 1080.0, 50.0, 50.0);

        assert_eq!(
            max_w, 500.0,
            "lease budget (500) must win over display boundary (1920)"
        );
        assert_eq!(
            max_h, 400.0,
            "lease budget (400) must win over display boundary (1080)"
        );
    }

    /// When the display boundary is tighter than the lease budget, the display
    /// boundary wins.
    #[test]
    fn compute_portal_max_dims_display_boundary_is_more_restrictive() {
        // Lease budget larger than the display; display boundary must be the
        // binding constraint.
        let (max_w, max_h) = compute_portal_max_dims(5000.0, 5000.0, 1920.0, 1080.0, 50.0, 50.0);

        // Display boundary is the full display, not display - tile.origin.
        assert_eq!(
            max_w, 1920.0,
            "display boundary (1920) must win over lease budget (5000)"
        );
        assert_eq!(
            max_h, 1080.0,
            "display boundary (1080) must win over lease budget (5000)"
        );
    }

    /// A lease budget smaller than the token minimum is floored to the minimum
    /// so a portal is always growable to at least the token minimum.
    #[test]
    fn compute_portal_max_dims_lease_budget_floored_to_token_minimum() {
        let (max_w, max_h) = compute_portal_max_dims(10.0, 10.0, 1920.0, 1080.0, 50.0, 50.0);

        assert_eq!(
            max_w, 50.0,
            "budget smaller than min_width must be floored to min_width"
        );
        assert_eq!(
            max_h, 50.0,
            "budget smaller than min_height must be floored to min_height"
        );
    }

    /// Portal resize gesture respects the lease-bound maximum:
    /// dragging the right edge beyond the lease limit is clamped to the limit.
    #[test]
    fn pointer_drag_respects_lease_bound_maximum() {
        use tze_hud_input::FocusManager;
        use tze_hud_scene::{Rect, SceneGraph};

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        // Tile at (100, 100, 400, 300) — starts at 400×300.
        let tile_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(100.0, 100.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        // Set a tight lease budget on the authoritative lease entry:
        // max 450 wide, 350 tall.
        if let Some(lease) = scene.leases.get_mut(&lease_id) {
            lease.spatial_budget.max_tile_width_px = 450.0;
            lease.spatial_budget.max_tile_height_px = 350.0;
        }
        scene
            .register_tile_scroll_config(
                tile_id,
                tze_hud_scene::types::TileScrollConfig::vertical(),
            )
            .unwrap();

        let mut fm = FocusManager::new();
        fm.request_focus(
            tze_hud_input::FocusRequest {
                tile_id,
                node_id: None,
                steal: true,
                requesting_namespace: "portal-agent".to_string(),
            },
            tab_id,
            &scene,
        );

        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Start gesture on right edge (100+400-8=492 ≤ 496 ≤ 500).
        let down = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &down,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        // Drag far to the right — would grow the tile to >> 450px without a lease clamp.
        let mv = PointerEvent {
            x: 800.0,
            y: 250.0,
            kind: PointerEventKind::Move,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &mv,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        let width_after_drag = scene.tiles[&tile_id].bounds.width;
        assert!(
            width_after_drag <= 450.0,
            "tile width must be clamped to lease budget 450.0, got {width_after_drag}"
        );
    }

    // ── portal_resize_states map pruning (hud-kgu8u) ─────────────────────────

    /// `prune_portal_resize_states` removes the entry for a tile that was
    /// deleted from the scene. Verifies: entry present before deletion,
    /// entry absent after prune.
    ///
    /// This test drives `prune_portal_resize_states` directly via a thin
    /// harness because the method requires a `WinitApp` (holds scene + map).
    /// We use the existing scene helpers and a manually constructed state to
    /// avoid spinning up a full winit event loop.
    #[test]
    fn portal_resize_state_pruned_after_tile_removal() {
        use tze_hud_scene::{Rect, SceneGraph};

        // Build a minimal scene with one portal tile.
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

        // Simulate a resize-state entry that accumulated for this tile.
        let mut portal_resize_states: std::collections::HashMap<
            tze_hud_scene::SceneId,
            PortalResizeState,
        > = std::collections::HashMap::new();
        portal_resize_states.insert(
            tile_id,
            PortalResizeState::new(tile_id.as_uuid().as_u128() as u64),
        );
        assert!(
            portal_resize_states.contains_key(&tile_id),
            "entry must be present before tile removal"
        );

        // Remove the tile from the scene (simulates DeleteTile mutation).
        scene.tiles.remove(&tile_id);
        assert!(
            !scene.tiles.contains_key(&tile_id),
            "tile must be absent from scene after removal"
        );

        // Prune: entries for absent tiles must be removed.
        let before = portal_resize_states.len();
        portal_resize_states.retain(|id, _| scene.tiles.contains_key(id));
        let removed = before - portal_resize_states.len();

        assert_eq!(removed, 1, "exactly one stale entry must be pruned");
        assert!(
            !portal_resize_states.contains_key(&tile_id),
            "entry for removed tile must be gone after pruning"
        );
    }

    /// `prune_portal_resize_states` preserves entries for tiles that still
    /// exist in the scene.
    #[test]
    fn portal_resize_state_preserved_for_live_tiles() {
        use tze_hud_scene::{Rect, SceneGraph};

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        let tile_a = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(0.0, 0.0, 300.0, 200.0),
                1,
            )
            .unwrap();
        let tile_b = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(400.0, 0.0, 300.0, 200.0),
                2,
            )
            .unwrap();

        let mut portal_resize_states: std::collections::HashMap<
            tze_hud_scene::SceneId,
            PortalResizeState,
        > = std::collections::HashMap::new();
        portal_resize_states.insert(
            tile_a,
            PortalResizeState::new(tile_a.as_uuid().as_u128() as u64),
        );
        portal_resize_states.insert(
            tile_b,
            PortalResizeState::new(tile_b.as_uuid().as_u128() as u64),
        );

        // Remove only tile_b from the scene.
        scene.tiles.remove(&tile_b);

        // Prune.
        portal_resize_states.retain(|id, _| scene.tiles.contains_key(id));

        assert!(
            portal_resize_states.contains_key(&tile_a),
            "entry for still-live tile_a must be retained"
        );
        assert!(
            !portal_resize_states.contains_key(&tile_b),
            "entry for removed tile_b must be pruned"
        );
        assert_eq!(
            portal_resize_states.len(),
            1,
            "exactly one entry (tile_a) must remain"
        );
    }

    /// Verifies the eager drain-based `portal_resize_states` cleanup path
    /// introduced by hud-4tuw5.
    ///
    /// `SceneGraph::drain_removed_tile_ids` yields the IDs of tiles removed via
    /// `remove_tile_and_nodes`; `prune_portal_resize_states` then removes each
    /// returned ID from the map.  This test drives that contract directly
    /// without the windowed event loop, matching the style of the existing
    /// sweep-based prune tests above.
    ///
    /// The `remove_tile_and_nodes` → drain queue half of the contract is
    /// exercised by `portal_resize_drain_queue_populated_by_remove_tile` in
    /// `tze_hud_scene` (where the function is visible).
    #[test]
    fn portal_resize_state_pruned_via_drain_queue_on_tile_removal() {
        use tze_hud_scene::{Rect, SceneGraph};

        // Build a minimal scene with one portal tile.
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

        // Simulate a resize-state entry accumulated for this portal tile.
        let mut portal_resize_states: std::collections::HashMap<
            tze_hud_scene::SceneId,
            PortalResizeState,
        > = std::collections::HashMap::new();
        portal_resize_states.insert(
            tile_id,
            PortalResizeState::new(tile_id.as_uuid().as_u128() as u64),
        );
        assert!(
            portal_resize_states.contains_key(&tile_id),
            "entry must be present before tile removal"
        );

        // Sanity: drain queue is empty before any removal.
        assert!(
            scene.drain_removed_tile_ids().is_empty(),
            "drain queue must be empty before any tile removal"
        );

        // Simulate what remove_tile_and_nodes does: remove from tiles map and
        // enqueue in recently_removed_tile_ids.  (remove_tile_and_nodes is
        // pub(crate) in tze_hud_scene; we cannot call it directly here.)
        scene.tiles.remove(&tile_id);
        scene.overlay.recently_removed_tile_ids.push(tile_id);

        // Drain the removal queue — this mirrors what prune_portal_resize_states does.
        let removed_ids = scene.drain_removed_tile_ids();
        assert_eq!(
            removed_ids,
            vec![tile_id],
            "drain queue must yield exactly the removed tile ID"
        );

        // Apply the drain to the portal_resize_states map.
        for id in &removed_ids {
            portal_resize_states.remove(id);
        }

        assert!(
            !portal_resize_states.contains_key(&tile_id),
            "portal_resize_states entry must be gone after drain-based pruning (hud-4tuw5)"
        );
        assert!(
            portal_resize_states.is_empty(),
            "no stale entries must remain after drain"
        );

        // Confirm the drain queue is now empty (idempotent).
        assert!(
            scene.drain_removed_tile_ids().is_empty(),
            "drain queue must be empty after drain"
        );
    }

    // ── scene.version increment on pointer-drag resize (hud-g1hij) ───────────

    /// A pointer-drag resize (on_pointer_move → GestureUpdate) increments
    /// `scene.version` exactly once when the tile bounds change, matching the
    /// hotkey resize path (hud-ghhxa / hud-g1hij).
    ///
    /// Regression test — the 20Hz re-prime cadence gate in
    /// `prime_truncation_cache` is keyed on `scene.version`; if the drag path
    /// did NOT bump it, mid-drag re-truncation would never fire.
    ///
    /// Also verifies that a pointer-move at the identical position (no size
    /// delta) does NOT bump the version a second time, preventing spurious
    /// cache invalidations when the gesture is clamped at a boundary.
    #[test]
    fn drag_resize_pointer_move_bumps_scene_version_on_size_change() {
        let (mut scene, tab_id, _tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Pointer-down on the right-edge affordance (x=496 hits the 8px strip
        // of a tile whose right edge is at 100+400=500).  No version bump expected
        // on gesture start (clamped initial rect is identical to current rect).
        let version_before_down = scene.version;
        let down = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &down,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );
        assert_eq!(
            scene.version, version_before_down,
            "scene.version must NOT change on gesture start (bounds unchanged at down)"
        );

        // Pointer-move 20px right → right edge grows, size changes.
        // scene.version must increment exactly once.
        let version_before_move = scene.version;
        let mv_grow = PointerEvent {
            x: 516.0,
            y: 250.0,
            kind: PointerEventKind::Move,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &mv_grow,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );
        assert_eq!(
            scene.version,
            version_before_move + 1,
            "scene.version must increment exactly once when drag resize changes tile size"
        );

        // Pointer-move at the exact same position (no delta) — the tile size
        // is already at the value from the previous move, so `size_changed` is
        // false and the version must NOT advance again.
        let version_before_noop = scene.version;
        let mv_noop = PointerEvent {
            x: 516.0,
            y: 250.0,
            kind: PointerEventKind::Move,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &mv_noop,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );
        assert_eq!(
            scene.version, version_before_noop,
            "scene.version must NOT change when pointer-move produces no size delta"
        );
    }

    // ── Whole-portal resize (hud-fb3en) ──────────────────────────────────────

    /// Build a multi-surface portal (frame + transcript pane + composer pane +
    /// far-corner drag shield) sharing one lease, with keyboard focus on the
    /// composer/input pane — the exact live configuration that made Ctrl resize
    /// scale only the focused pane. Returns (scene, tab, frame, transcript,
    /// composer, drag_shield, focus_manager).
    #[allow(clippy::type_complexity)]
    fn multi_surface_portal_scene() -> (
        tze_hud_scene::graph::SceneGraph,
        tze_hud_scene::SceneId,
        tze_hud_scene::SceneId,
        tze_hud_scene::SceneId,
        tze_hud_scene::SceneId,
        tze_hud_scene::SceneId,
        FocusManager,
    ) {
        use tze_hud_scene::types::TileScrollConfig;
        use tze_hud_scene::{Rect, SceneGraph};

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        // Frame is the portal-sized anchor (largest area, not scrollable).
        let frame_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(100.0, 100.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        // Transcript (output) pane — a scrollable surface inside the frame.
        let transcript_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(110.0, 110.0, 180.0, 280.0),
                3,
            )
            .unwrap();
        // Composer (input) pane — a scrollable surface inside the frame.
        let composer_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(300.0, 110.0, 190.0, 280.0),
                3,
            )
            .unwrap();
        // Drag shield — parked in the far display corner, NOT scrollable, NOT
        // spatially part of the portal frame.
        let shield_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(1900.0, 1070.0, 1.0, 1.0),
                20,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(transcript_id, TileScrollConfig::vertical())
            .unwrap();
        scene
            .register_tile_scroll_config(composer_id, TileScrollConfig::vertical())
            .unwrap();

        // Focus the composer/input pane — the live trigger for the bug.
        let mut fm = FocusManager::new();
        fm.request_focus(
            FocusRequest {
                tile_id: composer_id,
                node_id: None,
                steal: true,
                requesting_namespace: "portal-agent".to_string(),
            },
            tab_id,
            &scene,
        );

        (
            scene,
            tab_id,
            frame_id,
            transcript_id,
            composer_id,
            shield_id,
            fm,
        )
    }

    /// Relative geometry of a surface `b` expressed as fractions of the frame
    /// `f`: (x offset, y offset, width, height) all divided by the frame rect.
    /// Two surfaces share a portal layout iff these tuples are (approx) equal
    /// before and after a resize.
    fn rel_to_frame(b: tze_hud_scene::Rect, f: tze_hud_scene::Rect) -> (f32, f32, f32, f32) {
        (
            (b.x - f.x) / f.width,
            (b.y - f.y) / f.height,
            b.width / f.width,
            b.height / f.height,
        )
    }

    fn approx_tuple(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-3
            && (a.1 - b.1).abs() < 1e-3
            && (a.2 - b.2).abs() < 1e-3
            && (a.3 - b.3).abs() < 1e-3
    }

    /// Core hud-fb3en regression: focusing the composer and pressing Ctrl+= must
    /// grow the WHOLE portal — every constituent surface scales together,
    /// preserving relative layout, anchored top-left — not just the focused
    /// pane. The far-corner drag shield must NOT move.
    #[test]
    fn ctrl_resize_hotkey_scales_whole_portal_not_just_focused_surface() {
        use tze_hud_input::{HotkeyResizeDir, InputProcessor};

        let (scene, tab_id, frame_id, transcript_id, composer_id, shield_id, fm) =
            multi_surface_portal_scene();
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        let read = |app: &WinitApp, id: tze_hud_scene::SceneId| {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&id).unwrap().bounds
        };

        let frame_before = read(&app, frame_id);
        let composer_before = read(&app, composer_id);
        let transcript_before = read(&app, transcript_id);
        let shield_before = read(&app, shield_id);

        let consumed = app.apply_portal_resize_hotkey(
            tab_id,
            HotkeyResizeDir::Grow,
            tze_hud_input::HotkeyResizeAxis::Both,
        );
        assert!(
            consumed,
            "Ctrl resize hotkey must be consumed for a focused portal surface"
        );

        let frame_after = read(&app, frame_id);
        let composer_after = read(&app, composer_id);
        let transcript_after = read(&app, transcript_id);
        let shield_after = read(&app, shield_id);

        // Frame (anchor) grew, top-left anchored (origin fixed).
        assert!(
            frame_after.width > frame_before.width && frame_after.height > frame_before.height,
            "the portal frame must grow"
        );
        assert_eq!(
            (frame_after.x, frame_after.y),
            (frame_before.x, frame_before.y),
            "grow must be anchored at the frame's top-left corner"
        );

        // Both panes scaled with the portal — not left in place, not resized in
        // isolation.
        assert!(
            composer_after.width > composer_before.width
                && composer_after.height > composer_before.height,
            "the focused composer pane must scale WITH the whole portal"
        );
        assert!(
            transcript_after.width > transcript_before.width
                && transcript_after.height > transcript_before.height,
            "the transcript pane must scale WITH the whole portal"
        );

        // Relative layout preserved for every constituent surface.
        assert!(
            approx_tuple(
                rel_to_frame(composer_before, frame_before),
                rel_to_frame(composer_after, frame_after)
            ),
            "composer must keep its relative position/size within the portal"
        );
        assert!(
            approx_tuple(
                rel_to_frame(transcript_before, frame_before),
                rel_to_frame(transcript_after, frame_after)
            ),
            "transcript must keep its relative position/size within the portal"
        );

        // The spatially-detached drag shield is not a spatial member of the
        // portal frame and must not scale or move.
        assert_eq!(
            shield_after, shield_before,
            "the far-corner drag shield must not scale/move with a portal resize"
        );
    }

    // ── Directional (Ctrl+Shift+Arrow) whole-portal WIDTH resize (hud-csrmf) ──

    /// Attach a tile-filling `TextMarkdown` root node to `tile_id` at
    /// `local_bounds`, so a node-tree scale (the compositor's wrap-width source)
    /// is observable after a resize. Returns the node id.
    fn attach_text_node(
        scene: &mut tze_hud_scene::graph::SceneGraph,
        tile_id: tze_hud_scene::SceneId,
        local_bounds: tze_hud_scene::Rect,
    ) -> tze_hud_scene::SceneId {
        use tze_hud_scene::types::{
            FontFamily, Node, NodeData, Rgba, TextAlign, TextMarkdownNode, TextOverflow,
        };
        let id = tze_hud_scene::SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id,
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "the quick brown fox jumps over the lazy dog".to_owned(),
                        bounds: local_bounds,
                        font_size_px: 14.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
            .unwrap();
        id
    }

    fn node_width(app: &WinitApp, node_id: tze_hud_scene::SceneId) -> f32 {
        use tze_hud_scene::types::NodeData;
        let shared = app.state.shared_state.try_lock().unwrap();
        let scene = shared.scene.try_lock().unwrap();
        match &scene.nodes.get(&node_id).unwrap().data {
            NodeData::TextMarkdown(tm) => tm.bounds.width,
            other => panic!("expected TextMarkdown, got {other:?}"),
        }
    }

    /// (a) Ctrl+Shift+ArrowRight width resize scales the whole-portal tile bounds
    /// AND the node tree along WIDTH ONLY, takes viewer geometry authority over
    /// every member, and leaves height untouched. Width is the text-wrap axis, so
    /// this is exactly what drives the dynamic hud-rpmwt re-wrap without pointer
    /// injection.
    #[test]
    fn ctrl_shift_arrow_width_resize_scales_whole_portal_and_locks() {
        use tze_hud_input::{HotkeyResizeAxis, HotkeyResizeDir, InputProcessor};

        let (mut scene, tab_id, frame_id, transcript_id, composer_id, _shield_id, fm) =
            multi_surface_portal_scene();
        // Transcript pane is 180×280 (tile-local node fills it).
        let node_id = attach_text_node(
            &mut scene,
            transcript_id,
            tze_hud_scene::Rect::new(0.0, 0.0, 180.0, 280.0),
        );
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        let read = |app: &WinitApp, id: tze_hud_scene::SceneId| {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&id).unwrap().bounds
        };

        let frame_before = read(&app, frame_id);
        let transcript_before = read(&app, transcript_id);
        let node_w_before = node_width(&app, node_id);

        let consumed =
            app.apply_portal_resize_hotkey(tab_id, HotkeyResizeDir::Grow, HotkeyResizeAxis::Width);
        assert!(
            consumed,
            "width hotkey must be consumed for a focused portal"
        );

        let frame_after = read(&app, frame_id);
        let transcript_after = read(&app, transcript_id);
        let node_w_after = node_width(&app, node_id);

        // Frame grew in WIDTH only, top-left anchored.
        assert!(
            frame_after.width > frame_before.width,
            "frame width must grow"
        );
        assert_eq!(
            frame_after.height, frame_before.height,
            "width-axis resize must not change the frame height"
        );
        assert_eq!(
            (frame_after.x, frame_after.y),
            (frame_before.x, frame_before.y),
            "grow must stay anchored at the frame top-left"
        );

        // Transcript pane scaled width, kept height.
        assert!(
            transcript_after.width > transcript_before.width,
            "transcript pane width must scale with the whole portal"
        );
        assert_eq!(
            transcript_after.height, transcript_before.height,
            "transcript pane height must not change on a width-axis resize"
        );

        // Node tree scaled by the same WIDTH ratio → the compositor's wrap column
        // tracks the resized pane.
        assert!(
            node_w_after > node_w_before,
            "the tile's text node width must scale with the pane (wrap-column authority)"
        );

        // Viewer geometry authority taken over every member so a republish cannot
        // stomp them back (hud-lyqun) — the precondition the reconcile fix needs.
        for id in [frame_id, transcript_id, composer_id] {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            assert!(
                scene.is_viewer_geometry_locked(id),
                "every portal member must be viewer-geometry-locked after a width resize"
            );
        }
    }

    /// (b) After a Ctrl+Shift+ArrowLeft width shrink, an adapter content republish
    /// with STALE attach-time node bounds must be reconciled to the narrower pane
    /// — the exact hud-rpmwt reconcile-on-republish path, now reached via the
    /// keyboard width chord instead of a pointer drag.
    #[test]
    fn ctrl_shift_arrow_width_resize_then_republish_reconciles_to_narrower_pane() {
        use tze_hud_input::{HotkeyResizeAxis, HotkeyResizeDir, InputProcessor};

        let (mut scene, tab_id, _frame_id, transcript_id, _composer_id, _shield_id, fm) =
            multi_surface_portal_scene();
        let attach = tze_hud_scene::Rect::new(0.0, 0.0, 180.0, 280.0);
        let _first = attach_text_node(&mut scene, transcript_id, attach);
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        // Width SHRINK: one 32px step narrows the 400px frame → the 180px
        // transcript pane drops below its attach-time node width.
        assert!(
            app.apply_portal_resize_hotkey(
                tab_id,
                HotkeyResizeDir::Shrink,
                HotkeyResizeAxis::Width
            ),
            "width shrink hotkey must be consumed"
        );

        let transcript_after = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&transcript_id).unwrap().bounds
        };
        assert!(
            transcript_after.width < 180.0,
            "precondition: the transcript pane must have shrunk below the attach width (got {})",
            transcript_after.width
        );

        // Adapter republishes its stale (wide) attach-time node tree. The tile is
        // viewer-geometry-locked, so `set_tile_root` reconciles the republished
        // root to the resized pane instead of re-homing it to the stale column.
        let second = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let mut scene = shared.scene.try_lock().unwrap();
            attach_text_node(&mut scene, transcript_id, attach)
        };
        let republished_w = node_width(&app, second);
        assert!(
            (republished_w - transcript_after.width).abs() < 1e-2,
            "republished node width must be reconciled to the resized pane: expected ~{}, got {}",
            transcript_after.width,
            republished_w
        );
        assert!(
            republished_w < 180.0,
            "republished node must not keep the stale attach-time wrap width"
        );
    }

    /// (c) With no focused portal the width chord is a no-op: not consumed, no
    /// bounds change, and no resize state is created.
    #[test]
    fn ctrl_shift_arrow_width_resize_noop_when_unfocused() {
        use tze_hud_input::{FocusManager, HotkeyResizeAxis, HotkeyResizeDir, InputProcessor};

        let (scene, tab_id, frame_id, _transcript_id, _composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();
        // Fresh FocusManager with the tab but NO focus owner.
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        let before = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&frame_id).unwrap().bounds
        };

        let consumed =
            app.apply_portal_resize_hotkey(tab_id, HotkeyResizeDir::Grow, HotkeyResizeAxis::Width);
        assert!(
            !consumed,
            "width chord must not be consumed without a focused portal"
        );

        let after = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&frame_id).unwrap().bounds
        };
        assert_eq!(
            after, before,
            "unfocused width chord must not change any bounds"
        );
        assert!(
            app.state.portal_resize_states.is_empty(),
            "unfocused width chord must not create resize state"
        );
    }

    /// (d) A width grow is clamped so the portal never leaves the display: after
    /// many steps the right edge stays within the display bound.
    #[test]
    fn ctrl_shift_arrow_width_resize_clamps_at_display_edge() {
        use tze_hud_input::{HotkeyResizeAxis, HotkeyResizeDir, InputProcessor};

        let (scene, tab_id, frame_id, _transcript_id, _composer_id, _shield_id, fm) =
            multi_surface_portal_scene();
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());
        let display_w = app.state.config.window.width as f32;

        // Grow far past the display bound; each step clamps.
        for _ in 0..200 {
            app.apply_portal_resize_hotkey(tab_id, HotkeyResizeDir::Grow, HotkeyResizeAxis::Width);
        }

        let frame = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&frame_id).unwrap().bounds
        };
        assert!(
            frame.x + frame.width <= display_w + 1e-3,
            "width grow must clamp the right edge to the display bound (x={} w={} display_w={})",
            frame.x,
            frame.width,
            display_w
        );
    }

    /// (e) Dispatch wiring: Ctrl+Shift+ArrowRight resizes the width of a portal
    /// focused on a NON-composer surface, and is NOT stolen from the composer's
    /// word-selection while the composer is active.
    #[test]
    fn ctrl_shift_arrow_dispatch_respects_composer_focus() {
        use tze_hud_input::{FocusManager, InputProcessor, KeyboardModifiers};
        use tze_hud_scene::types::{HitRegionNode, TileScrollConfig};
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

        // Single-tile portal with a focusable NON-composer control node.
        let build = |accepts_composer_input: bool| {
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
            scene
                .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
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
                            bounds: Rect::new(0.0, 0.0, 400.0, 60.0),
                            interaction_id: "portal-surface".to_string(),
                            accepts_focus: true,
                            accepts_pointer: true,
                            accepts_composer_input,
                            ..Default::default()
                        }),
                    },
                )
                .unwrap();

            let mut processor = InputProcessor::new();
            let mut focus_manager = FocusManager::new();
            focus_manager.add_tab(tab_id);
            // Focus the node via a pointer down (click-to-focus).
            processor.process_with_focus(
                &tze_hud_input::PointerEvent {
                    x: 110.0,
                    y: 110.0,
                    kind: tze_hud_input::PointerEventKind::Down,
                    device_id: 1,
                    timestamp: None,
                },
                &mut scene,
                &mut focus_manager,
                tab_id,
            );
            (
                tab_id,
                tile_id,
                make_windowed_keyboard_test_app(scene, focus_manager, processor),
            )
        };

        let dispatch_ctrl_shift_arrow = |app: &mut WinitApp, tab_id, key_code: &str| {
            app.dispatch_key_down_event_inner(
                &RawKeyDownEvent {
                    key_code: key_code.to_string(),
                    key: key_code.to_string(),
                    modifiers: KeyboardModifiers {
                        ctrl: true,
                        shift: true,
                        ..KeyboardModifiers::NONE
                    },
                    repeat: false,
                    timestamp_mono_us: tze_hud_scene::MonoUs(1),
                },
                Some(tab_id),
            );
        };
        let width = |app: &WinitApp, tile_id| {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&tile_id).unwrap().bounds.width
        };

        // Composer INACTIVE (non-composer surface focused): the chord resizes.
        let (tab_id, tile_id, (mut app, _rx)) = build(false);
        assert!(
            !app.state.input_processor.is_composer_active(),
            "precondition: composer must be inactive for the non-composer surface"
        );
        let w0 = width(&app, tile_id);
        dispatch_ctrl_shift_arrow(&mut app, tab_id, "ArrowRight");
        assert!(
            width(&app, tile_id) > w0,
            "Ctrl+Shift+ArrowRight must grow width when the composer is inactive"
        );

        // Composer ACTIVE: the chord is NOT taken for resize (word-select wins).
        let (tab_id2, tile_id2, (mut app2, _rx2)) = build(true);
        assert!(
            app2.state.input_processor.is_composer_active(),
            "precondition: composer must be active for the composer surface"
        );
        let w0b = width(&app2, tile_id2);
        dispatch_ctrl_shift_arrow(&mut app2, tab_id2, "ArrowRight");
        assert_eq!(
            width(&app2, tile_id2),
            w0b,
            "Ctrl+Shift+ArrowRight must NOT resize while the composer is active (word-select preserved)"
        );
    }

    /// Register an `ElementType::Tile` entry (no override) in a shared state's
    /// element store for each id, so an id-keyed override write has a target.
    /// Mirrors the runtime tile-creation reconcile that seeds these entries.
    fn seed_tile_entries(app: &WinitApp, ids: &[tze_hud_scene::SceneId], namespace: &str) {
        use tze_hud_scene::element_store::{ElementStoreEntry, ElementType};
        let mut state = app.state.shared_state.try_lock().unwrap();
        for id in ids {
            state.element_store.entries.insert(
                *id,
                ElementStoreEntry {
                    element_type: ElementType::Tile,
                    namespace: namespace.to_string(),
                    created_at: 1,
                    last_published_at: 1,
                    z_order: 0,
                    unseen_restarts: 0,
                    geometry_override: None,
                },
            );
        }
    }

    fn has_override(app: &WinitApp, id: tze_hud_scene::SceneId) -> bool {
        let state = app.state.shared_state.try_lock().unwrap();
        state
            .element_store
            .entries
            .get(&id)
            .map(|e| e.geometry_override.is_some())
            .unwrap_or(false)
    }

    /// hud-8vejp: a whole-portal resize must write a durable per-member geometry
    /// override for EVERY constituent surface — not just one namespace/drag
    /// member — so `list_elements` reports `has_user_override=true` for all of
    /// them (the override key is `has_user_override = geometry_override.is_some()`).
    #[test]
    fn whole_portal_resize_writes_durable_override_for_every_member() {
        use tze_hud_input::{HotkeyResizeDir, InputProcessor};

        let (scene, tab_id, frame_id, transcript_id, composer_id, _shield_id, fm) =
            multi_surface_portal_scene();
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        // Seed store entries for the members (as the runtime does on tile
        // creation) so the id-keyed override write has a target.
        seed_tile_entries(
            &app,
            &[frame_id, transcript_id, composer_id],
            "portal-agent",
        );

        // Pre-condition: no member carries a user override.
        for id in [frame_id, transcript_id, composer_id] {
            assert!(
                !has_override(&app, id),
                "no member should have an override before the resize"
            );
        }

        assert!(
            app.apply_portal_resize_hotkey(
                tab_id,
                HotkeyResizeDir::Grow,
                tze_hud_input::HotkeyResizeAxis::Both
            ),
            "hotkey must be consumed for a focused portal surface"
        );

        // Every member — frame, transcript, composer — now has a durable
        // override, not just the anchor/one namespace member.
        for id in [frame_id, transcript_id, composer_id] {
            assert!(
                has_override(&app, id),
                "every portal member must have a durable geometry override after a whole-portal resize"
            );
        }
    }

    /// hud-8vejp: a whole-portal drag-move must write a durable per-member
    /// override for EVERY member (a portal's tiles share one namespace, so the
    /// legacy namespace-matched persist reached only one). Drives the real
    /// capture (`translate_portal_group_on_drag` + `resolve_portal_group`) and
    /// the real persist (`persist_drag_release`).
    #[test]
    fn whole_portal_drag_writes_durable_override_for_every_member() {
        use tze_hud_input::InputProcessor;

        let (scene, _tab, frame_id, transcript_id, composer_id, shield_id, fm) =
            multi_surface_portal_scene();
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());
        seed_tile_entries(
            &app,
            &[frame_id, transcript_id, composer_id, shield_id],
            "portal-agent",
        );

        // Reproduce exactly what the `Released` arm does: translate the whole
        // group, then collect each member's final bounds by SceneId.
        let (frame_final, group_members) = {
            let state = app.state.shared_state.try_lock().unwrap();
            let mut scene = state.scene.try_lock().unwrap();
            let (dx, dy) = (60.0_f32, -30.0_f32);
            assert!(
                translate_portal_group_on_drag(&mut scene, frame_id, dx, dy),
                "a multi-surface portal must engage the whole-portal translate"
            );
            let members: Vec<(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)> =
                resolve_portal_group(&scene, frame_id)
                    .unwrap()
                    .member_ids
                    .iter()
                    .filter_map(|id| scene.tiles.get(id).map(|t| (*id, t.bounds)))
                    .collect();
            let frame_final = scene.tiles.get(&frame_id).unwrap().bounds;
            (frame_final, members)
        };

        let released = DragReleasedData {
            element_id: frame_id,
            final_x: frame_final.x,
            final_y: frame_final.y,
            width: frame_final.width,
            height: frame_final.height,
            display_width: 1920.0,
            display_height: 1080.0,
            namespace: "portal-agent".to_string(),
            group_members,
        };
        app.persist_drag_release(released);

        // Every spatial portal member has a durable override.
        for id in [frame_id, transcript_id, composer_id] {
            assert!(
                has_override(&app, id),
                "every portal member must have a durable geometry override after a whole-portal drag"
            );
        }
        // The far-corner drag shield is not a spatial portal member, so it is not
        // translated and gets no override.
        assert!(
            !has_override(&app, shield_id),
            "the detached drag shield is not a portal member and must not get an override"
        );
    }

    /// A non-portal (non-scrollable) focused surface must not consume the resize
    /// hotkey nor change any geometry — the whole-portal path only engages for a
    /// focused portal surface.
    #[test]
    fn ctrl_resize_hotkey_ignored_when_focused_surface_is_not_a_portal() {
        use tze_hud_input::{HotkeyResizeDir, InputProcessor};

        let (scene, tab_id, frame_id, _transcript_id, _composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        // Focus the frame, which is NOT a scrollable portal surface.
        let mut fm = FocusManager::new();
        fm.request_focus(
            FocusRequest {
                tile_id: frame_id,
                node_id: None,
                steal: true,
                requesting_namespace: "portal-agent".to_string(),
            },
            tab_id,
            &scene,
        );

        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        let read = |app: &WinitApp, id: tze_hud_scene::SceneId| {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&id).unwrap().bounds
        };
        let frame_before = read(&app, frame_id);

        let consumed = app.apply_portal_resize_hotkey(
            tab_id,
            HotkeyResizeDir::Grow,
            tze_hud_input::HotkeyResizeAxis::Both,
        );
        assert!(
            !consumed,
            "resize hotkey must not be consumed when the focused surface is not a portal"
        );
        assert_eq!(
            read(&app, frame_id),
            frame_before,
            "geometry must not change when the focused surface is not a portal"
        );
    }

    /// Pointer-affordance resize (dragging the frame's bottom-right corner) must
    /// also scale the WHOLE portal: the returned member set covers the frame and
    /// both panes but excludes the far-corner drag shield, and every member
    /// scales together while the shield stays put.
    #[test]
    fn pointer_affordance_resize_scales_whole_portal_from_frame() {
        use tze_hud_input::{
            PointerEvent, PointerEventKind, PortalResizeState, PortalWindowTokens,
        };

        let (mut scene, tab_id, frame_id, transcript_id, composer_id, shield_id, fm) =
            multi_surface_portal_scene();
        let mut states: std::collections::HashMap<tze_hud_scene::SceneId, PortalResizeState> =
            std::collections::HashMap::new();
        let tokens = PortalWindowTokens::default();
        let (display_w, display_h) = (1920.0_f32, 1080.0_f32);

        let read = |scene: &tze_hud_scene::graph::SceneGraph, id: tze_hud_scene::SceneId| {
            scene.tiles.get(&id).unwrap().bounds
        };
        let frame_before = read(&scene, frame_id);
        let composer_before = read(&scene, composer_id);
        let transcript_before = read(&scene, transcript_id);
        let shield_before = read(&scene, shield_id);

        // PointerDown on the frame's bottom-right corner affordance (500, 400).
        let down = PointerEvent {
            x: 498.0,
            y: 398.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        let out_down = apply_portal_resize_pointer_event(
            &down,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        );
        assert!(
            out_down.is_some(),
            "pointer-down on the frame resize affordance must start a whole-portal gesture even with the composer focused"
        );

        // PointerMove outward to grow the portal.
        let mv = PointerEvent {
            x: 570.0,
            y: 470.0,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        };
        let out_move = apply_portal_resize_pointer_event(
            &mv,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("pointer-move must update the whole portal");

        let member_ids: Vec<_> = out_move.members.iter().map(|m| m.tile_id).collect();
        assert!(
            member_ids.contains(&frame_id)
                && member_ids.contains(&composer_id)
                && member_ids.contains(&transcript_id),
            "whole-portal resize members must include the frame and both panes"
        );
        assert!(
            !member_ids.contains(&shield_id),
            "the far-corner drag shield must be excluded from resize members"
        );

        let frame_after = read(&scene, frame_id);
        let composer_after = read(&scene, composer_id);
        let transcript_after = read(&scene, transcript_id);
        let shield_after = read(&scene, shield_id);

        assert!(
            frame_after.width > frame_before.width && frame_after.height > frame_before.height,
            "pointer resize must grow the frame"
        );
        assert!(
            composer_after.width > composer_before.width,
            "pointer resize must scale the composer pane with the portal"
        );
        assert!(
            transcript_after.width > transcript_before.width,
            "pointer resize must scale the transcript pane with the portal"
        );
        assert!(
            approx_tuple(
                rel_to_frame(composer_before, frame_before),
                rel_to_frame(composer_after, frame_after)
            ),
            "pointer resize must preserve the composer's relative layout"
        );
        assert_eq!(
            shield_after, shield_before,
            "pointer resize must not move the far-corner drag shield"
        );

        // End the gesture cleanly.
        let up = PointerEvent {
            x: 570.0,
            y: 470.0,
            kind: PointerEventKind::Up,
            device_id: 0,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &up,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        );
    }

    /// hud-yno2r regression (deterministic): whole-portal pointer resize must
    /// start from the frame's bottom-right affordance even when keyboard focus
    /// is NOT on a scrollable pane.
    ///
    /// On the live OS-pointer path the initiating click-to-focus moves focus
    /// onto the (non-scrollable) frame tile under the resize corner *before* the
    /// resize handler runs. The old gate required the *focused* tile to be
    /// scrollable (`scene.tile_scroll_config(focused_tile_id)?`), so the very
    /// click that started the gesture also disqualified it — resize was inert
    /// (Δw=Δh=0) while whole-portal move (which resolves the group from the tile
    /// under the pointer) kept working. This test pins focus to the frame — the
    /// exact post-click-to-focus state — and asserts the gesture still starts and
    /// grows every member. The prior test
    /// (`pointer_affordance_resize_scales_whole_portal_from_frame`) pre-focused
    /// the scrollable composer and so never exercised this focus state.
    #[test]
    fn pointer_resize_starts_from_frame_corner_regardless_of_focused_pane() {
        let (mut scene, tab_id, frame_id, transcript_id, composer_id, _shield_id, mut fm) =
            multi_surface_portal_scene();

        // Emulate the live post-click-to-focus state: the pointer-down on the
        // bottom-right resize corner lands on the frame tile, so click-to-focus
        // has already moved keyboard focus OFF the scrollable composer and ONTO
        // the non-scrollable frame. The old focus-gated resize returned early
        // here (frame has no scroll config).
        fm.request_focus(
            FocusRequest {
                tile_id: frame_id,
                node_id: None,
                steal: true,
                requesting_namespace: "portal-agent".to_string(),
            },
            tab_id,
            &scene,
        );
        assert_eq!(
            fm.current_owner(tab_id).tile_id(),
            Some(frame_id),
            "focus must be on the non-scrollable frame for this regression"
        );
        assert!(
            scene.tile_scroll_config(frame_id).is_none(),
            "the frame must be non-scrollable — the condition the old gate failed on"
        );

        let mut states: std::collections::HashMap<tze_hud_scene::SceneId, PortalResizeState> =
            std::collections::HashMap::new();
        let tokens = PortalWindowTokens::default();
        let (display_w, display_h) = (1920.0_f32, 1080.0_f32);

        let read = |scene: &tze_hud_scene::graph::SceneGraph, id: tze_hud_scene::SceneId| {
            scene.tiles.get(&id).unwrap().bounds
        };
        let frame_before = read(&scene, frame_id);
        let composer_before = read(&scene, composer_id);
        let transcript_before = read(&scene, transcript_id);

        // PointerDown on the frame's bottom-right corner affordance. The frame is
        // (100,100,400,300) → corner (500,400); (498,398) sits within the band.
        let down = PointerEvent {
            x: 498.0,
            y: 398.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        let out_down = apply_portal_resize_pointer_event(
            &down,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        );
        assert!(
            out_down.is_some(),
            "pointer-down on the frame resize affordance must start a whole-portal \
             gesture even when focus is on the non-scrollable frame (hud-yno2r)"
        );

        // PointerMove outward to grow the portal.
        let mv = PointerEvent {
            x: 570.0,
            y: 470.0,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        };
        let out_move = apply_portal_resize_pointer_event(
            &mv,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("pointer-move must update the whole portal");

        let member_ids: Vec<_> = out_move.members.iter().map(|m| m.tile_id).collect();
        assert!(
            member_ids.contains(&frame_id)
                && member_ids.contains(&composer_id)
                && member_ids.contains(&transcript_id),
            "whole-portal resize members must include the frame and both panes"
        );

        let frame_after = read(&scene, frame_id);
        let composer_after = read(&scene, composer_id);
        let transcript_after = read(&scene, transcript_id);
        assert!(
            frame_after.width > frame_before.width && frame_after.height > frame_before.height,
            "pointer resize must grow the frame"
        );
        assert!(
            composer_after.width > composer_before.width,
            "the composer pane must scale with the portal"
        );
        assert!(
            transcript_after.width > transcript_before.width,
            "the transcript pane must scale with the portal"
        );
        assert!(
            approx_tuple(
                rel_to_frame(composer_before, frame_before),
                rel_to_frame(composer_after, frame_after)
            ),
            "the composer must keep its relative layout within the portal"
        );

        // End the gesture cleanly.
        let up = PointerEvent {
            x: 570.0,
            y: 470.0,
            kind: PointerEventKind::Up,
            device_id: 0,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &up,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        );
    }

    /// hud-yno2r regression (end-to-end): drive the *full* pointer pipeline
    /// (`WinitApp::enqueue_pointer_event`) so click-to-focus and the resize
    /// handler run in their real order on one pointer-down — the coverage the
    /// live OS-injection sweep found was missing.
    ///
    /// The frame carries a focusable region, so the corner pointer-down moves
    /// keyboard focus onto the (non-scrollable) frame — reproducing the live
    /// state where the initiating click disqualified the old focus-gated resize.
    /// A subsequent move must then grow the whole portal. Under the old gate the
    /// gesture never started, so the move found nothing to update and the frame
    /// stayed put.
    #[test]
    fn enqueue_pointer_corner_drag_resizes_whole_portal_after_click_to_focus() {
        use tze_hud_scene::types::{HitRegionNode, Node, NodeData};
        use tze_hud_scene::{Rect, SceneId};

        let (mut scene, tab_id, frame_id, _transcript_id, composer_id, _shield_id, fm) =
            multi_surface_portal_scene();

        // Make the frame focusable so the corner-click moves focus onto it (the
        // live click-to-focus behaviour). Focus starts on the composer (set by
        // the scene helper); the corner-down must pull it to the frame.
        let frame_region_id = SceneId::new();
        scene
            .set_tile_root(
                frame_id,
                Node {
                    layout: Default::default(),
                    id: frame_region_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                        interaction_id: "portal-frame-focus".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        assert_eq!(
            fm.current_owner(tab_id).tile_id(),
            Some(composer_id),
            "focus must start on the scrollable composer pane"
        );

        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        let read = |app: &WinitApp, id: tze_hud_scene::SceneId| {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&id).unwrap().bounds
        };
        let frame_before = read(&app, frame_id);

        // PointerDown on the frame's bottom-right resize corner (500,400 corner;
        // 498,398 within the band). This ONE event runs click-to-focus (moving
        // focus composer → frame) and THEN the resize handler, in the real order.
        app.state.cursor_x = 498.0;
        app.state.cursor_y = 398.0;
        app.enqueue_pointer_event(PointerEventKind::Down);

        assert_eq!(
            app.state.focus_manager.current_owner(tab_id).tile_id(),
            Some(frame_id),
            "the corner click-to-focus must move focus onto the non-scrollable frame — \
             the exact state that made the old focus-gated resize inert"
        );

        // PointerMove outward: the active gesture must grow the whole portal.
        app.state.cursor_x = 570.0;
        app.state.cursor_y = 470.0;
        app.enqueue_pointer_event(PointerEventKind::Move);

        let frame_after = read(&app, frame_id);
        assert!(
            frame_after.width > frame_before.width && frame_after.height > frame_before.height,
            "dragging the frame corner must grow the whole portal even though \
             click-to-focus moved focus off the scrollable pane (hud-yno2r)"
        );

        // Release the gesture.
        app.enqueue_pointer_event(PointerEventKind::Up);
    }

    /// Grow the whole portal group that owns `frame_id` to `new_rect`, mirroring
    /// what a viewer resize gesture commits. Returns after the members have been
    /// scaled and viewer-geometry-locked.
    fn resize_group_to(
        scene: &mut tze_hud_scene::graph::SceneGraph,
        frame_id: tze_hud_scene::SceneId,
        new_rect: tze_hud_input::PortalRect,
    ) {
        let group = resolve_portal_group(scene, frame_id).expect("group must resolve");
        let old_rect = group.portal_rect;
        let snapshot = tze_hud_input::GeometrySnapshot {
            portal_id_hash: group.portal_id_hash,
            rect: new_rect,
            gesture_active: false,
            sequence: 1,
        };
        commit_portal_group_resize(scene, &group, old_rect, snapshot);
    }

    /// hud-s4lrw unit: `scale_portal_surface_parts` scales every part's
    /// tile-local bounds by the per-axis ratio, and is a no-op when the tile has
    /// no first-class surface.
    #[test]
    fn scale_portal_surface_parts_scales_tile_local_bounds() {
        use tze_hud_scene::types::{PortalPart, PortalPartKind, PortalSurface, Rect};

        let mut scene = tze_hud_scene::graph::SceneGraph::new(800.0, 600.0);
        let tab = scene.create_tab("t", 0).unwrap();
        let lease = scene.grant_lease("ns", 120_000);
        let tile = scene
            .create_tile(tab, "ns", lease, Rect::new(0.0, 0.0, 300.0, 200.0), 1)
            .unwrap();

        // No-op before any surface is declared.
        scale_portal_surface_parts(&mut scene, tile, 2.0, 0.5);
        assert!(!scene.overlay.portal_surfaces.contains_key(&tile));

        scene.overlay.portal_surfaces.insert(
            tile,
            PortalSurface {
                parts: vec![
                    PortalPart {
                        kind: PortalPartKind::Transcript,
                        bounds: Rect::new(10.0, 20.0, 100.0, 40.0),
                        node: Some(tze_hud_scene::SceneId::new()),
                    },
                    PortalPart {
                        kind: PortalPartKind::Composer,
                        bounds: Rect::new(0.0, 150.0, 300.0, 50.0),
                        node: Some(tze_hud_scene::SceneId::new()),
                    },
                ],
                ..Default::default()
            },
        );

        scale_portal_surface_parts(&mut scene, tile, 2.0, 0.5);
        let parts = &scene.overlay.portal_surfaces.get(&tile).unwrap().parts;
        // Transcript part: x,w ×2.0 ; y,h ×0.5.
        assert_eq!(parts[0].bounds.x, 20.0);
        assert_eq!(parts[0].bounds.y, 10.0);
        assert_eq!(parts[0].bounds.width, 200.0);
        assert_eq!(parts[0].bounds.height, 20.0);
        // Composer part scales the same way.
        assert_eq!(parts[1].bounds.y, 75.0);
        assert_eq!(parts[1].bounds.width, 600.0);
        assert_eq!(parts[1].bounds.height, 25.0);
    }

    /// hud-s4lrw integration: a whole-portal resize scales the host tile's
    /// first-class `PortalSurface` part bounds in lock-step with the re-flowed
    /// node tree, so the compositor's per-part clip band stays aligned at the new
    /// geometry ("overflow invariants hold at all geometries").
    #[test]
    fn whole_portal_resize_scales_surface_part_bounds() {
        use tze_hud_scene::types::{PortalPart, PortalPartKind, PortalSurface, Rect};

        let (mut scene, _tab, frame_id, transcript_id, _composer_id, _shield, _fm) =
            multi_surface_portal_scene();

        // Attach a first-class surface to the transcript (host) tile.
        let part_node = tze_hud_scene::SceneId::new();
        scene.overlay.portal_surfaces.insert(
            transcript_id,
            PortalSurface {
                parts: vec![PortalPart {
                    kind: PortalPartKind::Transcript,
                    bounds: Rect::new(10.0, 20.0, 100.0, 40.0),
                    node: Some(part_node),
                }],
                ..Default::default()
            },
        );

        let old_tile = scene.tiles.get(&transcript_id).unwrap().bounds;
        let old_frame = scene.tiles.get(&frame_id).unwrap().bounds;
        // Grow the whole portal (anisotropic to exercise both axes).
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: old_frame.x,
                y: old_frame.y,
                width: old_frame.width * 1.4,
                height: old_frame.height * 1.2,
            },
        );
        let new_tile = scene.tiles.get(&transcript_id).unwrap().bounds;
        let r_w = new_tile.width / old_tile.width;
        let r_h = new_tile.height / old_tile.height;
        assert!(r_w > 1.0 && r_h > 1.0, "resize must grow the host tile");

        let part = &scene
            .overlay
            .portal_surfaces
            .get(&transcript_id)
            .unwrap()
            .parts[0];
        assert!((part.bounds.x - 10.0 * r_w).abs() < 0.5, "x scaled by r_w");
        assert!((part.bounds.y - 20.0 * r_h).abs() < 0.5, "y scaled by r_h");
        assert!(
            (part.bounds.width - 100.0 * r_w).abs() < 0.5,
            "width scaled by r_w"
        );
        assert!(
            (part.bounds.height - 40.0 * r_h).abs() < 0.5,
            "height scaled by r_h"
        );
    }

    /// hud-rpmwt core: after a whole-portal resize the transcript/composer text
    /// must re-resolve to the NEW pane geometry, not stay wrapped at the
    /// attach-time width. The compositor wraps `TextMarkdownNode` text to the
    /// node's own `bounds.width` (tile-local, see
    /// `TextItem::from_text_markdown_node` / `from_text_markdown_cached`), so a
    /// resize that scales only `tile.bounds` — leaving the node tree stale —
    /// leaves the text wrapped at the old column: "resize works but the text
    /// isn't being resized". Assert both the node bounds AND the resulting
    /// `TextItem` layout width track the resized pane, at the draw-item seam.
    #[test]
    fn whole_portal_resize_accumulates_font_scale_by_width_ratio() {
        // hud-ovjxu.1: whole-portal resize records a viewer-local per-tile font
        // scale = the portal WIDTH ratio, uniform across every member, and
        // accumulates across steps. Bumps scene.version exactly once per step so
        // text re-shapes at most once per commit.
        let (mut scene, _tab, frame_id, transcript_id, composer_id, _shield, _fm) =
            multi_surface_portal_scene();
        let members = [frame_id, transcript_id, composer_id];
        let old_w = scene.tiles.get(&frame_id).unwrap().bounds.width; // 400 (fixture)

        // Default: no scaling before any resize.
        for id in members {
            assert_eq!(scene.tile_font_scale(id), 1.0);
        }

        // ── Grow to width 560 → ratio 1.4, applied to every member. ──────────
        let v0 = scene.version;
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 560.0,
                height: 420.0,
            },
        );
        let grow_ratio = 560.0 / old_w;
        for id in members {
            assert!(
                (scene.tile_font_scale(id) - grow_ratio).abs() < 1e-3,
                "every member's font scale must equal the portal width ratio after grow"
            );
        }
        assert_eq!(
            scene.version,
            v0 + 1,
            "a resize step must bump scene.version exactly once (single re-shape)"
        );

        // ── Shrink to width 280 → step ratio 0.5, ACCUMULATES to 1.4*0.5=0.7. ─
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 280.0,
                height: 210.0,
            },
        );
        let accumulated = grow_ratio * (280.0 / 560.0);
        for id in members {
            assert!(
                (scene.tile_font_scale(id) - accumulated).abs() < 1e-3,
                "font scale must accumulate multiplicatively across resize steps \
                 (got {}, want {accumulated})",
                scene.tile_font_scale(id)
            );
        }
    }

    #[test]
    fn whole_portal_resize_reflows_transcript_text_to_new_pane_width() {
        use tze_hud_compositor::TextItem;
        use tze_hud_scene::SceneId;
        use tze_hud_scene::types::{
            FontFamily, Node, NodeData, Rect, Rgba, TextAlign, TextMarkdownNode, TextOverflow,
        };

        let (mut scene, _tab_id, frame_id, transcript_id, _composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        // Attach a wrapping TextMarkdown node to the transcript pane. Node bounds
        // are tile-local: fill the transcript tile (180 x 280 from the fixture).
        let text_id = SceneId::new();
        scene.nodes.insert(
            text_id,
            Node {
                layout: Default::default(),
                id: text_id,
                children: vec![],
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: "the quick brown fox jumps over the lazy dog again and again"
                        .to_owned(),
                    bounds: Rect::new(0.0, 0.0, 180.0, 280.0),
                    font_size_px: 14.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                    background: None,
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Clip,
                    color_runs: Box::default(),
                }),
            },
        );
        scene.tiles.get_mut(&transcript_id).unwrap().root_node = Some(text_id);

        let node_width = |scene: &tze_hud_scene::graph::SceneGraph| match &scene
            .nodes
            .get(&text_id)
            .unwrap()
            .data
        {
            NodeData::TextMarkdown(tm) => tm.bounds.width,
            other => panic!("expected TextMarkdown, got {other:?}"),
        };
        // Wrap column the compositor would use, before the resize.
        let layout_width_before = {
            let NodeData::TextMarkdown(tm) = &scene.nodes.get(&text_id).unwrap().data else {
                unreachable!()
            };
            TextItem::from_text_markdown_node(tm, 0.0, 0.0).bounds_width
        };
        let node_width_before = node_width(&scene);
        let transcript_before = scene.tiles.get(&transcript_id).unwrap().bounds;

        // Grow the whole portal ~1.5x in width, ~1.3x in height.
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 600.0,
                height: 390.0,
            },
        );

        let transcript_after = scene.tiles.get(&transcript_id).unwrap().bounds;
        assert!(
            transcript_after.width > transcript_before.width,
            "precondition: the transcript tile must have grown"
        );

        // 1) The node tree re-resolved: node width scaled with its tile.
        let node_width_after = node_width(&scene);
        let tile_ratio = transcript_after.width / transcript_before.width;
        let expected_node_width = node_width_before * tile_ratio;
        assert!(
            (node_width_after - expected_node_width).abs() < 1e-2,
            "transcript text node width must scale with the pane: expected \
             ~{expected_node_width}, got {node_width_after}"
        );

        // 2) The draw-item wrap column tracks the new pane — the seam the
        //    compositor shapes/wraps against. Before the fix this stayed pinned
        //    to the attach-time width.
        let layout_width_after = {
            let NodeData::TextMarkdown(tm) = &scene.nodes.get(&text_id).unwrap().data else {
                unreachable!()
            };
            TextItem::from_text_markdown_node(tm, 0.0, 0.0).bounds_width
        };
        assert!(
            layout_width_after > layout_width_before + 1.0,
            "TextItem layout/wrap width must grow with the resized pane: \
             before={layout_width_before}, after={layout_width_after}"
        );

        // 3) The wrap column stays within the resized pane — no layout to a
        //    width wider than the tile (overflow contract: no partially clipped
        //    glyphs from a stale-wide layout).
        assert!(
            layout_width_after <= transcript_after.width + 1e-3,
            "wrap width {layout_width_after} must not exceed the resized pane \
             width {}",
            transcript_after.width
        );
    }

    /// hud-rpmwt live-repro core: after a whole-portal resize, an ADAPTER
    /// content republish must NOT snap the transcript back to its stale
    /// attach-time wrap width. The resize scales `tile.bounds` + the node tree
    /// and takes viewer geometry authority (hud-lyqun). But a republish replaces
    /// the whole node tree via `set_tile_root`, and the compositor wraps
    /// `TextMarkdownNode` text to `node.bounds.width` and clips to `tile.bounds`.
    /// An adapter that republishes its stale (config-width) node bounds would
    /// re-home the transcript to the old column — the fresh repro: text rendered
    /// at the stale ~1720px wrap, overflowing the resized frame UNCLIPPED. The
    /// node-bounds-authority fix reconciles the republished root to the tile's
    /// viewer-defined bounds. Assert both the node width AND the draw-item wrap
    /// width re-resolve to the resized pane (and never exceed it — the clip
    /// contract) at the same `TextItem` seam the sibling reflow test uses.
    #[test]
    fn adapter_republish_after_resize_keeps_transcript_wrapped_to_resized_pane() {
        use tze_hud_compositor::TextItem;
        use tze_hud_scene::SceneId;
        use tze_hud_scene::types::{
            FontFamily, Node, NodeData, Rect, Rgba, TextAlign, TextMarkdownNode, TextOverflow,
        };

        let (mut scene, _tab_id, frame_id, transcript_id, _composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        // The adapter's attach-time node geometry: tile-local, filling the
        // transcript pane at its ORIGINAL (config) size (180 x 280 fixture).
        let attach_bounds = Rect::new(0.0, 0.0, 180.0, 280.0);
        let make_node = |bounds: Rect| {
            let id = SceneId::new();
            (
                id,
                Node {
                    layout: Default::default(),
                    id,
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "the quick brown fox jumps over the lazy dog again and again"
                            .to_owned(),
                        bounds,
                        font_size_px: 14.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
        };

        // Initial attach: publish the transcript node at the attach-time width.
        let (first_id, first_node) = make_node(attach_bounds);
        scene.set_tile_root(transcript_id, first_node).unwrap();
        let _ = first_id;

        // Whole-portal SHRINK: 400→260 wide frame. The transcript tile shrinks
        // with it, the resize scales the node tree, and the tile takes viewer
        // geometry authority (lock).
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 260.0,
                height: 260.0,
            },
        );
        let transcript_after = scene.tiles.get(&transcript_id).unwrap().bounds;
        assert!(
            transcript_after.width < 180.0,
            "precondition: the transcript pane must have shrunk below the \
             attach-time node width (got {})",
            transcript_after.width
        );
        assert!(
            scene.is_viewer_geometry_locked(transcript_id),
            "precondition: a whole-portal resize must take viewer geometry authority"
        );

        // ── ADAPTER REPUBLISH with STALE attach-time bounds ──────────────────
        // Simulate the exemplar re-publishing its content at the config width
        // (the live failure mode): a fresh node tree carrying the wide
        // attach-time bounds, which would otherwise re-home the transcript to the
        // stale column and overflow the resized pane unclipped.
        let (_second_id, second_node) = make_node(attach_bounds);
        scene.set_tile_root(transcript_id, second_node).unwrap();

        let node_width_after = match &scene
            .nodes
            .get(&scene.tiles.get(&transcript_id).unwrap().root_node.unwrap())
            .unwrap()
            .data
        {
            NodeData::TextMarkdown(tm) => tm.bounds.width,
            other => panic!("expected TextMarkdown, got {other:?}"),
        };

        // 1) The republished node's width was reconciled to the resized pane, NOT
        //    left at the stale attach-time width.
        assert!(
            (node_width_after - transcript_after.width).abs() < 1e-2,
            "republished transcript node width must track the resized pane: \
             expected ~{}, got {node_width_after} (stale attach width was 180)",
            transcript_after.width
        );
        assert!(
            node_width_after < 180.0,
            "republished node must not keep the stale attach-time wrap width"
        );

        // 2) The draw-item wrap column (the seam the compositor shapes against)
        //    stays within the resized pane — no wrap-to-stale-width, and the clip
        //    can never be exceeded because the node no longer overflows the tile.
        let layout_width_after = {
            let NodeData::TextMarkdown(tm) = &scene
                .nodes
                .get(&scene.tiles.get(&transcript_id).unwrap().root_node.unwrap())
                .unwrap()
                .data
            else {
                unreachable!()
            };
            TextItem::from_text_markdown_node(tm, 0.0, 0.0).bounds_width
        };
        assert!(
            layout_width_after <= transcript_after.width + 1e-3,
            "wrap width {layout_width_after} must not exceed the resized pane \
             width {} (stale-wide layout would overflow the frame unclipped)",
            transcript_after.width
        );

        // 2b) A child node added AFTER the root within the same republish (a
        //     portal batch attaches the composer as a separate `AddNode` after
        //     `SetTileRoot`) is reconciled too — otherwise only the root scales
        //     and the child keeps stale attach-time geometry. Add a child at the
        //     stale attach width and assert it tracks the resized pane.
        let root_id_for_child = scene.tiles.get(&transcript_id).unwrap().root_node.unwrap();
        let (child_id, child_node) = make_node(attach_bounds);
        scene
            .add_node_to_tile(transcript_id, Some(root_id_for_child), child_node)
            .unwrap();
        let child_width = match &scene.nodes.get(&child_id).unwrap().data {
            NodeData::TextMarkdown(tm) => tm.bounds.width,
            other => panic!("expected TextMarkdown, got {other:?}"),
        };
        assert!(
            (child_width - transcript_after.width).abs() < 1e-2,
            "a child added after the root must also be reconciled to the resized \
             pane: expected ~{}, got {child_width} (stale attach width was 180)",
            transcript_after.width
        );

        // 3) Scoping guard: once the viewer releases geometry authority, a
        //    republish is NOT reconciled — the adapter regains node-bounds
        //    control, proving the fix is gated on the lock and cannot distort
        //    ordinary (non-viewer-resized) tiles.
        scene.unlock_viewer_geometry(transcript_id);
        let (_third_id, third_node) = make_node(attach_bounds);
        scene.set_tile_root(transcript_id, third_node).unwrap();
        let node_width_unlocked = match &scene
            .nodes
            .get(&scene.tiles.get(&transcript_id).unwrap().root_node.unwrap())
            .unwrap()
            .data
        {
            NodeData::TextMarkdown(tm) => tm.bounds.width,
            other => panic!("expected TextMarkdown, got {other:?}"),
        };
        assert!(
            (node_width_unlocked - 180.0).abs() < 1e-2,
            "an unlocked tile must apply the adapter's published bounds verbatim \
             (got {node_width_unlocked}, want 180)"
        );
    }

    /// hud-lyqun core: dragging one constituent surface of a text-stream portal
    /// must translate the WHOLE portal by the same delta — every member moves
    /// together preserving relative layout, the far-corner drag shield stays
    /// parked, and every moved member takes viewer geometry authority.
    #[test]
    fn drag_move_translates_whole_portal_group_coherently() {
        let (mut scene, _tab_id, frame_id, transcript_id, composer_id, shield_id, _fm) =
            multi_surface_portal_scene();

        let read =
            |scene: &tze_hud_scene::graph::SceneGraph, id| scene.tiles.get(&id).unwrap().bounds;
        let frame_before = read(&scene, frame_id);
        let transcript_before = read(&scene, transcript_id);
        let composer_before = read(&scene, composer_id);
        let shield_before = read(&scene, shield_id);

        let (dx, dy) = (140.0_f32, -35.0_f32);
        let translated = translate_portal_group_on_drag(&mut scene, frame_id, dx, dy);
        assert!(
            translated,
            "dragging a portal surface must engage the whole-portal translate path"
        );

        let frame_after = read(&scene, frame_id);
        let transcript_after = read(&scene, transcript_id);
        let composer_after = read(&scene, composer_id);
        let shield_after = read(&scene, shield_id);

        // Every constituent surface moved by exactly the drag delta.
        for (before, after, name) in [
            (frame_before, frame_after, "frame"),
            (transcript_before, transcript_after, "transcript"),
            (composer_before, composer_after, "composer"),
        ] {
            assert!(
                (after.x - (before.x + dx)).abs() < 1e-3
                    && (after.y - (before.y + dy)).abs() < 1e-3,
                "{name} must translate by the drag delta"
            );
            assert!(
                (after.width - before.width).abs() < 1e-3
                    && (after.height - before.height).abs() < 1e-3,
                "{name} size must not change on a move"
            );
        }

        // Relative layout preserved for every member.
        assert!(approx_tuple(
            rel_to_frame(transcript_before, frame_before),
            rel_to_frame(transcript_after, frame_after)
        ));
        assert!(approx_tuple(
            rel_to_frame(composer_before, frame_before),
            rel_to_frame(composer_after, frame_after)
        ));

        // The far-corner drag shield is not a spatial member and stays put.
        assert_eq!(
            shield_after, shield_before,
            "the far-corner drag shield must not move with a portal drag"
        );

        // Every moved member now holds viewer geometry authority.
        for id in [frame_id, transcript_id, composer_id] {
            assert!(
                scene.is_viewer_geometry_locked(id),
                "each dragged portal member must take viewer geometry authority"
            );
        }
        assert!(
            !scene.is_viewer_geometry_locked(shield_id),
            "the untouched drag shield must not be locked"
        );
    }

    /// hud-uyhpn core: a multi-frame drag-move is a POSITION-ONLY mutation. It
    /// must NOT bump `scene.version` (the sentinel the compositor's markdown /
    /// truncation caches gate on) — otherwise every pointer delta invalidates the
    /// content-shaped caches and forces a full re-hash / re-shape per frame, the
    /// low-fps / flickery drag observed live. Instead each frame advances
    /// `scene.geometry_epoch` so the present-gate still repaints the moved portal.
    ///
    /// Baseline (pre-fix) over a K-frame drag: `scene.version` advanced K times →
    /// K content-cache re-primes. Fixed: `scene.version` advances 0 times → ZERO
    /// re-primes, while position updates every frame (`geometry_epoch` +K, bounds
    /// move each frame).
    #[test]
    fn drag_move_is_position_only_no_version_bump_repaints_each_frame() {
        let (mut scene, _tab_id, frame_id, transcript_id, composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        let read =
            |scene: &tze_hud_scene::graph::SceneGraph, id| scene.tiles.get(&id).unwrap().bounds;

        // Settle to a known baseline as if content had just been committed and the
        // compositor primed its caches at this version.
        let version_at_drag_start = scene.version;
        let epoch_at_drag_start = scene.geometry_epoch;

        // Simulate a K-frame pointer drag: many small deltas, one per frame.
        const FRAMES: usize = 30;
        let (dx, dy) = (3.0_f32, -2.0_f32);
        let members = [frame_id, transcript_id, composer_id];
        let mut prev: Vec<_> = members.iter().map(|&id| read(&scene, id)).collect();

        for frame in 0..FRAMES {
            let translated = translate_portal_group_on_drag(&mut scene, frame_id, dx, dy);
            assert!(translated, "frame {frame}: portal translate must engage");

            // Content-cache sentinel must NOT move: version is frozen across the
            // whole drag, so the version-gated markdown/truncation caches skip
            // every frame — ZERO re-primes / re-shapes.
            assert_eq!(
                scene.version, version_at_drag_start,
                "frame {frame}: a position-only drag must NOT bump scene.version \
                 (would re-prime content caches every frame — hud-uyhpn)"
            );

            // The present-gate must still see the frame as dirty: geometry_epoch
            // advances exactly once per applied translate.
            assert_eq!(
                scene.geometry_epoch,
                epoch_at_drag_start + (frame as u64 + 1),
                "frame {frame}: geometry_epoch must advance once per drag frame so \
                 the moved portal repaints"
            );

            // Every member actually moved by the delta this frame.
            for (i, &id) in members.iter().enumerate() {
                let now = read(&scene, id);
                assert!(
                    (now.x - (prev[i].x + dx)).abs() < 1e-3
                        && (now.y - (prev[i].y + dy)).abs() < 1e-3,
                    "frame {frame}: member {i} must translate by the per-frame delta"
                );
                // Size never changes on a move — so truncation keys (size-based)
                // stay identical and the cache stays valid the whole drag.
                assert!(
                    (now.width - prev[i].width).abs() < 1e-3
                        && (now.height - prev[i].height).abs() < 1e-3,
                    "frame {frame}: member {i} size must not change on a move"
                );
                prev[i] = now;
            }
        }

        // Over the whole drag: version never moved (0 re-primes), geometry_epoch
        // moved once per frame (FRAMES repaints).
        assert_eq!(
            scene.version, version_at_drag_start,
            "scene.version must be unchanged across the entire drag"
        );
        assert_eq!(
            scene.geometry_epoch,
            epoch_at_drag_start + FRAMES as u64,
            "geometry_epoch must have advanced once per drag frame"
        );
    }

    /// hud-uyhpn: the single-tile (non-portal) move fallback is also position-only
    /// — it must advance `geometry_epoch`, not `scene.version`.
    #[test]
    fn single_tile_move_fallback_is_position_only() {
        use tze_hud_scene::{Rect, SceneGraph};
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(10.0, 10.0, 100.0, 80.0),
                1,
            )
            .unwrap();

        let version_before = scene.version;
        let epoch_before = scene.geometry_epoch;

        // A plain tile resolves to the single-tile fallback (translate returns
        // false); the caller then applies bounds + geometry epoch. Reproduce that
        // position-only contract exactly.
        assert!(
            !translate_portal_group_on_drag(&mut scene, tile_id, 20.0, 15.0),
            "a plain tile must fall through to the single-tile move path"
        );
        if scene.tiles.contains_key(&tile_id) {
            if let Some(tile) = scene.tiles.get_mut(&tile_id) {
                tile.bounds.x += 20.0;
                tile.bounds.y += 15.0;
            }
            scene.bump_geometry_epoch();
        }

        assert_eq!(
            scene.version, version_before,
            "single-tile move must not bump scene.version (position-only)"
        );
        assert_eq!(
            scene.geometry_epoch,
            epoch_before + 1,
            "single-tile move must advance geometry_epoch so it repaints"
        );
    }

    /// hud-643dv: the runtime header-band drag handle targets the portal FRAME
    /// (the largest-area lease member), and a drag originating from that anchor —
    /// exactly what the band produces — moves the whole group position-only
    /// (leverages the #991 geometry-epoch path: no `scene.version` bump).
    #[test]
    fn header_band_anchor_is_the_frame_and_band_drag_is_position_only() {
        let (mut scene, _tab, frame_id, transcript_id, composer_id, _shield, _fm) =
            multi_surface_portal_scene();

        // The band handle the compositor emits targets the frame/anchor tile.
        let anchors = scene.portal_header_band_anchors(52.0);
        assert!(
            anchors.iter().any(|(a, _)| *a == frame_id),
            "the header-band drag handle must target the portal frame anchor"
        );
        assert!(
            !anchors
                .iter()
                .any(|(a, _)| *a == transcript_id || *a == composer_id),
            "panes must NOT get their own header band"
        );

        // Dragging from the anchor (as a band drag does) moves the whole group
        // position-only: no scene.version bump, geometry_epoch advances once.
        let read = |s: &tze_hud_scene::graph::SceneGraph, id| s.tiles.get(&id).unwrap().bounds;
        let (fb, tb, cb) = (
            read(&scene, frame_id),
            read(&scene, transcript_id),
            read(&scene, composer_id),
        );
        let version_before = scene.version;
        let epoch_before = scene.geometry_epoch;
        let (dx, dy) = (40.0_f32, -25.0_f32);
        assert!(translate_portal_group_on_drag(&mut scene, frame_id, dx, dy));

        assert_eq!(
            scene.version, version_before,
            "a band drag must be position-only — no content-cache re-prime (hud-uyhpn)"
        );
        assert_eq!(scene.geometry_epoch, epoch_before + 1);
        for (before, id) in [(fb, frame_id), (tb, transcript_id), (cb, composer_id)] {
            let after = read(&scene, id);
            assert!(
                (after.x - (before.x + dx)).abs() < 1e-3
                    && (after.y - (before.y + dy)).abs() < 1e-3,
                "every member must translate by the band-drag delta"
            );
        }
    }

    /// A single non-portal tile drag must NOT engage the whole-portal translate
    /// path (no scrollable constituent), so behavior is unchanged for plain tiles.
    #[test]
    fn drag_move_single_non_portal_tile_is_not_group_translated() {
        use tze_hud_scene::{Rect, SceneGraph};
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(10.0, 10.0, 100.0, 80.0),
                1,
            )
            .unwrap();

        let translated = translate_portal_group_on_drag(&mut scene, tile_id, 20.0, 20.0);
        assert!(
            !translated,
            "a plain non-portal tile must be left to the single-tile move path"
        );
        assert!(
            !scene.is_viewer_geometry_locked(tile_id),
            "a non-portal single-tile drag must not take geometry authority"
        );
    }

    /// hud-lyqun regression: resize the whole portal, THEN drag it — the group
    /// must stay coherent (relative layout preserved) rather than fracturing.
    #[test]
    fn resize_then_drag_keeps_portal_group_coherent() {
        let (mut scene, _tab_id, frame_id, transcript_id, composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        // Grow the portal (top-left anchored) to a larger rect.
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 560.0,
                height: 420.0,
            },
        );

        let read =
            |scene: &tze_hud_scene::graph::SceneGraph, id| scene.tiles.get(&id).unwrap().bounds;
        let frame_mid = read(&scene, frame_id);
        let transcript_mid = read(&scene, transcript_id);
        let composer_mid = read(&scene, composer_id);

        // Now drag the resized portal.
        let (dx, dy) = (-60.0_f32, 90.0_f32);
        assert!(translate_portal_group_on_drag(&mut scene, frame_id, dx, dy));

        let frame_after = read(&scene, frame_id);
        let transcript_after = read(&scene, transcript_id);
        let composer_after = read(&scene, composer_id);

        // Relative layout is preserved through resize AND the subsequent drag.
        assert!(
            approx_tuple(
                rel_to_frame(transcript_mid, frame_mid),
                rel_to_frame(transcript_after, frame_after)
            ),
            "transcript must keep relative layout after resize+drag"
        );
        assert!(
            approx_tuple(
                rel_to_frame(composer_mid, frame_mid),
                rel_to_frame(composer_after, frame_after)
            ),
            "composer must keep relative layout after resize+drag"
        );
        // Frame translated by the drag delta.
        assert!(
            (frame_after.x - (frame_mid.x + dx)).abs() < 1e-3
                && (frame_after.y - (frame_mid.y + dy)).abs() < 1e-3,
            "frame must translate by the drag delta after a resize"
        );
    }

    /// hud-lyqun proof: after a whole-portal resize, an adapter republishing its
    /// stale client-side member layout (via `update_tile_bounds`) CANNOT move any
    /// member — the group cannot be fractured.
    #[test]
    fn adapter_republish_cannot_fracture_resized_portal_group() {
        let (mut scene, _tab_id, frame_id, transcript_id, composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 560.0,
                height: 420.0,
            },
        );

        let read =
            |scene: &tze_hud_scene::graph::SceneGraph, id| scene.tiles.get(&id).unwrap().bounds;
        let frame_scaled = read(&scene, frame_id);
        let transcript_scaled = read(&scene, transcript_id);
        let composer_scaled = read(&scene, composer_id);

        // The adapter re-emits its OLD pre-resize client-side layout for a subset
        // of members (exactly the live-observed fracture: some members stomped to
        // stale bounds while others keep runtime-scaled bounds).
        let _ = scene.update_tile_bounds(
            transcript_id,
            tze_hud_scene::Rect::new(110.0, 110.0, 180.0, 280.0),
            "portal-agent",
        );
        let _ = scene.update_tile_bounds(
            composer_id,
            tze_hud_scene::Rect::new(300.0, 110.0, 190.0, 280.0),
            "portal-agent",
        );

        // Nothing moved: the runtime-owned scaled geometry held for every member.
        assert_eq!(
            read(&scene, transcript_id),
            transcript_scaled,
            "adapter republish must not stomp the transcript pane after a resize"
        );
        assert_eq!(
            read(&scene, composer_id),
            composer_scaled,
            "adapter republish must not stomp the composer pane after a resize"
        );
        assert_eq!(
            read(&scene, frame_id),
            frame_scaled,
            "the frame must keep its resized geometry"
        );

        // The group is still internally coherent (relative layout intact).
        assert!(approx_tuple(
            rel_to_frame(transcript_scaled, frame_scaled),
            rel_to_frame(read(&scene, transcript_id), read(&scene, frame_id))
        ));
    }

    // ── Composer pointer caret hit-test + drag-select (hud-etrs0) ───────────

    /// Build a scene with one focusable composer `HitRegion` spanning
    /// `(0, 0, 400, 100)` in a single active tab, ready for
    /// `WinitApp::enqueue_pointer_event` pointer-down/move/up dispatch.
    fn scene_with_pointer_composer() -> (tze_hud_scene::graph::SceneGraph, tze_hud_scene::SceneId) {
        use tze_hud_scene::types::HitRegionNode;
        use tze_hud_scene::{Node, NodeData, Rect, SceneGraph, SceneId};

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 400.0, 100.0),
                1,
            )
            .unwrap();
        let composer_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: composer_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 400.0, 100.0),
                        interaction_id: "portal-composer-focus".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        accepts_composer_input: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();
        (scene, tab_id)
    }

    /// PointerDown on the focused composer must place the caret via the real
    /// glyph-geometry hit-test (`byte_at_point`/`byte_at_x`) once the
    /// compositor has published a fresh wrapped visual-row layout — not the
    /// old `(local_x / node_width) * text_byte_len` linear guess, which for
    /// this fixture (draft "abcdef", click at node-local x=20 on row 0, text
    /// content width ~388px after the 6px inset) would land near byte 0 or 1
    /// instead of the glyph-accurate byte 2.
    ///
    /// Then a drag (PointerMove to row 1 at the same x) must extend the
    /// selection from that anchor to the byte under the new position —
    /// asserted at the byte/selection layer, not pixels — and PointerUp must
    /// end the drag gesture without disturbing the selection it produced.
    #[test]
    fn composer_pointer_down_then_drag_selects_byte_range_via_glyph_hit_test() {
        use tze_hud_input::{
            ComposerVisualLayout, ComposerVisualLine, FocusManager, InputProcessor,
        };

        let (scene, tab_id) = scene_with_pointer_composer();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);
        let processor = InputProcessor::new();
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, processor);

        // ── Focus the composer (empty draft) via an initial click+release. ──
        app.state.cursor_x = 200.0;
        app.state.cursor_y = 50.0;
        app.enqueue_pointer_event(PointerEventKind::Down);
        app.enqueue_pointer_event(PointerEventKind::Up);
        assert!(
            app.state.input_processor.is_composer_active(),
            "pointer-down on the composer HitRegion must focus and activate the draft"
        );

        // ── Type "abcdef" so the draft has real content to hit-test against. ──
        for ch in ["a", "b", "c", "d", "e", "f"] {
            app.state.input_processor.route_character_to_composer(ch);
        }
        assert_eq!(
            app.state
                .input_processor
                .composer_draft_snapshot()
                .unwrap()
                .0,
            "abcdef"
        );

        // ── Publish the wrapped visual-row layout the compositor would emit
        // for "abcdef" wrapped as "abc" / "def", with non-uniform (proportional)
        // per-glyph advances so the test cannot pass via a uniform-width
        // shortcut. `text_len` must match the draft (6) to be considered fresh.
        let layout = ComposerVisualLayout {
            lines: vec![
                ComposerVisualLine {
                    start_byte: 0,
                    end_byte: 3,
                    glyph_x: vec![(0, 0.0), (1, 5.0), (2, 15.0), (3, 30.0)],
                },
                ComposerVisualLine {
                    start_byte: 3,
                    end_byte: 6,
                    glyph_x: vec![(3, 0.0), (4, 8.0), (5, 20.0), (6, 30.0)],
                },
            ],
            text_len: 6,
            input_box: None,
            h_scroll_px: 0.0,
        };
        *app.state.composer_visual_layout.lock().unwrap() = Some(layout);

        // ── PointerDown at node-local (x=26, y=10): row 0 (top half of the
        // 100px-tall node), text_x = 26 - 6px content inset = 20 → nearest
        // glyph boundary on row 0's table is byte 2 (glyph_x distances 20, 15,
        // 5, 10 → byte 2 wins). ──────────────────────────────────────────────
        app.state.cursor_x = 26.0;
        app.state.cursor_y = 10.0;
        app.enqueue_pointer_event(PointerEventKind::Down);
        let snapshot = app
            .state
            .input_processor
            .composer_draft_snapshot()
            .expect("composer draft must still be active");
        assert_eq!(
            (snapshot.1, snapshot.2),
            (2, 2),
            "pointer-down must place the caret at the glyph-accurate byte (2), \
             not a selection, and not the old linear-guess byte"
        );
        assert_eq!(
            app.state.composer_pointer_drag_anchor.map(|(_, b)| b),
            Some(2),
            "pointer-down must record a drag anchor at the placed byte"
        );
        // The compositor renders the caret/selection from the local echo slot,
        // not the input processor, so the pointer caret must be published there
        // too (hud-etrs0 local-feedback-first) or the visible caret stays stale
        // until the next keystroke.
        let echo = app
            .state
            .local_composer_state
            .lock()
            .unwrap()
            .clone()
            .flatten()
            .expect("pointer-down must publish the composer echo");
        assert_eq!(
            (echo.cursor_byte, echo.selection_anchor),
            (2, 2),
            "pointer-down must push the placed caret into the local echo slot"
        );

        // ── PointerMove to node-local (x=26, y=90): row 1 (bottom half), same
        // text_x=20 → row 1's table nearest byte is 5 (distances 20, 12, 0, 10
        // → byte 5 wins). Must extend the selection from the anchor (2) to 5,
        // not collapse or jump to an unrelated byte. ───────────────────────
        app.state.cursor_x = 26.0;
        app.state.cursor_y = 90.0;
        app.enqueue_pointer_event(PointerEventKind::Move);
        let snapshot = app
            .state
            .input_processor
            .composer_draft_snapshot()
            .expect("composer draft must still be active");
        assert_eq!(
            (snapshot.1, snapshot.2),
            (5, 2),
            "drag must extend the selection from anchor byte 2 to byte 5 under \
             the new pointer position"
        );
        // The drag-extended selection must also reach the local echo slot so
        // the highlight repaints live during the drag (hud-etrs0), not only
        // after a later keystroke.
        let echo = app
            .state
            .local_composer_state
            .lock()
            .unwrap()
            .clone()
            .flatten()
            .expect("drag-move must publish the composer echo");
        assert_eq!(
            (echo.cursor_byte, echo.selection_anchor),
            (5, 2),
            "drag-move must push the extended selection into the local echo slot"
        );

        // ── PointerUp ends the drag gesture but must not disturb the
        // selection it produced. ────────────────────────────────────────────
        app.enqueue_pointer_event(PointerEventKind::Up);
        assert_eq!(
            app.state.composer_pointer_drag_anchor, None,
            "pointer-up must clear the drag anchor"
        );
        let snapshot = app
            .state
            .input_processor
            .composer_draft_snapshot()
            .expect("composer draft must still be active");
        assert_eq!(
            (snapshot.1, snapshot.2),
            (5, 2),
            "pointer-up must not change the selection established by the drag"
        );
    }

    /// hud-uui70: the single-line profile's `glyph_x` table is measured in
    /// UNSCROLLED draft space (`measure_composer_single_line_layout`); once a
    /// long draft triggers caret-follow horizontal scroll (`h_scroll_px`,
    /// hud-zlfi4), `composer_pointer_byte_offset` must fold that scroll back
    /// into the de-inset screen-space x before consulting the glyph table —
    /// otherwise a click's screen x is looked up directly against unscrolled
    /// draft coordinates and lands on the WRONG byte.
    ///
    /// One row `[0, 6)` ("abcdef") with the SAME non-uniform per-glyph table
    /// as the sibling multi-line drag test, published with `h_scroll_px:
    /// 20.0` (`input_box: None`, matching what the single-line profile
    /// actually publishes). A click at node-local x=26 de-insets to
    /// screen-space text_x=20:
    /// - WITHOUT the h_scroll correction (the pre-hud-uui70 bug), 20 is
    ///   nearest to byte 2 (glyph x=15, distance 5) — the exact wrong byte
    ///   the sibling multi-line test's row 0 also resolves to at this same
    ///   pixel, proving this is a real, distinct mismatch and not a fixture
    ///   artifact.
    /// - WITH the correction, draft-space x = 20 + 20 (h_scroll_px) = 40,
    ///   nearest to byte 4 (glyph x=45, distance 5) — a DIFFERENT byte,
    ///   proving the fix actually changes the outcome (mutation-checked).
    #[test]
    fn composer_pointer_down_folds_h_scroll_into_single_line_hit_test() {
        use tze_hud_input::{
            ComposerVisualLayout, ComposerVisualLine, FocusManager, InputProcessor,
        };

        let (scene, tab_id) = scene_with_pointer_composer();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);
        let processor = InputProcessor::new();
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, processor);

        app.state.cursor_x = 200.0;
        app.state.cursor_y = 50.0;
        app.enqueue_pointer_event(PointerEventKind::Down);
        app.enqueue_pointer_event(PointerEventKind::Up);
        assert!(app.state.input_processor.is_composer_active());

        for ch in ["a", "b", "c", "d", "e", "f"] {
            app.state.input_processor.route_character_to_composer(ch);
        }

        let layout = ComposerVisualLayout {
            lines: vec![ComposerVisualLine {
                start_byte: 0,
                end_byte: 6,
                glyph_x: vec![
                    (0, 0.0),
                    (1, 5.0),
                    (2, 15.0),
                    (3, 30.0),
                    (4, 45.0),
                    (5, 60.0),
                    (6, 75.0),
                ],
            }],
            text_len: 6,
            input_box: None,
            h_scroll_px: 20.0,
        };
        *app.state.composer_visual_layout.lock().unwrap() = Some(layout);

        app.state.cursor_x = 26.0;
        app.state.cursor_y = 10.0;
        app.enqueue_pointer_event(PointerEventKind::Down);
        let snapshot = app
            .state
            .input_processor
            .composer_draft_snapshot()
            .expect("composer draft must still be active");
        assert_eq!(
            (snapshot.1, snapshot.2),
            (4, 4),
            "pointer-down at screen text_x=20 with h_scroll_px=20 must resolve \
             to draft-space x=40 -> byte 4, not byte 2 (the wrong answer a \
             missing h_scroll correction would produce)"
        );
    }
}
