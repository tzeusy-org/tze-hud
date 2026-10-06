//! Phase-1 overflow and ellipsis contract (hud-5jbra.3, hud-pvoc1).
//!
//! # Contract
//!
//! [`truncate_for_ellipsis`] implements the normative Phase-1 overflow
//! algorithm for [`TextOverflow::Ellipsis`]:
//!
//! 1. **Word-boundary truncation.** Truncation occurs at the last word
//!    boundary whose shaped width, **plus the shaped ellipsis glyph in the
//!    same style run**, fits the content box width.
//! 2. **Grapheme-cluster fallback.** When no word boundary fits, truncation
//!    falls back to the last grapheme-cluster boundary whose shaped width plus
//!    the ellipsis glyph fits the content box.
//! 3. **Whole-line vertical visibility.** The last visible line is either
//!    fully visible or not rendered — no partially clipped glyph rows.
//! 4. **Stable shape caching.** Truncation is called only when content or
//!    geometry changes; the per-frame render path consumes the already-truncated
//!    string without reshaping.
//!
//! ## Viewport Anchoring (spec tasks 3.2 / 3.3, hud-pvoc1)
//!
//! [`TruncationViewport`] selects between two viewport-anchor modes:
//!
//! - **`HeadAnchored`** (default / existing behavior): when content exceeds
//!   `max_lines`, the *first* `max_lines` are shown.  Used for static display
//!   tiles and scrolled-back transcript viewports.
//!
//! - **`TailAnchored`**: when content exceeds `max_lines`, the *last*
//!   `max_lines` are shown (newest content visible) with an ellipsis prepended
//!   to signal omitted leading lines.  Used for follow-tail streaming transcripts.
//!
//! [`truncate_for_ellipsis`] is unchanged (head-anchored).
//! [`truncate_tail_anchored`] is the new entry point for tail-anchored truncation.
//!
//! ### Spec scenarios implemented (tasks 3.2 / 3.3 / 3.4)
//!
//! **Scenario: append does not disturb a scrolled-back viewport (task 3.3)**
//!
//! When the caller holds a fixed head-anchored viewport (scroll offset frozen),
//! it calls [`truncate_for_ellipsis`] with the text visible in that window.
//! Appending new lines beyond the viewport does not change the truncation output
//! because the first `max_lines` are determined solely by the current text, not
//! future appends.  This is verified by the structural test
//! `append_stability_truncation_prefix_unchanged` (original) and the more
//! descriptive `head_anchored_append_stability_matches_prefix_truncation` (added in hud-pvoc1).
//!
//! **Scenario: follow-tail advances by whole lines (task 3.2)**
//!
//! When the viewport is at the tail (no scroll-back), the caller uses
//! [`truncate_tail_anchored`].  The function always shows the last `max_lines`
//! of the full text, so every append automatically advances the visible window
//! by the exact number of new lines added — never producing a partially clipped
//! line at the bottom.  This is verified by
//! `follow_tail_advances_by_whole_lines`.
//!
//! Source: RFC 0013 §3.4 and §4.2, Phase-1 design §3, spec requirement
//! "Transcript Overflow and Ellipsis Contract".
//!
//! # Complexity
//!
//! `truncate_for_ellipsis` shapes the full text once (O(n)) and then locates
//! the ellipsis cut point with O(log W) additional shape calls via binary
//! search, where W is the number of word (or grapheme) boundaries.  Each
//! individual shape call is O(k) in the prefix length k, giving a total of
//! O(n + k·log W) ≈ O(n log n) — sub-quadratic in text length.  The old
//! linear scan over candidates was O(W·k) — one full reshape per candidate,
//! each costing O(k) in its prefix length — which is O(n²) in the worst case
//! when W and k are both O(n).  Results must be cached by the caller
//! keyed on `(content_hash, bounds_width, bounds_height, font_size_px)`.

use glyphon::{Attrs, Buffer, FontSystem, Metrics, Shaping, Wrap};
use unicode_segmentation::UnicodeSegmentation;

/// Glyph info per layout run: `(start_byte_in_line, end_byte_in_line, glyph_x_right)`.
type GlyphInfo = (usize, usize, f32);

/// Return the logical start byte of a run's glyph slice.
///
/// cosmic-text places glyphs in **visual** order: for LTR runs `glyphs[0]` is
/// the logical start, but for RTL runs `glyphs[0]` is the logical end.
/// Taking `glyphs.first().start` therefore silently discards the leading bytes
/// of every RTL run.  The correct logical start is `min(g.start)` across all
/// glyphs in the run.
#[inline]
fn run_logical_start(glyphs: &[GlyphInfo]) -> usize {
    glyphs.iter().map(|(s, _, _)| *s).min().unwrap_or(0)
}

/// A collected layout run: `(line_i, line_w, glyph_infos)`.
type LayoutRunEntry = (usize, f32, Vec<GlyphInfo>);

/// The ellipsis character appended when text is truncated.
pub(crate) const ELLIPSIS: &str = "…";

/// Truncation result returned by [`truncate_for_ellipsis`].
#[derive(Debug, Clone, PartialEq)]
pub struct TruncationResult {
    /// The display text — either the original (if it fits) or a truncated
    /// version with `ELLIPSIS` appended.
    pub(crate) text: String,
    /// `true` when truncation was applied and the ellipsis glyph was appended.
    pub(crate) was_truncated: bool,
}

/// Viewport anchoring mode for transcript overflow.
///
/// Controls which lines are shown when the full text has more layout runs than
/// `max_lines` allows.  See module-level documentation for the spec contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationViewport {
    /// Show the **first** `max_lines` runs (head of content).
    ///
    /// This is the existing / default behaviour.  Use for static tiles and
    /// viewports that have been scrolled back by the user.
    HeadAnchored,
    /// Show the **last** `max_lines` runs (tail / newest content).
    ///
    /// Use for follow-tail streaming transcripts where new appended lines must
    /// always be visible.  An ellipsis is prepended to signal omitted leading lines.
    TailAnchored,
}

/// Default byte threshold for the viewport-adjacent-window truncation fallback
/// (`max_truncation_input_bytes`, spec.md §324/§331).
///
/// When a single uncached truncation would shape an input larger than this many
/// bytes, [`viewport_adjacent_input`] restricts the shaped input to a
/// viewport-adjacent window of whole source lines instead of the full retained
/// transcript.  The default sits well below the ~8 KiB point where the
/// `overflow_truncate` benchmark first exceeds the Stage-5 Layout Resolve budget
/// (< 1 ms, `about/craft-and-care/engineering-bar.md` §2), keeping a single
/// uncached call within budget for multi-line transcripts.
pub const DEFAULT_MAX_TRUNCATION_INPUT_BYTES: usize = 4096;

/// Bounded whole-line overscan margin kept beyond the visible layout window when
/// the viewport-adjacent-window fallback engages (spec.md §325).
///
/// Must be `>= 1`: the overscan guarantees the windowed input still produces
/// strictly more layout runs than `max_lines`, so the truncation path engages
/// exactly as it did on the full input (otherwise a window of exactly
/// `max_lines` single-layout-line sources would not trigger truncation).
pub const TRUNCATION_OVERSCAN_LINES: usize = 8;

/// Restrict `text` to a viewport-adjacent window of whole source lines when it
/// exceeds `max_input_bytes`, so a single uncached truncation shapes a bounded
/// input instead of the full retained transcript (spec.md §324/§331, the
/// **viewport-adjacent-window fallback**).
///
/// This runs **before** shaping and does no shaping itself: it splits `text` on
/// `'\n'` (the same paragraph/hard-wrap unit cosmic-text uses) and slices on
/// whole-line boundaries.  Because cosmic-text wraps each source line (a
/// `BufferLine`) independently, the layout runs of a contiguous slice of source
/// lines are byte-for-byte identical to those same lines within the full text —
/// so the visible window and ellipsis placement of the truncation result are
/// unchanged for the lines actually within the window.
///
/// # Window selection
///
/// `max_lines = floor(bounds_height / line_height)` whole lines are visible.
/// Each source line yields **at least one** layout line, so `max_lines` source
/// lines always cover the visible layout window; an overscan margin of
/// `overscan_lines` (clamped to `>= 1`) whole lines is added so the window still
/// has strictly more than `max_lines` layout runs (truncation still triggers).
///
/// - [`TruncationViewport::HeadAnchored`]: the visible window is the **first**
///   `max_lines` layout lines, so the window keeps the **first**
///   `max_lines + overscan` source lines.
/// - [`TruncationViewport::TailAnchored`]: the visible window is the **last**
///   `max_lines` layout lines, so the window keeps the **last**
///   `max_lines + overscan` source lines.
///
/// # Returns
///
/// A sub-slice of `text` (zero-copy).  Returns `text` unchanged when it is at or
/// below `max_input_bytes`, when geometry is degenerate (`max_lines == 0`), or
/// when the transcript has too few source lines to window (`source_lines <=
/// max_lines + overscan`) — including the degenerate single-giant-line case,
/// which cannot be windowed without violating the whole-line guarantee.
pub fn viewport_adjacent_input(
    text: &str,
    bounds_height: f32,
    line_height: f32,
    viewport: TruncationViewport,
    max_input_bytes: usize,
    overscan_lines: usize,
) -> &str {
    // Only engage above the configured byte bound: small windows shape within
    // the Stage-5 budget and must measure the full committed input (spec §331).
    if text.len() <= max_input_bytes {
        return text;
    }

    // Degenerate geometry is handled by shape_runs' early-return; don't window.
    let max_lines = max_whole_lines(bounds_height, line_height);
    if max_lines == 0 {
        return text;
    }

    let overscan = overscan_lines.max(1);
    // Source lines covering the visible layout window (>= max_lines, since each
    // source line is >= 1 layout line) plus the bounded whole-line overscan.
    let keep = max_lines.saturating_add(overscan);

    // Byte positions of every '\n' (the source-line / paragraph separators that
    // text_prefix_up_to_run and truncate_tail_anchored already count).
    // source_line_count == nl.len() + 1.
    let nl: Vec<usize> = text
        .bytes()
        .enumerate()
        .filter_map(|(i, b)| (b == b'\n').then_some(i))
        .collect();

    // Too few source lines to window: keeping them all is the full input, so the
    // visible window would be unchanged anyway. (Also covers a single giant line
    // with no '\n', which cannot be split on a whole-line boundary.)
    if nl.len() < keep {
        return text;
    }

    match viewport {
        TruncationViewport::HeadAnchored => {
            // Keep the first `keep` whole source lines (lines 0..keep). The end
            // is the '\n' that terminates line `keep - 1`, excluded so the
            // window holds exactly `keep` whole lines with no trailing empty
            // line. nl[keep - 1] is valid because nl.len() >= keep.
            &text[..nl[keep - 1]]
        }
        TruncationViewport::TailAnchored => {
            // Keep the last `keep` whole source lines. The first kept line starts
            // immediately after the '\n' that terminates the line before it:
            // line index (source_line_count - keep) starts at nl[nl.len() - keep] + 1.
            let start = nl[nl.len() - keep] + 1;
            &text[start..]
        }
    }
}

/// Truncate `text` so that the first full line fits within `bounds_width`
/// physical pixels, then restrict to the number of whole lines that fit within
/// `bounds_height`.
///
/// This is the sole implementation of the Phase-1 ellipsis contract.  It must
/// be called **outside the per-frame pipeline** (at content-commit time or on
/// geometry change).
///
/// # Parameters
///
/// - `text` — the plain-text string to truncate (already stripped of markup
///   by the markdown parse path or `strip_markdown_v1`).
/// - `base_attrs` — the base [`Attrs`] used when shaping the text.  Must
///   match the attrs passed to the render buffer for the same item.
/// - `bounds_width` — content box width in physical pixels.
/// - `bounds_height` — content box height in physical pixels.
/// - `font_size_px` — font size in physical pixels.
/// - `line_height` — line height in physical pixels (typically `font_size_px * 1.4`).
/// - `font_system` — shared `FontSystem` for shaping.
///
/// # Return value
///
/// A [`TruncationResult`] containing the display text and a flag indicating
/// whether ellipsis was applied.
///
/// # Guarantee
///
/// The returned text, when shaped into a `Buffer` with the same attrs and
/// geometry, will render without partially clipped glyph rows.
pub fn truncate_for_ellipsis<'a>(
    text: &str,
    base_attrs: Attrs<'a>,
    bounds_width: f32,
    bounds_height: f32,
    font_size_px: f32,
    line_height: f32,
    font_system: &mut FontSystem,
) -> TruncationResult {
    // ── Shared setup: guard geometry, compute max_lines/ellipsis_w, shape ────
    let shaped = match shape_runs(
        text,
        base_attrs,
        bounds_width,
        bounds_height,
        font_size_px,
        line_height,
        font_system,
    ) {
        ShapeOutcome::EarlyReturn(result) => return result,
        ShapeOutcome::Proceed(shaped) => shaped,
    };
    let ShapedRuns {
        full_buf,
        runs,
        max_lines,
        ellipsis_w,
    } = &*shaped;
    // `full_buf` and `runs` stay borrowed from `shaped`; copy the scalars out.
    let (max_lines, ellipsis_w) = (*max_lines, *ellipsis_w);

    // ── Step 5: whole-line vertical visibility ────────────────────────────────
    // If the text produces more runs than max_lines, we must truncate.
    let total_runs = runs.len();
    if total_runs <= max_lines {
        // All lines fit vertically; only horizontal overflow can force a cut.
        return try_horizontal_truncate(
            &shaped,
            text,
            base_attrs,
            bounds_width,
            font_size_px,
            line_height,
            font_system,
        )
        .unwrap_or(TruncationResult {
            text: text.to_owned(),
            was_truncated: false,
        });
    }

    // More runs than fit: take only the first `max_lines` runs.
    // Before truncating the last visible run, check whether any visible run
    // overflows horizontally.  With Shaping::Advanced, an unbreakable word
    // (e.g. "Mk0") can measure wider than bounds_width even in the visible
    // prefix.  In that case, treat it the same as the horizontal-overflow path:
    // truncate at the first overflowing run and drop everything after it.
    let visible_runs = &runs[..max_lines];
    let first_overflow_in_visible = visible_runs.iter().position(|run| run.1 > bounds_width);

    let (truncation_run_idx, _truncation_line_i, truncation_line_text) =
        if let Some(overflow_idx) = first_overflow_in_visible {
            // A run in the visible window overflows — truncate there.
            let overflow_run = &runs[overflow_idx];
            let line_i = overflow_run.0;
            let line_text = if let Some(line) = full_buf.lines.get(line_i) {
                line.text().to_owned()
            } else {
                return TruncationResult {
                    text: text.to_owned(),
                    was_truncated: true,
                };
            };
            (overflow_idx, line_i, line_text)
        } else {
            // No horizontal overflow in the visible window — truncate the last
            // visible run with an ellipsis (the standard vertical-truncation path).
            let last_visible_run = &runs[max_lines - 1];
            let line_i = last_visible_run.0;
            let line_text = if let Some(line) = full_buf.lines.get(line_i) {
                line.text().to_owned()
            } else {
                return TruncationResult {
                    text: text.to_owned(),
                    was_truncated: true,
                };
            };
            (max_lines - 1, line_i, line_text)
        };

    let prefix = text_prefix_up_to_run(runs, truncation_run_idx, text, full_buf);
    // Use min(g.start) so RTL runs resolve to the logical start, not the
    // visual-first (logical-last) glyph that glyphs[0] gives for RTL text.
    let truncation_run = &runs[truncation_run_idx];
    let run_start = run_logical_start(&truncation_run.2);
    let run_slice = run_start_slice(&truncation_line_text, run_start);
    let truncated_last = truncate_line_to_ellipsis(
        run_slice,
        base_attrs,
        bounds_width,
        font_size_px,
        line_height,
        ellipsis_w,
        font_system,
    );
    let result = format!("{prefix}{truncated_last}");
    TruncationResult {
        text: result,
        was_truncated: true,
    }
}

/// Tail-anchored truncation: show the **last** `max_lines` whole lines of `text`.
///
/// This is the entry point for the **follow-tail** scenario (spec task 3.2):
/// when the viewport is at the tail of a streaming transcript, appended content
/// should always be visible.  The function shapes the full text, selects the
/// last `max_lines` layout runs, and prepends an ellipsis to signal that leading
/// lines have been omitted.
///
/// All existing truncation semantics from [`truncate_for_ellipsis`] are
/// preserved for the visible window:
/// - Whole-line vertical visibility: no partially clipped glyph rows.
/// - Word-boundary / grapheme-cluster ellipsis on the first visible line when
///   it was word-wrapped and only part of a paragraph is shown.
/// - RTL/bidi safety: the same `run_logical_start` path is used.
///
/// # Parameters
///
/// Same as [`truncate_for_ellipsis`].  `bounds_height` and `line_height`
/// together determine how many whole lines (`max_lines`) are visible.
///
/// # Return value
///
/// A [`TruncationResult`].  When the full text fits in `max_lines`, returns
/// the original text unchanged (identical to head-anchored behaviour).  When
/// the content exceeds `max_lines` (and `max_lines >= 2`), returns the last
/// `max_lines - 1` content runs preceded by a single `ELLIPSIS` line, so the
/// total visible line count is exactly `max_lines` (not `max_lines + 1`).
/// Sets `was_truncated = true`.
///
/// ## Degenerate `max_lines == 1` (hud-qoq52, option (b))
///
/// With only one line slot, a dedicated `ELLIPSIS` line would leave no room for
/// content — a bare `"…"` that shows nothing, defeating the purpose of a
/// follow-tail surface.  Instead the single line shows the tail of the **newest**
/// layout run with an **inline leading ellipsis** (`"…tail of newest line"`):
/// the leading `"…"` preserves the omission signal while the newest content stays
/// visible.  The line is horizontally truncated to fit `bounds_width` with no
/// clipped glyph, and the result is exactly one logical line.
///
/// # Spec scenarios
///
/// - **Task 3.2 — follow-tail advances by whole lines**: Every call with a
///   longer text (more appended lines) returns a window that ends on the last
///   line of the new content — no partial line is ever shown at the bottom.
/// - **Task 3.3 / 3.4 — append stability**: The HEAD-anchored path
///   ([`truncate_for_ellipsis`]) handles scrolled-back viewports; this function
///   is only called when the viewport is known to be at the tail.
pub(crate) fn truncate_tail_anchored<'a>(
    text: &str,
    base_attrs: Attrs<'a>,
    bounds_width: f32,
    bounds_height: f32,
    font_size_px: f32,
    line_height: f32,
    font_system: &mut FontSystem,
) -> TruncationResult {
    // ── Shared setup: guard geometry, compute max_lines/ellipsis_w, shape ────
    let shaped = match shape_runs(
        text,
        base_attrs,
        bounds_width,
        bounds_height,
        font_size_px,
        line_height,
        font_system,
    ) {
        ShapeOutcome::EarlyReturn(result) => return result,
        ShapeOutcome::Proceed(shaped) => shaped,
    };
    let ShapedRuns {
        full_buf,
        runs,
        max_lines,
        ellipsis_w,
    } = &*shaped;
    // `full_buf` and `runs` stay borrowed from `shaped`; copy the scalars out.
    let (max_lines, ellipsis_w) = (*max_lines, *ellipsis_w);
    let total_runs = runs.len();

    // ── Step 5: if all lines fit, both anchor modes produce the same result ──
    // When content fits entirely, only horizontal overflow can force a cut, and
    // tail-anchored behaves identically to head-anchored in that case.
    if total_runs <= max_lines {
        return try_horizontal_truncate(
            &shaped,
            text,
            base_attrs,
            bounds_width,
            font_size_px,
            line_height,
            font_system,
        )
        .unwrap_or(TruncationResult {
            text: text.to_owned(),
            was_truncated: false,
        });
    }

    // ── Step 6: more runs than fit — show LAST (max_lines - 1) content runs ──
    //
    // The ellipsis is emitted on its own line (ELLIPSIS + "\n"), which consumes
    // one of the max_lines slots.  Therefore only (max_lines - 1) content runs
    // can be shown, or the result would have max_lines + 1 visible lines.
    //
    // The first visible run is runs[total_runs - (max_lines - 1)].
    // The last  visible run is runs[total_runs - 1].
    //
    // ── Degenerate case: max_lines == 1 (DECISION — option (b), hud-qoq52) ───
    //
    // A follow-tail surface exists to show the NEWEST content line.  When only
    // one line slot is available, spending it on a bare "…" (showing no content
    // at all) is the worst possible outcome for this surface — the viewer sees
    // an omission marker and none of the data the surface is supposed to follow.
    //
    // Instead, show the tail of the NEWEST layout run with an INLINE LEADING
    // ellipsis ("…tail of newest line").  This keeps the omission signal (the
    // leading "…" marks that earlier lines — and possibly the head of this line
    // — were dropped) AND shows the newest content, on a single line.
    //
    // Spec-compatibility (openspec/specs/text-stream-portals/spec.md, "Transcript
    // Overflow and Ellipsis Contract"): the contract requires the omission signal
    // and forbids partially clipped glyphs; it does NOT mandate the ellipsis on a
    // dedicated line.  The leading-ellipsis line satisfies both: it carries the
    // omission signal and is horizontally truncated to fit bounds_width with no
    // clipped glyph.  Line count is exactly 1 (== max_lines).
    //
    // Considered and rejected: (a) keep bare "…" — degenerate, shows nothing;
    // (c) truncate the newest line with NO omission signal — drops the contract's
    // required omission indicator.  (b) preserves both properties, so it wins.
    if max_lines == 1 {
        // The newest content is the LAST layout run.
        let newest_run = &runs[total_runs - 1];
        let newest_line_i = newest_run.0;
        let newest_run_start = run_logical_start(&newest_run.2);
        let newest_line_text = if let Some(line) = full_buf.lines.get(newest_line_i) {
            line.text().to_owned()
        } else {
            // Fall back to the bare omission signal if the line is unavailable.
            return TruncationResult {
                text: ELLIPSIS.to_owned(),
                was_truncated: true,
            };
        };
        // Slice the newest run's logical text from its paragraph line (handles
        // word-wrapped sub-lines that start mid-paragraph).
        let run_slice = run_start_slice(&newest_line_text, newest_run_start);
        let leading = truncate_line_to_leading_ellipsis(
            run_slice,
            base_attrs,
            bounds_width,
            font_size_px,
            line_height,
            ellipsis_w,
            font_system,
        );
        return TruncationResult {
            text: leading,
            was_truncated: true,
        };
    }
    let content_run_count = max_lines - 1;
    // We reconstruct the text slice starting at the first visible run.
    let first_visible_idx = total_runs - content_run_count;
    let first_visible_run = &runs[first_visible_idx];

    // Find the byte offset within the full text that corresponds to the start
    // of the first visible run.
    let first_visible_line_i = first_visible_run.0;
    let first_visible_run_start = run_logical_start(&first_visible_run.2);

    // Walk original_text to find the byte offset of paragraph first_visible_line_i.
    //
    // Strategy: count '\n' separators.  Paragraph 0 starts at byte 0; paragraph N
    // starts at the byte immediately after the N-th '\n'.  If the text has fewer
    // than first_visible_line_i newlines, clamp to text.len().
    let mut para_byte_offset = 0usize;
    if first_visible_line_i > 0 {
        let mut current_para = 0usize;
        for (idx, ch) in text.char_indices() {
            if ch == '\n' {
                current_para += 1;
                if current_para == first_visible_line_i {
                    para_byte_offset = idx + 1; // byte after the '\n'
                    break;
                }
            }
        }
        if current_para < first_visible_line_i {
            para_byte_offset = text.len();
        }
    }

    let total_offset = para_byte_offset + first_visible_run_start;
    let safe_offset = if total_offset >= text.len() {
        text.len()
    } else {
        // Walk back to nearest valid UTF-8 boundary.
        (0..=total_offset)
            .rev()
            .find(|&o| text.is_char_boundary(o))
            .unwrap_or(0)
    };

    // The visible tail slice begins at safe_offset.
    let tail_text = &text[safe_offset..];

    // Check whether the first line of the tail slice overflows horizontally.
    // If it does, truncate with ellipsis (using the first visible run's width).
    let first_tail_run_w = first_visible_run.1;
    let visible_tail = if first_tail_run_w > bounds_width {
        // The first visible line overflows — apply horizontal ellipsis.
        // We need to truncate it against the line text for that run.
        let fv_line_i = first_visible_run.0;
        let fv_line_text = if let Some(line) = full_buf.lines.get(fv_line_i) {
            line.text().to_owned()
        } else {
            tail_text.to_owned()
        };
        let run_start_b = first_visible_run_start;
        let run_slice = run_start_slice(&fv_line_text, run_start_b);
        let truncated_first = truncate_line_to_ellipsis(
            run_slice,
            base_attrs,
            bounds_width,
            font_size_px,
            line_height,
            ellipsis_w,
            font_system,
        );
        // Reconstruct: truncated first line + the remaining tail lines.
        //
        // `truncated_first` already contains the (truncated) first visible run.
        // We must append everything in `tail_text` that comes *after* the first
        // line, i.e. from the first '\n' onwards.  Using `.lines()` + `.skip()`
        // was buggy when `first_visible_run_start != 0` (word-wrapped paragraph):
        // `skip(0)` would keep the first line in `remaining`, duplicating it.
        // Slicing from the first '\n' is correct in all cases and avoids
        // allocating an intermediate Vec.
        let remaining = if let Some(pos) = tail_text.find('\n') {
            &tail_text[pos..]
        } else {
            ""
        };
        format!("{truncated_first}{remaining}")
    } else {
        tail_text.to_owned()
    };

    // Prepend ELLIPSIS to signal omitted leading content.
    let result = format!("{ELLIPSIS}\n{visible_tail}");
    TruncationResult {
        text: result,
        was_truncated: true,
    }
}

// ─── Private helpers ──────────────────────────────────────────────────────────

/// The shared setup state produced by [`shape_runs`] once geometry has been
/// validated and the text shaped.
///
/// Both [`truncate_for_ellipsis`] and [`truncate_tail_anchored`] need exactly
/// this state before they diverge into head- vs tail-anchored selection.
struct ShapedRuns {
    /// The shaped buffer (owns the `lines` referenced by run reconstruction).
    full_buf: Buffer,
    /// Collected layout runs: `(line_i, line_w, glyph_infos)`.
    runs: Vec<LayoutRunEntry>,
    /// Number of whole lines that fit vertically.
    max_lines: usize,
    /// Shaped width of the ellipsis glyph in the base style run.
    ellipsis_w: f32,
}

/// Outcome of the shared truncation setup.
///
/// Either the caller should return `result` immediately (degenerate geometry,
/// zero visible lines, or empty text), or it receives the populated
/// [`ShapedRuns`] and proceeds with anchor-specific selection.
enum ShapeOutcome {
    /// Short-circuit: return this result without further work.
    EarlyReturn(TruncationResult),
    /// Proceed with the shaped state.
    ///
    /// Boxed because `ShapedRuns` owns a glyphon `Buffer` (~360 bytes), which
    /// would otherwise bloat every `ShapeOutcome` value. Truncation runs only
    /// on content/geometry change (not per frame), so the one-time allocation
    /// is off the hot path.
    Proceed(Box<ShapedRuns>),
}

/// Validate geometry, compute `max_lines` and the ellipsis width, shape `text`
/// with word-wrap, and collect its layout runs.
///
/// This is the verbatim-shared prologue of both truncation entry points
/// (guard → `max_lines` → ellipsis measure → shape → collect runs → empty
/// check). It returns [`ShapeOutcome::EarlyReturn`] for the degenerate /
/// zero-line / empty-text cases that previously appeared identically in both
/// functions, preserving their exact `was_truncated` semantics.
fn shape_runs<'a>(
    text: &str,
    base_attrs: Attrs<'a>,
    bounds_width: f32,
    bounds_height: f32,
    font_size_px: f32,
    line_height: f32,
    font_system: &mut FontSystem,
) -> ShapeOutcome {
    // Guard: degenerate or non-finite geometry produces an empty result.
    // NaN comparisons always return false, so we must check is_finite() before
    // arithmetic that feeds into floor() / usize casts.
    if !bounds_width.is_finite()
        || !bounds_height.is_finite()
        || !font_size_px.is_finite()
        || !line_height.is_finite()
        || bounds_width <= 0.0
        || bounds_height <= 0.0
        || font_size_px <= 0.0
    {
        return ShapeOutcome::EarlyReturn(TruncationResult {
            text: String::new(),
            was_truncated: !text.is_empty(),
        });
    }

    // ── Step 1: determine how many whole lines fit vertically ────────────────
    let max_lines = max_whole_lines(bounds_height, line_height);
    if max_lines == 0 {
        return ShapeOutcome::EarlyReturn(TruncationResult {
            text: String::new(),
            was_truncated: !text.is_empty(),
        });
    }

    // ── Step 2: measure the ellipsis glyph width ────────────────────────────
    let ellipsis_w = measure_single_line(
        ELLIPSIS,
        base_attrs,
        bounds_width * 2.0,
        font_size_px,
        line_height,
        font_system,
    );

    // ── Step 3: shape the full text with word-wrap enabled ───────────────────
    // We shape with Wrap::Word and bounds_width so that layout_runs() reflects
    // the actual word-wrapped line structure.  Multiple LayoutRuns can share the
    // same line_i when a paragraph soft-wraps.
    let mut full_buf = Buffer::new(font_system, Metrics::new(font_size_px, line_height));
    full_buf.set_size(font_system, Some(bounds_width), None);
    full_buf.set_wrap(font_system, Wrap::Word);
    full_buf.set_text(font_system, text, base_attrs, Shaping::Advanced);
    full_buf.shape_until_scroll(font_system, false);

    // ── Step 4: collect rendered lines and their widths ───────────────────────
    // LayoutRun gives us (line_i, line_w, glyphs, text).
    // line_i is the index into buffer.lines[] (which is a paragraph/hard-wrap unit).
    // Multiple LayoutRuns can share the same line_i when word-wrap splits a paragraph.
    let runs: Vec<LayoutRunEntry> = full_buf
        .layout_runs()
        .map(|run| {
            // Collect (start_byte_in_line, end_byte_in_line, glyph_x_right) per glyph.
            let glyph_info: Vec<GlyphInfo> = run
                .glyphs
                .iter()
                .map(|g| (g.start, g.end, g.x + g.w))
                .collect();
            (run.line_i, run.line_w, glyph_info)
        })
        .collect();

    // If the text produces no runs at all (empty text), return as-is.
    if runs.is_empty() {
        return ShapeOutcome::EarlyReturn(TruncationResult {
            text: text.to_owned(),
            was_truncated: false,
        });
    }

    ShapeOutcome::Proceed(Box::new(ShapedRuns {
        full_buf,
        runs,
        max_lines,
        ellipsis_w,
    }))
}

/// Handle the "all lines fit vertically" case: check every run for horizontal
/// overflow and, if any overflows, truncate at the first overflowing run.
///
/// Returns `None` when no truncation is applied — the caller returns the
/// original text unchanged (`was_truncated: false`). This covers both the
/// "entire text fits" case and the (defensive) "overflow run's source line is
/// missing" case; the original code returned the same unchanged result for
/// both. Returns `Some(result)` when at least one run overflows horizontally
/// and was truncated with an ellipsis.
///
/// This is the shared body of Step 5 (head-anchored) / Step 5 (tail-anchored),
/// which were previously byte-for-byte identical. The horizontal-overflow scan
/// over **all** runs (not just the last) is the hud-so7zu fix; centralizing it
/// here means that fix lives once.
fn try_horizontal_truncate<'a>(
    shaped: &ShapedRuns,
    text: &str,
    base_attrs: Attrs<'a>,
    bounds_width: f32,
    font_size_px: f32,
    line_height: f32,
    font_system: &mut FontSystem,
) -> Option<TruncationResult> {
    let runs = &shaped.runs;
    let full_buf = &shaped.full_buf;
    let ellipsis_w = shaped.ellipsis_w;

    // Check ALL runs for horizontal overflow, not just the last one.
    // A long unbreakable token on any non-final line overflows horizontally
    // and would be clipped by TextBounds without truncation (hud-so7zu).
    // `None` (no overflow) → caller returns the original text unchanged.
    let overflow_idx = runs.iter().position(|run| run.1 > bounds_width)?;

    // At least one run overflows horizontally: truncate at the first
    // overflowing run so the result fits within bounds_width.
    let overflow_run = &runs[overflow_idx];
    let overflow_line_i = overflow_run.0;
    // Defensive: if the source line is missing, fall back to the unchanged
    // result (matches the original `was_truncated: false` fallback).
    let overflow_line_text = full_buf.lines.get(overflow_line_i)?.text().to_owned();

    // Reconstruct the prefix of `text` that corresponds to all runs before
    // the overflowing run, then truncate only the slice of the paragraph
    // that belongs to the overflowing run (avoiding duplication for
    // word-wrapped paragraphs).
    let prefix = text_prefix_up_to_run(runs, overflow_idx, text, full_buf);
    // Use min(g.start) so RTL runs resolve to the logical start, not the
    // visual-first (logical-last) glyph that glyphs[0] gives for RTL text.
    let run_start = run_logical_start(&overflow_run.2);
    let run_slice = run_start_slice(&overflow_line_text, run_start);
    let truncated_overflow = truncate_line_to_ellipsis(
        run_slice,
        base_attrs,
        bounds_width,
        font_size_px,
        line_height,
        ellipsis_w,
        font_system,
    );
    let result = format!("{prefix}{truncated_overflow}");
    Some(TruncationResult {
        text: result,
        was_truncated: true,
    })
}

/// Compute the maximum number of whole lines that fit in `bounds_height`
/// given `line_height`.
///
/// Returns at least 0.  A line_height ≤ 0 returns 0.
#[inline]
fn max_whole_lines(bounds_height: f32, line_height: f32) -> usize {
    if line_height <= 0.0 {
        return 0;
    }
    (bounds_height / line_height).floor() as usize
}

/// Measure the advance width of `text` when shaped as a single line with the
/// given parameters.  Returns the `line_w` of the first `LayoutRun`.
///
/// This shapes the text in a buffer wide enough that no word-wrapping occurs
/// (width = `max_width`), so the result is the natural single-line width.
fn measure_single_line<'a>(
    text: &str,
    base_attrs: Attrs<'a>,
    max_width: f32,
    font_size_px: f32,
    line_height: f32,
    font_system: &mut FontSystem,
) -> f32 {
    let mut buf = Buffer::new(font_system, Metrics::new(font_size_px, line_height));
    buf.set_size(font_system, Some(max_width), None);
    buf.set_wrap(font_system, Wrap::None);
    buf.set_text(font_system, text, base_attrs, Shaping::Advanced);
    buf.shape_until_scroll(font_system, false);
    buf.layout_runs().next().map(|r| r.line_w).unwrap_or(0.0)
}

/// Reconstruct the text prefix that corresponds to runs `[0, run_idx)`.
///
/// We build this by finding the absolute byte offset in `original_text` that
/// corresponds to the start of `runs[run_idx]`.  The algorithm:
///
/// 1. Walk `original_text` scanning for `\n` characters to find the byte
///    offset of the start of paragraph `target_line_i`.  (Scanning for `\n`
///    handles both `\n` and `\r\n` line endings correctly.)
/// 2. Add the intra-paragraph offset from the first glyph of `runs[run_idx]`
///    to account for word-wrapped sub-lines within the same paragraph.
/// 3. Clamp to a valid UTF-8 boundary.
fn text_prefix_up_to_run(
    runs: &[LayoutRunEntry],
    run_idx: usize,
    original_text: &str,
    _buf: &Buffer,
) -> String {
    if run_idx == 0 {
        return String::new();
    }

    let target_line_i = runs[run_idx].0;

    // Step 1: find the byte offset of the start of paragraph `target_line_i`
    // by scanning for '\n' separators.  This handles both '\n' and '\r\n'.
    let mut byte_offset = 0usize;
    let mut current_line = 0usize;
    for (idx, ch) in original_text.char_indices() {
        if current_line == target_line_i {
            byte_offset = idx;
            break;
        }
        if ch == '\n' {
            current_line += 1;
            // byte_offset will be updated on the next iteration
        }
    }
    // If the loop exhausted all characters without reaching target_line_i,
    // byte_offset remains at whatever it was (safe: clamped below).

    // Step 2: add the intra-paragraph glyph start offset.  This is the byte
    // offset of the logical start of `runs[run_idx]` within its paragraph line,
    // which accounts for word-wrapped sub-lines starting mid-paragraph.
    //
    // We use min(g.start) across all glyphs instead of glyphs[0].start because
    // cosmic-text orders glyphs in visual order: for RTL runs glyphs[0] is the
    // visual-first / logical-last glyph, so glyphs[0].start would point past
    // the run's actual logical content start.
    let intra_para_offset = run_logical_start(&runs[run_idx].2);

    let total_offset = byte_offset + intra_para_offset;

    if total_offset > original_text.len() {
        original_text.to_owned()
    } else {
        // Step 3: walk back to the nearest valid UTF-8 boundary.
        let safe_offset = (0..=total_offset)
            .rev()
            .find(|&o| original_text.is_char_boundary(o))
            .unwrap_or(0);
        original_text[..safe_offset].to_owned()
    }
}

/// Slice `paragraph_text` from `run_start_byte` to the end, clamping to a
/// valid UTF-8 boundary.  Used to extract the text visible in a word-wrapped
/// run from its parent paragraph string.
fn run_start_slice(paragraph_text: &str, run_start_byte: usize) -> &str {
    if run_start_byte == 0 {
        return paragraph_text;
    }
    if run_start_byte >= paragraph_text.len() {
        return "";
    }
    // Walk forward from run_start_byte to find a valid UTF-8 boundary.
    let safe = (run_start_byte..=paragraph_text.len())
        .find(|&o| paragraph_text.is_char_boundary(o))
        .unwrap_or(paragraph_text.len());
    &paragraph_text[safe..]
}

/// Truncate a single line of text so that the result (with `"…"` appended)
/// fits within `bounds_width`.
///
/// # Algorithm (sub-quadratic — O(n log n))
///
/// The old O(W·k) implementation issued one full `measure_single_line` reshape
/// per candidate in a right-to-left linear scan (O(k) per candidate, O(W·k) =
/// O(n²) total in the worst case).  This version uses binary search to reduce
/// shape calls from O(W) to O(log W):
///
/// 1. Collect word-boundary byte offsets into a sorted slice.
/// 2. Binary-search for the largest boundary where `measure("prefix…") ≤
///    bounds_width`.  The combined shape is measured as a single unit so that
///    any kerning/ligature between the last glyph of the prefix and `"…"` is
///    accounted for (critical with `Shaping::Advanced`).  The predicate is
///    monotone: once a prefix+ellipsis is too wide, all longer prefixes also
///    produce a wider result.  Binary search issues O(log W) shape calls.
/// 3. If no non-empty word boundary fits, repeat the binary search over
///    grapheme-cluster boundaries (O(log G) shapes, G = grapheme count).
/// 4. If even a single grapheme + ellipsis does not fit, return just `"…"`.
///
/// # RTL / bidi safety
///
/// Width is measured by reshaping the candidate prefix — the same
/// `measure_single_line` path used before.  Logical vs visual glyph ordering
/// (cosmic-text RTL fix, PR #676) is therefore preserved: we shape the logical
/// prefix and measure `line_w`, not glyph x-positions.
///
/// # Grapheme-cluster safety
///
/// Boundaries come from `unicode_segmentation::grapheme_indices`, which
/// guarantees whole-cluster cuts.  The binary-search pivot is always clamped
/// to a `char_boundary` before slicing.
fn truncate_line_to_ellipsis<'a>(
    line: &str,
    base_attrs: Attrs<'a>,
    bounds_width: f32,
    font_size_px: f32,
    line_height: f32,
    ellipsis_w: f32,
    font_system: &mut FontSystem,
) -> String {
    let budget = bounds_width - ellipsis_w;

    // Fast path: if budget ≤ 0, return just the ellipsis (nothing else fits).
    if budget <= 0.0 {
        return ELLIPSIS.to_owned();
    }

    // ── Helper: measure prefix+ellipsis width as a unit ─────────────────────
    // With Shaping::Advanced, kerning between the final glyph of the prefix and
    // the ellipsis character means the combined width can differ from
    // measure(prefix) + measure(ELLIPSIS).  Measuring the full candidate string
    // as one shaped unit is the only way to guarantee the result fits within
    // bounds_width.  We therefore use bounds_width as the fitting budget rather
    // than the pre-subtracted `budget`.
    let mut measure_with_ellipsis = |boundary: usize| -> f32 {
        if boundary == 0 {
            // Zero-length prefix: the result is just ELLIPSIS.
            return measure_single_line(
                ELLIPSIS,
                base_attrs,
                bounds_width * 2.0,
                font_size_px,
                line_height,
                font_system,
            );
        }
        let candidate = &line[..boundary];
        let trimmed = candidate.trim_end();
        // Shape "prefix…" as a single unit so kerning/ligatures between the
        // last glyph of the prefix and "…" are accounted for.
        let with_ellipsis = format!("{trimmed}{ELLIPSIS}");
        measure_single_line(
            &with_ellipsis,
            base_attrs,
            bounds_width * 2.0,
            font_size_px,
            line_height,
            font_system,
        )
    };

    // ── Step 1: word-boundary binary search ──────────────────────────────────
    //
    // Collect only valid char-boundary word ends (unicode_word_indices yields
    // pairs whose end bytes are always char boundaries, but we guard anyway).
    //
    // `unicode_word_indices()` yields `(start, word)` pairs where every word
    // has length >= 1, so every boundary > 0.  No empty-prefix guard needed.
    let word_boundaries: Vec<usize> = line
        .unicode_word_indices()
        .map(|(i, word)| i + word.len())
        .filter(|&b| line.is_char_boundary(b))
        .collect();

    // Binary search: find the rightmost word boundary such that
    // `measure_with_ellipsis(boundary) ≤ bounds_width`.
    //
    // We compare against bounds_width (not budget) because measure_with_ellipsis
    // already includes the ellipsis in the shaped measurement.
    if let Some(boundary) =
        binary_search_largest_fitting(&word_boundaries, bounds_width, &mut measure_with_ellipsis)
    {
        let candidate = &line[..boundary];
        let trimmed = candidate.trim_end();
        return format!("{trimmed}{ELLIPSIS}");
    }

    // ── Step 2: grapheme-cluster fallback (binary search) ────────────────────
    //
    // No word boundary fits — fall back to grapheme-cluster boundaries so that
    // long unbroken tokens still show a visible prefix before the ellipsis.
    let grapheme_boundaries: Vec<usize> = line
        .grapheme_indices(true)
        .map(|(i, g)| i + g.len())
        .filter(|&b| line.is_char_boundary(b))
        .collect();

    if let Some(boundary) = binary_search_largest_fitting(
        &grapheme_boundaries,
        bounds_width,
        &mut measure_with_ellipsis,
    ) {
        let candidate = &line[..boundary];
        let trimmed = candidate.trim_end();
        return format!("{trimmed}{ELLIPSIS}");
    }

    // Even a single grapheme + ellipsis does not fit — return just the ellipsis.
    ELLIPSIS.to_owned()
}

/// Truncate a single line from the **left** so that the result
/// (`"…"` + visible suffix) fits within `bounds_width`.
///
/// This is the leading-ellipsis mirror of [`truncate_line_to_ellipsis`].  It is
/// used by [`truncate_tail_anchored`] in the degenerate `max_lines == 1` case:
/// a follow-tail surface exists to show the **newest** line, so the single line
/// slot must carry the *tail* of the newest content with a leading `"…"` that
/// signals omitted leading content — never a bare `"…"` that shows no content.
///
/// # Algorithm
///
/// Symmetric to the trailing-ellipsis path but searching for the **smallest**
/// cut offset whose suffix fits (i.e. the *longest* visible tail).  The
/// predicate `measure("…" + line[cut..]) ≤ bounds_width` is monotone in `cut`:
/// a larger cut yields a shorter, narrower suffix, so once a suffix fits every
/// larger cut also fits.  We binary-search the cut boundaries to find the
/// smallest fitting one.
///
/// 1. If `"…" + line` already fits, return it (no leading omission inside the
///    line — only the leading `"…"` marking omitted earlier lines).
/// 2. Otherwise binary-search word-start boundaries for the smallest fitting
///    cut; the visible tail begins on a word boundary.
/// 3. Fall back to grapheme-cluster boundaries when no word boundary fits, so a
///    long unbroken token still shows a visible suffix.
/// 4. If not even a single trailing grapheme + ellipsis fits, return bare `"…"`.
///
/// # Guarantees
///
/// - The result is shaped to fit `bounds_width` — no horizontally clipped glyph.
/// - Cuts land on grapheme-cluster (and char) boundaries — no split clusters.
/// - RTL/bidi safety matches [`truncate_line_to_ellipsis`]: width comes from
///   reshaping the logical suffix, never from glyph x-positions.
fn truncate_line_to_leading_ellipsis<'a>(
    line: &str,
    base_attrs: Attrs<'a>,
    bounds_width: f32,
    font_size_px: f32,
    line_height: f32,
    ellipsis_w: f32,
    font_system: &mut FontSystem,
) -> String {
    let budget = bounds_width - ellipsis_w;

    // Fast path: if budget ≤ 0, only the ellipsis itself fits.
    if budget <= 0.0 {
        return ELLIPSIS.to_owned();
    }

    // ── Helper: measure "…" + suffix-from-`cut` as a single shaped unit ──────
    // Measuring the combined string accounts for kerning/ligatures between the
    // ellipsis and the first glyph of the suffix under Shaping::Advanced.
    let mut measure_with_leading_ellipsis = |cut: usize| -> f32 {
        if cut >= line.len() {
            // Empty suffix: the result is just ELLIPSIS.
            return measure_single_line(
                ELLIPSIS,
                base_attrs,
                bounds_width * 2.0,
                font_size_px,
                line_height,
                font_system,
            );
        }
        let suffix = line[cut..].trim_start();
        let with_ellipsis = format!("{ELLIPSIS}{suffix}");
        measure_single_line(
            &with_ellipsis,
            base_attrs,
            bounds_width * 2.0,
            font_size_px,
            line_height,
            font_system,
        )
    };

    // Fast path: the whole line plus a leading ellipsis fits — show it all.
    if measure_with_leading_ellipsis(0) <= bounds_width {
        let suffix = line.trim_start();
        return format!("{ELLIPSIS}{suffix}");
    }

    // ── Step 1: word-start boundary binary search ────────────────────────────
    //
    // Candidate cut points are the START byte offsets of words.  A larger cut
    // means a shorter suffix, which is the monotone-narrowing direction, so we
    // search for the SMALLEST fitting cut (longest visible tail).
    let word_cuts: Vec<usize> = line
        .unicode_word_indices()
        .map(|(i, _word)| i)
        .filter(|&b| b > 0 && line.is_char_boundary(b))
        .collect();

    if let Some(cut) =
        binary_search_smallest_fitting(&word_cuts, bounds_width, &mut measure_with_leading_ellipsis)
    {
        let suffix = line[cut..].trim_start();
        return format!("{ELLIPSIS}{suffix}");
    }

    // ── Step 2: grapheme-cluster boundary fallback ───────────────────────────
    //
    // No word-start cut fits — fall back to grapheme-cluster boundaries so a
    // long unbroken token still shows a visible suffix before/after the cut.
    let grapheme_cuts: Vec<usize> = line
        .grapheme_indices(true)
        .map(|(i, _g)| i)
        .filter(|&b| b > 0 && line.is_char_boundary(b))
        .collect();

    if let Some(cut) = binary_search_smallest_fitting(
        &grapheme_cuts,
        bounds_width,
        &mut measure_with_leading_ellipsis,
    ) {
        let suffix = line[cut..].trim_start();
        return format!("{ELLIPSIS}{suffix}");
    }

    // Even a single trailing grapheme + ellipsis does not fit — bare ellipsis.
    ELLIPSIS.to_owned()
}

/// Find the **smallest** element in the **sorted, ascending** `boundaries`
/// slice such that `measure(boundary) ≤ budget`, using binary search.
///
/// This is the mirror of [`binary_search_largest_fitting`] for leading-ellipsis
/// (suffix) truncation, where a larger cut offset yields a shorter, narrower
/// suffix.
///
/// Returns `None` if no boundary fits (all are too wide or the slice is empty).
///
/// # Correctness requirement
///
/// The predicate `measure(b) ≤ budget` must be **monotone increasing in `b`**:
/// if cut `b` fits, every larger cut `b' > b` also fits.  This holds for suffix
/// width because advancing the cut can only remove leading characters, which
/// cannot increase the shaped width.
///
/// # Complexity
///
/// O(log N) calls to `measure` where N = `boundaries.len()`.
fn binary_search_smallest_fitting(
    boundaries: &[usize],
    budget: f32,
    measure: &mut impl FnMut(usize) -> f32,
) -> Option<usize> {
    if !budget.is_finite() || boundaries.is_empty() {
        return None;
    }

    // Quick check: does the smallest cut (longest suffix) fit?  If yes, take it
    // directly (common case: the line is only slightly too wide).
    let first = boundaries[0];
    if measure(first) <= budget {
        return Some(first);
    }

    // Quick check: does even the largest cut (shortest suffix) fit?  If not,
    // nothing fits.
    let last = *boundaries.last().unwrap();
    if measure(last) > budget {
        return None;
    }

    // Binary search over the index space [0, boundaries.len()).
    // Invariant: boundaries[lo] does NOT fit, boundaries[hi] fits.
    let mut lo = 0usize;
    let mut hi = boundaries.len() - 1;

    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if measure(boundaries[mid]) <= budget {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    // `hi` is the first index where the predicate holds.
    Some(boundaries[hi])
}

/// Find the largest element in the **sorted, ascending** `boundaries` slice
/// such that `measure(boundary) ≤ budget`, using binary search.
///
/// Returns `None` if no boundary fits (all are too wide or the slice is empty).
///
/// # Correctness requirement
///
/// The predicate `measure(b) ≤ budget` must be **monotone**: if boundary `b`
/// fits, all boundaries `b' < b` also fit.  This holds for text width because
/// removing characters from a prefix cannot increase its shaped width.
///
/// # Complexity
///
/// O(log N) calls to `measure` where N = `boundaries.len()`.
fn binary_search_largest_fitting(
    boundaries: &[usize],
    budget: f32,
    measure: &mut impl FnMut(usize) -> f32,
) -> Option<usize> {
    if !budget.is_finite() || boundaries.is_empty() {
        return None;
    }

    // Quick check: does the largest boundary fit?  If yes, return it directly
    // (common case: the line nearly fits and only needs a tiny trim).
    let last = *boundaries.last().unwrap();
    if measure(last) <= budget {
        return Some(last);
    }

    // Quick check: does even the smallest boundary fit?  If not, nothing fits.
    let first = boundaries[0];
    if measure(first) > budget {
        return None;
    }

    // Binary search over the index space [0, boundaries.len()).
    // Invariant: boundaries[lo] fits, boundaries[hi] does not fit.
    let mut lo = 0usize;
    let mut hi = boundaries.len() - 1;

    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if measure(boundaries[mid]) <= budget {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    // `lo` is the last index where the predicate holds.
    Some(boundaries[lo])
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use glyphon::FontSystem;
    use proptest::prelude::*;

    /// Helper: create a fresh FontSystem for testing.
    fn make_font_system() -> FontSystem {
        FontSystem::new()
    }

    fn base_attrs() -> Attrs<'static> {
        Attrs::new()
    }

    // ── Task 3.4: max_whole_lines invariants ──────────────────────────────────

    #[test]
    fn max_whole_lines_cases() {
        // (case, height, line_height, expected)
        let cases = [
            ("exact fit", 60.0, 20.0, 3),
            ("partial line excluded", 65.0, 20.0, 3),
            ("zero height", 0.0, 20.0, 0),
            ("zero line height", 100.0, 0.0, 0),
            ("smaller than one line", 10.0, 20.0, 0),
        ];
        for (name, h, lh, expected) in cases {
            assert_eq!(max_whole_lines(h, lh), expected, "case: {name}");
        }
    }

    // ── Task 3.4: ellipsis content correctness ────────────────────────────────

    /// When a line fits, truncate_for_ellipsis returns the original text unchanged.
    #[test]
    fn short_text_fits_unchanged() {
        let mut fs = make_font_system();
        let text = "Hi";
        let result = truncate_for_ellipsis(
            text,
            base_attrs(),
            1000.0, // very wide
            100.0,
            16.0,
            22.4,
            &mut fs,
        );
        assert_eq!(
            result.text, text,
            "short text that fits must not be truncated"
        );
        assert!(
            !result.was_truncated,
            "was_truncated must be false when text fits"
        );
    }

    /// Empty text returns empty result with no truncation.
    #[test]
    fn empty_text_no_truncation() {
        let mut fs = make_font_system();
        let result = truncate_for_ellipsis("", base_attrs(), 200.0, 100.0, 16.0, 22.4, &mut fs);
        assert_eq!(result.text, "", "empty text must remain empty");
        assert!(!result.was_truncated);
    }

    /// Degenerate geometry (zero width) produces empty result.
    #[test]
    fn zero_width_produces_empty() {
        let mut fs = make_font_system();
        let result = truncate_for_ellipsis("hello", base_attrs(), 0.0, 100.0, 16.0, 22.4, &mut fs);
        assert_eq!(result.text, "", "zero-width bounds must produce empty text");
        assert!(
            result.was_truncated,
            "was_truncated must be true for non-empty input"
        );
    }

    /// NaN and infinite geometry inputs are guarded: no panic, empty result with
    /// was_truncated=true for non-empty input.
    ///
    /// Mirrors the is_finite() guard added to `truncate_tail_anchored` in
    /// PR #678/#684.  NaN bypasses `<= 0.0` comparisons and flows into
    /// floor()/usize casts, panicking in debug mode.
    #[test]
    fn non_finite_geometry_produces_empty_no_panic() {
        let inputs: &[(&str, f32, f32, f32, f32)] = &[
            // (label omitted — indexed below)
            ("hello", f32::NAN, 100.0, 16.0, 22.4),
            ("hello", 200.0, f32::NAN, 16.0, 22.4),
            ("hello", 200.0, 100.0, f32::NAN, 22.4),
            ("hello", 200.0, 100.0, 16.0, f32::NAN),
            ("hello", f32::INFINITY, 100.0, 16.0, 22.4),
            ("hello", 200.0, f32::INFINITY, 16.0, 22.4),
            ("hello", 200.0, 100.0, f32::INFINITY, 22.4),
            ("hello", 200.0, 100.0, 16.0, f32::INFINITY),
            ("hello", f32::NEG_INFINITY, 100.0, 16.0, 22.4),
            ("hello", 200.0, f32::NEG_INFINITY, 16.0, 22.4),
            // Empty text must still return empty with was_truncated=false.
            ("", f32::NAN, 100.0, 16.0, 22.4),
        ];
        for (i, &(text, bw, bh, fs_px, lh)) in inputs.iter().enumerate() {
            let mut font_system = make_font_system();
            let result =
                truncate_for_ellipsis(text, base_attrs(), bw, bh, fs_px, lh, &mut font_system);
            assert_eq!(
                result.text, "",
                "input #{i}: non-finite geometry must produce empty text; \
                 bounds_width={bw} bounds_height={bh} font_size={fs_px} line_height={lh}"
            );
            let expected_truncated = !text.is_empty();
            assert_eq!(
                result.was_truncated,
                expected_truncated,
                "input #{i}: was_truncated must be {} for {:?} input; \
                 bounds_width={bw} bounds_height={bh} font_size={fs_px} line_height={lh}",
                expected_truncated,
                if text.is_empty() {
                    "empty"
                } else {
                    "non-empty"
                },
            );
        }
    }

    /// When text is truncated, the result ends with the ellipsis character.
    #[test]
    fn truncated_text_ends_with_ellipsis() {
        let mut fs = make_font_system();
        // Very narrow box: 60px wide — any non-trivial text will be truncated.
        let long = "This is a long sentence that surely does not fit in sixty pixels.";
        let result = truncate_for_ellipsis(long, base_attrs(), 60.0, 100.0, 16.0, 22.4, &mut fs);
        if result.was_truncated {
            assert!(
                result.text.ends_with(ELLIPSIS),
                "truncated text must end with '…'; got: {:?}",
                result.text
            );
        }
    }

    /// Truncated result must be strictly shorter than the original text.
    #[test]
    fn truncated_text_is_shorter() {
        let mut fs = make_font_system();
        let long = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; // 37 'A's
        let result = truncate_for_ellipsis(long, base_attrs(), 60.0, 100.0, 16.0, 22.4, &mut fs);
        if result.was_truncated {
            assert!(
                result.text.len() < long.len() + ELLIPSIS.len(),
                "truncated text must be shorter than original + ellipsis; got {} chars",
                result.text.chars().count()
            );
        }
    }

    /// No glyph is partially visible: result text does not exceed bounds_width
    /// when shaped.
    ///
    /// This is a structural guarantee: we trust our measurement. The test
    /// below verifies that the returned text re-measures as fitting.
    #[test]
    fn truncated_text_remeasures_within_width() {
        let mut fs = make_font_system();
        let long = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; // long enough to need truncation at 60px
        let bounds_w = 60.0;
        let font_size = 16.0;
        let line_h = 22.4;
        let result = truncate_for_ellipsis(
            long,
            base_attrs(),
            bounds_w,
            100.0,
            font_size,
            line_h,
            &mut fs,
        );
        if result.was_truncated {
            // Re-measure the first line of the result.
            let measured_w = measure_single_line(
                &result.text,
                base_attrs(),
                bounds_w * 4.0,
                font_size,
                line_h,
                &mut fs,
            );
            assert!(
                measured_w <= bounds_w + 1.0, // +1.0 for floating-point tolerance
                "truncated text must re-measure ≤ bounds_width ({bounds_w}px); got {measured_w:.2}px"
            );
        }
    }

    /// Multi-line text: lines beyond max_whole_lines are dropped.
    #[test]
    fn multiline_beyond_height_is_truncated() {
        let mut fs = make_font_system();
        // 3 lines, but only 1 line's worth of height.
        let text = "Line one\nLine two\nLine three";
        let font_size = 16.0;
        let line_h = 22.4;
        // Only ~1 line fits.
        let result = truncate_for_ellipsis(
            text,
            base_attrs(),
            500.0,
            line_h + 1.0,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            result.was_truncated,
            "multi-line text taller than bounds must be truncated"
        );
        // The result must not contain the last line's content.
        assert!(
            !result.text.contains("Line three"),
            "lines beyond height must be dropped; got: {:?}",
            result.text
        );
    }

    /// Grapheme fallback: single long word without spaces is truncated at a
    /// grapheme boundary.
    ///
    /// Regression guard for the "dead-code grapheme fallback" bug: previously
    /// the word-boundary loop claimed boundary 0 (empty prefix) which always
    /// fitted the budget, producing bare `"…"` with zero visible clusters.
    #[test]
    fn single_long_word_truncated_at_grapheme_boundary() {
        let mut fs = make_font_system();
        // A long unbroken token — no word boundaries within it.
        let long_word = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; // 40 'A's
        let result =
            truncate_for_ellipsis(long_word, base_attrs(), 60.0, 100.0, 16.0, 22.4, &mut fs);
        assert!(
            result.was_truncated,
            "40 'A's in a 60px box must be truncated; got: {:?}",
            result.text
        );
        assert!(
            result.text.ends_with(ELLIPSIS),
            "grapheme-fallback result must end with '…'; got: {:?}",
            result.text
        );
        // The prefix before the ellipsis must be non-empty: at least one visible
        // grapheme cluster must precede the ellipsis character.
        let prefix_before_ellipsis = result.text.strip_suffix(ELLIPSIS).unwrap_or("");
        assert!(
            !prefix_before_ellipsis.is_empty(),
            "grapheme-fallback must include at least one visible cluster before '…'; \
             got bare '…' — long unbroken token regression; result: {:?}",
            result.text
        );
        assert!(
            result.text.len() < long_word.len() + ELLIPSIS.len(),
            "grapheme-fallback result must be shorter than original + ellipsis"
        );
    }

    /// Canonical repro from the bug report: `'W'×30` in a `200px` box must yield
    /// at least one visible `W` cluster before the ellipsis, not bare `"…"`.
    ///
    /// Source: issue hud-wq6qp — "Ellipsis grapheme fallback is dead code".
    ///
    /// Preconditions are measured at runtime so the test remains correct across
    /// environments with different default font metrics:
    ///   1. A single `"W"` + ellipsis must fit within 200px (otherwise there is
    ///      no room to show even one cluster and a bare `"…"` would be correct).
    ///   2. The full 30-`"W"` token must overflow 200px (otherwise truncation
    ///      would not occur and there is nothing to test).
    #[test]
    fn long_unbroken_token_w30_in_200px_has_visible_prefix() {
        let mut fs = make_font_system();
        let font_size = 16.0_f32;
        let line_h = 22.4_f32;
        let bounds_w = 200.0_f32;

        // Measure preconditions using the same font system and parameters.
        let ellipsis_w = measure_single_line(
            ELLIPSIS,
            base_attrs(),
            bounds_w * 2.0,
            font_size,
            line_h,
            &mut fs,
        );
        let single_w_w = measure_single_line(
            "W",
            base_attrs(),
            bounds_w * 2.0,
            font_size,
            line_h,
            &mut fs,
        );
        let text = "W".repeat(30);
        let full_w = measure_single_line(
            &text,
            base_attrs(),
            bounds_w * 2.0,
            font_size,
            line_h,
            &mut fs,
        );

        // Pre-condition 1: at least one cluster + ellipsis must fit.
        // If this fails the font is so wide that a bare "…" is the correct answer.
        if single_w_w + ellipsis_w > bounds_w {
            // Cannot run the non-empty-prefix assertion — skip gracefully.
            return;
        }
        // Pre-condition 2: the full token must overflow.
        if full_w <= bounds_w {
            // Cannot run the was_truncated assertion — skip gracefully.
            return;
        }

        let result = truncate_for_ellipsis(
            &text,
            base_attrs(),
            bounds_w,
            100.0,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            result.was_truncated,
            "30 'W's ({full_w:.1}px) in a {bounds_w}px box must be truncated; got was_truncated=false"
        );
        assert!(
            result.text.ends_with(ELLIPSIS),
            "truncated long-token result must end with '…'; got: {:?}",
            result.text
        );
        let prefix = result
            .text
            .strip_suffix(ELLIPSIS)
            .expect("ends_with ELLIPSIS already asserted");
        assert!(
            !prefix.is_empty(),
            "long unbroken token must produce at least one visible cluster before '…'; \
             got bare '…' — grapheme fallback regression; result: {:?}",
            result.text
        );
        // Verify all characters in the prefix are 'W' (no corruption / mid-codepoint split).
        assert!(
            prefix.chars().all(|c| c == 'W'),
            "prefix before ellipsis must consist only of source characters; got: {prefix:?}",
        );
    }

    /// Multi-codepoint grapheme cluster boundary: combining characters must not
    /// be split.  Uses a base letter + combining acute accent (U+0301) as the
    /// cluster unit.  The fallback must only cut at cluster boundaries.
    #[test]
    fn grapheme_cluster_boundary_not_split_mid_cluster() {
        let mut fs = make_font_system();
        // Build a string of 20 "é" (e + combining acute, 2 bytes each → 20 clusters).
        // Using the decomposed form to guarantee a multi-codepoint cluster.
        let cluster = "e\u{0301}"; // 'e' + combining acute accent (NFD)
        let text: String = cluster.repeat(20);
        // Verify our test input is actually multi-byte-per-cluster
        let cluster_count = text.graphemes(true).count();
        assert_eq!(
            cluster_count, 20,
            "expected 20 grapheme clusters in test input"
        );

        let result = truncate_for_ellipsis(&text, base_attrs(), 80.0, 100.0, 16.0, 22.4, &mut fs);
        if result.was_truncated {
            assert!(
                result.text.ends_with(ELLIPSIS),
                "multi-codepoint-cluster truncation must end with '…'; got: {:?}",
                result.text
            );
            // The prefix before the ellipsis must be valid UTF-8 and consist of
            // whole grapheme clusters only.
            let prefix = result.text.strip_suffix(ELLIPSIS).unwrap_or("");
            // Every grapheme in the prefix must equal the original cluster unit.
            // If the fallback split a cluster mid-codepoint we'd get partial
            // graphemes (e.g. "e" without the accent).
            for g in prefix.graphemes(true) {
                // Accept either the precomposed form or NFD form — the font system
                // may normalise; what matters is no lone combining codepoints.
                let has_lone_combining = g
                    .chars()
                    .next()
                    .map(|c| {
                        // Unicode "combining character" range: 0x0300..=0x036F and beyond.
                        // A lone combining as the *first* codepoint of a grapheme cluster
                        // means we split before the base.
                        (c as u32) >= 0x0300 && (c as u32) <= 0x036F
                    })
                    .unwrap_or(false);
                assert!(
                    !has_lone_combining,
                    "grapheme cluster boundary violated: prefix grapheme {g:?} starts with \
                     a combining codepoint — the fallback split a multi-codepoint cluster; \
                     full result: {:?}",
                    result.text
                );
            }
            // At least one whole cluster must be visible before the ellipsis.
            assert!(
                !prefix.is_empty(),
                "at least one grapheme cluster must precede '…' in the truncated result; \
                 got bare '…'; result: {:?}",
                result.text
            );
        }
    }

    // ── Task 3.4: property-based tests (proptest) ────────────────────────────
    //
    // These use proptest to verify invariants across random content/widths.
    // FontSystem is not Send, so each proptest body owns its own instance.
    //
    // Iteration count is 32 to keep total CI time well under 10 s in debug mode
    // with software font rasterisation.  The invariant guarantee comes from random
    // input coverage, not iteration count; real regression detection uses criterion
    // benchmarks in crates/tze_hud_compositor/benches/overflow_truncate.rs.

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 32,
            source_file: Some("crates/tze_hud_compositor/src/overflow.rs"),
            ..proptest::test_runner::Config::default()
        })]

        /// Invariant: if truncation occurs, the result always ends with ELLIPSIS.
        #[test]
        fn proptest_truncated_always_ends_with_ellipsis(
            text in "[a-zA-Z0-9 ]{1,80}",
            width_px in 20.0_f32..200.0_f32,
        ) {
            let mut fs = make_font_system();
            let result = truncate_for_ellipsis(
                &text, base_attrs(), width_px, 200.0, 16.0, 22.4, &mut fs,
            );
            if result.was_truncated {
                prop_assert!(
                    result.text.ends_with(ELLIPSIS),
                    "was_truncated=true but result does not end with '…'; \
                     text={text:?} width={width_px} result={:?}",
                    result.text,
                );
            }
        }

        /// Invariant: truncated text, when rendered at bounds_width with word-wrap,
        /// has every layout run fitting within bounds_width.  This is the "no clipped
        /// glyphs" contract: no run is wider than the content box.
        ///
        /// Note: the result text may be multi-line (when a word-wrapped paragraph has
        /// a non-final line that overflows horizontally, the preceding whole lines are
        /// preserved and the overflow line is truncated).  In that case
        /// `measure_single_line(result)` can legitimately exceed `bounds_width`, but
        /// every individual layout run must still fit.  We therefore shape the result
        /// at `bounds_width` with `Wrap::Word` and assert each run's `line_w ≤ bounds_width`.
        #[test]
        fn proptest_truncated_result_fits_within_bounds(
            text in "[a-zA-Z0-9 ]{1,80}",
            width_px in 30.0_f32..300.0_f32,
            font_size in 10.0_f32..24.0_f32,
        ) {
            let mut fs = make_font_system();
            let line_h = font_size * 1.4;
            let result = truncate_for_ellipsis(
                &text, base_attrs(), width_px, 200.0, font_size, line_h, &mut fs,
            );
            if result.was_truncated && !result.text.is_empty() {
                // Shape the result at bounds_width with word-wrap — the same
                // geometry used by the renderer — and check every run's width.
                let mut check = Buffer::new(&mut fs, Metrics::new(font_size, line_h));
                check.set_size(&mut fs, Some(width_px), None);
                check.set_wrap(&mut fs, Wrap::Word);
                check.set_text(&mut fs, &result.text, base_attrs(), Shaping::Advanced);
                check.shape_until_scroll(&mut fs, false);
                for run in check.layout_runs() {
                    prop_assert!(
                        run.line_w <= width_px + 2.0, // 2px tolerance for fp rounding
                        "layout run measured {:.2}px > bounds {width_px}px; \
                         text={text:?} result={:?} run_line_i={}",
                        run.line_w, result.text, run.line_i,
                    );
                }
            }
        }

        /// Invariant: whole-line vertical visibility — the result must not produce
        /// more layout runs than the number of lines that fit in bounds_height.
        #[test]
        fn proptest_whole_line_visibility_no_partial_lines(
            n_lines in 2usize..6usize,
            bounds_scale in 1.0_f32..1.5_f32,
        ) {
            let mut fs = make_font_system();
            let font_size = 14.0_f32;
            let line_h = font_size * 1.4;
            // Build text with more lines than n_lines so truncation always applies.
            let text: String = (0..n_lines + 3)
                .map(|i| format!("Line number {i} of test content"))
                .collect::<Vec<_>>()
                .join("\n");
            let height = line_h * (n_lines as f32) * bounds_scale;
            let bounds_w = 400.0_f32;

            let result = truncate_for_ellipsis(
                &text, base_attrs(), bounds_w, height, font_size, line_h, &mut fs,
            );

            // Shape the result and verify the layout run count.
            let mut check_buf = Buffer::new(&mut fs, Metrics::new(font_size, line_h));
            check_buf.set_size(&mut fs, Some(bounds_w), None);
            check_buf.set_wrap(&mut fs, Wrap::Word);
            check_buf.set_text(&mut fs, &result.text, base_attrs(), Shaping::Advanced);
            check_buf.shape_until_scroll(&mut fs, false);
            let run_count = check_buf.layout_runs().count();
            let max_allowed = max_whole_lines(height, line_h);
            prop_assert!(
                run_count <= max_allowed + 1, // +1: trailing ellipsis may add one run
                "result produced {run_count} runs but max_whole_lines({height}, {line_h}) = {max_allowed}; \
                 result={:?}",
                result.text,
            );
        }

        /// Invariant: grapheme fallback — unbroken tokens (no spaces) are truncated
        /// at a grapheme boundary with ellipsis appended, never at mid-codepoint.
        ///
        /// Strengthened: when the box is measurably wide enough to fit at least
        /// one `"A"` cluster + ellipsis, the prefix before `"…"` must be non-empty
        /// (not bare `"…"`).  The threshold is derived by measuring "A" + ELLIPSIS
        /// with the same font system rather than relying on a fixed pixel constant.
        #[test]
        fn proptest_grapheme_fallback_valid_utf8_boundary(
            repeat in 10usize..50usize,
            width_px in 40.0_f32..80.0_f32,
        ) {
            let mut fs = make_font_system();
            let font_size = 16.0_f32;
            let line_h = 22.4_f32;

            // Measure the minimum width needed to show one "A" cluster + ellipsis.
            // If `width_px` is narrower than this, a bare "…" is the correct answer
            // and we must not assert a non-empty prefix.
            let single_a_w = measure_single_line("A", base_attrs(), width_px * 4.0, font_size, line_h, &mut fs);
            let ellipsis_w = measure_single_line(ELLIPSIS, base_attrs(), width_px * 4.0, font_size, line_h, &mut fs);
            let min_width_for_prefix = single_a_w + ellipsis_w;

            // Single unbroken token — no word boundaries.
            let text = "A".repeat(repeat);
            let result = truncate_for_ellipsis(
                &text, base_attrs(), width_px, 200.0, font_size, line_h, &mut fs,
            );
            // Result must be valid UTF-8 and never split at a non-char-boundary.
            prop_assert!(
                std::str::from_utf8(result.text.as_bytes()).is_ok(),
                "result is not valid UTF-8; text={text:?} width={width_px} result={:?}",
                result.text,
            );
            if result.was_truncated {
                prop_assert!(
                    result.text.ends_with(ELLIPSIS),
                    "grapheme fallback result must end with '…'; text={text:?} result={:?}",
                    result.text,
                );
                // Only assert a non-empty prefix when the box is measurably wide
                // enough to fit at least one cluster + ellipsis.  When the box is
                // too narrow even for that, a bare "…" is correct behavior.
                if width_px >= min_width_for_prefix {
                    let prefix = result.text.strip_suffix(ELLIPSIS).unwrap_or("");
                    prop_assert!(
                        !prefix.is_empty(),
                        "unbroken token must have at least one visible cluster before '…' \
                         when width ({width_px}px) >= min_width_for_prefix ({min_width_for_prefix:.1}px); \
                         got bare '…' — grapheme fallback regression; \
                         text={text:?} result={:?}",
                        result.text,
                    );
                }
            }
        }
    }

    // ── Task 3.4: scrolled-back append stability (structural test) ────────────
    //
    // The scrolled-back append stability guarantee is architectural: appends
    // beyond the viewport do not cause layout_runs() to change for already-
    // rendered lines.  This is enforced by the compositor's scene-commit path
    // (not by this function), so we test it structurally: truncating a
    // prefix of text and then the same prefix + suffix produces the same
    // truncated prefix.
    #[test]
    fn append_stability_truncation_prefix_unchanged() {
        let mut fs = make_font_system();
        let font_size = 16.0_f32;
        let line_h = font_size * 1.4;
        let bounds_w = 100.0_f32;
        // Height for exactly 1 line.
        let height = line_h * 1.0 + 1.0;

        let prefix = "Hello world foo bar";
        let suffix = "\nNew line of text appended after scroll";
        let full = format!("{prefix}{suffix}");

        let result_prefix = truncate_for_ellipsis(
            prefix,
            base_attrs(),
            bounds_w,
            height,
            font_size,
            line_h,
            &mut fs,
        );
        let result_full = truncate_for_ellipsis(
            &full,
            base_attrs(),
            bounds_w,
            height,
            font_size,
            line_h,
            &mut fs,
        );

        // Both should produce the same first-line text (within the single-line height).
        assert_eq!(
            result_prefix.text, result_full.text,
            "appending lines beyond the viewport must not change the truncated first-line content"
        );
    }

    // ── RTL / bidi regression tests (hud-u7nyn) ──────────────────────────────
    //
    // These tests guard the fix for "Ellipsis truncation corrupts RTL text:
    // visual-order glyph offsets treated as logical".  cosmic-text outputs
    // glyphs in visual order; for RTL runs glyphs[0] is the logical end, not
    // the logical start.  The old code took glyphs[0].start as run_start,
    // silently skipping the leading bytes of every RTL run.
    //
    // Each test:
    //   1. Verifies the result ends with ELLIPSIS (contract invariant).
    //   2. Asserts the rendered prefix against the actual text — no silent
    //      leading-byte drop, no mid-token split.
    //   3. Verifies the result is valid UTF-8 (no byte-offset misalignment).

    /// Arabic canonical repro from the issue description.
    ///
    /// "السلام عليكم" repeated to guarantee overflow in a 150×25 box must not
    /// silently drop the first letter of the source string (ا, U+0627).  The
    /// truncated prefix, stripped of the trailing "…", must begin with exactly
    /// U+0627 — not merely some Arabic-block character — confirming that
    /// truncation started from the logical beginning of the run, not mid-word.
    ///
    /// Overflow is guaranteed by repetition; the precondition is an `assert!`
    /// rather than an early `return` so this test cannot be silently skipped.
    /// Because font metrics vary across environments the exact truncation point
    /// is not asserted; we verify structural correctness only.
    #[test]
    fn rtl_arabic_truncation_no_leading_byte_drop() {
        let mut fs = make_font_system();
        // Arabic greeting: "السلام عليكم" (Peace be upon you).
        // U+0627 U+0644 U+0633 U+0644 U+0627 U+0645 SPACE
        // U+0639 U+0644 U+064A U+0643 U+0645
        // Repeated to guarantee overflow at any reasonable font metrics.
        let arabic_unit = "السلام عليكم";
        let arabic: String = format!("{arabic_unit} {arabic_unit} {arabic_unit}");
        let bounds_w = 150.0_f32;
        let font_size = 14.0_f32;
        let line_h = font_size * 1.4;

        // Verify overflow is guaranteed. Assert rather than return so the test
        // cannot be silently skipped on unusual font configurations.
        let full_w = measure_single_line(
            &arabic,
            base_attrs(),
            bounds_w * 4.0,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            full_w > bounds_w,
            "repeated Arabic string ({full_w:.1}px) must exceed {bounds_w}px; \
             font metrics are unexpectedly narrow — adjust the repetition count"
        );

        let result = truncate_for_ellipsis(
            &arabic,
            base_attrs(),
            bounds_w,
            25.0, // single-line height
            font_size,
            line_h,
            &mut fs,
        );

        assert!(
            result.was_truncated,
            "Arabic text ({full_w:.1}px) in a {bounds_w}px box must be truncated; got was_truncated=false"
        );
        assert!(
            result.text.ends_with(ELLIPSIS),
            "RTL truncated text must end with '…'; got: {:?}",
            result.text
        );

        // The prefix before the ellipsis must begin with the first Arabic
        // codepoint of the source — U+0627 (ARABIC LETTER ALEF) — exactly.
        // Checking only "some Arabic-block character" is insufficient: the old
        // bug dropped leading bytes and could still produce an Arabic codepoint
        // that is not the logical start (e.g. U+0633 or U+0644).
        let prefix = result
            .text
            .strip_suffix(ELLIPSIS)
            .expect("ends_with ELLIPSIS already asserted");
        assert!(
            !prefix.is_empty(),
            "RTL truncation must produce at least one visible codepoint before '…'; \
             got bare '…'; result: {:?}",
            result.text
        );

        let first_cp = prefix.chars().next().unwrap();
        assert_eq!(
            first_cp, '\u{0627}',
            "RTL prefix must start with U+0627 (ARABIC LETTER ALEF, ا) — the logical \
             first character of the source string; got U+{:04X} ({first_cp:?}); \
             this indicates the old leading-byte-drop bug is still present; \
             prefix: {prefix:?}; result: {:?}",
            first_cp as u32, result.text,
        );
    }

    /// Mixed bidi: LTR prefix + RTL suffix in one string.
    ///
    /// "Hello السلام عليكم" repeated to guarantee overflow in a 120px box.
    /// The LTR word "Hello" is at the logical start and must appear in the
    /// truncated result:
    ///   - the prefix (before "…") must start with 'H',
    ///   - no mid-codepoint split may occur.
    ///
    /// Overflow is guaranteed by repetition; the precondition is an `assert!`
    /// rather than an early `return` so this test cannot be silently skipped.
    /// We intentionally do not assert the exact truncation boundary because
    /// bidi rendering and ellipsis placement on mixed lines is font-dependent.
    #[test]
    fn mixed_bidi_ltr_rtl_no_corruption() {
        let mut fs = make_font_system();
        // Mixed-direction string: Latin greeting + Arabic greeting, repeated.
        let mixed_unit = "Hello السلام عليكم";
        let mixed: String = format!("{mixed_unit} {mixed_unit} {mixed_unit}");
        let bounds_w = 120.0_f32;
        let font_size = 14.0_f32;
        let line_h = font_size * 1.4;

        let full_w = measure_single_line(
            &mixed,
            base_attrs(),
            bounds_w * 4.0,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            full_w > bounds_w,
            "repeated mixed-bidi string ({full_w:.1}px) must exceed {bounds_w}px; \
             adjust the repetition count"
        );

        let result = truncate_for_ellipsis(
            &mixed,
            base_attrs(),
            bounds_w,
            30.0,
            font_size,
            line_h,
            &mut fs,
        );

        assert!(
            result.was_truncated,
            "mixed-bidi text ({full_w:.1}px) in {bounds_w}px must be truncated"
        );
        assert!(
            result.text.ends_with(ELLIPSIS),
            "mixed-bidi truncated text must end with '…'; got: {:?}",
            result.text
        );

        let prefix = result
            .text
            .strip_suffix(ELLIPSIS)
            .expect("ends_with ELLIPSIS asserted");
        assert!(
            !prefix.is_empty(),
            "mixed-bidi truncation must produce at least one visible codepoint before '…'; \
             got bare '…'; result: {:?}",
            result.text
        );

        // The result prefix must begin with 'H' — the LTR content is at the
        // logical start and must not be silently dropped or reordered.
        let first_char = prefix.chars().next().unwrap();
        assert_eq!(
            first_char, 'H',
            "mixed-bidi result prefix must start with 'H' (logical start of the string); \
             got: {first_char:?}; prefix: {prefix:?}; result: {:?}",
            result.text
        );
    }

    /// Pure LTR regression guard: the RTL fix must not break the existing LTR path.
    ///
    /// Verifies that a simple LTR string still produces a truncated prefix
    /// starting with the first character ('A') when repeated to guarantee
    /// overflow.  Overflow is assured by assertion, not by an early `return`,
    /// so this test cannot be silently skipped on any font configuration.
    #[test]
    fn ltr_truncation_unaffected_by_rtl_fix() {
        let mut fs = make_font_system();
        // Repeat the alphabet to guarantee overflow at any reasonable font size.
        let text: String = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(3);
        let bounds_w = 100.0_f32;
        let font_size = 14.0_f32;
        let line_h = font_size * 1.4;

        let full_w = measure_single_line(
            &text,
            base_attrs(),
            bounds_w * 4.0,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            full_w > bounds_w,
            "repeated LTR string ({full_w:.1}px) must exceed {bounds_w}px; \
             adjust the repetition count"
        );

        let result = truncate_for_ellipsis(
            &text,
            base_attrs(),
            bounds_w,
            30.0,
            font_size,
            line_h,
            &mut fs,
        );

        assert!(result.was_truncated, "LTR text must be truncated");
        assert!(
            result.text.ends_with(ELLIPSIS),
            "LTR truncated text must end with '…'; got: {:?}",
            result.text
        );

        let prefix = result
            .text
            .strip_suffix(ELLIPSIS)
            .expect("ends_with ELLIPSIS asserted");
        assert_eq!(
            prefix.chars().next(),
            Some('A'),
            "LTR prefix must start with 'A' (logical start); got: {:?}; result: {:?}",
            prefix,
            result.text
        );
    }

    /// run_logical_start helper — unit test for the core fix.
    ///
    /// Verifies that for a simulated RTL glyph ordering (visual order: highest
    /// start byte first) the helper returns the minimum (logical) start, not the
    /// first element.
    #[test]
    fn run_logical_start_returns_min_not_first() {
        // Simulate RTL visual ordering: glyph[0] has a high byte offset (it is
        // the visual-first / logical-last glyph), glyph[1] has a lower byte offset.
        let rtl_glyphs: Vec<GlyphInfo> = vec![
            (10, 12, 20.0), // visual-first glyph: logical bytes 10..12
            (6, 10, 12.0),  // second glyph: logical bytes 6..10
            (0, 6, 6.0),    // visual-last glyph: logical bytes 0..6
        ];
        assert_eq!(
            run_logical_start(&rtl_glyphs),
            0,
            "run_logical_start must return the minimum start offset (0), not glyphs[0].start (10)"
        );

        // LTR case: glyph[0] already has the smallest start — result is the same.
        let ltr_glyphs: Vec<GlyphInfo> = vec![(0, 4, 6.0), (4, 8, 12.0), (8, 12, 20.0)];
        assert_eq!(
            run_logical_start(&ltr_glyphs),
            0,
            "run_logical_start must return 0 for LTR glyphs[0].start == 0"
        );

        // Empty slice returns 0.
        assert_eq!(
            run_logical_start(&[]),
            0,
            "run_logical_start on empty slice must return 0"
        );
    }

    // ── Task 3.2 — follow-tail whole-line advancement (truncate_tail_anchored) ─

    /// When content has more layout runs than max_lines, `truncate_tail_anchored`
    /// shows the LAST max_lines, not the first.  This is the follow-tail guarantee:
    /// every append produces a result ending with the newest content.
    ///
    /// Spec task 3.2: "follow-tail advances by whole lines".
    #[test]
    fn follow_tail_advances_by_whole_lines() {
        let mut fs = make_font_system();
        let font_size = 14.0_f32;
        let line_h = font_size * 1.4;
        // 5 lines of content, but only space for 3.
        let lines = [
            "Line one",
            "Line two",
            "Line three",
            "Line four",
            "Line five (newest)",
        ];
        let text = lines.join("\n");
        let bounds_w = 400.0_f32;
        // Height for exactly 3 lines.
        let bounds_h = line_h * 3.0 + 1.0;

        let result = truncate_tail_anchored(
            &text,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );

        assert!(
            result.was_truncated,
            "5 lines into 3-line box must be truncated; got: {:?}",
            result.text
        );

        // The newest line must be visible in the result.
        assert!(
            result.text.contains("Line five"),
            "tail-anchored result must contain the newest content ('Line five'); \
             got: {:?}",
            result.text
        );

        // The oldest lines must NOT be visible (they were dropped).
        assert!(
            !result.text.contains("Line one"),
            "tail-anchored result must NOT contain 'Line one' (oldest content was dropped); \
             got: {:?}",
            result.text
        );

        // The result starts with the ELLIPSIS to signal omitted leading content.
        assert!(
            result.text.starts_with(ELLIPSIS),
            "tail-anchored result must start with '…' when leading lines are omitted; \
             got: {:?}",
            result.text
        );

        // ── LINE COUNT ASSERTION (defect #1 regression guard) ────────────────
        //
        // The total number of output lines (after shaping the result) must be
        // <= max_lines (3 in this test: bounds_h = line_h * 3.0 + 1.0).
        //
        // Before the fix, the ellipsis was prepended on top of max_lines content
        // runs, producing max_lines + 1 = 4 layout lines in a 3-line box.
        //
        // We count '\n'-separated logical lines in the result string as an
        // inexpensive proxy.  Word-wrapping can split a single logical line
        // into multiple shaped runs, so the actual rendered line count can
        // exceed this logical count.  Keeping `max_lines` small in this test
        // (3 lines) avoids wrapping in practice; the proptest below covers
        // the general case.
        let max_lines_in_box = (bounds_h / line_h).floor() as usize;
        let logical_line_count = result.text.split('\n').count();
        assert!(
            logical_line_count <= max_lines_in_box,
            "tail-anchored result has {logical_line_count} logical lines but box fits \
             only {max_lines_in_box}; result: {:?}",
            result.text
        );
    }

    /// Degenerate `max_lines == 1` (hud-qoq52, option (b)): a follow-tail box
    /// with room for exactly one line must show the NEWEST line's content with
    /// an inline LEADING ellipsis — never a bare "…" that shows nothing.
    ///
    /// Two sub-cases are exercised:
    ///   1. The newest line fits within bounds_width: result is "…<newest line>"
    ///      (leading ellipsis marks the omitted earlier lines; full newest line
    ///      shown).
    ///   2. The newest line itself overflows bounds_width: result is
    ///      "…<tail of newest line>" — the newest content's TAIL is shown after
    ///      the leading ellipsis, horizontally truncated to fit.
    ///
    /// In both cases the result is exactly one logical line, starts with "…",
    /// and carries visible newest content after the ellipsis.
    #[test]
    fn tail_anchored_max_lines_one_shows_newest_with_leading_ellipsis() {
        let mut fs = make_font_system();
        let font_size = 14.0_f32;
        let line_h = font_size * 1.4;
        // Height for exactly ONE line: line_h + a fractional surplus, < 2*line_h.
        let bounds_h = line_h + 1.0;
        assert_eq!(
            max_whole_lines(bounds_h, line_h),
            1,
            "test precondition: box must fit exactly one line"
        );

        // ── Sub-case 1: short newest line that fits horizontally ─────────────
        let text = "First old line\nSecond old line\nNewest";
        let bounds_w = 400.0_f32; // wide enough for "…Newest"
        let result = truncate_tail_anchored(
            text,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            result.was_truncated,
            "3 lines into a 1-line box must be truncated; got: {:?}",
            result.text
        );
        // Exactly one logical line (no '\n').
        assert_eq!(
            result.text.split('\n').count(),
            1,
            "max_lines==1 result must be a single line; got: {:?}",
            result.text
        );
        // Must NOT be the degenerate bare ellipsis — newest content is the point.
        assert_ne!(
            result.text, ELLIPSIS,
            "max_lines==1 must NOT return a bare '…' (option (a) rejected); \
             the newest content must be shown"
        );
        // Leading omission signal preserved.
        assert!(
            result.text.starts_with(ELLIPSIS),
            "max_lines==1 result must start with the leading '…' omission signal; got: {:?}",
            result.text
        );
        // Newest content visible after the ellipsis.
        assert!(
            result.text.contains("Newest"),
            "max_lines==1 result must contain the newest line's content; got: {:?}",
            result.text
        );
        // Older lines dropped.
        assert!(
            !result.text.contains("First old line"),
            "older content must be dropped; got: {:?}",
            result.text
        );

        // ── Sub-case 2: newest line overflows horizontally ───────────────────
        // A long unbroken newest line forces a horizontal cut; the TAIL of the
        // newest content is shown after the leading ellipsis.
        let long_newest = "WWWWWWWWWWWWWWWWWWWWWWWWWWWWWW"; // 30 'W's
        let text2 = format!("old\nold\n{long_newest}");
        let narrow_w = 80.0_f32;
        // Precondition: the long newest line overflows the narrow box.
        let newest_w = measure_single_line(
            long_newest,
            base_attrs(),
            narrow_w * 4.0,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            newest_w > narrow_w,
            "test precondition: newest line ({newest_w:.1}px) must overflow {narrow_w}px"
        );
        let result2 = truncate_tail_anchored(
            &text2,
            base_attrs(),
            narrow_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(result2.was_truncated, "overflowing case must be truncated");
        assert_eq!(
            result2.text.split('\n').count(),
            1,
            "max_lines==1 overflow result must be a single line; got: {:?}",
            result2.text
        );
        assert!(
            result2.text.starts_with(ELLIPSIS),
            "overflow result must start with leading '…'; got: {:?}",
            result2.text
        );
        // No-partial-glyph guarantee: the result re-measures within bounds_width.
        let measured_w = measure_single_line(
            &result2.text,
            base_attrs(),
            narrow_w * 4.0,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            measured_w <= narrow_w + 1.0, // +1.0 float tolerance
            "max_lines==1 newest-line truncation must re-measure ≤ bounds_width \
             ({narrow_w}px); got {measured_w:.2}px; result: {:?}",
            result2.text
        );
        // Some newest content must be visible after the ellipsis (not bare '…').
        let tail = result2.text.strip_prefix(ELLIPSIS).unwrap_or("");
        assert!(
            tail.chars().any(|c| c == 'W'),
            "max_lines==1 overflow result must show some newest content ('W') \
             after the leading '…'; got: {:?}",
            result2.text
        );
    }

    /// When content fits entirely (fewer runs than max_lines), both anchoring
    /// modes produce identical results — the original text unchanged.
    #[test]
    fn tail_anchored_short_text_fits_unchanged() {
        let mut fs = make_font_system();
        let font_size = 14.0_f32;
        let line_h = font_size * 1.4;
        let text = "Short text\nTwo lines";
        let bounds_w = 400.0_f32;
        // Height for 5 lines — more than enough.
        let bounds_h = line_h * 5.0;

        let head_result = truncate_for_ellipsis(
            text,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );
        let tail_result = truncate_tail_anchored(
            text,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );

        assert_eq!(
            head_result.text, tail_result.text,
            "when content fits, head- and tail-anchored must produce the same result"
        );
        assert!(
            !tail_result.was_truncated,
            "tail-anchored must not set was_truncated when content fits"
        );
    }

    /// Appending more content to an already-overflowing transcript must not
    /// disturb the visible window when the caller is head-anchored (scrolled
    /// back).  This is the structural mirror of the scroll-layer task 3.3 test
    /// in tze_hud_input::scroll.
    ///
    /// Spec task 3.3: "append stability for scrolled-back viewports".
    ///
    /// For the truncation layer: head-anchored truncation of `prefix` must
    /// produce the same result as head-anchored truncation of `prefix + suffix`.
    /// The suffix is new streaming content appended beyond the viewport.
    #[test]
    fn head_anchored_append_stability_matches_prefix_truncation() {
        let mut fs = make_font_system();
        let font_size = 14.0_f32;
        let line_h = font_size * 1.4;
        let bounds_w = 200.0_f32;
        // Height for exactly 2 lines.
        let bounds_h = line_h * 2.0 + 1.0;

        let prefix = "Existing line A\nExisting line B\nExisting line C";
        let suffix = "\nNew line D appended during streaming";
        let full = format!("{prefix}{suffix}");

        let result_prefix = truncate_for_ellipsis(
            prefix,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );
        let result_full = truncate_for_ellipsis(
            &full,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );

        assert_eq!(
            result_prefix.text, result_full.text,
            "head-anchored append stability: visible content must be identical \
             whether or not new lines have been appended beyond the viewport"
        );
    }

    /// Tail-anchored degenerate inputs: empty text stays empty (untruncated);
    /// zero-width bounds yield empty text flagged as truncated.
    #[test]
    fn tail_anchored_degenerate_inputs_produce_empty() {
        let mut fs = make_font_system();
        for (text, width, truncated) in [("", 200.0, false), ("hello", 0.0, true)] {
            let r = truncate_tail_anchored(text, base_attrs(), width, 100.0, 16.0, 22.4, &mut fs);
            assert_eq!(r.text, "", "text={text:?} width={width}");
            assert_eq!(r.was_truncated, truncated, "text={text:?} width={width}");
        }
    }

    // ── Task 3.2: property-based tests for truncate_tail_anchored (hud-347b4) ──
    //
    // These proptests verify the four core invariants of tail-anchored truncation:
    //
    //   1. Tail content always visible — the LAST line(s) of the input always
    //      appear in the output when truncation occurs.
    //   2. Leading ellipsis — when leading lines are omitted, the result starts
    //      with ELLIPSIS (the omission indicator for tail-anchored display).
    //   3. Result is strictly shorter — truncated result is shorter than the
    //      original text plus the ELLIPSIS prefix overhead.
    //   4. Grapheme-cluster integrity — result is valid UTF-8 and no grapheme
    //      cluster is split at the truncation boundary.
    //
    // A fifth proptest exercises RTL/bidi inputs to guard the run_logical_start
    // path (hud-676 semantics) under tail-anchored truncation.
    //
    // Configuration mirrors the existing proptest! blocks above: 32 cases, same
    // source_file annotation.  FontSystem is not Send so each closure owns its
    // own instance.

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 32,
            source_file: Some("crates/tze_hud_compositor/src/overflow.rs"),
            ..proptest::test_runner::Config::default()
        })]

        /// Invariant 1 — Tail content is always visible.
        ///
        /// For any multi-line input that gets truncated, the LAST line of the
        /// original text must appear in the result.  The newest content (tail)
        /// is the whole point of tail-anchored mode.
        #[test]
        fn proptest_tail_anchored_last_line_always_visible(
            n_lines in 3usize..8usize,
            width_px in 200.0_f32..600.0_f32,
        ) {
            let mut fs = make_font_system();
            let font_size = 14.0_f32;
            let line_h = font_size * 1.4;
            // Build `n_lines` lines; the last one is the "newest" content we guard.
            let lines: Vec<String> = (0..n_lines)
                .map(|i| format!("Content line number {i}"))
                .collect();
            let last_line = lines.last().unwrap().clone();
            let text = lines.join("\n");

            // Constrain height so that at most n_lines-1 lines fit — forcing truncation.
            let bounds_h = line_h * ((n_lines - 1) as f32) + 1.0;

            let result = truncate_tail_anchored(
                &text, base_attrs(), width_px, bounds_h, font_size, line_h, &mut fs,
            );

            if result.was_truncated {
                prop_assert!(
                    result.text.contains(&last_line),
                    "tail-anchored: last line must be visible after truncation; \
                     last_line={last_line:?} result={:?}",
                    result.text,
                );
            }
        }

        /// Invariant 2 — Leading ellipsis when leading lines are omitted.
        ///
        /// Whenever `truncate_tail_anchored` sets `was_truncated = true` due to
        /// vertical overflow (more lines than fit), the result must START with
        /// ELLIPSIS — the signal that leading content has been omitted.
        ///
        /// This is the mirror of the head-anchored invariant (result ends with
        /// ELLIPSIS); for tail-anchored the omission indicator is at the front.
        #[test]
        fn proptest_tail_anchored_truncated_starts_with_ellipsis(
            n_lines in 3usize..8usize,
            width_px in 200.0_f32..600.0_f32,
        ) {
            let mut fs = make_font_system();
            let font_size = 14.0_f32;
            let line_h = font_size * 1.4;
            let lines: Vec<String> = (0..n_lines)
                .map(|i| format!("Line {i} of streamed transcript content"))
                .collect();
            let text = lines.join("\n");

            // Force truncation: only n_lines-1 lines fit vertically.
            let bounds_h = line_h * ((n_lines - 1) as f32) + 1.0;

            let result = truncate_tail_anchored(
                &text, base_attrs(), width_px, bounds_h, font_size, line_h, &mut fs,
            );

            if result.was_truncated {
                prop_assert!(
                    result.text.starts_with(ELLIPSIS),
                    "tail-anchored truncated result must START with '…' (leading-ellipsis \
                     omission indicator); was_truncated=true but result does not begin with \
                     ELLIPSIS; n_lines={n_lines} width={width_px} result={:?}",
                    result.text,
                );
            }
        }

        /// Invariant 3 — Result is strictly shorter than original + ellipsis overhead.
        ///
        /// When truncation occurs the result's byte length must be strictly less
        /// than `original.len() + ELLIPSIS.len() + 1` (the "+1" accounts for the
        /// newline separator between the ellipsis line and the visible tail).
        /// If the result were as long or longer the caller would have gained nothing
        /// from truncation.
        #[test]
        fn proptest_tail_anchored_result_shorter_when_truncated(
            n_lines in 3usize..8usize,
            width_px in 200.0_f32..600.0_f32,
        ) {
            let mut fs = make_font_system();
            let font_size = 14.0_f32;
            let line_h = font_size * 1.4;
            let lines: Vec<String> = (0..n_lines)
                .map(|i| format!("Transcript line {i} with some content"))
                .collect();
            let text = lines.join("\n");

            // Force truncation: only n_lines-1 lines fit vertically.
            let bounds_h = line_h * ((n_lines - 1) as f32) + 1.0;

            let result = truncate_tail_anchored(
                &text, base_attrs(), width_px, bounds_h, font_size, line_h, &mut fs,
            );

            if result.was_truncated {
                // The result is the ELLIPSIS header + "\n" + visible tail.
                // Even with the overhead it must be shorter than the original.
                let overhead = ELLIPSIS.len() + 1; // "…\n"
                prop_assert!(
                    result.text.len() < text.len() + overhead,
                    "tail-anchored truncated result must be shorter than original + ellipsis overhead; \
                     result.len()={} original.len()={} overhead={overhead}; \
                     n_lines={n_lines} width={width_px} result={:?}",
                    result.text.len(), text.len(), result.text,
                );
            }
        }

        /// Invariant 3b — Line count fits within max_lines (defect #1 regression guard).
        ///
        /// When truncation occurs the result must have at most `max_lines` logical
        /// lines.  Before the fix, the ellipsis was prepended as an extra line on
        /// top of `max_lines` content runs, yielding `max_lines + 1` lines.  This
        /// property asserts the invariant across a range of inputs and box heights.
        #[test]
        fn proptest_tail_anchored_line_count_fits_within_max_lines(
            n_lines in 3usize..8usize,
            max_lines in 2usize..5usize,
            width_px in 200.0_f32..600.0_f32,
        ) {
            let mut fs = make_font_system();
            let font_size = 14.0_f32;
            let line_h = font_size * 1.4;
            let lines: Vec<String> = (0..n_lines)
                .map(|i| format!("Proptest line {i} for line-count invariant"))
                .collect();
            let text = lines.join("\n");

            // Box height: exactly `max_lines` lines, plus a tiny fractional
            // surplus to avoid height rounding edge cases.
            let bounds_h = line_h * (max_lines as f32) + 0.5;

            let result = truncate_tail_anchored(
                &text, base_attrs(), width_px, bounds_h, font_size, line_h, &mut fs,
            );

            // Logical-line count must never exceed max_lines.
            let logical_line_count = result.text.split('\n').count();
            prop_assert!(
                logical_line_count <= max_lines,
                "tail-anchored result has {logical_line_count} logical lines but \
                 max_lines={max_lines} (bounds_h={bounds_h} line_h={line_h}); \
                 n_lines={n_lines} width={width_px} result={:?}",
                result.text,
            );
        }

        /// Invariant 4 — Grapheme-cluster integrity.
        ///
        /// The result must be valid UTF-8 and every grapheme cluster in the result
        /// must be whole — no grapheme may begin with a combining codepoint
        /// (which would indicate a split mid-cluster).  This mirrors the existing
        /// `proptest_grapheme_fallback_valid_utf8_boundary` for the head-anchored
        /// path and guards the same `truncate_line_to_ellipsis` code path reached
        /// from the tail-anchored entry point.
        ///
        /// Uses multi-codepoint clusters (NFD "e\u{0301}" = 'é') as the stress input
        /// so that any mid-codepoint split would produce a lone combining codepoint
        /// as the first character of a grapheme.
        #[test]
        fn proptest_tail_anchored_grapheme_cluster_integrity(
            repeat in 8usize..30usize,
            width_px in 40.0_f32..120.0_f32,
        ) {
            let mut fs = make_font_system();
            let font_size = 16.0_f32;
            let line_h = 22.4_f32;

            // Build a single long line of NFD "é" clusters (no word boundaries).
            // NFD form: 'e' + combining acute (U+0301), 3 bytes per cluster ('e'=1, U+0301=2).
            let cluster = "e\u{0301}";
            let long_line = cluster.repeat(repeat);

            // Wrap in a multi-line string so vertical truncation is guaranteed.
            // Three copies separated by newlines ensure at least one line is dropped.
            let text = format!("{long_line}\n{long_line}\n{long_line}");
            let bounds_h = line_h * 1.5; // only ~1 line fits

            let result = truncate_tail_anchored(
                &text, base_attrs(), width_px, bounds_h, font_size, line_h, &mut fs,
            );

            // Must always be valid UTF-8.
            prop_assert!(
                std::str::from_utf8(result.text.as_bytes()).is_ok(),
                "tail-anchored result is not valid UTF-8; result={:?}",
                result.text,
            );

            // No grapheme in the result may start with a bare combining codepoint.
            for g in result.text.graphemes(true) {
                let has_lone_combining = g
                    .chars()
                    .next()
                    .map(|c| (c as u32) >= 0x0300 && (c as u32) <= 0x036F)
                    .unwrap_or(false);
                prop_assert!(
                    !has_lone_combining,
                    "grapheme cluster boundary violated in tail-anchored result: \
                     grapheme {g:?} starts with a combining codepoint — cluster was split; \
                     repeat={repeat} width={width_px} result={:?}",
                    result.text,
                );
            }
        }
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 32,
            source_file: Some("crates/tze_hud_compositor/src/overflow.rs"),
            ..proptest::test_runner::Config::default()
        })]

        /// Invariant 5 — RTL/bidi logical-offset correctness (hud-676 semantics).
        ///
        /// `truncate_tail_anchored` uses the same `run_logical_start` helper as
        /// the head-anchored path to resolve the byte start of each layout run.
        /// For RTL runs, cosmic-text orders glyphs visually (highest logical byte
        /// first), so `run_logical_start` must take `min(g.start)` not `glyphs[0].start`.
        ///
        /// This proptest exercises RTL Arabic and mixed-bidi (LTR + Arabic) inputs
        /// to confirm that:
        ///   (a) the result is valid UTF-8 (no byte-boundary misalignment),
        ///   (b) when truncated, the result starts with ELLIPSIS,
        ///   (c) the visible tail contains at least one character from the source text
        ///       (no silent byte-drop that produces garbage).
        ///
        /// Strategy: generate a repetition count to guarantee overflow, then run
        /// with Arabic text and with mixed LTR+Arabic text.
        #[test]
        fn proptest_tail_anchored_rtl_bidi_logical_offset(
            repeat in 2usize..5usize,
        ) {
            let mut fs = make_font_system();
            let font_size = 14.0_f32;
            let line_h = font_size * 1.4;
            let bounds_w = 150.0_f32;

            // Arabic greeting repeated across multiple lines to guarantee both
            // horizontal overflow (on each line) and vertical overflow (>max_lines).
            let arabic_unit = "السلام عليكم";
            let arabic_line = arabic_unit.repeat(repeat);
            // Three lines so vertical truncation forces tail-anchored selection.
            let arabic_text = format!("{arabic_line}\n{arabic_line}\n{arabic_line}");

            // Height for exactly 2 lines — ensures vertical truncation while still
            // leaving room for one ellipsis line + one content line.
            // (With max_lines=1 the box can only show the ellipsis itself; using
            // max_lines=2 exercises the tail-selection path and content visibility.)
            let bounds_h = line_h * 2.5;

            let result = truncate_tail_anchored(
                &arabic_text, base_attrs(), bounds_w, bounds_h, font_size, line_h, &mut fs,
            );

            // (a) Valid UTF-8.
            prop_assert!(
                std::str::from_utf8(result.text.as_bytes()).is_ok(),
                "RTL tail-anchored result is not valid UTF-8; result={:?}",
                result.text,
            );

            if result.was_truncated {
                // (b) Leading ellipsis.
                prop_assert!(
                    result.text.starts_with(ELLIPSIS),
                    "RTL tail-anchored truncated result must start with '…'; \
                     repeat={repeat} result={:?}",
                    result.text,
                );

                // (c) Non-empty visible content after the ellipsis line.
                // Strip the leading "…\n" and verify something remains.
                // (When max_lines >= 2 there is always room for at least 1 content run.)
                let after_ellipsis = result.text
                    .strip_prefix(ELLIPSIS)
                    .map(|s| s.trim_start_matches('\n'))
                    .unwrap_or("");
                prop_assert!(
                    !after_ellipsis.is_empty(),
                    "RTL tail-anchored result must have visible content after the ellipsis line; \
                     got empty tail; repeat={repeat} result={:?}",
                    result.text,
                );

                // (d) Tail contains only valid Arabic or ASCII characters (no garbage bytes).
                for ch in after_ellipsis.chars() {
                    prop_assert!(
                        ch.is_ascii() || ('\u{0600}'..='\u{06FF}').contains(&ch)
                            || ch == ' ' || ch == '\n' || ch == '\u{2026}',
                        "RTL tail-anchored result contains unexpected codepoint U+{:04X} ({ch:?}); \
                         this may indicate a logical-offset corruption (hud-676 regression); \
                         repeat={repeat} result={:?}",
                        ch as u32, result.text,
                    );
                }
            }

            // Also test mixed LTR+RTL input.
            let mixed_unit = "Hello السلام";
            let mixed_line = mixed_unit.repeat(repeat);
            let mixed_text = format!("{mixed_line}\n{mixed_line}\n{mixed_line}");

            let mixed_result = truncate_tail_anchored(
                &mixed_text, base_attrs(), bounds_w, bounds_h, font_size, line_h, &mut fs,
            );

            prop_assert!(
                std::str::from_utf8(mixed_result.text.as_bytes()).is_ok(),
                "mixed-bidi tail-anchored result is not valid UTF-8; result={:?}",
                mixed_result.text,
            );

            if mixed_result.was_truncated {
                prop_assert!(
                    mixed_result.text.starts_with(ELLIPSIS),
                    "mixed-bidi tail-anchored truncated result must start with '…'; \
                     repeat={repeat} result={:?}",
                    mixed_result.text,
                );
            }
        }
    }

    // ── hud-so7zu: multi-line horizontal overflow check ───────────────────────

    /// A long unbreakable token on the FIRST line (not the last) must be
    /// truncated even when all lines fit vertically.
    ///
    /// Before the fix, only the last run's width was checked; a wide token on
    /// any non-final line was silently hard-clipped by TextBounds.
    ///
    /// We build a two-line text where line 1 has a long unbreakable token that
    /// overflows bounds_width, and line 2 fits.  After truncation the result
    /// must end with ELLIPSIS (indicating horizontal truncation was applied).
    #[test]
    fn non_final_line_horizontal_overflow_triggers_truncation() {
        let mut fs = make_font_system();
        let font_size = 16.0_f32;
        let line_h = font_size * 1.4;
        let bounds_w = 80.0_f32;
        // Height fits 3 lines comfortably — no vertical truncation.
        let bounds_h = line_h * 3.0 + 1.0;

        // Line 1: long unbreakable token that must overflow at bounds_w=80px
        let long_token = "WWWWWWWWWWWWWWWWWWWW"; // 20 'W's, should overflow 80px at 16px
        // Verify the precondition: long_token must overflow bounds_w.
        let token_w = measure_single_line(
            long_token,
            base_attrs(),
            bounds_w * 4.0,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            token_w > bounds_w,
            "precondition failed: \"{long_token}\" ({token_w:.1}px) must overflow \
             bounds_w={bounds_w}px at font_size={font_size}px so the \
             non-final-line overflow regression is exercised; check that \
             glyphon metrics are available in this environment"
        );

        // Line 2: short text that fits.
        let text = format!("{long_token}\nshort");

        let result = truncate_for_ellipsis(
            &text,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );

        assert!(
            result.was_truncated,
            "text with an overflowing first line must be truncated; \
             token_w={token_w:.1}px bounds_w={bounds_w}px; result: {:?}",
            result.text
        );
        assert!(
            result.text.ends_with(ELLIPSIS),
            "truncated result must end with '…'; got: {:?}",
            result.text
        );
    }

    /// Same test for tail-anchored truncation: non-final line overflow is caught.
    #[test]
    fn tail_anchored_non_final_line_horizontal_overflow_triggers_truncation() {
        let mut fs = make_font_system();
        let font_size = 16.0_f32;
        let line_h = font_size * 1.4;
        let bounds_w = 80.0_f32;
        let bounds_h = line_h * 3.0 + 1.0;

        let long_token = "WWWWWWWWWWWWWWWWWWWW";
        let token_w = measure_single_line(
            long_token,
            base_attrs(),
            bounds_w * 4.0,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            token_w > bounds_w,
            "precondition failed: \"{long_token}\" ({token_w:.1}px) must overflow \
             bounds_w={bounds_w}px at font_size={font_size}px so the \
             tail-anchored non-final-line overflow regression is exercised; \
             check that glyphon metrics are available in this environment"
        );

        let text = format!("{long_token}\nshort");

        let result = truncate_tail_anchored(
            &text,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );

        assert!(
            result.was_truncated,
            "tail-anchored: text with an overflowing first line must be truncated; \
             token_w={token_w:.1}px bounds_w={bounds_w}px; result: {:?}",
            result.text
        );
        assert!(
            result.text.ends_with(ELLIPSIS),
            "tail-anchored: truncated result must end with '…'; got: {:?}",
            result.text
        );
    }

    /// Multi-line text where ONLY the middle line overflows: truncation applies at
    /// the first overflowing line, not the last.
    #[test]
    fn middle_line_overflow_truncated_at_correct_position() {
        let mut fs = make_font_system();
        let font_size = 16.0_f32;
        let line_h = font_size * 1.4;
        let bounds_w = 80.0_f32;
        // Height fits 4 lines comfortably — no vertical truncation.
        let bounds_h = line_h * 4.0 + 1.0;

        let long_token = "WWWWWWWWWWWWWWWWWWWW";
        let token_w = measure_single_line(
            long_token,
            base_attrs(),
            bounds_w * 4.0,
            font_size,
            line_h,
            &mut fs,
        );
        assert!(
            token_w > bounds_w,
            "precondition failed: \"{long_token}\" ({token_w:.1}px) must overflow \
             bounds_w={bounds_w}px at font_size={font_size}px so the \
             middle-line overflow regression is exercised; check that \
             glyphon metrics are available in this environment"
        );

        // Line 1 and Line 3 are short; Line 2 is the long overflowing token.
        let text = format!("short\n{long_token}\nalso short");

        let result = truncate_for_ellipsis(
            &text,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );

        assert!(
            result.was_truncated,
            "text with an overflowing middle line must be truncated; \
             token_w={token_w:.1}px bounds_w={bounds_w}px; result: {:?}",
            result.text
        );
        assert!(
            result.text.ends_with(ELLIPSIS),
            "truncated result must end with '…'; got: {:?}",
            result.text
        );
        // The first line must appear in the result (it was before the overflow).
        assert!(
            result.text.starts_with("short"),
            "first short line must be preserved before the truncation point; \
             got: {:?}",
            result.text
        );
    }

    // ── Viewport-adjacent-window truncation fallback (spec.md §324/§331) ───────

    /// Build a multi-line transcript of `n_lines` lines, each tagged with its
    /// index so windowing mistakes (dropping the wrong lines) are observable.
    fn transcript(n_lines: usize) -> String {
        let mut s = String::new();
        for i in 0..n_lines {
            // ~40 bytes/line of distinct prose so the whole transcript is many KiB.
            s.push_str(&format!("line {i:04}: the quick brown fox jumps over\n"));
        }
        // Drop the trailing newline so the last line has no empty line after it.
        if s.ends_with('\n') {
            s.pop();
        }
        s
    }

    /// Below the byte bound, the fallback is a no-op: the full input is returned
    /// unchanged so truncation measures the full committed input (spec §331).
    #[test]
    fn fallback_noop_below_bound() {
        let text = transcript(5); // well under 4 KiB
        assert!(text.len() <= DEFAULT_MAX_TRUNCATION_INPUT_BYTES);
        let line_h = 14.0 * 1.4;
        let bounds_h = line_h * 5.0;
        for vp in [
            TruncationViewport::HeadAnchored,
            TruncationViewport::TailAnchored,
        ] {
            let window = viewport_adjacent_input(
                &text,
                bounds_h,
                line_h,
                vp,
                DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
                TRUNCATION_OVERSCAN_LINES,
            );
            assert_eq!(window, text, "small input must not be windowed ({vp:?})");
        }
    }

    /// A single giant line (no '\n') cannot be windowed on a whole-line boundary,
    /// so the fallback returns it unchanged even above the byte bound.
    #[test]
    fn fallback_single_giant_line_unwindowed() {
        let text = "x".repeat(DEFAULT_MAX_TRUNCATION_INPUT_BYTES * 2);
        let line_h = 14.0 * 1.4;
        let window = viewport_adjacent_input(
            &text,
            line_h * 5.0,
            line_h,
            TruncationViewport::HeadAnchored,
            DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
            TRUNCATION_OVERSCAN_LINES,
        );
        assert_eq!(window.len(), text.len(), "single line cannot be windowed");
    }

    /// Above the bound, the window is bounded by viewport geometry (max_lines +
    /// overscan source lines), not by the full transcript size.
    #[test]
    fn fallback_window_is_bounded_by_viewport() {
        let text = transcript(2000); // tens of KiB
        assert!(text.len() > DEFAULT_MAX_TRUNCATION_INPUT_BYTES);
        let line_h = 14.0 * 1.4;
        let max_lines = 5usize;
        let bounds_h = line_h * max_lines as f32;

        let head = viewport_adjacent_input(
            &text,
            bounds_h,
            line_h,
            TruncationViewport::HeadAnchored,
            DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
            TRUNCATION_OVERSCAN_LINES,
        );
        let tail = viewport_adjacent_input(
            &text,
            bounds_h,
            line_h,
            TruncationViewport::TailAnchored,
            DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
            TRUNCATION_OVERSCAN_LINES,
        );

        // Window holds exactly max_lines + overscan whole lines.
        let expect_lines = max_lines + TRUNCATION_OVERSCAN_LINES;
        assert_eq!(head.lines().count(), expect_lines, "head window line count");
        assert_eq!(tail.lines().count(), expect_lines, "tail window line count");
        // Head keeps the FIRST lines; tail keeps the LAST lines.
        assert!(
            head.starts_with("line 0000:"),
            "head keeps first lines: {head:?}"
        );
        assert!(
            tail.trim_end()
                .ends_with("line 1999: the quick brown fox jumps over"),
            "tail keeps last lines: {:?}",
            tail.lines().last()
        );
        // Window is far smaller than the full transcript.
        assert!(head.len() < text.len() / 10, "head window must be bounded");
        assert!(tail.len() < text.len() / 10, "tail window must be bounded");
    }

    /// **Acceptance #2**: for the lines within the viewport-adjacent window, the
    /// truncation result (visible window + ellipsis) is **byte-identical** to the
    /// full-input result, for both anchor modes. This empirically verifies the
    /// per-source-line shaping-independence property the fallback relies on.
    #[test]
    fn fallback_visible_window_is_byte_identical_to_full_input() {
        let text = transcript(2000);
        assert!(text.len() > DEFAULT_MAX_TRUNCATION_INPUT_BYTES);
        let font_size = 14.0_f32;
        let line_h = font_size * 1.4;
        let bounds_w = 400.0_f32;

        // Exercise several viewport heights, including the degenerate single line.
        for max_lines in [1usize, 3, 5, 9] {
            let bounds_h = line_h * max_lines as f32;
            let mut fs = make_font_system();

            // HeadAnchored.
            let full_head = truncate_for_ellipsis(
                &text,
                base_attrs(),
                bounds_w,
                bounds_h,
                font_size,
                line_h,
                &mut fs,
            );
            let win_head_input = viewport_adjacent_input(
                &text,
                bounds_h,
                line_h,
                TruncationViewport::HeadAnchored,
                DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
                TRUNCATION_OVERSCAN_LINES,
            );
            assert!(
                win_head_input.len() < text.len(),
                "max_lines={max_lines}: head fallback must engage"
            );
            let win_head = truncate_for_ellipsis(
                win_head_input,
                base_attrs(),
                bounds_w,
                bounds_h,
                font_size,
                line_h,
                &mut fs,
            );
            assert_eq!(
                win_head.text, full_head.text,
                "max_lines={max_lines}: head-anchored windowed result must be byte-identical"
            );
            assert_eq!(win_head.was_truncated, full_head.was_truncated);

            // TailAnchored.
            let full_tail = truncate_tail_anchored(
                &text,
                base_attrs(),
                bounds_w,
                bounds_h,
                font_size,
                line_h,
                &mut fs,
            );
            let win_tail_input = viewport_adjacent_input(
                &text,
                bounds_h,
                line_h,
                TruncationViewport::TailAnchored,
                DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
                TRUNCATION_OVERSCAN_LINES,
            );
            assert!(
                win_tail_input.len() < text.len(),
                "max_lines={max_lines}: tail fallback must engage"
            );
            let win_tail = truncate_tail_anchored(
                win_tail_input,
                base_attrs(),
                bounds_w,
                bounds_h,
                font_size,
                line_h,
                &mut fs,
            );
            assert_eq!(
                win_tail.text, full_tail.text,
                "max_lines={max_lines}: tail-anchored windowed result must be byte-identical"
            );
            assert_eq!(win_tail.was_truncated, full_tail.was_truncated);
        }
    }

    /// A ~64 KiB transcript truncated through the windowed fallback yields the
    /// same visible result as shaping the whole input. (The warm-path latency
    /// budget is measured by the `overflow_truncate/fallback_64KiB` bench, not here.)
    #[test]
    fn windowed_fallback_matches_full_input_on_large_transcript() {
        let text = transcript(64 * 1024 / 40); // ~64 KiB transcript, far above bound
        assert!(text.len() > 8 * DEFAULT_MAX_TRUNCATION_INPUT_BYTES);
        let font_size = 14.0_f32;
        let line_h = font_size * 1.4;
        let bounds_w = 400.0_f32;
        let bounds_h = line_h * 5.0;
        let mut fs = make_font_system();

        // Full-input truncation (no windowing): shapes the entire transcript.
        let full = truncate_for_ellipsis(
            &text,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );

        // Windowed truncation through the fallback: shapes only the window.
        let input = viewport_adjacent_input(
            &text,
            bounds_h,
            line_h,
            TruncationViewport::HeadAnchored,
            DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
            TRUNCATION_OVERSCAN_LINES,
        );
        let windowed = truncate_for_ellipsis(
            input,
            base_attrs(),
            bounds_w,
            bounds_h,
            font_size,
            line_h,
            &mut fs,
        );

        assert!(windowed.was_truncated, "large transcript must be truncated");
        // Correctness: windowing did not change the visible result.
        assert_eq!(
            windowed.text, full.text,
            "windowed result must stay byte-identical to the full-input result"
        );
    }
}
