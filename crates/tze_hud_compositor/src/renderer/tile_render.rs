//! Tile and node rendering methods for the compositor.
//!
//! Moved from `renderer/mod.rs` (the "Drag-boost helpers" + tile/node
//! rendering cluster, formerly ~L7900–8213 at plan date) by Step R-9 of the
//! renderer module split (hud-fgryk).  No logic was changed; `pub(super)`
//! visibility was added to the methods called from sibling modules or from
//! `mod.rs`:
//!
//! - `effective_tile_z_order` — called by tests in `mod.rs`
//! - `effective_tile_opacity` — called by tests in `mod.rs`
//! - `sort_tiles_with_drag_boost` — called by `render_frame*` in `mod.rs`
//! - `tile_background_color` — called by `render_frame*` in `mod.rs`
//! - `render_composer_overlay` — called by `render_frame*` in `mod.rs`
//! - `render_node` — called by `render_frame*` in `mod.rs`
//! - `collect_composer_text_item` — called by `collect_text_items` in `text.rs`
//!
//! ## Methods in this file
//!
//! - `effective_tile_z_order` — effective sort key for a tile, with drag boost.
//! - `effective_tile_opacity` — effective opacity for a tile, with drag boost.
//! - `sort_tiles_with_drag_boost` — re-sort tiles by effective z-order.
//! - `tile_background_color` — background fill color for a tile from design tokens.
//! - `render_composer_overlay` — geometry for the local composer echo overlay.
//! - `tile_contains_composer_node` — search tile subtree for a composer HitRegion.
//! - `node_tree_contains` — depth-first search for a node in a subtree.
//! - `collect_composer_text_item` — build a TextItem for the composer draft text.
//! - `render_node` — render a node and its children within a tile.

use std::sync::Arc;

use glyphon::FontSystem;
use tze_hud_input::{DRAG_OPACITY_BOOST, DRAG_Z_ORDER_BOOST};
use tze_hud_scene::DegradationLevel;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;

use crate::pipeline::{RectVertex, rect_vertices};

use super::ViewerEchoEntry;
use super::draw_cmds::{TexturedDrawCmd, compute_fit_mode};
use super::image_cache::{
    ComposerLayout, caret_visible_at, composer_region_fit_lines, composer_scroll_offset,
    composer_vertical_line_offset, composer_visible_line_count,
};
use super::token_colors::{
    ComposerOverlayTokens, ComposerVerticalAnchor, TILE_BG_DEFAULT, TILE_BG_STATIC_IMAGE,
    TILE_BG_TEXT_MARKDOWN, linear_to_srgb, resolve_composer_overlay_tokens,
    resolve_focus_ring_tokens, resolve_resize_grip_tokens, resolve_section_gap_px,
    resolve_tile_bg_token, resolve_tile_spacing_tokens, resolve_viewer_echo_tokens, srgb_to_linear,
};
use super::{Compositor, CompositorDegradationPolicy};

/// Apply the DisableTransparency degradation rule without changing visibility.
///
/// Level 3 makes every *visible* tile opaque, but zero remains the semantic
/// signal for a hidden/minimized tile (and for the zero endpoint of a portal
/// transition). Turning zero into one would resurrect content the scene graph
/// deliberately removed from the frame.
#[inline]
pub(super) fn degradation_tile_opacity(opacity: f32, level: DegradationLevel) -> f32 {
    if level >= DegradationLevel::Simplified && opacity > 0.0 {
        1.0
    } else {
        opacity
    }
}

/// Renderer adapter for the scene-owned scroll classification.
///
/// Pointer mapping uses the same `SceneGraph` decision, keeping fixed portal
/// chrome and scrolling document coordinates aligned after resize (hud-jeqop).
pub(super) fn node_uses_display_tile_scroll(
    scene: &SceneGraph,
    tile: &Tile,
    node_id: SceneId,
) -> bool {
    scene.node_uses_display_tile_scroll(tile.id, node_id)
}

// The composer's content inset (historically the `COMPOSER_TEXT_MARGIN = 6.0`
// literal) is now token-driven: it resolves from the shared
// `portal.spacing.content_inset_px` token into
// [`ComposerOverlayTokens::content_inset_px`] (hud-ar10c) and is threaded through
// the composer geometry (`composer_input_box`, the caret-follow window, the draft
// `pixel_x`/clip, and `viewer_echo_zone_width`) so no spacing literal survives in
// the composer render path. The token defaults to 6.0, reproducing the prior
// spacing exactly (no visual regression). This is caret-follow-geometry-sensitive:
// the caret x-origin is `region.x + content_inset`, so the inset and the caret
// stay in lockstep.

/// Compute divider rectangles for transcript turn separators (hud-nx7yq.4).
///
/// Pure geometry: for each thematic-break byte offset in `breaks`, count the
/// newlines in `plain[..offset]` to find the break's blank-line index, then place
/// a `thickness`-tall full-width rule centred vertically within that line. The
/// origin (`origin_x`, `origin_y`) is the node's top-left in display space (tile
/// and node bounds, minus scroll); `width` is the node width. Returns
/// absolute-space `Rect`s the caller clips to the tile and fills with the token
/// divider color.
///
/// Kept free-standing (no `self`, no GPU) so the line-counting / centring math is
/// unit-testable without a headless compositor.
pub(super) fn transcript_separator_rects(
    plain: &str,
    breaks: &[usize],
    origin_x: f32,
    origin_y: f32,
    width: f32,
    line_height: f32,
    thickness: f32,
) -> Vec<Rect> {
    let w = width.max(0.0);
    if w <= 0.0 || thickness <= 0.0 {
        return Vec::new();
    }
    let mut rects = Vec::with_capacity(breaks.len());
    for &offset in breaks {
        let clamped = offset.min(plain.len());
        // Line index of the divider's (blank) line = newlines before it.
        let lines_before = plain[..clamped].chars().filter(|&c| c == '\n').count();
        // Centre the rule within its line, then offset up by half its thickness.
        let center_y = (lines_before as f32 + 0.5) * line_height;
        let y = origin_y + center_y - thickness / 2.0;
        rects.push(Rect::new(origin_x, y, w, thickness));
    }
    rects
}

/// Compute divider rectangles between adjacent viewer-echo entries (hud-hsc1t).
///
/// The runtime-authored viewer reply echo (hud-nx7yq.3) renders its retained
/// entries as a single wrapped block bottom-anchored above the composer box.
/// Unlike the adapter transcript — which encodes entry boundaries as `---`
/// thematic breaks the markdown pass turns into dividers — the echo block joins
/// its entries with plain `\n`, so without this helper the pilot-path viewer
/// history reads as one undivided run (the "no dividers between history entries"
/// live report).
///
/// Given the per-entry WRAPPED visual-line counts (oldest→newest), this places a
/// `thickness`-tall rule on the boundary line between each adjacent pair — the
/// same token-styled divider the transcript turn separators use (§Transcript Turn
/// Separators). `block_top` is the display-space y of the block's first line;
/// `[band_top, band_bottom]` is the visible band above the composer box, so a
/// boundary whose history has scrolled out of the band is dropped (matching the
/// oldest-clips-first text bound). N entries yield at most N−1 dividers; the
/// separators are content-free geometry and reveal nothing under redaction.
///
/// Free-standing (no `self`, no GPU) so the cumulative-line math is unit-testable
/// without a headless compositor, mirroring [`transcript_separator_rects`].
// The args are the per-entry line counts plus the block/band geometry scalars;
// bundling them into a struct would add indirection without reducing fan-out.
#[allow(clippy::too_many_arguments)]
pub(super) fn viewer_echo_divider_rects(
    entry_line_counts: &[usize],
    origin_x: f32,
    block_top: f32,
    width: f32,
    line_height: f32,
    thickness: f32,
    band_top: f32,
    band_bottom: f32,
) -> Vec<Rect> {
    let w = width.max(0.0);
    if w <= 0.0 || thickness <= 0.0 || entry_line_counts.len() < 2 {
        return Vec::new();
    }
    let mut rects = Vec::with_capacity(entry_line_counts.len() - 1);
    let mut cumulative = 0usize;
    // Only interior boundaries: skip the final entry (no divider after the newest).
    for count in &entry_line_counts[..entry_line_counts.len() - 1] {
        cumulative += *count;
        // The boundary sits at the top of the next entry's first line.
        let boundary_y = block_top + cumulative as f32 * line_height;
        if boundary_y < band_top || boundary_y > band_bottom {
            continue;
        }
        rects.push(Rect::new(
            origin_x,
            boundary_y - thickness / 2.0,
            w,
            thickness,
        ));
    }
    rects
}

/// Display-space y of the input-history block's first (oldest wrapped) line,
/// given the band geometry and the input tile's clamped vertical scroll offset
/// (hud-acfvp).
///
/// The runtime-authored viewer-echo history is a bounded window in the band
/// `[band_top, band_bottom]` directly above the composer input box
/// (`band_bottom == draft_box.y`). At the **tail** — the resting state — the
/// block is bottom-aligned: its last (newest) line sits on `band_bottom` and the
/// oldest lines clip off `band_top` (the pre-scroll newest-fit window). Scrolling
/// the input tile UP eases its displayed vertical scroll offset DOWN from the
/// tail toward `0`, which slides the whole block DOWN inside the fixed band and
/// reveals older lines; at the fully-scrolled bound the oldest line rests on
/// `band_top`.
///
/// `scroll_offset_y` is the tile's *displayed* vertical scroll offset. It moves
/// this history document inside its fixed composer band; geometry-only resident
/// portal chrome (caret, pane background, focus ring) remains tile-anchored.
/// With no scroll config the block pins to the tail, reproducing the prior
/// newest-fit window byte-for-byte. The offset is clamped to
/// `[0, max_scrollback]` where
/// `max_scrollback = (block_height - band_height).max(0)`, so the window can
/// never overscroll past the oldest line or below the tail.
///
/// Free-standing (no `self`, no GPU) so the scroll math is unit-testable without
/// a headless compositor, mirroring [`viewer_echo_divider_rects`].
pub(super) fn input_history_block_top(
    band_top: f32,
    band_bottom: f32,
    block_height: f32,
    scroll_offset_y: Option<f32>,
) -> f32 {
    // The tail is always bottom-aligned: the block's bottom sits on the band
    // bottom regardless of whether the history overflows the band, so a history
    // that fits keeps its prior bottom-aligned position exactly.
    let tail_top = band_bottom - block_height;
    let band_height = (band_bottom - band_top).max(0.0);
    let max_scrollback = (block_height - band_height).max(0.0);
    // Resting scroll-back is the tail (max): a scrollable input tile seeds its
    // offset to the tail and eases it toward 0 as the viewer scrolls up. A tile
    // with no scroll config has no offset to read, so it pins to the tail.
    let scrollback = scroll_offset_y
        .map(|o| o.clamp(0.0, max_scrollback))
        .unwrap_or(max_scrollback);
    // Reveal older lines by sliding the block DOWN from the tail as the offset
    // eases below the tail (max_scrollback). At the fully-scrolled bound
    // (offset 0) the oldest line rests on the band top; when the history fits the
    // band `max_scrollback` is 0 and the block never moves.
    tail_top + (max_scrollback - scrollback)
}

/// The input-history block geometry — the visible band `[band_top, band_bottom]`
/// and the display-space `block_top` of its first line — for a composer whose
/// live input box is `draft_box` within `region`, selected by the composer
/// `anchor` (hud-3nus3).
///
/// The history always hugs the composer input box on the side AWAY from its
/// anchored edge, so a viewer's submissions read adjacent to the box they typed
/// in — never off-pane:
/// - [`ComposerVerticalAnchor::Bottom`] (default profile) — the box rests on the
///   region's BOTTOM edge, so the band is the space ABOVE it
///   (`[region.y, draft_box.y]`) and the block bottom-aligns, riding the newest
///   reply just above the box (hud-nx7yq.3 / hud-acfvp scroll).
/// - [`ComposerVerticalAnchor::Top`] — the box rests on the region's TOP edge
///   (exemplar two-pane input pane, hud-nottc), so the band is the space BELOW it
///   (`[draft_box.bottom, region.bottom]`) and the block top-aligns, flowing
///   submissions DOWNWARD beneath the box to match the anchor=Top draft-growth
///   direction. Reusing the bottom-anchored band here would measure a zero-height
///   band above the top-pinned box, and the whole history would silently fail to
///   paint (the hud-3nus3 live report: input tracked, nothing rendered). Input-tile
///   scroll for this profile is deferred to hud-acfvp, so the block pins to the
///   band top at rest.
///
/// Returns `None` when the band is degenerate (non-positive height), so both the
/// text and divider passes short-circuit identically. Free-standing (no `self`,
/// no GPU) so the anchor geometry is unit-testable without a headless compositor,
/// mirroring [`input_history_block_top`].
pub(super) fn input_history_band_layout(
    anchor: ComposerVerticalAnchor,
    region: Rect,
    draft_box: Rect,
    block_height: f32,
    scroll_offset_y: Option<f32>,
) -> Option<(f32, f32, f32)> {
    let (band_top, band_bottom) = match anchor {
        ComposerVerticalAnchor::Bottom => (region.y, draft_box.y),
        ComposerVerticalAnchor::Top => (draft_box.y + draft_box.height, region.y + region.height),
    };
    if band_bottom - band_top <= 0.0 {
        return None;
    }
    let block_top = match anchor {
        ComposerVerticalAnchor::Bottom => {
            input_history_block_top(band_top, band_bottom, block_height, scroll_offset_y)
        }
        // Top-anchored history hugs the box from below and flows downward; it pins
        // to the band top at rest (input-tile scroll for this profile is hud-acfvp,
        // so the offset is intentionally unused here).
        ComposerVerticalAnchor::Top => {
            let _ = (block_height, scroll_offset_y);
            band_top
        }
    };
    Some((band_top, band_bottom, block_top))
}

/// Compact "HH:MM " clock prefix for a viewer-echo entry's `submitted_at_wall_us`
/// (hud-7ic89), or `None` when it is `0` — the "no timestamp captured" sentinel
/// already used for absent/invalid submit times elsewhere (see
/// `tze_hud_projection::authority`'s `submitted_at_wall_us == 0` check). `None`
/// lets a legacy append path without a real submit time render its entry with
/// no prefix rather than a misleading "00:00".
///
/// Formatted in UTC rather than local time: the workspace carries no
/// timezone-conversion dependency (chrono/time — confirmed absent from
/// `Cargo.lock`), and a clock-of-day only needs day-seconds modulo arithmetic,
/// so this stays pure `std` with zero new dependencies. If local display is
/// wanted later, this is the single seam to extend.
///
/// Free-standing (no `self`) so the derivation is unit-testable without a
/// headless compositor, mirroring [`viewer_echo_divider_rects`].
pub(super) fn viewer_echo_timestamp_prefix(submitted_at_wall_us: u64) -> Option<String> {
    if submitted_at_wall_us == 0 {
        return None;
    }
    let secs_of_day = (submitted_at_wall_us / 1_000_000) % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    Some(format!("{hour:02}:{minute:02}  "))
}

/// The DISPLAY text for a viewer-echo entry: its timestamp prefix (if any,
/// hud-7ic89) followed by the entry's submitted text.
///
/// Kept separate from [`ViewerEchoEntry::text`] deliberately — the store stays
/// a pure record of what the viewer submitted, and formatting for presentation
/// lives entirely at the render seam.
pub(super) fn viewer_echo_display_text(entry: &ViewerEchoEntry) -> String {
    match viewer_echo_timestamp_prefix(entry.submitted_at_wall_us) {
        Some(prefix) => format!("{prefix}{}", entry.text),
        None => entry.text.clone(),
    }
}

/// Resolve the sRGB-u8 base color for the composer draft line.
///
/// The base color fills every draft glyph not covered by a more specific styled
/// run (caret / selection), so it is effectively the per-line color of the draft
/// text:
/// - a placeholder hint -> the dimmed `portal.composer.placeholder_color`;
/// - a live draft **at capacity** -> the token-driven
///   `portal.composer.at_capacity_color` (hud-9gyao). This is the precise,
///   bounded, per-line at-capacity treatment that replaced the Phase-1
///   zero-length color-run sentinel: the whole draft line takes the at-capacity
///   hue the instant the byte cap is hit, reinforced by the 2px left-edge accent
///   in [`Compositor::render_composer_overlay`];
/// - a normal live draft -> `portal.composer.text_color`.
///
/// All three resolve from design tokens -- no literal color in the render path
/// (section 6.1). Placeholder wins over at-capacity: a placeholder only shows for
/// an EMPTY draft, which can never be at capacity, but resolving it first keeps
/// the dimmed hint from ever flipping to the at-capacity hue.
pub(super) fn composer_draft_base_color(
    tokens: &ComposerOverlayTokens,
    at_capacity: bool,
    is_placeholder: bool,
) -> [u8; 4] {
    // Convert linear-sRGB floats -> sRGB u8 for TextItem (matches rgba_to_srgb_u8
    // in text.rs: RGB channels go through the sRGB transfer curve; alpha is linear).
    let to_srgb_u8 = |v: f32| (linear_to_srgb(v.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
    let to_alpha_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    if is_placeholder {
        tokens.placeholder_color
    } else if at_capacity {
        [
            to_srgb_u8(tokens.at_capacity_r),
            to_srgb_u8(tokens.at_capacity_g),
            to_srgb_u8(tokens.at_capacity_b),
            to_alpha_u8(tokens.at_capacity_a),
        ]
    } else {
        [
            to_srgb_u8(tokens.text_r),
            to_srgb_u8(tokens.text_g),
            to_srgb_u8(tokens.text_b),
            to_alpha_u8(tokens.text_a),
        ]
    }
}

impl Compositor {
    /// Install the complete runtime-selected policy for the next frame.
    pub fn set_degradation_policy(&mut self, policy: CompositorDegradationPolicy) {
        let texture_policy_changed = (self.degradation_policy.level < DegradationLevel::Simplified)
            != (policy.level < DegradationLevel::Simplified)
            || self.degradation_policy.texture_quality_threshold_px
                != policy.texture_quality_threshold_px
            || self.degradation_policy.texture_scale_factor != policy.texture_scale_factor;
        if texture_policy_changed {
            self.image_texture_cache.clear();
        }
        self.degradation_level = policy.level;
        self.degradation_policy = policy;
    }

    /// Visible tiles for this frame. Degradation never hides tiles.
    pub(super) fn policy_visible_tiles<'a>(&self, scene: &'a SceneGraph) -> Vec<&'a Tile> {
        scene.visible_tiles()
    }

    pub(super) fn rendered_tile_opacity(&self, tile: &Tile, scene: &SceneGraph) -> f32 {
        degradation_tile_opacity(
            Self::effective_tile_opacity(tile, scene),
            self.degradation_policy.level,
        )
    }

    /// Whether flat geometry must bypass alpha blending for this frame.
    ///
    /// Overlay composition always writes RGBA directly. Level 3 and above use
    /// the same no-blend path to implement the degradation ladder's
    /// DisableTransparency contract.
    pub(super) fn use_opaque_rect_pipeline(&self) -> bool {
        self.overlay_mode || self.degradation_policy.level >= DegradationLevel::Simplified
    }

    // ─── Drag-boost helpers ───────────────────────────────────────────────────

    /// Return the effective sort key for a tile, applying `DRAG_Z_ORDER_BOOST`
    /// when the tile is in the `Activated` drag phase.
    ///
    /// The boost raises the dragged tile above its peers in painter's-algorithm
    /// order (back-to-front).  `saturating_add` prevents wraparound for tiles
    /// already near `u32::MAX`.
    ///
    /// Per `tze_hud_input::drag::DRAG_Z_ORDER_BOOST` (0x1000).
    pub(super) fn effective_tile_z_order(tile: &Tile, scene: &SceneGraph) -> u32 {
        if scene.is_drag_active(tile.id) {
            tile.z_order.saturating_add(DRAG_Z_ORDER_BOOST)
        } else {
            tile.z_order
        }
    }

    /// Return the effective opacity for a tile, applying `DRAG_OPACITY_BOOST`
    /// (clamped to 1.0) when the tile is in the `Activated` drag phase.
    ///
    /// `DRAG_OPACITY_BOOST` is currently 1.0 (no visible change), but is applied
    /// faithfully so future changes to the constant take effect without a code
    /// change in the compositor.
    ///
    /// Per `tze_hud_input::drag::DRAG_OPACITY_BOOST`.
    pub(super) fn effective_tile_opacity(tile: &Tile, scene: &SceneGraph) -> f32 {
        if scene.is_drag_active(tile.id) {
            (tile.opacity * DRAG_OPACITY_BOOST).min(1.0)
        } else {
            tile.opacity
        }
    }

    /// Re-sort a slice of tile references by effective z-order (back to front),
    /// applying `DRAG_Z_ORDER_BOOST` to the dragged tile's sort key.
    ///
    /// This does not mutate the scene; it returns a new owned `Vec<&Tile>` with
    /// the drag-boosted ordering.  The original `z_order` fields are unchanged.
    pub(super) fn sort_tiles_with_drag_boost<'a>(
        tiles: Vec<&'a Tile>,
        scene: &SceneGraph,
    ) -> Vec<&'a Tile> {
        let mut sorted = tiles;
        sorted.sort_by_key(|t| Self::effective_tile_z_order(t, scene));
        sorted
    }

    pub(super) fn clip_rect_to_tile(tile: &Tile, rect: Rect) -> Option<Rect> {
        let left = rect.x.max(tile.bounds.x);
        let top = rect.y.max(tile.bounds.y);
        let right = (rect.x + rect.width).min(tile.bounds.x + tile.bounds.width);
        let bottom = (rect.y + rect.height).min(tile.bounds.y + tile.bounds.height);
        if right <= left || bottom <= top {
            return None;
        }
        Some(Rect::new(left, top, right - left, bottom - top))
    }

    pub(super) fn append_clipped_rect_vertices(
        tile: &Tile,
        rect: Rect,
        sw: f32,
        sh: f32,
        color: [f32; 4],
        vertices: &mut Vec<RectVertex>,
    ) {
        let Some(clipped) = Self::clip_rect_to_tile(tile, rect) else {
            return;
        };
        vertices.extend_from_slice(&rect_vertices(
            clipped.x,
            clipped.y,
            clipped.width,
            clipped.height,
            sw,
            sh,
            color,
        ));
    }

    /// Four edge rectangles forming a `width`-px focus ring stroked *inside* the
    /// edges of `region` (top, bottom, left, right).
    ///
    /// Drawing inward (rather than outward) keeps the ring within the focused
    /// region's own bounds, so it never bleeds past the owning tile and is
    /// clipped consistently with the region it decorates. The width is clamped to
    /// half the smaller dimension so a thin region cannot produce inverted rects.
    /// Returns empty edges (zero-size rects are dropped by
    /// `append_clipped_rect_vertices`) when `region` has no area.
    fn focus_ring_edge_rects(region: Rect, width: f32) -> [Rect; 4] {
        let w = width
            .max(0.0)
            .min(region.width / 2.0)
            .min(region.height / 2.0);
        if w <= 0.0 || region.width <= 0.0 || region.height <= 0.0 {
            return [Rect::new(0.0, 0.0, 0.0, 0.0); 4];
        }
        [
            // Top edge.
            Rect::new(region.x, region.y, region.width, w),
            // Bottom edge.
            Rect::new(region.x, region.y + region.height - w, region.width, w),
            // Left edge.
            Rect::new(region.x, region.y, w, region.height),
            // Right edge.
            Rect::new(region.x + region.width - w, region.y, w, region.height),
        ]
    }

    /// Emit the keyboard focus ring for the current focus owner into `vertices`,
    /// for the chrome-layer pass drawn above all agent content (hud-k6yvb).
    ///
    /// Reads the runtime-plumbed [`focus_ring::FocusRingOwner`] (drained into
    /// `self.focus_ring_owner`). Draws nothing when focus is cleared, on a
    /// non-active tab, or the owning tile is gone. The ring covers BOTH owner
    /// kinds a keyboard user can land on:
    /// - **node** focus → a ring around the node's display-space bounds (with the
    ///   tile's scroll offset applied so it tracks the visibly-rendered node);
    /// - **tile-level** focus (a non-passthrough tile with no focusable nodes) →
    ///   a ring around the whole tile.
    ///
    /// Token-driven color/width; clipped to the owning tile so a scrolled-off
    /// node's ring never bleeds outside the portal. Overlay-safe (same
    /// `gpu_color_raw` flat-rect path as the hover/press tints).
    pub(super) fn append_focus_ring_vertices(
        &self,
        scene: &SceneGraph,
        vertices: &mut Vec<RectVertex>,
        sw: f32,
        sh: f32,
    ) {
        let Some(owner) = self.focus_ring_owner else {
            return;
        };
        // Only the active tab's focus draws a ring (focus is per-tab).
        if scene.active_tab != Some(owner.tab_id) {
            return;
        }
        let Some(tile) = scene.tiles.get(&owner.tile_id) else {
            return;
        };

        let region = match owner.node_id {
            Some(node_id) => {
                let Some(node) = scene.nodes.get(&node_id) else {
                    return;
                };
                let NodeData::HitRegion(hr) = &node.data else {
                    return;
                };
                let (scroll_x, scroll_y) = if node_uses_display_tile_scroll(scene, tile, node_id) {
                    self.display_tile_scroll_offset(scene, owner.tile_id)
                } else {
                    (0.0, 0.0)
                };
                Rect::new(
                    tile.bounds.x + hr.bounds.x - scroll_x,
                    tile.bounds.y + hr.bounds.y - scroll_y,
                    hr.bounds.width,
                    hr.bounds.height,
                )
            }
            None => tile.bounds,
        };

        let ring = resolve_focus_ring_tokens(&self.token_map);
        let ring_color = self.gpu_color_raw(ring.color);
        for edge in Self::focus_ring_edge_rects(region, ring.width_px) {
            Self::append_clipped_rect_vertices(tile, edge, sw, sh, ring_color, vertices);
        }
    }

    /// Emit the composer caret as a thin, token-styled, ZERO-WIDTH-RELATIVE
    /// vertical quad into `vertices`, for the chrome-layer pass drawn above the
    /// composer draft text — the SAME layer/primitive as the keyboard focus ring
    /// (`append_focus_ring_vertices`, immediately above) (hud-hxhnt finding 2).
    ///
    /// Replaces the previous approach of inserting a `▌` (U+258C) glyph into the
    /// rendered draft string: that glyph occupied width, so every blink toggle
    /// shifted every trailing character by one glyph-width (visible jitter). The
    /// draft text collected by `collect_composer_text_item` is now shaped RAW —
    /// blink-invariant — and this quad marks the caret position independently, so
    /// toggling it on/off never moves a single text pixel.
    ///
    /// No-op (emits nothing) when:
    /// - no composer is active, or its owning tile cannot be located this frame;
    /// - the empty-draft placeholder hint is showing (mirrors
    ///   `collect_composer_text_item`'s placeholder gate — the placeholder is a
    ///   static hint with no caret, hud-evk0j);
    /// - the blink phase is hidden AND no selection is active. While a selection
    ///   is active the caret marks the moving selection edge, so it stays solid
    ///   (mirrors the pre-hud-hxhnt "recolor caret glyph" gate exactly).
    ///
    /// Geometry:
    /// - **Single-line profile** (`self.composer_layout.wrap == false`): x is the
    ///   raw caret x stashed on `composer_layout.caret_x` by
    ///   `prime_composer_scroll_offset` (from `measure_composer_caret`), shifted by
    ///   the same horizontal caret-follow scroll the draft text uses
    ///   (`h_scroll_px`) so the quad and the text never drift apart. y/height span
    ///   the single input line.
    /// - **Multi-line profile** (`wrap == true`): x/row come from the published
    ///   `self.composer_visual_layout` (the hud-21o6x reverse channel) —
    ///   `x_at_cursor` / `line_of` — the SAME measurement the input layer's
    ///   soft-wrap vertical-caret-movement uses, so the quad can never disagree
    ///   with where the text pipeline actually painted the row. A stale layout
    ///   (`text_len` mismatch against the current draft) is skipped rather than
    ///   drawn at a wrong position, mirroring the input layer's own staleness
    ///   guard.
    pub(super) fn append_composer_caret_vertices(
        &self,
        scene: &SceneGraph,
        vertices: &mut Vec<RectVertex>,
        sw: f32,
        sh: f32,
    ) {
        let Some(ref cs) = self.local_composer else {
            return;
        };
        // Placeholder path: the empty-draft dimmed hint carries no caret (mirrors
        // collect_composer_text_item's placeholder gate exactly).
        if cs.text.is_empty() && cs.placeholder.as_deref().is_some_and(|p| !p.is_empty()) {
            return;
        }
        let has_selection = cs.selection_anchor != cs.cursor_byte;
        if !has_selection && !caret_visible_at(self.composer_caret_blink_start.elapsed()) {
            return;
        }

        // Locate the tile + region the same way collect_composer_text_item /
        // prime_composer_scroll_offset do: the first visible tile (by drag-boosted
        // z-order) whose subtree contains the focused composer node.
        let Some(tile) = Self::sort_tiles_with_drag_boost(self.policy_visible_tiles(scene), scene)
            .into_iter()
            .find(|t| Self::composer_region_bounds(t, scene, cs.node_id).is_some())
        else {
            return;
        };
        let Some(region) = Self::composer_region_bounds(tile, scene, cs.node_id) else {
            return;
        };

        let tokens = resolve_composer_overlay_tokens(&self.token_map);
        let font_size_px = self.scaled_portal_font(tokens.font_size_px, tile.id, scene);
        let line_height_multiplier =
            crate::markdown::MarkdownTokens::default().line_height_multiplier;
        let layout = self.composer_layout;
        let input_box = Self::composer_input_box(
            region,
            font_size_px,
            line_height_multiplier,
            layout.visible_lines,
            tokens.anchor,
            tokens.content_inset_px,
        );
        let line_height = (font_size_px * line_height_multiplier).max(1.0);
        let content_inset = tokens.content_inset_px;

        // Caret x/y, box-local (relative to the input box's top-left interior).
        let (caret_x, caret_y, caret_h) = if layout.wrap {
            // Multi-line profile: locate the caret's visual row + x via the
            // published ComposerVisualLayout (hud-21o6x channel) — the SAME
            // measurement the input layer's soft-wrap navigation uses.
            let Ok(guard) = self.composer_visual_layout.lock() else {
                return;
            };
            let Some(visual) = guard.as_ref() else {
                return;
            };
            if visual.text_len != cs.text.len() {
                // Stale layout for this frame's draft — skip rather than draw at a
                // wrong position (mirrors the input layer's staleness guard).
                return;
            }
            let cursor = cs.cursor_byte.min(cs.text.len());
            let Some(line_idx) = visual.line_of(cursor) else {
                return;
            };
            let x = visual.x_at_cursor(cursor);
            let y = (line_idx as f32) * line_height - layout.vscroll_px;
            (x, y, line_height)
        } else {
            // Single-line profile: caret_x was measured once in
            // prime_composer_scroll_offset and stashed on composer_layout, shifted
            // by the same horizontal caret-follow scroll the draft text uses.
            let x = layout.caret_x - layout.h_scroll_px;
            let one_line = (input_box.height - content_inset * 2.0).max(1.0);
            (x, 0.0, one_line)
        };

        if !caret_x.is_finite() || !caret_y.is_finite() {
            return;
        }

        let caret_width = tokens.caret_width_px.max(0.5);
        let rect = Rect::new(
            input_box.x + content_inset + caret_x,
            input_box.y + content_inset + caret_y,
            caret_width,
            caret_h,
        );

        // caret_color is stored sRGB u8 (StyledRunItem-encoding, hud-khfgx); the
        // flat-rect quad pipeline wants linear f32 like the focus ring / resize
        // grip, so round-trip it back through srgb_to_linear.
        let [cr, cg, cb, ca] = tokens.caret_color;
        let to_linear_u8 = |v: u8| srgb_to_linear(v as f32 / 255.0);
        let caret_color = self.gpu_color_raw([
            to_linear_u8(cr),
            to_linear_u8(cg),
            to_linear_u8(cb),
            ca as f32 / 255.0,
        ]);
        Self::append_clipped_rect_vertices(tile, rect, sw, sh, caret_color, vertices);
    }

    /// Dot rects for the portal resize-grip affordance: a diagonal dot-grid mark
    /// in the bottom-right corner of `region`, occupying a `size_px` square.
    ///
    /// The dots form the lower-right triangle of a 3×3 grid (cells where
    /// `col + row >= 2`), so the mark reads as a diagonal grip pointing at the
    /// bottom-right resize corner — the conventional window-grip glyph:
    ///
    /// ```text
    ///     · · ●
    ///     · ● ●
    ///     ● ● ●
    /// ```
    ///
    /// Each dot is a square inset within its grid cell. Returns all-zero rects
    /// (dropped by [`Self::append_clipped_rect_vertices`], which discards
    /// zero-area rects) when `size_px <= 0` so callers need no separate guard.
    pub(super) fn resize_grip_dot_rects(region: Rect, size_px: f32) -> [Rect; 6] {
        let empty = [Rect::new(0.0, 0.0, 0.0, 0.0); 6];
        if !size_px.is_finite() || size_px <= 0.0 || region.width <= 0.0 || region.height <= 0.0 {
            return empty;
        }
        // Anchor the grip square at the bottom-right corner of the region.
        let grip_x = region.x + region.width - size_px;
        let grip_y = region.y + region.height - size_px;
        let cell = size_px / 3.0;
        // Dot square inset within its cell (half the cell, centred).
        let dot = cell * 0.5;
        let dot_off = (cell - dot) * 0.5;

        // Lower-right triangle cells (col + row >= 2) of the 3×3 grid, in a
        // fixed order so the emitted quads are deterministic for tests.
        const CELLS: [(u8, u8); 6] = [(2, 0), (1, 1), (2, 1), (0, 2), (1, 2), (2, 2)];
        let mut rects = empty;
        for (i, (col, row)) in CELLS.iter().enumerate() {
            let cell_x = grip_x + f32::from(*col) * cell;
            let cell_y = grip_y + f32::from(*row) * cell;
            rects[i] = Rect::new(cell_x + dot_off, cell_y + dot_off, dot, dot);
        }
        rects
    }

    /// Emit the portal resize-grip affordance (vd-crude-resize-handle-grip) into
    /// `vertices`: a token-colored dot-grid mark at the bottom-right resize
    /// corner of every visible portal (scrollable) tile.
    ///
    /// Portal tiles are the only resizable surfaces — the pointer resize bands
    /// are scoped to the focused portal (see `tze_hud_input::hit_affordance`) —
    /// so the grip is drawn only on tiles that carry a runtime scroll config.
    /// Geometry-only; it carries no transcript content and is redaction-safe.
    ///
    /// The mark is sized from `portal.window.resize_grip.size_px` and colored
    /// from `portal.window.resize_grip.color`. The pointer-hover tint
    /// (`hover_color`) is selected via
    /// [`ResizeGripTokens::mark_color`](super::token_colors::ResizeGripTokens::mark_color):
    /// the tile named by the runtime-plumbed [`resize_grip_hover`] slot — the
    /// focused portal whose bottom-right resize corner the pointer is over
    /// (hud-wgiys) — renders in `hover_color`; every other grip stays resting.
    /// Dots are clipped to the owning tile so an undersized tile never bleeds the
    /// mark outside its bounds.
    ///
    /// [`resize_grip_hover`]: super::Compositor::resize_grip_hover
    pub(super) fn append_resize_grip_vertices(
        &self,
        scene: &SceneGraph,
        vertices: &mut Vec<RectVertex>,
        sw: f32,
        sh: f32,
    ) {
        let grip = resolve_resize_grip_tokens(&self.token_map);
        if grip.size_px <= 0.0 {
            return;
        }
        // Resolve both tints once; the per-tile hover slot selects between them.
        let resting = self.gpu_color_raw(grip.mark_color(false));
        let hover = self.gpu_color_raw(grip.mark_color(true));
        for tile in self.policy_visible_tiles(scene) {
            // Only portal (scrollable) tiles are resizable.
            if scene.tile_scroll_config(tile.id).is_none() {
                continue;
            }
            let color = if self.resize_grip_hover == Some(tile.id) {
                hover
            } else {
                resting
            };
            for dot in Self::resize_grip_dot_rects(tile.bounds, grip.size_px) {
                Self::append_clipped_rect_vertices(tile, dot, sw, sh, color, vertices);
            }
        }
    }

    fn clipped_textured_rect(
        tile: &Tile,
        rect: Rect,
        uv_rect: [f32; 4],
    ) -> Option<(Rect, [f32; 4])> {
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return None;
        }
        let clipped = Self::clip_rect_to_tile(tile, rect)?;
        if clipped.x == rect.x
            && clipped.y == rect.y
            && clipped.width == rect.width
            && clipped.height == rect.height
        {
            return Some((rect, uv_rect));
        }
        let left_frac = ((clipped.x - rect.x) / rect.width).clamp(0.0, 1.0);
        let right_frac = ((clipped.x + clipped.width - rect.x) / rect.width).clamp(0.0, 1.0);
        let top_frac = ((clipped.y - rect.y) / rect.height).clamp(0.0, 1.0);
        let bottom_frac = ((clipped.y + clipped.height - rect.y) / rect.height).clamp(0.0, 1.0);

        let [u0, v0, u1, v1] = uv_rect;
        let du = u1 - u0;
        let dv = v1 - v0;
        Some((
            clipped,
            [
                u0 + du * left_frac,
                v0 + dv * top_frac,
                u0 + du * right_frac,
                v0 + dv * bottom_frac,
            ],
        ))
    }

    /// Effective per-frame render opacity for a tile: scene-level tile opacity
    /// (with drag boost) multiplied by the §6.3 portal-transition fade opacity
    /// for scrollable (portal) tiles, clamped to `[0, 1]`.
    ///
    /// This is the single source of truth for "how faded is the whole tile this
    /// frame". It MUST be applied uniformly to every element that composes the
    /// tile's opaque body — both the flat tile backdrop ([`tile_background_color`])
    /// and the content-node backgrounds painted in [`render_node`] — so the tile
    /// backdrop fades as a single unit. (The text pass fades on the portal-fade
    /// component alone via `portal_tile_anim_opacity`, which for a portal — where
    /// `tile.opacity` stays 1 — equals this value.) A backdrop site that omits it
    /// paints at full opacity while its neighbours fade, leaving that region
    /// see-through relative to the rest of the tile on any fade or geometry change
    /// that exposes it (hud-w41ef).
    pub(super) fn tile_effective_opacity(&self, tile: &Tile, scene: &SceneGraph) -> f32 {
        // Base opacity from drag state (scene-level tile.opacity × drag boost).
        let base_opacity = self.rendered_tile_opacity(tile, scene);
        // §6.3: multiply portal tile animation opacity for scrollable (portal) tiles.
        let opacity = (base_opacity * self.portal_tile_anim_opacity(tile.id)).clamp(0.0, 1.0);
        degradation_tile_opacity(opacity, self.degradation_policy.level)
    }

    /// Determine the background fill color for a tile based on its root content.
    ///
    /// Colors are resolved from design tokens (`color.tile.background.*`) with
    /// documented fallback constants — no naked color literals here.  The opacity
    /// channel is derived from the tile's effective opacity (drag state) multiplied
    /// by the portal tile animation opacity (§6.3 transition tokens) when the tile
    /// has a registered `TileScrollConfig`.
    ///
    /// Returns `None` when the tile's rounded root node should be solely
    /// responsible for its own backdrop shape.
    pub(super) fn tile_background_color(
        &self,
        tile: &Tile,
        scene: &SceneGraph,
    ) -> Option<[f32; 4]> {
        let opacity = self.tile_effective_opacity(tile, scene);
        if let Some(root_id) = tile.root_node
            && let Some(node) = scene.nodes.get(&root_id)
        {
            match &node.data {
                NodeData::SolidColor(sc) => {
                    if self.degradation_policy.level < DegradationLevel::Simplified
                        && sc.radius.is_some_and(|r| r > 0.0)
                    {
                        return None;
                    }
                    // Apply combined opacity to the solid color alpha.
                    let mut c = sc.color.to_array();
                    c[3] *= opacity;
                    return Some(c);
                }
                NodeData::TextMarkdown(tm) => {
                    if let Some(bg) = &tm.background {
                        // Apply combined opacity to the token-supplied background alpha.
                        let mut c = bg.to_array();
                        c[3] *= opacity;
                        return Some(c);
                    }
                    let c = resolve_tile_bg_token(
                        &self.token_map,
                        "color.tile.background.text_markdown",
                        TILE_BG_TEXT_MARKDOWN,
                    );
                    return Some([c.r, c.g, c.b, opacity]);
                }
                NodeData::HitRegion(_) => {
                    // HitRegion is an invisible interaction primitive. Its visible
                    // footprint is supplied by sibling content nodes; the runtime
                    // only paints local-feedback tints on hover/press (RFC 0004 §6.5).
                    return None;
                }
                NodeData::StaticImage(_) => {
                    let c = resolve_tile_bg_token(
                        &self.token_map,
                        "color.tile.background.static_image",
                        TILE_BG_STATIC_IMAGE,
                    );
                    return Some([c.r, c.g, c.b, opacity]);
                }
            }
        }
        let c = resolve_tile_bg_token(
            &self.token_map,
            "color.tile.background.default",
            TILE_BG_DEFAULT,
        );
        Some([c.r, c.g, c.b, opacity])
    }

    /// Compute the lifecycle-affordance accent bar geometry+color for a tile, or
    /// `None` when nothing should paint.
    ///
    /// Pure (no GPU / no `self`) so it is unit-testable. The bar hugs the tile's
    /// left edge and spans the full tile height; its width is the token-resolved
    /// `accent.width_px` clamped to `[0, tile width]` so an out-of-range token
    /// value can never push it past the tile edge. The returned color is the
    /// token-resolved accent color (no literal visual value here, §6.1) with the
    /// caller-supplied combined tile opacity folded into the alpha channel, so the
    /// accent fades with the tile (matching the tile background).
    ///
    /// Returns `None` for a zero/negative width or non-positive opacity (nothing
    /// visible to draw). Source: hud-m48i0.
    pub(super) fn lifecycle_accent_bar_geom(
        tile_bounds: Rect,
        accent: LifecycleAccent,
        opacity: f32,
    ) -> Option<(f32, [f32; 4])> {
        // Sanitize the dynamic upper bound: f32::clamp panics when min > max, so a
        // negative (or NaN) tile width would otherwise crash the frame loop. `.max(0.0)`
        // pins it to a valid `[0, _]` range before clamping the token-resolved width.
        let bar_w = accent.width_px.clamp(0.0, tile_bounds.width.max(0.0));
        if bar_w <= 0.0 || opacity <= 0.0 {
            return None;
        }
        let mut color = accent.color.to_array();
        color[3] *= opacity;
        Some((bar_w, color))
    }

    /// Emit geometry for the local composer echo overlay.
    ///
    /// Called from the tile loop (after the content pass) when
    /// `self.local_composer` is `Some` and the state's `node_id` belongs to a
    /// `HitRegionNode` with `accepts_composer_input = true` inside the given
    /// tile.  If no matching node is found in the tile the call is a no-op.
    ///
    /// Emits:
    /// 1. A background fill rect for the composer hit-region.
    /// 2. When at_capacity, a 2px left-edge accent rect using the at-capacity
    ///    color so the user has a visual signal that no further input is
    ///    accepted.  The text pass (see `collect_composer_text_item`) renders
    ///    the draft text on top in the same hit-region.
    ///
    /// Geometry is clamped to the tile bounds to avoid overdraw outside the portal.
    pub(super) fn render_composer_overlay(
        &self,
        tile: &Tile,
        scene: &SceneGraph,
        vertices: &mut Vec<RectVertex>,
        sw: f32,
        sh: f32,
        tokens: &ComposerOverlayTokens,
    ) {
        let Some(ref cs) = self.local_composer else {
            return;
        };

        let Some(region) = Self::composer_region_bounds(tile, scene, cs.node_id) else {
            return;
        };
        // Confine the composer chrome to the input box so it does not paint over
        // the whole portal (hud-2zsbf). The box grows upward with the wrapped-line
        // count primed into `composer_layout` (hud-nx7yq.1); `visible_lines == 1`
        // keeps the single-line strip. Kept in lockstep with
        // `collect_composer_text_item`, which anchors the draft to the same box.
        let line_height_multiplier =
            crate::markdown::MarkdownTokens::default().line_height_multiplier;
        let font_size_px = self.scaled_portal_font(tokens.font_size_px, tile.id, scene);
        let input_box = Self::composer_input_box(
            region,
            font_size_px,
            line_height_multiplier,
            self.composer_layout.visible_lines,
            tokens.anchor,
            tokens.content_inset_px,
        );

        // Background fill.
        let bg_color = [tokens.bg_r, tokens.bg_g, tokens.bg_b, tokens.bg_a];
        vertices.extend_from_slice(&rect_vertices(
            input_box.x,
            input_box.y,
            input_box.width,
            input_box.height,
            sw,
            sh,
            bg_color,
        ));

        // At-capacity left-edge accent (2px wide, full box height).
        if cs.at_capacity {
            let accent = [
                tokens.at_capacity_r,
                tokens.at_capacity_g,
                tokens.at_capacity_b,
                tokens.at_capacity_a,
            ];
            vertices.extend_from_slice(&rect_vertices(
                input_box.x,
                input_box.y,
                2.0,
                input_box.height,
                sw,
                sh,
                accent,
            ));
        }
    }

    /// Return display-space bounds for the focused composer HitRegion inside a
    /// tile, clamped to the tile bounds.
    ///
    /// Local composer echo belongs to the composer region, not the bottom edge
    /// of the containing tile.  This keeps runtime-local feedback aligned with
    /// agent-rendered composer chrome in raw-tile portals.
    fn composer_region_bounds(tile: &Tile, scene: &SceneGraph, target: SceneId) -> Option<Rect> {
        let root_id = tile.root_node?;
        let local = Self::node_tree_composer_bounds(root_id, scene, target)?;

        let left = local.x.clamp(0.0, tile.bounds.width);
        let top = local.y.clamp(0.0, tile.bounds.height);
        let right = (local.x + local.width).clamp(0.0, tile.bounds.width);
        let bottom = (local.y + local.height).clamp(0.0, tile.bounds.height);
        if right <= left || bottom <= top {
            return None;
        }

        Some(Rect::new(
            tile.bounds.x + left,
            tile.bounds.y + top,
            right - left,
            bottom - top,
        ))
    }

    /// Confine the rendered composer echo to a single input-line strip at the
    /// BOTTOM of the composer region (hud-2zsbf).
    ///
    /// The composer HitRegion published by the projection authority spans the
    /// WHOLE portal (`resident_grpc::local_bounds_for_state` → `x:0, y:0, w, h`)
    /// so a pointer-down anywhere in the portal focuses the composer (hud-v4k1h).
    /// That full-portal region is the correct *pointer/focus* target, but using
    /// it verbatim as the echo's layout+clip box laid the single unwrapped draft
    /// line across the entire portal width at the portal's TOP — reading live as
    /// "the draft extends forever past the composer box". The clip was effective;
    /// the box was simply the whole portal.
    ///
    /// This derives the actual composer *input box* from that region: one text
    /// line tall (font line-height plus symmetric vertical padding equal to the
    /// horizontal text margin), pinned to the bottom edge where a chat-style
    /// composer belongs. Width and x are unchanged, so horizontal caret-follow is
    /// unaffected; only the vertical placement + clip height shrink to one line.
    /// For a composer region that is already ~one line tall (the intended
    /// promotion-era structured composer node) the strip equals the region, so
    /// behaviour there is unchanged.
    /// Composer input box for `visible_lines` wrapped text lines within `region`.
    ///
    /// Height is `visible_lines` text lines plus symmetric vertical padding (equal
    /// to the horizontal text margin), clamped to the region height.
    /// `visible_lines == 1.0` reproduces the single input-line strip (hud-2zsbf)
    /// exactly, so single-line behaviour is unchanged.
    ///
    /// Vertical placement is selected by `anchor` (hud-nottc):
    /// - [`ComposerVerticalAnchor::Bottom`] (default profile) — the box pins to the
    ///   BOTTOM edge of the region and grows UPWARD (hud-nx7yq.1). Growth is
    ///   viewer-local: the box extends upward over the transcript, which yields the
    ///   space by occlusion; the portal's outer geometry is untouched.
    /// - [`ComposerVerticalAnchor::Top`] — the box pins to the TOP edge of the
    ///   region so the draft caret rests at the pane's top-left content origin when
    ///   empty and the text flows DOWNWARD as it grows, with NO teleport between the
    ///   empty and non-empty states (exemplar two-pane input pane).
    pub(super) fn composer_input_box(
        region: Rect,
        font_size_px: f32,
        line_height_multiplier: f32,
        visible_lines: f32,
        anchor: ComposerVerticalAnchor,
        content_inset_px: f32,
    ) -> Rect {
        let line_height = (font_size_px * line_height_multiplier).max(1.0);
        let lines = visible_lines.max(1.0);
        let box_height = (line_height * lines + content_inset_px * 2.0)
            .min(region.height)
            .max(1.0);
        let box_y = match anchor {
            // Pin to the bottom edge; the box grows upward as `visible_lines` rises.
            ComposerVerticalAnchor::Bottom => region.y + (region.height - box_height).max(0.0),
            // Pin to the top edge; the box grows downward — caret starts at the
            // pane content origin and never teleports when the first glyph arrives.
            ComposerVerticalAnchor::Top => region.y,
        };
        Rect::new(region.x, box_y, region.width, box_height)
    }

    /// Depth-first search for `target` in the node sub-tree rooted at `node_id`.
    fn node_tree_composer_bounds(
        node_id: SceneId,
        scene: &SceneGraph,
        target: SceneId,
    ) -> Option<Rect> {
        if node_id == target {
            // Verify the node is indeed a composer-capable HitRegion.
            return scene.nodes.get(&node_id).and_then(|n| match &n.data {
                NodeData::HitRegion(hr) if hr.accepts_composer_input => Some(hr.bounds),
                _ => None,
            });
        }
        if let Some(node) = scene.nodes.get(&node_id) {
            for child in &node.children {
                if let Some(bounds) = Self::node_tree_composer_bounds(*child, scene, target) {
                    return Some(bounds);
                }
            }
        }
        None
    }

    /// Recompute the active composer's per-frame layout into `self.composer_layout`,
    /// ready for the following `collect_text_items` pass (hud-zlfi4 single-line
    /// caret-follow + hud-nx7yq.1 multi-line wrap / upward growth / vertical scroll).
    ///
    /// Must be called once per frame BEFORE `collect_text_items` (it measures the
    /// draft against the composer font via the mutable text rasterizer, which the
    /// immutable collect path cannot do).  When no composer is active, the text
    /// rasterizer is missing, or the composer region cannot be located, the layout
    /// resets to [`ComposerLayout::default`] (single line, no scroll).
    ///
    /// Two profiles, selected by the `portal.composer.max_lines` token:
    /// - **max_lines == 1** — single-line profile: measure the caret x and pin a
    ///   horizontal scroll offset so the caret stays visible (hud-zlfi4).
    /// - **max_lines > 1** — multi-line profile: wrap-measure the draft to the box
    ///   width, size the box to grow upward to at most `max_lines`, and pin a
    ///   vertical scroll once the draft exceeds that bound.
    ///
    /// The measured window / margin / wrap width here mirror exactly the geometry
    /// `collect_composer_text_item` uses to place the draft. Local presentation
    /// state — no adapter round trip.
    pub(crate) fn prime_composer_scroll_offset(&mut self, scene: &SceneGraph) {
        self.composer_layout = ComposerLayout::default();
        // Clear the reverse visual-layout channel by default (hud-21o6x); only the
        // multi-line branch below republishes a fresh layout. Every early return
        // (no composer / no rasterizer / no region / single-line) leaves it `None`,
        // so the input layer falls back to hard-newline vertical movement.
        if let Ok(mut guard) = self.composer_visual_layout.lock() {
            *guard = None;
        }

        // Gather the immutable inputs first (draft text, caret, region geometry,
        // font size) so the mutable text-rasterizer borrow below does not overlap
        // the `&self` reads.
        let Some(cs) = self.local_composer.as_ref() else {
            return;
        };
        if self.text_rasterizer.is_none() {
            return;
        }

        // Locate the composer region the same way collect_composer_text_item does:
        // the first visible tile whose subtree contains the focused composer node.
        let mut composer_target: Option<(SceneId, Rect)> = None;
        for tile in &Self::sort_tiles_with_drag_boost(self.policy_visible_tiles(scene), scene) {
            if let Some(r) = Self::composer_region_bounds(tile, scene, cs.node_id) {
                composer_target = Some((tile.id, r));
                break;
            }
        }
        let Some((tile_id, region)) = composer_target else {
            return;
        };

        let tokens = resolve_composer_overlay_tokens(&self.token_map);
        let font_size_px = self.scaled_portal_font(tokens.font_size_px, tile_id, scene);
        let max_lines = tokens.max_lines.max(1);
        // Token-driven composer content inset (hud-ar10c); the caret-follow window
        // and keep-visible margin below key off the same value the draft render
        // uses, so caret geometry stays in lockstep with the box padding.
        let content_inset = tokens.content_inset_px;
        // Visible text window = region interior width (region width minus the left
        // and right text margins).  This is the same `bw` collect uses.
        let window_width = (region.width - content_inset * 2.0).max(1.0);
        let line_height_multiplier =
            crate::markdown::MarkdownTokens::default().line_height_multiplier;
        let line_height = (font_size_px * line_height_multiplier).max(1.0);

        // Own the measurement inputs, then drop the `&self` borrow before the
        // mutable rasterizer borrow.
        let text = cs.text.clone();
        let cursor_byte = cs.cursor_byte;

        let Some(tr) = self.text_rasterizer.as_mut() else {
            return;
        };

        if max_lines <= 1 {
            // ── Single-line profile (hud-zlfi4): horizontal caret-follow. ──
            let (caret_x, content_width) =
                tr.measure_composer_caret(&text, cursor_byte, font_size_px, line_height_multiplier);
            self.composer_layout = ComposerLayout {
                wrap: false,
                h_scroll_px: composer_scroll_offset(
                    caret_x,
                    content_width,
                    window_width,
                    content_inset,
                ),
                content_width,
                // Stashed raw (pre-scroll) so the chrome-layer caret quad
                // (append_composer_caret_vertices) can relocate the caret without
                // its own text-rasterizer pass (hud-hxhnt finding 2).
                caret_x,
                visible_lines: 1.0,
                total_lines: 1.0,
                vscroll_px: 0.0,
            };

            // Publish a one-row ComposerVisualLayout for the single-line profile
            // too (hud-hxhnt finding 1): before this, only the multi-line branch
            // below published one, so the runtime's pointer hit-test
            // (`composer_pointer_byte_offset`) fell back to a crude linear
            // byte-fraction guess for every single-line composer. `input_box:
            // None` here is correct (not a gap) — a single row already occupies
            // the whole box, so `byte_at_point`'s even-split fallback is exact.
            let mut visual_layout =
                tr.measure_composer_single_line_layout(&text, font_size_px, line_height_multiplier);
            // Stamp the same-frame caret-follow scroll onto the published layout
            // (hud-hxhnt finding 3) so a consumer that needs the caret's actual
            // on-screen x — the IME anchor — can recover rendered screen space
            // from the layout's unscrolled `glyph_x` by subtracting this. Mirrors
            // `ComposerLayout.h_scroll_px` above exactly (same frame, same value).
            visual_layout.h_scroll_px = self.composer_layout.h_scroll_px;
            if let Ok(mut guard) = self.composer_visual_layout.lock() {
                *guard = Some(visual_layout);
            }
            return;
        }

        // ── Multi-line profile (hud-nx7yq.1): wrap, grow upward, vscroll. ──
        // Measure the RAW draft text (hud-hxhnt): the caret is a chrome-layer
        // quad now, not an inserted glyph, so the rendered text is always the raw
        // draft and this measures the same string the render pass shapes — a
        // zero-width caret at a wrap boundary clips harmlessly (no glyph there).
        let (total_lines, caret_line) = tr.measure_composer_wrapped(
            &text,
            cursor_byte,
            window_width,
            font_size_px,
            line_height_multiplier,
        );
        // Bound growth AND vertical scroll to what the composer REGION actually
        // fits (hud-nottc), not just the `max_lines` token. A short composer pane
        // (e.g. the exemplar's top input strip, ~2 lines) would otherwise grow the
        // box past its bounds while the scroll math — keyed on `max_lines` — left
        // the caret line clipped outside the visible box. `composer_input_box`
        // clamps the box height to the region too, so this keeps visible_lines,
        // the box, and vscroll mutually consistent and the caret always in view.
        let region_fit_lines = composer_region_fit_lines(region.height, line_height, content_inset);
        let effective_max_lines = (max_lines as usize).min(region_fit_lines).max(1);
        let visible_lines = composer_visible_line_count(total_lines, effective_max_lines);
        let first_visible =
            composer_vertical_line_offset(caret_line, total_lines, effective_max_lines);
        let vscroll_px = first_visible as f32 * line_height;
        self.composer_layout = ComposerLayout {
            wrap: true,
            h_scroll_px: 0.0,
            content_width: 0.0,
            // Unused in the multi-line profile: the caret quad's x is located via
            // the published ComposerVisualLayout (below) instead.
            caret_x: 0.0,
            visible_lines: visible_lines as f32,
            total_lines: total_lines as f32,
            vscroll_px,
        };

        // Publish the wrapped VISUAL-LINE layout for the input thread's soft-wrap
        // vertical caret movement (hud-21o6x). Measured on the RAW draft (byte
        // space the caret uses), same wrap width as the render, so the input layer
        // maps caret byte ↔ visual row ↔ pixel x.
        let mut visual_layout = tr.measure_composer_visual_layout(
            &text,
            window_width,
            font_size_px,
            line_height_multiplier,
        );

        // Publish the RENDERED input-box vertical geometry alongside the rows so
        // the input layer's pointer hit-test maps pointer Y through the box the
        // draft actually renders in — NOT the full node height (hud-lw60x). The
        // box is bottom-anchored and short for a full-portal projection composer,
        // so an even split of the node height sends every click to the last row.
        // Derive it from the SAME `composer_input_box` the render path uses so the
        // two cannot drift; node-local space (relative to `region.y`) matches the
        // `local_y` the runtime feeds `byte_at_point`.
        let input_box = Self::composer_input_box(
            region,
            font_size_px,
            line_height_multiplier,
            visible_lines as f32,
            tokens.anchor,
            content_inset,
        );
        visual_layout.input_box = Some(tze_hud_input::ComposerInputBoxGeometry {
            box_top: input_box.y - region.y,
            box_height: input_box.height,
            // Visual row 0's top edge in node-local space, matching the render's
            // `pixel_y = input_box.y + content_inset − vscroll_px` (collect path).
            row0_top: (input_box.y - region.y) + content_inset - vscroll_px,
            line_height,
            // Visible row window: the pointer hit-test clamps into this so a click
            // in the box padding while scrolled cannot land on a clipped row
            // outside the window (hud-lw60x).
            first_visible_row: first_visible,
            visible_rows: visible_lines,
        });

        if let Ok(mut guard) = self.composer_visual_layout.lock() {
            *guard = Some(visual_layout);
        }
    }

    /// Build a [`TextItem`] for the local composer echo draft text.
    ///
    /// Returns `None` when:
    /// - `self.local_composer` is absent (no active composer).
    /// - `self.text_rasterizer` is absent (text path not initialised).
    /// - The tile does not contain the focused composer node.
    ///
    /// The rendered text is the RAW draft (or the placeholder hint) — no caret
    /// glyph is inserted (hud-hxhnt finding 2). The caret is drawn separately as a
    /// chrome-layer quad (`append_composer_caret_vertices`, above all composer
    /// content, mirroring the focus ring), so this text is blink-invariant: toggling
    /// the caret on/off never reflows a single glyph in the draft.
    ///
    /// When a selection is active (`cursor_byte != selection_anchor`), a
    /// [`crate::text::StyledRunItem`] with `background_color` set to
    /// `tokens.selection_bg` is emitted covering the selected byte range — now the
    /// RAW `[min(cursor, anchor), max(cursor, anchor))` range, with no caret-glyph
    /// shift to account for. The text pipeline's `compute_inline_backdrop_quads`
    /// then renders a highlight quad behind the selected characters using
    /// glyph-level geometry — no separate geometry pass is required. The run sets
    /// `fill_line_width` so the text pipeline draws the standard multi-line
    /// text-selection shape (hud-scgyw): partial first/last line, full-width
    /// interior lines.
    pub(super) fn collect_composer_text_item(
        &self,
        tile: &Tile,
        scene: &SceneGraph,
        sw: f32,
        sh: f32,
        tokens: &ComposerOverlayTokens,
    ) -> Option<crate::text::TextItem> {
        let cs = self.local_composer.as_ref()?;
        self.text_rasterizer.as_ref()?;
        let region = Self::composer_region_bounds(tile, scene, cs.node_id)?;
        let layout = self.composer_layout;
        let font_size_px = self.scaled_portal_font(tokens.font_size_px, tile.id, scene);
        // The full HitRegion spans the whole portal (click-anywhere-to-focus,
        // hud-v4k1h); confine the rendered draft to the input box at its bottom
        // edge so it does not stretch across the portal (hud-2zsbf). The box grows
        // upward with the wrapped-line count (hud-nx7yq.1); `visible_lines == 1`
        // reproduces the single input-line strip.
        let input_box = Self::composer_input_box(
            region,
            font_size_px,
            crate::markdown::MarkdownTokens::default().line_height_multiplier,
            layout.visible_lines,
            tokens.anchor,
            tokens.content_inset_px,
        );

        // Empty-draft placeholder (hud-evk0j): when the draft buffer is empty and
        // the composer carries a non-empty placeholder hint, render that hint in
        // the dimmed placeholder color instead of the (empty) draft. The
        // placeholder is render-only — it is not in `cs.text`, so it is never
        // submitted, carries no caret or selection, and vanishes the instant the
        // user types (a non-empty `cs.text` falls through to the normal path).
        let placeholder = if cs.text.is_empty() {
            cs.placeholder.as_deref().filter(|p| !p.is_empty())
        } else {
            None
        };

        let display_text: &str = placeholder.unwrap_or(cs.text.as_str());

        let text_margin = tokens.content_inset_px;

        // Horizontal caret-follow (hud-zlfi4, single-line profile only): shift the
        // draft LEFT by the per-frame scroll offset primed in
        // `prime_composer_scroll_offset` so the caret stays visible once the draft
        // is wider than the box.  Only the draft `pixel_x` moves; the clip stays
        // pinned to the box interior, so overflowing text is clipped (never painted
        // outside the box) and the selection run — byte-anchored relative to
        // `pixel_x` — scrolls with the text.  `0.0` in the multi-line profile,
        // which wraps instead of sliding horizontally.
        let scroll_offset = layout.h_scroll_px;

        // Placeholder text uses the dimmed placeholder token color; a live draft AT
        // CAPACITY uses the token-driven at-capacity color so the whole draft line
        // reads in the at-capacity hue the instant the byte cap is hit; a normal live
        // draft uses the composer text color. The selection styled run below still
        // overrides its own sub-range (hud-9gyao, hud-evk0j).
        let text_color = composer_draft_base_color(tokens, cs.at_capacity, placeholder.is_some());

        let bw = (region.width - text_margin * 2.0).max(1.0);
        let line_height = (font_size_px
            * crate::markdown::MarkdownTokens::default().line_height_multiplier)
            .max(1.0);

        let _ = sw; // retained for API symmetry with other collect helpers
        let _ = sh;

        // Build a selection-highlight styled run when a non-empty selection
        // exists, over the RAW text byte range (hud-hxhnt: no caret glyph, so no
        // +3-byte shift to account for).
        //
        // `cursor_byte` and `selection_anchor` are agent-provided; clamp with
        // `.min(text.len())` so a stale snapshot with an out-of-range anchor
        // cannot panic.
        let styled_runs: Box<[crate::text::StyledRunItem]> = if placeholder.is_some() {
            // Placeholder is a static dimmed hint: no selection highlight — the
            // whole run is the placeholder color (hud-evk0j).
            Box::new([])
        } else {
            let anchor = cs.selection_anchor.min(cs.text.len());
            let cursor = cs.cursor_byte.min(cs.text.len());
            let sel_start = anchor.min(cursor);
            let sel_end = anchor.max(cursor);
            if sel_start < sel_end {
                Box::new([crate::text::StyledRunItem {
                    start_byte: sel_start,
                    end_byte: sel_end,
                    weight: None,
                    italic: false,
                    monospace: false,
                    color: None,
                    background_color: Some(tokens.selection_bg),
                    size_scale: None,
                    // Standard multi-line text-selection shape: full-width
                    // interior lines when the selection wraps (hud-scgyw).
                    fill_line_width: true,
                }])
            } else {
                Box::new([])
            }
        };

        // Per-profile text layout:
        // - Single-line (hud-zlfi4): lay the draft on ONE unwrapped line — layout
        //   width is the wider of the box and the measured content width plus a
        //   small slack margin (sub-pixel shaping-rounding safety; no caret glyph
        //   needs room for any more, hud-hxhnt), so word-wrap never triggers and
        //   the draft slides horizontally + clips instead of wrapping.
        // - Multi-line (hud-nx7yq.1): layout width is the box interior so glyphon
        //   word-wraps within it; layout height is the FULL wrapped-content height
        //   so every line lays out, and the box then clips + vertically scrolls it.
        let (layout_width, bounds_height) = if layout.wrap {
            let content_height = (layout.total_lines * line_height + text_margin * 2.0).max(1.0);
            (bw, content_height)
        } else {
            // One-line interior height — identical to the pre-multiline strip.
            let one_line = (input_box.height - text_margin * 2.0).max(1.0);
            (bw.max(layout.content_width + font_size_px), one_line)
        };

        Some(crate::text::TextItem {
            text: Arc::from(display_text),
            // Shift the draft left by the horizontal caret-follow offset (0 in the
            // multi-line profile).
            pixel_x: region.x + text_margin - scroll_offset,
            // Shift the draft up by the vertical scroll offset (0 in the single-line
            // profile and until the multi-line draft exceeds the max line count).
            pixel_y: input_box.y + text_margin - layout.vscroll_px,
            bounds_width: layout_width,
            bounds_height,
            // Clip stays pinned to the input box interior so scrolled-off text
            // (horizontally or vertically) is clipped at the box edge, never painted
            // outside it. The box grows upward with the wrapped-line count (hud-nx7yq.1).
            clip_pixel_x: region.x + text_margin,
            clip_pixel_y: input_box.y,
            clip_bounds_width: bw.max(1.0),
            clip_bounds_height: input_box.height.max(1.0),
            font_size_px,
            font_family: tze_hud_scene::types::FontFamily::SystemSansSerif,
            font_weight: 400,
            color: text_color,
            alignment: tze_hud_scene::types::TextAlign::Start,
            overflow: tze_hud_scene::types::TextOverflow::Clip,
            outline_color: None,
            outline_width: None,
            opacity: 1.0,
            color_runs: Box::new([]),
            styled_runs,
            line_height_multiplier: crate::markdown::MarkdownTokens::default()
                .line_height_multiplier,
            viewport: crate::overflow::TruncationViewport::HeadAnchored,
        })
    }

    /// Build a `TextItem` for the ambient unread-count badge the jump-to-latest
    /// pill MAY carry (hud-g1ena.3, portal-chat-grade-affordances §Jump-to-Latest
    /// Affordance).
    ///
    /// Returns `None` unless every gate the pill render in `render_frame` uses
    /// also holds — the tile has a registered scroll config with a known content
    /// height, the content overflows (a scroll indicator would render), the
    /// viewport is scrolled away from the tail, and the pill geometry is
    /// renderable — AND the tile carries a nonzero, non-redacted unread count.
    /// Mirroring the pill's gates exactly keeps the badge and pill appearing and
    /// disappearing together; because the pill is gated on `scrolled_back`, the
    /// badge clears the instant the viewer returns to the tail, with no adapter
    /// round trip (local-first).
    ///
    /// The count is centered in the pill and clipped to it, token-styled from the
    /// same [`tze_hud_input::JumpToLatestTokens`] as the pill fill — no literal
    /// color in the render path.
    pub(super) fn collect_jump_to_latest_badge_item(
        &self,
        tile: &Tile,
        scene: &SceneGraph,
        jump_to_latest_tokens: &tze_hud_input::JumpToLatestTokens,
        scroll_indicator_tokens: &tze_hud_input::ScrollIndicatorTokens,
    ) -> Option<crate::text::TextItem> {
        // No rasterizer → no glyphs (headless snapshot tests still exercise the
        // gating via the pure functions this delegates to).
        self.text_rasterizer.as_ref()?;

        let scroll_cfg = scene.tile_scroll_config(tile.id)?;
        let content_height = scroll_cfg.content_height?;
        let viewport_px = tile.bounds.height;
        let (_, scroll_offset_y) = self.display_tile_scroll_offset(scene, tile.id);
        // Only offer the badge where the pill itself renders: content overflows.
        tze_hud_input::compute_scroll_indicator(
            viewport_px,
            content_height,
            scroll_offset_y,
            scroll_indicator_tokens,
        )?;

        let scrolled_back = !scene.tile_follow_tail_at_tail(tile.id);
        let pill = tze_hud_input::compute_jump_to_latest_pill(
            tile.bounds.width,
            viewport_px,
            scrolled_back,
            jump_to_latest_tokens,
        )?;

        // The count the pill MAY carry — `None`/`0` leaves the plain pill (no badge).
        let count = scene.tile_unread_count(tile.id);
        let label = tze_hud_input::jump_to_latest_badge_label(Some(count))?;

        // `text_*` are STRAIGHT sRGB floats → sRGB u8 by a plain scale (no curve;
        // the resolver already normalized any token-parsed linear value back to
        // sRGB). Matches the badge-text convention documented on JumpToLatestTokens.
        let to_srgb_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        let color = [
            to_srgb_u8(jump_to_latest_tokens.text_r),
            to_srgb_u8(jump_to_latest_tokens.text_g),
            to_srgb_u8(jump_to_latest_tokens.text_b),
            to_srgb_u8(jump_to_latest_tokens.text_a),
        ];

        let font_size_px = jump_to_latest_tokens.text_size_px.max(1.0);
        let line_height_multiplier =
            crate::markdown::MarkdownTokens::default().line_height_multiplier;
        let line_height = (font_size_px * line_height_multiplier).max(1.0);

        // Pill origin in absolute pixels; vertically center one text line inside it.
        let pill_x = tile.bounds.x + pill.x_px;
        let pill_y = tile.bounds.y + pill.y_px;
        let text_y = pill_y + ((pill.height_px - line_height) / 2.0).max(0.0);
        let bounds_width = pill.width_px.max(1.0);
        let bounds_height = pill.height_px.max(1.0);

        Some(crate::text::TextItem {
            text: Arc::from(label.as_str()),
            pixel_x: pill_x,
            pixel_y: text_y,
            bounds_width,
            bounds_height,
            clip_pixel_x: pill_x,
            clip_pixel_y: pill_y,
            clip_bounds_width: bounds_width,
            clip_bounds_height: bounds_height,
            font_size_px,
            font_family: tze_hud_scene::types::FontFamily::SystemSansSerif,
            font_weight: 400,
            color,
            // Horizontally center the count within the pill.
            alignment: tze_hud_scene::types::TextAlign::Center,
            overflow: tze_hud_scene::types::TextOverflow::Clip,
            outline_color: None,
            outline_width: None,
            opacity: 1.0,
            color_runs: Box::new([]),
            styled_runs: Box::new([]),
            line_height_multiplier,
            viewport: crate::overflow::TruncationViewport::HeadAnchored,
        })
    }

    /// Depth-first search for the first composer-input `HitRegionNode` reachable
    /// from the tile root (a `HitRegionNode` with `accepts_composer_input`).
    ///
    /// Used to place the viewer reply echo relative to the composer even when no
    /// draft is active (post-submit `local_composer` is `None`), so history lines
    /// can render above the composer strip.
    fn find_composer_node_in_tile(tile: &Tile, scene: &SceneGraph) -> Option<SceneId> {
        fn dfs(node_id: SceneId, scene: &SceneGraph) -> Option<SceneId> {
            let node = scene.nodes.get(&node_id)?;
            if let NodeData::HitRegion(hr) = &node.data {
                if hr.accepts_composer_input {
                    return Some(node_id);
                }
            }
            for child in &node.children {
                if let Some(found) = dfs(*child, scene) {
                    return Some(found);
                }
            }
            None
        }
        dfs(tile.root_node?, scene)
    }

    /// The zone width available for viewer-echo text inside `region` (the
    /// composer region), i.e. the region interior minus the horizontal text
    /// margins.  Shared by the prime (wrap measurement) and collect (render) so
    /// the measured line count and the rendered wrap agree.
    fn viewer_echo_zone_width(region: Rect, content_inset_px: f32) -> f32 {
        (region.width - content_inset_px * 2.0).max(1.0)
    }

    /// The retained viewer-echo entries for `tile` joined oldest-first with `\n`,
    /// so the rendered block reads top (oldest) to bottom (newest) and embedded
    /// newlines from Ctrl+Enter drafts (#992) break as their own lines (hud-pncm3).
    ///
    /// Joins the DISPLAY text (each entry's timestamp-prefixed form, hud-7ic89)
    /// rather than the raw submitted text, so this stays byte-for-byte the text
    /// the block actually paints.
    fn viewer_echo_joined_text(&self, tile_id: SceneId) -> Option<String> {
        Some(self.viewer_echo_entry_texts(tile_id)?.join("\n"))
    }

    /// The retained viewer-echo entry DISPLAY texts for `tile`, oldest-first,
    /// each as its own owned `String` so the layout prime can measure per-entry
    /// wrapped line counts for the turn dividers (hud-hsc1t).
    ///
    /// Each entry is prefixed with its compact "HH:MM " clock (hud-7ic89, see
    /// [`viewer_echo_timestamp_prefix`]) when `submitted_at_wall_us` is
    /// non-zero, so wrap measurement here and the render collect below stay
    /// consistent with the text actually painted — a bare `.text` accessor
    /// would silently drop the timestamp from both.
    fn viewer_echo_entry_texts(&self, tile_id: SceneId) -> Option<Vec<String>> {
        let entries = self.viewer_echoes.entries_for(tile_id)?;
        Some(entries.iter().map(viewer_echo_display_text).collect())
    }

    /// Byte ranges within the JOINED viewer-echo text (as produced by
    /// [`Self::viewer_echo_joined_text`]) covering each entry's timestamp
    /// prefix, if any (hud-7ic89). Consumed by
    /// [`Self::collect_viewer_echo_text_items`] to style the clock distinctly
    /// (muted color, smaller scale) from the message body within the same
    /// wrapped `TextItem`, without a second draw pass or extra layout math.
    ///
    /// Mirrors the join order/separator of `viewer_echo_joined_text` exactly
    /// (oldest-first, `\n`-joined) so the returned offsets land on the prefix
    /// bytes of the text that function produces.
    fn viewer_echo_timestamp_byte_ranges(&self, tile_id: SceneId) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        let Some(entries) = self.viewer_echoes.entries_for(tile_id) else {
            return ranges;
        };
        let mut cursor = 0usize;
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                cursor += 1; // the '\n' joiner
            }
            match viewer_echo_timestamp_prefix(entry.submitted_at_wall_us) {
                Some(prefix) => {
                    ranges.push((cursor, cursor + prefix.len()));
                    cursor += prefix.len() + entry.text.len();
                }
                None => cursor += entry.text.len(),
            }
        }
        ranges
    }

    /// Measure the wrapped visual-line count of each tile's viewer-echo history
    /// once per frame (hud-pncm3), storing the total in `viewer_echo_line_counts`
    /// and the per-entry counts in `viewer_echo_entry_line_counts` (hud-hsc1t).
    ///
    /// Word-wrap measurement needs the `&mut` text rasterizer (font metrics),
    /// which the `&self` `collect_viewer_echo_text_items` cannot reach — so it is
    /// primed here, mirroring `prime_composer_scroll_offset`. Must run once per
    /// frame BEFORE the text pass. Off the transcript hot path: it runs only when
    /// echoes exist and over a bounded (`MAX_VIEWER_ECHO_ENTRIES`) history.
    ///
    /// Each entry is measured independently because the entries join with a hard
    /// `\n` — so the summed per-entry count equals the joined-block count, and the
    /// cumulative boundaries the divider pass reads stay consistent with the total
    /// block height the text pass reads.
    pub(crate) fn prime_viewer_echo_layout(&mut self, scene: &SceneGraph) {
        self.viewer_echo_line_counts.clear();
        self.viewer_echo_entry_line_counts.clear();
        if self.viewer_echoes.is_empty() || self.text_rasterizer.is_none() {
            return;
        }
        let lhm = crate::markdown::MarkdownTokens::default().line_height_multiplier;
        let base_echo_font = resolve_viewer_echo_tokens(&self.token_map).font_size_px;
        // Token-driven composer content inset (hud-ar10c): the echo wrap-measure
        // zone width must match the render path's zone width in
        // `collect_viewer_echo_text_items`, which insets by the same value.
        let content_inset = resolve_composer_overlay_tokens(&self.token_map).content_inset_px;

        // Gather (tile, zone_width, per-entry texts) under &self first, then
        // measure under &mut self.text_rasterizer — the two borrows do not overlap.
        let mut jobs: Vec<(SceneId, f32, f32, Vec<String>)> = Vec::new();
        for tile in self.policy_visible_tiles(scene) {
            let Some(composer_node) = Self::find_composer_node_in_tile(tile, scene) else {
                continue;
            };
            let Some(region) = Self::composer_region_bounds(tile, scene, composer_node) else {
                continue;
            };
            let Some(entries) = self.viewer_echo_entry_texts(tile.id) else {
                continue;
            };
            jobs.push((
                tile.id,
                Self::viewer_echo_zone_width(region, content_inset),
                self.scaled_portal_font(base_echo_font, tile.id, scene),
                entries,
            ));
        }

        let mut results: Vec<(SceneId, Vec<usize>)> = Vec::with_capacity(jobs.len());
        if let Some(tr) = self.text_rasterizer.as_mut() {
            for (tile_id, zone_width, echo_font, entries) in &jobs {
                // Break-anywhere per-entry line count (WRAPPED_TEXT_WRAP, hud-n0x4u):
                // an over-long single-word reply in one entry is counted as the
                // multiple in-box lines it paints as, not one clipped line — so the
                // cumulative boundaries the divider pass derives stay aligned with
                // the painted wrap (hud-hsc1t).
                let per_entry: Vec<usize> = entries
                    .iter()
                    .map(|entry| {
                        tr.measure_wrapped_line_count(entry, *zone_width, *echo_font, lhm)
                            .max(1)
                    })
                    .collect();
                results.push((*tile_id, per_entry));
            }
        }
        for (tile_id, per_entry) in results {
            let total: usize = per_entry.iter().sum::<usize>().max(1);
            self.viewer_echo_line_counts.insert(tile_id, total);
            self.viewer_echo_entry_line_counts
                .insert(tile_id, per_entry);
        }
    }

    /// Resolve the per-frame vertical-flow layout (hud-pd9bp): for every
    /// `NodeLayout::VerticalFlow` node in the scene, stack its children and store
    /// each child's resolved tile-local `y` in [`Self::tile_flow_offsets`], which
    /// the `&self` geometry sites (`render_node`, the text-collect walks, and the
    /// ellipsis-prime twin) read to substitute the resolved `y` for the child's
    /// own `bounds.y`.
    ///
    /// Mirrors [`Self::prime_viewer_echo_layout`]: measuring wrapped child heights
    /// needs a `&mut FontSystem` the `&self` geometry sites cannot reach, so it
    /// runs once per frame BEFORE the geometry and text passes.
    ///
    /// Behavior-preserving early-out: a scene with no `VerticalFlow` node clears
    /// the map and returns WITHOUT constructing a `FontSystem`, so the all-Absolute
    /// hot path (every production scene today) pays only a cheap scan and every
    /// geometry site falls back to `bounds.y` — byte-identical to before this
    /// capability existed, including the ellipsis truncation-cache geometry.
    pub(crate) fn prime_vertical_flow_layout(&mut self, scene: &SceneGraph) {
        self.tile_flow_offsets.clear();
        if !scene
            .nodes
            .values()
            .any(|node| node.layout == NodeLayout::VerticalFlow)
        {
            return;
        }
        let gap = resolve_section_gap_px(&self.token_map);
        // Measure flow child heights against the RENDER rasterizer's OWN
        // `FontSystem` (hud-9gopx) — the same one that shapes the glyphs — so any
        // agent-uploaded font loaded via `load_font_data` is reflected in the
        // stacked heights, keeping "measured == painted" true for uploaded fonts
        // too. Clone the token set first so the `&self.markdown_tokens` borrow ends
        // before the rasterizer is taken mutably (mirrors
        // `prime_viewer_echo_layout`'s two-phase borrow discipline); the resolver
        // gets a real `&MarkdownTokens` (hud-ysyis) so markdown / attributed turns
        // measure on the same basis the render constructors paint them.
        //
        // If the rasterizer is not yet initialized, fall back to a fresh
        // `bundled_font_system()` (hud-tfm3p review fix) — matching this
        // function's pre-hud-9gopx behavior for that state exactly. Flow
        // resolution positions EVERY `VerticalFlow` child, not just text
        // (`SolidColor` / `StaticImage` / `HitRegion` children read
        // `tile_flow_offsets` too, per hud-pd9bp), so gating the whole resolve on
        // rasterizer presence would silently drop stacking for non-text geometry
        // that never needed a `FontSystem` in the first place — a real regression
        // Codex caught on this PR versus the pre-hud-9gopx baseline, which ran
        // unconditionally. There can be no agent-uploaded fonts to reflect when
        // there is no rasterizer yet anyway, so the bundled fallback is exactly
        // as accurate as the code it replaces.
        let markdown_tokens = self.markdown_tokens.clone();
        // hud-5wx09: measure markdown flow children against the commit-time cached
        // parse the render path already produced, NOT a fresh per-frame parse
        // (text-stream-portals MUST). Pin the render `MarkdownCache` snapshot (a
        // cheap atomic `Arc` load) and build a `SceneId -> &ParsedMarkdown` lookup
        // BEFORE the `&mut FontSystem` borrow below.
        let cache = self.markdown_cache();

        // hud-wrlv1: content over the async-parse threshold parses on the
        // background worker, so the AUTHORITATIVE snapshot misses for several
        // frames after a commit. Without self-heal, a large unchanged flow child
        // would take the resolver's parse-on-miss fallback EVERY frame of that
        // window. Mirror the render path (`collect_text_items_from_node`): before
        // building the lookup, parse any missing flow markdown child ONCE with its
        // scoped tokens and park it in `markdown_fallback_cache`, then consult that
        // side cache alongside the authoritative one below. Large content then
        // re-parses at most once per content version, not per frame. The
        // `borrow_mut()` (populate) is scoped to a block so it is released before
        // the `borrow()` (consult) — the render path's own `BorrowMutError`
        // discipline (hud-u4lq2).
        let flow_child_ids: std::collections::HashSet<SceneId> = scene
            .nodes
            .values()
            .filter(|node| node.layout == NodeLayout::VerticalFlow)
            .flat_map(|node| node.children.iter().copied())
            .collect();
        let portal_node_ids = super::portal_markdown_node_ids(scene);
        {
            let mut fallback = self.markdown_fallback_cache.borrow_mut();
            heal_flow_markdown_fallback(
                &cache,
                &mut fallback,
                &self.node_key_cache,
                &scene.nodes,
                &flow_child_ids,
                &portal_node_ids,
                &self.markdown_tokens,
                &self.markdown_tokens_generic,
            );
        }
        // Consult authoritative FIRST, then the self-heal side cache. Reusing
        // `node_key_cache` keys each node under the SAME portal/generic token scope
        // `prime_markdown_cache` used, so a recomputed-key scope mismatch can't turn
        // a hit into a miss. The `fallback` `Ref` and the `cache` `Arc` both pin the
        // `&ParsedMarkdown` references for the whole resolve; both are disjoint from
        // the `&mut self.text_rasterizer` borrow taken below.
        let fallback = self.markdown_fallback_cache.borrow();
        let parsed_by_id: std::collections::HashMap<SceneId, &crate::markdown::ParsedMarkdown> =
            self.node_key_cache
                .iter()
                .filter_map(|(id, key)| {
                    cache
                        .get_by_key(key)
                        .or_else(|| fallback.get(key))
                        .map(|parsed| (*id, parsed))
                })
                .collect();
        let mut bundled_fallback = None;
        let font_system: &mut FontSystem = match self.text_rasterizer.as_mut() {
            Some(rasterizer) => rasterizer.font_system_mut(),
            None => bundled_fallback.get_or_insert_with(crate::fonts::bundled_font_system),
        };
        self.tile_flow_offsets = crate::vertical_flow::resolve_tile_flow_offsets(
            font_system,
            &scene.nodes,
            gap,
            &markdown_tokens,
            &parsed_by_id,
        );
    }

    /// Collect kind-distinct `TextItem`s for the runtime-authored viewer reply
    /// echo (hud-nx7yq.3), as a single wrapped block bottom-aligned to the top of
    /// the LIVE composer input box — newest reply nearest the composer. Anchoring
    /// to the current `visible_lines`-aware box (not the fixed single-line strip)
    /// keeps the echo history riding above a growing multi-line draft rather than
    /// colliding with it (hud-xgtuf). The block word-wraps to the zone width and
    /// honors embedded newlines (hud-pncm3); the oldest lines clip first when the
    /// history would exceed the band above the composer box.
    ///
    /// Returns an empty vec when the tile has no retained viewer echoes, no
    /// composer node to anchor to, or the text rasterizer is unavailable. Lines
    /// that would land above the composer region are dropped (bounded window).
    /// Each line fades with the tile's effective opacity so the echo redacts /
    /// hides in lockstep with the surface it belongs to.
    pub(super) fn collect_viewer_echo_text_items(
        &self,
        tile: &Tile,
        scene: &SceneGraph,
        sw: f32,
        sh: f32,
        tokens: &super::token_colors::ViewerEchoTokens,
    ) -> Vec<crate::text::TextItem> {
        let _ = (sw, sh); // retained for API symmetry with sibling collect helpers
        let mut items = Vec::new();
        if self.text_rasterizer.is_none() {
            return items;
        }
        let Some(joined) = self.viewer_echo_joined_text(tile.id) else {
            return items;
        };
        let Some(composer_node) = Self::find_composer_node_in_tile(tile, scene) else {
            return items;
        };
        let Some(region) = Self::composer_region_bounds(tile, scene, composer_node) else {
            return items;
        };

        let line_height_multiplier =
            crate::markdown::MarkdownTokens::default().line_height_multiplier;
        // Anchor the history block to the TOP of the LIVE composer input box —
        // the same `visible_lines`-aware box the draft render uses (hud-xgtuf).
        // Post-submit the box rests at one line (composer_layout resets to
        // default each frame), but while the viewer types a multi-line draft the
        // box grows upward; anchoring here keeps the echo riding above the live
        // draft instead of the fixed single-line position it would otherwise grow
        // into. The box is measured with the COMPOSER font (matching the draft box
        // exactly), while the echo lines use their own font below.
        let composer_tokens = resolve_composer_overlay_tokens(&self.token_map);
        let composer_font_size_px =
            self.scaled_portal_font(composer_tokens.font_size_px, tile.id, scene);
        let echo_font_size_px = self.scaled_portal_font(tokens.font_size_px, tile.id, scene);
        let draft_box = Self::composer_input_box(
            region,
            composer_font_size_px,
            line_height_multiplier,
            self.composer_layout.visible_lines,
            composer_tokens.anchor,
            composer_tokens.content_inset_px,
        );
        let line_h = (echo_font_size_px * line_height_multiplier).max(1.0);
        let margin = composer_tokens.content_inset_px;
        let opacity = self.tile_effective_opacity(tile, scene);
        let zone_width = Self::viewer_echo_zone_width(region, composer_tokens.content_inset_px);

        // Total WRAPPED visual-line count of the joined history: primed
        // (wrap-accurate) or, absent a prime this frame, the logical `\n`-split
        // count so embedded newlines still lay out one-line-per-break (hud-pncm3).
        let total_lines = self
            .viewer_echo_line_counts
            .get(&tile.id)
            .copied()
            .unwrap_or_else(|| joined.split('\n').count())
            .max(1);
        let block_height = (total_lines as f32 * line_h).max(line_h);

        // Slide the block within the fixed band by the input tile's displayed
        // vertical scroll offset so the viewer can scroll UP through older history
        // (hud-acfvp). For a bottom-anchored composer the tail is bottom-aligned
        // (NEWEST reply just above the box, oldest clipping off `band_top`); a
        // top-anchored composer instead flows submissions DOWNWARD beneath the box.
        // A tile with no scroll config pins to the resting position, so the
        // bottom-anchored newest-fit window is byte-identical to the pre-scroll
        // behavior. The scissor below still clips the band, so lines pushed past
        // its edges drop out (bounded window). The band collapses (returns) for a
        // degenerate composer region — never silently painting off-pane.
        let scroll_offset_y = scene
            .tile_scroll_config(tile.id)
            .map(|_| self.display_tile_scroll_offset(scene, tile.id).1);
        let Some((band_top, band_bottom, block_top)) = input_history_band_layout(
            composer_tokens.anchor,
            region,
            draft_box,
            block_height,
            scroll_offset_y,
        ) else {
            return items;
        };
        let band_height = band_bottom - band_top;

        // Style each entry's "HH:MM " prefix (hud-7ic89) as a muted, smaller run
        // within the single joined `TextItem` — same mechanism the markdown pass
        // uses for heading/inline-code spans (`StyledRunItem`), so the clock reads
        // as ambient metadata beside the message body with no extra draw pass.
        let styled_runs: Box<[crate::text::StyledRunItem]> = self
            .viewer_echo_timestamp_byte_ranges(tile.id)
            .into_iter()
            .map(|(start_byte, end_byte)| crate::text::StyledRunItem {
                start_byte,
                end_byte,
                weight: None,
                italic: false,
                monospace: false,
                color: Some(tokens.timestamp_color),
                background_color: None,
                size_scale: Some(tokens.timestamp_font_scale),
                fill_line_width: false,
            })
            .collect();

        items.push(crate::text::TextItem {
            text: Arc::from(joined.as_str()),
            pixel_x: region.x + margin,
            pixel_y: block_top,
            // Wrap to the zone width (WRAPPED_TEXT_WRAP in the render path) so
            // long replies wrap instead of overflowing — including a single
            // over-long word, broken at the glyph level (hud-n0x4u) — and
            // embedded `\n`s break.
            bounds_width: zone_width,
            bounds_height: block_height,
            // Scissor to the band between the region top and the live box top:
            // oldest lines clip first; the block never intrudes into the box.
            clip_pixel_x: region.x + margin,
            clip_pixel_y: band_top,
            clip_bounds_width: zone_width,
            clip_bounds_height: band_height,
            font_size_px: echo_font_size_px,
            font_family: tze_hud_scene::types::FontFamily::SystemSansSerif,
            font_weight: 400,
            color: tokens.color,
            alignment: tze_hud_scene::types::TextAlign::Start,
            overflow: tze_hud_scene::types::TextOverflow::Clip,
            outline_color: None,
            outline_width: None,
            opacity,
            color_runs: Box::new([]),
            styled_runs,
            line_height_multiplier,
            viewport: crate::overflow::TruncationViewport::HeadAnchored,
        });
        items
    }

    /// Collect token-styled divider rects between adjacent viewer-echo entries
    /// for `tile` (hud-hsc1t), in display space, clipped to the same band the
    /// echo text occupies. Empty when the divider token is absent, the tile has
    /// fewer than two retained entries, or the composer anchor is unavailable.
    ///
    /// Mirrors the block geometry of [`Self::collect_viewer_echo_text_items`] so
    /// the divider lands exactly on the boundary between each pair of rendered
    /// entries, then defers the cumulative-line math to the pure
    /// [`viewer_echo_divider_rects`] helper. Content-free geometry: the rects
    /// carry no text and reveal nothing under redaction.
    pub(super) fn collect_viewer_echo_divider_rects(
        &self,
        tile: &Tile,
        scene: &SceneGraph,
    ) -> Vec<Rect> {
        // Gate on the shared portal.divider.* token, exactly like the markdown
        // transcript separators — no divider token ⇒ no separator geometry.
        let Some(_sep_color) = self.markdown_tokens.separator_color else {
            return Vec::new();
        };
        let thickness = self.markdown_tokens.separator_thickness_px.max(1.0);

        let echo_tokens = resolve_viewer_echo_tokens(&self.token_map);
        let Some(entries) = self.viewer_echo_entry_texts(tile.id) else {
            return Vec::new();
        };
        if entries.len() < 2 {
            return Vec::new();
        }
        let Some(composer_node) = Self::find_composer_node_in_tile(tile, scene) else {
            return Vec::new();
        };
        let Some(region) = Self::composer_region_bounds(tile, scene, composer_node) else {
            return Vec::new();
        };

        let line_height_multiplier =
            crate::markdown::MarkdownTokens::default().line_height_multiplier;
        let composer_tokens = resolve_composer_overlay_tokens(&self.token_map);
        let composer_font_size_px =
            self.scaled_portal_font(composer_tokens.font_size_px, tile.id, scene);
        let echo_font_size_px = self.scaled_portal_font(echo_tokens.font_size_px, tile.id, scene);
        let draft_box = Self::composer_input_box(
            region,
            composer_font_size_px,
            line_height_multiplier,
            self.composer_layout.visible_lines,
            composer_tokens.anchor,
            composer_tokens.content_inset_px,
        );
        let line_h = (echo_font_size_px * line_height_multiplier).max(1.0);
        let margin = composer_tokens.content_inset_px;
        let zone_width = Self::viewer_echo_zone_width(region, composer_tokens.content_inset_px);

        // Per-entry wrapped counts: primed (wrap-accurate) or, absent a prime this
        // frame, the logical `\n`-split count per entry. Either way their sum
        // equals the total the text path uses, so boundaries stay consistent.
        let per_entry: Vec<usize> = self
            .viewer_echo_entry_line_counts
            .get(&tile.id)
            .cloned()
            .unwrap_or_else(|| {
                entries
                    .iter()
                    .map(|e| e.split('\n').count().max(1))
                    .collect()
            });
        let total_lines: usize = per_entry.iter().sum::<usize>().max(1);
        let block_height = (total_lines as f32 * line_h).max(line_h);
        // Slide by the input tile's displayed scroll offset — identical geometry
        // to the text block (hud-acfvp) — so the turn dividers stay locked onto
        // the boundaries between the scrolled entries. Pins to the resting position
        // when the tile has no scroll config, and short-circuits (no dividers) on a
        // degenerate band, exactly mirroring `collect_viewer_echo_text_items`.
        let scroll_offset_y = scene
            .tile_scroll_config(tile.id)
            .map(|_| self.display_tile_scroll_offset(scene, tile.id).1);
        let Some((band_top, band_bottom, block_top)) = input_history_band_layout(
            composer_tokens.anchor,
            region,
            draft_box,
            block_height,
            scroll_offset_y,
        ) else {
            return Vec::new();
        };

        viewer_echo_divider_rects(
            &per_entry,
            region.x + margin,
            block_top,
            zone_width,
            line_h,
            thickness,
            band_top,
            band_bottom,
        )
    }

    /// Render a node and its children within a tile.
    // Lint suppressed deliberately: `render_node` is a recursive tree walk.
    // `too_many_arguments` — the args are the node id plus the four distinct
    // output/scene buffers and two surface dimensions threaded unchanged through
    // every recursion; a context struct would add indirection on a hot path
    // without reducing the real fan-out. `only_used_in_recursion` — `tile`,
    // `sw`, and `sh` are forwarded to child calls, which is the intended shape.
    #[allow(clippy::only_used_in_recursion, clippy::too_many_arguments)]
    pub(super) fn render_node(
        &self,
        node_id: SceneId,
        tile: &Tile,
        scene: &SceneGraph,
        vertices: &mut Vec<RectVertex>,
        textured_cmds: &mut Vec<TexturedDrawCmd>,
        sw: f32,
        sh: f32,
    ) {
        let node = match scene.nodes.get(&node_id) {
            Some(n) => n,
            None => return,
        };
        // hud-pd9bp: a child of a `NodeLayout::VerticalFlow` node takes its
        // runtime-resolved stacked y (from `prime_vertical_flow_layout`) in place
        // of its own `bounds.y`. Absent from the map — every node in an Absolute
        // scene — yields its own `bounds.y`, so the substitutions below are
        // byte-identical there. Only the Y ORIGIN is affected; width/height/x are
        // untouched.
        let effective_y = self
            .tile_flow_offsets
            .get(&node_id)
            .copied()
            .unwrap_or_else(|| node.data.bounds().y);
        let (scroll_x, scroll_y) = if node_uses_display_tile_scroll(scene, tile, node_id) {
            self.display_tile_scroll_offset(scene, tile.id)
        } else {
            (0.0, 0.0)
        };
        // §6.3 fade: the whole tile (backdrop + content backgrounds + text) must
        // fade as one unit. `tile_background_color` and `collect_text_items`
        // already apply this; the content-node backgrounds below MUST match, or a
        // faded/resized tile shows the content background at full opacity while
        // the flat backdrop around it goes see-through (hud-w41ef).
        let tile_opacity = self.tile_effective_opacity(tile, scene);

        match &node.data {
            NodeData::SolidColor(sc) => {
                if !sc.radius.is_some_and(|r| r > 0.0) {
                    Self::append_clipped_rect_vertices(
                        tile,
                        Rect::new(
                            tile.bounds.x + sc.bounds.x - scroll_x,
                            tile.bounds.y + effective_y - scroll_y,
                            sc.bounds.width,
                            sc.bounds.height,
                        ),
                        sw,
                        sh,
                        self.gpu_color(Rgba {
                            a: sc.color.a * tile_opacity,
                            ..sc.color
                        }),
                        vertices,
                    );
                }
            }
            NodeData::TextMarkdown(tm) => {
                if self.text_rasterizer.is_some() {
                    if let Some(bg) = tm.background {
                        Self::append_clipped_rect_vertices(
                            tile,
                            Rect::new(
                                tile.bounds.x + tm.bounds.x - scroll_x,
                                tile.bounds.y + effective_y - scroll_y,
                                tm.bounds.width,
                                tm.bounds.height,
                            ),
                            sw,
                            sh,
                            self.gpu_color(Rgba {
                                a: bg.a * tile_opacity,
                                ..bg
                            }),
                            vertices,
                        );
                    }
                    // Code panel backdrop quads: emitted behind fenced/indented code
                    // blocks when the `color.code.background` design token is set.
                    //
                    // Phase-1 geometry approximation: glyph pixel positions are not
                    // available in `render_node` (the geometry pass runs before the
                    // text rasterizer), so block panels are positioned by counting
                    // lines in the parsed plain text up to the panel byte range.
                    // Inline panel pixel-exact geometry is deferred to Phase 2.
                    if let Some(code_bg) = self.markdown_tokens.code_background {
                        // Load the current snapshot lock-free (hud-33qo7); pinned
                        // by the Arc for the lookup + panel emit below.
                        let markdown_cache = self.markdown_cache();
                        if let Some(key) = self.node_key_cache.get(&node_id) {
                            if let Some(parsed) = markdown_cache.get_by_key(key) {
                                // Match the portal-resize-scaled font the text
                                // glyphs are laid out with (hud-6n9iv): at the
                                // default scale this is byte-identical to
                                // `tm.font_size_px`, but under a whole-portal
                                // resize the panel tracks the scaled line pitch
                                // instead of drifting from the code it backs.
                                let line_height =
                                    self.scaled_portal_font(tm.font_size_px, tile.id, scene) * 1.4;
                                // Code-panel backdrop geometry is token-driven
                                // (portal.spacing.code_panel_*); defaults equal the
                                // historical 4.0/2.0 literals (no visual regression).
                                let tile_spacing = resolve_tile_spacing_tokens(&self.token_map);
                                let panel_margin_x = tile_spacing.code_panel_margin_x_px;
                                let panel_pad_y = tile_spacing.code_panel_pad_y_px;
                                let plain = parsed.plain_text.as_ref();
                                for panel in &parsed.code_panels {
                                    use crate::markdown::CodePanelKind;
                                    if !matches!(panel.kind, CodePanelKind::Block) {
                                        // Inline panels require glyph-level layout;
                                        // deferred to Phase 2.
                                        continue;
                                    }
                                    let clamped_start = panel.start_byte.min(plain.len());
                                    let clamped_end = panel.end_byte.min(plain.len());
                                    if clamped_start >= clamped_end
                                        || !plain.is_char_boundary(clamped_start)
                                        || !plain.is_char_boundary(clamped_end)
                                    {
                                        continue;
                                    }
                                    // Count lines before the panel to get y-offset,
                                    // and lines within the panel to get height.
                                    // Use `.lines().count()` (not newline counting) so that a
                                    // code block without a trailing newline (e.g. "a\nb") is
                                    // correctly measured as 2 lines rather than 1.
                                    let lines_before = plain[..clamped_start]
                                        .chars()
                                        .filter(|&c| c == '\n')
                                        .count();
                                    let lines_in_panel =
                                        plain[clamped_start..clamped_end].lines().count().max(1);
                                    let panel_y_offset =
                                        lines_before as f32 * line_height - panel_pad_y;
                                    let panel_height =
                                        lines_in_panel as f32 * line_height + panel_pad_y * 2.0;
                                    let panel_x =
                                        tile.bounds.x + tm.bounds.x + panel_margin_x - scroll_x;
                                    let panel_y =
                                        tile.bounds.y + effective_y + panel_y_offset - scroll_y;
                                    let panel_w = (tm.bounds.width - panel_margin_x * 2.0).max(0.0);
                                    if panel_w > 0.0 && panel_height > 0.0 {
                                        Self::append_clipped_rect_vertices(
                                            tile,
                                            Rect::new(panel_x, panel_y, panel_w, panel_height),
                                            sw,
                                            sh,
                                            self.gpu_color(code_bg),
                                            vertices,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    // Transcript turn separators: thin token-styled divider quads on
                    // thematic-break (`---`) lines (hud-nx7yq.4). Same line-counted
                    // geometry approximation as code panels. Content-free geometry —
                    // no text, so nothing is revealed under redaction (the transcript
                    // units are zeroed upstream when redacted, removing the breaks).
                    if let Some(sep_color) = self.markdown_tokens.separator_color {
                        let markdown_cache = self.markdown_cache();
                        if let Some(key) = self.node_key_cache.get(&node_id) {
                            if let Some(parsed) = markdown_cache.get_by_key(key) {
                                // Match the portal-resize-scaled font the text
                                // glyphs use so the divider stays glued to the
                                // entries it separates (hud-6n9iv). No-op at the
                                // default scale (scaled_portal_font returns the
                                // base font unchanged); under a whole-portal
                                // resize the divider tracks the scaled line pitch
                                // instead of detaching further down the transcript.
                                let line_height =
                                    self.scaled_portal_font(tm.font_size_px, tile.id, scene) * 1.4;
                                let thickness =
                                    self.markdown_tokens.separator_thickness_px.max(1.0);
                                let divider_color = self.gpu_color(Rgba {
                                    a: sep_color.a * tile_opacity,
                                    ..sep_color
                                });
                                let rects = transcript_separator_rects(
                                    parsed.plain_text.as_ref(),
                                    &parsed.thematic_breaks,
                                    tile.bounds.x + tm.bounds.x - scroll_x,
                                    tile.bounds.y + effective_y - scroll_y,
                                    tm.bounds.width,
                                    line_height,
                                    thickness,
                                );
                                for rect in rects {
                                    Self::append_clipped_rect_vertices(
                                        tile,
                                        rect,
                                        sw,
                                        sh,
                                        divider_color,
                                        vertices,
                                    );
                                }
                            }
                        }
                    }
                } else {
                    // Fallback when glyphon is unavailable: preserve the old
                    // placeholder treatment so text tiles remain visible.
                    let bg = tm.background.unwrap_or(TILE_BG_TEXT_MARKDOWN);
                    Self::append_clipped_rect_vertices(
                        tile,
                        Rect::new(
                            tile.bounds.x + tm.bounds.x - scroll_x,
                            tile.bounds.y + effective_y - scroll_y,
                            tm.bounds.width,
                            tm.bounds.height,
                        ),
                        sw,
                        sh,
                        self.gpu_color(Rgba {
                            a: bg.a * tile_opacity,
                            ..bg
                        }),
                        vertices,
                    );

                    // Fallback placeholder inset is token-driven
                    // (portal.spacing.transcript_fallback_inset_px); default equals
                    // the historical 8.0 literal (no visual regression).
                    let text_margin =
                        resolve_tile_spacing_tokens(&self.token_map).transcript_fallback_inset_px;
                    if tm.bounds.width > text_margin * 2.0 && tm.bounds.height > text_margin * 2.0 {
                        Self::append_clipped_rect_vertices(
                            tile,
                            Rect::new(
                                tile.bounds.x + tm.bounds.x + text_margin - scroll_x,
                                tile.bounds.y + effective_y + text_margin - scroll_y,
                                tm.bounds.width - text_margin * 2.0,
                                (tm.font_size_px * 1.2).min(tm.bounds.height - text_margin * 2.0),
                            ),
                            sw,
                            sh,
                            self.gpu_color(Rgba {
                                a: tm.color.a * tile_opacity,
                                ..tm.color
                            }),
                            vertices,
                        );
                    }
                }
            }
            NodeData::HitRegion(hr) => {
                // HitRegion is an invisible interaction primitive. Default (no
                // state) paints nothing so siblings show through. Hover + press
                // emit sibling-over tints per RFC 0004 §6.5:
                //   hover:   add 0.1 white overlay (lightening)
                //   pressed: multiply by 0.85 (~15% darken; approximated with
                //            a black overlay at 0.15 alpha against the sibling)
                // Per-node overrides come from `local_style.hover_tint` /
                // `pressed_tint` when set.
                let state = scene.hit_region_states.get(&node_id);
                let tint = match state {
                    Some(s) if s.pressed => Some(
                        hr.local_style
                            .pressed_tint
                            .map(|c| c.to_array())
                            .unwrap_or([0.0, 0.0, 0.0, 0.15]),
                    ),
                    Some(s) if s.hovered => Some(
                        hr.local_style
                            .hover_tint
                            .map(|c| c.to_array())
                            .unwrap_or([1.0, 1.0, 1.0, 0.1]),
                    ),
                    _ => None,
                };
                if let Some(color) = tint {
                    Self::append_clipped_rect_vertices(
                        tile,
                        Rect::new(
                            tile.bounds.x + hr.bounds.x - scroll_x,
                            tile.bounds.y + effective_y - scroll_y,
                            hr.bounds.width,
                            hr.bounds.height,
                        ),
                        sw,
                        sh,
                        self.gpu_color_raw(color),
                        vertices,
                    );
                }

                // The keyboard focus ring is NOT drawn here: it moved to the
                // chrome-layer pass (`append_focus_ring_vertices`, hud-k6yvb) so it
                // renders above all agent content (input-model §416) and covers
                // tile-level / composer-less focus owners the per-node scene state
                // cannot express.
            }
            NodeData::StaticImage(img) => {
                // If a GPU texture is cached for this resource, emit a textured
                // draw command with fit-mode UV calculations.
                if let Some(entry) = self.image_texture_cache.get(&img.resource_id) {
                    let (dx, dy, dw, dh, uv_rect) = compute_fit_mode(
                        img.fit_mode,
                        tile.bounds.x + img.bounds.x - scroll_x,
                        tile.bounds.y + effective_y - scroll_y,
                        img.bounds.width,
                        img.bounds.height,
                        entry.width,
                        entry.height,
                    );
                    if let Some((clipped, clipped_uv)) =
                        Self::clipped_textured_rect(tile, Rect::new(dx, dy, dw, dh), uv_rect)
                    {
                        textured_cmds.push(TexturedDrawCmd {
                            resource_id: img.resource_id,
                            x: clipped.x,
                            y: clipped.y,
                            w: clipped.width,
                            h: clipped.height,
                            uv_rect: clipped_uv,
                            // Fade the image with the whole tile (drag + §6.3 portal
                            // transition), matching the tile backdrop/text so a
                            // faded/resized tile stays uniform (hud-b0x0m).
                            tint: [1.0, 1.0, 1.0, tile_opacity],
                        });
                    }
                } else {
                    // Fallback: warm-gray placeholder when bytes not registered.
                    let outer_color = [0.55_f32, 0.50, 0.45, tile_opacity];
                    Self::append_clipped_rect_vertices(
                        tile,
                        Rect::new(
                            tile.bounds.x + img.bounds.x - scroll_x,
                            tile.bounds.y + effective_y - scroll_y,
                            img.bounds.width,
                            img.bounds.height,
                        ),
                        sw,
                        sh,
                        self.gpu_color_raw(outer_color),
                        vertices,
                    );

                    // Placeholder accent margin is token-driven
                    // (portal.spacing.image_margin_px); default equals the historical
                    // 4.0 literal (no visual regression).
                    let margin = resolve_tile_spacing_tokens(&self.token_map).image_margin_px;
                    if img.bounds.width > margin * 2.0 && img.bounds.height > margin * 2.0 {
                        let accent_color = [0.75_f32, 0.70, 0.65, tile_opacity];
                        Self::append_clipped_rect_vertices(
                            tile,
                            Rect::new(
                                tile.bounds.x + img.bounds.x + margin - scroll_x,
                                tile.bounds.y + effective_y + margin - scroll_y,
                                img.bounds.width - margin * 2.0,
                                img.bounds.height - margin * 2.0,
                            ),
                            sw,
                            sh,
                            self.gpu_color_raw(accent_color),
                            vertices,
                        );
                    }
                }
            }
        }

        // Render children
        for child_id in &node.children {
            self.render_node(*child_id, tile, scene, vertices, textured_cmds, sw, sh);
        }
    }
}

/// Self-heal the vertical-flow markdown fallback cache (hud-wrlv1): for each flow
/// child markdown node missing from BOTH the authoritative `MarkdownCache` and the
/// `fallback` side cache, parse it ONCE with its scoped tokens and store it in
/// `fallback` — mirroring the render path's own check-then-store
/// (`collect_text_items_from_node`). Content over the async-parse threshold
/// authoritative-misses for several frames after a commit; without this, the flow
/// measurement would re-parse it every frame of that window. Idempotent: once a
/// node's key is in `fallback` (or the authoritative snapshot lands), a later call
/// finds it and does not re-parse, so large content re-parses at most once per
/// content version.
///
/// The raw / pixel-bearing-color-run fork never parses markdown, so those nodes
/// are skipped. The scope (portal vs generic tokens) is chosen exactly as
/// `prime_markdown_cache` chooses it, and the key is taken from `node_key_cache`
/// (already scope-baked) or computed with the same scoped tokens on a miss, so the
/// stored entry is keyed identically to what the render path will look up.
#[allow(clippy::too_many_arguments)]
fn heal_flow_markdown_fallback(
    authoritative: &crate::markdown::MarkdownCache,
    fallback: &mut std::collections::HashMap<[u8; 32], crate::markdown::ParsedMarkdown>,
    node_key_cache: &std::collections::HashMap<SceneId, [u8; 32]>,
    nodes: &std::collections::HashMap<SceneId, Node>,
    flow_child_ids: &std::collections::HashSet<SceneId>,
    portal_node_ids: &std::collections::HashSet<SceneId>,
    portal_tokens: &crate::markdown::MarkdownTokens,
    generic_tokens: &crate::markdown::MarkdownTokens,
) {
    for id in flow_child_ids {
        let Some(node) = nodes.get(id) else {
            continue;
        };
        let NodeData::TextMarkdown(tm) = &node.data else {
            continue;
        };
        // The raw/color-run branch measures un-parsed content, so it needs no
        // cached parse and must not be filled here.
        if crate::text::markdown_node_has_pixel_runs(tm) {
            continue;
        }
        let tokens = if portal_node_ids.contains(id) {
            portal_tokens
        } else {
            generic_tokens
        };
        let key = node_key_cache
            .get(id)
            .copied()
            .unwrap_or_else(|| crate::markdown::MarkdownCache::compute_key(&tm.content, tokens));
        // Already authoritative, or already self-healed → no parse this frame.
        if authoritative.get_by_key(&key).is_some() || fallback.contains_key(&key) {
            continue;
        }
        fallback.insert(
            key,
            crate::markdown::parse_markdown_subset(&tm.content, tokens),
        );
    }
}

#[cfg(test)]
mod viewer_echo_timestamp_tests {
    use super::*;

    /// `submitted_at_wall_us == 0` is the "no timestamp captured" sentinel
    /// (mirroring `tze_hud_projection::authority`'s own `== 0` check) — no
    /// prefix, so a legacy append without a real submit time renders unchanged
    /// (hud-7ic89 backward-compatibility requirement).
    #[test]
    fn zero_submitted_at_yields_no_prefix() {
        assert_eq!(viewer_echo_timestamp_prefix(0), None);
    }

    /// A known wall-clock microsecond value derives the expected "HH:MM  "
    /// clock string: 12:34:56 UTC on any day is `12*3600 + 34*60 + 56` seconds
    /// into that day.
    #[test]
    fn nonzero_submitted_at_derives_hh_mm_prefix() {
        let secs_of_day = 12 * 3600 + 34 * 60 + 56;
        let us = secs_of_day as u64 * 1_000_000;
        assert_eq!(viewer_echo_timestamp_prefix(us), Some("12:34  ".to_owned()));
    }

    /// The day-seconds derivation wraps at 24h (only time-of-day matters, not
    /// the date), and rounds down to the minute (56 seconds past 12:34 is
    /// still "12:34").
    #[test]
    fn prefix_wraps_at_day_boundary_and_truncates_seconds() {
        let one_day_us = 86_400u64 * 1_000_000;
        let half_past_midnight_us = one_day_us + 30 * 60 * 1_000_000; // +00:30
        assert_eq!(
            viewer_echo_timestamp_prefix(half_past_midnight_us),
            Some("00:30  ".to_owned()),
            "a value past one full day must wrap to the correct time-of-day"
        );
    }

    /// [`viewer_echo_display_text`] prepends the derived prefix to the entry's
    /// submitted text unchanged, and leaves a zero-timestamp entry's text as-is.
    #[test]
    fn display_text_prefixes_message_when_timestamped() {
        let timestamped = ViewerEchoEntry {
            text: "hello there".to_owned(),
            submitted_at_wall_us: (12 * 3600 + 34 * 60) * 1_000_000,
        };
        assert_eq!(viewer_echo_display_text(&timestamped), "12:34  hello there");

        let untimestamped = ViewerEchoEntry {
            text: "hello there".to_owned(),
            submitted_at_wall_us: 0,
        };
        assert_eq!(
            viewer_echo_display_text(&untimestamped),
            "hello there",
            "a zero (no-timestamp) entry must render its text unchanged"
        );
    }
}

#[cfg(test)]
mod resize_grip_tests {
    use super::*;
    use crate::renderer::token_colors::{parse_hex_color, resolve_resize_grip_tokens};
    use std::collections::HashMap;

    /// The grip dots occupy a `size_px` square anchored at the region's
    /// bottom-right corner and form a lower-right triangle (1 dot in the left
    /// column, 2 in the middle, 3 in the right) — the diagonal grip glyph.
    #[test]
    fn dots_anchor_bottom_right_and_form_diagonal() {
        let region = Rect::new(50.0, 40.0, 300.0, 200.0);
        let size = 30.0;
        let dots = Compositor::resize_grip_dot_rects(region, size);

        // Grip square: bottom-right `size`×`size` corner of the region.
        let grip_x = region.x + region.width - size; // 320
        let grip_y = region.y + region.height - size; // 210
        let corner_x = region.x + region.width; // 350
        let corner_y = region.y + region.height; // 240

        for d in &dots {
            assert!(
                d.width > 0.0 && d.height > 0.0,
                "every dot must be non-empty"
            );
            assert!(
                d.x >= grip_x - 1e-3 && d.x + d.width <= corner_x + 1e-3,
                "dot x {} must sit inside the grip square [{grip_x}, {corner_x}]",
                d.x
            );
            assert!(
                d.y >= grip_y - 1e-3 && d.y + d.height <= corner_y + 1e-3,
                "dot y {} must sit inside the grip square [{grip_y}, {corner_y}]",
                d.y
            );
        }

        // Column histogram: cell width is size/3, so a dot's column index is
        // floor((x - grip_x) / cell). The diagonal grip has 1/2/3 dots in
        // columns 0/1/2 respectively.
        let cell = size / 3.0;
        let mut per_col = [0u8; 3];
        for d in &dots {
            let col = (((d.x - grip_x) / cell).floor() as i32).clamp(0, 2) as usize;
            per_col[col] += 1;
        }
        assert_eq!(
            per_col,
            [1, 2, 3],
            "grip must taper as a diagonal: 1 dot left, 2 middle, 3 right"
        );
    }

    /// A non-positive grip size yields all-zero rects (nothing drawn) so the
    /// render site needs no separate guard.
    #[test]
    fn nonpositive_size_yields_empty_dots() {
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);
        for size in [0.0, -5.0] {
            let dots = Compositor::resize_grip_dot_rects(region, size);
            assert!(
                dots.iter().all(|d| d.width == 0.0 && d.height == 0.0),
                "size {size} must produce only zero-area dots"
            );
        }
    }

    /// The resolver falls back to the tze_hud_config resize-grip defaults when
    /// no override token is present: a positive size and a distinct, brighter
    /// hover tint than the resting color.
    #[test]
    fn resolver_defaults_are_distinct_and_positive() {
        let grip = resolve_resize_grip_tokens(&HashMap::new());
        assert!(grip.size_px > 0.0, "default grip size must be positive");
        assert_ne!(
            grip.color, grip.hover_color,
            "hover tint must differ from the resting grip color"
        );
        // The default hover (#8A93A6) is brighter than the resting grip
        // (#5A6373) on every RGB channel.
        for c in 0..3 {
            assert!(
                grip.hover_color[c] > grip.color[c],
                "default hover channel {c} must be brighter than resting"
            );
        }
    }

    /// Override tokens flow through: size, resting color, and hover color are
    /// each taken from the token map, and `mark_color` selects between resting
    /// and hover by the pointer state.
    #[test]
    fn overrides_flow_through_and_hover_swaps_color() {
        let mut map = HashMap::new();
        map.insert(
            "portal.window.resize_grip.size_px".to_owned(),
            "20".to_owned(),
        );
        map.insert(
            "portal.window.resize_grip.color".to_owned(),
            "#101010".to_owned(),
        );
        map.insert(
            "portal.window.resize_grip.hover_color".to_owned(),
            "#f0f0f0".to_owned(),
        );
        let grip = resolve_resize_grip_tokens(&map);

        assert!((grip.size_px - 20.0).abs() < 1e-4, "size override applied");

        let want_rest = parse_hex_color("#101010").unwrap().to_array();
        let want_hover = parse_hex_color("#f0f0f0").unwrap().to_array();
        assert_eq!(grip.mark_color(false), want_rest, "resting color override");
        assert_eq!(grip.mark_color(true), want_hover, "hover color override");
        assert_ne!(
            grip.mark_color(false),
            grip.mark_color(true),
            "hover state must swap the grip color"
        );
    }
}

#[cfg(test)]
mod lifecycle_accent_tests {
    use super::*;

    fn rect(w: f32, h: f32) -> Rect {
        Rect::new(0.0, 0.0, w, h)
    }

    /// The accent bar passes the token color through unchanged and clamps its
    /// width to the tile, folding tile opacity into the alpha (hud-m48i0).
    #[test]
    fn accent_bar_clamps_width_and_folds_opacity() {
        let accent = LifecycleAccent {
            color: Rgba::new(0.2, 0.4, 0.6, 1.0),
            width_px: 4.0,
        };
        let (w, color) =
            Compositor::lifecycle_accent_bar_geom(rect(200.0, 150.0), accent, 0.5).unwrap();
        assert_eq!(w, 4.0, "in-range width is used as-is");
        assert_eq!(
            [color[0], color[1], color[2]],
            [0.2, 0.4, 0.6],
            "color passes through"
        );
        assert!(
            (color[3] - 0.5).abs() < 1e-6,
            "tile opacity folds into alpha"
        );

        // Oversized token width is clamped to the tile width.
        let wide = LifecycleAccent {
            color: Rgba::WHITE,
            width_px: 9999.0,
        };
        let (w, _) = Compositor::lifecycle_accent_bar_geom(rect(12.0, 80.0), wide, 1.0).unwrap();
        assert_eq!(w, 12.0, "width is clamped to the tile edge");
    }

    /// Zero width or zero opacity paints nothing.
    #[test]
    fn accent_bar_none_when_invisible() {
        let zero_w = LifecycleAccent {
            color: Rgba::WHITE,
            width_px: 0.0,
        };
        assert!(Compositor::lifecycle_accent_bar_geom(rect(100.0, 50.0), zero_w, 1.0).is_none());
        let some = LifecycleAccent {
            color: Rgba::WHITE,
            width_px: 4.0,
        };
        assert!(Compositor::lifecycle_accent_bar_geom(rect(100.0, 50.0), some, 0.0).is_none());
    }
}

#[cfg(test)]
mod composer_draft_color_tests {
    use super::composer_draft_base_color;
    use crate::renderer::token_colors::{linear_to_srgb, resolve_composer_overlay_tokens};
    use std::collections::HashMap;

    /// Expected sRGB-u8 rendering of a linear-sRGB token channel, matching the
    /// conversion `composer_draft_base_color` applies (kept independent of the
    /// implementation so the test would catch a broken conversion).
    fn to_srgb_u8(v: f32) -> u8 {
        (linear_to_srgb(v.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8
    }
    fn to_alpha_u8(v: f32) -> u8 {
        (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    }

    /// hud-9gyao: an at-capacity live draft takes the token-driven at-capacity
    /// color as its per-line base color -- a real, bounded coloring of the draft
    /// glyphs, replacing the Phase-1 zero-length color-run sentinel. Below
    /// capacity the draft uses the composer text color. Both resolve from design
    /// tokens, never a literal in the render path.
    #[test]
    fn at_capacity_draft_uses_token_at_capacity_color() {
        let tokens = resolve_composer_overlay_tokens(&HashMap::new());

        let below = composer_draft_base_color(&tokens, false, false);
        let at_cap = composer_draft_base_color(&tokens, true, false);

        assert_eq!(
            below,
            [
                to_srgb_u8(tokens.text_r),
                to_srgb_u8(tokens.text_g),
                to_srgb_u8(tokens.text_b),
                to_alpha_u8(tokens.text_a),
            ],
            "a below-capacity draft must render in the composer text color"
        );
        assert_eq!(
            at_cap,
            [
                to_srgb_u8(tokens.at_capacity_r),
                to_srgb_u8(tokens.at_capacity_g),
                to_srgb_u8(tokens.at_capacity_b),
                to_alpha_u8(tokens.at_capacity_a),
            ],
            "an at-capacity draft must render in portal.composer.at_capacity_color"
        );
        assert_ne!(
            below, at_cap,
            "at capacity must change the draft color so the byte cap is visible"
        );
    }

    /// The empty-draft placeholder hint wins over the at-capacity color: a
    /// placeholder only shows for an empty draft (never at capacity).
    #[test]
    fn placeholder_color_wins_over_at_capacity() {
        let tokens = resolve_composer_overlay_tokens(&HashMap::new());
        assert_eq!(
            composer_draft_base_color(&tokens, true, true),
            tokens.placeholder_color,
            "a placeholder hint must always render in the placeholder color"
        );
    }
}

#[cfg(test)]
mod flow_fallback_cache_tests {
    use super::heal_flow_markdown_fallback;
    use crate::markdown::{MarkdownCache, MarkdownTokens, PARSE_MARKDOWN_SUBSET_CALLS};
    use std::collections::{HashMap, HashSet};
    use tze_hud_scene::types::{
        FontFamily, Node, NodeData, NodeLayout, Rect, Rgba, SceneId, TextAlign, TextMarkdownNode,
        TextOverflow,
    };

    /// Markdown content comfortably over `INLINE_PARSE_BYTE_THRESHOLD` (4096B), so
    /// in production it parses on the background worker and the authoritative
    /// `MarkdownCache` misses for several frames after a commit.
    fn big_markdown_content() -> String {
        let content = format!("# Heading\n{}", "lorem ipsum dolor sit amet ".repeat(200));
        assert!(
            content.len() > 4096,
            "fixture must exceed the async-parse threshold"
        );
        content
    }

    fn markdown_node(id: SceneId, content: &str) -> Node {
        Node {
            id,
            children: vec![],
            layout: NodeLayout::Absolute,
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: content.to_string(),
                bounds: Rect::new(0.0, 0.0, 300.0, 40.0),
                font_size_px: 16.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                background: None,
                alignment: TextAlign::Start,
                overflow: TextOverflow::Ellipsis,
                color_runs: Box::default(),
            }),
        }
    }

    /// hud-wrlv1: a large unchanged flow child that keeps authoritative-missing
    /// (background parse still pending) must re-parse AT MOST ONCE across repeated
    /// per-frame heal calls — the self-heal side cache absorbs every frame after
    /// the first.
    #[test]
    fn heal_parses_large_content_at_most_once_across_repeated_calls() {
        let child_id = SceneId::new();
        let content = big_markdown_content();
        let tokens = MarkdownTokens::default();
        let key = MarkdownCache::compute_key(&content, &tokens);

        let mut node_key_cache = HashMap::new();
        node_key_cache.insert(child_id, key);
        let nodes: HashMap<SceneId, Node> = [(child_id, markdown_node(child_id, &content))]
            .into_iter()
            .collect();
        let flow_child_ids: HashSet<SceneId> = [child_id].into_iter().collect();
        let portal_node_ids: HashSet<SceneId> = [child_id].into_iter().collect();
        // Empty authoritative snapshot → the background parse has not landed yet,
        // so every frame authoritative-misses.
        let authoritative = MarkdownCache::new();
        let mut fallback: HashMap<[u8; 32], _> = HashMap::new();

        // Cold frame: one parse, parked in the fallback cache.
        PARSE_MARKDOWN_SUBSET_CALLS.with(|c| c.set(0));
        heal_flow_markdown_fallback(
            &authoritative,
            &mut fallback,
            &node_key_cache,
            &nodes,
            &flow_child_ids,
            &portal_node_ids,
            &tokens,
            &tokens,
        );
        assert_eq!(
            PARSE_MARKDOWN_SUBSET_CALLS.with(|c| c.get()),
            1,
            "cold heal parses the large child exactly once"
        );
        assert!(
            fallback.contains_key(&key),
            "the parse must be parked in the fallback cache for later frames"
        );

        // Subsequent frames (content unchanged, authoritative still missing): the
        // side cache absorbs them — NOT one parse per frame (the bug).
        for _ in 0..5 {
            heal_flow_markdown_fallback(
                &authoritative,
                &mut fallback,
                &node_key_cache,
                &nodes,
                &flow_child_ids,
                &portal_node_ids,
                &tokens,
                &tokens,
            );
        }
        assert_eq!(
            PARSE_MARKDOWN_SUBSET_CALLS.with(|c| c.get()),
            1,
            "repeated frames must re-parse at most once, not per frame (hud-wrlv1)"
        );
    }

    /// A node already present in the authoritative snapshot is never parsed or
    /// parked by the heal pass (the common steady-state).
    #[test]
    fn heal_skips_nodes_already_in_the_authoritative_cache() {
        let child_id = SceneId::new();
        let content = big_markdown_content();
        let tokens = MarkdownTokens::default();
        let key = MarkdownCache::compute_key(&content, &tokens);

        let mut node_key_cache = HashMap::new();
        node_key_cache.insert(child_id, key);
        let nodes: HashMap<SceneId, Node> = [(child_id, markdown_node(child_id, &content))]
            .into_iter()
            .collect();
        let flow_child_ids: HashSet<SceneId> = [child_id].into_iter().collect();
        let portal_node_ids: HashSet<SceneId> = [child_id].into_iter().collect();
        let mut authoritative = MarkdownCache::new();
        authoritative.prime(&content, &tokens);
        let mut fallback: HashMap<[u8; 32], _> = HashMap::new();

        PARSE_MARKDOWN_SUBSET_CALLS.with(|c| c.set(0));
        heal_flow_markdown_fallback(
            &authoritative,
            &mut fallback,
            &node_key_cache,
            &nodes,
            &flow_child_ids,
            &portal_node_ids,
            &tokens,
            &tokens,
        );
        assert_eq!(
            PARSE_MARKDOWN_SUBSET_CALLS.with(|c| c.get()),
            0,
            "an authoritative-cached node must not be re-parsed by the heal pass"
        );
        assert!(
            fallback.is_empty(),
            "no fallback entry needed for an authoritative hit"
        );
    }
}

#[cfg(test)]
mod degradation_opacity_tests {
    use super::degradation_tile_opacity;
    use tze_hud_scene::DegradationLevel;

    #[test]
    fn significant_degradation_preserves_hidden_tiles_and_opaques_visible_tiles() {
        assert_eq!(
            degradation_tile_opacity(0.0, DegradationLevel::Simplified),
            0.0,
            "durably hidden tiles must remain hidden when transparency is disabled"
        );
        assert_eq!(
            degradation_tile_opacity(0.35, DegradationLevel::Simplified),
            1.0,
            "visible tiles must become opaque when transparency is disabled"
        );
        assert_eq!(
            degradation_tile_opacity(0.35, DegradationLevel::Nominal),
            0.35,
            "nominal rendering must preserve fractional opacity"
        );
    }
}
