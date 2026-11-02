//! Runtime-owned bounded plain-text draft buffer for portal composer regions.
//!
//! ## Editing primitives
//!
//! - Character insert at cursor (from CharacterEvent).
//! - Caret movement: character, word, line-start/end.
//! - Selection: keyboard (shift + movement) and pointer.
//! - Backspace / Delete: character-wise and word-wise (Ctrl/Alt + Backspace,
//!   Ctrl/Alt + Delete).
//! - Paste with UTF-8-boundary truncation at the cap; at-capacity feedback.
//!
//! ## Excluded operations (normative §4.4)
//!
//! - No IME composition (v1-reserved).
//! - No undo/redo.
//! - No rich text.
//! - No multi-caret.
//! - Editing keystrokes are never terminal input.
//!
//! ## Governance (§4.5)
//!
//! Under safe mode the draft suspends: `insert`, `paste`, and submit are
//! rejected while `suspended == true`. Caret and selection queries still work
//! (read-only) but mutations are no-ops. Draft content is subject to portal
//! redaction policy (tracked by caller via `suspended`).
//!
//! # Size cap
//!
//! `ComposerDraft::new(cap)` enforces `cap ≤ MAX_DRAFT_BYTES` (65535). Paste
//! and insert that would exceed the cap are truncated at a grapheme-cluster
//! boundary; `EditOutcome::AtCapacity` is returned and no notification leaves
//! the runtime with content exceeding the cap.

use tze_hud_scene::SceneId;
use unicode_segmentation::UnicodeSegmentation;

/// Hard ceiling matching the TextMarkdownNode content limit (spec §4.3, §4.5).
pub const MAX_DRAFT_BYTES: usize = 65_535;

/// Default lower draft cap (suitable for typical composer inputs).
pub const DEFAULT_DRAFT_CAP: usize = 4_096;

// ─── Outcome types ────────────────────────────────────────────────────────────

/// Outcome of a mutating draft operation.
///
/// `Mutated` — draft content or caret changed; re-render locally.
///
/// `Unchanged` — operation was a no-op (e.g. backspace at offset 0, move at
/// boundary). No local render needed.
///
/// `AtCapacity` — a paste or insert was truncated at the byte cap. Draft
/// contains the truncated prefix. Caller MUST render at-capacity feedback.
///
/// `Suspended` — the draft is suspended under safe mode; the operation was
/// rejected without side effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditOutcome {
    /// Draft content or caret moved; re-render locally.
    Mutated,
    /// No change; no notification needed.
    Unchanged,
    /// Paste/insert exceeded cap; content truncated to cap boundary.
    /// Render at-capacity feedback.
    AtCapacity,
    /// Draft suspended under safe mode; operation rejected.
    Suspended,
}

/// High-level result of composer key routing or a terminal draft action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposerOutcome {
    Edited,
    Submitted(String),
    Cancelled,
    Unchanged,
}

impl From<EditOutcome> for ComposerOutcome {
    fn from(outcome: EditOutcome) -> Self {
        match outcome {
            EditOutcome::Mutated | EditOutcome::AtCapacity => Self::Edited,
            EditOutcome::Unchanged | EditOutcome::Suspended => Self::Unchanged,
        }
    }
}

// ─── Selection ────────────────────────────────────────────────────────────────

/// An active selection range `[start, end)` over byte offsets in the draft.
/// Both offsets are valid UTF-8 boundaries; `start <= end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub start: usize,
    pub end: usize,
}

impl Selection {
    /// True when the selection is empty (caret position only).
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Cursor position when there is no selection (normalized anchor offset).
    pub fn cursor(&self) -> usize {
        self.start
    }
}

// ─── Visual (wrapped) line layout (hud-21o6x) ───────────────────────────────

/// One visual (wrapped) line of the composer draft, in RAW-draft byte space.
///
/// Produced by the compositor (`measure_composer_visual_layout`) each frame and
/// consumed by the input layer for vertical caret movement across soft-wrapped
/// lines. A visual line is a single on-screen row: soft word-wrap splits one
/// logical (`\n`-delimited) line into several of these, and hard newlines start
/// new ones — so consecutive `ComposerVisualLine`s are the exact visual rows.
#[derive(Clone, Debug, PartialEq)]
pub struct ComposerVisualLine {
    /// Inclusive raw-text byte offset where this visual row begins.
    pub start_byte: usize,
    /// Exclusive raw-text byte offset where this visual row's content ends.
    pub end_byte: usize,
    /// `(byte_offset, x_px)` at each glyph boundary on this row, ascending by
    /// byte, with a trailing `(end_byte, line_width_px)` sentinel. Maps caret byte
    /// ↔ pixel x for pixel-precise goal-x tracking.
    pub glyph_x: Vec<(usize, f32)>,
}

/// Vertical geometry of the RENDERED composer input box, in composer-node-local
/// pixel space (y measured from the composer HitRegion's top edge — the same
/// origin `ComposerVisualLayout::byte_at_point` receives its `y` in).
///
/// Published by the compositor (`Compositor::prime_composer_scroll_offset`) from
/// its own `composer_input_box` geometry so the input layer never re-derives the
/// box — the compositor is the single source of the box placement, and the two
/// cannot drift (craft-and-care engineering-bar §9, single geometry authority).
///
/// # Why the input layer needs this (hud-lw60x)
///
/// For a full-tile-HitRegion PROJECTION composer the HitRegion spans the WHOLE
/// portal (click-anywhere-to-focus, hud-v4k1h) but the draft renders in a
/// short, BOTTOM-anchored input box (`box_y = region.y + region.height −
/// box_height`, hud-2zsbf / hud-nx7yq.1). Splitting the FULL node height evenly
/// across rows (the pre-hud-lw60x fallback below) therefore maps a click on the
/// visible text strip to `frac_y ≈ 1.0` → always the last row, leaving the upper
/// visible rows unreachable. Mapping the click through THIS box instead makes
/// every visible row hittable.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ComposerInputBoxGeometry {
    /// Node-local y (px) of the rendered input box's top edge.
    pub box_top: f32,
    /// Rendered input box height (px). The visible text strip is
    /// `[box_top, box_top + box_height]`.
    pub box_height: f32,
    /// Node-local y (px) of VISUAL ROW 0's top edge, already shifted UP by the
    /// vertical scroll offset (`box_top + content_inset − vscroll_px`). Because
    /// rows are a uniform `line_height` tall, row `i`'s top is
    /// `row0_top + i * line_height`; folding `vscroll_px` in here keeps the row
    /// mapping exact even while the draft is scrolled (the vscroll drift the
    /// even-split path below could not account for).
    pub row0_top: f32,
    /// Per-visual-row height (px). Uniform across rows.
    pub line_height: f32,
    /// Absolute index of the FIRST visual row inside the visible box (the
    /// vertical scroll offset in rows, `vscroll_px / line_height`). Zero when the
    /// draft has not scrolled.
    pub first_visible_row: usize,
    /// Count of visual rows the box actually shows. Together with
    /// `first_visible_row` this bounds the pointer hit-test to the VISIBLE window
    /// `[first_visible_row, first_visible_row + visible_rows − 1]`, so a click in
    /// the box's own top/bottom padding while scrolled resolves to the nearest
    /// visible row instead of a clipped row above/below the window (hud-lw60x).
    pub visible_rows: usize,
}

/// The composer draft's wrapped layout: the ordered visual rows plus the draft
/// length it was measured for (a staleness guard, hud-21o6x).
///
/// Local presentation state published from the compositor render thread to the
/// input thread via a shared slot (mirrors the forward `LocalComposerState`
/// channel). One-frame staleness is imperceptible for caret movement; when the
/// draft changed since measurement (`text_len` mismatch) the input layer falls
/// back to hard-newline vertical movement for that keystroke.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComposerVisualLayout {
    /// Visual rows, top to bottom.
    pub lines: Vec<ComposerVisualLine>,
    /// Raw-draft byte length this layout was measured for.
    pub text_len: usize,
    /// Rendered input-box vertical geometry, when the compositor published it
    /// (the multi-line profile always does; `None` on the single-line profile
    /// and until the first post-focus frame). Drives `byte_at_point`'s pointer-Y
    /// → visual-row mapping (hud-lw60x); when `None`, `byte_at_point` falls back
    /// to splitting the passed content height evenly across the rows.
    pub input_box: Option<ComposerInputBoxGeometry>,
    /// Horizontal caret-follow scroll (px) applied when this layout was
    /// rendered — single-line profile only; `0.0` for the multi-line profile
    /// (which wraps instead of scrolling horizontally, hud-hxhnt finding 3).
    ///
    /// `glyph_x` / `x_at_cursor` are measured in UNSCROLLED draft space (the
    /// natural single-line width, `Wrap::None`); consumers that need the
    /// caret's actual on-screen x — e.g. the OS IME candidate-window anchor
    /// (`composer_ime_caret_anchor`) — must subtract this to land in rendered
    /// screen space, mirroring the chrome-layer caret quad's own
    /// `caret_x - h_scroll_px` (`append_composer_caret_vertices`). The
    /// pointer hit-test (`byte_at_point`/`byte_at_x`) intentionally does NOT
    /// apply this yet — that is the tracked, separate hud-uui70 follow-up.
    pub h_scroll_px: f32,
}

impl ComposerVisualLine {
    /// Caret x (px) at raw byte `byte` on this row: the x of the first glyph
    /// boundary at or after `byte` (the caret sits at that glyph's left edge);
    /// the trailing line-width sentinel covers the row end.
    pub fn x_at_byte(&self, byte: usize) -> f32 {
        for &(b, x) in &self.glyph_x {
            if b >= byte {
                return x;
            }
        }
        self.glyph_x.last().map(|&(_, x)| x).unwrap_or(0.0)
    }

    /// Raw byte whose glyph boundary is nearest `goal_x` (px), clamped to this
    /// row's `[start_byte, end_byte]`. Lands a vertical move near the goal column.
    pub fn byte_at_x(&self, goal_x: f32) -> usize {
        let mut best_byte = self.start_byte;
        let mut best_dist = f32::INFINITY;
        for &(b, x) in &self.glyph_x {
            let d = (x - goal_x).abs();
            if d < best_dist {
                best_dist = d;
                best_byte = b;
            }
        }
        best_byte.clamp(self.start_byte, self.end_byte)
    }
}

impl ComposerVisualLayout {
    /// Index of the visual row containing raw byte `cursor`. At a soft-wrap
    /// boundary (`cursor == end_of_row_i == start_of_row_{i+1}`) the caret has
    /// affinity to the LATER row (standard editor behavior: it renders at the
    /// next row's start). Returns `None` when there are no rows.
    pub fn line_of(&self, cursor: usize) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        for (i, line) in self.lines.iter().enumerate() {
            if cursor >= line.start_byte && cursor < line.end_byte {
                return Some(i);
            }
        }
        // cursor at (or past) the last row's end.
        Some(self.lines.len() - 1)
    }

    /// Caret x (px) at raw byte `cursor` on its visual row.
    pub fn x_at_cursor(&self, cursor: usize) -> f32 {
        match self.line_of(cursor) {
            Some(i) => self.lines[i].x_at_byte(cursor),
            None => 0.0,
        }
    }

    /// Raw byte the caret should move to for one visual-row vertical step at
    /// `goal_x` (px). `up` selects the previous row; otherwise the next. On the
    /// first row an upward step goes to byte 0; on the last row a downward step
    /// goes to `text_len`. Returns `None` when already at that boundary.
    pub fn vertical_target(&self, cursor: usize, up: bool, goal_x: f32) -> Option<usize> {
        let cur = self.line_of(cursor)?;
        if up {
            if cur == 0 {
                return if cursor == 0 { None } else { Some(0) };
            }
            Some(self.lines[cur - 1].byte_at_x(goal_x))
        } else {
            if cur + 1 >= self.lines.len() {
                return if cursor >= self.text_len {
                    None
                } else {
                    Some(self.text_len)
                };
            }
            Some(self.lines[cur + 1].byte_at_x(goal_x))
        }
    }

    /// Raw byte nearest the point `(x, y)` in composer-NODE-local pixel space —
    /// `y` measured from the composer HitRegion's top edge, `x` from the content
    /// box's left after the caller has de-inset the horizontal margin (hud-etrs0
    /// pointer hit-test).
    ///
    /// Row selection: when the compositor published the rendered input-box
    /// geometry ([`Self::input_box`], the multi-line PROJECTION path, hud-lw60x),
    /// `y` is clamped into the visible box and mapped to a visual row through
    /// `row0_top`/`line_height`, then clamped into the visible row window — so a
    /// click on the short, bottom-anchored text strip of a TALL projection portal
    /// lands on the row actually under the pointer, `row0_top`'s baked-in
    /// `vscroll_px` keeps that exact while the draft is scrolled, and a click in
    /// the box's own padding resolves to the nearest visible row rather than a
    /// clipped one outside the window.
    ///
    /// Fallback (no published geometry — the single-line profile never publishes
    /// one, and a multi-line composer may not yet on the first post-focus frame):
    /// split `content_height` evenly across `self.lines`. That even split is
    /// exact only while the box height equals the node height and nothing has
    /// scrolled; it is the pre-hud-lw60x behavior, retained for the
    /// no-geometry case.
    ///
    /// Either way, `x` maps to a byte via the selected row's real glyph geometry
    /// ([`ComposerVisualLine::byte_at_x`]). Returns byte `0` when there are no
    /// rows.
    pub fn byte_at_point(&self, x: f32, y: f32, content_height: f32) -> usize {
        let Some(last) = self.lines.len().checked_sub(1) else {
            return 0;
        };
        let row = match self.input_box {
            // Map the pointer Y through the RENDERED input box (hud-lw60x). Clamp
            // into the visible strip first, then locate the row band. `row0_top`
            // already folds in the vertical scroll, so a click while scrolled
            // lands on the correct absolute row. Finally clamp the row into the
            // VISIBLE window `[first_visible_row, last_visible]`: the box's own
            // top/bottom padding (content inset) sits outside the text rows, so
            // without this a padding click while scrolled would resolve to a
            // clipped row just above/below the window and yank the caret there.
            Some(g) if g.line_height > 0.0 => {
                let y_in_box = y.clamp(g.box_top, g.box_top + g.box_height);
                let rel = y_in_box - g.row0_top;
                let idx = (rel / g.line_height).floor().max(0.0) as usize;
                let last_visible = g
                    .first_visible_row
                    .saturating_add(g.visible_rows.saturating_sub(1))
                    .min(last);
                idx.clamp(g.first_visible_row.min(last_visible), last_visible)
            }
            // No published geometry: even split across the passed content height.
            _ => {
                if content_height > 0.0 {
                    let frac = (y / content_height).clamp(0.0, 1.0);
                    ((frac * self.lines.len() as f32) as usize).min(last)
                } else {
                    0
                }
            }
        };
        self.lines[row].byte_at_x(x)
    }
}

// ─── ComposerDraft ────────────────────────────────────────────────────────────

/// Runtime-owned bounded plain-text draft buffer.
///
/// One `ComposerDraft` per focused composer region. The runtime creates it on
/// focus-in and destroys it on focus-out (or on submit/cancel).
///
/// Editing methods retain capacity and suspension detail in [`EditOutcome`].
/// Submit and cancel return a direct [`ComposerOutcome`].
///
/// # Invariants
///
/// - `text.len() <= cap <= MAX_DRAFT_BYTES`
/// - `cursor` and `selection_anchor` are valid byte offsets into `text`
/// - `cursor <= text.len()` and `selection_anchor <= text.len()`
///
#[derive(Clone, Debug)]
pub struct ComposerDraft {
    text: String,
    /// Cursor position in bytes (insert/caret position).
    cursor: usize,
    /// Selection anchor in bytes. Equal to `cursor` when no selection.
    selection_anchor: usize,
    /// Byte cap (≤ MAX_DRAFT_BYTES).
    cap: usize,
    /// True when the draft is suspended under safe mode (§4.5).
    suspended: bool,
}

impl ComposerDraft {
    /// Create a new draft buffer with the given byte `cap`.
    ///
    /// `cap` is clamped to `[1, MAX_DRAFT_BYTES]`.
    pub fn new(cap: usize) -> Self {
        let cap = cap.clamp(1, MAX_DRAFT_BYTES);
        Self {
            text: String::new(),
            cursor: 0,
            selection_anchor: 0,
            cap,
            suspended: false,
        }
    }

    // ── Read-only accessors ───────────────────────────────────────────────

    /// Current draft text. Never exceeds the byte cap.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Cursor byte offset.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Selection anchor byte offset (equal to cursor when no selection active).
    pub fn selection_anchor(&self) -> usize {
        self.selection_anchor
    }

    /// Active selection `[start, end)`, or a zero-width cursor position.
    pub fn selection(&self) -> Selection {
        let (start, end) = if self.selection_anchor <= self.cursor {
            (self.selection_anchor, self.cursor)
        } else {
            (self.cursor, self.selection_anchor)
        };
        Selection { start, end }
    }

    /// True when an active selection is present (start != end).
    pub fn has_selection(&self) -> bool {
        self.cursor != self.selection_anchor
    }

    /// Whether the draft is at or exceeds the byte cap.
    pub fn is_at_capacity(&self) -> bool {
        self.text.len() >= self.cap
    }

    /// Configured byte cap.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Whether the draft is suspended under safe mode.
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    // ── Governance ────────────────────────────────────────────────────────

    /// Suspend the draft under safe mode (§4.5).
    ///
    /// While suspended, mutating operations return `EditOutcome::Suspended`.
    /// The draft text is preserved but not modified. Submission is also
    /// rejected.
    pub fn set_suspended(&mut self, suspended: bool) {
        self.suspended = suspended;
    }

    // ── Editing operations ────────────────────────────────────────────────

    /// Insert `character` at the cursor position (from a CharacterEvent).
    ///
    /// If a selection is active, the selected range is first deleted, then the
    /// character is inserted. Control characters and `\r`/`\n` are rejected as
    /// no-ops (newlines end editing via submit).
    ///
    /// Returns `AtCapacity` if the inserted text was truncated at the byte cap.
    pub fn insert(&mut self, character: &str) -> EditOutcome {
        if self.suspended {
            return EditOutcome::Suspended;
        }
        // Reject control characters and line endings (spec §4.4: no newlines in draft)
        if character
            .chars()
            .any(|c| c == '\r' || c == '\n' || c.is_control())
        {
            return EditOutcome::Unchanged;
        }
        // Delete selection first if active
        if self.has_selection() {
            let sel = self.selection();
            self.text.replace_range(sel.start..sel.end, "");
            self.cursor = sel.start;
            self.selection_anchor = sel.start;
        }
        let remaining = self.cap.saturating_sub(self.text.len());
        if remaining == 0 {
            return EditOutcome::AtCapacity;
        }
        // Truncate to remaining byte capacity at UTF-8 boundary
        let truncated = truncate_at_utf8_boundary(character, remaining);
        let at_cap = truncated.len() < character.len();
        self.text.insert_str(self.cursor, truncated);
        self.cursor += truncated.len();
        self.selection_anchor = self.cursor;
        if at_cap {
            EditOutcome::AtCapacity
        } else {
            EditOutcome::Mutated
        }
    }

    /// Insert a newline (`\n`) at the cursor as a LOCAL draft edit — the
    /// Ctrl+Enter / Shift+Enter path for multi-line composer drafts (hud-nx7yq.2).
    ///
    /// Unlike [`Self::insert`], which rejects line endings (Enter alone submits),
    /// this is the explicit multi-line-newline path. If a selection is active it
    /// is deleted first, then a single `\n` is inserted, honoring the byte cap
    /// (`\n` is one byte). Newline is subject to the same suspend/cap governance
    /// as any other edit; the submitted draft carries embedded newlines verbatim.
    pub fn insert_newline(&mut self) -> EditOutcome {
        if self.suspended {
            return EditOutcome::Suspended;
        }
        // Delete selection first if active (frees capacity for the newline).
        if self.has_selection() {
            let sel = self.selection();
            self.text.replace_range(sel.start..sel.end, "");
            self.cursor = sel.start;
            self.selection_anchor = sel.start;
        }
        if self.cap.saturating_sub(self.text.len()) == 0 {
            // At capacity — nothing further to insert. A selection delete above
            // (if any) already freed space, so this only trips on a full buffer
            return EditOutcome::AtCapacity;
        }
        self.text.insert(self.cursor, '\n');
        self.cursor += 1; // '\n' is one byte
        self.selection_anchor = self.cursor;
        EditOutcome::Mutated
    }

    /// Paste `text` at the cursor position, truncating at the UTF-8 boundary
    /// at the remaining byte cap.
    ///
    /// Oversized content is truncated and `AtCapacity` is returned. No
    /// untruncated bytes are stored or forwarded to any notification.
    ///
    /// Spec: §4.5 (Composer Draft Bounds and Paste Caps).
    pub fn paste(&mut self, text: &str) -> EditOutcome {
        if self.suspended {
            return EditOutcome::Suspended;
        }
        if text.is_empty() {
            return EditOutcome::Unchanged;
        }
        // Delete selection first if active
        if self.has_selection() {
            let sel = self.selection();
            self.text.replace_range(sel.start..sel.end, "");
            self.cursor = sel.start;
            self.selection_anchor = sel.start;
        }
        let remaining = self.cap.saturating_sub(self.text.len());
        if remaining == 0 {
            return EditOutcome::AtCapacity;
        }
        let truncated = truncate_at_utf8_boundary(text, remaining);
        let at_cap = truncated.len() < text.len();
        self.text.insert_str(self.cursor, truncated);
        self.cursor += truncated.len();
        self.selection_anchor = self.cursor;
        if at_cap {
            EditOutcome::AtCapacity
        } else {
            EditOutcome::Mutated
        }
    }

    /// Backspace: delete the character before the cursor (or the selection).
    ///
    /// If a selection is active, deletes the selection. Otherwise removes the
    /// grapheme cluster immediately before the cursor.
    pub fn backspace(&mut self) -> EditOutcome {
        if self.suspended {
            return EditOutcome::Suspended;
        }
        if self.has_selection() {
            return self.delete_selection();
        }
        if self.cursor == 0 {
            return EditOutcome::Unchanged;
        }
        let prev = prev_grapheme_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.selection_anchor = prev;
        EditOutcome::Mutated
    }

    /// Delete: delete the character after the cursor (or the selection).
    pub fn delete_forward(&mut self) -> EditOutcome {
        if self.suspended {
            return EditOutcome::Suspended;
        }
        if self.has_selection() {
            return self.delete_selection();
        }
        if self.cursor == self.text.len() {
            return EditOutcome::Unchanged;
        }
        let next = next_grapheme_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
        // cursor stays at same position
        self.selection_anchor = self.cursor;
        EditOutcome::Mutated
    }

    /// Word-wise backspace: delete from cursor to the start of the preceding word.
    ///
    /// Mirrors the behaviour of Ctrl/Alt + Backspace in the exemplar adapter.
    /// Spec: §4.2, §4.7 (word-wise delete scenario).
    pub fn word_backspace(&mut self) -> EditOutcome {
        if self.suspended {
            return EditOutcome::Suspended;
        }
        if self.has_selection() {
            return self.delete_selection();
        }
        let word_start = word_delete_start(&self.text, self.cursor);
        if word_start == self.cursor {
            return EditOutcome::Unchanged;
        }
        self.text.replace_range(word_start..self.cursor, "");
        self.cursor = word_start;
        self.selection_anchor = word_start;
        EditOutcome::Mutated
    }

    /// Word-wise forward delete: delete from cursor to the end of the next word.
    pub fn word_delete_forward(&mut self) -> EditOutcome {
        if self.suspended {
            return EditOutcome::Suspended;
        }
        if self.has_selection() {
            return self.delete_selection();
        }
        let word_end = word_delete_end(&self.text, self.cursor);
        if word_end == self.cursor {
            return EditOutcome::Unchanged;
        }
        self.text.replace_range(self.cursor..word_end, "");
        self.selection_anchor = self.cursor;
        EditOutcome::Mutated
    }

    /// Move the cursor one grapheme cluster to the left, collapsing any selection.
    pub fn move_left(&mut self) -> EditOutcome {
        if self.has_selection() {
            let start = self.selection().start;
            self.cursor = start;
            self.selection_anchor = start;
            return EditOutcome::Mutated;
        }
        if self.cursor == 0 {
            return EditOutcome::Unchanged;
        }
        let prev = prev_grapheme_boundary(&self.text, self.cursor);
        self.cursor = prev;
        self.selection_anchor = prev;
        EditOutcome::Mutated
    }

    /// Move the cursor one grapheme cluster to the right, collapsing any selection.
    pub fn move_right(&mut self) -> EditOutcome {
        if self.has_selection() {
            let end = self.selection().end;
            self.cursor = end;
            self.selection_anchor = end;
            return EditOutcome::Mutated;
        }
        if self.cursor == self.text.len() {
            return EditOutcome::Unchanged;
        }
        let next = next_grapheme_boundary(&self.text, self.cursor);
        self.cursor = next;
        self.selection_anchor = next;
        EditOutcome::Mutated
    }

    /// Move the cursor to the start of the preceding word (collapsing selection).
    pub fn move_word_left(&mut self) -> EditOutcome {
        let target = word_delete_start(&self.text, self.cursor);
        if target == self.cursor && !self.has_selection() {
            return EditOutcome::Unchanged;
        }
        self.cursor = target;
        self.selection_anchor = target;
        EditOutcome::Mutated
    }

    /// Move the cursor to the end of the next word (collapsing selection).
    pub fn move_word_right(&mut self) -> EditOutcome {
        let target = word_delete_end(&self.text, self.cursor);
        if target == self.cursor && !self.has_selection() {
            return EditOutcome::Unchanged;
        }
        self.cursor = target;
        self.selection_anchor = target;
        EditOutcome::Mutated
    }

    /// Move the cursor to the start of the text (collapsing selection).
    pub fn move_to_start(&mut self) -> EditOutcome {
        if self.cursor == 0 && !self.has_selection() {
            return EditOutcome::Unchanged;
        }
        self.cursor = 0;
        self.selection_anchor = 0;
        EditOutcome::Mutated
    }

    /// Move the cursor to the end of the text (collapsing selection).
    pub fn move_to_end(&mut self) -> EditOutcome {
        let end = self.text.len();
        if self.cursor == end && !self.has_selection() {
            return EditOutcome::Unchanged;
        }
        self.cursor = end;
        self.selection_anchor = end;
        EditOutcome::Mutated
    }

    // ── Vertical movement across hard-newline lines (hud-nx7yq.2) ─────────
    //
    // The draft buffer is the source of truth for logical lines (delimited by
    // embedded '\n' from Ctrl/Shift+Enter). Vertical movement here is over those
    // hard lines, tracked by grapheme column. Soft (visual) wrap sub-lines within
    // one logical line are a compositor-side layout concern with no font metrics
    // in this crate, so moving within a soft-wrapped long line is out of scope for
    // this bead (tracked separately) — the caret still lands on a valid byte.

    /// The caret's current column: grapheme clusters from the start of its
    /// logical line (the byte after the preceding '\n', or 0). Used to seed the
    /// vertical goal column.
    pub fn current_col(&self) -> usize {
        let line_start = logical_line_start(&self.text, self.cursor);
        grapheme_count(&self.text[line_start..self.cursor])
    }

    /// Byte offset of the vertical-move target, or `None` when the caret cannot
    /// move that direction (already on the first/last line at the boundary).
    ///
    /// `goal_col` is the desired grapheme column; the target lands at that column
    /// in the adjacent logical line, clamped to that line's end. On the first line
    /// an upward move goes to buffer start; on the last line a downward move goes
    /// to buffer end (standard editor edge behavior).
    fn vertical_target(&self, up: bool, goal_col: usize) -> Option<usize> {
        let text = &self.text;
        let line_start = logical_line_start(text, self.cursor);
        if up {
            if line_start == 0 {
                // First logical line: move to buffer start (or no-op if there).
                return if self.cursor == 0 { None } else { Some(0) };
            }
            let prev_line_end = line_start - 1; // the '\n' terminating the prev line
            let prev_line_start = logical_line_start(text, prev_line_end);
            Some(byte_at_grapheme_col(
                text,
                prev_line_start,
                prev_line_end,
                goal_col,
            ))
        } else {
            let line_end = logical_line_end(text, self.cursor);
            if line_end == text.len() {
                // Last logical line: move to buffer end (or no-op if there).
                return if self.cursor == text.len() {
                    None
                } else {
                    Some(text.len())
                };
            }
            let next_line_start = line_end + 1; // skip the '\n'
            let next_line_end = logical_line_end(text, next_line_start);
            Some(byte_at_grapheme_col(
                text,
                next_line_start,
                next_line_end,
                goal_col,
            ))
        }
    }

    /// Move the caret up one logical line at `goal_col`, collapsing any selection.
    pub fn move_up(&mut self, goal_col: usize) -> EditOutcome {
        match self.vertical_target(true, goal_col) {
            Some(target) => {
                self.cursor = target;
                self.selection_anchor = target;
                EditOutcome::Mutated
            }
            None => EditOutcome::Unchanged,
        }
    }

    /// Move the caret down one logical line at `goal_col`, collapsing any selection.
    pub fn move_down(&mut self, goal_col: usize) -> EditOutcome {
        match self.vertical_target(false, goal_col) {
            Some(target) => {
                self.cursor = target;
                self.selection_anchor = target;
                EditOutcome::Mutated
            }
            None => EditOutcome::Unchanged,
        }
    }

    /// Extend the selection up one logical line at `goal_col` (Shift+ArrowUp):
    /// moves the caret without collapsing the anchor.
    pub fn select_up(&mut self, goal_col: usize) -> EditOutcome {
        match self.vertical_target(true, goal_col) {
            Some(target) if target != self.cursor => {
                self.cursor = target;
                EditOutcome::Mutated
            }
            _ => EditOutcome::Unchanged,
        }
    }

    /// Extend the selection down one logical line at `goal_col` (Shift+ArrowDown).
    pub fn select_down(&mut self, goal_col: usize) -> EditOutcome {
        match self.vertical_target(false, goal_col) {
            Some(target) if target != self.cursor => {
                self.cursor = target;
                EditOutcome::Mutated
            }
            _ => EditOutcome::Unchanged,
        }
    }

    /// Move the caret to an absolute byte offset, collapsing any selection
    /// (hud-21o6x). Used by visual-line vertical movement, which computes a target
    /// byte from the compositor's wrapped-line layout. The byte is clamped to
    /// `text.len()` and snapped down to a char boundary so a stale layout can
    /// never place the caret mid-character.
    pub fn move_to_byte(&mut self, byte: usize) -> EditOutcome {
        let target = self.snap_byte(byte);
        if target == self.cursor && !self.has_selection() {
            return EditOutcome::Unchanged;
        }
        self.cursor = target;
        self.selection_anchor = target;
        EditOutcome::Mutated
    }

    /// Extend the selection to an absolute byte offset without collapsing the
    /// anchor (Shift + visual vertical move, hud-21o6x).
    pub fn select_to_byte(&mut self, byte: usize) -> EditOutcome {
        let target = self.snap_byte(byte);
        if target == self.cursor {
            return EditOutcome::Unchanged;
        }
        self.cursor = target;
        EditOutcome::Mutated
    }

    /// Select the entire draft (Ctrl+A): anchor at start, cursor at end, so the
    /// whole buffer is highlighted and a subsequent copy/cut/insert acts on all
    /// of it. Returns `Unchanged` for an empty draft or when the whole buffer is
    /// already selected (idempotent — no spurious redraw/notification).
    ///
    /// Spec: §"Local-First Composer Draft Editing" — selection (keyboard).
    pub fn select_all(&mut self) -> EditOutcome {
        let end = self.text.len();
        if end == 0 || (self.selection_anchor == 0 && self.cursor == end) {
            return EditOutcome::Unchanged;
        }
        self.selection_anchor = 0;
        self.cursor = end;
        EditOutcome::Mutated
    }

    /// The currently selected text (`""` when there is no active selection).
    ///
    /// Used by the runtime clipboard-write path (Ctrl+C / Ctrl+X) to snapshot the
    /// selection BEFORE a cut mutates the buffer. Selection offsets are always on
    /// char boundaries (maintained by every selection mutator), so the slice never
    /// panics.
    pub fn selected_text(&self) -> &str {
        let sel = self.selection();
        &self.text[sel.start..sel.end]
    }

    /// Cut (Ctrl+X): remove the active selection from the buffer, leaving the
    /// caret collapsed at the deletion point. Returns `Unchanged` when there is
    /// no selection (nothing to cut). The caller is responsible for having copied
    /// [`Self::selected_text`] to the OS clipboard first.
    pub fn cut(&mut self) -> EditOutcome {
        if self.suspended {
            return EditOutcome::Suspended;
        }
        self.delete_selection()
    }

    /// Clamp `byte` to `text.len()` and snap DOWN to the nearest char boundary.
    fn snap_byte(&self, byte: usize) -> usize {
        let mut b = byte.min(self.text.len());
        while b > 0 && !self.text.is_char_boundary(b) {
            b -= 1;
        }
        b
    }

    // ── Selection editing operations (shift + movement) ───────────────────

    /// Extend the selection one grapheme cluster to the left (shift + Left).
    pub fn select_left(&mut self) -> EditOutcome {
        if self.cursor == 0 {
            return EditOutcome::Unchanged;
        }
        let prev = prev_grapheme_boundary(&self.text, self.cursor);
        self.cursor = prev;
        EditOutcome::Mutated
    }

    /// Extend the selection one grapheme cluster to the right (shift + Right).
    pub fn select_right(&mut self) -> EditOutcome {
        if self.cursor == self.text.len() {
            return EditOutcome::Unchanged;
        }
        let next = next_grapheme_boundary(&self.text, self.cursor);
        self.cursor = next;
        EditOutcome::Mutated
    }

    /// Extend the selection to the start of the text (shift + Home).
    pub fn select_to_start(&mut self) -> EditOutcome {
        if self.cursor == 0 {
            return EditOutcome::Unchanged;
        }
        self.cursor = 0;
        EditOutcome::Mutated
    }

    /// Extend the selection to the end of the text (shift + End).
    pub fn select_to_end(&mut self) -> EditOutcome {
        let end = self.text.len();
        if self.cursor == end {
            return EditOutcome::Unchanged;
        }
        self.cursor = end;
        EditOutcome::Mutated
    }

    /// Extend the selection to the start of the preceding word
    /// (Shift+Ctrl+Left / Shift+Alt+Left). Moves the cursor to the word
    /// boundary **without** collapsing the selection anchor, so the highlighted
    /// range grows by a whole word rather than a single grapheme.
    pub fn select_word_left(&mut self) -> EditOutcome {
        let target = word_delete_start(&self.text, self.cursor);
        if target == self.cursor {
            return EditOutcome::Unchanged;
        }
        self.cursor = target;
        EditOutcome::Mutated
    }

    /// Extend the selection to the end of the next word
    /// (Shift+Ctrl+Right / Shift+Alt+Right). Moves the cursor to the word
    /// boundary **without** collapsing the selection anchor.
    pub fn select_word_right(&mut self) -> EditOutcome {
        let target = word_delete_end(&self.text, self.cursor);
        if target == self.cursor {
            return EditOutcome::Unchanged;
        }
        self.cursor = target;
        EditOutcome::Mutated
    }

    /// Set pointer selection: `anchor` is the click position, `cursor` is the
    /// drag position. Both are raw byte offsets from the UI layer and are
    /// snapped down to the nearest valid UTF-8 character boundary before
    /// storage, preventing panics in subsequent `replace_range`/`insert_str`
    /// calls on multi-byte characters.
    pub fn set_pointer_selection(&mut self, anchor: usize, cursor: usize) -> EditOutcome {
        debug_assert!(anchor <= self.text.len(), "anchor out of bounds");
        debug_assert!(cursor <= self.text.len(), "cursor out of bounds");
        let mut anchor = anchor.min(self.text.len());
        while !self.text.is_char_boundary(anchor) {
            anchor -= 1;
        }
        let mut cursor = cursor.min(self.text.len());
        while !self.text.is_char_boundary(cursor) {
            cursor -= 1;
        }
        if self.selection_anchor == anchor && self.cursor == cursor {
            return EditOutcome::Unchanged;
        }
        self.selection_anchor = anchor;
        self.cursor = cursor;
        EditOutcome::Mutated
    }

    // ── Submit / Cancel ───────────────────────────────────────────────────

    /// Submit exact non-whitespace text and reset the local draft.
    pub fn submit(&mut self) -> ComposerOutcome {
        if self.suspended || self.text.trim().is_empty() {
            return ComposerOutcome::Unchanged;
        }
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.selection_anchor = 0;
        ComposerOutcome::Submitted(text)
    }

    /// Clear the draft without producing portal input.
    pub fn cancel(&mut self) -> ComposerOutcome {
        if self.suspended {
            return ComposerOutcome::Unchanged;
        }
        self.text.clear();
        self.cursor = 0;
        self.selection_anchor = 0;
        ComposerOutcome::Cancelled
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    fn delete_selection(&mut self) -> EditOutcome {
        if !self.has_selection() {
            return EditOutcome::Unchanged;
        }
        let sel = self.selection();
        self.text.replace_range(sel.start..sel.end, "");
        self.cursor = sel.start;
        self.selection_anchor = sel.start;
        EditOutcome::Mutated
    }
}

impl Default for ComposerDraft {
    /// Create a draft buffer with the default cap (`DEFAULT_DRAFT_CAP`).
    fn default() -> Self {
        Self::new(DEFAULT_DRAFT_CAP)
    }
}

// ─── Unicode helpers ──────────────────────────────────────────────────────────

/// Truncate `s` to at most `max_bytes`, cutting on a **grapheme-cluster boundary**.
///
/// # Why grapheme clusters and not char boundaries
///
/// Truncating at a bare char boundary can split multi-codepoint grapheme clusters
/// (e.g. NFD `"e\u{0301}"`, emoji ZWJ sequences, or skin-tone modifiers). The
/// result is a string that is valid UTF-8 but visually incomplete / semantically
/// wrong. Since the composer cap guards *visible* content, we must truncate so that
/// the last retained grapheme is whole.
///
/// Algorithm: iterate grapheme clusters from the front, accumulate byte lengths,
/// stop when the next cluster would push us past `max_bytes`. Returns the largest
/// grapheme-aligned prefix that fits within `max_bytes`.
///
/// Spec: §4.5 — paste truncated at a UTF-8 grapheme-cluster boundary at the cap.
fn truncate_at_utf8_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk grapheme clusters and collect the largest prefix ≤ max_bytes.
    let mut end = 0usize;
    for cluster in s.graphemes(true) {
        let next = end + cluster.len();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    &s[..end]
}

/// Byte offset of the start of the logical line (delimited by `\n`) containing
/// `pos`: one past the nearest preceding `\n`, or `0` (hud-nx7yq.2).
fn logical_line_start(text: &str, pos: usize) -> usize {
    match text[..pos].rfind('\n') {
        Some(nl) => nl + 1,
        None => 0,
    }
}

/// Byte offset of the end of the logical line containing `pos`: the next `\n` at
/// or after `pos`, or `text.len()` (hud-nx7yq.2).
fn logical_line_end(text: &str, pos: usize) -> usize {
    match text[pos..].find('\n') {
        Some(off) => pos + off,
        None => text.len(),
    }
}

/// Number of grapheme clusters in `s`.
fn grapheme_count(s: &str) -> usize {
    s.graphemes(true).count()
}

/// Byte offset of the `col`-th grapheme boundary within the line `[line_start,
/// line_end)`, clamped to `line_end` when the line is shorter than `col`
/// (hud-nx7yq.2). Used to land a vertical move at the goal column.
fn byte_at_grapheme_col(text: &str, line_start: usize, line_end: usize, col: usize) -> usize {
    let line = &text[line_start..line_end];
    let mut offset = 0usize;
    for (i, cluster) in line.graphemes(true).enumerate() {
        if i == col {
            return line_start + offset;
        }
        offset += cluster.len();
    }
    // Column past the end of the line → clamp to the line end.
    line_end
}

/// Byte offset of the grapheme boundary immediately before `byte_pos`.
fn prev_grapheme_boundary(text: &str, byte_pos: usize) -> usize {
    if byte_pos == 0 {
        return 0;
    }
    let before = &text[..byte_pos];
    let mut clusters = before.grapheme_indices(true);
    // Find the last cluster start
    let mut last_start = 0;
    for (idx, _) in &mut clusters {
        last_start = idx;
    }
    last_start
}

/// Byte offset of the grapheme boundary immediately after `byte_pos`.
fn next_grapheme_boundary(text: &str, byte_pos: usize) -> usize {
    if byte_pos == text.len() {
        return byte_pos;
    }
    let after = &text[byte_pos..];
    let mut clusters = after.grapheme_indices(true);
    match clusters.next() {
        Some((_, gc)) => byte_pos + gc.len(),
        None => byte_pos,
    }
}

/// Byte offset of the start of the word to delete on word-backspace.
///
/// Mirrors `composer_word_delete_start` from the exemplar adapter (Python):
/// skip trailing whitespace, then skip the non-whitespace word.
fn word_delete_start(text: &str, cursor: usize) -> usize {
    let before = &text[..cursor];
    let bytes = before.as_bytes();
    let mut pos = cursor;
    // Skip trailing whitespace
    while pos > 0 && bytes[pos - 1].is_ascii_whitespace() {
        pos -= 1;
    }
    // Skip the preceding word (non-whitespace)
    while pos > 0 && !bytes[pos - 1].is_ascii_whitespace() {
        pos -= 1;
    }
    pos
}

/// Byte offset of the end of the word after cursor for word-forward-delete.
///
/// Skip leading whitespace, then skip the non-whitespace word.
fn word_delete_end(text: &str, cursor: usize) -> usize {
    let after = &text[cursor..];
    let bytes = after.as_bytes();
    let mut pos = 0;
    // Skip leading whitespace
    while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    // Skip the word
    while pos < bytes.len() && !bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    cursor + pos
}

// ─── ComposerDraftManager ─────────────────────────────────────────────────────

/// Owns the focused local composer and one keyed retained unsent draft.
/// Portal submission is returned directly; local editing never waits for an agent.
#[derive(Debug, Default)]
pub struct ComposerDraftManager {
    /// Active draft buffer, present while a composer region is focused.
    draft: Option<ComposerDraft>,
    /// The node_id of the currently focused composer region, if any.
    focused_node: Option<SceneId>,
    /// Per-composer placeholder-hint override for the currently focused
    /// composer node, mirroring `HitRegionNode::composer_placeholder`'s
    /// three-state `Option<String>` convention (`None` = no override
    /// configured, `Some("")` = explicit opt-out, `Some(text)` = custom
    /// hint). Reset to `None` on every `on_focus_gained` call; the caller
    /// (which holds scene access this manager does not) resolves the
    /// focused node's config and supplies it via `set_focused_placeholder`
    /// immediately afterward. Source: hud-se6hs, follow-up to hud-evk0j.
    focused_placeholder: Option<String>,
    /// One keyed unsent draft retained alongside the active draft.
    retained_draft: Option<(SceneId, ComposerDraft)>,
    /// Goal column (grapheme count from the logical-line start) preserved across
    /// a run of consecutive vertical caret moves (ArrowUp/ArrowDown), so the
    /// caret tracks the same column when passing through shorter lines — standard
    /// editor "desired column" behavior (hud-nx7yq.2).
    ///
    /// Set on the first vertical move of a run and cleared by any other key, so
    /// horizontal movement or editing re-establishes the column on the next
    /// vertical move. Grapheme-column based (not pixel-x): the runtime-owned draft
    /// has no font metrics, so this tracks logical columns across the draft's
    /// hard newlines. See [`Self::route_key_down`].
    vertical_goal_col: Option<usize>,
    /// Pixel goal-x preserved across a run of vertical moves when a fresh visual
    /// layout is available (hud-21o6x). The visual-line path uses this instead of
    /// `vertical_goal_col` so the caret tracks the same on-screen x across
    /// soft-wrapped rows of differing character widths. Set/cleared with the same
    /// lifecycle as `vertical_goal_col`.
    vertical_goal_x: Option<f32>,
    /// Latest wrapped-line layout for the active composer, pushed from the
    /// compositor render thread each frame (hud-21o6x). Consumed by ArrowUp/Down
    /// for visual-row vertical movement; `None` when no multi-line composer is
    /// active. May be one frame stale — guarded by `text_len` before use.
    latest_visual_layout: Option<ComposerVisualLayout>,
}

impl ComposerDraftManager {
    /// Create a new manager with no active draft.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore only a matching retained draft, adopting current governance.
    pub fn on_focus_gained(&mut self, node_id: SceneId, suspended: bool) {
        let mut draft = if self
            .retained_draft
            .as_ref()
            .is_some_and(|(id, _)| *id == node_id)
        {
            self.retained_draft
                .take()
                .expect("matching retained draft")
                .1
        } else {
            ComposerDraft::new(DEFAULT_DRAFT_CAP)
        };
        draft.set_suspended(suspended);
        self.draft = Some(draft);
        self.focused_node = Some(node_id);
        // Reset to "no override" so a placeholder from a previously-focused
        // node never leaks onto this one; the caller supplies the new node's
        // config (if any) via `set_focused_placeholder` right after this call.
        self.focused_placeholder = None;
    }

    /// Set the composer-placeholder override for the currently focused
    /// composer node (see `HitRegionNode::composer_placeholder`).
    ///
    /// Called by the input processor immediately after `on_focus_gained`,
    /// which has scene access this manager does not, to resolve the newly
    /// focused node's placeholder config. No-op in the sense that it does
    /// not validate a composer is actually focused — callers only invoke it
    /// from the focus-gained path.
    pub fn set_focused_placeholder(&mut self, placeholder: Option<String>) {
        self.focused_placeholder = placeholder;
    }

    /// The composer-placeholder override for the currently focused composer
    /// node, if one was set via `set_focused_placeholder`. See
    /// `HitRegionNode::composer_placeholder` for the three-state convention.
    pub fn focused_placeholder(&self) -> Option<String> {
        self.focused_placeholder.clone()
    }

    /// Retain a nonempty draft on blur, replacing the previous retained slot.
    /// An empty peer does not erase another node's unsent text.
    pub fn on_focus_lost(&mut self) {
        if let (Some(node_id), Some(draft)) = (self.focused_node.take(), self.draft.take()) {
            if !draft.text().is_empty() {
                self.retained_draft = Some((node_id, draft));
            } else if self
                .retained_draft
                .as_ref()
                .is_some_and(|(id, _)| *id == node_id)
            {
                self.retained_draft = None;
            }
        }
        self.focused_placeholder = None;
        self.vertical_goal_col = None;
        self.vertical_goal_x = None;
        self.latest_visual_layout = None;
    }

    /// Removed scene nodes must never restore an old draft into a new incarnation.
    pub(crate) fn retain_existing_nodes(&mut self, scene: &tze_hud_scene::SceneGraph) {
        if self
            .retained_draft
            .as_ref()
            .is_some_and(|(id, _)| !scene.nodes.contains_key(id))
        {
            self.retained_draft = None;
        }
        if self
            .focused_node
            .is_some_and(|id| !scene.nodes.contains_key(&id))
        {
            self.draft = None;
            self.focused_node = None;
            self.focused_placeholder = None;
            self.vertical_goal_col = None;
            self.vertical_goal_x = None;
            self.latest_visual_layout = None;
        }
    }

    /// Insert sanitized character or paste text into the active local draft.
    pub fn route_character(&mut self, character: &str) -> EditOutcome {
        let Some(draft) = self.draft.as_mut() else {
            return EditOutcome::Unchanged;
        };
        // Route multiline/control text through paste() after sanitisation so
        // it is consumed by the composer rather than falling through to the
        // agent's raw character stream (§4.4 / hud-083az).
        if character
            .chars()
            .any(|c| c == '\r' || c == '\n' || c.is_control())
        {
            let sanitised: String = character
                .chars()
                .filter(|c| *c != '\r' && *c != '\n' && !c.is_control())
                .collect();
            draft.paste(&sanitised)
        } else {
            draft.insert(character)
        }
    }

    /// Route a pointer-down event to set the cursor position in the active
    /// draft buffer.
    ///
    /// `anchor` and `cursor` are byte offsets into the draft text; callers are
    /// responsible for computing them from the pointer position (e.g.
    /// proportional to local_x / node width, scaled by `draft.text().len()`).
    /// Both values are clamped and snapped to UTF-8 boundaries inside
    /// `ComposerDraft::set_pointer_selection`.
    ///
    /// Returns `EditOutcome::Unchanged` when no composer is focused.
    ///
    /// Spec: §4.1 — pointer selection into the runtime-owned draft buffer
    /// (hud-083az).
    pub fn route_pointer_selection(&mut self, anchor: usize, cursor: usize) -> EditOutcome {
        let Some(draft) = self.draft.as_mut() else {
            return EditOutcome::Unchanged;
        };
        draft.set_pointer_selection(anchor, cursor)
    }

    /// Route a key-down event into the active draft buffer.
    ///
    /// Interprets:
    /// - `Backspace` → `draft.backspace()`
    /// - `Delete` → `draft.delete_forward()`
    /// - `Ctrl+Backspace` / `Alt+Backspace` → `draft.word_backspace()`
    /// - `Ctrl+Delete` / `Alt+Delete` → `draft.word_delete_forward()`
    /// - `ArrowLeft` → `draft.move_left()` (or `select_left` with Shift)
    /// - `ArrowRight` → `draft.move_right()` (or `select_right` with Shift)
    /// - `Home` → `draft.move_to_start()` (or `select_to_start` with Shift)
    /// - `End` → `draft.move_to_end()` (or `select_to_end` with Shift)
    /// - `Ctrl+ArrowLeft` → `draft.move_word_left()`
    /// - `Ctrl+ArrowRight` → `draft.move_word_right()`
    /// - `Shift+Ctrl+ArrowLeft` → `draft.select_word_left()` (extend by word)
    /// - `Shift+Ctrl+ArrowRight` → `draft.select_word_right()` (extend by word)
    /// - `ArrowUp` / `ArrowDown` → move the caret up/down one logical line at the
    ///   preserved goal column (or extend the selection with `Shift`)
    /// - `Space` → insert literal space
    /// - `Enter` / `NumpadEnter` → submit the draft (non-empty, non-whitespace)
    /// - `Ctrl+Enter` / `Shift+Enter` → insert a newline into the draft (multi-line)
    /// - `Escape` → cancel without portal input
    ///
    /// Returns an independent consumed flag and high-level local outcome.
    /// `consumed = true` means the keystroke was handled by the draft (do NOT
    /// forward to the agent as a raw `KeyDownEvent`).
    pub fn route_key_down(
        &mut self,
        key_code: &str,
        key: &str,
        shift: bool,
        ctrl: bool,
        alt: bool,
    ) -> (bool, ComposerOutcome) {
        // Take the preserved vertical goal (grapheme column AND pixel x): both
        // survive ONLY a run of consecutive ArrowUp/ArrowDown keys (those arms
        // re-store the relevant one). Any other key clears them here, so the next
        // vertical move re-seeds from the caret (standard editor "desired column"
        // reset on horizontal move / edit).
        let prev_goal = self.vertical_goal_col.take();
        let prev_goal_x = self.vertical_goal_x.take();

        let Some(draft) = self.draft.as_mut() else {
            return (false, ComposerOutcome::Unchanged);
        };

        let enter_key = is_enter_key(key_code, key);
        match key_code {
            "Backspace" => {
                let o = if ctrl || alt {
                    draft.word_backspace()
                } else {
                    draft.backspace()
                };
                (true, o.into())
            }
            "Delete" => {
                let o = if ctrl || alt {
                    draft.word_delete_forward()
                } else {
                    draft.delete_forward()
                };
                (true, o.into())
            }
            "ArrowLeft" => {
                // Order matters: Shift+Ctrl/Alt extends by word, so it must be
                // checked before the bare-Shift (one-grapheme) arm — otherwise
                // Ctrl+Shift+Left would only extend a single character.
                let o = if shift && (ctrl || alt) {
                    draft.select_word_left()
                } else if shift {
                    draft.select_left()
                } else if ctrl || alt {
                    draft.move_word_left()
                } else {
                    draft.move_left()
                };
                (true, o.into())
            }
            "ArrowRight" => {
                let o = if shift && (ctrl || alt) {
                    draft.select_word_right()
                } else if shift {
                    draft.select_right()
                } else if ctrl || alt {
                    draft.move_word_right()
                } else {
                    draft.move_right()
                };
                (true, o.into())
            }
            "ArrowUp" | "ArrowDown" => {
                let up = key_code == "ArrowUp";
                // Prefer the compositor's wrapped-line layout when it is FRESH
                // (measured for exactly this draft) and actually multi-row, so the
                // caret steps between VISUAL rows (soft-wrap aware) at a pixel
                // goal-x (hud-21o6x). Otherwise fall back to hard-newline movement
                // at a grapheme goal-column (hud-nx7yq.2) — used for the
                // single-line profile and for the one keystroke right after a
                // wrap-changing edit, before the next frame re-measures.
                let fresh_layout = self
                    .latest_visual_layout
                    .as_ref()
                    .filter(|l| l.text_len == draft.text().len() && l.lines.len() > 1);
                let o = if let Some(layout) = fresh_layout {
                    let goal_x = prev_goal_x.unwrap_or_else(|| layout.x_at_cursor(draft.cursor()));
                    self.vertical_goal_x = Some(goal_x);
                    match layout.vertical_target(draft.cursor(), up, goal_x) {
                        Some(target) => {
                            if shift {
                                draft.select_to_byte(target)
                            } else {
                                draft.move_to_byte(target)
                            }
                        }
                        None => EditOutcome::Unchanged,
                    }
                } else {
                    // Hard-newline fallback: grapheme goal-column.
                    let goal = prev_goal.unwrap_or_else(|| draft.current_col());
                    self.vertical_goal_col = Some(goal);
                    match (up, shift) {
                        (true, false) => draft.move_up(goal),
                        (false, false) => draft.move_down(goal),
                        (true, true) => draft.select_up(goal),
                        (false, true) => draft.select_down(goal),
                    }
                };
                (true, o.into())
            }
            "Home" => {
                let o = if shift {
                    draft.select_to_start()
                } else {
                    draft.move_to_start()
                };
                (true, o.into())
            }
            "End" => {
                let o = if shift {
                    draft.select_to_end()
                } else {
                    draft.move_to_end()
                };
                (true, o.into())
            }
            "Space" if !ctrl && !alt => {
                let o = draft.insert(" ");
                (true, o.into())
            }
            _ if enter_key => {
                // Ctrl+Enter / Shift+Enter insert a newline as a local draft edit;
                // plain Enter submits (spec: Composer Submit-Key Contract). Covers
                // "Enter"/"NumpadEnter" key codes and logical "\r"/"\n" keys.
                if ctrl || shift {
                    let o = draft.insert_newline();
                    return (true, o.into());
                }
                (true, draft.submit())
            }
            "Escape" => (true, draft.cancel()),
            "KeyV" if ctrl && !alt => {
                // Ctrl+V (paste shortcut): consume the KeyDown so it is never
                // forwarded to the agent as a raw KeyDownEvent while a composer
                // is focused.  The actual paste content arrives separately via
                // `route_character` (dispatch_character_event reads the
                // clipboard and fires a RawCharacterEvent immediately after the
                // KeyDown — hud-083az).
                (true, ComposerOutcome::Unchanged)
            }
            "KeyA" if ctrl && !alt => {
                // Ctrl+A: select the whole draft. Pure local selection change —
                // no clipboard involvement — so it is handled entirely here.
                let o = draft.select_all();
                (true, o.into())
            }
            "KeyC" if ctrl && !alt => {
                // Ctrl+C: copy. The OS clipboard write lives in the runtime layer
                // (this crate has no clipboard access); the runtime snapshots
                // `selected_text()` before calling us. We only CONSUME the
                // KeyDown so it never leaks to the agent, and never mutate the
                // draft (copy is non-destructive).
                (true, ComposerOutcome::Unchanged)
            }
            "KeyX" if ctrl && !alt => {
                // Ctrl+X: cut. The runtime has already copied `selected_text()`
                // to the clipboard; here we remove the selection locally. A cut
                // with no selection is a consumed no-op.
                let o = draft.cut();
                (true, o.into())
            }
            _ => {
                // Not a handled editing key; do not consume (let caller forward
                // to agent if needed).
                (false, ComposerOutcome::Unchanged)
            }
        }
    }

    /// Called when safe mode activates or deactivates.
    ///
    /// Propagates the suspension state to the active draft buffer.
    pub fn set_suspended(&mut self, suspended: bool) {
        if let Some(draft) = self.draft.as_mut() {
            draft.set_suspended(suspended);
        }
    }

    /// Update the wrapped-line layout used for visual-row vertical caret movement
    /// (hud-21o6x). Called by the runtime each time it observes a fresh layout
    /// from the compositor (before dispatching an ArrowUp/ArrowDown). `None`
    /// clears it (no multi-line composer active), reverting to hard-newline
    /// vertical movement.
    pub fn set_visual_layout(&mut self, layout: Option<ComposerVisualLayout>) {
        self.latest_visual_layout = layout;
    }

    /// Returns a reference to the active draft buffer, if any.
    pub fn draft(&self) -> Option<&ComposerDraft> {
        self.draft.as_ref()
    }

    /// Returns the node_id of the currently focused composer region, if any.
    pub fn focused_node(&self) -> Option<SceneId> {
        self.focused_node
    }

    /// True when a composer region is currently focused (draft buffer present).
    pub fn is_active(&self) -> bool {
        self.draft.is_some()
    }
}

fn is_enter_key(key_code: &str, key: &str) -> bool {
    matches!(key_code, "Enter" | "NumpadEnter")
        || matches!(key, "Enter" | "NumpadEnter" | "\r" | "\n")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn submitted(outcome: ComposerOutcome) -> String {
        match outcome {
            ComposerOutcome::Submitted(text) => text,
            other => panic!("expected submitted text, got {other:?}"),
        }
    }

    // ─── Basic insert / backspace ─────────────────────────────────────────

    #[test]
    fn insert_and_backspace_round_trip() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        assert_eq!(draft.insert("hello"), EditOutcome::Mutated);
        assert_eq!(draft.text(), "hello");
        assert_eq!(draft.cursor(), 5);
        assert_eq!(draft.backspace(), EditOutcome::Mutated);
        assert_eq!(draft.text(), "hell");
        assert_eq!(draft.cursor(), 4);
    }

    #[test]
    fn backspace_at_start_is_unchanged() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        assert_eq!(draft.backspace(), EditOutcome::Unchanged);
    }

    #[test]
    fn insert_mid_text() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("ab");
        draft.move_left();
        draft.insert("X");
        assert_eq!(draft.text(), "aXb");
        assert_eq!(draft.cursor(), 2);
    }

    // ─── Newline / control rejection ──────────────────────────────────────

    #[test]
    fn insert_newline_rejected() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        let outcome = draft.insert("\n");
        assert_eq!(outcome, EditOutcome::Unchanged);
        assert!(draft.text().is_empty());
    }

    #[test]
    fn insert_cr_rejected() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        assert_eq!(draft.insert("\r"), EditOutcome::Unchanged);
    }

    // ─── Word-wise delete ─────────────────────────────────────────────────

    /// Spec scenario 4.7: word-wise backspace removes the preceding word and
    /// the adapter observes only a draft-state notification, not per-keystroke
    /// traffic.
    #[test]
    fn word_backspace_removes_preceding_word() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("hello world");
        assert_eq!(draft.cursor(), 11);
        let outcome = draft.word_backspace();
        assert_eq!(outcome, EditOutcome::Mutated);
        assert_eq!(draft.text(), "hello ");
        assert_eq!(draft.cursor(), 6);
    }

    #[test]
    fn word_backspace_skips_trailing_whitespace_then_deletes_word() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("foo bar   ");
        let outcome = draft.word_backspace();
        assert_eq!(outcome, EditOutcome::Mutated);
        assert_eq!(draft.text(), "foo ");
    }

    #[test]
    fn word_backspace_at_start_is_unchanged() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        assert_eq!(draft.word_backspace(), EditOutcome::Unchanged);
    }

    #[test]
    fn word_delete_forward_removes_next_word() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("hello world");
        draft.move_to_start();
        let outcome = draft.word_delete_forward();
        assert_eq!(outcome, EditOutcome::Mutated);
        assert_eq!(draft.text(), " world");
    }

    // ─── Caret movement ───────────────────────────────────────────────────

    #[test]
    fn move_left_right_around_text() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("abc");
        assert_eq!(draft.move_left(), EditOutcome::Mutated);
        assert_eq!(draft.cursor(), 2);
        assert_eq!(draft.move_right(), EditOutcome::Mutated);
        assert_eq!(draft.cursor(), 3);
        assert_eq!(draft.move_right(), EditOutcome::Unchanged);
    }

    #[test]
    fn move_to_start_and_end() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("hello");
        assert_eq!(draft.move_to_start(), EditOutcome::Mutated);
        assert_eq!(draft.cursor(), 0);
        assert_eq!(draft.move_to_start(), EditOutcome::Unchanged);
        assert_eq!(draft.move_to_end(), EditOutcome::Mutated);
        assert_eq!(draft.cursor(), 5);
        assert_eq!(draft.move_to_end(), EditOutcome::Unchanged);
    }

    // ─── Selection ────────────────────────────────────────────────────────

    #[test]
    fn selection_left_right() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("abcde");
        draft.move_to_start();
        draft.select_right();
        draft.select_right();
        let sel = draft.selection();
        assert_eq!(sel.start, 0);
        assert_eq!(sel.end, 2);
        assert!(draft.has_selection());
    }

    #[test]
    fn move_collapses_selection_to_start() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("abcde");
        draft.move_to_start();
        draft.select_to_end();
        assert!(draft.has_selection());
        draft.move_left();
        assert!(!draft.has_selection());
        assert_eq!(draft.cursor(), 0);
    }

    #[test]
    fn backspace_deletes_selection() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("abcde");
        draft.move_to_start();
        draft.select_right();
        draft.select_right();
        draft.select_right(); // select "abc"
        let outcome = draft.backspace();
        assert_eq!(outcome, EditOutcome::Mutated);
        assert_eq!(draft.text(), "de");
        assert_eq!(draft.cursor(), 0);
    }

    #[test]
    fn pointer_selection() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("hello");
        draft.set_pointer_selection(1, 4);
        let sel = draft.selection();
        assert_eq!(sel.start, 1);
        assert_eq!(sel.end, 4);
    }

    // ─── Paste and cap ────────────────────────────────────────────────────

    /// Spec §4.5 + §4.7: oversized paste truncates at UTF-8 boundary; at-capacity
    /// result returned; no overflow bytes in the notification or submission.
    #[test]
    fn paste_truncates_at_cap() {
        let cap = 10;
        let mut draft = ComposerDraft::new(cap);
        let big = "a".repeat(20);
        let outcome = draft.paste(&big);
        assert_eq!(outcome, EditOutcome::AtCapacity);
        assert_eq!(draft.text().len(), 10);
        assert!(draft.is_at_capacity());
        assert_eq!(draft.cursor(), 10);
    }

    #[test]
    fn paste_utf8_boundary_preserved() {
        // "é" is 2 bytes; cap at 1 byte means the é gets dropped, not split.
        let mut draft = ComposerDraft::new(1);
        let outcome = draft.paste("é");
        assert_eq!(outcome, EditOutcome::AtCapacity);
        assert_eq!(draft.text(), "");
    }

    #[test]
    fn paste_within_cap_is_mutated() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        assert_eq!(draft.paste("hello"), EditOutcome::Mutated);
        assert_eq!(draft.text(), "hello");
    }

    #[test]
    fn insert_at_cap_returns_at_capacity() {
        let mut draft = ComposerDraft::new(5);
        draft.insert("hello");
        assert!(draft.is_at_capacity());
        let outcome = draft.insert("z");
        assert_eq!(outcome, EditOutcome::AtCapacity);
        assert_eq!(draft.text(), "hello");
    }

    // ─── Submit / Cancel ──────────────────────────────────────────────────

    /// Spec §4.7: submit-content fidelity — submitted text equals local buffer.
    #[test]
    fn submit_returns_exact_buffer_and_clears() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("send me");
        let sub = submitted(draft.submit());
        assert_eq!(sub, "send me");
        assert_eq!(draft.text(), "");
        assert_eq!(draft.cursor(), 0);
    }

    #[test]
    fn cancel_clears_without_submission() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("typed stuff");
        assert_eq!(draft.cancel(), ComposerOutcome::Cancelled);
        assert_eq!(draft.text(), "");
    }

    // ─── Governance: safe-mode suspension ────────────────────────────────

    /// Spec §4.5 + §4.7: draft suspends under safe mode; insert is rejected.
    #[test]
    fn suspended_draft_rejects_insert() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.set_suspended(true);
        let outcome = draft.insert("x");
        assert_eq!(outcome, EditOutcome::Suspended);
        assert_eq!(draft.text(), "");
    }

    #[test]
    fn suspended_draft_rejects_paste() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.set_suspended(true);
        assert_eq!(draft.paste("hello"), EditOutcome::Suspended);
    }

    #[test]
    fn suspended_draft_rejects_backspace() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("hello");
        draft.set_suspended(true);
        assert_eq!(draft.backspace(), EditOutcome::Suspended);
        assert_eq!(draft.text(), "hello"); // text preserved
    }

    #[test]
    fn suspended_submit_preserves_draft() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("pending text");
        draft.set_suspended(true);
        assert!(draft.submit() == ComposerOutcome::Unchanged);
        assert_eq!(draft.text(), "pending text"); // not cleared
    }

    /// Safe-mode: content preserved while suspended, editable again after lift.
    #[test]
    fn draft_resumes_after_unsuspend() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("partial");
        draft.set_suspended(true);
        assert_eq!(draft.insert("x"), EditOutcome::Suspended);
        draft.set_suspended(false);
        assert_eq!(draft.insert("!"), EditOutcome::Mutated);
        assert_eq!(draft.text(), "partial!");
    }

    // ─── Keystroke non-passthrough ────────────────────────────────────────

    /// Spec §4.7: editing keystrokes are never terminal input.
    /// This is enforced by design — the draft buffer never routes to
    /// a terminal / provider byte stream. The test confirms that after
    /// editing operations, the draft contains only the locally rendered
    /// content and nothing has been "forwarded."
    ///
    /// Insert operations reject any string containing control characters
    /// (including `\r`/`\n`). The caller (input pipeline) receives individual
    /// CharacterEvents that do not include line endings — those arrive as
    /// KeyDown(Enter) and are handled as submit, never as character insert.
    #[test]
    fn editing_keystrokes_only_affect_local_draft() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        // Characters arrive as individual CharacterEvents from the runtime;
        // control characters are rejected at the insert boundary.
        draft.insert("l");
        draft.insert("s");
        draft.insert(" ");
        draft.insert("-");
        draft.insert("l");
        draft.insert("a");
        assert_eq!(draft.text(), "ls -la");
        // No external queue is modified — callers receive only draft state
        assert_eq!(draft.cursor(), 6);

        // Ensure control chars are rejected
        let newline_outcome = draft.insert("\n");
        assert_eq!(newline_outcome, EditOutcome::Unchanged);
        assert_eq!(draft.text(), "ls -la");
    }

    // ─── Unicode correctness (property-based) ────────────────────────────

    proptest! {
        /// Invariant: text.len() always ≤ cap.
        #[test]
        fn prop_text_never_exceeds_cap(
            cap in 1usize..=256,
            ops in proptest::collection::vec(0u8..10, 0..50),
        ) {
            let mut draft = ComposerDraft::new(cap);
            for op in ops {
                match op % 6 {
                    0 => { let _ = draft.insert("abc"); }
                    1 => { let _ = draft.paste("hello world"); }
                    2 => { let _ = draft.backspace(); }
                    3 => { let _ = draft.delete_forward(); }
                    4 => { let _ = draft.word_backspace(); }
                    _ => { let _ = draft.move_left(); }
                }
                prop_assert!(draft.text().len() <= cap,
                    "text.len()={} > cap={}", draft.text().len(), cap);
            }
        }

        /// Invariant: cursor is always a valid UTF-8 boundary.
        #[test]
        fn prop_cursor_is_valid_utf8_boundary(
            text in ".*",
            cursor_ops in proptest::collection::vec(0u8..8, 0..20),
        ) {
            let mut draft = ComposerDraft::new(MAX_DRAFT_BYTES);
            let _ = draft.paste(&text);
            for op in cursor_ops {
                match op % 8 {
                    0 => { let _ = draft.move_left(); }
                    1 => { let _ = draft.move_right(); }
                    2 => { let _ = draft.move_to_start(); }
                    3 => { let _ = draft.move_to_end(); }
                    4 => { let _ = draft.move_word_left(); }
                    5 => { let _ = draft.move_word_right(); }
                    6 => { let _ = draft.backspace(); }
                    _ => { let _ = draft.delete_forward(); }
                }
                let pos = draft.cursor();
                prop_assert!(
                    draft.text().is_char_boundary(pos),
                    "cursor={} is not a char boundary in {:?}",
                    pos, draft.text()
                );
            }
        }

        /// Invariant: submitted text always equals the local buffer at submit time.
        #[test]
        fn prop_submitted_text_equals_local_buffer(
            chars in proptest::collection::vec("[a-z ]", 0..30),
        ) {
            let mut draft = ComposerDraft::new(MAX_DRAFT_BYTES);
            for ch in &chars {
                let _ = draft.insert(ch);
            }
            let expected = draft.text().to_string();
            if let ComposerOutcome::Submitted(sub) = draft.submit() {
                prop_assert_eq!(sub, expected,
                    "submitted text must equal buffer before submit");
            }
        }
    }

    // ─── Grapheme-cluster cap boundary fix ──────────────────────────────────

    /// Truncation at the byte cap must preserve whole grapheme clusters.
    ///
    /// NFD `"e\u{0301}"` (LATIN SMALL LETTER E + COMBINING ACUTE ACCENT) is a
    /// 2-codepoint grapheme cluster (3 bytes: `0x65` + `0xcc` `0x81`). If the cap
    /// falls in the middle of the combining accent, a naive char-boundary
    /// truncation would produce `"e"` (dropping only the accent half) while a
    /// grapheme-boundary truncation correctly drops the entire cluster.
    ///
    /// This test places the cap at exactly 2 bytes — the `e` (1 byte) and the
    /// first byte of the combining accent — to verify the cluster is not split.
    #[test]
    fn truncate_at_grapheme_boundary_nfd_combining_accent() {
        // "e\u{0301}" is U+0065 (1 byte) + U+0301 (2 bytes) = 3 bytes total.
        // The grapheme cluster boundary falls at 0 or 3; 2 is in the middle.
        let s = "e\u{0301}"; // NFD: e + combining acute accent
        assert_eq!(s.len(), 3);

        // Cap = 2: cannot fit the full cluster (3 bytes). Must return "".
        let truncated = truncate_at_utf8_boundary(s, 2);
        assert!(
            truncated.is_empty(),
            "a 3-byte grapheme cluster must not be split at byte 2; got {truncated:?}",
        );

        // Cap = 3: fits exactly.
        let full = truncate_at_utf8_boundary(s, 3);
        assert_eq!(full, s);

        // Cap = 0: always empty.
        let empty = truncate_at_utf8_boundary(s, 0);
        assert_eq!(empty, "");
    }

    /// Insert with a cap that falls inside a multi-codepoint grapheme cluster
    /// does not split the cluster (uses `truncate_at_utf8_boundary` internally).
    #[test]
    fn insert_does_not_split_grapheme_cluster_at_cap() {
        // NFD "e\u{0301}" = 3 bytes. Cap = 2 → the cluster does not fit.
        let mut draft = ComposerDraft::new(2);
        let outcome = draft.insert("e\u{0301}");
        assert_eq!(
            outcome,
            EditOutcome::AtCapacity,
            "inserting a cluster that exceeds the cap must return AtCapacity"
        );
        assert_eq!(
            draft.text(),
            "",
            "the cluster must not be partially inserted"
        );
    }

    /// Paste with a ZWJ emoji sequence at the cap boundary must not split it.
    ///
    /// A ZWJ sequence like "👨\u{200D}👩" is a 3-element grapheme cluster
    /// (4 + 3 + 4 = 11 bytes).  Prepend 10 'a's (cap=15 → 5 bytes remaining for
    /// the cluster) to verify the whole cluster is either kept or dropped, never
    /// split.
    #[test]
    fn paste_does_not_split_zwj_emoji_at_cap() {
        // ZWJ sequence: 👨 (U+1F468, 4 bytes) + ZWJ (U+200D, 3 bytes) + 👩 (U+1F469, 4 bytes)
        // = 11 bytes as one grapheme cluster.
        let zwj = "\u{1F468}\u{200D}\u{1F469}";
        assert_eq!(zwj.len(), 11);

        // Cap = 14 bytes: 10 'a's (10 bytes) + 4 remaining → not enough for the cluster (11 bytes).
        let cap = 14;
        let mut draft = ComposerDraft::new(cap);
        draft.paste(&"a".repeat(10));
        assert_eq!(draft.text().len(), 10);

        // Paste the ZWJ sequence. Only 4 bytes remain; cluster needs 11.
        // The cluster must be dropped entirely (not split).
        let outcome = draft.paste(zwj);
        assert_eq!(
            outcome,
            EditOutcome::AtCapacity,
            "ZWJ cluster exceeding cap must return AtCapacity"
        );
        // Text must contain only the 10 'a's — no partial emoji bytes.
        assert_eq!(
            draft.text(),
            "aaaaaaaaaa",
            "draft must not contain a partial ZWJ cluster; got {:?}",
            draft.text()
        );
    }

    // ─── ComposerDraftManager: per-composer placeholder override (hud-se6hs) ──

    /// With no override set, `focused_placeholder` reports `None` — the
    /// caller (widgets.rs) falls back to its own global default.
    #[test]
    fn focused_placeholder_defaults_to_none_when_unset() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        assert_eq!(
            mgr.focused_placeholder(),
            None,
            "no override configured — caller must fall back to its own default"
        );
    }

    /// `set_focused_placeholder` overrides the global default with the
    /// focused node's own hint copy.
    #[test]
    fn set_focused_placeholder_overrides_default() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        mgr.set_focused_placeholder(Some("Search…".to_string()));

        assert_eq!(
            mgr.focused_placeholder(),
            Some("Search…".to_string()),
            "custom override must be reported verbatim"
        );
    }

    /// An empty-string override is a distinct explicit opt-out, not "unset".
    #[test]
    fn set_focused_placeholder_empty_string_is_explicit_opt_out() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        mgr.set_focused_placeholder(Some(String::new()));

        assert_eq!(
            mgr.focused_placeholder(),
            Some(String::new()),
            "explicit opt-out (Some(\"\")) must round-trip distinctly from unset (None)"
        );
    }

    /// A fresh `on_focus_gained` resets the override so a placeholder from a
    /// previously-focused composer never leaks onto the next one.
    #[test]
    fn on_focus_gained_resets_placeholder_override() {
        let mut mgr = ComposerDraftManager::new();
        let node_a = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_a, false);
        mgr.set_focused_placeholder(Some("Node A hint".to_string()));
        assert_eq!(mgr.focused_placeholder(), Some("Node A hint".to_string()));

        mgr.on_focus_lost();
        let node_b = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_b, false);

        assert_eq!(
            mgr.focused_placeholder(),
            None,
            "re-focusing a different node without an explicit set must not inherit \
             the previous node's override"
        );
    }

    // ─── ComposerDraftManager: keystroke routing ─────────────────────────────

    /// When a composer is focused, character events are routed into the draft.
    #[test]
    fn manager_routes_character_to_draft() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        let outcome = mgr.route_character("h");
        assert_eq!(outcome, EditOutcome::Mutated);

        let _ = mgr.route_character("i");
        let snap = mgr.draft().unwrap();
        assert_eq!(snap.text(), "hi", "draft must contain routed characters");
    }

    /// Space arrives as a named KeyDown on some platforms instead of a
    /// Character(" ") event; the composer must still treat it as text.
    #[test]
    fn manager_space_key_down_inserts_literal_space() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        mgr.route_character("h");
        mgr.route_character("i");
        let (consumed, outcome) = mgr.route_key_down("Space", "Space", false, false, false);

        assert!(consumed, "Space must be consumed by the draft manager");
        assert_eq!(outcome, ComposerOutcome::Edited);

        let snap = mgr.draft().unwrap();
        assert_eq!(snap.text(), "hi ", "draft must include the literal space");
        assert_eq!(
            snap.cursor(),
            3,
            "cursor must advance after the inserted space"
        );
    }

    /// Ctrl+A selects the whole draft (spec: selection by keyboard); consumed.
    #[test]
    fn manager_ctrl_a_selects_all() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for c in ["h", "e", "l", "l", "o"] {
            mgr.route_character(c);
        }
        // Cursor is at end (5); move somewhere else to prove select-all resets both ends.
        mgr.route_key_down("Home", "Home", false, false, false);

        let (consumed, outcome) = mgr.route_key_down("KeyA", "a", false, true, false);
        assert!(consumed, "Ctrl+A must be consumed by the draft manager");
        assert_eq!(outcome, ComposerOutcome::Edited);

        let draft = mgr.draft().expect("draft active");
        assert_eq!(draft.selection_anchor(), 0);
        assert_eq!(draft.cursor(), 5);
        assert!(draft.has_selection());
        assert_eq!(draft.selected_text(), "hello");
    }

    /// Ctrl+A on an empty draft is a consumed no-op (no selection, no notification).
    #[test]
    fn manager_ctrl_a_empty_is_noop() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        let (consumed, outcome) = mgr.route_key_down("KeyA", "a", false, true, false);
        assert!(consumed, "Ctrl+A must still be consumed on an empty draft");
        assert_eq!(outcome, ComposerOutcome::Unchanged);
        assert!(!mgr.draft().unwrap().has_selection());
    }

    /// Ctrl+C is consumed but never mutates the draft (copy is non-destructive);
    /// the clipboard write itself lives in the runtime layer.
    #[test]
    fn manager_ctrl_c_consumes_without_mutation() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for c in ["h", "i"] {
            mgr.route_character(c);
        }
        mgr.route_key_down("KeyA", "a", false, true, false); // select "hi"

        let (consumed, outcome) = mgr.route_key_down("KeyC", "c", false, true, false);
        assert!(
            consumed,
            "Ctrl+C must be consumed so it never reaches the agent"
        );
        assert_eq!(outcome, ComposerOutcome::Unchanged);
        let draft = mgr.draft().unwrap();
        assert_eq!(draft.text(), "hi", "copy must not change the draft text");
        assert!(draft.has_selection(), "copy leaves the selection intact");
    }

    /// Ctrl+X cuts the active selection: text shrinks, caret collapses, consumed.
    #[test]
    fn manager_ctrl_x_cuts_selection() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for c in ["a", "b", "c", "d"] {
            mgr.route_character(c);
        }
        // Select the first two chars: Home, then Shift+Right twice.
        mgr.route_key_down("Home", "Home", false, false, false);
        mgr.route_key_down("ArrowRight", "ArrowRight", true, false, false);
        mgr.route_key_down("ArrowRight", "ArrowRight", true, false, false);
        assert_eq!(mgr.draft().unwrap().selected_text(), "ab");

        let (consumed, outcome) = mgr.route_key_down("KeyX", "x", false, true, false);
        assert!(consumed, "Ctrl+X must be consumed by the draft manager");
        assert_eq!(outcome, ComposerOutcome::Edited);

        let snap = mgr.draft().unwrap();
        assert_eq!(snap.text(), "cd", "cut removes the selected range");
        assert_eq!(snap.cursor(), 0, "caret collapses to the cut point");
    }

    /// Ctrl+X with no selection is a consumed no-op (nothing to cut).
    #[test]
    fn manager_ctrl_x_no_selection_is_noop() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for c in ["x", "y"] {
            mgr.route_character(c);
        }
        let (consumed, outcome) = mgr.route_key_down("KeyX", "x", false, true, false);
        assert!(consumed, "Ctrl+X is consumed even with nothing selected");
        assert_eq!(outcome, ComposerOutcome::Unchanged);
        assert_eq!(
            mgr.draft().unwrap().text(),
            "xy",
            "no selection → no change"
        );
    }

    /// Ctrl+Alt+A/C/X are NOT treated as editing shortcuts (AltGr guard): they
    /// fall through unconsumed so composed characters still reach the draft.
    #[test]
    fn manager_ctrl_alt_shortcuts_not_consumed() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        mgr.route_character("z");
        for kc in ["KeyA", "KeyC", "KeyX"] {
            let (consumed, _) = mgr.route_key_down(kc, "", false, true, true);
            assert!(
                !consumed,
                "{kc} with Ctrl+Alt must not be consumed as a composer shortcut"
            );
        }
    }

    /// Enter key submits the draft and emits a clear notification.
    #[test]
    fn manager_enter_submits_and_clears() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        mgr.route_character("h");
        mgr.route_character("i");

        let (consumed, outcome) = mgr.route_key_down("Enter", "", false, false, false);
        assert!(consumed);
        assert_eq!(outcome, ComposerOutcome::Submitted("hi".into()));
        let draft = mgr.draft().unwrap();
        assert_eq!(
            (draft.text(), draft.cursor(), draft.selection_anchor()),
            ("", 0, 0)
        );
        assert_eq!(
            mgr.route_key_down("Enter", "", false, false, false),
            (true, ComposerOutcome::Unchanged)
        );
    }

    // ─── Submit-key contract + newline + vertical movement (hud-nx7yq.2) ──────

    /// `insert_newline` adds a literal `\n` at the caret and advances the cursor,
    /// unlike `insert` which rejects line endings.
    #[test]
    fn insert_newline_inserts_and_advances() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        assert_eq!(draft.insert("a"), EditOutcome::Mutated);
        assert_eq!(draft.insert_newline(), EditOutcome::Mutated);
        assert_eq!(draft.insert("b"), EditOutcome::Mutated);
        assert_eq!(draft.text(), "a\nb");
        assert_eq!(draft.cursor(), 3);
    }

    /// `insert_newline` deletes an active selection first (like `insert`).
    #[test]
    fn insert_newline_replaces_selection() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        for c in ["a", "b", "c"] {
            draft.insert(c);
        }
        // Select "bc" (anchor at 1, cursor at 3): move left twice with shift.
        draft.select_left();
        draft.select_left();
        assert!(draft.has_selection());
        assert_eq!(draft.insert_newline(), EditOutcome::Mutated);
        assert_eq!(draft.text(), "a\n", "selection replaced by newline");
        assert_eq!(draft.cursor(), 2);
    }

    /// `insert_newline` is suspended under safe mode and honors the byte cap.
    #[test]
    fn insert_newline_governance() {
        // Suspend → rejected.
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.set_suspended(true);
        assert_eq!(draft.insert_newline(), EditOutcome::Suspended);
        assert!(draft.text().is_empty());

        // At capacity → AtCapacity, no insert.
        let mut small = ComposerDraft::new(2);
        assert_eq!(small.insert("a"), EditOutcome::Mutated);
        assert_eq!(small.insert("b"), EditOutcome::Mutated); // fills to cap 2
        assert!(small.is_at_capacity());
        assert_eq!(small.insert_newline(), EditOutcome::AtCapacity);
        assert_eq!(small.text(), "ab", "no newline past the cap");
    }

    /// A whitespace-only (or empty) draft does not submit; the buffer is intact.
    #[test]
    fn submit_rejects_whitespace_only() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        assert!(
            draft.submit() == ComposerOutcome::Unchanged,
            "empty draft does not submit"
        );

        for c in [" ", " "] {
            draft.insert(c);
        }
        draft.insert_newline();
        // Draft is "  \n" — all whitespace.
        assert!(
            draft.submit() == ComposerOutcome::Unchanged,
            "whitespace-only draft must not submit"
        );
        assert_eq!(
            draft.text(),
            "  \n",
            "buffer left intact for continued editing"
        );
    }

    /// A submitted multi-line draft preserves embedded newlines verbatim.
    #[test]
    fn submit_preserves_embedded_newlines() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        draft.insert("a");
        draft.insert_newline();
        draft.insert("b");
        let sub = submitted(draft.submit());
        assert_eq!(sub, "a\nb", "embedded newline preserved in submission");
        assert!(draft.text().is_empty(), "buffer cleared after submit");
    }

    /// Vertical movement lands at the goal column and clamps to shorter lines.
    #[test]
    fn vertical_move_tracks_goal_column() {
        // "hello\nhi\nworld" — lines of length 5, 2, 5.
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        for c in ["h", "e", "l", "l", "o"] {
            draft.insert(c);
        }
        draft.insert_newline();
        for c in ["h", "i"] {
            draft.insert(c);
        }
        draft.insert_newline();
        for c in ["w", "o", "r", "l", "d"] {
            draft.insert(c);
        }
        // Caret at end (col 5 of "world"). Move up with goal col 5.
        assert_eq!(draft.current_col(), 5);
        assert_eq!(draft.move_up(5), EditOutcome::Mutated);
        // "hi" is only length 2 → clamp to its end (byte offset 8: "hello\nhi").
        assert_eq!(draft.cursor(), 8);
        // Move up again at goal col 5 → "hello" has col 5 (its end, byte 5).
        assert_eq!(draft.move_up(5), EditOutcome::Mutated);
        assert_eq!(draft.cursor(), 5);
        // Move down at goal col 5 → back to "hi" clamped end (byte 8).
        assert_eq!(draft.move_down(5), EditOutcome::Mutated);
        assert_eq!(draft.cursor(), 8);
    }

    /// Up on the first line goes to buffer start; Down on the last goes to end.
    #[test]
    fn vertical_move_edges() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        for c in ["a", "b"] {
            draft.insert(c);
        }
        draft.insert_newline();
        for c in ["c", "d"] {
            draft.insert(c);
        }
        // Caret at end of last line → Down goes to buffer end (already there → Unchanged).
        assert_eq!(draft.move_down(2), EditOutcome::Unchanged);
        // Move up to first line at col 2 → "ab" end (byte 2).
        assert_eq!(draft.move_up(2), EditOutcome::Mutated);
        assert_eq!(draft.cursor(), 2);
        // Up again on the first line → buffer start (byte 0).
        assert_eq!(draft.move_up(2), EditOutcome::Mutated);
        assert_eq!(draft.cursor(), 0);
        // Up at start → no-op.
        assert_eq!(draft.move_up(2), EditOutcome::Unchanged);
    }

    /// Shift+vertical extends the selection instead of collapsing it.
    #[test]
    fn vertical_select_extends() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        for c in ["a", "b"] {
            draft.insert(c);
        }
        draft.insert_newline();
        for c in ["c", "d"] {
            draft.insert(c);
        }
        // Caret at end (byte 5). Select up at col 2 → cursor to byte 2, anchor stays 5.
        assert_eq!(draft.select_up(2), EditOutcome::Mutated);
        assert!(draft.has_selection(), "shift+up extends a selection");
        assert_eq!(draft.cursor(), 2);
        assert_eq!(draft.selection_anchor(), 5);
    }

    /// Ctrl+Enter inserts a newline (no submission); a subsequent plain Enter
    /// submits the full multi-line draft transactionally with newlines preserved.
    #[test]
    fn manager_ctrl_enter_newline_then_enter_submits_multiline() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        mgr.route_character("a");
        let (consumed, outcome) = mgr.route_key_down("Enter", "Enter", false, true, false);
        assert!(consumed, "Ctrl+Enter must be consumed");
        assert_eq!(outcome, ComposerOutcome::Edited);
        mgr.route_character("b");

        // Plain Enter submits the whole multi-line draft.
        let (_, outcome) = mgr.route_key_down("Enter", "Enter", false, false, false);
        assert_eq!(outcome, ComposerOutcome::Submitted("a\nb".into()));
    }

    /// Shift+Enter also inserts a newline rather than submitting.
    #[test]
    fn manager_shift_enter_inserts_newline() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        mgr.route_character("x");
        let (consumed, outcome) = mgr.route_key_down("Enter", "Enter", true, false, false);
        assert!(consumed);
        assert_eq!(outcome, ComposerOutcome::Edited);

        assert_eq!(mgr.draft().unwrap().text(), "x\n");
    }

    /// Enter on a whitespace-only draft does not submit and keeps focus.
    #[test]
    fn manager_enter_whitespace_only_no_submit() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        mgr.route_key_down("Space", "Space", false, false, false);
        mgr.route_key_down("Space", "Space", false, false, false);
        let (consumed, outcome) = mgr.route_key_down("Enter", "Enter", false, false, false);
        assert!(consumed, "Enter is still consumed");
        assert_eq!(outcome, ComposerOutcome::Unchanged);
        assert!(mgr.is_active(), "composer stays focused");
        assert_eq!(mgr.draft().unwrap().text(), "  ", "draft preserved");
    }

    /// ArrowUp/ArrowDown move the caret across logical lines and are consumed
    /// (never forwarded to the agent), preserving the goal column across the run.
    #[test]
    fn manager_arrow_up_down_move_caret() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        // Build "hello\nhi\nworld", caret at end.
        for c in ["h", "e", "l", "l", "o"] {
            mgr.route_character(c);
        }
        mgr.route_key_down("Enter", "Enter", false, true, false); // Ctrl+Enter newline
        for c in ["h", "i"] {
            mgr.route_character(c);
        }
        mgr.route_key_down("Enter", "Enter", false, true, false);
        for c in ["w", "o", "r", "l", "d"] {
            mgr.route_character(c);
        }

        // ArrowUp from col 5 → clamps to "hi" (byte 8), goal col 5 retained.
        let (consumed, outcome) = mgr.route_key_down("ArrowUp", "ArrowUp", false, false, false);
        assert!(consumed, "ArrowUp is consumed by the composer");
        assert_eq!(outcome, ComposerOutcome::Edited);
        assert_eq!(mgr.draft().unwrap().cursor(), 8);
        // ArrowUp again keeps goal col 5 → "hello" end (byte 5), NOT clamped to 2.
        mgr.route_key_down("ArrowUp", "ArrowUp", false, false, false);
        assert_eq!(
            mgr.draft().unwrap().cursor(),
            5,
            "goal column preserved across consecutive vertical moves"
        );
    }

    /// A horizontal move between vertical moves resets the goal column.
    #[test]
    fn manager_goal_column_resets_on_horizontal_move() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        for c in ["h", "e", "l", "l", "o"] {
            mgr.route_character(c);
        }
        mgr.route_key_down("Enter", "Enter", false, true, false);
        for c in ["h", "i"] {
            mgr.route_character(c);
        }
        // Caret at end of "hi" (byte 8, col 2). ArrowUp goal col 2 → "hello" byte 2.
        mgr.route_key_down("ArrowUp", "ArrowUp", false, false, false);
        assert_eq!(mgr.draft().unwrap().cursor(), 2);
        // Horizontal move (Home) resets goal; then ArrowDown uses col 0, not 2.
        mgr.route_key_down("Home", "Home", false, false, false);
        assert_eq!(mgr.draft().unwrap().cursor(), 0);
        mgr.route_key_down("ArrowDown", "ArrowDown", false, false, false);
        assert_eq!(
            mgr.draft().unwrap().cursor(),
            6,
            "goal column re-seeded to 0 after the horizontal move (start of 'hi')"
        );
    }

    // ─── Soft-wrap visual-line vertical movement (hud-21o6x) ──────────────────

    /// Build a synthetic 2-row layout for "abcdef" wrapped as "abc" / "def" at a
    /// uniform 10px advance (each row's x restarts at 0, as cosmic-text reports).
    fn two_row_layout() -> ComposerVisualLayout {
        ComposerVisualLayout {
            lines: vec![
                ComposerVisualLine {
                    start_byte: 0,
                    end_byte: 3,
                    glyph_x: vec![(0, 0.0), (1, 10.0), (2, 20.0), (3, 30.0)],
                },
                ComposerVisualLine {
                    start_byte: 3,
                    end_byte: 6,
                    glyph_x: vec![(3, 0.0), (4, 10.0), (5, 20.0), (6, 30.0)],
                },
            ],
            text_len: 6,
            input_box: None,
            h_scroll_px: 0.0,
        }
    }

    #[test]
    fn visual_layout_line_of_has_wrap_boundary_affinity() {
        let l = two_row_layout();
        assert_eq!(l.line_of(0), Some(0));
        assert_eq!(l.line_of(2), Some(0));
        // Boundary byte 3 = end of row 0 = start of row 1 → affinity to the LATER row.
        assert_eq!(l.line_of(3), Some(1));
        assert_eq!(l.line_of(5), Some(1));
        assert_eq!(
            l.line_of(6),
            Some(1),
            "caret past last glyph is on the last row"
        );
    }

    #[test]
    fn visual_layout_byte_at_x_lands_near_goal() {
        let l = two_row_layout();
        assert_eq!(l.lines[0].byte_at_x(20.0), 2, "x=20 → byte 2 on row 0");
        assert_eq!(l.lines[1].byte_at_x(20.0), 5, "x=20 → byte 5 on row 1");
        // Clamped to the row's byte range.
        assert_eq!(
            l.lines[0].byte_at_x(999.0),
            3,
            "far-right clamps to row end"
        );
    }

    #[test]
    fn visual_layout_byte_at_point_selects_row_by_y_then_x_by_glyph() {
        let l = two_row_layout();
        // content_height=100 split across 2 rows → row 0 is y in [0,50), row 1 [50,100].
        assert_eq!(
            l.byte_at_point(20.0, 10.0, 100.0),
            2,
            "y in the top half hits row 0, x=20 → byte 2"
        );
        assert_eq!(
            l.byte_at_point(20.0, 90.0, 100.0),
            5,
            "y in the bottom half hits row 1, x=20 → byte 5"
        );
        // Exactly on the row boundary rounds down into the lower row (frac == 0.5
        // maps to row index 1 via floor(0.5 * 2) == 1).
        assert_eq!(l.byte_at_point(20.0, 50.0, 100.0), 5);
        // y past the content height clamps to the last row.
        assert_eq!(l.byte_at_point(20.0, 500.0, 100.0), 5);
        // Non-positive content_height falls back to row 0.
        assert_eq!(l.byte_at_point(20.0, 10.0, 0.0), 2);
    }

    #[test]
    fn visual_layout_byte_at_point_empty_layout_returns_zero() {
        let empty = ComposerVisualLayout {
            lines: vec![],
            text_len: 0,
            input_box: None,
            h_scroll_px: 0.0,
        };
        assert_eq!(empty.byte_at_point(20.0, 10.0, 100.0), 0);
    }

    // ─── Tall-projection input-box pointer hit-test (hud-lw60x) ───────────────

    /// A TALL full-portal PROJECTION composer: three wrapped rows rendered in a
    /// short, BOTTOM-anchored input box inside a much taller HitRegion (the
    /// click-anywhere-to-focus target). Geometry mirrors the compositor's
    /// `composer_input_box` for node height 300, `line_height` 20, `content_inset`
    /// 6 → `box_height = 20*3 + 6*2 = 72`, `box_top = 300 − 72 = 228`,
    /// `row0_top = 228 + 6 − vscroll`. Per-row glyph tables give a DISTINCT byte
    /// per row at the same x, so the assertion isolates the row selection.
    fn tall_projection_layout(input_box: Option<ComposerInputBoxGeometry>) -> ComposerVisualLayout {
        ComposerVisualLayout {
            lines: vec![
                ComposerVisualLine {
                    start_byte: 0,
                    end_byte: 3,
                    glyph_x: vec![(0, 0.0), (1, 10.0), (2, 20.0), (3, 30.0)],
                },
                ComposerVisualLine {
                    start_byte: 3,
                    end_byte: 6,
                    glyph_x: vec![(3, 0.0), (4, 10.0), (5, 20.0), (6, 30.0)],
                },
                ComposerVisualLine {
                    start_byte: 6,
                    end_byte: 9,
                    glyph_x: vec![(6, 0.0), (7, 10.0), (8, 20.0), (9, 30.0)],
                },
            ],
            text_len: 9,
            input_box,
            h_scroll_px: 0.0,
        }
    }

    #[test]
    fn byte_at_point_maps_pointer_y_through_bottom_anchored_input_box() {
        // Three rows all visible, no scroll: row0_top = 228 + 6 = 234; rows at
        // 234 / 254 / 274; visible box spans y ∈ [228, 300].
        let geom = ComposerInputBoxGeometry {
            box_top: 228.0,
            box_height: 72.0,
            row0_top: 234.0,
            line_height: 20.0,
            first_visible_row: 0,
            visible_rows: 3,
        };
        let l = tall_projection_layout(Some(geom));

        // x=12 → nearest glyph boundary is the 10px stop on every row, so the byte
        // returned is purely a function of the SELECTED row: row0→1, row1→4, row2→7.
        // A click on the TOP of the visible strip must select the top visible row
        // (byte 1) — the pre-fix bug mapped it to the LAST row (byte 7).
        assert_eq!(
            l.byte_at_point(12.0, 240.0, 300.0),
            1,
            "top of the visible strip selects the TOP visible row, not the last"
        );
        // Middle visible row.
        assert_eq!(
            l.byte_at_point(12.0, 258.0, 300.0),
            4,
            "middle of the visible strip selects the middle row"
        );
        // Bottom of the visible strip → bottom row.
        assert_eq!(
            l.byte_at_point(12.0, 290.0, 300.0),
            7,
            "bottom of the visible strip selects the bottom row"
        );
        // A click ABOVE the box (transcript area of the full portal) clamps into
        // the box and resolves to the TOP visible row, never the last.
        assert_eq!(
            l.byte_at_point(12.0, 50.0, 300.0),
            1,
            "click above the box clamps to the top visible row"
        );
        // A click BELOW the box clamps to the bottom row.
        assert_eq!(
            l.byte_at_point(12.0, 500.0, 300.0),
            7,
            "click below the box clamps to the bottom row"
        );
    }

    #[test]
    fn byte_at_point_without_geometry_reproduces_even_split_regression() {
        // SAME rows, but no published box geometry (the pre-hud-lw60x path / the
        // single-line + first-frame fallback): the node height is split evenly, so
        // a click on the visible strip of a tall portal (y=240 of 300) lands on the
        // LAST row — the exact defect the geometry mapping above fixes. Kept as a
        // contrast so a future change that drops the geometry can't pass silently.
        let l = tall_projection_layout(None);
        assert_eq!(
            l.byte_at_point(12.0, 240.0, 300.0),
            7,
            "even-split fallback mislocates the visible strip to the last row"
        );
    }

    #[test]
    fn byte_at_point_input_box_folds_vertical_scroll() {
        // Five rows, three visible, scrolled so rows 2..5 show (first_visible = 2):
        // vscroll_px = 2*20 = 40; row0_top = 228 + 6 − 40 = 194. The first VISIBLE
        // row (row 2) then sits at 194 + 2*20 = 234, i.e. the box top interior.
        let geom = ComposerInputBoxGeometry {
            box_top: 228.0,
            box_height: 72.0,
            row0_top: 194.0,
            line_height: 20.0,
            first_visible_row: 2,
            visible_rows: 3,
        };
        let mut l = tall_projection_layout(Some(geom));
        // Extend to five rows so the scrolled indices exist.
        l.lines.push(ComposerVisualLine {
            start_byte: 9,
            end_byte: 12,
            glyph_x: vec![(9, 0.0), (10, 10.0), (11, 20.0), (12, 30.0)],
        });
        l.lines.push(ComposerVisualLine {
            start_byte: 12,
            end_byte: 15,
            glyph_x: vec![(12, 0.0), (13, 10.0), (14, 20.0), (15, 30.0)],
        });
        l.text_len = 15;

        // A click at the top of the visible strip selects the first VISIBLE row
        // (absolute row 2, byte 7) — folding vscroll in, not row 0.
        assert_eq!(
            l.byte_at_point(12.0, 240.0, 300.0),
            7,
            "scrolled: top of the strip is the first visible row (row 2), not row 0"
        );
        // Bottom of the strip → last visible row (absolute row 4, byte 13).
        assert_eq!(
            l.byte_at_point(12.0, 292.0, 300.0),
            13,
            "scrolled: bottom of the strip is the last visible row (row 4)"
        );
    }

    #[test]
    fn byte_at_point_input_box_padding_clamps_to_visible_window() {
        // Six rows, three visible, scrolled to the MIDDLE so rows 1..4 show
        // (first_visible = 1): rows 0 and 4,5 are clipped ABOVE / BELOW the box.
        // vscroll_px = 1*20 = 20; row0_top = 228 + 6 − 20 = 214, so the absolute
        // row tops are 214/234/254/274/294/314. The box is [228, 300] with a 6px
        // content inset, so its top padding [228, 234) sits over the clipped row 0
        // and its bottom padding (294, 300] over the clipped row 4. A click in
        // that padding must resolve to the nearest VISIBLE row (1 or 3), never the
        // clipped row that happens to fall under the padding band (hud-lw60x P2).
        let geom = ComposerInputBoxGeometry {
            box_top: 228.0,
            box_height: 72.0,
            row0_top: 214.0,
            line_height: 20.0,
            first_visible_row: 1,
            visible_rows: 3,
        };
        let mut l = tall_projection_layout(Some(geom));
        // Extend to six rows; row i's 10px glyph stop is byte 3*i + 1.
        for start in [9usize, 12, 15] {
            l.lines.push(ComposerVisualLine {
                start_byte: start,
                end_byte: start + 3,
                glyph_x: vec![
                    (start, 0.0),
                    (start + 1, 10.0),
                    (start + 2, 20.0),
                    (start + 3, 30.0),
                ],
            });
        }
        l.text_len = 18;

        // Top padding (y=228, over clipped row 0) → first VISIBLE row (row 1, byte
        // 4), not the clipped row 0 (byte 1).
        assert_eq!(
            l.byte_at_point(12.0, 228.0, 300.0),
            4,
            "top padding clamps to the first visible row, not the clipped row above"
        );
        // Bottom padding (y=299, over clipped row 4) → last VISIBLE row (row 3,
        // byte 10), not the clipped row 4 (byte 13) — the pre-clamp overshoot.
        assert_eq!(
            l.byte_at_point(12.0, 299.0, 300.0),
            10,
            "bottom padding clamps to the last visible row, not the clipped row below"
        );
        // Interior middle still lands on the true row under the pointer (row 2).
        assert_eq!(
            l.byte_at_point(12.0, 258.0, 300.0),
            7,
            "interior click still selects the row actually under the pointer"
        );
    }

    #[test]
    fn visual_layout_vertical_target_moves_between_rows() {
        let l = two_row_layout();
        // Down from row 0 col-x 20 (byte 2) → row 1 near x=20 → byte 5.
        assert_eq!(l.vertical_target(2, false, 20.0), Some(5));
        // Up from row 1 (byte 5, x=20) → row 0 near x=20 → byte 2.
        assert_eq!(l.vertical_target(5, true, 20.0), Some(2));
        // Up on the first row goes to buffer start; at start → None.
        assert_eq!(l.vertical_target(2, true, 20.0), Some(0));
        assert_eq!(l.vertical_target(0, true, 0.0), None);
        // Down on the last row goes to buffer end; at end → None.
        assert_eq!(l.vertical_target(5, false, 20.0), Some(6));
        assert_eq!(l.vertical_target(6, false, 30.0), None);
    }

    #[test]
    fn move_to_byte_collapses_and_snaps() {
        let mut draft = ComposerDraft::new(DEFAULT_DRAFT_CAP);
        for c in ["a", "b", "c"] {
            draft.insert(c);
        }
        draft.select_left(); // selection anchor 3, cursor 2
        assert!(draft.has_selection());
        assert_eq!(draft.move_to_byte(1), EditOutcome::Mutated);
        assert_eq!(draft.cursor(), 1);
        assert!(!draft.has_selection(), "move_to_byte collapses selection");
        // Out-of-range clamps to text end.
        assert_eq!(draft.move_to_byte(99), EditOutcome::Mutated);
        assert_eq!(draft.cursor(), 3);
    }

    /// With a fresh multi-row layout, ArrowUp steps between VISUAL rows at pixel
    /// goal-x — NOT to buffer start (which is what hard-newline movement would do
    /// for this single-logical-line draft).
    #[test]
    fn manager_arrow_uses_visual_layout_when_fresh() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for c in ["a", "b", "c", "d", "e", "f"] {
            mgr.route_character(c);
        }
        // Caret at end (byte 6). Move left to a mid-row position (byte 5).
        mgr.route_key_down("ArrowLeft", "ArrowLeft", false, false, false);
        assert_eq!(mgr.draft().unwrap().cursor(), 5);

        mgr.set_visual_layout(Some(two_row_layout()));
        // ArrowUp: visual path → row 1 (byte 5, x=20) up to row 0 near x=20 → byte 2.
        mgr.route_key_down("ArrowUp", "ArrowUp", false, false, false);
        assert_eq!(
            mgr.draft().unwrap().cursor(),
            2,
            "visual ArrowUp lands on the adjacent row near goal-x, not buffer start"
        );
        // ArrowDown preserves goal-x=20 → back to row 1 byte 5.
        mgr.route_key_down("ArrowDown", "ArrowDown", false, false, false);
        assert_eq!(
            mgr.draft().unwrap().cursor(),
            5,
            "pixel goal-x preserved across consecutive visual vertical moves"
        );
    }

    /// A stale layout (measured for a different draft length) is ignored — the
    /// keystroke falls back to hard-newline movement until the next frame.
    #[test]
    fn manager_arrow_falls_back_when_layout_stale() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for c in ["a", "b", "c", "d", "e", "f"] {
            mgr.route_character(c);
        }
        // Layout for a DIFFERENT text length → not fresh.
        let mut stale = two_row_layout();
        stale.text_len = 99;
        mgr.set_visual_layout(Some(stale));
        // Caret at byte 6; hard-newline ArrowUp on a single logical line → buffer start.
        mgr.route_key_down("ArrowUp", "ArrowUp", false, false, false);
        assert_eq!(
            mgr.draft().unwrap().cursor(),
            0,
            "stale layout ignored → hard-newline fallback (to buffer start)"
        );
    }

    /// With no layout (e.g. single-line profile), vertical movement uses the
    /// hard-newline fallback unchanged.
    #[test]
    fn manager_arrow_no_layout_uses_hard_newline() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for c in ["a", "b", "c"] {
            mgr.route_character(c);
        }
        // No layout set. ArrowDown on single logical line → buffer end.
        mgr.route_key_down("ArrowDown", "ArrowDown", false, false, false);
        assert_eq!(
            mgr.draft().unwrap().cursor(),
            3,
            "hard-newline fallback to end"
        );
    }

    /// Boundaries update after a wrap-changing edit: once a new layout arrives,
    /// vertical movement uses the new visual rows.
    #[test]
    fn manager_visual_layout_updates_after_edit() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for c in ["a", "b", "c", "d", "e", "f"] {
            mgr.route_character(c);
        }
        mgr.set_visual_layout(Some(two_row_layout()));
        // Delete two chars → "abcd" (len 4); the old layout (text_len 6) is now stale.
        mgr.route_key_down("Backspace", "Backspace", false, false, false);
        mgr.route_key_down("Backspace", "Backspace", false, false, false);
        assert_eq!(mgr.draft().unwrap().text(), "abcd");
        // New layout for "abcd" as one row [0,4) — single row → vertical falls back.
        mgr.set_visual_layout(Some(ComposerVisualLayout {
            lines: vec![ComposerVisualLine {
                start_byte: 0,
                end_byte: 4,
                glyph_x: vec![(0, 0.0), (1, 10.0), (2, 20.0), (3, 30.0), (4, 40.0)],
            }],
            text_len: 4,
            input_box: None,
            h_scroll_px: 0.0,
        }));
        // Single-row layout (lines.len()==1) is not multi-row → hard-newline: Up → start.
        mgr.route_key_down("ArrowUp", "ArrowUp", false, false, false);
        assert_eq!(
            mgr.draft().unwrap().cursor(),
            0,
            "post-edit layout consulted"
        );
    }

    /// Logical Enter must submit even when the physical key/code is unknown.
    #[test]
    fn manager_logical_enter_submits_when_physical_key_is_unidentified() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        mgr.route_character("h");
        mgr.route_character("i");

        let (consumed, batch) = mgr.route_key_down("Unidentified", "Enter", false, false, false);
        assert!(
            consumed,
            "logical Enter must be consumed by the draft manager"
        );
        assert_eq!(batch, ComposerOutcome::Submitted("hi".into()));
        assert!(mgr.draft().unwrap().text().is_empty());
    }

    /// Backspace is consumed and routed to word_backspace when Ctrl is held.
    #[test]
    fn manager_ctrl_backspace_word_delete() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        // Insert "hello world"
        for ch in "hello world".chars() {
            mgr.route_character(&ch.to_string());
        }

        // Ctrl+Backspace should delete "world"
        let (consumed, _) = mgr.route_key_down("Backspace", "", false, true, false);
        assert!(consumed, "Ctrl+Backspace must be consumed");

        let snap = mgr.draft().unwrap();
        assert_eq!(snap.text(), "hello ", "word backspace must delete 'world'");
    }

    /// Ctrl+Shift+ArrowLeft extends the selection by a whole word, not one
    /// grapheme (regression for the shift-before-ctrl routing bug: shift used to
    /// win and only `select_left` ran).
    #[test]
    fn manager_ctrl_shift_arrow_left_extends_selection_by_word() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for ch in "hello world".chars() {
            mgr.route_character(&ch.to_string());
        }
        // Caret at end. Ctrl+Shift+Left must select the whole last word.
        let (consumed, _) = mgr.route_key_down("ArrowLeft", "ArrowLeft", true, true, false);
        assert!(consumed, "Ctrl+Shift+Left must be consumed by the composer");
        let draft = mgr.draft().expect("active draft");
        assert!(draft.has_selection(), "selection must be non-empty");
        let sel = draft.selection();
        assert_eq!(
            &draft.text()[sel.start..sel.end],
            "world",
            "Ctrl+Shift+Left must extend selection by a word, not one char"
        );
    }

    /// Ctrl+Shift+ArrowRight extends the selection forward by a whole word.
    #[test]
    fn manager_ctrl_shift_arrow_right_extends_selection_by_word() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for ch in "hello world".chars() {
            mgr.route_character(&ch.to_string());
        }
        // Move caret to start, then Ctrl+Shift+Right selects the first word.
        mgr.route_key_down("Home", "Home", false, false, false);
        let (consumed, _) = mgr.route_key_down("ArrowRight", "ArrowRight", true, true, false);
        assert!(
            consumed,
            "Ctrl+Shift+Right must be consumed by the composer"
        );
        let draft = mgr.draft().expect("active draft");
        let sel = draft.selection();
        assert_eq!(
            &draft.text()[sel.start..sel.end],
            "hello",
            "Ctrl+Shift+Right must extend selection forward by a word"
        );
    }

    /// An unsent draft survives an accidental blur and is restored on re-focus
    /// of the same composer node (chat-app draft behaviour).
    #[test]
    fn manager_draft_persists_across_focus_loss_and_restores() {
        let mut mgr = ComposerDraftManager::new();
        let node_a = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_a, false);
        for ch in "unsent reply".chars() {
            mgr.route_character(&ch.to_string());
        }
        // Blur without submitting (e.g. clicked another portal).
        mgr.on_focus_lost();
        assert!(!mgr.is_active(), "no active draft after blur");

        // Re-focusing the same node restores the in-progress text + caret.
        mgr.on_focus_gained(node_a, false);
        let draft = mgr.draft().expect("draft restored on re-focus");
        assert_eq!(
            draft.text(),
            "unsent reply",
            "blurred draft must be restored"
        );
        assert_eq!(
            draft.cursor(),
            "unsent reply".len(),
            "caret restored at end"
        );

        // A different composer node starts empty (drafts are per-node).
        let node_b = tze_hud_scene::SceneId::new();
        mgr.on_focus_lost();
        mgr.on_focus_gained(node_b, false);
        assert_eq!(
            mgr.draft().expect("node_b draft").text(),
            "",
            "a different composer node must not inherit node_a's draft"
        );

        // An empty peer must leave the one retained draft alone.
        mgr.on_focus_lost();
        mgr.on_focus_gained(node_a, false);
        assert_eq!(mgr.draft().unwrap().text(), "unsent reply");
        mgr.on_focus_lost();
        mgr.on_focus_gained(node_b, false);
        mgr.route_character("new peer draft");
        mgr.on_focus_lost();
        mgr.on_focus_gained(node_a, false);
        assert_eq!(
            mgr.draft().unwrap().text(),
            "",
            "nonempty peer blur replaces the single retained slot"
        );
        mgr.on_focus_lost();
        mgr.on_focus_lost(); // repeated blur cannot erase B's slot
        mgr.on_focus_gained(node_b, true);
        mgr.set_focused_placeholder(Some("current B hint".into()));
        assert_eq!(mgr.draft().unwrap().text(), "new peer draft");
        assert!(mgr.draft().unwrap().is_suspended());
        assert_eq!(mgr.route_character("blocked"), EditOutcome::Suspended);
        mgr.set_suspended(false);
        mgr.route_pointer_selection(2, 5);
        mgr.on_focus_lost();
        mgr.on_focus_gained(node_b, false);
        assert_eq!(
            (
                mgr.draft().unwrap().cursor(),
                mgr.draft().unwrap().selection_anchor()
            ),
            (5, 2)
        );
        assert_eq!(
            mgr.focused_placeholder(),
            None,
            "regain uses the new node config"
        );
        // Empty matching blur clears a stale same-key slot, never a peer slot.
        mgr.retained_draft = Some((node_b, ComposerDraft::default()));
        mgr.route_key_down("Escape", "Escape", false, false, false);
        mgr.on_focus_lost();
        assert!(mgr.retained_draft.is_none());
    }

    /// A submitted draft is empty and must NOT be resurrected on re-focus.
    #[test]
    fn manager_submitted_draft_is_not_persisted() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);
        for ch in "send me".chars() {
            mgr.route_character(&ch.to_string());
        }
        // Submit, then blur, then re-focus — must be a clean empty draft.
        let (_, batch) = mgr.route_key_down("Enter", "Enter", false, false, false);
        assert_eq!(batch, ComposerOutcome::Submitted("send me".into()));
        mgr.on_focus_lost();
        mgr.on_focus_gained(node_id, false);
        assert_eq!(
            mgr.draft().expect("draft").text(),
            "",
            "a submitted draft must not be restored on re-focus"
        );
        mgr.route_character("cancel me");
        assert_eq!(
            mgr.route_key_down("Escape", "Escape", false, false, false),
            (true, ComposerOutcome::Cancelled)
        );
        mgr.on_focus_lost();
        mgr.on_focus_gained(node_id, false);
        assert_eq!(
            mgr.draft().unwrap().text(),
            "",
            "cancelled text cannot resurrect"
        );
    }

    /// Unknown key codes are not consumed (caller forwards to agent).
    #[test]
    fn manager_unknown_key_not_consumed() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, false);

        let (consumed, outcome) = mgr.route_key_down("F5", "", false, false, false);
        assert!(!consumed, "unknown key must not be consumed");
        assert_eq!(outcome, ComposerOutcome::Unchanged);
    }

    /// Suspended draft rejects character routing but allows key navigation.
    #[test]
    fn manager_suspended_rejects_character() {
        let mut mgr = ComposerDraftManager::new();
        let node_id = tze_hud_scene::SceneId::new();
        mgr.on_focus_gained(node_id, true); // suspended = true

        let outcome = mgr.route_character("x");
        assert_eq!(outcome, EditOutcome::Suspended);
    }
}
