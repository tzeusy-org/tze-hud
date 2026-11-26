//! Overflow and ellipsis behavior.
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
//! ## Viewport Anchoring
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
//! ### Viewport behavior
//!
//! **Scenario: append does not disturb a scrolled-back viewport**
//!
//! When the caller holds a fixed head-anchored viewport (scroll offset frozen),
//! it calls [`truncate_for_ellipsis`] with the text visible in that window.
//! Appending new lines beyond the viewport does not change the truncation output
//! because the first `max_lines` are determined solely by the current text, not
//! future appends. See `head_stays_put_while_tail_advances_by_whole_lines`.
//!
//! **Scenario: follow-tail advances by whole lines**
//!
//! When the viewport is at the tail (no scroll-back), the caller uses
//! [`truncate_tail_anchored`].  The function always shows the last `max_lines`
//! of the full text, so every append automatically advances the visible window
//! by the exact number of new lines added — never producing a partially clipped
//! line at the bottom. The same test checks both viewport anchors.
//!
//! # Complexity
//!
//! `truncate_for_ellipsis` still shapes the full text to determine layout.
//! A short-prefix exponential bracket followed by binary search uses
//! O(log(W + G)) candidate measurements under the existing monotone-fit
//! assumption, where W/G count word/grapheme boundaries. Each measurement
//! shapes the entire contextual candidate. This avoids an unconditional
//! longest-prefix probe, but does not bound the initial full-text shape or
//! establish the frame budget. Boundary collection and sorting remain linear
//! storage and O((W + G) log(W + G)) work. Callers retain the existing cache
//! keyed by content, geometry and effective attributes.

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

/// A collected layout run: `(line_i, line_w, minimum_start_glyph)`.
/// The vector is empty or contains the first real glyph attaining min(start);
/// it is a logical-start summary, not the backend glyph inventory.
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

impl TruncationResult {
    /// Characters (Unicode scalar values) of `original` this result draws,
    /// not counting the ellipsis: the length of their common prefix. Every
    /// head-anchored result is a prefix of `original` (trailing whitespace
    /// trimmed) plus [`ELLIPSIS`]; the comparison keeps the count a prefix
    /// count even if that ever changes.
    pub(crate) fn drawn_chars(&self, original: &str) -> usize {
        let drawn = self.text.strip_suffix(ELLIPSIS).unwrap_or(&self.text);
        drawn
            .chars()
            .zip(original.chars())
            .take_while(|(a, b)| a == b)
            .count()
    }
}

#[cfg(test)]
thread_local! {
    /// Layouts [`longest_fitting_prefix`] measured on this thread.
    pub(crate) static PREFIX_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The largest `n` in `0..=upper` for which `fits(n)` holds, assuming
/// `fits(0)` holds and fit is monotone in `n`. Binary search: at most
/// `ceil(log2(upper + 1))` calls, so a cut field of `len` characters
/// (`upper < len`) costs at most `ceil(log2(len)) + 1` layouts.
pub(crate) fn longest_fitting_prefix(upper: usize, mut fits: impl FnMut(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, upper);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        #[cfg(test)]
        PREFIX_PROBES.with(|p| p.set(p.get() + 1));
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// The first `n` characters of `text`.
pub(crate) fn char_prefix(text: &str, n: usize) -> &str {
    text.char_indices()
        .nth(n)
        .map_or(text, |(byte, _)| &text[..byte])
}

/// Viewport anchoring mode for transcript overflow.
///
/// Controls which lines are shown when the full text has more layout runs than
/// `max_lines` allows.  See module-level documentation for the truncation contract.
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
///
/// When a single uncached truncation would shape an input larger than this many
/// bytes, [`viewport_adjacent_input`] restricts the shaped input to a
/// viewport-adjacent window of whole source lines instead of the full retained
/// transcript. The 4096-byte default bounds work toward the Stage-5 Layout
/// Resolve target (< 1 ms);
/// conformance still requires measurement of the actual workload.
pub const DEFAULT_MAX_TRUNCATION_INPUT_BYTES: usize = 4096;

/// Bounded whole-line overscan margin kept beyond the visible layout window when
/// the viewport-adjacent-window fallback engages.
///
/// Must be `>= 1`: the overscan guarantees the windowed input still produces
/// strictly more layout runs than `max_lines`, so the truncation path engages
/// exactly as it did on the full input (otherwise a window of exactly
/// `max_lines` single-layout-line sources would not trigger truncation).
pub const TRUNCATION_OVERSCAN_LINES: usize = 8;

/// Restrict `text` to a viewport-adjacent window of whole source lines when it
/// exceeds `max_input_bytes`, so a single uncached truncation shapes a bounded
/// input instead of the full retained transcript. This is the
/// **viewport-adjacent-window fallback**.
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
/// `max_lines = max(1, floor(bounds_height / line_height))` whole lines are
/// visible.
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
    // the Stage-5 budget and must measure the full committed input.
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
/// This is the entry point for the **follow-tail** scenario:
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
/// ## Degenerate `max_lines == 1`
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
    // ── Degenerate case: max_lines == 1 (DECISION — option (b)) ───
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
    // Overflow requires the omission signal
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
    /// Backend line widths and real minimum-logical-start representatives.
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
    let mut full_buf = Buffer::new_empty(Metrics::new(font_size_px, line_height));
    full_buf.set_size(font_system, Some(bounds_width * 2.0), None);
    full_buf.set_wrap(font_system, Wrap::None);
    let ellipsis_w =
        measure_single_line_in_buffer(ELLIPSIS, base_attrs, font_system, &mut full_buf);

    // ── Step 3: shape the full text with word-wrap enabled ───────────────────
    // We shape with Wrap::Word and bounds_width so that layout_runs() reflects
    // the actual word-wrapped line structure.  Multiple LayoutRuns can share the
    // same line_i when a paragraph soft-wraps.
    // Carry only scratch from this required ellipsis measurement. Clear its
    // lines before the eager setters so they cannot relayout the ellipsis.
    full_buf.lines.clear();
    full_buf.set_scroll(Default::default());
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
            // Retain one real glyph attaining the minimum logical start.
            // Consumers only need that minimum, not a full glyph inventory.
            let glyph_info: Vec<GlyphInfo> = run
                .glyphs
                .iter()
                .min_by_key(|g| g.start)
                .map(|g| (g.start, g.end, g.x + g.w))
                .into_iter()
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
/// over **all** runs (not just the last) is the fix; centralizing it
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
    // and would be clipped by TextBounds without truncation.
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
/// A positive box shorter than one line still gets one line: the
/// line height includes leading, so a single-line label in a box sized to its
/// glyphs (e.g. 20 px text in a 26 px box at the 1.4 multiplier) would
/// otherwise truncate to nothing. The renderer clips that line to the box, as
/// `TextOverflow::Clip` does. A height or line height ≤ 0 returns 0.
#[inline]
fn max_whole_lines(bounds_height: f32, line_height: f32) -> usize {
    if line_height <= 0.0 || bounds_height <= 0.0 {
        return 0;
    }
    ((bounds_height / line_height).floor() as usize).max(1)
}

/// Measure the advance width of `text` when shaped as a single line with the
/// given parameters.  Returns the `line_w` of the first `LayoutRun`.
///
/// This shapes the text in a buffer wide enough that no word-wrapping occurs
/// (width = `max_width`), so the result is the natural single-line width.
#[cfg(test)]
fn measure_single_line<'a>(
    text: &str,
    base_attrs: Attrs<'a>,
    max_width: f32,
    font_size_px: f32,
    line_height: f32,
    font_system: &mut FontSystem,
) -> f32 {
    let mut buf = Buffer::new_empty(Metrics::new(font_size_px, line_height));
    buf.set_size(font_system, Some(max_width), None);
    buf.set_wrap(font_system, Wrap::None);
    measure_single_line_in_buffer(text, base_attrs, font_system, &mut buf)
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

/// Measure a complete contextual candidate with a callback-local buffer.
/// Stock Advanced shaping resets text, attributes, endings and scroll on every
/// call. Width comes from the first visible backend LayoutRun, never summed
/// glyph advances; no state is shared across callbacks or frames.
fn measure_single_line_in_buffer<'a>(
    text: &str,
    base_attrs: Attrs<'a>,
    font_system: &mut FontSystem,
    buf: &mut Buffer,
) -> f32 {
    buf.set_text(font_system, text, base_attrs, Shaping::Advanced);
    buf.shape_until_scroll(font_system, false);
    buf.layout_runs().next().map(|r| r.line_w).unwrap_or(0.0)
}

/// Truncate a single line of text so that the result (with `"…"` appended)
/// fits within `bounds_width`.
///
/// # Algorithm
///
/// Collect a sorted, deduplicated union of word ends and grapheme ends. Under
/// the existing monotone-fit assumption, bracket short fitting prefixes and
/// binary-search the last fitting union boundary. Prefer the last original
/// word end at or before that boundary, then use the grapheme fallback when
/// no word end fits. Contextual Unicode shaping is not universally monotone;
/// unit tests compare concrete vectors against a reference, not every font.
///
/// # RTL / bidi safety
///
/// Every complete logical prefix plus ellipsis is Advanced-shaped together;
/// the fit predicate uses backend line_w, not visual glyph x positions or
/// summed glyph advances.
///
/// # Boundary safety
///
/// Grapheme ends are preserved alongside original word ends. Word ends are
/// not assumed to be grapheme ends. Both species are filtered for actual
/// character boundaries before slicing; search pivots index this union.
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
    let mut measurement_buffer = None;
    let mut measure_with_ellipsis = |boundary: usize| -> f32 {
        let with_ellipsis;
        let text = if boundary == 0 {
            ELLIPSIS
        } else {
            let trimmed = line[..boundary].trim_end();
            // Shape the complete prefix+ellipsis as one contextual unit.
            with_ellipsis = format!("{trimmed}{ELLIPSIS}");
            &with_ellipsis
        };
        let buf = measurement_buffer.get_or_insert_with(|| {
            let mut buf = Buffer::new_empty(Metrics::new(font_size_px, line_height));
            buf.set_size(font_system, Some(bounds_width * 2.0), None);
            buf.set_wrap(font_system, Wrap::None);
            buf
        });
        measure_single_line_in_buffer(text, base_attrs, font_system, buf)
    };

    // Search short prefixes across both boundary species, then preserve the
    // original word preference. A long first word must not force a full-word
    // measurement before we discover a nonfitting short grapheme prefix.
    let word_boundaries: Vec<usize> = line
        .unicode_word_indices()
        .map(|(i, word)| i + word.len())
        .filter(|&b| line.is_char_boundary(b))
        .collect();
    let mut boundaries: Vec<usize> = line
        .grapheme_indices(true)
        .map(|(i, g)| i + g.len())
        .filter(|&b| line.is_char_boundary(b))
        .chain(word_boundaries.iter().copied())
        .collect();
    boundaries.sort_unstable();
    boundaries.dedup();

    if let Some(fitting_boundary) =
        binary_search_largest_fitting(&boundaries, bounds_width, &mut measure_with_ellipsis)
    {
        // With the existing monotone predicate, the fitting union boundary
        // bounds every fitting word end. If none is available, this boundary
        // belongs to the grapheme fallback. The measurement itself remains
        // the complete Advanced-shaped prefix plus ellipsis.
        let boundary = word_boundaries
            .iter()
            .rev()
            .find(|&&boundary| boundary <= fitting_boundary)
            .copied()
            .unwrap_or(fitting_boundary);
        return format!("{}{ELLIPSIS}", line[..boundary].trim_end());
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
    let mut measurement_buffer = None;
    let mut measure_with_leading_ellipsis = |cut: usize| -> f32 {
        let with_ellipsis;
        let text = if cut >= line.len() {
            ELLIPSIS
        } else {
            let suffix = line[cut..].trim_start();
            with_ellipsis = format!("{ELLIPSIS}{suffix}");
            &with_ellipsis
        };
        let buf = measurement_buffer.get_or_insert_with(|| {
            let mut buf = Buffer::new_empty(Metrics::new(font_size_px, line_height));
            buf.set_size(font_system, Some(bounds_width * 2.0), None);
            buf.set_wrap(font_system, Wrap::None);
            buf
        });
        measure_single_line_in_buffer(text, base_attrs, font_system, buf)
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
/// fits, all boundaries `b' < b` also fit. Contextual Advanced shaping does
/// not guarantee monotone prefix widths for arbitrary text. The text callers
/// rely on this precondition; parity tests cover sampled cases
/// rather than proving it universally.
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

    // Quick check: does even the smallest boundary fit?  If not, nothing fits.
    let first = boundaries[0];
    if measure(first) > budget {
        return None;
    }

    let last = boundaries.len() - 1;
    if last == 0 {
        return Some(first);
    }

    // Start near the visible prefix, rather than reshaping the full paragraph
    // before knowing whether even its first few boundaries fill the box.
    let mut lo = 0usize;
    let mut hi = 1usize;
    while measure(boundaries[hi]) <= budget {
        if hi == last {
            return Some(boundaries[hi]);
        }
        lo = hi;
        hi = hi.saturating_mul(2).min(last);
    }

    // Both endpoints have already been measured: lo fits, hi does not.
    // Search only their interior and never measure either endpoint again.
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
    use TruncationViewport::{HeadAnchored, TailAnchored};
    use proptest::prelude::*;

    const FONT: f32 = 16.0;
    const LINE: f32 = 22.4;

    fn fonts() -> FontSystem {
        crate::fonts::bundled_font_system()
    }

    fn truncate(
        anchor: TruncationViewport,
        text: &str,
        w: f32,
        h: f32,
        fonts: &mut FontSystem,
    ) -> TruncationResult {
        let truncate = match anchor {
            HeadAnchored => truncate_for_ellipsis,
            TailAnchored => truncate_tail_anchored,
        };
        truncate(text, Attrs::new(), w, h, FONT, LINE, fonts)
    }

    fn shape(
        text: &str,
        width: f32,
        wrap: Wrap,
        metrics: Metrics,
        fonts: &mut FontSystem,
    ) -> Buffer {
        let mut buffer = Buffer::new(fonts, metrics);
        buffer.set_size(fonts, Some(width), None);
        buffer.set_wrap(fonts, wrap);
        buffer.set_text(fonts, text, Attrs::new(), Shaping::Advanced);
        buffer.shape_until_scroll(fonts, false);
        buffer
    }

    fn unwrapped_width(text: &str, width: f32, fonts: &mut FontSystem) -> f32 {
        measure_single_line(text, Attrs::new(), width, FONT, LINE, fonts)
    }

    #[test]
    fn whole_line_limits_and_invalid_geometry() {
        for (height, line, count) in [
            (60.0, 20.0, 3),
            (65.0, 20.0, 3),
            (0.0, 20.0, 0),
            (100.0, 0.0, 0),
            (10.0, 20.0, 1),
        ] {
            assert_eq!(
                max_whole_lines(height, line),
                count,
                "{height}px / {line}px"
            );
        }

        let mut fs = fonts();
        let transcript_line_height = 14.0 * 1.4;
        for anchor in [
            TruncationViewport::HeadAnchored,
            TruncationViewport::TailAnchored,
        ] {
            let truncate = match anchor {
                TruncationViewport::HeadAnchored => truncate_for_ellipsis,
                TruncationViewport::TailAnchored => truncate_tail_anchored,
            };
            for (text, width, height, font, line) in [
                ("Hi", 1000.0, 100.0, 16.0, 22.4),
                ("", 200.0, 100.0, 16.0, 22.4),
                (
                    "Short text\nTwo lines",
                    400.0,
                    transcript_line_height * 5.0,
                    14.0,
                    transcript_line_height,
                ),
            ] {
                let result = truncate(text, Attrs::new(), width, height, font, line, &mut fs);
                assert_eq!(result.text, text, "{anchor:?}: fitting input");
                assert!(!result.was_truncated, "{anchor:?}: fitting input {text:?}");
            }

            for invalid in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                for (width, height, font, line) in [
                    (invalid, 100.0, 16.0, 22.4),
                    (200.0, invalid, 16.0, 22.4),
                    (200.0, 100.0, invalid, 22.4),
                    (200.0, 100.0, 16.0, invalid),
                ] {
                    for text in ["hello", ""] {
                        let result =
                            truncate(text, Attrs::new(), width, height, font, line, &mut fs);
                        let geometry = (width, height, font, line);
                        assert!(result.text.is_empty(), "{anchor:?} {geometry:?}");
                        assert_eq!(
                            result.was_truncated,
                            !text.is_empty(),
                            "{anchor:?} {geometry:?} {text:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn long_tokens_retain_the_maximal_fitting_grapheme_prefix() {
        let mut fonts = fonts();
        for (input, width, height) in [
            ("x".repeat(4096), 60.0, 100.0),
            ("W".repeat(30), 200.0, 100.0),
            ("e\u{0301}".repeat(40), 80.0, 24.0),
        ] {
            let result = truncate(HeadAnchored, &input, width, height, &mut fonts);
            assert!(result.was_truncated);
            let prefix = result.text.strip_suffix(ELLIPSIS).unwrap();
            assert!(!prefix.is_empty());
            assert!(input.starts_with(prefix));
            let next = input
                .grapheme_indices(true)
                .find(|(at, _)| *at == prefix.len())
                .expect("cut must lie at a source grapheme boundary")
                .1;
            let mut measure = |text: &str| {
                first_run_width(&shape(
                    text,
                    width * 2.0,
                    Wrap::None,
                    Metrics::new(FONT, LINE),
                    &mut fonts,
                ))
            };
            assert!(measure(&result.text) <= width);
            let larger = format!("{prefix}{next}{ELLIPSIS}");
            assert!(measure(&larger) > width);
        }
    }

    #[test]
    fn ellipsis_prefers_a_whole_word_to_more_fitting_graphemes() {
        let mut fonts = fonts();
        let text = format!("alpha {}", "z".repeat(120));
        let width = unwrapped_width("alpha zz…", 800.0, &mut fonts);
        let result = truncate(HeadAnchored, &text, width, LINE + 1.0, &mut fonts);
        assert_eq!(result.text, "alpha…");
        assert!(result.was_truncated);
    }

    fn first_run_width(buffer: &Buffer) -> f32 {
        buffer.layout_runs().next().map_or(0.0, |run| run.line_w)
    }

    // Shape each complete candidate in a fresh Buffer. The head oracle
    // searches word ends and graphemes separately, as before the union search.
    fn last_fitting_boundary(
        boundaries: &[usize],
        fits: &mut impl FnMut(usize) -> bool,
    ) -> Option<usize> {
        if fits(*boundaries.last()?) {
            return boundaries.last().copied();
        }
        if !fits(boundaries[0]) {
            return None;
        }
        let (mut low, mut high) = (0, boundaries.len() - 1);
        while low + 1 < high {
            let mid = low + (high - low) / 2;
            if fits(boundaries[mid]) {
                low = mid;
            } else {
                high = mid;
            }
        }
        Some(boundaries[low])
    }

    fn fresh_head(line: &str, width: f32, fonts: &mut FontSystem) -> String {
        let metrics = Metrics::new(FONT, LINE);
        let candidate = |end: usize| format!("{}{ELLIPSIS}", line[..end].trim_end());
        let mut fits = |end| {
            first_run_width(&shape(
                &candidate(end),
                width * 2.0,
                Wrap::None,
                metrics,
                fonts,
            )) <= width
        };
        let word_ends: Vec<_> = line
            .unicode_word_indices()
            .map(|(start, word)| start + word.len())
            .collect();
        let boundary = last_fitting_boundary(&word_ends, &mut fits).or_else(|| {
            let grapheme_ends: Vec<_> = line
                .grapheme_indices(true)
                .map(|(start, grapheme)| start + grapheme.len())
                .collect();
            last_fitting_boundary(&grapheme_ends, &mut fits)
        });
        boundary.map_or_else(|| ELLIPSIS.to_owned(), candidate)
    }

    fn fresh_tail(line: &str, width: f32, fonts: &mut FontSystem) -> String {
        let metrics = Metrics::new(FONT, LINE);
        let candidate = |start: usize| format!("{ELLIPSIS}{}", line[start..].trim_start());
        let words = line
            .unicode_word_indices()
            .map(|(start, _)| start)
            .filter(|&start| start > 0);
        let graphemes = line.grapheme_indices(true).skip(1).map(|(start, _)| start);
        std::iter::once(0)
            .chain(words)
            .chain(graphemes)
            .map(candidate)
            .find(|text| {
                first_run_width(&shape(text, width * 2.0, Wrap::None, metrics, fonts)) <= width
            })
            .unwrap_or_else(|| ELLIPSIS.to_owned())
    }

    #[test]
    fn ellipsis_search_matches_fresh_word_then_grapheme_measurement() {
        let mut fs = fonts();
        let metrics = Metrics::new(FONT, LINE);
        let long_token = "x".repeat(4096);
        for text in [
            long_token.as_str(),
            "alpha beta gamma delta",
            "alpha   beta\tlongwordlongwordlongword",
            "العربية الطويلة مع نص آخر",
            "LTR العربية ffi e\u{301} 👨‍👩‍👧‍👦 tail",
            "👨‍👩‍👧‍👦👨‍👩‍👧‍👦e\u{301}e\u{301}ffi",
        ] {
            for width in [60.0, 120.0, 400.0] {
                let ellipsis = unwrapped_width(ELLIPSIS, width * 2.0, &mut fs);
                let actual = truncate_line_to_ellipsis(
                    text,
                    Attrs::new(),
                    width,
                    FONT,
                    LINE,
                    ellipsis,
                    &mut fs,
                );
                assert_eq!(
                    actual,
                    fresh_head(text, width, &mut fonts()),
                    "head: {text:?} at {width}px"
                );
                assert!(
                    shape(&actual, width * 2.0, Wrap::None, metrics, &mut fs)
                        .layout_runs()
                        .all(|run| run.line_w <= width),
                    "head: {actual:?} exceeds {width}px",
                );
            }
        }

        let word_before_long_token = format!("alpha {long_token}");
        let width = unwrapped_width("alpha zz…", 800.0, &mut fs);
        let ellipsis = unwrapped_width(ELLIPSIS, 800.0, &mut fs);
        assert!(
            unwrapped_width(
                &format!("{word_before_long_token}{ELLIPSIS}"),
                width * 2.0,
                &mut fs
            ) > width
        );
        assert_eq!(
            truncate_line_to_ellipsis(
                &word_before_long_token,
                Attrs::new(),
                width,
                FONT,
                LINE,
                ellipsis,
                &mut fs
            ),
            "alpha…",
        );

        let width = 120.0;
        let ellipsis = unwrapped_width(ELLIPSIS, width * 2.0, &mut fs);
        for text in [
            "alpha beta gamma delta",
            "العربية الطويلة مع نص آخر",
            "ffi e\u{301} 👨‍👩‍👧‍👦 tail",
        ] {
            let actual = truncate_line_to_leading_ellipsis(
                text,
                Attrs::new(),
                width,
                FONT,
                LINE,
                ellipsis,
                &mut fs,
            );
            assert_eq!(actual, fresh_tail(text, width, &mut fs), "tail: {text:?}");
        }

        let text = "first\nsecond\nthird";
        let width = 400.0;
        let reference = shape(text, width, Wrap::Word, metrics, &mut fs);
        let runs: Vec<_> = reference.layout_runs().collect();
        assert_eq!(runs.len(), 3);
        let first = reference.lines[runs[0].line_i].text();
        let last = reference.lines[runs[2].line_i].text();
        let head = truncate(HeadAnchored, text, width, LINE, &mut fs);
        let tail = truncate(TailAnchored, text, width, LINE, &mut fs);
        assert!(head.was_truncated && tail.was_truncated);
        assert_eq!(head.text, fresh_head(first, width, &mut fs));
        assert_eq!(tail.text, fresh_tail(last, width, &mut fs));
    }

    #[test]
    fn head_truncation_retains_logical_source_order_across_scripts() {
        let mut fonts = fonts();
        for (input, width) in [
            ("ABC fi ".repeat(16), 80.0),
            ("السلام عليكم ".repeat(3), 150.0),
            ("Hello السلام عليكم ".repeat(3), 120.0),
        ] {
            let result = truncate(HeadAnchored, &input, width, LINE + 1.0, &mut fonts);
            assert!(result.was_truncated);
            let prefix = result.text.strip_suffix(ELLIPSIS).unwrap();
            assert!(!prefix.is_empty());
            assert!(
                input.starts_with(prefix),
                "logical beginning lost: {prefix:?} of {input:?}"
            );
        }
    }

    #[test]
    fn first_or_middle_horizontal_overflow_truncates_before_later_lines() {
        let mut fs = fonts();
        let font = 16.0;
        let line = font * 1.4;
        let width = 80.0;
        let wide = "W".repeat(20);
        let token_width =
            measure_single_line(&wide, Attrs::new(), width * 4.0, font, line, &mut fs);
        assert!(
            token_width > width,
            "wide token measures {token_width}px in a {width}px box"
        );

        for anchor in [
            TruncationViewport::HeadAnchored,
            TruncationViewport::TailAnchored,
        ] {
            let truncate = match anchor {
                TruncationViewport::HeadAnchored => truncate_for_ellipsis,
                TruncationViewport::TailAnchored => truncate_tail_anchored,
            };
            for (prefix, later, rows) in [("", "short", 3.0), ("short\n", "also short", 4.0)] {
                // All source lines fit vertically. Only the unbreakable token overflows.
                let text = format!("{prefix}{wide}\n{later}");
                let result = truncate(
                    &text,
                    Attrs::new(),
                    width,
                    line * rows + 1.0,
                    font,
                    line,
                    &mut fs,
                );
                assert!(result.was_truncated, "{anchor:?}: {text:?}");
                assert!(
                    result.text.starts_with(prefix),
                    "{anchor:?}: {:?}",
                    result.text
                );
                assert!(
                    result.text.ends_with(ELLIPSIS),
                    "{anchor:?}: {:?}",
                    result.text
                );
                assert!(
                    !result.text.contains(later),
                    "{anchor:?}: {:?}",
                    result.text
                );
            }
        }
    }

    #[test]
    fn one_line_tail_keeps_recent_text_within_the_box() {
        let mut fonts = fonts();
        let newest = "W".repeat(30);
        for (tail, width) in [("Newest", 400.0), (newest.as_str(), 80.0)] {
            let text = format!("first old line\nsecond old line\n{tail}");
            let result = truncate(TailAnchored, &text, width, LINE + 1.0, &mut fonts);
            assert!(result.was_truncated);
            let visible = result.text.strip_prefix(ELLIPSIS).unwrap();
            assert!(!visible.is_empty());
            assert!(tail.ends_with(visible), "{visible:?}");
            assert!(unwrapped_width(&result.text, width * 2.0, &mut fonts) <= width + 1.0);
            if width == 400.0 {
                assert_eq!(visible, tail);
            }
        }
    }

    #[test]
    fn head_stays_put_while_tail_advances_by_whole_lines() {
        let mut fonts = fonts();
        let metrics = Metrics::new(FONT, LINE);
        for max_lines in [1, 2, 3] {
            let height = LINE * max_lines as f32 + 1.0;
            let before = "line 01\nline 02\nline 03\nline 04";
            let after = format!("{before}\nline 05 latest");
            let head = truncate(HeadAnchored, before, 400.0, height, &mut fonts);
            assert_eq!(
                head,
                truncate(HeadAnchored, &after, 400.0, height, &mut fonts)
            );
            assert!(head.text.starts_with("line 01") && head.text.ends_with(ELLIPSIS));

            let tail = truncate(TailAnchored, &after, 400.0, height, &mut fonts);
            assert!(tail.was_truncated);
            assert!(tail.text.starts_with(ELLIPSIS) && tail.text.ends_with("line 05 latest"));
            assert!(!tail.text.contains("line 01"));
            let shaped = shape(&tail.text, 400.0, Wrap::Word, metrics, &mut fonts);
            assert!(shaped.layout_runs().count() <= max_lines);
        }
    }

    #[test]
    fn wrapped_head_ignores_lines_appended_beyond_viewport() {
        let mut fs = fonts();
        let prefix = "Hello world foo bar";
        let full = format!("{prefix}\nNew line of text appended after scroll");
        let width = 100.0;
        let metrics = Metrics::new(FONT, LINE);
        assert!(
            shape(prefix, width, Wrap::Word, metrics, &mut fs)
                .layout_runs()
                .count()
                > 1
        );
        let before = truncate(HeadAnchored, prefix, width, LINE + 1.0, &mut fs);
        let after = truncate(HeadAnchored, &full, width, LINE + 1.0, &mut fs);
        assert!(before.was_truncated);
        assert_eq!(before, after);
    }

    #[test]
    fn two_row_tail_preserves_visible_wrapped_arabic_and_mixed_content() {
        let mut fs = fonts();
        let font = 14.0;
        let line = font * 1.4;
        let width = 150.0;
        let height = line * 2.5;
        let metrics = Metrics::new(font, line);
        for portion in ["السلام عليكم", "Hello السلام"] {
            let mut wraps = false;
            for count in 2..=4 {
                let row = portion.repeat(count);
                let text = format!("{row}\n{row}\n{row}");
                wraps |= shape(&text, width, Wrap::Word, metrics, &mut fs)
                    .layout_runs()
                    .count()
                    > 3;
                let result =
                    truncate_tail_anchored(&text, Attrs::new(), width, height, font, line, &mut fs);
                assert!(result.was_truncated, "{portion:?} x{count}");
                let visible = result
                    .text
                    .strip_prefix("…\n")
                    .expect("leading ellipsis on its own row");
                assert!(
                    !visible.is_empty(),
                    "{portion:?} x{count}: {:?}",
                    result.text
                );
                assert!(
                    visible.chars().all(|ch| ch.is_ascii()
                        || ('\u{0600}'..='\u{06ff}').contains(&ch)
                        || ch == '…'),
                    "{portion:?} x{count}: {visible:?}",
                );
            }
            assert!(wraps, "at least one {portion:?} fixture must soft wrap");
        }
    }

    fn assert_same_backend_layout(actual: &Buffer, reference: &Buffer) {
        assert_eq!(actual.metrics(), reference.metrics());
        assert_eq!(actual.size(), reference.size());
        assert_eq!(actual.scroll(), reference.scroll());
        assert_eq!(actual.wrap(), reference.wrap());
        assert_eq!(actual.tab_width(), reference.tab_width());
        assert_eq!(actual.monospace_width(), reference.monospace_width());
        assert_eq!(
            format!("{:?}", actual.lines),
            format!("{:?}", reference.lines)
        );
        let ordered_runs = |buffer: &Buffer| {
            buffer
                .layout_runs()
                .map(|run| format!("{run:?}"))
                .collect::<Vec<_>>()
        };
        assert_eq!(ordered_runs(actual), ordered_runs(reference));
    }

    #[test]
    fn shape_summary_and_reused_candidate_buffer_match_fresh_backend_layout() {
        for glyphs in [
            &[(10, 12, 20.0), (6, 10, 12.0), (0, 6, 6.0)][..],
            &[(0, 4, 6.0), (4, 8, 12.0), (8, 12, 20.0)][..],
            &[],
        ] {
            assert_eq!(run_logical_start(glyphs), 0, "{glyphs:?}");
        }

        let mut fs = fonts();
        for (font, width) in [(12.0, 60.0), (16.0, 120.0), (24.0, 400.0)] {
            let line = font * 1.4;
            let height = line * 3.0;
            let metrics = Metrics::new(font, line);
            let ellipsis =
                first_run_width(&shape(ELLIPSIS, width * 2.0, Wrap::None, metrics, &mut fs));
            for text in [
                "",
                "العربية الطويلة مع نص آخر",
                "LTR العربية ffi e\u{301} 👨‍👩‍👧‍👦 tail",
                "several words that soft wrap across lines",
                "first\r\nsecond\nthird\tfourth",
            ] {
                let reference = shape(text, width, Wrap::Word, metrics, &mut fs);
                match shape_runs(text, Attrs::new(), width, height, font, line, &mut fs) {
                    ShapeOutcome::EarlyReturn(result) => {
                        assert_eq!(reference.layout_runs().count(), 0);
                        assert_eq!(result.text, text);
                        assert!(!result.was_truncated);
                    }
                    ShapeOutcome::Proceed(actual) => {
                        assert_eq!(actual.ellipsis_w, ellipsis);
                        assert_eq!(actual.max_lines, (height / line).floor() as usize);
                        assert_same_backend_layout(&actual.full_buf, &reference);
                        assert_eq!(actual.full_buf.tab_width(), 8);
                        assert_eq!(actual.full_buf.monospace_width(), None);
                        assert_eq!(actual.runs.len(), reference.layout_runs().count());
                        for ((index, run_width, summary), run) in
                            actual.runs.iter().zip(reference.layout_runs())
                        {
                            assert_eq!((*index, *run_width), (run.line_i, run.line_w));
                            let minimum = run.glyphs.iter().min_by_key(|glyph| glyph.start);
                            let expected =
                                minimum.map(|glyph| (glyph.start, glyph.end, glyph.x + glyph.w));
                            assert_eq!(
                                run_logical_start(summary),
                                minimum.map_or(0, |glyph| glyph.start)
                            );
                            assert!(
                                summary.iter().copied().eq(expected),
                                "minimum logical glyph differs"
                            );
                        }
                    }
                }
            }

            let mut reused = Buffer::new_empty(metrics);
            reused.set_size(&mut fs, Some(width * 2.0), None);
            reused.set_wrap(&mut fs, Wrap::None);
            for text in [
                "",
                "العربية…",
                "LTR ffi e\u{301} 👨‍👩‍👧‍👦…",
                "one\ttwo…",
                "LTR العربية ffi e\u{301} 👨‍👩‍👧‍👦 tail",
                "one\ttwo",
                "",
                "one\r\ntwo\rthree\n",
            ] {
                let measured =
                    measure_single_line_in_buffer(text, Attrs::new(), &mut fs, &mut reused);
                let reference = shape(text, width * 2.0, Wrap::None, metrics, &mut fs);
                assert_eq!(measured, first_run_width(&reference));
                assert_same_backend_layout(&reused, &reference);
            }
        }
    }

    fn transcript(lines: usize) -> String {
        (0..lines)
            .map(|index| format!("line {index:04}: the quick brown fox jumps over"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn fallback_windows_are_bounded_and_preserve_visible_output() {
        let mut fs = fonts();
        let font = 14.0;
        let line = font * 1.4;
        let width = 400.0;
        let short = transcript(5);
        let giant = "x".repeat(DEFAULT_MAX_TRUNCATION_INPUT_BYTES * 2);
        let long = transcript(2000);
        let replay = transcript(256);
        assert!(short.len() <= DEFAULT_MAX_TRUNCATION_INPUT_BYTES);
        assert!(long.len() > DEFAULT_MAX_TRUNCATION_INPUT_BYTES);
        assert!(replay.len() > DEFAULT_MAX_TRUNCATION_INPUT_BYTES);

        for anchor in [
            TruncationViewport::HeadAnchored,
            TruncationViewport::TailAnchored,
        ] {
            let window = |text, height| {
                viewport_adjacent_input(
                    text,
                    height,
                    line,
                    anchor,
                    DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
                    TRUNCATION_OVERSCAN_LINES,
                )
            };
            let height = line * 5.0;
            assert_eq!(
                window(short.as_str(), height),
                short,
                "{anchor:?}: below byte bound"
            );
            assert_eq!(
                window(giant.as_str(), height),
                giant,
                "{anchor:?}: no whole-line split"
            );

            let bounded = window(long.as_str(), height);
            assert_eq!(
                bounded.lines().count(),
                5 + TRUNCATION_OVERSCAN_LINES,
                "{anchor:?}"
            );
            assert!(
                bounded.len() < long.len() / 10,
                "{anchor:?}: bounded transcript"
            );
            let truncate = match anchor {
                TruncationViewport::HeadAnchored => {
                    assert!(bounded.starts_with("line 0000:"), "{bounded:?}");
                    truncate_for_ellipsis
                }
                TruncationViewport::TailAnchored => {
                    assert!(
                        bounded.ends_with("line 1999: the quick brown fox jumps over"),
                        "{bounded:?}"
                    );
                    truncate_tail_anchored
                }
            };
            for rows in [1, 3, 5, 9] {
                let height = line * rows as f32;
                let adjacent = window(replay.as_str(), height);
                let correct_edge = match anchor {
                    TruncationViewport::HeadAnchored => adjacent.starts_with("line 0000:"),
                    TruncationViewport::TailAnchored => {
                        adjacent.ends_with("line 0255: the quick brown fox jumps over")
                    }
                };
                assert!(correct_edge, "{anchor:?}, {rows} rows: wrong source edge");
                assert!(
                    adjacent.len() < replay.len(),
                    "{anchor:?}, {rows} rows: fallback must engage"
                );
                let full = truncate(&replay, Attrs::new(), width, height, font, line, &mut fs);
                let windowed = truncate(adjacent, Attrs::new(), width, height, font, line, &mut fs);
                assert!(
                    full.was_truncated,
                    "{anchor:?}, {rows} rows: replay must clip"
                );
                assert_eq!(
                    windowed, full,
                    "{anchor:?}, {rows} rows: text and truncation flag"
                );
            }
        }
    }

    #[test]
    fn clipped_character_count_can_be_republished_and_search_is_logarithmic() {
        for upper in 0..=130usize {
            let bound = (usize::BITS - upper.leading_zeros()) as usize;
            for threshold in 0..=upper {
                let mut probes = 0;
                let actual = longest_fitting_prefix(upper, |n| {
                    probes += 1;
                    n <= threshold
                });
                assert_eq!(actual, threshold);
                assert!(probes <= bound, "{upper}: {probes} > {bound}");
            }
        }
        assert_eq!(char_prefix("café 👩‍💻", 4), "café");
        assert_eq!(char_prefix("ab", 5), "ab");
        assert_eq!(char_prefix("ab", 0), "");

        let mut fonts = fonts();
        for (text, width, height) in [
            (
                "The quick brown fox jumps over the lazy dog again and again",
                120.0,
                20.0,
            ),
            (
                "待機 café 👩‍💻 — 待機 café 👩‍💻 — 待機 café 👩‍💻 — 待機",
                90.0,
                20.0,
            ),
            (
                "one two three four five six seven eight nine ten eleven twelve",
                140.0,
                44.0,
            ),
            ("Line one\nline two\nline three\nline four", 200.0, 44.0),
        ] {
            let cut =
                truncate_for_ellipsis(text, Attrs::new(), width, height, FONT, 22.0, &mut fonts);
            assert!(cut.was_truncated);
            let n = cut.drawn_chars(text);
            assert!(n < text.chars().count());
            let prefix = char_prefix(text, n);
            assert_eq!(cut.text, format!("{}{ELLIPSIS}", prefix.trim_end()));
            assert!(
                !truncate_for_ellipsis(prefix, Attrs::new(), width, height, FONT, 22.0, &mut fonts)
                    .was_truncated
            );
        }
        let fits = truncate_for_ellipsis("Hi", Attrs::new(), 200.0, 20.0, 16.0, 22.0, &mut fonts);
        assert_eq!(fits.drawn_chars("Hi"), 2);
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 32,
            source_file: Some("crates/tze_hud_compositor/src/overflow.rs"),
            ..proptest::test_runner::Config::default()
        })]

        #[test]
        fn head_ellipsis_always_fits_the_wrapped_width(
            text in "[a-zA-Z0-9 ]{1,80}",
            width in 30.0_f32..300.0_f32,
            suffix_width in 20.0_f32..200.0_f32,
            font in 10.0_f32..24.0_f32,
        ) {
            let mut fonts = fonts();
            let head = truncate(HeadAnchored, &text, suffix_width, 200.0, &mut fonts);
            if head.was_truncated {
                prop_assert!(head.text.ends_with(ELLIPSIS));
            }

            let line = font * 1.4;
            let result = truncate_for_ellipsis(&text, Attrs::new(), width, 200.0, font, line, &mut fonts);
            if result.was_truncated && !result.text.is_empty() {
                prop_assert!(result.text.ends_with(ELLIPSIS));
                let buffer = shape(&result.text, width, Wrap::Word, Metrics::new(font, line), &mut fonts);
                for run in buffer.layout_runs() {
                    prop_assert!(run.line_w <= width + 2.0, "{:?} at {width}px", result.text);
                }
            }
        }

        #[test]
        fn truncation_respects_whole_line_budget(
            input_rows in 3usize..8usize,
            visible_rows in 1usize..8usize,
            width in 200.0_f32..600.0_f32,
            surplus in 0.0_f32..0.8_f32,
        ) {
            let mut fonts = fonts();
            let font = 14.0;
            let line = font * 1.4;
            let lines: Vec<_> = (0..input_rows).map(|i| format!("Content line number {i}")).collect();
            let text = lines.join("\n");
            let height = line * (visible_rows as f32 + surplus) + 0.01;
            let metrics = Metrics::new(font, line);
            let source = shape(&text, width, Wrap::Word, metrics, &mut fonts);
            prop_assert_eq!(source.layout_runs().count(), input_rows);
            for anchor in [HeadAnchored, TailAnchored] {
                let truncate = match anchor {
                    HeadAnchored => truncate_for_ellipsis,
                    TailAnchored => truncate_tail_anchored,
                };
                let result = truncate(&text, Attrs::new(), width, height, font, line, &mut fonts);
                let layout = shape(&result.text, width, Wrap::Word, metrics, &mut fonts);
                prop_assert!(layout.layout_runs().count() <= visible_rows);
                prop_assert_eq!(result.was_truncated, input_rows > visible_rows);
                if result.was_truncated {
                    prop_assert!(result.text.len() < text.len() + ELLIPSIS.len() + 1);
                    let (start, end) = match anchor {
                        HeadAnchored => (lines[0].as_str(), ELLIPSIS),
                        TailAnchored => (ELLIPSIS, lines[input_rows - 1].as_str()),
                    };
                    prop_assert!(result.text.starts_with(start) && result.text.ends_with(end));
                } else {
                    prop_assert_eq!(&result.text, &text);
                }
            }
        }

        #[test]
        fn narrow_unbroken_tokens_cut_on_grapheme_boundaries(
            head_repeat in 10usize..50usize,
            tail_repeat in 8usize..30usize,
            head_width in 40.0_f32..80.0_f32,
            tail_width in 40.0_f32..120.0_f32,
        ) {
            let mut fonts = fonts();
            let metrics = Metrics::new(FONT, LINE);
            let token = "A".repeat(head_repeat);
            let result = truncate(HeadAnchored, &token, head_width, 200.0, &mut fonts);
            if result.was_truncated {
                let retained = result.text.strip_suffix(ELLIPSIS).expect("trailing ellipsis");
                let minimum = shape("A…", head_width * 2.0, Wrap::None, metrics, &mut fonts);
                prop_assert!(first_run_width(&minimum) > head_width || !retained.is_empty());
                prop_assert!(token.starts_with(retained));
            }

            let grapheme = "e\u{301}";
            let row = grapheme.repeat(tail_repeat);
            let text = format!("{row}\n{row}\n{row}");
            let result = truncate(TailAnchored, &text, tail_width, LINE * 1.5, &mut fonts);
            prop_assert!(result.was_truncated);
            let retained = result.text.strip_prefix(ELLIPSIS).expect("leading ellipsis");
            let minimum = shape("…e\u{301}", tail_width * 2.0, Wrap::None, metrics, &mut fonts);
            prop_assert!(first_run_width(&minimum) > tail_width || !retained.is_empty());
            prop_assert!(row.ends_with(retained));
            prop_assert_eq!(retained.len() % grapheme.len(), 0);
        }
    }

    #[test]
    fn tail_keeps_whole_graphemes_and_logical_newest_content() {
        // Exhaust the finite domain instead of sampling the same repeats again.
        for repeat in 3usize..11usize {
            let mut fonts = fonts();
            for portion in ["السلام عليكم", "Hello السلام", "👨‍👩‍👧‍👦e\u{301}"]
            {
                let case = (repeat, portion);
                let last = portion.repeat(repeat);
                let text = format!("old line\nolder line\n{last}");
                let result = truncate(TailAnchored, &text, 150.0, LINE + 1.0, &mut fonts);
                assert!(result.was_truncated, "case={case:?}");
                let retained = result.text.strip_prefix(ELLIPSIS).expect("ellipsis");
                assert!(!retained.is_empty(), "case={case:?}");
                assert!(last.ends_with(retained), "{retained:?} of {last:?}");
                let offset = last.len() - retained.len();
                let whole_grapheme = last
                    .grapheme_indices(true)
                    .any(|(start, _)| start == offset);
                assert!(whole_grapheme, "{case:?}: {retained:?}");
            }
        }
    }
}
